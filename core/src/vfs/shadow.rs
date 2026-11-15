//! Feature 2 — Shadow Mount: Copy-on-Write write-back overlay (dry-run mode).
//!
//! Every mutation is kept on the Mac: file contents in a chunked staging file
//! (`~/Library/Application Support/LumenDisk/Shadow/<volume>.stage`), metadata and the
//! directory tree in memory, plus an ordered log of structural operations. Nothing touches
//! the physical volume until `commit()`, which replays the structural log in order and then
//! writes the final content of every modified file. `rollback()` discards everything.
//!
//! Inode identity: objects that exist on the lower volume keep their inode number; objects
//! created in the overlay get ids ≥ `SHADOW_BASE`. After a commit those ids keep resolving
//! (alias table) so open Finder windows and NFS file handles stay valid.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use super::*;

pub const SHADOW_BASE: Ino = 1 << 61;
const CHUNK: u64 = 64 * 1024;

#[derive(Clone, Debug)]
struct FileData {
    size: u64,
    /// Bytes [0, base_limit) not present in `chunks` come from the lower file.
    base_limit: u64,
    /// chunk index → offset in the staging file
    chunks: BTreeMap<u64, u64>,
}

#[derive(Clone, Debug)]
struct SNode {
    attr: Attr,
    lower: Option<Ino>,
    data: Option<FileData>,
    children: Option<BTreeMap<String, (Ino, FileKind)>>,
    target: Option<String>,
    parent: Ino,
    name: String,
    alive: bool,
    attr_dirty: bool,
}

#[derive(Clone, Debug)]
enum Op {
    Create { parent: Ino, name: String, ino: Ino, perm: u16, uid: u32, gid: u32 },
    Mkdir { parent: Ino, name: String, ino: Ino, perm: u16, uid: u32, gid: u32 },
    Symlink { parent: Ino, name: String, target: String, ino: Ino, uid: u32, gid: u32 },
    SetAttr { ino: Ino, set: SetAttr },
    Unlink { parent: Ino, name: String },
    Rmdir { parent: Ino, name: String },
    Rename { fp: Ino, fname: String, tp: Ino, tname: String },
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct ShadowChange {
    pub path: String,
    pub kind: String,
    pub is_dir: bool,
    pub old_size: u64,
    pub new_size: u64,
    pub delta: i64,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ShadowSummary {
    pub added: u64,
    pub modified: u64,
    pub deleted: u64,
    pub pending_bytes: i64,
}

pub struct ShadowOverlay {
    pub lower: Box<dyn FileSystem>,
    nodes: HashMap<Ino, SNode>,
    deleted: HashMap<Ino, (String, u64, bool)>,
    original_path: HashMap<Ino, String>,
    known_parent: HashMap<Ino, (Ino, String)>,
    log: Vec<Op>,
    next: u64,
    stage: File,
    stage_path: PathBuf,
    stage_len: u64,
    alias: HashMap<Ino, Ino>,
}

impl ShadowOverlay {
    pub fn new(lower: Box<dyn FileSystem>, stage_path: PathBuf) -> FsResult<Self> {
        if let Some(d) = stage_path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let stage = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&stage_path)?;
        Ok(ShadowOverlay {
            lower,
            nodes: HashMap::new(),
            deleted: HashMap::new(),
            original_path: HashMap::new(),
            known_parent: HashMap::new(),
            log: vec![],
            next: 1,
            stage,
            stage_path,
            stage_len: 0,
            alias: HashMap::new(),
        })
    }

    fn r(&self, ino: Ino) -> Ino {
        *self.alias.get(&ino).unwrap_or(&ino)
    }

    fn live_node(&self, ino: Ino) -> FsResult<Option<&SNode>> {
        match self.nodes.get(&ino) {
            Some(n) if !n.alive => Err(FsError::Stale),
            Some(n) => Ok(Some(n)),
            None if self.deleted.contains_key(&ino) || ino >= SHADOW_BASE => Err(FsError::Stale),
            None => Ok(None),
        }
    }

    fn lower_path(&self, ino: Ino) -> String {
        let root = self.lower.root_ino();
        let mut parts = vec![];
        let mut cur = ino;
        for _ in 0..512 {
            if cur == root {
                break;
            }
            if let Some(n) = self.nodes.get(&cur) {
                parts.push(n.name.clone());
                cur = n.parent;
            } else if let Some((p, n)) = self.known_parent.get(&cur) {
                parts.push(n.clone());
                cur = *p;
            } else {
                parts.push(format!("<inode {cur}>"));
                break;
            }
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    fn ensure_node(&mut self, ino: Ino) -> FsResult<()> {
        if self.live_node(ino)?.is_some() {
            return Ok(());
        }
        let attr = self.lower.getattr(ino)?;
        let (parent, name) = self.known_parent.get(&ino).cloned().unwrap_or((self.lower.root_ino(), String::new()));
        let target = if attr.kind == FileKind::Symlink { self.lower.readlink(ino).ok() } else { None };
        self.nodes.insert(
            ino,
            SNode { attr, lower: Some(ino), data: None, children: None, target, parent, name, alive: true, attr_dirty: false },
        );
        Ok(())
    }

    fn materialize_dir(&mut self, ino: Ino) -> FsResult<()> {
        self.ensure_node(ino)?;
        let n = &self.nodes[&ino];
        if n.attr.kind != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        if n.children.is_none() {
            let entries = self.lower.readdir(ino)?;
            let mut m = BTreeMap::new();
            for e in entries {
                self.known_parent.insert(e.ino, (ino, e.name.clone()));
                m.insert(e.name, (e.ino, e.kind));
            }
            self.nodes.get_mut(&ino).unwrap().children = Some(m);
        }
        Ok(())
    }

    fn copy_up(&mut self, ino: Ino) -> FsResult<()> {
        self.ensure_node(ino)?;
        let n = self.nodes.get_mut(&ino).unwrap();
        if n.attr.kind == FileKind::Dir {
            return Err(FsError::IsDir);
        }
        if n.data.is_none() {
            n.data = Some(FileData { size: n.attr.size, base_limit: if n.lower.is_some() { n.attr.size } else { 0 }, chunks: BTreeMap::new() });
        }
        Ok(())
    }

    fn read_chunk(&mut self, ino: Ino, idx: u64) -> FsResult<Vec<u8>> {
        let (lower, fd) = {
            let n = &self.nodes[&ino];
            (n.lower, n.data.clone().unwrap())
        };
        let mut buf = vec![0u8; CHUNK as usize];
        if let Some(&off) = fd.chunks.get(&idx) {
            self.stage.read_exact_at(&mut buf, off)?;
            return Ok(buf);
        }
        let start = idx * CHUNK;
        if let Some(l) = lower {
            if start < fd.base_limit {
                let want = (fd.base_limit - start).min(CHUNK) as u32;
                let d = self.lower.read(l, start, want)?;
                buf[..d.len()].copy_from_slice(&d);
            }
        }
        Ok(buf)
    }

    fn write_chunk(&mut self, ino: Ino, idx: u64, buf: &[u8]) -> FsResult<()> {
        let existing = self.nodes[&ino].data.as_ref().unwrap().chunks.get(&idx).copied();
        let off = match existing {
            Some(o) => o,
            None => {
                let o = self.stage_len;
                self.stage_len += CHUNK;
                self.nodes.get_mut(&ino).unwrap().data.as_mut().unwrap().chunks.insert(idx, o);
                o
            }
        };
        self.stage.write_all_at(buf, off)?;
        Ok(())
    }

    fn new_id(&mut self) -> Ino {
        let i = SHADOW_BASE + self.next;
        self.next += 1;
        i
    }

    fn add_node(&mut self, parent: Ino, name: &str, kind: FileKind, perm: u16, uid: u32, gid: u32, target: Option<String>) -> FsResult<Ino> {
        check_name(name, 255)?;
        self.materialize_dir(parent)?;
        if self.nodes[&parent].children.as_ref().unwrap().contains_key(name) {
            return Err(FsError::Exists);
        }
        let ino = self.new_id();
        let now = Timestamp::now();
        let size = target.as_ref().map(|t| t.len() as u64).unwrap_or(0);
        let attr = Attr {
            ino,
            kind,
            size,
            perm,
            nlink: if kind == FileKind::Dir { 2 } else { 1 },
            uid,
            gid,
            atime: now,
            mtime: now,
            ctime: now,
            used_bytes: 0,
        };
        let node = SNode {
            attr,
            lower: None,
            data: if kind == FileKind::File { Some(FileData { size: 0, base_limit: 0, chunks: BTreeMap::new() }) } else { None },
            children: if kind == FileKind::Dir { Some(BTreeMap::new()) } else { None },
            target,
            parent,
            name: name.to_string(),
            alive: true,
            attr_dirty: false,
        };
        self.nodes.insert(ino, node);
        let p = self.nodes.get_mut(&parent).unwrap();
        p.children.as_mut().unwrap().insert(name.to_string(), (ino, kind));
        p.attr.mtime = now;
        Ok(ino)
    }

    /// Detach (parent,name) from the tree without logging. `expect_dir`: Some(true)=rmdir semantics.
    fn detach(&mut self, parent: Ino, name: &str, expect_dir: Option<bool>) -> FsResult<Ino> {
        self.materialize_dir(parent)?;
        let (ino, kind) = *self.nodes[&parent].children.as_ref().unwrap().get(name).ok_or(FsError::NotFound)?;
        match expect_dir {
            Some(true) if kind != FileKind::Dir => return Err(FsError::NotDir),
            Some(false) if kind == FileKind::Dir => return Err(FsError::IsDir),
            _ => {}
        }
        if kind == FileKind::Dir && !self.readdir(ino)?.is_empty() {
            return Err(FsError::NotEmpty);
        }
        let path = self.lower_path(ino);
        self.nodes.get_mut(&parent).unwrap().children.as_mut().unwrap().remove(name);
        self.nodes.get_mut(&parent).unwrap().attr.mtime = Timestamp::now();
        let is_lower = self.nodes.get(&ino).map(|n| n.lower.is_some()).unwrap_or(true);
        if is_lower {
            let size = match self.nodes.get(&ino) {
                Some(n) => self.lower.getattr(n.lower.unwrap()).map(|a| a.size).unwrap_or(n.attr.size),
                None => self.lower.getattr(ino).map(|a| a.size).unwrap_or(0),
            };
            let orig = self.original_path.remove(&ino).unwrap_or(path);
            self.deleted.insert(ino, (orig, size, kind == FileKind::Dir));
        }
        if let Some(n) = self.nodes.get_mut(&ino) {
            n.alive = false;
            n.data = None;
        }
        Ok(ino)
    }

    // ── public API for the UI ──

    pub fn changes(&mut self) -> Vec<ShadowChange> {
        let mut out = vec![];
        let ids: Vec<Ino> = self.nodes.keys().copied().collect();
        for ino in ids {
            let n = self.nodes[&ino].clone();
            if !n.alive || ino == self.lower.root_ino() {
                continue;
            }
            let path = self.lower_path(ino);
            let is_dir = n.attr.kind == FileKind::Dir;
            let new_size = if is_dir { 0 } else { n.data.as_ref().map(|d| d.size).unwrap_or(n.attr.size) };
            match n.lower {
                None => out.push(ShadowChange { path, kind: "added".into(), is_dir, old_size: 0, new_size, delta: new_size as i64 }),
                Some(l) => {
                    let old_size = if is_dir { 0 } else { self.lower.getattr(l).map(|a| a.size).unwrap_or(0) };
                    if let Some(orig) = self.original_path.get(&ino).cloned().filter(|o| *o != path) {
                        out.push(ShadowChange { path: orig, kind: "deleted".into(), is_dir, old_size, new_size: 0, delta: -(old_size as i64) });
                        out.push(ShadowChange { path, kind: "added".into(), is_dir, old_size: 0, new_size, delta: new_size as i64 });
                    } else if n.data.as_ref().map(|d| !d.chunks.is_empty() || d.size != old_size || d.base_limit < old_size).unwrap_or(false)
                        || n.attr_dirty
                    {
                        out.push(ShadowChange { path, kind: "modified".into(), is_dir, old_size, new_size, delta: new_size as i64 - old_size as i64 });
                    }
                }
            }
        }
        for (path, size, is_dir) in self.deleted.values() {
            out.push(ShadowChange { path: path.clone(), kind: "deleted".into(), is_dir: *is_dir, old_size: *size, new_size: 0, delta: -(*size as i64) });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
        out
    }

    pub fn summary(&mut self) -> ShadowSummary {
        let mut s = ShadowSummary::default();
        for c in self.changes() {
            match c.kind.as_str() {
                "added" => s.added += 1,
                "modified" => s.modified += 1,
                _ => s.deleted += 1,
            }
            s.pending_bytes += c.delta;
        }
        s
    }

    pub fn has_changes(&self) -> bool {
        !self.log.is_empty() || self.nodes.values().any(|n| n.data.as_ref().map(|d| !d.chunks.is_empty() || d.base_limit < d.size || n.lower.is_some() && d.base_limit != d.size).unwrap_or(false) || n.attr_dirty)
    }

    fn clear(&mut self) {
        self.nodes.clear();
        self.deleted.clear();
        self.original_path.clear();
        self.log.clear();
        self.stage_len = 0;
        let _ = self.stage.set_len(0);
    }

    /// Discard every pending change. Returns the number of discarded changes.
    pub fn rollback(&mut self) -> u64 {
        let n = self.changes().len() as u64;
        // Overlay-only ids become stale; that's what a rollback means.
        self.clear();
        n
    }

    /// Apply all pending changes to the lower volume. Returns the number of applied changes.
    pub fn commit(&mut self) -> FsResult<u64> {
        let applied = self.changes().len() as u64;
        let mut map: HashMap<Ino, Ino> = HashMap::new();
        let m = |map: &HashMap<Ino, Ino>, alias: &HashMap<Ino, Ino>, i: Ino| -> Ino {
            *map.get(&i).or_else(|| alias.get(&i)).unwrap_or(&i)
        };
        let log = std::mem::take(&mut self.log);
        for (k, op) in log.iter().enumerate() {
            let res = match op {
                Op::Create { parent, name, ino, perm, uid, gid } => {
                    self.lower.create(m(&map, &self.alias, *parent), name, *perm, *uid, *gid).map(|li| {
                        map.insert(*ino, li);
                    })
                }
                Op::Mkdir { parent, name, ino, perm, uid, gid } => {
                    self.lower.mkdir(m(&map, &self.alias, *parent), name, *perm, *uid, *gid).map(|li| {
                        map.insert(*ino, li);
                    })
                }
                Op::Symlink { parent, name, target, ino, uid, gid } => {
                    self.lower.symlink(m(&map, &self.alias, *parent), name, target, *uid, *gid).map(|li| {
                        map.insert(*ino, li);
                    })
                }
                Op::SetAttr { ino, set } => self.lower.setattr(m(&map, &self.alias, *ino), set).map(|_| ()),
                Op::Unlink { parent, name } => self.lower.unlink(m(&map, &self.alias, *parent), name),
                Op::Rmdir { parent, name } => self.lower.rmdir(m(&map, &self.alias, *parent), name),
                Op::Rename { fp, fname, tp, tname } => {
                    self.lower.rename(m(&map, &self.alias, *fp), fname, m(&map, &self.alias, *tp), tname)
                }
            };
            if let Err(e) = res {
                // Keep the unapplied tail so the user can retry after fixing the cause.
                self.log = log[k..].to_vec();
                for (o, l) in &map {
                    self.alias.insert(*o, *l);
                }
                return Err(FsError::Io(format!("commit stopped at operation {} of {}: {e}", k + 1, log.len())));
            }
        }
        // File contents.
        let ids: Vec<Ino> = self.nodes.keys().copied().collect();
        for ino in ids {
            let n = self.nodes[&ino].clone();
            if !n.alive {
                continue;
            }
            let Some(fd) = n.data else { continue };
            let li = m(&map, &self.alias, ino);
            let cur = self.lower.getattr(li)?.size;
            if n.lower.is_some() && fd.chunks.is_empty() && fd.size == cur && fd.base_limit >= cur {
                continue;
            }
            if fd.base_limit < cur {
                self.lower.setattr(li, &SetAttr { size: Some(fd.base_limit), ..Default::default() })?;
            }
            if fd.size != fd.base_limit.min(cur) || fd.size != cur {
                self.lower.setattr(li, &SetAttr { size: Some(fd.size), ..Default::default() })?;
            }
            for (&idx, &off) in &fd.chunks {
                let start = idx * CHUNK;
                if start >= fd.size {
                    continue;
                }
                let len = (fd.size - start).min(CHUNK) as usize;
                let mut buf = vec![0u8; len];
                self.stage.read_exact_at(&mut buf, off)?;
                self.lower.write(li, start, &buf)?;
            }
            self.lower.setattr(li, &SetAttr { mtime: Some(n.attr.mtime), atime: Some(n.attr.atime), ..Default::default() })?;
        }
        self.lower.sync()?;
        for (o, l) in map {
            self.alias.insert(o, l);
        }
        self.clear();
        Ok(applied)
    }
}

impl Drop for ShadowOverlay {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.stage_path);
    }
}

impl FileSystem for ShadowOverlay {
    fn volume_info(&self) -> VolumeInfo {
        let mut v = self.lower.volume_info();
        v.read_only = false;
        v
    }

    fn statfs(&mut self) -> FsResult<FsStat> {
        let mut s = self.lower.statfs()?;
        let pending = self.summary().pending_bytes;
        s.free_bytes = (s.free_bytes as i64 - pending.max(0)).max(0) as u64;
        Ok(s)
    }

    fn root_ino(&self) -> Ino {
        self.lower.root_ino()
    }

    fn lookup(&mut self, parent: Ino, name: &str) -> FsResult<Ino> {
        let parent = self.r(parent);
        if let Some(n) = self.live_node(parent)? {
            if let Some(c) = &n.children {
                return c.get(name).map(|x| x.0).ok_or(FsError::NotFound);
            }
        }
        let i = self.lower.lookup(parent, name)?;
        self.known_parent.insert(i, (parent, name.to_string()));
        Ok(i)
    }

    fn getattr(&mut self, ino: Ino) -> FsResult<Attr> {
        let ino = self.r(ino);
        if let Some(n) = self.live_node(ino)? {
            let mut a = n.attr.clone();
            if let Some(d) = &n.data {
                a.size = d.size;
            }
            if let Some(c) = &n.children {
                a.nlink = 2 + c.values().filter(|(_, k)| *k == FileKind::Dir).count() as u32;
            }
            return Ok(a);
        }
        self.lower.getattr(ino)
    }

    fn setattr(&mut self, ino: Ino, set: &SetAttr) -> FsResult<Attr> {
        let ino = self.r(ino);
        self.ensure_node(ino)?;
        if let Some(sz) = set.size {
            self.copy_up(ino)?;
            let n = self.nodes.get_mut(&ino).unwrap();
            let fd = n.data.as_mut().unwrap();
            if sz < fd.base_limit {
                fd.base_limit = sz;
            }
            let keep_chunks = sz.div_ceil(CHUNK);
            fd.chunks.retain(|&k, _| k < keep_chunks);
            let old = fd.size;
            fd.size = sz;
            // zero the tail of the last partial chunk when shrinking
            if sz < old && sz % CHUNK != 0 {
                let idx = sz / CHUNK;
                if fd.chunks.contains_key(&idx) {
                    let mut c = self.read_chunk(ino, idx)?;
                    c[(sz % CHUNK) as usize..].fill(0);
                    self.write_chunk(ino, idx, &c)?;
                }
            }
            let n = self.nodes.get_mut(&ino).unwrap();
            n.attr.mtime = Timestamp::now();
        }
        let n = self.nodes.get_mut(&ino).unwrap();
        let meta = SetAttr { size: None, ..set.clone() };
        if let Some(p) = meta.perm {
            n.attr.perm = p;
        }
        if let Some(u) = meta.uid {
            n.attr.uid = u;
        }
        if let Some(g) = meta.gid {
            n.attr.gid = g;
        }
        if let Some(t) = meta.atime {
            n.attr.atime = t;
        }
        if let Some(t) = meta.mtime {
            n.attr.mtime = t;
        }
        n.attr.ctime = Timestamp::now();
        if meta.perm.is_some() || meta.uid.is_some() || meta.gid.is_some() {
            n.attr_dirty = true;
            self.log.push(Op::SetAttr { ino, set: SetAttr { perm: meta.perm, uid: meta.uid, gid: meta.gid, ..Default::default() } });
        }
        self.getattr(ino)
    }

    fn read(&mut self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>> {
        let ino = self.r(ino);
        let fd = match self.live_node(ino)? {
            Some(n) if n.data.is_some() => n.data.clone().unwrap(),
            Some(n) if n.attr.kind == FileKind::Dir => return Err(FsError::IsDir),
            Some(n) if n.lower.is_none() => return Ok(vec![]),
            _ => return self.lower.read(ino, offset, len),
        };
        if offset >= fd.size {
            return Ok(vec![]);
        }
        let end = (offset + len as u64).min(fd.size);
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            let idx = pos / CHUNK;
            let c = self.read_chunk(ino, idx)?;
            let s = (pos % CHUNK) as usize;
            let e = ((end - idx * CHUNK).min(CHUNK)) as usize;
            out.extend_from_slice(&c[s..e]);
            pos = idx * CHUNK + e as u64;
        }
        Ok(out)
    }

    fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<u32> {
        let ino = self.r(ino);
        self.copy_up(ino)?;
        let end = offset + data.len() as u64;
        let mut pos = offset;
        while pos < end {
            let idx = pos / CHUNK;
            let mut c = self.read_chunk(ino, idx)?;
            let s = (pos % CHUNK) as usize;
            let n = ((end - pos) as usize).min(CHUNK as usize - s);
            let o = (pos - offset) as usize;
            c[s..s + n].copy_from_slice(&data[o..o + n]);
            self.write_chunk(ino, idx, &c)?;
            pos += n as u64;
        }
        let n = self.nodes.get_mut(&ino).unwrap();
        let fd = n.data.as_mut().unwrap();
        if end > fd.size {
            fd.size = end;
        }
        n.attr.mtime = Timestamp::now();
        n.attr.ctime = n.attr.mtime;
        Ok(data.len() as u32)
    }

    fn readdir(&mut self, dir: Ino) -> FsResult<Vec<DirEntry>> {
        let dir = self.r(dir);
        if let Some(n) = self.live_node(dir)? {
            if n.attr.kind != FileKind::Dir {
                return Err(FsError::NotDir);
            }
            if let Some(c) = &n.children {
                return Ok(c.iter().map(|(name, (ino, kind))| DirEntry { name: name.clone(), ino: *ino, kind: *kind }).collect());
            }
        }
        let v = self.lower.readdir(dir)?;
        for e in &v {
            self.known_parent.insert(e.ino, (dir, e.name.clone()));
        }
        Ok(v)
    }

    fn create(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino> {
        let parent = self.r(parent);
        let ino = self.add_node(parent, name, FileKind::File, perm, uid, gid, None)?;
        self.log.push(Op::Create { parent, name: name.into(), ino, perm, uid, gid });
        Ok(ino)
    }

    fn mkdir(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino> {
        let parent = self.r(parent);
        let ino = self.add_node(parent, name, FileKind::Dir, perm, uid, gid, None)?;
        self.log.push(Op::Mkdir { parent, name: name.into(), ino, perm, uid, gid });
        Ok(ino)
    }

    fn symlink(&mut self, parent: Ino, name: &str, target: &str, uid: u32, gid: u32) -> FsResult<Ino> {
        let parent = self.r(parent);
        let ino = self.add_node(parent, name, FileKind::Symlink, 0o777, uid, gid, Some(target.to_string()))?;
        self.log.push(Op::Symlink { parent, name: name.into(), target: target.into(), ino, uid, gid });
        Ok(ino)
    }

    fn readlink(&mut self, ino: Ino) -> FsResult<String> {
        let ino = self.r(ino);
        if let Some(n) = self.live_node(ino)? {
            if let Some(t) = &n.target {
                return Ok(t.clone());
            }
        }
        self.lower.readlink(ino)
    }

    fn unlink(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        let parent = self.r(parent);
        self.detach(parent, name, Some(false))?;
        self.log.push(Op::Unlink { parent, name: name.into() });
        Ok(())
    }

    fn rmdir(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        let parent = self.r(parent);
        self.detach(parent, name, Some(true))?;
        self.log.push(Op::Rmdir { parent, name: name.into() });
        Ok(())
    }

    fn rename(&mut self, fp: Ino, fname: &str, tp: Ino, tname: &str) -> FsResult<()> {
        let (fp, tp) = (self.r(fp), self.r(tp));
        check_name(tname, 255)?;
        self.materialize_dir(fp)?;
        self.materialize_dir(tp)?;
        let (ino, kind) = *self.nodes[&fp].children.as_ref().unwrap().get(fname).ok_or(FsError::NotFound)?;
        if kind == FileKind::Dir {
            // refuse moving a directory below itself
            let mut cur = tp;
            for _ in 0..512 {
                if cur == ino {
                    return Err(FsError::Invalid("cannot move a directory inside itself".into()));
                }
                if cur == self.lower.root_ino() {
                    break;
                }
                cur = match self.nodes.get(&cur) {
                    Some(n) => n.parent,
                    None => match self.known_parent.get(&cur) {
                        Some((p, _)) => *p,
                        None => break,
                    },
                };
            }
        }
        if let Some(&(dino, dkind)) = self.nodes[&tp].children.as_ref().unwrap().get(tname) {
            if dino == ino {
                return Ok(());
            }
            match (kind == FileKind::Dir, dkind == FileKind::Dir) {
                (true, false) => return Err(FsError::NotDir),
                (false, true) => return Err(FsError::IsDir),
                _ => {}
            }
            self.detach(tp, tname, Some(dkind == FileKind::Dir))?;
        }
        self.ensure_node(ino)?;
        let lower_backed = self.nodes[&ino].lower.is_some();
        if lower_backed && !self.original_path.contains_key(&ino) {
            let p = self.lower_path(ino);
            self.original_path.insert(ino, p);
        }
        self.nodes.get_mut(&fp).unwrap().children.as_mut().unwrap().remove(fname);
        self.nodes.get_mut(&tp).unwrap().children.as_mut().unwrap().insert(tname.to_string(), (ino, kind));
        let n = self.nodes.get_mut(&ino).unwrap();
        n.parent = tp;
        n.name = tname.to_string();
        n.attr.ctime = Timestamp::now();
        self.log.push(Op::Rename { fp, fname: fname.into(), tp, tname: tname.into() });
        Ok(())
    }

    fn sync(&mut self) -> FsResult<()> {
        // Nothing reaches the lower volume until commit.
        Ok(())
    }
    fn dirty_state(&mut self) -> DirtyState {
        self.lower.dirty_state()
    }
    fn repair(&mut self) -> FsResult<String> {
        Err(FsError::Unsupported("repair is not available in Shadow mode".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BlockDev, MemDev};
    use crate::vfs::{ext4_fs, testutil};
    use std::sync::Arc;

    fn setup() -> (Arc<dyn BlockDev>, Vec<u8>) {
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(48 << 20));
        ext4_fs::format(dev.clone(), FsType::Ext4, "s").unwrap();
        let mut fs = ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
        let root = fs.root_ino();
        let d = fs.mkdir(root, "boot", 0o755, 0, 0).unwrap();
        let c = fs.create(d, "config.txt", 0o644, 0, 0).unwrap();
        fs.write(c, 0, b"arm_64bit=1\n").unwrap();
        let big = fs.create(root, "big.bin", 0o644, 0, 0).unwrap();
        fs.write(big, 0, &testutil::pattern(500_000, 9)).unwrap();
        fs.create(root, "obsolete.log", 0o644, 0, 0).unwrap();
        fs.sync().unwrap();
        drop(fs);
        let snap = (dev.as_ref() as &dyn BlockDev).read_vec(0, 48 << 20).unwrap();
        (dev, snap)
    }

    fn stage() -> PathBuf {
        std::env::temp_dir().join(format!("ld-shadow-{}-{:?}.stage", std::process::id(), std::thread::current().id()))
    }

    fn mutate(s: &mut ShadowOverlay) {
        let root = s.root_ino();
        let d = s.lookup(root, "boot").unwrap();
        let c = s.lookup(d, "config.txt").unwrap();
        s.write(c, 12, b"enable_uart=1\n").unwrap();
        let big = s.lookup(root, "big.bin").unwrap();
        s.write(big, 100_000, b"PATCHED").unwrap();
        s.setattr(big, &SetAttr { size: Some(300_000), ..Default::default() }).unwrap();
        s.unlink(root, "obsolete.log").unwrap();
        let n = s.mkdir(root, "new", 0o755, 0, 0).unwrap();
        let f = s.create(n, "ssh", 0o644, 0, 0).unwrap();
        s.write(f, 0, b"").unwrap();
        let w = s.create(n, "wpa.conf", 0o600, 0, 0).unwrap();
        s.write(w, 0, b"network={ssid=\"x\"}\n").unwrap();
        s.rename(n, "wpa.conf", d, "wpa_supplicant.conf").unwrap();
    }

    #[test]
    fn overlay_view_and_rollback_leave_disk_untouched() {
        let (dev, snap) = setup();
        let lower = ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
        let mut s = ShadowOverlay::new(Box::new(lower), stage()).unwrap();
        mutate(&mut s);
        let root = s.root_ino();
        let d = s.lookup(root, "boot").unwrap();
        let c = s.lookup(d, "config.txt").unwrap();
        assert_eq!(s.read(c, 0, 100).unwrap(), b"arm_64bit=1\nenable_uart=1\n");
        let big = s.lookup(root, "big.bin").unwrap();
        let b = s.read(big, 99_995, 20).unwrap();
        assert_eq!(&b[5..12], b"PATCHED");
        assert_eq!(s.getattr(big).unwrap().size, 300_000);
        assert_eq!(s.lookup(root, "obsolete.log"), Err(FsError::NotFound));
        let ch = s.changes();
        let kinds: Vec<(String, String)> = ch.iter().map(|c| (c.path.clone(), c.kind.clone())).collect();
        assert!(kinds.contains(&("/boot/config.txt".into(), "modified".into())), "{kinds:?}");
        assert!(kinds.contains(&("/obsolete.log".into(), "deleted".into())), "{kinds:?}");
        assert!(kinds.contains(&("/boot/wpa_supplicant.conf".into(), "added".into())), "{kinds:?}");
        assert!(kinds.contains(&("/big.bin".into(), "modified".into())), "{kinds:?}");
        let sm = s.summary();
        assert_eq!(sm.deleted, 1);
        // The physical device is bit-for-bit untouched.
        assert!((dev.as_ref() as &dyn BlockDev).read_vec(0, 48 << 20).unwrap() == snap);
        assert!(s.rollback() > 0);
        assert!(s.lookup(root, "obsolete.log").is_ok());
        assert_eq!(s.read(c, 0, 100).unwrap(), b"arm_64bit=1\n");
        assert!(s.changes().is_empty());
        assert!((dev.as_ref() as &dyn BlockDev).read_vec(0, 48 << 20).unwrap() == snap);
    }

    #[test]
    fn commit_applies_everything() {
        let (dev, _) = setup();
        let lower = ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
        let mut s = ShadowOverlay::new(Box::new(lower), stage()).unwrap();
        mutate(&mut s);
        let root = s.root_ino();
        let n = s.lookup(root, "new").unwrap();
        let ssh = s.lookup(n, "ssh").unwrap();
        assert!(s.commit().unwrap() >= 5);
        // old overlay ids still resolve after commit
        assert_eq!(s.getattr(ssh).unwrap().kind, FileKind::File);
        drop(s);
        let mut fs = ext4_fs::Ext4Fs::open(dev, false).unwrap();
        let d = fs.lookup(root, "boot").unwrap();
        let c = fs.lookup(d, "config.txt").unwrap();
        assert_eq!(fs.read(c, 0, 100).unwrap(), b"arm_64bit=1\nenable_uart=1\n");
        let w = fs.lookup(d, "wpa_supplicant.conf").unwrap();
        assert_eq!(fs.read(w, 0, 100).unwrap(), b"network={ssid=\"x\"}\n");
        assert_eq!(fs.getattr(w).unwrap().perm, 0o600);
        let big = fs.lookup(root, "big.bin").unwrap();
        assert_eq!(fs.getattr(big).unwrap().size, 300_000);
        let mut want = testutil::pattern(500_000, 9);
        want.truncate(300_000);
        want[100_000..100_007].copy_from_slice(b"PATCHED");
        assert!(fs.read(big, 0, 400_000).unwrap() == want);
        assert_eq!(fs.lookup(root, "obsolete.log"), Err(FsError::NotFound));
        let nd = fs.lookup(root, "new").unwrap();
        assert!(fs.lookup(nd, "ssh").is_ok());
        assert_eq!(fs.lookup(nd, "wpa.conf"), Err(FsError::NotFound));
    }

    #[test]
    fn shadow_passes_conformance_then_commits() {
        let (dev, _) = setup();
        let lower = ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
        let mut s = ShadowOverlay::new(Box::new(lower), stage()).unwrap();
        let root = s.root_ino();
        s.unlink(root, "big.bin").unwrap(); // the suite creates its own big.bin
        testutil::conformance(&mut s, true);
        s.commit().unwrap();
        drop(s);
        let mut fs = ext4_fs::Ext4Fs::open(dev, false).unwrap();
        testutil::check_persisted(&mut fs);
    }
}
