//! Persistent Shadow sessions over a remote server (`RemoteVfs` over the in-memory backend):
//! restore after a crash, base fingerprints checked against the server, commit = applied by
//! the server.

use super::*;
use crate::remote::cache::tests::{cfg, MemBackend};
use crate::remote::cache::{RemoteConfig, RemoteVfs};
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn identity() -> Identity {
    Identity {
        kind: "sftp".into(),
        fs_type: "SFTP".into(),
        label: "MEM".into(),
        source_path: "sftp://pi@mem/".into(),
        endpoint: Some("sftp://pi@mem:22/".into()),
        display: Some("pi@mem:/".into()),
        ..Default::default()
    }
}

struct Env {
    be: Arc<MemBackend>,
    cfg: RemoteConfig,
    dir: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let be = MemBackend::new();
        {
            let mut f = be.files.lock().unwrap();
            f.insert("/docs".into(), (FileKind::Dir, vec![], 0o755, None));
            f.insert("/docs/a.txt".into(), (FileKind::File, b"alpha".to_vec(), 0o644, None));
            f.insert("/docs/b.txt".into(), (FileKind::File, b"bravo".to_vec(), 0o644, None));
            f.insert("/old.log".into(), (FileKind::File, b"log".to_vec(), 0o644, None));
        }
        let dir = std::env::temp_dir().join(format!("ld-rshadow-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Env { be, cfg: cfg(name), dir }
    }

    fn open(&self) -> Result<ShadowOverlay, String> {
        let v = RemoteVfs::new(self.be.clone(), self.cfg.clone()).map_err(|e| e.to_string())?;
        ShadowOverlay::open_session(Box::new(v), self.dir.clone(), identity())
    }

    fn server(&self, path: &str) -> Option<Vec<u8>> {
        self.be.files.lock().unwrap().get(path).map(|v| v.1.clone())
    }

    fn manifest(&self) -> Option<Manifest> {
        Manifest::load(&self.dir)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(&self.cfg.cache_dir);
        if let Some(q) = &self.cfg.queue_dir {
            let _ = std::fs::remove_dir_all(q);
        }
    }
}

fn walk(fs: &mut dyn FileSystem, path: &str) -> FsResult<Ino> {
    let mut cur = fs.root_ino();
    for c in path.split('/').filter(|c| !c.is_empty()) {
        cur = fs.lookup(cur, c)?;
    }
    Ok(cur)
}

fn read_all(fs: &mut dyn FileSystem, path: &str) -> Vec<u8> {
    let i = walk(fs, path).unwrap_or_else(|e| panic!("{path}: {e}"));
    fs.read(i, 0, 1 << 20).unwrap()
}

fn edit(ov: &mut ShadowOverlay) {
    let r = ov.root_ino();
    let docs = ov.lookup(r, "docs").unwrap();
    let a = ov.lookup(docs, "a.txt").unwrap();
    ov.write(a, 0, b"ALPHA, edited in Shadow").unwrap();
    let n = ov.create(r, "new.txt", 0o644, 0, 0).unwrap();
    ov.write(n, 0, b"brand new").unwrap();
    ov.unlink(r, "old.log").unwrap();
    ov.rename(r, "docs", r, "documents").unwrap();
    ov.mkdir(r, "empty", 0o755, 0, 0).unwrap();
}

#[test]
fn remote_session_survives_a_crash_and_commit_reaches_the_server() {
    let env = Env::new("rs-persist");
    let mut ov = env.open().unwrap();
    assert!(!ov.stable_inos, "remote inode numbers are per process");
    edit(&mut ov);
    assert!(ov.is_persistent());
    assert!(store::session_key(&identity()).starts_with("sftp-"));
    ov.force_flush();
    ov.simulate_crash();
    assert_eq!(env.server("/docs/a.txt").unwrap(), b"alpha", "Shadow must not touch the server");
    assert!(env.server("/new.txt").is_none() && env.server("/old.log").is_some());
    let m = env.manifest().unwrap();
    assert_eq!(m.identity.endpoint.as_deref(), Some("sftp://pi@mem:22/"));
    assert!(m.drift.is_none());

    let mut ov = env.open().unwrap();
    assert!(ov.summary().restored);
    assert_eq!(read_all(&mut ov, "/documents/a.txt"), b"ALPHA, edited in Shadow");
    assert_eq!(read_all(&mut ov, "/new.txt"), b"brand new");
    assert!(walk(&mut ov, "/old.log").is_err() && walk(&mut ov, "/docs").is_err());
    let n = ov.commit().unwrap();
    assert!(n >= 4, "{n}");
    // The commit returns only once the server applied everything (no waiting here).
    assert_eq!(env.server("/documents/a.txt").unwrap(), b"ALPHA, edited in Shadow");
    assert_eq!(env.server("/documents/b.txt").unwrap(), b"bravo");
    assert_eq!(env.server("/new.txt").unwrap(), b"brand new");
    assert!(env.server("/old.log").is_none() && env.server("/docs").is_none() && env.server("/empty").is_some());
    drop(ov);
    assert!(env.manifest().is_none(), "nothing left to resume");
}

#[test]
fn remote_change_on_the_server_while_unmounted_is_drift() {
    let env = Env::new("rs-drift");
    let mut ov = env.open().unwrap();
    edit(&mut ov);
    ov.force_flush();
    ov.simulate_crash();
    // Somebody edits a file the session read.
    env.be.files.lock().unwrap().get_mut("/docs/a.txt").unwrap().1 = b"alpha, changed on the server".to_vec();
    let e = env.open().err().expect("restore must refuse");
    assert!(e.contains("server"), "{e}");
    let m = env.manifest().unwrap();
    assert!(m.drift.as_deref().unwrap_or("").contains("a.txt"), "{:?}", m.drift);
    assert!(env.open().is_err(), "a drifted session can only be discarded");
    assert_eq!(env.server("/docs/a.txt").unwrap(), b"alpha, changed on the server");
}

#[test]
fn remote_change_on_the_server_while_mounted_refuses_the_commit() {
    let env = Env::new("rs-live");
    let mut ov = env.open().unwrap();
    edit(&mut ov);
    // Same size, new mtime: only the mtime tells.
    env.be.mtimes.lock().unwrap().insert("/docs/a.txt".into(), Timestamp { secs: 1_800_000_000, nsecs: 0 });
    let e = ov.commit().unwrap_err().to_string();
    assert!(e.contains("server") && e.contains("a.txt"), "{e}");
    assert!(ov.summary().drift.is_some());
    assert!(env.manifest().unwrap().drift.is_some());
    let r = ov.root_ino();
    assert!(matches!(ov.create(r, "more.txt", 0o644, 0, 0), Err(FsError::ReadOnly)), "inspect / discard only");
    assert!(ov.commit().is_err(), "still refused");
    assert_eq!(env.server("/docs/a.txt").unwrap(), b"alpha");
    assert!(env.server("/new.txt").is_none());
    // Discard works and starts over.
    assert!(ov.rollback() >= 4);
    assert!(ov.summary().drift.is_none());
    let n = ov.create(r, "after.txt", 0o644, 0, 0).unwrap();
    ov.write(n, 0, b"ok").unwrap();
    assert_eq!(ov.commit().unwrap(), 1);
    assert_eq!(env.server("/after.txt").unwrap(), b"ok");
}

#[test]
fn remote_restore_while_unreachable_leaves_the_session_untouched() {
    let env = Env::new("rs-offline");
    let mut ov = env.open().unwrap();
    edit(&mut ov);
    ov.force_flush();
    ov.simulate_crash();
    env.be.stalled.store(true, Ordering::SeqCst);
    let e = env.open().err().expect("can't check the bases offline");
    assert!(e.contains("reachable") || e.contains("raggiungibile"), "{e}");
    assert!(env.manifest().unwrap().drift.is_none(), "unreachable is not drift");
    env.be.stalled.store(false, Ordering::SeqCst);
    let mut ov = env.open().unwrap();
    assert_eq!(read_all(&mut ov, "/new.txt"), b"brand new");
}

/// Reads work, every change times out while `fail` is set: the commit's base check passes,
/// then the uploads can't get through.
struct WriteOutage {
    inner: Arc<MemBackend>,
    fail: std::sync::atomic::AtomicBool,
}

impl WriteOutage {
    fn gate(&self) -> crate::remote::RResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            Err(crate::remote::RemoteError::Timeout)
        } else {
            Ok(())
        }
    }
}

impl crate::remote::RemoteBackend for WriteOutage {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn set_timeout(&self, t: Duration) {
        self.inner.set_timeout(t)
    }
    fn ping(&self) -> crate::remote::RResult<Duration> {
        self.inner.ping()
    }
    fn stat(&self, p: &str) -> crate::remote::RResult<crate::remote::RemoteEntry> {
        self.inner.stat(p)
    }
    fn list(&self, p: &str) -> crate::remote::RResult<Vec<crate::remote::RemoteEntry>> {
        self.inner.list(p)
    }
    fn read(&self, p: &str, o: u64, l: u32) -> crate::remote::RResult<Vec<u8>> {
        self.inner.read(p, o, l)
    }
    fn supports_partial_write(&self) -> bool {
        true
    }
    fn write(&self, p: &str, o: u64, d: &[u8]) -> crate::remote::RResult<()> {
        self.gate()?;
        self.inner.write(p, o, d)
    }
    fn put_all(&self, p: &str, d: &[u8]) -> crate::remote::RResult<()> {
        self.gate()?;
        self.inner.put_all(p, d)
    }
    fn create(&self, p: &str, perm: u16) -> crate::remote::RResult<()> {
        self.gate()?;
        self.inner.create(p, perm)
    }
    fn truncate(&self, p: &str, s: u64) -> crate::remote::RResult<()> {
        self.gate()?;
        self.inner.truncate(p, s)
    }
    fn mkdir(&self, p: &str, perm: u16) -> crate::remote::RResult<()> {
        self.gate()?;
        self.inner.mkdir(p, perm)
    }
    fn remove(&self, p: &str) -> crate::remote::RResult<()> {
        self.gate()?;
        self.inner.remove(p)
    }
    fn rmdir(&self, p: &str) -> crate::remote::RResult<()> {
        self.gate()?;
        self.inner.rmdir(p)
    }
    fn rename(&self, f: &str, t: &str) -> crate::remote::RResult<()> {
        self.gate()?;
        self.inner.rename(f, t)
    }
    fn symlink(&self, p: &str, t: &str) -> crate::remote::RResult<()> {
        self.gate()?;
        self.inner.symlink(p, t)
    }
    fn readlink(&self, p: &str) -> crate::remote::RResult<String> {
        self.inner.readlink(p)
    }
    fn set_perm(&self, p: &str, perm: u16) -> crate::remote::RResult<()> {
        self.gate()?;
        self.inner.set_perm(p, perm)
    }
    fn set_mtime(&self, p: &str, t: Timestamp) -> crate::remote::RResult<()> {
        self.gate()?;
        self.inner.set_mtime(p, t)
    }
    fn statfs(&self) -> crate::remote::RResult<Option<(u64, u64)>> {
        self.inner.statfs()
    }
}

#[test]
fn remote_commit_never_succeeds_while_changes_are_only_queued() {
    let env = Env::new("rs-commit-offline");
    let be = Arc::new(WriteOutage { inner: env.be.clone(), fail: false.into() });
    let v = RemoteVfs::new(be.clone(), env.cfg.clone()).unwrap();
    let mut ov = ShadowOverlay::open_session(Box::new(v), env.dir.clone(), identity()).unwrap();
    let r = ov.root_ino();
    let n = ov.create(r, "queued.txt", 0o644, 0, 0).unwrap();
    ov.write(n, 0, b"must reach the server").unwrap();
    let docs = ov.lookup(r, "docs").unwrap();
    ov.unlink(docs, "b.txt").unwrap();
    be.fail.store(true, Ordering::SeqCst);
    let e = ov.commit().unwrap_err().to_string();
    assert!(e.contains("reachable") || e.contains("raggiungibile"), "{e}");
    assert!(ov.commit_pending(), "the commit stays resumable");
    assert_eq!(ov.summary().state, "committing");
    assert!(env.server("/queued.txt").is_none() && env.server("/docs/b.txt").is_some());
    be.fail.store(false, Ordering::SeqCst);
    assert_eq!(ov.commit().unwrap(), 2);
    assert_eq!(env.server("/queued.txt").unwrap(), b"must reach the server");
    assert!(env.server("/docs/b.txt").is_none());
    assert!(!ov.commit_pending());
}
