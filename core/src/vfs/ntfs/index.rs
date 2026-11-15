//! Index B+trees: $I30 directory indexes (collation FILE_NAME) and view indexes such as
//! `$Extend/$Reparse:$R` (collation NTOFS_ULONGS). Search, enumeration, insertion (root
//! push-down, node split) and deletion (predecessor replacement, merge of emptied nodes).
//!
//! Structural edits happen on an in-memory node cache; nodes may temporarily overflow and
//! are split in a final pass (`fix_overflows`) before everything is written back.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use super::layout::*;
use super::{Inode, NtfsFs, I30};
use crate::vfs::{FsError, FsResult, Ino};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Nid {
    Root,
    Blk(u64),
}

/// Collation rule of an index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Coll {
    /// $I30: $FILE_NAME keys, upcase-insensitive then binary.
    FileName,
    /// COLLATION_NTOFS_ULONGS: keys compared as sequences of little-endian u32.
    Ulongs,
}

/// What we are searching for.
enum Probe {
    Name(Vec<u16>, bool),
    Key(Vec<u8>),
}

pub(super) struct DirIndex {
    ino: Inode,
    name: Vec<u16>,
    coll: Coll,
    hdr: IndexRootHdr,
    root: Node,
    root_dirty: bool,
    nodes: HashMap<u64, Node>,
    dirty: HashSet<u64>,
    bitmap: Option<Vec<u8>>,
    bitmap_dirty: bool,
    vcn_unit: u64,
}

const MAX_DEPTH: usize = 32;

fn key_name(key: &[u8]) -> Vec<u16> {
    match FileName::parse(key) {
        Ok(f) => f.name,
        Err(_) => vec![],
    }
}

/// COLLATION_NTOFS_ULONGS (as ntfs-3g: u32 by u32, then by length).
pub(super) fn cmp_ulongs(a: &[u8], b: &[u8]) -> Ordering {
    for (x, y) in a.chunks(4).zip(b.chunks(4)) {
        let v = |c: &[u8]| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(w)
        };
        match v(x).cmp(&v(y)) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    a.len().cmp(&b.len())
}

fn probe_of(d: &DirIndex, key: &[u8]) -> Probe {
    match d.coll {
        Coll::FileName => Probe::Name(key_name(key), true),
        Coll::Ulongs => Probe::Key(key.to_vec()),
    }
}

impl NtfsFs {
    pub(super) fn new_root_hdr(&self) -> IndexRootHdr {
        if let Ok(r) = self.load_inode(super::ROOT_INO) {
            if let Some(l) = r.find(AT_INDEX_ROOT, I30) {
                if let Some(v) = r.attr(l).value() {
                    if let Ok((h, _)) = parse_index_root(v) {
                        return IndexRootHdr { ty: AT_FILE_NAME, collation: 1, ..h };
                    }
                }
            }
        }
        let bs = self.index_block;
        let cpb = if bs as u64 >= self.cluster { bs as u64 / self.cluster } else { bs as u64 / 512 };
        IndexRootHdr { ty: AT_FILE_NAME, collation: 1, block_size: bs, clusters_per_block: cpb as u8 }
    }

    fn open_dir(&self, dir: Ino) -> FsResult<DirIndex> {
        let ino = self.load_inode(dir)?;
        if !ino.is_dir() {
            return Err(FsError::NotDir);
        }
        let d = self.open_index(ino, I30, Coll::FileName)?;
        if d.hdr.ty != AT_FILE_NAME {
            return corrupt("directory index is not a $FILE_NAME index");
        }
        Ok(d)
    }

    fn open_index(&self, ino: Inode, name: &[u16], coll: Coll) -> FsResult<DirIndex> {
        let no = ino.no;
        let l = ino.find(AT_INDEX_ROOT, name).ok_or_else(|| FsError::Corrupt(format!("record {no} has no $INDEX_ROOT")))?;
        let v = ino.attr(l).value().ok_or_else(|| FsError::Corrupt("non-resident $INDEX_ROOT".into()))?;
        let (hdr, root) = parse_index_root(v)?;
        let bs = hdr.block_size as u64;
        if !(512..=65536).contains(&bs) || !bs.is_power_of_two() {
            return corrupt("bad index block size");
        }
        let vcn_unit = if bs >= self.cluster { self.cluster } else { 512 };
        Ok(DirIndex { ino, name: name.to_vec(), coll, hdr, root, root_dirty: false, nodes: HashMap::new(), dirty: HashSet::new(), bitmap: None, bitmap_dirty: false, vcn_unit })
    }

    /// Open a view index (e.g. `$Reparse:$R`) of record `no`.
    fn open_view(&self, no: Ino, name: &[u16], coll: Coll) -> FsResult<DirIndex> {
        let ino = self.load_inode(no)?;
        self.open_index(ino, name, coll)
    }

    /// All entries of a view index, in order.
    #[cfg(test)]
    pub(super) fn view_list(&mut self, no: Ino, name: &[u16], coll: Coll) -> FsResult<Vec<IEntry>> {
        let mut d = self.open_view(no, name, coll)?;
        let mut out = vec![];
        let mut visited = HashSet::new();
        self.walk(&mut d, Nid::Root, 0, &mut visited, &mut |e| {
            out.push(e.clone());
            true
        })?;
        Ok(out)
    }

    /// Insert an entry (`data` = raw first 8 bytes of the entry) into a view index.
    pub(super) fn view_insert(&mut self, no: Ino, name: &[u16], coll: Coll, data: u64, key: Vec<u8>) -> FsResult<()> {
        let d = self.open_view(no, name, coll)?;
        self.index_insert(d, data, key)
    }

    /// Remove the entry with `key` from a view index.
    pub(super) fn view_remove(&mut self, no: Ino, name: &[u16], coll: Coll, key: &[u8]) -> FsResult<()> {
        let d = self.open_view(no, name, coll)?;
        self.index_remove(d, &Probe::Key(key.to_vec()), None)
    }

    fn get(&self, d: &mut DirIndex, id: Nid) -> FsResult<Node> {
        match id {
            Nid::Root => Ok(d.root.clone()),
            Nid::Blk(v) => {
                if let Some(n) = d.nodes.get(&v) {
                    return Ok(n.clone());
                }
                let bs = d.hdr.block_size as u64;
                let s = self.stream(&d.ino, AT_INDEX_ALLOCATION, &d.name)?.ok_or_else(|| FsError::Corrupt("index points to missing $INDEX_ALLOCATION".into()))?;
                let off = v.checked_mul(d.vcn_unit).ok_or_else(|| FsError::Corrupt("vcn".into()))?;
                if off.checked_add(bs).map_or(true, |e| e > s.size()) {
                    return corrupt("index block beyond $INDEX_ALLOCATION");
                }
                let mut b = self.read_stream(&s, off, bs)?;
                if b.len() as u64 != bs {
                    return corrupt("short index block");
                }
                fixup_read(&mut b)?;
                let (vcn, n) = parse_indx(&b)?;
                if vcn != v {
                    return corrupt(format!("index block vcn mismatch ({vcn} != {v})"));
                }
                d.nodes.insert(v, n.clone());
                Ok(n)
            }
        }
    }

    fn put(&self, d: &mut DirIndex, id: Nid, n: Node) {
        match id {
            Nid::Root => {
                d.root = n;
                d.root_dirty = true;
            }
            Nid::Blk(v) => {
                d.nodes.insert(v, n);
                d.dirty.insert(v);
            }
        }
    }

    fn cmp_key(&self, p: &Probe, e: &IEntry) -> Ordering {
        match p {
            Probe::Name(name, exact) => {
                let en = key_name(&e.key);
                if *exact {
                    self.cmp_full(name, &en)
                } else {
                    self.cmp_ci(name, &en)
                }
            }
            Probe::Key(k) => cmp_ulongs(k, &e.key),
        }
    }

    /// Search; returns the matching entry.
    pub(super) fn dir_find(&mut self, dir: Ino, name: &[u16], exact: bool) -> FsResult<Option<IEntry>> {
        let mut d = self.open_dir(dir)?;
        let p = Probe::Name(name.to_vec(), exact);
        let mut id = Nid::Root;
        for _ in 0..MAX_DEPTH {
            let n = self.get(&mut d, id)?;
            let mut next = None;
            for e in &n.entries {
                if e.last {
                    next = e.child;
                    break;
                }
                match self.cmp_key(&p, e) {
                    Ordering::Equal => return Ok(Some(e.clone())),
                    Ordering::Less => {
                        next = e.child;
                        break;
                    }
                    Ordering::Greater => {}
                }
            }
            match next {
                Some(c) => id = Nid::Blk(c),
                None => return Ok(None),
            }
        }
        corrupt("index tree too deep")
    }

    /// In-order enumeration of all entries.
    pub(super) fn dir_list(&mut self, dir: Ino) -> FsResult<Vec<IEntry>> {
        let mut d = self.open_dir(dir)?;
        let mut out = vec![];
        let mut visited = HashSet::new();
        self.walk(&mut d, Nid::Root, 0, &mut visited, &mut |e| {
            out.push(e.clone());
            true
        })?;
        Ok(out)
    }

    pub(super) fn dir_is_empty(&mut self, dir: Ino) -> FsResult<bool> {
        let mut d = self.open_dir(dir)?;
        let mut visited = HashSet::new();
        let mut any = false;
        self.walk(&mut d, Nid::Root, 0, &mut visited, &mut |_| {
            any = true;
            false
        })?;
        Ok(!any)
    }

    /// Returns false if the visitor asked to stop.
    fn walk(&self, d: &mut DirIndex, id: Nid, depth: usize, visited: &mut HashSet<u64>, f: &mut dyn FnMut(&IEntry) -> bool) -> FsResult<bool> {
        if depth > MAX_DEPTH {
            return corrupt("index tree too deep");
        }
        if let Nid::Blk(v) = id {
            if !visited.insert(v) {
                return corrupt("index tree loop");
            }
        }
        let n = self.get(d, id)?;
        for e in &n.entries {
            if let Some(c) = e.child {
                if !self.walk(d, Nid::Blk(c), depth + 1, visited, f)? {
                    return Ok(false);
                }
            }
            if !e.last && !f(e) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn dir_insert(&mut self, dir: Ino, key: Vec<u8>, mref: u64) -> FsResult<()> {
        let d = self.open_dir(dir)?;
        self.index_insert(d, mref, key)
    }

    fn index_insert(&mut self, mut d: DirIndex, data: u64, key: Vec<u8>) -> FsResult<()> {
        let p = probe_of(&d, &key);
        let mut id = Nid::Root;
        for _ in 0..MAX_DEPTH {
            let mut n = self.get(&mut d, id)?;
            let mut pos = n.entries.len() - 1;
            for (i, e) in n.entries.iter().enumerate() {
                if e.last {
                    pos = i;
                    break;
                }
                match self.cmp_key(&p, e) {
                    Ordering::Equal => return Err(FsError::Exists),
                    Ordering::Less => {
                        pos = i;
                        break;
                    }
                    Ordering::Greater => {}
                }
            }
            match n.entries[pos].child {
                Some(c) => id = Nid::Blk(c),
                None => {
                    n.entries.insert(pos, IEntry { data, key, extra: vec![], child: None, last: false });
                    self.put(&mut d, id, n);
                    self.fix_overflows(&mut d)?;
                    return self.commit(d);
                }
            }
        }
        corrupt("index tree too deep")
    }

    /// Locate the exact entry for `name`; returns the path of (node, index) pairs down to it.
    fn find_path(&self, d: &mut DirIndex, p: &Probe) -> FsResult<Option<Vec<(Nid, usize)>>> {
        let mut id = Nid::Root;
        let mut path = vec![];
        for _ in 0..MAX_DEPTH {
            let n = self.get(d, id)?;
            let mut next = None;
            for (i, e) in n.entries.iter().enumerate() {
                if e.last {
                    next = e.child.map(|c| (i, c));
                    break;
                }
                match self.cmp_key(p, e) {
                    Ordering::Equal => {
                        path.push((id, i));
                        return Ok(Some(path));
                    }
                    Ordering::Less => {
                        next = e.child.map(|c| (i, c));
                        break;
                    }
                    Ordering::Greater => {}
                }
            }
            match next {
                Some((i, c)) => {
                    path.push((id, i));
                    id = Nid::Blk(c);
                }
                None => return Ok(None),
            }
        }
        corrupt("index tree too deep")
    }

    pub(super) fn dir_update_key(&mut self, dir: Ino, key: Vec<u8>, ino: Ino) -> FsResult<()> {
        let mut d = self.open_dir(dir)?;
        let p = Probe::Name(key_name(&key), true);
        let Some(path) = self.find_path(&mut d, &p)? else { return Err(FsError::NotFound) };
        let (id, i) = *path.last().unwrap();
        let mut n = self.get(&mut d, id)?;
        if mref_no(n.entries[i].data) != ino {
            return Err(FsError::NotFound);
        }
        if n.entries[i].key == key {
            return Ok(());
        }
        n.entries[i].key = key;
        self.put(&mut d, id, n);
        self.fix_overflows(&mut d)?;
        self.commit(d)
    }

    pub(super) fn dir_remove(&mut self, dir: Ino, name: &[u16], ino: Ino) -> FsResult<()> {
        let d = self.open_dir(dir)?;
        self.index_remove(d, &Probe::Name(name.to_vec(), true), Some(ino))
    }

    /// Remove the entry matching `p`; for $I30, `ino` must match the entry's file reference.
    fn index_remove(&mut self, mut d: DirIndex, p: &Probe, ino: Option<Ino>) -> FsResult<()> {
        let Some(mut path) = self.find_path(&mut d, p)? else { return Err(FsError::NotFound) };
        let (id, i) = path.pop().unwrap();
        let mut n = self.get(&mut d, id)?;
        if ino.map_or(false, |ino| mref_no(n.entries[i].data) != ino) {
            return Err(FsError::NotFound);
        }
        match n.entries[i].child {
            None => {
                n.entries.remove(i);
                self.put(&mut d, id, n);
                self.fix_underflow(&mut d, id, path)?;
            }
            Some(c) => {
                // replace with the predecessor (rightmost entry of the left subtree)
                path.push((id, i));
                let mut cur = c;
                for _ in 0..MAX_DEPTH {
                    let cn = self.get(&mut d, Nid::Blk(cur))?;
                    match cn.entries.last().and_then(|e| e.child) {
                        Some(next) => {
                            path.push((Nid::Blk(cur), cn.entries.len() - 1));
                            cur = next;
                        }
                        None => break,
                    }
                }
                let mut leaf = self.get(&mut d, Nid::Blk(cur))?;
                if leaf.real() == 0 || !leaf.is_leaf() {
                    return corrupt("empty index leaf");
                }
                let pred = leaf.entries.remove(leaf.real() - 1);
                self.put(&mut d, Nid::Blk(cur), leaf);
                let mut n = self.get(&mut d, id)?;
                let child = n.entries[i].child;
                n.entries[i] = IEntry { child, ..pred };
                self.put(&mut d, id, n);
                self.fix_underflow(&mut d, Nid::Blk(cur), path)?;
            }
        }
        self.fix_overflows(&mut d)?;
        self.commit(d)
    }

    fn fix_underflow(&mut self, d: &mut DirIndex, mut id: Nid, mut path: Vec<(Nid, usize)>) -> FsResult<()> {
        for _ in 0..=MAX_DEPTH + 2 {
            let n = self.get(d, id)?;
            if n.real() > 0 {
                return Ok(());
            }
            let v = match id {
                Nid::Root => {
                    if let Some(c) = n.entries[0].child {
                        let cn = self.get(d, Nid::Blk(c))?;
                        if cn.real() == 0 {
                            self.free_block(d, c)?;
                            self.put(d, Nid::Root, Node { entries: vec![IEntry::end(cn.entries[0].child)] });
                            continue;
                        }
                    }
                    return Ok(());
                }
                Nid::Blk(v) => v,
            };
            let Some((pid, pidx)) = path.pop() else { return corrupt("index path lost") };
            let mut p = self.get(d, pid)?;
            let nchild = n.entries[0].child;
            if p.real() == 0 {
                // only child of its parent: hoist our END child into the parent
                p.entries[0].child = nchild;
                self.free_block(d, v)?;
                self.put(d, pid, p);
                id = pid;
                continue;
            }
            if pidx < p.real() {
                let e = p.entries.remove(pidx);
                let s = p.entries[pidx].child.ok_or_else(|| FsError::Corrupt("index node without child".into()))?;
                let mut sn = self.get(d, Nid::Blk(s))?;
                sn.entries.insert(0, IEntry { child: nchild, ..e });
                self.put(d, Nid::Blk(s), sn);
            } else {
                let dsep = p.entries.remove(pidx - 1);
                let s = dsep.child.ok_or_else(|| FsError::Corrupt("index node without child".into()))?;
                let mut sn = self.get(d, Nid::Blk(s))?;
                let send = sn.entries.len() - 1;
                let moved = IEntry { child: sn.entries[send].child, ..dsep };
                sn.entries.insert(send, moved);
                sn.entries.last_mut().unwrap().child = nchild;
                p.entries.last_mut().unwrap().child = Some(s);
                self.put(d, Nid::Blk(s), sn);
            }
            self.free_block(d, v)?;
            self.put(d, pid, p);
            id = pid;
        }
        corrupt("index underflow did not converge")
    }

    fn root_fits(&self, d: &DirIndex) -> bool {
        let Some(l) = d.ino.find(AT_INDEX_ROOT, &d.name) else { return false };
        let rec = &d.ino.recs[l.0].r;
        let old = rec.attrs[l.1].disk_len();
        let newv = index_root_value(&d.hdr, &d.root);
        let new = align8(align8(0x18 + 2 * d.name.len()) + newv.len());
        rec.used_size(self.rec_size) - old + new <= self.rec_size
    }

    fn fix_overflows(&mut self, d: &mut DirIndex) -> FsResult<()> {
        let cap = indx_capacity(d.hdr.block_size as usize);
        for _ in 0..10_000 {
            if !self.root_fits(d) {
                self.push_down(d)?;
                continue;
            }
            let over = d.dirty.iter().copied().find(|v| d.nodes.get(v).map_or(false, |n| n.entries_len() > cap));
            match over {
                None => return Ok(()),
                Some(v) => self.split(d, v)?,
            }
        }
        corrupt("index split did not converge")
    }

    fn ensure_alloc_attrs(&mut self, d: &mut DirIndex) -> FsResult<()> {
        if d.ino.find(AT_INDEX_ALLOCATION, &d.name).is_none() {
            d.ino.add(Attribute {
                ty: AT_INDEX_ALLOCATION,
                name: d.name.clone(),
                flags: 0,
                instance: 0,
                body: Body::NonRes(NonRes { start_vcn: 0, runs: vec![], alloc: 0, size: 0, init: 0, comp_unit: 0, comp_size: 0, last_vcn: -1 }),
            });
        }
        if d.ino.find(AT_BITMAP, &d.name).is_none() {
            d.ino.add(Attribute::resident(AT_BITMAP, &d.name, vec![0u8; 8]));
            d.bitmap = Some(vec![0u8; 8]);
        }
        Ok(())
    }

    fn load_bitmap(&mut self, d: &mut DirIndex) -> FsResult<()> {
        if d.bitmap.is_none() {
            let v = match self.stream(&d.ino, AT_BITMAP, &d.name)? {
                Some(s) => {
                    let n = s.size().min(1 << 24);
                    self.read_stream(&s, 0, n)?
                }
                None => vec![0u8; 8],
            };
            d.bitmap = Some(v);
        }
        Ok(())
    }

    fn alloc_block(&mut self, d: &mut DirIndex) -> FsResult<u64> {
        self.ensure_alloc_attrs(d)?;
        self.load_bitmap(d)?;
        let bm = d.bitmap.as_mut().unwrap();
        let bit = (0..bm.len() * 8).find(|&i| bm[i / 8] & (1 << (i % 8)) == 0).unwrap_or(bm.len() * 8);
        if bit / 8 >= bm.len() {
            bm.resize(bm.len() + 8, 0);
        }
        bm[bit / 8] |= 1 << (bit % 8);
        d.bitmap_dirty = true;
        let vcn = bit as u64 * d.hdr.block_size as u64 / d.vcn_unit;
        d.nodes.insert(vcn, Node::empty_leaf());
        d.dirty.insert(vcn);
        Ok(vcn)
    }

    fn free_block(&mut self, d: &mut DirIndex, vcn: u64) -> FsResult<()> {
        self.load_bitmap(d)?;
        let bit = (vcn.saturating_mul(d.vcn_unit) / d.hdr.block_size as u64).min(usize::MAX as u64) as usize;
        let bm = d.bitmap.as_mut().unwrap();
        if bit / 8 < bm.len() {
            bm[bit / 8] &= !(1 << (bit % 8));
            d.bitmap_dirty = true;
        }
        d.nodes.remove(&vcn);
        d.dirty.remove(&vcn);
        Ok(())
    }

    fn push_down(&mut self, d: &mut DirIndex) -> FsResult<()> {
        let v = self.alloc_block(d)?;
        let old = std::mem::replace(&mut d.root, Node { entries: vec![IEntry::end(Some(v))] });
        d.root_dirty = true;
        d.nodes.insert(v, old);
        d.dirty.insert(v);
        if !self.root_fits(d) {
            return Err(FsError::NoSpace);
        }
        Ok(())
    }

    fn split(&mut self, d: &mut DirIndex, v: u64) -> FsResult<()> {
        let n = self.get(d, Nid::Blk(v))?;
        let real = n.real();
        if real < 3 {
            return corrupt("cannot split index node with fewer than 3 entries");
        }
        // parent lookup by descending with the node's first key
        let probe = probe_of(d, &n.entries[0].key);
        let mut id = Nid::Root;
        let mut parent = None;
        for _ in 0..MAX_DEPTH {
            let pn = self.get(d, id)?;
            let mut pos = pn.entries.len() - 1;
            for (i, e) in pn.entries.iter().enumerate() {
                if e.last || self.cmp_key(&probe, e) == Ordering::Less {
                    pos = i;
                    break;
                }
            }
            match pn.entries[pos].child {
                Some(c) if c == v => {
                    parent = Some((id, pos));
                    break;
                }
                Some(c) => id = Nid::Blk(c),
                None => break,
            }
        }
        let Some((pid, pidx)) = parent else { return corrupt("index node parent not found") };
        let total: usize = n.entries[..real].iter().map(|e| e.disk_len()).sum();
        let mut acc = 0;
        let mut m = real / 2;
        for (i, e) in n.entries[..real].iter().enumerate() {
            if acc + e.disk_len() > total / 2 {
                m = i;
                break;
            }
            acc += e.disk_len();
        }
        let m = m.clamp(1, real - 2);
        let mut left: Vec<IEntry> = n.entries[..m].to_vec();
        left.push(IEntry::end(n.entries[m].child));
        let right: Vec<IEntry> = n.entries[m + 1..].to_vec();
        let lv = self.alloc_block(d)?;
        self.put(d, Nid::Blk(lv), Node { entries: left });
        self.put(d, Nid::Blk(v), Node { entries: right });
        let mut pn = self.get(d, pid)?;
        pn.entries.insert(pidx, IEntry { child: Some(lv), ..n.entries[m].clone() });
        self.put(d, pid, pn);
        Ok(())
    }

    fn commit(&mut self, mut d: DirIndex) -> FsResult<()> {
        let bs = d.hdr.block_size as usize;
        // collapse to a small index when the tree has no blocks left
        let shrink = d.root.is_leaf() && d.ino.find(AT_INDEX_ALLOCATION, &d.name).is_some() && {
            self.load_bitmap(&mut d)?;
            d.bitmap.as_ref().unwrap().iter().all(|&b| b == 0)
        };
        let mut dirty: Vec<u64> = d.dirty.iter().copied().collect();
        dirty.sort_unstable();
        if !shrink {
            for v in dirty {
                let n = &d.nodes[&v];
                let usn = 1;
                let raw = serialize_indx(n, v, bs, usn).ok_or_else(|| FsError::Corrupt("index node overflow".into()))?;
                self.write_attr(&mut d.ino, AT_INDEX_ALLOCATION, &d.name, v * d.vcn_unit, &raw, false)?;
            }
            if d.bitmap_dirty {
                let bm = d.bitmap.clone().unwrap();
                let cur = self.stream(&d.ino, AT_BITMAP, &d.name)?.map_or(0, |s| s.size());
                if (bm.len() as u64) > cur {
                    self.resize_attr(&mut d.ino, AT_BITMAP, &d.name, bm.len() as u64, true)?;
                }
                self.write_attr(&mut d.ino, AT_BITMAP, &d.name, 0, &bm, true)?;
            }
        } else {
            for ty in [AT_INDEX_ALLOCATION, AT_BITMAP] {
                if let Some(l) = d.ino.find(ty, &d.name) {
                    let a = d.ino.remove(l);
                    if let Some(n) = a.nonres() {
                        let runs = n.runs.clone();
                        self.free_runs(&runs)?;
                    }
                }
            }
        }
        if d.root_dirty || shrink {
            let v = index_root_value(&d.hdr, &d.root);
            let l = d.ino.find(AT_INDEX_ROOT, &d.name).ok_or_else(|| FsError::Corrupt("no $INDEX_ROOT".into()))?;
            if let Body::Res { value, .. } = &mut d.ino.attr_mut(l).body {
                *value = v;
            }
        }
        self.store_inode(&mut d.ino)
    }
}
