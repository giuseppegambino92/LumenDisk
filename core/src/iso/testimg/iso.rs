//! ISO 9660 image builder for tests: Rock Ridge (SP/ER/PX/TF/NM/SL/CE and deep-directory
//! relocation with RE/CL/PL), Joliet, multi-extent files (with a configurable extent size so
//! small files exercise them, and real > 4 GiB files written sparsely), El Torito (BIOS +
//! UEFI entries) and an isohybrid-style MBR. Path tables are not written (readers of the
//! directory hierarchy, Linux included, don't need them).

use std::collections::BTreeMap;

const S: u64 = 2048;

/// A built image: `size` bytes, zero except for `chunks` (offset, bytes).
#[derive(Clone, Debug, Default)]
pub struct Image {
    pub size: u64,
    pub chunks: Vec<(u64, Vec<u8>)>,
}

impl Image {
    fn put(&mut self, off: u64, data: Vec<u8>) {
        if !data.is_empty() {
            self.size = self.size.max(off + data.len() as u64);
            self.chunks.push((off, data));
        }
    }

    /// The whole image in memory (small images only).
    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = vec![0u8; self.size as usize];
        for (o, d) in &self.chunks {
            v[*o as usize..*o as usize + d.len()].copy_from_slice(d);
        }
        v
    }

    /// Write the image as a sparse file.
    pub fn write_sparse(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::create(path)?;
        f.set_len(self.size)?;
        for (o, d) in &self.chunks {
            f.write_all_at(d, *o)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
enum Kind {
    Dir,
    File(Vec<u8>),
    /// `size` bytes of zeros except `pieces` (file offset, bytes).
    Sparse { size: u64, pieces: Vec<(u64, Vec<u8>)> },
    Symlink(String),
}

#[derive(Clone, Debug)]
struct TNode {
    name: String,
    kind: Kind,
    parent: usize,
    children: Vec<usize>,
    depth: usize,
    mtime: i64,
    // Assigned while building.
    iso_id: Vec<u8>,
    joliet_id: Vec<u8>,
    /// Parent in the ISO 9660 hierarchy (differs for relocated directories).
    iso_parent: usize,
    relocated: bool,
    dir_lba: [u32; 2],
    dir_size: [u32; 2],
    data_lba: u32,
}

/// Builder. Paths are absolute ("/a/b.txt"); parents are created as needed.
#[derive(Clone)]
pub struct IsoBuilder {
    label: String,
    rock: bool,
    joliet: bool,
    max_extent: u64,
    relocate_below: Option<usize>,
    hybrid: bool,
    nodes: Vec<TNode>,
    bios: Option<String>,
    efi: Option<String>,
    efi_sectors: Option<u16>,
}

fn both16(v: u16) -> [u8; 4] {
    let mut b = [0u8; 4];
    b[..2].copy_from_slice(&v.to_le_bytes());
    b[2..].copy_from_slice(&v.to_be_bytes());
    b
}
fn both32(v: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&v.to_le_bytes());
    b[4..].copy_from_slice(&v.to_be_bytes());
    b
}

fn date7(t: i64) -> [u8; 7] {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    // civil from days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    [(y - 1900) as u8, m as u8, d as u8, (secs / 3600) as u8, (secs / 60 % 60) as u8, (secs % 60) as u8, 0]
}

impl Default for IsoBuilder {
    fn default() -> Self {
        Self::new("TEST")
    }
}

impl IsoBuilder {
    pub fn new(label: &str) -> Self {
        let root = TNode {
            name: String::new(),
            kind: Kind::Dir,
            parent: 0,
            children: vec![],
            depth: 0,
            mtime: 1_704_164_645,
            iso_id: vec![0],
            joliet_id: vec![0],
            iso_parent: 0,
            relocated: false,
            dir_lba: [0; 2],
            dir_size: [0; 2],
            data_lba: 0,
        };
        IsoBuilder { label: label.into(), rock: true, joliet: true, max_extent: 0xFFFF_F800, relocate_below: Some(8), hybrid: false, nodes: vec![root], bios: None, efi: None, efi_sectors: None }
    }
    pub fn rock_ridge(mut self, on: bool) -> Self {
        self.rock = on;
        self
    }
    pub fn joliet(mut self, on: bool) -> Self {
        self.joliet = on;
        self
    }
    /// Largest extent of a file (multiple of 2048); larger files become multi-extent.
    pub fn max_extent(mut self, bytes: u64) -> Self {
        self.max_extent = (bytes / S).max(1) * S;
        self
    }
    /// Relocate directories at this depth or deeper into /RR_MOVED (Rock Ridge only;
    /// ECMA-119 6.8.2.1 allows 8 levels). None: never.
    pub fn relocate_deeper_than(mut self, depth: Option<usize>) -> Self {
        self.relocate_below = depth;
        self
    }
    pub fn hybrid_mbr(mut self, on: bool) -> Self {
        self.hybrid = on;
        self
    }

    fn ensure_dir(&mut self, path: &str) -> usize {
        let mut cur = 0usize;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur = match self.nodes[cur].children.iter().copied().find(|&c| self.nodes[c].name == comp) {
                Some(c) => c,
                None => self.add(cur, comp, Kind::Dir),
            };
        }
        cur
    }

    fn add(&mut self, parent: usize, name: &str, kind: Kind) -> usize {
        let i = self.nodes.len();
        let depth = self.nodes[parent].depth + 1;
        let mtime = 1_704_164_645 + i as i64 * 61;
        self.nodes.push(TNode { name: name.into(), kind, parent, children: vec![], depth, mtime, iso_id: vec![], joliet_id: vec![], iso_parent: parent, relocated: false, dir_lba: [0; 2], dir_size: [0; 2], data_lba: 0 });
        self.nodes[parent].children.push(i);
        i
    }

    fn add_path(&mut self, path: &str, kind: Kind) {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let p = self.ensure_dir(dir);
        self.add(p, name, kind);
    }

    pub fn dir(mut self, path: &str) -> Self {
        self.ensure_dir(path);
        self
    }
    pub fn file(mut self, path: &str, data: impl Into<Vec<u8>>) -> Self {
        self.add_path(path, Kind::File(data.into()));
        self
    }
    pub fn sparse_file(mut self, path: &str, size: u64, pieces: Vec<(u64, Vec<u8>)>) -> Self {
        self.add_path(path, Kind::Sparse { size, pieces });
        self
    }
    pub fn symlink(mut self, path: &str, target: &str) -> Self {
        self.add_path(path, Kind::Symlink(target.into()));
        self
    }
    /// El Torito BIOS no-emulation entry for the file at `path` (added with [`Self::file`]).
    pub fn bios_boot(mut self, path: &str) -> Self {
        self.bios = Some(path.into());
        self
    }
    /// El Torito UEFI entry for the file at `path`; `sectors` overrides the recorded count.
    pub fn efi_boot(mut self, path: &str, sectors: Option<u16>) -> Self {
        self.efi = Some(path.into());
        self.efi_sectors = sectors;
        self
    }

    fn find(&self, path: &str) -> Option<usize> {
        let mut cur = 0usize;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur = self.nodes[cur].children.iter().copied().find(|&c| self.nodes[c].name == comp)?;
        }
        Some(cur)
    }

    fn size_of(&self, i: usize) -> u64 {
        match &self.nodes[i].kind {
            Kind::File(d) => d.len() as u64,
            Kind::Sparse { size, .. } => *size,
            _ => 0,
        }
    }

    /// ISO 9660 level-2 style identifiers (upper case d-characters, ";1" on files), unique per
    /// directory; Joliet identifiers (UCS-2 BE, ≤ 64 characters).
    fn assign_ids(&mut self) {
        for d in 0..self.nodes.len() {
            let kids = self.nodes[d].children.clone();
            let mut used = std::collections::HashSet::new();
            for (k, &c) in kids.iter().enumerate() {
                let n = &self.nodes[c];
                let mut base: String = n.name.to_uppercase().chars().map(|ch| if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' { ch } else { '_' }).collect();
                if matches!(n.kind, Kind::Dir) {
                    base = base.replace('.', "_");
                }
                base.truncate(24);
                if base.is_empty() || !used.insert(base.clone()) {
                    base = format!("{}_{k}", &base[..base.len().min(18)]);
                    used.insert(base.clone());
                }
                let iso = if matches!(n.kind, Kind::Dir) { base } else if base.contains('.') { format!("{base};1") } else { format!("{base}.;1") };
                let mut j: Vec<u8> = n.name.encode_utf16().take(64).flat_map(|u| u.to_be_bytes()).collect();
                if !matches!(n.kind, Kind::Dir) {
                    j.extend(";1".encode_utf16().flat_map(|u| u.to_be_bytes()));
                }
                self.nodes[c].iso_id = iso.into_bytes();
                self.nodes[c].joliet_id = j;
            }
        }
    }

    /// Rock Ridge relocation: directories at depth > limit move under /RR_MOVED.
    fn relocate(&mut self) {
        let Some(limit) = self.relocate_below.filter(|_| self.rock) else { return };
        let deep: Vec<usize> = (1..self.nodes.len()).filter(|&i| matches!(self.nodes[i].kind, Kind::Dir) && self.nodes[i].depth > limit && self.nodes[self.nodes[i].parent].depth == limit).collect();
        if deep.is_empty() {
            return;
        }
        let moved = self.ensure_dir("/rr_moved");
        self.nodes[moved].iso_id = b"RR_MOVED".to_vec();
        for (k, &i) in deep.iter().enumerate() {
            self.nodes[i].iso_parent = moved;
            self.nodes[i].relocated = true;
            self.nodes[i].iso_id = format!("D{k}").into_bytes();
        }
    }

    /// Children of `d` as listed in tree `t` (0 = ISO 9660 / Rock Ridge, 1 = Joliet).
    /// Relocated directories appear under /RR_MOVED in tree 0 and as CL placeholders in their
    /// real parent.
    fn listing(&self, d: usize, t: usize) -> Vec<(usize, bool)> {
        let mut out: Vec<(usize, bool)> = vec![];
        if t == 0 {
            for (i, n) in self.nodes.iter().enumerate().skip(1) {
                if n.iso_parent == d && (n.parent != d || !n.relocated) {
                    out.push((i, false));
                }
                if n.relocated && n.parent == d {
                    out.push((i, true)); // CL placeholder
                }
            }
            out.sort_by(|a, b| self.id(a.0, 0, a.1).cmp(&self.id(b.0, 0, b.1)));
        } else {
            out = self.nodes[d].children.iter().filter(|&&c| self.nodes[c].name != "rr_moved" || !self.rock_moved()).map(|&c| (c, false)).collect();
            out.sort_by(|a, b| self.nodes[a.0].joliet_id.cmp(&self.nodes[b.0].joliet_id));
        }
        out
    }

    fn rock_moved(&self) -> bool {
        self.nodes.iter().any(|n| n.relocated)
    }

    fn id(&self, i: usize, t: usize, placeholder: bool) -> Vec<u8> {
        let n = &self.nodes[i];
        if t == 1 {
            return n.joliet_id.clone();
        }
        if placeholder {
            // The placeholder keeps the directory's own (pre-relocation) identifier.
            let mut s: String = n.name.to_uppercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            s.truncate(24);
            return s.into_bytes();
        }
        n.iso_id.clone()
    }

    /// The SUSP entries of a record in tree 0.
    fn su_entries(&self, i: usize, dot: Option<u8>, placeholder: bool, cl_target: u32, pl_target: u32) -> Vec<Vec<u8>> {
        if !self.rock {
            return vec![];
        }
        let n = &self.nodes[i];
        let mut e = vec![];
        let entry = |sig: &[u8; 2], body: &[u8]| -> Vec<u8> {
            let mut v = vec![sig[0], sig[1], (4 + body.len()) as u8, 1];
            v.extend_from_slice(body);
            v
        };
        if dot == Some(0) && i == 0 {
            e.push(entry(b"SP", &[0xBE, 0xEF, 0]));
            let id = b"RRIP_1991A";
            let des = b"THE ROCK RIDGE INTERCHANGE PROTOCOL PROVIDES SUPPORT FOR POSIX FILE SYSTEM SEMANTICS";
            let src = b"PLEASE CONTACT DISC PUBLISHER FOR SPECIFICATION SOURCE.  SEE PUBLISHER IDENTIFIER IN PRIMARY VOLUME DESCRIPTOR FOR CONTACT INFORMATION.";
            let mut body = vec![id.len() as u8, des.len() as u8, src.len() as u8, 1];
            body.extend_from_slice(id);
            body.extend_from_slice(des);
            body.extend_from_slice(src);
            e.push(entry(b"ER", &body));
        }
        let is_dir = matches!(n.kind, Kind::Dir);
        let mode: u32 = match &n.kind {
            Kind::Dir => 0o040755,
            Kind::Symlink(_) => 0o120777,
            _ => 0o100644 | ((i as u32 % 2) * 0o100), // some executables
        };
        let mut px = vec![];
        for v in [mode, if is_dir { 2 } else { 1 }, 1000, 1000, i as u32] {
            px.extend_from_slice(&both32(v));
        }
        e.push(entry(b"PX", &px));
        let mut tf = vec![0x03u8];
        tf.extend_from_slice(&date7(n.mtime - 1000));
        tf.extend_from_slice(&date7(n.mtime));
        e.push(entry(b"TF", &tf));
        if dot.is_none() {
            // NM, split in ≤ 250-byte pieces with CONTINUE.
            let name = n.name.as_bytes();
            let chunks: Vec<&[u8]> = if name.is_empty() { vec![] } else { name.chunks(250).collect() };
            for (k, c) in chunks.iter().enumerate() {
                let mut body = vec![if k + 1 < chunks.len() { 1 } else { 0 }];
                body.extend_from_slice(c);
                e.push(entry(b"NM", &body));
            }
            if let Kind::Symlink(t) = &n.kind {
                // SL component records (RRIP 4.1.3.1), ≤ 240 bytes per SL entry.
                let mut comps: Vec<Vec<u8>> = vec![];
                if t.starts_with('/') {
                    comps.push(vec![0x08, 0]);
                }
                for part in t.split('/').filter(|p| !p.is_empty()) {
                    match part {
                        "." => comps.push(vec![0x02, 0]),
                        ".." => comps.push(vec![0x04, 0]),
                        p => {
                            let b = p.as_bytes();
                            let pieces: Vec<&[u8]> = b.chunks(200).collect();
                            for (k, c) in pieces.iter().enumerate() {
                                let mut v = vec![if k + 1 < pieces.len() { 1 } else { 0 }, c.len() as u8];
                                v.extend_from_slice(c);
                                comps.push(v);
                            }
                        }
                    }
                }
                let mut groups: Vec<Vec<u8>> = vec![vec![]];
                for c in comps {
                    if groups.last().map_or(0, |g| g.len()) + c.len() > 240 {
                        groups.push(vec![]);
                    }
                    if let Some(g) = groups.last_mut() {
                        g.extend_from_slice(&c);
                    }
                }
                let ng = groups.len();
                for (k, g) in groups.into_iter().enumerate() {
                    let mut body = vec![if k + 1 < ng { 1 } else { 0 }];
                    body.extend_from_slice(&g);
                    e.push(entry(b"SL", &body));
                }
            }
            if placeholder {
                e.push(entry(b"CL", &both32(cl_target)));
            } else if n.relocated {
                e.push(entry(b"RE", &[]));
            }
        }
        if dot == Some(1) && pl_target != 0 {
            e.push(entry(b"PL", &both32(pl_target)));
        }
        e
    }
}

/// A directory record (ECMA-119 9.1) with System Use `su` (already split, see `fit_su`).
fn record(lba: u32, size: u32, flags: u8, id: &[u8], su: &[u8], mtime: i64) -> Vec<u8> {
    let base = 33 + id.len() + (id.len() % 2 == 0) as usize;
    let mut len = base + su.len();
    len += len % 2;
    let mut r = vec![0u8; len];
    r[0] = len as u8;
    r[2..10].copy_from_slice(&both32(lba));
    r[10..18].copy_from_slice(&both32(size));
    r[18..25].copy_from_slice(&date7(mtime));
    r[25] = flags;
    r[28..32].copy_from_slice(&both16(1));
    r[32] = id.len() as u8;
    r[33..33 + id.len()].copy_from_slice(id);
    r[base..base + su.len()].copy_from_slice(su);
    r
}

/// Split SUSP entries between the record (≤ `room` bytes) and a continuation area. Returns
/// (in-record bytes without the CE entry, continuation bytes).
fn fit_su(entries: &[Vec<u8>], room: usize) -> (Vec<u8>, Vec<u8>) {
    let total: usize = entries.iter().map(|e| e.len()).sum();
    if total <= room {
        return (entries.concat(), vec![]);
    }
    let mut inside = vec![];
    let mut cont = vec![];
    let mut spill = false;
    for e in entries {
        if !spill && inside.len() + e.len() + 28 <= room {
            inside.extend_from_slice(e);
        } else {
            spill = true;
            cont.extend_from_slice(e);
        }
    }
    (inside, cont)
}

fn ce_entry(blk: u32, off: u32, len: u32) -> Vec<u8> {
    let mut v = vec![b'C', b'E', 28, 1];
    v.extend_from_slice(&both32(blk));
    v.extend_from_slice(&both32(off));
    v.extend_from_slice(&both32(len));
    v
}

struct Plan {
    /// Per directory and tree: records with placeholder LBA fields, the CE data per record.
    recs: BTreeMap<(usize, usize), Vec<RecPlan>>,
}

#[derive(Clone)]
struct RecPlan {
    /// Node, or the directory itself for "." / ".." (dot = Some(0|1)).
    node: usize,
    dot: Option<u8>,
    placeholder: bool,
    /// Index of the extent part for multi-extent files.
    part: usize,
    parts: usize,
    su_inside: Vec<u8>,
    su_cont: Vec<u8>,
    ce_pos: (u32, u32),
    len: usize,
}

impl IsoBuilder {
    pub fn build(mut self) -> Image {
        self.assign_ids();
        self.relocate();
        let trees = if self.joliet { 2 } else { 1 };
        let dirs: Vec<usize> = (0..self.nodes.len()).filter(|&i| matches!(self.nodes[i].kind, Kind::Dir)).collect();
        // ---- pass A: records and sizes
        let mut plan = Plan { recs: BTreeMap::new() };
        for t in 0..trees {
            for &d in &dirs {
                if t == 1 && self.rock_moved() && self.nodes[d].name == "rr_moved" && self.nodes[d].parent == 0 {
                    continue;
                }
                let mut v = vec![];
                for dot in [0u8, 1] {
                    let target = if dot == 0 { d } else if t == 0 { self.nodes[d].iso_parent } else { self.nodes[d].parent };
                    let su = if t == 0 { self.su_entries(if dot == 0 { d } else { target }, Some(dot), false, 0, if dot == 1 && self.nodes[d].relocated { 1 } else { 0 }) } else { vec![] };
                    let room = 220;
                    let (inside, cont) = fit_su(&su, room);
                    v.push(RecPlan { node: target, dot: Some(dot), placeholder: false, part: 0, parts: 1, su_inside: inside, su_cont: cont, ce_pos: (0, 0), len: 0 });
                }
                for (c, ph) in self.listing(d, t) {
                    let id = self.id(c, t, ph);
                    let size = self.size_of(c);
                    let parts = if size > self.max_extent { size.div_ceil(self.max_extent) as usize } else { 1 };
                    for part in 0..parts {
                        let su = if t == 0 && part == 0 { self.su_entries(c, None, ph, 1, 0) } else { vec![] };
                        let base = 33 + id.len() + (id.len() % 2 == 0) as usize;
                        let room = 254usize.saturating_sub(base);
                        let (inside, cont) = fit_su(&su, room);
                        v.push(RecPlan { node: c, dot: None, placeholder: ph, part, parts, su_inside: inside, su_cont: cont, ce_pos: (0, 0), len: 0 });
                    }
                }
                plan.recs.insert((d, t), v);
            }
        }
        // Record lengths and directory sizes (records never cross a sector, 6.8.1.1).
        for ((d, t), v) in plan.recs.iter_mut() {
            let mut pos = 0u64;
            for r in v.iter_mut() {
                let id_len = if r.dot.is_some() { 1 } else { self.id(r.node, *t, r.placeholder).len() };
                let base = 33 + id_len + (id_len % 2 == 0) as usize;
                let mut len = base + r.su_inside.len() + if r.su_cont.is_empty() { 0 } else { 28 };
                len += len % 2;
                r.len = len;
                if pos % S + len as u64 > S {
                    pos = pos.div_ceil(S) * S;
                }
                pos += len as u64;
            }
            self.nodes[*d].dir_size[*t] = (pos.div_ceil(S) * S) as u32;
        }
        // ---- layout
        let mut next: u32 = 16;
        let pvd_lba = next;
        next += 1;
        let br_lba = (self.bios.is_some() || self.efi.is_some()).then(|| {
            next += 1;
            next - 1
        });
        let svd_lba = (trees == 2).then(|| {
            next += 1;
            next - 1
        });
        let term_lba = next;
        next += 1;
        let cat_lba = br_lba.map(|_| {
            next += 1;
            next - 1
        });
        for t in 0..trees {
            for &d in &dirs {
                if plan.recs.contains_key(&(d, t)) {
                    self.nodes[d].dir_lba[t] = next;
                    next += self.nodes[d].dir_size[t] / S as u32;
                }
            }
        }
        // Continuation areas, each inside one sector.
        let mut ce_blk = next;
        let mut ce_off = 0u32;
        let mut any_ce = false;
        for v in plan.recs.values_mut() {
            for r in v.iter_mut() {
                if r.su_cont.is_empty() {
                    continue;
                }
                any_ce = true;
                let l = r.su_cont.len() as u32;
                if ce_off + l > S as u32 {
                    ce_blk += 1;
                    ce_off = 0;
                }
                r.ce_pos = (ce_blk, ce_off);
                ce_off += l;
            }
        }
        next = if any_ce { ce_blk + 1 } else { next };
        for i in 0..self.nodes.len() {
            let sz = self.size_of(i);
            if matches!(self.nodes[i].kind, Kind::File(_) | Kind::Sparse { .. }) && sz > 0 {
                self.nodes[i].data_lba = next;
                next = next.checked_add(sz.div_ceil(S) as u32).expect("image too large");
            }
        }
        let total = next;
        // ---- pass B: emit
        let mut img = Image::default();
        let mut pvd = vec![0u8; S as usize];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        pvd[8..40].fill(b' ');
        pvd[40..72].fill(b' ');
        let l = self.label.as_bytes();
        pvd[40..40 + l.len().min(32)].copy_from_slice(&l[..l.len().min(32)]);
        pvd[80..88].copy_from_slice(&both32(total));
        pvd[120..124].copy_from_slice(&both16(1));
        pvd[124..128].copy_from_slice(&both16(1));
        pvd[128..132].copy_from_slice(&both16(S as u16));
        pvd[190..813].fill(b' ');
        for k in 0..4 {
            let o = 813 + k * 17;
            pvd[o..o + 16].fill(b'0');
        }
        pvd[881] = 1;
        let mut svd = pvd.clone();
        let root0 = record(self.nodes[0].dir_lba[0], self.nodes[0].dir_size[0], 2, &[0], &[], self.nodes[0].mtime);
        pvd[156..190].copy_from_slice(&root0[..34]);
        img.put(pvd_lba as u64 * S, pvd);
        if let Some(svd_lba) = svd_lba {
            svd[0] = 2;
            svd[88..91].copy_from_slice(b"%/E");
            svd[40..72].fill(0);
            let lj: Vec<u8> = self.label.encode_utf16().take(16).flat_map(|u| u.to_be_bytes()).collect();
            svd[40..40 + lj.len()].copy_from_slice(&lj);
            for k in (40 + lj.len()..72).step_by(2) {
                svd[k] = 0;
                svd[k + 1] = b' ';
            }
            let root1 = record(self.nodes[0].dir_lba[1], self.nodes[0].dir_size[1], 2, &[0], &[], self.nodes[0].mtime);
            svd[156..190].copy_from_slice(&root1[..34]);
            img.put(svd_lba as u64 * S, svd);
        }
        let mut term = vec![0u8; S as usize];
        term[0] = 255;
        term[1..6].copy_from_slice(b"CD001");
        term[6] = 1;
        img.put(term_lba as u64 * S, term);
        if let (Some(br), Some(cat)) = (br_lba, cat_lba) {
            let mut b = vec![0u8; S as usize];
            b[1..6].copy_from_slice(b"CD001");
            b[6] = 1;
            b[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
            b[0x47..0x4B].copy_from_slice(&cat.to_le_bytes());
            img.put(br as u64 * S, b);
            let mut c = vec![0u8; S as usize];
            c[0] = 1;
            c[4..18].copy_from_slice(b"LumenDisk test");
            c[30] = 0x55;
            c[31] = 0xAA;
            let sum = (0..16).fold(0u16, |s, i| s.wrapping_add(u16::from_le_bytes([c[i * 2], c[i * 2 + 1]])));
            c[28..30].copy_from_slice(&(0u16.wrapping_sub(sum)).to_le_bytes());
            let mut p = 32;
            if let Some(i) = self.bios.as_deref().and_then(|b| self.find(b)) {
                c[p] = 0x88;
                c[p + 6..p + 8].copy_from_slice(&4u16.to_le_bytes());
                c[p + 8..p + 12].copy_from_slice(&self.nodes[i].data_lba.to_le_bytes());
            }
            p += 32;
            if let Some(i) = self.efi.as_deref().and_then(|b| self.find(b)) {
                c[p] = 0x91;
                c[p + 1] = 0xEF;
                c[p + 2] = 1;
                p += 32;
                c[p] = 0x88;
                let secs = self.efi_sectors.unwrap_or_else(|| (self.size_of(i) / 512).min(0xFFFF) as u16);
                c[p + 6..p + 8].copy_from_slice(&secs.to_le_bytes());
                c[p + 8..p + 12].copy_from_slice(&self.nodes[i].data_lba.to_le_bytes());
            }
            img.put(cat as u64 * S, c);
        }
        for ((d, t), v) in &plan.recs {
            let mut buf = vec![0u8; self.nodes[*d].dir_size[*t] as usize];
            let mut pos = 0usize;
            for r in v {
                let n = &self.nodes[r.node];
                let (id, lba, size, flags) = if let Some(dot) = r.dot {
                    let tgt = r.node;
                    (vec![dot], self.nodes[tgt].dir_lba[*t], self.nodes[tgt].dir_size[*t], 2u8)
                } else if r.placeholder {
                    (self.id(r.node, 0, true), 0, 0, 0u8)
                } else {
                    let id = self.id(r.node, *t, false);
                    match &n.kind {
                        Kind::Dir => (id, n.dir_lba[*t], n.dir_size[*t], 2u8),
                        Kind::Symlink(_) => (id, 0, 0, 0),
                        _ => {
                            let size = self.size_of(r.node);
                            let start = r.part as u64 * self.max_extent;
                            let len = (size - start.min(size)).min(self.max_extent);
                            let lba = n.data_lba + (start / S) as u32;
                            let flags = if r.part + 1 < r.parts { 0x80 } else { 0 };
                            (id, if size == 0 { 0 } else { lba }, len as u32, flags)
                        }
                    }
                };
                let mut su = r.su_inside.clone();
                if !r.su_cont.is_empty() {
                    su.extend_from_slice(&ce_entry(r.ce_pos.0, r.ce_pos.1, r.su_cont.len() as u32));
                }
                // The CL placeholder's own CL target: the relocated directory.
                if r.placeholder {
                    let tgt = n.dir_lba[0];
                    patch_cl(&mut su, tgt);
                }
                // PL in ".." of a relocated directory: its real parent.
                if r.dot == Some(1) && self.nodes[*d].relocated {
                    patch_pl(&mut su, self.nodes[self.nodes[*d].parent].dir_lba[0]);
                }
                let rec = record(lba, size, flags, &id, &su, n.mtime);
                if pos % S as usize + rec.len() > S as usize {
                    pos = pos.div_ceil(S as usize) * S as usize;
                }
                buf[pos..pos + rec.len()].copy_from_slice(&rec);
                pos += rec.len();
                if !r.su_cont.is_empty() {
                    img.put(r.ce_pos.0 as u64 * S + r.ce_pos.1 as u64, r.su_cont.clone());
                }
            }
            img.put(self.nodes[*d].dir_lba[*t] as u64 * S, buf);
        }
        for n in &self.nodes {
            let base = n.data_lba as u64 * S;
            match &n.kind {
                Kind::File(d) => img.put(base, d.clone()),
                Kind::Sparse { pieces, .. } => {
                    for (o, d) in pieces {
                        img.put(base + o, d.clone());
                    }
                }
                _ => {}
            }
        }
        img.size = img.size.max(total as u64 * S);
        if self.hybrid {
            let mut mbr = vec![0u8; 512];
            mbr[446] = 0x80;
            mbr[446 + 4] = 0x17;
            mbr[446 + 12..446 + 16].copy_from_slice(&((img.size / 512) as u32).to_le_bytes());
            mbr[510] = 0x55;
            mbr[511] = 0xAA;
            img.put(0, mbr);
        }
        img
    }
}

fn patch_entry(su: &mut [u8], sig: &[u8; 2], v: u32) {
    let mut p = 0;
    while p + 4 <= su.len() {
        let l = su[p + 2] as usize;
        if l < 4 {
            break;
        }
        if &su[p..p + 2] == sig && l >= 12 {
            su[p + 4..p + 12].copy_from_slice(&both32(v));
        }
        p += l;
    }
}
fn patch_cl(su: &mut [u8], v: u32) {
    patch_entry(su, b"CL", v)
}
fn patch_pl(su: &mut [u8], v: u32) {
    patch_entry(su, b"PL", v)
}
