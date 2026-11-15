use super::*;
use crate::block::{BlockDev, MemDev};
use crate::vfs::{ext4_fs, fat_fs, testutil};
use crate::wal::{Fault, Journaled, WalDev};
use std::path::Path;
use std::sync::{Arc, Mutex};

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
    let docs = fs.mkdir(root, "docs", 0o755, 0, 0).unwrap();
    let a = fs.create(docs, "a.txt", 0o644, 0, 0).unwrap();
    fs.write(a, 0, b"alpha").unwrap();
    let b = fs.create(docs, "b.txt", 0o644, 0, 0).unwrap();
    fs.write(b, 0, b"bravo").unwrap();
    fs.mkdir(docs, "old", 0o755, 0, 0).unwrap();
    fs.sync().unwrap();
    drop(fs);
    let snap = dev.read_vec(0, 48 << 20).unwrap();
    (dev, snap)
}

fn mem_from(snap: &[u8]) -> Arc<dyn BlockDev> {
    let d = MemDev::new(snap.len());
    d.write_at(0, snap).unwrap();
    Arc::new(d)
}

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("ld-shadow-{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn stage() -> PathBuf {
    tmp("eph").join("x.stage")
}

fn ident(fs: &str) -> Identity {
    Identity { kind: "local".into(), fs_type: fs.into(), uuid: "test".into(), source_path: "/test.img".into(), ..Default::default() }
}

fn mutate(s: &mut dyn FileSystem) {
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

/// A broader scenario: directory renames, rename over an existing file, delete + recreate,
/// sparse writes, chmod, symlinks.
fn scenario(s: &mut dyn FileSystem, symlinks: bool) {
    mutate(s);
    let root = s.root_ino();
    let docs = s.lookup(root, "docs").unwrap();
    // replace b.txt by a new file through a rename (safe-save pattern)
    let t = s.create(docs, ".b.tmp", 0o644, 0, 0).unwrap();
    s.write(t, 0, b"bravo v2").unwrap();
    s.rename(docs, ".b.tmp", docs, "b.txt").unwrap();
    // delete and recreate the same name
    s.unlink(docs, "a.txt").unwrap();
    let a = s.create(docs, "a.txt", 0o640, 0, 0).unwrap();
    s.write(a, 0, b"alpha reborn").unwrap();
    // move a directory, then create inside it
    s.rename(docs, "old", root, "archive").unwrap();
    let arch = s.lookup(root, "archive").unwrap();
    let sp = s.create(arch, "sparse.bin", 0o644, 0, 0).unwrap();
    s.write(sp, 3 * 1024 * 1024 + 17, b"tail").unwrap();
    s.write(sp, 70_000, &testutil::pattern(150_000, 3)).unwrap();
    // rename the parent of modified files
    s.rename(root, "docs", root, "documents").unwrap();
    let documents = s.lookup(root, "documents").unwrap();
    let bt = s.lookup(documents, "b.txt").unwrap();
    s.setattr(bt, &SetAttr { perm: Some(0o600), ..Default::default() }).unwrap();
    if symlinks {
        s.symlink(arch, "link", "../documents/b.txt", 0, 0).unwrap();
    }
    // many small files
    let many = s.mkdir(root, "many", 0o755, 0, 0).unwrap();
    for i in 0..12 {
        let f = s.create(many, &format!("f{i}.txt"), 0o644, 0, 0).unwrap();
        s.write(f, 0, format!("file {i}\n").as_bytes()).unwrap();
    }
    s.unlink(many, "f3.txt").unwrap();
    s.rename(many, "f4.txt", many, "f5.txt").unwrap(); // over an existing file
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    path: String,
    kind: FileKind,
    perm: u16,
    size: u64,
    data: Vec<u8>,
    target: Option<String>,
}

/// Content snapshot of a volume (no timestamps / inode numbers).
fn tree(fs: &mut dyn FileSystem) -> Vec<Entry> {
    fn walk(fs: &mut dyn FileSystem, dir: Ino, path: &str, out: &mut Vec<Entry>) {
        let mut es = fs.readdir(dir).unwrap();
        es.sort_by(|a, b| a.name.cmp(&b.name));
        for e in es {
            if e.name == "lost+found" {
                continue;
            }
            let p = format!("{path}/{}", e.name);
            let a = fs.getattr(e.ino).unwrap();
            let data = if a.kind == FileKind::File { fs.read(e.ino, 0, a.size as u32 + 10).unwrap() } else { vec![] };
            let target = (a.kind == FileKind::Symlink).then(|| fs.readlink(e.ino).unwrap());
            out.push(Entry { path: p.clone(), kind: a.kind, perm: a.perm, size: a.size, data, target });
            if a.kind == FileKind::Dir {
                walk(fs, e.ino, &p, out);
            }
        }
    }
    let mut out = vec![];
    let r = fs.root_ino();
    walk(fs, r, "", &mut out);
    out
}

/// Full overlay view including inode numbers and every attribute (replay equivalence).
fn full_view(fs: &mut ShadowOverlay) -> (Vec<(String, Attr, Vec<u8>, Option<String>)>, Vec<ShadowChange>) {
    fn walk(fs: &mut ShadowOverlay, dir: Ino, path: &str, out: &mut Vec<(String, Attr, Vec<u8>, Option<String>)>) {
        let mut es = fs.readdir(dir).unwrap();
        es.sort_by(|a, b| a.name.cmp(&b.name));
        for e in es {
            let p = format!("{path}/{}", e.name);
            let a = fs.getattr(e.ino).unwrap();
            let data = if a.kind == FileKind::File { fs.read(e.ino, 0, a.size as u32 + 10).unwrap() } else { vec![] };
            let t = (a.kind == FileKind::Symlink).then(|| fs.readlink(e.ino).unwrap());
            out.push((p.clone(), a.clone(), data, t));
            if a.kind == FileKind::Dir {
                walk(fs, e.ino, &p, out);
            }
        }
    }
    let mut out = vec![];
    let r = fs.root_ino();
    let ra = fs.getattr(r).unwrap();
    out.push(("/".into(), ra, vec![], None));
    walk(fs, r, "", &mut out);
    let ch = fs.changes();
    (out, ch)
}

// ───────────────────────────── original behaviour ─────────────────────────────

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
    assert!(dev.read_vec(0, 48 << 20).unwrap() == snap);
    assert!(s.rollback() > 0);
    assert!(s.lookup(root, "obsolete.log").is_ok());
    assert_eq!(s.read(c, 0, 100).unwrap(), b"arm_64bit=1\n");
    assert!(s.changes().is_empty());
    assert!(dev.read_vec(0, 48 << 20).unwrap() == snap);
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

// ───────────────────────────── persistence ─────────────────────────────

fn open_ext4(dev: &Arc<dyn BlockDev>) -> Box<dyn FileSystem> {
    Box::new(ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap())
}

fn session(dev: &Arc<dyn BlockDev>, dir: &Path) -> ShadowOverlay {
    let mut s = ShadowOverlay::open_session(open_ext4(dev), dir.to_path_buf(), ident("EXT4")).unwrap();
    s.flush_interval = Duration::from_secs(3600); // tests decide when frames are written
    s
}

#[test]
fn persistent_session_survives_crash_and_commits() {
    let (dev, snap) = setup();
    let dir = tmp("persist").join("s");
    let mut s = session(&dev, &dir);
    scenario(&mut s, true);
    s.sync().unwrap();
    let before = full_view(&mut s);
    let want_summary = s.summary_counts();
    s.simulate_crash();
    assert!(dev.read_vec(0, 48 << 20).unwrap() == snap, "shadow must not touch the disk");
    let m = Manifest::load(&dir).unwrap();
    assert_eq!(m.state, "pending");
    let mut r = session(&dev, &dir);
    assert!(r.summary().restored);
    let after = full_view(&mut r);
    assert_eq!(before.0, after.0);
    assert_eq!(before.1, after.1);
    assert_eq!(r.summary_counts().added, want_summary.added);
    // clean commit result equals the reference produced by an in-memory overlay
    let reference = {
        let d2 = mem_from(&snap);
        let mut e = ShadowOverlay::new(open_ext4(&d2), stage()).unwrap();
        scenario(&mut e, true);
        e.commit().unwrap();
        drop(e);
        tree(open_ext4(&d2).as_mut())
    };
    r.commit().unwrap();
    assert!(!r.summary().restored);
    drop(r);
    assert!(!dir.exists(), "the session is deleted after a successful commit");
    assert_eq!(tree(open_ext4(&dev).as_mut()), reference);
}

#[test]
fn clean_unmount_without_changes_leaves_nothing_and_with_changes_keeps_them() {
    let (dev, _) = setup();
    let dir = tmp("lifecycle").join("s");
    let mut s = session(&dev, &dir);
    let root = s.root_ino();
    let f = s.create(root, "tmp", 0o644, 0, 0).unwrap();
    s.write(f, 0, b"x").unwrap();
    s.unlink(root, "tmp").unwrap();
    drop(s); // net zero changes
    assert!(!dir.exists());
    let mut s = session(&dev, &dir);
    s.create(root, "keep", 0o644, 0, 0).unwrap();
    drop(s);
    let m = Manifest::load(&dir).unwrap();
    assert_eq!(m.counts.added, 1);
    let mut s = session(&dev, &dir);
    assert!(s.lookup(root, "keep").is_ok());
    assert_eq!(s.rollback(), 1);
    drop(s);
    assert!(!dir.exists(), "rollback deletes the session");
}

/// xorshift for reproducible random operation sequences
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn collect(fs: &mut dyn FileSystem) -> (Vec<Ino>, Vec<(Ino, Ino, String, FileKind)>) {
    let mut dirs = vec![fs.root_ino()];
    let mut ents = vec![];
    let mut i = 0;
    while i < dirs.len() {
        let d = dirs[i];
        for e in fs.readdir(d).unwrap() {
            if e.name == "lost+found" {
                continue;
            }
            if e.kind == FileKind::Dir {
                dirs.push(e.ino);
            }
            ents.push((d, e.ino, e.name, e.kind));
        }
        i += 1;
    }
    (dirs, ents)
}

fn random_op(s: &mut dyn FileSystem, rng: &mut Rng) {
    const NAMES: [&str; 8] = ["a", "b", "c", "d", "Ä", "e.txt", "f", "g"];
    let (dirs, ents) = collect(s);
    let files: Vec<_> = ents.iter().filter(|e| e.3 == FileKind::File).cloned().collect();
    let dir = dirs[rng.below(dirs.len() as u64) as usize];
    let name = NAMES[rng.below(NAMES.len() as u64) as usize];
    match rng.below(12) {
        0 => {
            let _ = s.create(dir, name, 0o644, 1000, 1000);
        }
        1 => {
            let _ = s.mkdir(dir, name, 0o755, 1000, 1000);
        }
        2 => {
            let _ = s.symlink(dir, name, "../target", 1000, 1000);
        }
        3 | 4 if !files.is_empty() => {
            let f = &files[rng.below(files.len() as u64) as usize];
            let off = if rng.below(5) == 0 { 2 * 1024 * 1024 + rng.below(100_000) } else { rng.below(300_000) };
            let len = 1 + rng.below(150_000) as usize;
            let _ = s.write(f.1, off, &testutil::pattern(len, rng.next() as u32));
        }
        5 if !files.is_empty() => {
            let f = &files[rng.below(files.len() as u64) as usize];
            let _ = s.setattr(f.1, &SetAttr { size: Some(rng.below(400_000)), ..Default::default() });
        }
        6 if !files.is_empty() => {
            let f = &files[rng.below(files.len() as u64) as usize];
            let _ = s.setattr(f.1, &SetAttr { perm: Some(0o600 + rng.below(0o100) as u16), mtime: Some(Timestamp { secs: 1_600_000_000, nsecs: 5 }), ..Default::default() });
        }
        7 if !ents.is_empty() => {
            let e = &ents[rng.below(ents.len() as u64) as usize];
            let _ = if e.3 == FileKind::Dir { s.rmdir(e.0, &e.2) } else { s.unlink(e.0, &e.2) };
        }
        8 | 9 if !ents.is_empty() => {
            let e = &ents[rng.below(ents.len() as u64) as usize];
            let _ = s.rename(e.0, &e.2, dir, name);
        }
        10 if !files.is_empty() => {
            // delete then recreate the same name
            let f = &files[rng.below(files.len() as u64) as usize];
            if s.unlink(f.0, &f.2).is_ok() {
                let n = s.create(f.0, &f.2, 0o644, 0, 0).unwrap();
                let _ = s.write(n, 0, b"recreated");
            }
        }
        _ => {
            let _ = s.create(dir, &format!("n{}", rng.below(1000)), 0o644, 0, 0);
        }
    }
}

#[test]
fn replay_reproduces_random_sessions_exactly() {
    for seed in 1..=6u64 {
        let (dev, _) = setup();
        let dir = tmp("replay").join("s");
        let mut s = session(&dev, &dir);
        let mut rng = Rng(seed * 0x9E37_79B9_7F4A_7C15);
        for i in 0..250 {
            random_op(&mut s, &mut rng);
            if rng.below(15) == 0 || i == 249 {
                s.sync().unwrap();
            }
        }
        let before = full_view(&mut s);
        let next = s.next;
        s.simulate_crash();
        let mut r = session(&dev, &dir);
        assert_eq!(r.next, next, "seed {seed}: overlay id counter");
        let after = full_view(&mut r);
        assert_eq!(before.0.len(), after.0.len(), "seed {seed}");
        for (a, b) in before.0.iter().zip(after.0.iter()) {
            assert_eq!(a, b, "seed {seed}");
        }
        assert_eq!(before.1, after.1, "seed {seed}");
        // the restored session keeps working and commits like the original would
        r.commit().unwrap();
        drop(r);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}

#[test]
fn crash_recovers_a_consistent_prefix_and_torn_tail_is_ignored() {
    let (dev, _) = setup();
    let dir = tmp("prefix").join("s");
    let mut s = session(&dev, &dir);
    let mut rng = Rng(77);
    let mut views = vec![];
    for _ in 0..40 {
        random_op(&mut s, &mut rng);
        s.force_flush();
        views.push(full_view(&mut s));
    }
    // ops after the last flush are lost on a crash
    for _ in 0..10 {
        random_op(&mut s, &mut rng);
    }
    s.simulate_crash();
    let mut r = session(&dev, &dir);
    assert_eq!(full_view(&mut r).0, views.last().unwrap().0);
    r.simulate_crash();
    // tear the last frame: the state is the one before the last flushed op
    let j = dir.join(store::JOURNAL);
    let len = std::fs::metadata(&j).unwrap().len();
    std::fs::OpenOptions::new().write(true).open(&j).unwrap().set_len(len - 5).unwrap();
    let mut r = session(&dev, &dir);
    let v = full_view(&mut r);
    // the torn frame may hold a no-op (failed ops are not journaled): find the matching view
    assert!(views.iter().rev().take(3).any(|x| x.0 == v.0), "state is not a prefix");
    r.simulate_crash();
    // garbage after the end is ignored too
    let mut raw = std::fs::read(&j).unwrap();
    raw.extend_from_slice(b"LDSJ\xff\xff\x00\x00garbage");
    std::fs::write(&j, raw).unwrap();
    let mut r = session(&dev, &dir);
    assert_eq!(full_view(&mut r).0, v.0);
}

#[test]
fn copy_on_write_never_overwrites_durable_chunks() {
    let (dev, _) = setup();
    let dir = tmp("cow").join("s");
    let mut s = session(&dev, &dir);
    let root = s.root_ino();
    let f = s.create(root, "f", 0o644, 0, 0).unwrap();
    s.write(f, 0, &[1u8; 100_000]).unwrap();
    s.force_flush();
    let durable = full_view(&mut s);
    // overwrite in place after the flush, then crash before the next flush
    s.write(f, 10, &[2u8; 50_000]).unwrap();
    s.setattr(f, &SetAttr { size: Some(20), ..Default::default() }).unwrap();
    s.simulate_crash();
    let mut r = session(&dev, &dir);
    assert_eq!(full_view(&mut r).0, durable.0);
    assert_eq!(r.read(f, 0, 200_000).unwrap(), vec![1u8; 100_000]);
}

/// Hides the lower fingerprint (to exercise the per-object checks alone).
struct NoFp(Box<dyn FileSystem>);
macro_rules! fwd {
    ($t:ty) => {
        impl FileSystem for $t {
            fn volume_info(&self) -> VolumeInfo { self.0.volume_info() }
            fn statfs(&mut self) -> FsResult<FsStat> { self.0.statfs() }
            fn root_ino(&self) -> Ino { self.0.root_ino() }
            fn lookup(&mut self, p: Ino, n: &str) -> FsResult<Ino> { self.0.lookup(p, n) }
            fn getattr(&mut self, i: Ino) -> FsResult<Attr> { self.0.getattr(i) }
            fn setattr(&mut self, i: Ino, s: &SetAttr) -> FsResult<Attr> { self.0.setattr(i, s) }
            fn read(&mut self, i: Ino, o: u64, l: u32) -> FsResult<Vec<u8>> { self.0.read(i, o, l) }
            fn write(&mut self, i: Ino, o: u64, d: &[u8]) -> FsResult<u32> { self.0.write(i, o, d) }
            fn readdir(&mut self, d: Ino) -> FsResult<Vec<DirEntry>> { self.0.readdir(d) }
            fn create(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> { self.0.create(p, n, m, u, g) }
            fn mkdir(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> { self.0.mkdir(p, n, m, u, g) }
            fn symlink(&mut self, p: Ino, n: &str, t: &str, u: u32, g: u32) -> FsResult<Ino> { self.0.symlink(p, n, t, u, g) }
            fn readlink(&mut self, i: Ino) -> FsResult<String> { self.0.readlink(i) }
            fn unlink(&mut self, p: Ino, n: &str) -> FsResult<()> { self.0.unlink(p, n) }
            fn rmdir(&mut self, p: Ino, n: &str) -> FsResult<()> { self.0.rmdir(p, n) }
            fn rename(&mut self, a: Ino, b: &str, c: Ino, d: &str) -> FsResult<()> { self.0.rename(a, b, c, d) }
            fn sync(&mut self) -> FsResult<()> { self.0.sync() }
            fn dirty_state(&mut self) -> DirtyState { self.0.dirty_state() }
            fn repair(&mut self) -> FsResult<String> { self.0.repair() }
        }
    };
}
fwd!(NoFp);

#[test]
fn drift_on_the_lower_volume_refuses_recovery() {
    let (dev, _) = setup();
    // 1. global fingerprint: any write by another system changes the ext4 superblock
    let dir = tmp("drift").join("s");
    let mut s = session(&dev, &dir);
    let root = s.root_ino();
    s.create(root, "mine", 0o644, 0, 0).unwrap();
    drop(s);
    {
        let mut fs = ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
        fs.create(root, "from-linux", 0o644, 0, 0).unwrap();
        fs.sync().unwrap();
    }
    let e = ShadowOverlay::open_session(open_ext4(&dev), dir.clone(), ident("EXT4")).err().expect("must refuse");
    assert!(e.contains("modified elsewhere") || e.contains("modificato altrove"), "{e}");
    assert!(Manifest::load(&dir).unwrap().drift.is_some());
    // stays refused (only Discard), and nothing was deleted
    assert!(ShadowOverlay::open_session(open_ext4(&dev), dir.clone(), ident("EXT4")).is_err());
    assert!(dir.join(store::JOURNAL).exists());

    // 2. per-object check: a file the session read was changed in place
    let (dev, _) = setup();
    let dir = tmp("drift2").join("s");
    let mut s = ShadowOverlay::open_session(Box::new(NoFp(open_ext4(&dev))), dir.clone(), ident("EXT4")).unwrap();
    let d = s.lookup(root, "boot").unwrap();
    let c = s.lookup(d, "config.txt").unwrap();
    s.write(c, 0, b"ARM").unwrap();
    drop(s);
    {
        let mut fs = ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
        fs.write(c, 0, b"xyz").unwrap();
        fs.setattr(c, &SetAttr { mtime: Some(Timestamp { secs: 42, nsecs: 0 }), ..Default::default() }).unwrap();
        fs.sync().unwrap();
    }
    let e = ShadowOverlay::open_session(Box::new(NoFp(open_ext4(&dev))), dir.clone(), ident("EXT4")).err().expect("must refuse");
    assert!(e.contains("config.txt"), "{e}");
}

#[test]
fn fingerprints_are_stable_across_plain_mounts() {
    let defaults = MountDefaults { uid: 501, gid: 20, file_perm: 0o644, dir_perm: 0o755 };
    for t in [FsType::Ext4, FsType::Fat32, FsType::ExFat, FsType::Ntfs] {
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(64 << 20));
        crate::vfs::format(dev.clone(), t, "FP").unwrap();
        {
            let mut fs = open_fs(dev.clone(), false, defaults).unwrap();
            let r = fs.root_ino();
            let d = fs.mkdir(r, "dir", 0o755, 0, 0).unwrap();
            let f = fs.create(d, "f.bin", 0o644, 0, 0).unwrap();
            fs.write(f, 0, &testutil::pattern(200_000, 1)).unwrap();
            fs.sync().unwrap();
        }
        let fp = |dev: &Arc<dyn BlockDev>| open_fs(dev.clone(), false, defaults).unwrap().fingerprint().unwrap();
        let a = fp(&dev);
        let image = dev.read_vec(0, 64 << 20).unwrap();
        // a Shadow session that reads and stages writes, then unmounts
        {
            let dir = tmp("fp").join("s");
            let lower = open_fs(dev.clone(), false, defaults).unwrap();
            let mut s = ShadowOverlay::open_session(lower, dir, ident(t.label())).unwrap();
            let r = s.root_ino();
            let d = s.lookup(r, "dir").unwrap();
            let f = s.lookup(d, "f.bin").unwrap();
            s.read(f, 0, 1000).unwrap();
            s.write(f, 5, b"staged").unwrap();
            s.create(r, "new", 0o644, 0, 0).unwrap();
            s.sync().unwrap();
        }
        assert!(dev.read_vec(0, 64 << 20).unwrap() == image, "{t:?}: a Shadow mount wrote to the disk");
        assert_eq!(fp(&dev), a, "{t:?}");
        // a real change is noticed
        {
            let mut fs = open_fs(dev.clone(), false, defaults).unwrap();
            let r = fs.root_ino();
            fs.create(r, "other-os", 0o644, 0, 0).unwrap();
            fs.sync().unwrap();
        }
        assert_ne!(fp(&dev), a, "{t:?}");
    }
}

// ───────────────────────────── resumable commit ─────────────────────────────

#[derive(Default)]
struct FaultCtl {
    count: usize,
    fail_at: Option<(usize, FsError)>,
    crash_at: Option<usize>,
    dead: bool,
    sync_each: bool,
    wal: Option<Arc<WalDev>>,
}

/// Lower wrapper counting mutating calls: fails the N-th, or "crashes" there (the WAL loses
/// everything not yet flushed and every later call fails).
struct FaultFs {
    inner: Box<dyn FileSystem>,
    ctl: Arc<Mutex<FaultCtl>>,
}

impl FaultFs {
    fn hit(&mut self) -> FsResult<()> {
        let mut c = self.ctl.lock().unwrap();
        if c.dead {
            return Err(FsError::Io("dead".into()));
        }
        let n = c.count;
        c.count += 1;
        if c.fail_at.as_ref().map(|f| f.0) == Some(n) {
            return Err(c.fail_at.take().unwrap().1);
        }
        if c.crash_at == Some(n) {
            c.dead = true;
            if let Some(w) = &c.wal {
                w.inject_fault(Fault::CrashBeforeWal);
            }
            return Err(FsError::Io("crash".into()));
        }
        Ok(())
    }
    fn after<T>(&mut self, r: FsResult<T>) -> FsResult<T> {
        if r.is_ok() && self.ctl.lock().unwrap().sync_each {
            self.inner.sync()?;
        }
        r
    }
    fn alive(&self) -> FsResult<()> {
        if self.ctl.lock().unwrap().dead {
            return Err(FsError::Io("dead".into()));
        }
        Ok(())
    }
}

impl FileSystem for FaultFs {
    fn volume_info(&self) -> VolumeInfo {
        self.inner.volume_info()
    }
    fn statfs(&mut self) -> FsResult<FsStat> {
        self.inner.statfs()
    }
    fn root_ino(&self) -> Ino {
        self.inner.root_ino()
    }
    fn lookup(&mut self, p: Ino, n: &str) -> FsResult<Ino> {
        self.alive()?;
        self.inner.lookup(p, n)
    }
    fn getattr(&mut self, i: Ino) -> FsResult<Attr> {
        self.alive()?;
        self.inner.getattr(i)
    }
    fn setattr(&mut self, i: Ino, s: &SetAttr) -> FsResult<Attr> {
        self.hit()?;
        let r = self.inner.setattr(i, s);
        self.after(r)
    }
    fn read(&mut self, i: Ino, o: u64, l: u32) -> FsResult<Vec<u8>> {
        self.alive()?;
        self.inner.read(i, o, l)
    }
    fn write(&mut self, i: Ino, o: u64, d: &[u8]) -> FsResult<u32> {
        self.hit()?;
        let r = self.inner.write(i, o, d);
        self.after(r)
    }
    fn readdir(&mut self, d: Ino) -> FsResult<Vec<DirEntry>> {
        self.alive()?;
        self.inner.readdir(d)
    }
    fn create(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> {
        self.hit()?;
        let r = self.inner.create(p, n, m, u, g);
        self.after(r)
    }
    fn mkdir(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> {
        self.hit()?;
        let r = self.inner.mkdir(p, n, m, u, g);
        self.after(r)
    }
    fn symlink(&mut self, p: Ino, n: &str, t: &str, u: u32, g: u32) -> FsResult<Ino> {
        self.hit()?;
        let r = self.inner.symlink(p, n, t, u, g);
        self.after(r)
    }
    fn readlink(&mut self, i: Ino) -> FsResult<String> {
        self.alive()?;
        self.inner.readlink(i)
    }
    fn unlink(&mut self, p: Ino, n: &str) -> FsResult<()> {
        self.hit()?;
        let r = self.inner.unlink(p, n);
        self.after(r)
    }
    fn rmdir(&mut self, p: Ino, n: &str) -> FsResult<()> {
        self.hit()?;
        let r = self.inner.rmdir(p, n);
        self.after(r)
    }
    fn rename(&mut self, a: Ino, b: &str, c: Ino, d: &str) -> FsResult<()> {
        self.hit()?;
        let r = self.inner.rename(a, b, c, d);
        self.after(r)
    }
    fn sync(&mut self) -> FsResult<()> {
        self.alive()?;
        self.inner.sync()
    }
    fn dirty_state(&mut self) -> DirtyState {
        self.inner.dirty_state()
    }
    fn repair(&mut self) -> FsResult<String> {
        self.inner.repair()
    }
    fn fingerprint(&mut self) -> Option<String> {
        self.inner.fingerprint()
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Ext4,
    Fat32,
}

struct Rig {
    dev: Arc<dyn BlockDev>,
    wal_path: PathBuf,
    dir: PathBuf,
    kind: Kind,
}

impl Rig {
    fn new(kind: Kind, snap: &[u8]) -> Rig {
        let base = tmp("rig");
        Rig { dev: mem_from(snap), wal_path: base.join("v.wal"), dir: base.join("s"), kind }
    }
    /// The production stack: device → WAL → driver → Journaled → fault injector.
    fn lower(&self, ctl: &Arc<Mutex<FaultCtl>>) -> Box<dyn FileSystem> {
        let wal = WalDev::open(self.dev.clone(), &self.wal_path, "test").unwrap();
        let driver: Box<dyn FileSystem> = match self.kind {
            Kind::Ext4 => Box::new(ext4_fs::Ext4Fs::open(wal.clone(), false).unwrap()),
            Kind::Fat32 => Box::new(fat_fs::FatFs::open(wal.clone(), false, MountDefaults { uid: 0, gid: 0, file_perm: 0o644, dir_perm: 0o755 }).unwrap()),
        };
        ctl.lock().unwrap().wal = Some(wal.clone());
        Box::new(FaultFs { inner: Box::new(Journaled { inner: driver, wal }), ctl: ctl.clone() })
    }
    fn open(&self, ctl: &Arc<Mutex<FaultCtl>>) -> ShadowOverlay {
        let fs = match self.kind {
            Kind::Ext4 => "EXT4",
            Kind::Fat32 => "FAT32",
        };
        let mut s = ShadowOverlay::open_session(self.lower(ctl), self.dir.clone(), ident(fs)).unwrap();
        s.flush_interval = Duration::from_secs(3600);
        s
    }
    fn plain(&self) -> Box<dyn FileSystem> {
        match self.kind {
            Kind::Ext4 => open_ext4(&self.dev),
            Kind::Fat32 => Box::new(fat_fs::FatFs::open(self.dev.clone(), false, MountDefaults { uid: 0, gid: 0, file_perm: 0o644, dir_perm: 0o755 }).unwrap()),
        }
    }
}

fn fat_image() -> Vec<u8> {
    let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(48 << 20));
    fat_fs::format(dev.clone(), FsType::Fat32, "SHADOW").unwrap();
    let mut fs = fat_fs::FatFs::open(dev.clone(), false, MountDefaults { uid: 0, gid: 0, file_perm: 0o644, dir_perm: 0o755 }).unwrap();
    let root = fs.root_ino();
    let d = fs.mkdir(root, "boot", 0o755, 0, 0).unwrap();
    let c = fs.create(d, "config.txt", 0o644, 0, 0).unwrap();
    fs.write(c, 0, b"arm_64bit=1\n").unwrap();
    let big = fs.create(root, "big.bin", 0o644, 0, 0).unwrap();
    fs.write(big, 0, &testutil::pattern(500_000, 9)).unwrap();
    fs.create(root, "obsolete.log", 0o644, 0, 0).unwrap();
    let docs = fs.mkdir(root, "docs", 0o755, 0, 0).unwrap();
    let a = fs.create(docs, "a.txt", 0o644, 0, 0).unwrap();
    fs.write(a, 0, b"alpha").unwrap();
    let b = fs.create(docs, "b.txt", 0o644, 0, 0).unwrap();
    fs.write(b, 0, b"bravo").unwrap();
    fs.mkdir(docs, "old", 0o755, 0, 0).unwrap();
    fs.sync().unwrap();
    drop(fs);
    dev.read_vec(0, 48 << 20).unwrap()
}

/// Final lower tree of a clean, uninterrupted commit of `scenario`.
fn reference(kind: Kind, snap: &[u8]) -> (Vec<Entry>, usize) {
    let rig = Rig::new(kind, snap);
    let ctl = Arc::new(Mutex::new(FaultCtl::default()));
    let mut s = rig.open(&ctl);
    scenario(&mut s, matches!(kind, Kind::Ext4));
    s.sync().unwrap();
    ctl.lock().unwrap().count = 0;
    s.commit().unwrap();
    let ops = ctl.lock().unwrap().count;
    drop(s);
    (tree(rig.plain().as_mut()), ops)
}

fn crash_and_resume(kind: Kind, snap: &[u8], want: &[Entry], at: usize, sync_each: bool) {
    let rig = Rig::new(kind, snap);
    let ctl = Arc::new(Mutex::new(FaultCtl::default()));
    let mut s = rig.open(&ctl);
    scenario(&mut s, matches!(kind, Kind::Ext4));
    s.sync().unwrap();
    {
        let mut c = ctl.lock().unwrap();
        c.count = 0;
        c.crash_at = Some(at);
        c.sync_each = sync_each;
    }
    assert!(s.commit().is_err(), "crash at {at} did not interrupt the commit");
    s.simulate_crash();
    let ctl = Arc::new(Mutex::new(FaultCtl::default()));
    let mut r = rig.open(&ctl);
    assert!(r.commit_pending(), "crash at {at}: the commit must be resumable");
    assert_eq!(r.summary().state, "committing");
    assert!(r.create(r.root_ino(), "x", 0o644, 0, 0).is_err(), "writes are refused until the commit is finished");
    r.commit().unwrap_or_else(|e| panic!("resume after crash at {at} (sync_each={sync_each}): {e}"));
    drop(r);
    assert!(!rig.dir.exists());
    let got = tree(rig.plain().as_mut());
    if got != want {
        let gp: Vec<_> = got.iter().map(|e| (&e.path, e.size, e.perm)).collect();
        let wp: Vec<_> = want.iter().map(|e| (&e.path, e.size, e.perm)).collect();
        panic!("crash at {at} (sync_each={sync_each}): final tree differs\n got: {gp:?}\nwant: {wp:?}");
    }
    if matches!(kind, Kind::Ext4) {
        // the volume is healthy: nothing to repair, and it still takes writes
        let mut fs = rig.plain();
        assert_eq!(fs.dirty_state(), DirtyState::Clean);
        let r = fs.root_ino();
        let f = fs.create(r, "after", 0o644, 0, 0).unwrap();
        fs.write(f, 0, &testutil::pattern(300_000, 5)).unwrap();
        fs.sync().unwrap();
    }
    let _ = std::fs::remove_dir_all(rig.dir.parent().unwrap());
}

#[test]
fn commit_resumes_after_a_crash_at_every_step_ext4() {
    let (_, snap) = setup();
    let (want, ops) = reference(Kind::Ext4, &snap);
    assert!(ops > 30, "{ops}");
    for at in 0..ops {
        crash_and_resume(Kind::Ext4, &snap, &want, at, false);
        crash_and_resume(Kind::Ext4, &snap, &want, at, true);
    }
}

#[test]
fn commit_resumes_after_a_crash_at_every_step_fat32() {
    // FAT inode numbers change across mounts: the resume must find objects by path.
    let snap = fat_image();
    let (want, ops) = reference(Kind::Fat32, &snap);
    for at in 0..ops {
        crash_and_resume(Kind::Fat32, &snap, &want, at, false);
        crash_and_resume(Kind::Fat32, &snap, &want, at, true);
    }
}

#[test]
fn commit_retries_after_an_error_at_every_step() {
    let (_, snap) = setup();
    let (want, ops) = reference(Kind::Ext4, &snap);
    for at in 0..ops {
        let rig = Rig::new(Kind::Ext4, &snap);
        let ctl = Arc::new(Mutex::new(FaultCtl::default()));
        let mut s = rig.open(&ctl);
        scenario(&mut s, true);
        {
            let mut c = ctl.lock().unwrap();
            c.count = 0;
            c.fail_at = Some((at, FsError::NoSpace));
        }
        let e = s.commit().expect_err("injected failure");
        assert!(e.to_string().contains("no space"), "{e}");
        let sm = s.summary();
        assert_eq!(sm.state, "committing");
        assert!(sm.commit_error.is_some());
        assert!(sm.commit_applied <= sm.commit_total);
        assert_eq!(Manifest::load(&rig.dir).unwrap().state, "committing");
        s.commit().unwrap_or_else(|e| panic!("retry after failure at {at}: {e}"));
        drop(s);
        assert_eq!(tree(rig.plain().as_mut()), want, "failure at {at}");
        let _ = std::fs::remove_dir_all(rig.dir.parent().unwrap());
    }
}

#[test]
fn disk_full_in_content_phase_then_retry_then_crash_then_resume() {
    let (_, snap) = setup();
    let (want, ops) = reference(Kind::Ext4, &snap);
    let rig = Rig::new(Kind::Ext4, &snap);
    let ctl = Arc::new(Mutex::new(FaultCtl::default()));
    let mut s = rig.open(&ctl);
    scenario(&mut s, true);
    {
        let mut c = ctl.lock().unwrap();
        c.count = 0;
        c.fail_at = Some((ops - 3, FsError::NoSpace)); // inside the content phase
    }
    assert!(s.commit().is_err());
    // the failed retry crashes again further on
    {
        let mut c = ctl.lock().unwrap();
        c.crash_at = Some(c.count + 1);
    }
    assert!(s.commit().is_err());
    s.simulate_crash();
    let ctl = Arc::new(Mutex::new(FaultCtl::default()));
    let mut r = rig.open(&ctl);
    r.commit().unwrap();
    drop(r);
    assert_eq!(tree(rig.plain().as_mut()), want);
}

#[test]
fn abandoning_an_interrupted_commit_keeps_what_was_applied() {
    let (_, snap) = setup();
    let rig = Rig::new(Kind::Ext4, &snap);
    let ctl = Arc::new(Mutex::new(FaultCtl::default()));
    let mut s = rig.open(&ctl);
    mutate(&mut s);
    {
        let mut c = ctl.lock().unwrap();
        c.count = 0;
        c.fail_at = Some((3, FsError::Io("unplugged".into())));
    }
    assert!(s.commit().is_err());
    let applied = s.summary().commit_applied;
    let left = s.rollback();
    assert!(left > 0);
    assert!(!s.commit_pending());
    assert!(s.changes().is_empty());
    drop(s);
    assert!(!rig.dir.exists());
    // the volume is consistent and the first operations are on it
    let mut fs = rig.plain();
    let r = fs.root_ino();
    assert!(applied == 0 || fs.lookup(r, "obsolete.log").is_err() || fs.lookup(r, "new").is_ok());
}

#[test]
fn conformance_through_a_persistent_session_with_a_crash_in_the_commit() {
    let (_, snap) = setup();
    for at in [5usize, 40, 120] {
        let rig = Rig::new(Kind::Ext4, &snap);
        let ctl = Arc::new(Mutex::new(FaultCtl::default()));
        let mut s = rig.open(&ctl);
        let root = s.root_ino();
        s.unlink(root, "big.bin").unwrap();
        testutil::conformance(&mut s, true);
        s.sync().unwrap();
        {
            let mut c = ctl.lock().unwrap();
            c.count = 0;
            c.crash_at = Some(at);
        }
        let interrupted = s.commit().is_err();
        if interrupted {
            s.simulate_crash();
        } else {
            drop(s);
        }
        let ctl = Arc::new(Mutex::new(FaultCtl::default()));
        let mut r = rig.open(&ctl);
        if interrupted {
            r.commit().unwrap();
        }
        drop(r);
        testutil::check_persisted(rig.plain().as_mut());
        let _ = std::fs::remove_dir_all(rig.dir.parent().unwrap());
    }
}

#[test]
fn second_commit_in_the_same_mount_after_renames() {
    // After a commit the journal must name objects by their new paths.
    let (dev, _) = setup();
    let dir = tmp("twice").join("s");
    let mut s = session(&dev, &dir);
    let root = s.root_ino();
    s.rename(root, "docs", root, "documents").unwrap();
    let docs = s.lookup(root, "documents").unwrap();
    let a = s.lookup(docs, "a.txt").unwrap();
    s.commit().unwrap();
    s.write(a, 0, b"ALPHA").unwrap();
    s.sync().unwrap();
    s.simulate_crash();
    let mut r = session(&dev, &dir);
    let d2 = r.lookup(root, "documents").unwrap();
    let a2 = r.lookup(d2, "a.txt").unwrap();
    assert_eq!(r.read(a2, 0, 10).unwrap(), b"ALPHA");
    r.commit().unwrap();
}

#[test]
fn background_flush_covers_only_the_sealed_records() {
    let (dev, _) = setup();
    let dir = tmp("bg").join("s");
    let mut s = session(&dev, &dir);
    s.flush_interval = Duration::from_millis(50); // inline safety net: 500 ms
    let root = s.root_ino();
    let f = s.create(root, "f", 0o644, 0, 0).unwrap();
    s.write(f, 0, &[1u8; 70_000]).unwrap();
    let sealed = full_view(&mut s);
    std::thread::sleep(Duration::from_millis(60));
    let job = s.begin_background_flush().expect("records pending");
    // While the stage is being synced without the lock, the volume keeps changing.
    s.write(f, 0, &[2u8; 70_000]).unwrap();
    s.create(root, "later", 0o644, 0, 0).unwrap();
    let r = job.sync_stage();
    s.finish_background_flush(job, r);
    s.simulate_crash();
    let mut r = session(&dev, &dir);
    assert_eq!(full_view(&mut r).0, sealed.0);
    assert_eq!(r.read(f, 0, 100_000).unwrap(), vec![1u8; 70_000]);
}
