//! Defense in depth for untrusted volumes.
//!
//! Drivers validate on-disk structures and return `FsError::Corrupt`, but a bug that still
//! panics must not take more than the volume down with it. [`PanicFence`] sits directly on
//! top of every driver opened through [`super::open_fs`]:
//!
//! * a panic inside any driver call is caught right there, so it never unwinds through the
//!   NFS / LAN / sync locks above (no poisoned or wedged mutex);
//! * the first panic puts the volume in a failed state: every later call (including `sync`)
//!   returns an I/O error without entering the driver again, so half-updated in-memory
//!   structures are never written back to the disk. The volume reports itself read-only, with
//!   the reason, until it is unmounted.

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};

use super::*;

pub struct PanicFence {
    inner: Box<dyn FileSystem>,
    info: VolumeInfo,
    failed: RefCell<Option<String>>,
}

pub(crate) fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "unknown panic".into())
}

impl PanicFence {
    pub fn new(inner: Box<dyn FileSystem>) -> Self {
        let info = inner.volume_info();
        PanicFence { inner, info, failed: RefCell::new(None) }
    }

    /// Why the volume was disabled, if it was.
    pub fn failure(&self) -> Option<String> {
        self.failed.borrow().clone()
    }

    fn fail(&self, op: &str, p: Box<dyn std::any::Any + Send>) -> FsError {
        let why = format!("internal error in the {} driver during {op}: {}", self.info.fs_type.label(), panic_message(p.as_ref()));
        eprintln!("lumendisk: {why}; volume disabled (every further operation fails with EIO)");
        *self.failed.borrow_mut() = Some(why.clone());
        FsError::Io(why)
    }

    fn guard<T>(&mut self, op: &str, f: impl FnOnce(&mut dyn FileSystem) -> FsResult<T>) -> FsResult<T> {
        if let Some(why) = self.failed.borrow().as_ref() {
            return Err(FsError::Io(format!("volume disabled after an earlier error ({why})")));
        }
        let inner = self.inner.as_mut();
        match catch_unwind(AssertUnwindSafe(|| f(inner))) {
            Ok(r) => r,
            Err(p) => Err(self.fail(op, p)),
        }
    }
}

impl FileSystem for PanicFence {
    fn volume_info(&self) -> VolumeInfo {
        if self.failed.borrow().is_some() {
            return VolumeInfo { read_only: true, ..self.info.clone() };
        }
        match catch_unwind(AssertUnwindSafe(|| self.inner.volume_info())) {
            Ok(v) => v,
            Err(p) => {
                self.fail("volume_info", p);
                VolumeInfo { read_only: true, ..self.info.clone() }
            }
        }
    }
    fn statfs(&mut self) -> FsResult<FsStat> {
        self.guard("statfs", |fs| fs.statfs())
    }
    fn root_ino(&self) -> Ino {
        // A constant of every driver; computed at open time.
        self.inner.root_ino()
    }
    fn lookup(&mut self, parent: Ino, name: &str) -> FsResult<Ino> {
        self.guard("lookup", |fs| fs.lookup(parent, name))
    }
    fn getattr(&mut self, ino: Ino) -> FsResult<Attr> {
        self.guard("getattr", |fs| fs.getattr(ino))
    }
    fn setattr(&mut self, ino: Ino, set: &SetAttr) -> FsResult<Attr> {
        self.guard("setattr", |fs| fs.setattr(ino, set))
    }
    fn read(&mut self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>> {
        self.guard("read", |fs| fs.read(ino, offset, len))
    }
    fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<u32> {
        self.guard("write", |fs| fs.write(ino, offset, data))
    }
    fn readdir(&mut self, dir: Ino) -> FsResult<Vec<DirEntry>> {
        self.guard("readdir", |fs| fs.readdir(dir))
    }
    fn create(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino> {
        self.guard("create", |fs| fs.create(parent, name, perm, uid, gid))
    }
    fn mkdir(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino> {
        self.guard("mkdir", |fs| fs.mkdir(parent, name, perm, uid, gid))
    }
    fn symlink(&mut self, parent: Ino, name: &str, target: &str, uid: u32, gid: u32) -> FsResult<Ino> {
        self.guard("symlink", |fs| fs.symlink(parent, name, target, uid, gid))
    }
    fn readlink(&mut self, ino: Ino) -> FsResult<String> {
        self.guard("readlink", |fs| fs.readlink(ino))
    }
    fn unlink(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.guard("unlink", |fs| fs.unlink(parent, name))
    }
    fn rmdir(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.guard("rmdir", |fs| fs.rmdir(parent, name))
    }
    fn rename(&mut self, from_parent: Ino, from_name: &str, to_parent: Ino, to_name: &str) -> FsResult<()> {
        self.guard("rename", |fs| fs.rename(from_parent, from_name, to_parent, to_name))
    }
    fn sync(&mut self) -> FsResult<()> {
        self.guard("sync", |fs| fs.sync())
    }
    fn dirty_state(&mut self) -> DirtyState {
        match self.guard("dirty_state", |fs| Ok(fs.dirty_state())) {
            Ok(d) => d,
            Err(e) => DirtyState::Dirty(e.to_string()),
        }
    }
    fn repair(&mut self) -> FsResult<String> {
        self.guard("repair", |fs| fs.repair())
    }
    fn read_only_reason(&self) -> Option<String> {
        if let Some(why) = self.failed.borrow().clone() {
            return Some(why);
        }
        catch_unwind(AssertUnwindSafe(|| self.inner.read_only_reason())).unwrap_or(None)
    }
}

impl Drop for PanicFence {
    fn drop(&mut self) {
        // Drivers flush on drop (FAT / exFAT / NTFS). After a panic their state can't be
        // trusted: leak them instead of letting them write.
        if self.failed.get_mut().is_some() {
            let inner = std::mem::replace(&mut self.inner, Box::new(Disabled));
            std::mem::forget(inner);
        } else {
            let inner = std::mem::replace(&mut self.inner, Box::new(Disabled));
            if let Err(p) = catch_unwind(AssertUnwindSafe(move || drop(inner))) {
                eprintln!("lumendisk: driver panicked while closing: {}", panic_message(p.as_ref()));
            }
        }
    }
}

/// Placeholder left in a fence while it is being dropped.
struct Disabled;

impl FileSystem for Disabled {
    fn volume_info(&self) -> VolumeInfo {
        VolumeInfo { fs_type: FsType::Ext4, label: String::new(), uuid: String::new(), read_only: true }
    }
    fn statfs(&mut self) -> FsResult<FsStat> {
        Err(FsError::Stale)
    }
    fn root_ino(&self) -> Ino {
        1
    }
    fn lookup(&mut self, _: Ino, _: &str) -> FsResult<Ino> {
        Err(FsError::Stale)
    }
    fn getattr(&mut self, _: Ino) -> FsResult<Attr> {
        Err(FsError::Stale)
    }
    fn setattr(&mut self, _: Ino, _: &SetAttr) -> FsResult<Attr> {
        Err(FsError::Stale)
    }
    fn read(&mut self, _: Ino, _: u64, _: u32) -> FsResult<Vec<u8>> {
        Err(FsError::Stale)
    }
    fn write(&mut self, _: Ino, _: u64, _: &[u8]) -> FsResult<u32> {
        Err(FsError::Stale)
    }
    fn readdir(&mut self, _: Ino) -> FsResult<Vec<DirEntry>> {
        Err(FsError::Stale)
    }
    fn create(&mut self, _: Ino, _: &str, _: u16, _: u32, _: u32) -> FsResult<Ino> {
        Err(FsError::Stale)
    }
    fn mkdir(&mut self, _: Ino, _: &str, _: u16, _: u32, _: u32) -> FsResult<Ino> {
        Err(FsError::Stale)
    }
    fn symlink(&mut self, _: Ino, _: &str, _: &str, _: u32, _: u32) -> FsResult<Ino> {
        Err(FsError::Stale)
    }
    fn readlink(&mut self, _: Ino) -> FsResult<String> {
        Err(FsError::Stale)
    }
    fn unlink(&mut self, _: Ino, _: &str) -> FsResult<()> {
        Err(FsError::Stale)
    }
    fn rmdir(&mut self, _: Ino, _: &str) -> FsResult<()> {
        Err(FsError::Stale)
    }
    fn rename(&mut self, _: Ino, _: &str, _: Ino, _: &str) -> FsResult<()> {
        Err(FsError::Stale)
    }
    fn sync(&mut self) -> FsResult<()> {
        Ok(())
    }
    fn dirty_state(&mut self) -> DirtyState {
        DirtyState::Clean
    }
    fn repair(&mut self) -> FsResult<String> {
        Err(FsError::Stale)
    }
}

/// Open a driver with panics turned into errors (a panic while parsing the superblock / boot
/// sector is just a corrupt volume).
pub(crate) fn open_fenced(open: impl FnOnce() -> FsResult<Box<dyn FileSystem>>) -> FsResult<Box<dyn FileSystem>> {
    match catch_unwind(AssertUnwindSafe(open)) {
        Ok(r) => Ok(Box::new(PanicFence::new(r?))),
        Err(p) => {
            let why = panic_message(p.as_ref());
            eprintln!("lumendisk: driver panicked while opening the volume: {why}");
            Err(FsError::Corrupt(format!("internal error while opening the volume: {why}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemDev;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    /// A real FAT volume whose driver panics on a chosen read.
    struct Booby {
        inner: Box<dyn FileSystem>,
        panic_on: Ino,
        writes_after: Arc<AtomicU32>,
        armed: bool,
    }

    impl FileSystem for Booby {
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
            self.inner.lookup(p, n)
        }
        fn getattr(&mut self, i: Ino) -> FsResult<Attr> {
            self.inner.getattr(i)
        }
        fn setattr(&mut self, i: Ino, s: &SetAttr) -> FsResult<Attr> {
            self.inner.setattr(i, s)
        }
        fn read(&mut self, i: Ino, o: u64, l: u32) -> FsResult<Vec<u8>> {
            if i == self.panic_on {
                self.armed = true;
                panic!("simulated driver bug (corrupt structure not validated)");
            }
            self.inner.read(i, o, l)
        }
        fn write(&mut self, i: Ino, o: u64, d: &[u8]) -> FsResult<u32> {
            if self.armed {
                self.writes_after.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.write(i, o, d)
        }
        fn readdir(&mut self, d: Ino) -> FsResult<Vec<DirEntry>> {
            self.inner.readdir(d)
        }
        fn create(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> {
            self.inner.create(p, n, m, u, g)
        }
        fn mkdir(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> {
            self.inner.mkdir(p, n, m, u, g)
        }
        fn symlink(&mut self, p: Ino, n: &str, t: &str, u: u32, g: u32) -> FsResult<Ino> {
            self.inner.symlink(p, n, t, u, g)
        }
        fn readlink(&mut self, i: Ino) -> FsResult<String> {
            self.inner.readlink(i)
        }
        fn unlink(&mut self, p: Ino, n: &str) -> FsResult<()> {
            self.inner.unlink(p, n)
        }
        fn rmdir(&mut self, p: Ino, n: &str) -> FsResult<()> {
            self.inner.rmdir(p, n)
        }
        fn rename(&mut self, a: Ino, b: &str, c: Ino, d: &str) -> FsResult<()> {
            self.inner.rename(a, b, c, d)
        }
        fn sync(&mut self) -> FsResult<()> {
            if self.armed {
                self.writes_after.fetch_add(1, Ordering::SeqCst);
            }
            self.inner.sync()
        }
        fn dirty_state(&mut self) -> DirtyState {
            self.inner.dirty_state()
        }
        fn repair(&mut self) -> FsResult<String> {
            self.inner.repair()
        }
    }

    /// A panicking driver call becomes EIO, the shared lock is not poisoned and stays usable,
    /// every later call fails fast with EIO (also from other threads), and nothing is written
    /// to the volume after the panic (not even by sync or drop).
    #[test]
    fn panic_disables_the_volume_without_poisoning_the_lock() {
        let dev = Arc::new(MemDev::new(4 << 20));
        crate::vfs::fat_fs::format(dev.clone(), FsType::Fat16, "FENCE").unwrap();
        let d = MountDefaults { uid: 501, gid: 20, file_perm: 0o644, dir_perm: 0o755 };
        let writes_after = Arc::new(AtomicU32::new(0));
        // FAT inode numbers only live as long as the mount: set up through the same instance.
        let mut inner = Box::new(crate::vfs::fat_fs::FatFs::open(dev.clone(), false, d).unwrap());
        let bad = inner.create(1, "bad.bin", 0o644, 0, 0).unwrap();
        let good = inner.create(1, "good.bin", 0o644, 0, 0).unwrap();
        inner.write(good, 0, b"intact").unwrap();
        inner.sync().unwrap();
        let before = dev.snapshot();
        let fs: Box<dyn FileSystem> = Box::new(PanicFence::new(Box::new(Booby { inner, panic_on: bad, writes_after: writes_after.clone(), armed: false })));
        let shared = Arc::new(Mutex::new(fs));

        assert_eq!(shared.lock().unwrap().read(good, 0, 100).unwrap(), b"intact");
        let e = shared.lock().unwrap().read(bad, 0, 100).unwrap_err();
        assert!(matches!(e, FsError::Io(ref m) if m.contains("simulated driver bug")), "{e:?}");
        // Not poisoned, and every later call fails fast instead of hanging or touching the disk.
        assert!(!shared.is_poisoned());
        let s2 = shared.clone();
        let t = std::thread::spawn(move || {
            let mut g = s2.lock().unwrap();
            (g.read(good, 0, 100), g.write(good, 0, b"x"), g.create(1, "new", 0o644, 0, 0), g.readdir(1), g.sync())
        });
        let (r, w, c, l, s) = t.join().unwrap();
        for e in [r.err(), w.err(), c.err(), l.err(), s.err()] {
            assert!(matches!(e, Some(FsError::Io(_))), "{e:?}");
        }
        let g = shared.lock().unwrap();
        assert!(g.volume_info().read_only);
        assert!(g.read_only_reason().unwrap().contains("internal error"));
        drop(g);
        drop(shared);
        assert_eq!(writes_after.load(Ordering::SeqCst), 0, "the driver was used after it panicked");
        assert!(dev.snapshot() == before, "the volume was written after the panic");
    }

    #[test]
    fn panic_while_opening_is_a_corrupt_volume() {
        let r = open_fenced(|| panic!("boom while parsing"));
        assert!(matches!(r, Err(FsError::Corrupt(ref m)) if m.contains("boom")));
    }
}
