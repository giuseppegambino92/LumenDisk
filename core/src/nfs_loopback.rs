//! Ephemeral loopback NFSv3 bridge.
//!
//! Each mounted volume gets its own NFSv3 + MOUNTv3 server (RFC 1813, vendored `nfsserve`)
//! bound to `127.0.0.1:<ephemeral port>`, exporting an unguessable path, with file handles that
//! embed a per-mount random secret (forged handles are rejected as stale). LumenDisk then
//! asks macOS's own `/sbin/mount_nfs` to mount it — no kext, no MacFUSE.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nfsserve::nfs::*;
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry as NfsDirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use crate::vfs::{Attr, FileKind, FileSystem, FsError, SetAttr, Timestamp};

pub struct NfsBridge<S: FileSystem + 'static> {
    pub fs: Arc<Mutex<S>>,
    pub read_only: bool,
    secret: [u8; 16],
    owner_uid: u32,
    owner_gid: u32,
    fsid: u64,
    /// Change times observed through this server. FAT/exFAT (and Windows-written NTFS) do not
    /// update a directory's timestamps when entries change and only keep 2 s resolution; the
    /// macOS NFS client relies on mtime/ctime moving for cache coherence (stale name/attribute
    /// caches otherwise make it refuse AppleDouble xattr writes). We report
    /// max(on-disk time, last change seen here).
    tracker: Arc<ChangeTracker>,
    /// Group commit for stable replies; None: the stack has nothing to make durable on the
    /// Mac (remote volumes, in-memory Shadow overlays).
    durable: Option<Arc<GroupCommit>>,
    /// WRITE / COMMIT verifier: random per server instance, so that after a restart the client
    /// knows that UNSTABLE writes it never saw committed may be lost.
    verifier: [u8; 8],
}

/// Per-volume timestamp bookkeeping shared between the NFS bridge and the runtime.
#[derive(Default)]
pub struct ChangeTracker {
    changed: Mutex<HashMap<u64, (Option<Timestamp>, Timestamp)>>,
    /// Times explicitly set by the client (SETATTR). Filesystems with coarse timestamps (FAT:
    /// 2 s mtime, date-only atime) would otherwise answer with a different value than the one
    /// just set; the macOS client treats that as a foreign modification. While the on-disk
    /// value is still the rounded form of the requested one we report the exact value.
    exact: Mutex<HashMap<u64, (Option<Timestamp>, Option<Timestamp>)>>,
    /// Directories never report times older than this (bumped when the whole tree changes
    /// under the client's feet: Shadow commit / rollback).
    dir_floor: Mutex<Timestamp>,
    /// Set when a call panicked: from then on every call fails with EIO without touching the
    /// (possibly inconsistent) stack.
    failed: Mutex<Option<String>>,
}

impl ChangeTracker {
    /// The volume content was replaced wholesale (Shadow rollback/commit): forget overrides and
    /// make every directory look modified so the kernel drops its name / attribute caches.
    pub fn invalidate_all(&self) {
        self.changed.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.exact.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *self.dir_floor.lock().unwrap_or_else(|e| e.into_inner()) = Timestamp::now();
    }

    /// Why the volume stopped serving requests after an internal error, if it did.
    pub fn failure(&self) -> Option<String> {
        self.failed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

// ───────────────────────────── durability ─────────────────────────────

/// One durability step of a volume's stack, called without the volume lock (it takes the
/// lock itself, briefly).
pub type DurableFn = Box<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Durability of acknowledged operations, one per volume ("hybrid"):
///
/// * **written** ([`GroupCommit::written`]) — metadata operations (CREATE, MKDIR, SYMLINK,
///   REMOVE, RMDIR, RENAME, SETATTR) are answered once their journal / WAL record is handed to
///   the kernel with `write()`: they survive SIGKILL, a crash or a force-quit of LumenDisk
///   (the page cache does), with no `F_FULLFSYNC` in the reply path;
/// * **durable** ([`GroupCommit::wait`]) — FILE_SYNC / DATA_SYNC writes and COMMIT (what the
///   macOS client sends on close() and fsync()) are answered only once everything answered
///   so far, data and metadata, is `F_FULLFSYNC`ed: it survives a power cut too. The volume's
///   timer runs the same group commit every 100 ms, so after a power cut only changes younger
///   than that (and never closed / fsynced) can be missing.
///
/// Group commit: a request needs the first round that *starts* after it finished its
/// operation. When no round is running it runs one itself; otherwise it waits for the running
/// round to end, and then the next round — run by one of the waiters — covers every request
/// that arrived meanwhile: one `F_FULLFSYNC` for all of them. Rounds are cumulative (each one
/// covers everything done before it started), so a later successful round also satisfies a
/// request whose own round failed.
pub struct GroupCommit {
    make_durable: DurableFn,
    make_written: DurableFn,
    writes: std::sync::atomic::AtomicU64,
    st: Mutex<GcState>,
    notify: tokio::sync::Notify,
}

#[derive(Default)]
struct GcState {
    started: u64,
    finished: u64,
    ok_upto: u64,
    running: bool,
    last_error: Option<String>,
    waits: u64,
}

impl GroupCommit {
    /// `make_durable`: write and `F_FULLFSYNC` everything; `make_written`: hand everything to
    /// the kernel (no cache flush).
    pub fn new(make_durable: DurableFn, make_written: DurableFn) -> Arc<GroupCommit> {
        Arc::new(GroupCommit {
            make_durable,
            make_written,
            writes: Default::default(),
            st: Mutex::new(GcState::default()),
            notify: tokio::sync::Notify::new(),
        })
    }

    /// Return once everything done before the call is written to the journal / WAL (survives
    /// a crash of this process; the next round makes it survive a power cut).
    pub fn written(&self) -> Result<(), String> {
        self.writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::task::block_in_place(|| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.make_written)()))
                .unwrap_or_else(|p| Err(crate::vfs::fence::panic_message(p.as_ref())))
        })
        .inspect_err(|e| eprintln!("lumendisk: writing changes to the journal failed: {e}"))
    }

    /// Return once everything done before the call is durable.
    pub async fn wait(&self) -> Result<(), String> {
        let need = {
            let mut s = self.st.lock().unwrap_or_else(|e| e.into_inner());
            s.waits += 1;
            s.started + 1
        };
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let lead = {
                let mut s = self.st.lock().unwrap_or_else(|e| e.into_inner());
                if s.finished >= need {
                    return if s.ok_upto >= need { Ok(()) } else { Err(s.last_error.clone().unwrap_or_default()) };
                }
                if s.running {
                    None
                } else {
                    s.running = true;
                    s.started += 1;
                    Some(s.started)
                }
            };
            let Some(round) = lead else {
                notified.await;
                continue;
            };
            let r = tokio::task::block_in_place(|| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.make_durable)()))
                    .unwrap_or_else(|p| Err(crate::vfs::fence::panic_message(p.as_ref())))
            });
            {
                let mut s = self.st.lock().unwrap_or_else(|e| e.into_inner());
                s.running = false;
                s.finished = round;
                match r {
                    Ok(()) => s.ok_upto = round,
                    Err(e) => {
                        eprintln!("lumendisk: making changes durable failed: {e}");
                        s.last_error = Some(e);
                    }
                }
            }
            self.notify.notify_waiters();
        }
    }

    /// (durable rounds run, requests that waited for one, write-throughs) — for tests and
    /// diagnostics.
    pub fn stats(&self) -> (u64, u64, u64) {
        let s = self.st.lock().unwrap_or_else(|e| e.into_inner());
        (s.finished, s.waits, self.writes.load(std::sync::atomic::Ordering::Relaxed))
    }
}

fn trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMENDISK_TRACE").is_some())
}

/// `LUMENDISK_TRACE=1`: log the durability-relevant calls with a timestamp.
fn trace(what: std::fmt::Arguments) {
    if trace_on() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        eprintln!("lumendisk-trace: {:.6} {what}", t.as_secs_f64());
    }
}

/// Is `disk` a plausible coarse-grained rendering of `want` (same instant within 2 s, or the
/// same calendar day for date-only atimes)?
fn same_coarse(disk: Timestamp, want: Timestamp, allow_day: bool) -> bool {
    let d = (disk.secs - want.secs).abs();
    d <= 2 || (allow_day && d < 86_400 + 3_600 * 14)
}

pub fn map_err(e: FsError) -> nfsstat3 {
    match e {
        FsError::NotFound => nfsstat3::NFS3ERR_NOENT,
        FsError::Exists => nfsstat3::NFS3ERR_EXIST,
        FsError::NotDir => nfsstat3::NFS3ERR_NOTDIR,
        FsError::IsDir => nfsstat3::NFS3ERR_ISDIR,
        FsError::NotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
        FsError::NoSpace => nfsstat3::NFS3ERR_NOSPC,
        FsError::ReadOnly => nfsstat3::NFS3ERR_ROFS,
        FsError::NameTooLong => nfsstat3::NFS3ERR_NAMETOOLONG,
        FsError::InvalidName | FsError::Invalid(_) => nfsstat3::NFS3ERR_INVAL,
        FsError::Unsupported(_) => nfsstat3::NFS3ERR_NOTSUPP,
        FsError::Perm => nfsstat3::NFS3ERR_PERM,
        FsError::Stale => nfsstat3::NFS3ERR_STALE,
        FsError::Corrupt(_) | FsError::Io(_) => nfsstat3::NFS3ERR_IO,
    }
}

/// Does applying `s` to a file with attributes `a` change anything but ctime?
fn setattr_changes(s: &SetAttr, a: &Attr) -> bool {
    s.perm.is_some_and(|p| p != a.perm)
        || s.uid.is_some_and(|u| u != a.uid)
        || s.gid.is_some_and(|g| g != a.gid)
        || s.size.is_some_and(|z| z != a.size)
        || s.atime.is_some_and(|t| t != a.atime)
        || s.mtime.is_some_and(|t| t != a.mtime)
}

fn ts(t: Timestamp) -> nfstime3 {
    nfstime3 { seconds: t.secs.clamp(0, u32::MAX as i64) as u32, nseconds: t.nsecs }
}

fn name_of(n: &filename3) -> Result<String, nfsstat3> {
    String::from_utf8(n.0.clone()).map_err(|_| nfsstat3::NFS3ERR_INVAL)
}

impl<S: FileSystem + 'static> NfsBridge<S> {
    pub fn new(fs: Arc<Mutex<S>>, read_only: bool, fsid: u64, tracker: Arc<ChangeTracker>) -> Self {
        let secret = *uuid::Uuid::new_v4().as_bytes();
        // SAFETY: trivial libc getters.
        let (owner_uid, owner_gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut verifier = [0u8; 8];
        verifier.copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..8]);
        NfsBridge { fs, read_only, secret, owner_uid, owner_gid, fsid, tracker, durable: None, verifier }
    }

    /// Answer stable requests only once `g` made them durable.
    pub fn with_durability(mut self, g: Option<Arc<GroupCommit>>) -> Self {
        self.durable = g;
        self
    }

    /// Wait until every operation answered so far (this one included) is durable.
    async fn durable(&self, what: &str) -> Result<(), nfsstat3> {
        let Some(g) = &self.durable else { return Ok(()) };
        if self.tracker.failure().is_some() {
            return Err(nfsstat3::NFS3ERR_IO);
        }
        let t0 = std::time::Instant::now();
        let r = g.wait().await.map_err(|_| nfsstat3::NFS3ERR_IO);
        trace(format_args!("durable {what} in {:.2} ms", t0.elapsed().as_secs_f64() * 1000.0));
        r
    }

    /// Wait until every operation answered so far (this one included) is written to the
    /// journal / WAL: it survives a crash of this process (see [`GroupCommit`]).
    fn written(&self, what: &str) -> Result<(), nfsstat3> {
        let Some(g) = &self.durable else { return Ok(()) };
        if self.tracker.failure().is_some() {
            return Err(nfsstat3::NFS3ERR_IO);
        }
        let t0 = std::time::Instant::now();
        let r = g.written().map_err(|_| nfsstat3::NFS3ERR_IO);
        trace(format_args!("written {what} in {:.2} ms", t0.elapsed().as_secs_f64() * 1000.0));
        r
    }

    /// Record a content change (mtime + ctime) on `ids`.
    fn touch(&self, ids: &[u64]) {
        let now = Timestamp::now();
        let mut m = self.tracker.changed.lock().unwrap_or_else(|e| e.into_inner());
        for &i in ids {
            m.insert(i, (Some(now), now));
        }
    }

    /// Record a metadata-only change (ctime); `explicit_mtime` drops our mtime override.
    fn touch_meta(&self, id: u64, explicit_mtime: bool) {
        let now = Timestamp::now();
        let mut m = self.tracker.changed.lock().unwrap_or_else(|e| e.into_inner());
        let e = m.entry(id).or_insert((None, now));
        e.1 = now;
        if explicit_mtime {
            e.0 = None;
        }
    }

    fn fattr(&self, a: &Attr) -> fattr3 {
        let (mut mtime, mut ctime, mut atime) = (a.mtime, a.ctime, a.atime);
        if let Some((m, at)) = self.tracker.exact.lock().unwrap_or_else(|e| e.into_inner()).get(&a.ino) {
            if let Some(m) = m {
                if same_coarse(a.mtime, *m, false) {
                    mtime = *m;
                }
            }
            if let Some(t) = at {
                if same_coarse(a.atime, *t, true) {
                    atime = *t;
                }
            }
        }
        if let Some((m, c)) = self.tracker.changed.lock().unwrap_or_else(|e| e.into_inner()).get(&a.ino) {
            if let Some(m) = m {
                mtime = mtime.max(*m);
            }
            ctime = ctime.max(*c).max(mtime);
        }
        if a.kind == FileKind::Dir {
            let floor = *self.tracker.dir_floor.lock().unwrap_or_else(|e| e.into_inner());
            mtime = mtime.max(floor);
            ctime = ctime.max(floor);
        }
        fattr3 {
            ftype: match a.kind {
                FileKind::File => ftype3::NF3REG,
                FileKind::Dir => ftype3::NF3DIR,
                FileKind::Symlink => ftype3::NF3LNK,
            },
            mode: a.perm as u32,
            nlink: a.nlink.max(1),
            uid: a.uid,
            gid: a.gid,
            size: a.size,
            used: a.used_bytes,
            rdev: specdata3::default(),
            fsid: self.fsid,
            fileid: a.ino,
            atime: ts(atime),
            mtime: ts(mtime),
            ctime: ts(ctime),
        }
    }

    /// Run `f` under the volume lock. Blocking disk I/O is moved off the async reactor and a
    /// panic turns into NFS3ERR_IO instead of taking the whole volume down. Drivers are
    /// already fenced (`vfs::fence`); a panic caught here (any layer of the stack) disables
    /// the volume: every later call answers EIO at once, so the client fails fast instead of
    /// hanging, and nothing touches the stack's possibly half-updated state again.
    fn with<T>(&self, f: impl FnOnce(&mut S) -> Result<T, FsError>) -> Result<T, nfsstat3> {
        if self.tracker.failure().is_some() {
            return Err(nfsstat3::NFS3ERR_IO);
        }
        let run = || {
            let mut g = self.fs.lock().unwrap_or_else(|e| e.into_inner());
            if self.tracker.failure().is_some() {
                return Err(nfsstat3::NFS3ERR_IO);
            }
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut g))) {
                Ok(r) => r.map_err(|e| {
                    if std::env::var_os("LUMENDISK_TRACE").is_some() {
                        eprintln!("lumendisk-trace: {} -> {e:?}", std::any::type_name::<T>());
                    }
                    map_err(e)
                }),
                Err(p) => {
                    let why = crate::vfs::fence::panic_message(p.as_ref());
                    eprintln!("lumendisk: internal error ({why}); volume disabled, every further request fails with EIO");
                    *self.tracker.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
                    Err(nfsstat3::NFS3ERR_IO)
                }
            }
        };
        tokio::task::block_in_place(run)
    }

    fn to_setattr(s: &sattr3) -> SetAttr {
        let now = Timestamp::now();
        SetAttr {
            perm: match s.mode {
                set_mode3::mode(m) => Some((m & 0o7777) as u16),
                set_mode3::Void => None,
            },
            uid: match s.uid {
                set_uid3::uid(u) => Some(u),
                set_uid3::Void => None,
            },
            gid: match s.gid {
                set_gid3::gid(g) => Some(g),
                set_gid3::Void => None,
            },
            size: match s.size {
                set_size3::size(v) => Some(v),
                set_size3::Void => None,
            },
            atime: match s.atime {
                set_atime::DONT_CHANGE => None,
                set_atime::SET_TO_SERVER_TIME => Some(now),
                set_atime::SET_TO_CLIENT_TIME(t) => Some(Timestamp { secs: t.seconds as i64, nsecs: t.nseconds }),
            },
            mtime: match s.mtime {
                set_mtime::DONT_CHANGE => None,
                set_mtime::SET_TO_SERVER_TIME => Some(now),
                set_mtime::SET_TO_CLIENT_TIME(t) => Some(Timestamp { secs: t.seconds as i64, nsecs: t.nseconds }),
            },
        }
    }

    fn ro(&self) -> Result<(), nfsstat3> {
        if self.read_only {
            Err(nfsstat3::NFS3ERR_ROFS)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl<S: FileSystem + Send + 'static> NFSFileSystem for NfsBridge<S> {
    fn capabilities(&self) -> VFSCapabilities {
        if self.read_only {
            VFSCapabilities::ReadOnly
        } else {
            VFSCapabilities::ReadWrite
        }
    }

    fn root_dir(&self) -> fileid3 {
        self.fs.lock().map(|g| g.root_ino()).unwrap_or(1)
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = name_of(filename)?;
        if name == "." {
            return Ok(dirid);
        }
        if name == ".." {
            // The macOS client resolves ".." itself for NFSv3 in practice; support it anyway.
            return self.with(|fs| {
                let root = fs.root_ino();
                if dirid == root {
                    return Ok(root);
                }
                Err(FsError::NotFound)
            });
        }
        self.with(|fs| fs.lookup(dirid, &name))
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.with(|fs| fs.getattr(id)).map(|a| self.fattr(&a))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        self.ro()?;
        let s = Self::to_setattr(&setattr);
        let (a, changed) = self.with(|fs| {
            let before = fs.getattr(id)?;
            if s.is_empty() {
                return Ok((before, false));
            }
            let after = fs.setattr(id, &s)?;
            Ok((after, setattr_changes(&s, &before)))
        })?;
        // The macOS client repeats SETATTRs that change nothing (cp: the same mode up to three
        // times per file). Those have nothing to make durable (apart from ctime) and don't
        // wait for a group commit; the next one covers them.
        if changed {
            self.written("SETATTR")?;
        }
        if s.mtime.is_some() || s.atime.is_some() {
            let mut ex = self.tracker.exact.lock().unwrap_or_else(|e| e.into_inner());
            let e = ex.entry(id).or_insert((None, None));
            if s.mtime.is_some() {
                e.0 = s.mtime;
            }
            if s.atime.is_some() {
                e.1 = s.atime;
            }
        }
        if s.size.is_some() && s.mtime.is_none() {
            self.touch(&[id]);
        } else if !s.is_empty() {
            self.touch_meta(id, s.mtime.is_some());
        }
        Ok(self.fattr(&a))
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.with(|fs| {
            let size = fs.getattr(id)?.size;
            let d = fs.read(id, offset, count)?;
            let eof = offset + d.len() as u64 >= size;
            Ok((d, eof))
        })
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        self.ro()?;
        let a = self.with(|fs| {
            fs.write(id, offset, data)?;
            fs.getattr(id)
        })?;
        if let Some(e) = self.tracker.exact.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
            e.0 = None;
        }
        self.touch(&[id]);
        Ok(self.fattr(&a))
    }

    async fn write_stable(&self, id: fileid3, offset: u64, data: &[u8], stable: stable_how) -> Result<(fattr3, stable_how), nfsstat3> {
        let a = self.write(id, offset, data).await?;
        trace(format_args!("WRITE {id} {offset}+{} {stable:?}", data.len()));
        if stable == stable_how::UNSTABLE || self.durable.is_none() {
            // Nothing stronger can be promised without a durability provider: the client
            // COMMITs (which then has nothing to wait for either).
            return Ok((a, stable_how::UNSTABLE));
        }
        self.durable("WRITE").await?;
        // The whole volume state is durable, data and metadata: FILE_SYNC also covers DATA_SYNC.
        Ok((a, stable_how::FILE_SYNC))
    }

    async fn commit(&self, id: fileid3, offset: u64, count: u32) -> Result<fattr3, nfsstat3> {
        trace(format_args!("COMMIT {id} {offset}+{count}"));
        self.durable("COMMIT").await?;
        self.getattr(id).await
    }

    fn write_verifier(&self) -> writeverf3 {
        self.verifier
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        self.ro()?;
        let name = name_of(filename)?;
        let mut s = Self::to_setattr(&attr);
        let perm = s.perm.take().unwrap_or(0o644);
        let (uid, gid) = (self.owner_uid, self.owner_gid);
        s.uid = None;
        s.gid = None;
        let a = self.with(|fs| {
            let ino = fs.create(dirid, &name, perm, uid, gid)?;
            if s.is_empty() {
                fs.getattr(ino)
            } else {
                fs.setattr(ino, &s)
            }
        })?;
        self.written("CREATE")?;
        self.touch(&[dirid, a.ino]);
        Ok((a.ino, self.fattr(&a)))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        self.ro()?;
        let name = name_of(filename)?;
        let (uid, gid) = (self.owner_uid, self.owner_gid);
        let ino = self.with(|fs| fs.create(dirid, &name, 0o644, uid, gid))?;
        self.written("CREATE")?;
        self.touch(&[dirid, ino]);
        Ok(ino)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        self.ro()?;
        let name = name_of(dirname)?;
        let (uid, gid) = (self.owner_uid, self.owner_gid);
        let a = self.with(|fs| {
            let ino = fs.mkdir(dirid, &name, 0o755, uid, gid)?;
            fs.getattr(ino)
        })?;
        self.written("MKDIR")?;
        self.touch(&[dirid, a.ino]);
        Ok((a.ino, self.fattr(&a)))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        self.ro()?;
        let name = name_of(filename)?;
        let ino = self.with(|fs| {
            let ino = fs.lookup(dirid, &name)?;
            if fs.getattr(ino)?.kind == FileKind::Dir {
                fs.rmdir(dirid, &name)?;
            } else {
                fs.unlink(dirid, &name)?;
            }
            Ok(ino)
        })?;
        self.written("REMOVE")?;
        self.touch(&[dirid]);
        self.touch_meta(ino, false);
        Ok(())
    }

    async fn rename(&self, from_dirid: fileid3, from_filename: &filename3, to_dirid: fileid3, to_filename: &filename3) -> Result<(), nfsstat3> {
        self.ro()?;
        let (a, b) = (name_of(from_filename)?, name_of(to_filename)?);
        let moved = self.with(|fs| {
            let m = fs.lookup(from_dirid, &a)?;
            fs.rename(from_dirid, &a, to_dirid, &b)?;
            Ok(m)
        })?;
        self.written("RENAME")?;
        self.touch(&[from_dirid, to_dirid]);
        self.touch_meta(moved, false);
        Ok(())
    }

    async fn readdir(&self, dirid: fileid3, start_after: fileid3, max_entries: usize) -> Result<ReadDirResult, nfsstat3> {
        self.with(|fs| {
            let entries = fs.readdir(dirid)?;
            let start = if start_after == 0 {
                0
            } else {
                match entries.iter().position(|e| e.ino == start_after) {
                    Some(p) => p + 1,
                    None => return Err(FsError::Invalid("bad cookie".into())),
                }
            };
            let mut out = ReadDirResult { entries: vec![], end: false };
            for e in entries.iter().skip(start).take(max_entries) {
                let a = fs.getattr(e.ino)?;
                out.entries.push(NfsDirEntry { fileid: e.ino, name: e.name.as_bytes().to_vec().into(), attr: self.fattr(&a) });
            }
            out.end = start + out.entries.len() >= entries.len();
            Ok(out)
        })
        .map_err(|e| if matches!(e, nfsstat3::NFS3ERR_INVAL) { nfsstat3::NFS3ERR_BAD_COOKIE } else { e })
    }

    async fn symlink(&self, dirid: fileid3, linkname: &filename3, symlink: &nfspath3, _attr: &sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        self.ro()?;
        let name = name_of(linkname)?;
        let target = String::from_utf8(symlink.0.clone()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let (uid, gid) = (self.owner_uid, self.owner_gid);
        let a = self.with(|fs| {
            let ino = fs.symlink(dirid, &name, &target, uid, gid)?;
            fs.getattr(ino)
        })?;
        self.written("SYMLINK")?;
        self.touch(&[dirid, a.ino]);
        Ok((a.ino, self.fattr(&a)))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        self.with(|fs| fs.readlink(id)).map(|s| s.into_bytes().into())
    }

    async fn fsstat(&self) -> Result<(u64, u64, u64, u64, u64), nfsstat3> {
        self.with(|fs| fs.statfs()).map(|s| (s.total_bytes, s.free_bytes, s.free_bytes, s.total_inodes, s.free_inodes))
    }

    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&self.secret);
        data.extend_from_slice(&id.to_le_bytes());
        nfs_fh3 { data }
    }

    fn fh_to_id(&self, fh: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        if fh.data.len() != 24 {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        }
        // constant-time compare of the 128-bit secret
        let diff = fh.data[..16].iter().zip(self.secret.iter()).fold(0u8, |a, (x, y)| a | (x ^ y));
        if diff != 0 {
            return Err(nfsstat3::NFS3ERR_STALE);
        }
        Ok(u64::from_le_bytes(fh.data[16..24].try_into().unwrap()))
    }
}

/// A running loopback server.
pub struct LoopbackServer {
    pub port: u16,
    pub export: String,
    pub security: Arc<nfsserve::context::Security>,
    task: tokio::task::JoinHandle<()>,
}

impl LoopbackServer {
    pub fn stop(&self) {
        self.task.abort();
    }
}

/// Bind a server on 127.0.0.1:0 and start serving on `rt`.
pub fn start_server<S: FileSystem + Send + 'static>(rt: &tokio::runtime::Runtime, bridge: NfsBridge<S>) -> std::io::Result<LoopbackServer> {
    // 128 random bits. The path is visible to other local users while mount_nfs runs (ps),
    // so it is not what keeps them out (see `peer_is_trusted`), but it should not be guessable.
    let export = format!("lumendisk-{}", uuid::Uuid::new_v4().simple());
    let exp = export.clone();
    loopback_access_check().map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    let mut listener = rt.block_on(async move { NFSTcpListener::bind("127.0.0.1:0", bridge).await })?;
    listener.with_export_name(&exp);
    let port = listener.get_listen_port();
    let security = security_policy();
    listener.set_security(security.clone());
    listener.set_connection_filter(Arc::new(move |peer| peer_is_trusted(peer, port)));
    let task = rt.spawn(async move {
        if let Err(e) = listener.handle_forever().await {
            eprintln!("lumendisk: NFS server stopped: {e}");
        }
    });
    Ok(LoopbackServer { port, export: format!("/{export}"), security, task })
}

pub fn mount_options(port: u16, read_only: bool) -> String {
    let mut o = format!(
        "locallocks,vers=3,tcp,noresvport,rsize=65536,wsize=65536,actimeo=1,soft,intr,timeo=600,retrans=5,nfc,port={port},mountport={port}"
    );
    if read_only {
        o.push_str(",rdonly");
    }
    o
}

/// Default root for mountpoints: /Volumes when writable by us, else ~/Volumes.
pub fn default_mount_root() -> PathBuf {
    let v = Path::new("/Volumes");
    let probe = v.join(format!(".lumendisk-probe-{}", std::process::id()));
    if std::fs::create_dir(&probe).is_ok() {
        let _ = std::fs::remove_dir(&probe);
        return v.to_path_buf();
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Volumes")
}

/// Pick a unique mountpoint "<root>/<name>", "<root>/<name> 2", …
pub fn pick_mountpoint(root: &Path, name: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    let clean: String = name.chars().map(|c| if c == '/' || c == ':' || c.is_control() { '_' } else { c }).collect();
    let clean = if clean.trim().is_empty() { "LumenDisk".to_string() } else { clean.trim().to_string() };
    for k in 1..100 {
        let p = if k == 1 { root.join(&clean) } else { root.join(format!("{clean} {k}")) };
        if !p.exists() {
            std::fs::create_dir(&p)?;
            return Ok(p);
        }
        // Reuse an empty, unmounted leftover directory.
        if p.is_dir() && std::fs::read_dir(&p).map(|mut d| d.next().is_none()).unwrap_or(false) && !is_mountpoint(&p) {
            return Ok(p);
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free mountpoint name"))
}

pub fn is_mountpoint(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(a), Some(parent)) = (std::fs::metadata(p), p.parent()) else { return false };
    std::fs::metadata(parent).map(|b| a.dev() != b.dev()).unwrap_or(false)
}

pub fn mount_nfs(port: u16, export: &str, mountpoint: &Path, read_only: bool) -> Result<(), String> {
    let out = Command::new("/sbin/mount_nfs")
        .arg("-o")
        .arg(mount_options(port, read_only))
        .arg(format!("127.0.0.1:{export}"))
        .arg(mountpoint)
        .output()
        .map_err(|e| format!("mount_nfs: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("mount_nfs failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

pub fn unmount(mountpoint: &Path, force: bool) -> Result<(), String> {
    let mut cmd = Command::new("/sbin/umount");
    if force {
        cmd.arg("-f");
    }
    let out = cmd.arg(mountpoint).output().map_err(|e| e.to_string())?;
    if out.status.success() || !is_mountpoint(mountpoint) {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    // diskutil can sometimes succeed where umount reports EBUSY transiently.
    let mut d = Command::new("/usr/sbin/diskutil");
    d.arg("unmount");
    if force {
        d.arg("force");
    }
    let o2 = d.arg(mountpoint).output().map_err(|e| e.to_string())?;
    if o2.status.success() || !is_mountpoint(mountpoint) {
        Ok(())
    } else {
        Err(if err.is_empty() { "unmount failed".into() } else { err })
    }
}

// ───────────────────────────── loopback access control ─────────────────────────────

/// Owner of the client end of a loopback TCP connection, as reported by the system
/// `netstat -anv` (the unprivileged way to see socket owners on macOS).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerOwner {
    Kernel,
    Process { name: String, pid: i32 },
}

/// Parse `netstat -anv -p tcp` output: find the socket whose local address is
/// 127.0.0.1.<client_port> and foreign address 127.0.0.1.<server_port>.
pub fn parse_netstat_owner(out: &str, client_port: u16, server_port: u16) -> Option<PeerOwner> {
    let local = format!("127.0.0.1.{client_port}");
    let foreign = format!("127.0.0.1.{server_port}");
    for line in out.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 11 || !cols[0].starts_with("tcp") || cols[3] != local || cols[4] != foreign {
            continue;
        }
        // Columns: proto recvq sendq local foreign state rxbytes txbytes rhiwat shiwat process:pid …
        // The process name may contain spaces: take everything after the 10th column and
        // find the first "<name>:<digits>" token boundary.
        let mut idx = 0;
        let mut rest = line;
        for _ in 0..10 {
            rest = rest.trim_start();
            idx = rest.find(char::is_whitespace).unwrap_or(rest.len());
            rest = &rest[idx..];
        }
        let _ = idx;
        let rest = rest.trim_start();
        let bytes = rest.as_bytes();
        for (i, &c) in bytes.iter().enumerate() {
            if c == b':' {
                let digits: String = rest[i + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
                let after = rest[i + 1 + digits.len()..].chars().next();
                if !digits.is_empty() && after.map(|c| c.is_whitespace()).unwrap_or(true) {
                    let name = rest[..i].trim().to_string();
                    let pid: i32 = digits.parse().ok()?;
                    return Some(if pid == 0 && name == "kernel_task" { PeerOwner::Kernel } else { PeerOwner::Process { name, pid } });
                }
            }
        }
        return None;
    }
    None
}

fn uid_of_pid(pid: i32) -> Option<u32> {
    let out = Command::new("/bin/ps").args(["-o", "uid=", "-p", &pid.to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Connection filter, **fail-closed**. A connection is accepted only when its client socket is
/// positively identified as belonging to:
/// * the kernel (`kernel_task`, pid 0): the macOS NFS client, which performs both the MOUNT
///   handshake and all NFS traffic for `mount_nfs` mounts and stamps every call with the real
///   uid of the process doing the file operation (that is what makes the AUTH_SYS uid check
///   meaningful), or
/// * a process running as the same user as LumenDisk, or as root (both can already read the
///   disk image or device directly, so they gain nothing).
///
/// Anything else is refused, including when the owner cannot be determined at all (netstat
/// missing or failing, row not found after retries). This filter is what stops another local
/// user from talking to the server with a hand-made NFS client and a forged AUTH_SYS uid; the
/// random export path, single MOUNT and 128-bit handle secret are additional layers.
/// See SECURITY.md for the threat model.
pub fn peer_is_trusted(peer: std::net::SocketAddr, server_port: u16) -> bool {
    peer_is_trusted_with(peer, server_port, socket_table_visible(), unverified_loopback_allowed())
}

fn peer_is_trusted_with(peer: std::net::SocketAddr, server_port: u16, table_visible: bool, allow_unverified: bool) -> bool {
    if !peer.ip().is_loopback() {
        return false;
    }
    if !table_visible {
        // Development build: nothing can be verified. Only an explicit opt-in lets it through.
        if !allow_unverified {
            eprintln!("lumendisk: refused loopback connection from {peer}: this build cannot verify connections");
        }
        return allow_unverified;
    }
    let me = unsafe { libc::geteuid() };
    let verdict = peer_owner(peer, server_port).and_then(|owner| match owner {
        PeerOwner::Kernel => Ok(()),
        PeerOwner::Process { pid, name } => match uid_of_pid(pid) {
            Some(u) if u == me || u == 0 => Ok(()),
            uid => Err(format!("owned by {name}:{pid} uid {uid:?}")),
        },
    });
    if let Err(why) = &verdict {
        eprintln!("lumendisk: refused loopback connection from {peer}: {why}");
    }
    verdict.is_ok()
}

/// macOS only shows the system socket table to processes signed with a Developer ID; ad-hoc
/// and Apple Development builds get an empty table from netstat (and from the pcblist
/// sysctls), so for them no connection owner can ever be verified.
pub fn socket_table_visible() -> bool {
    static VISIBLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VISIBLE.get_or_init(|| {
        // Our own listener guarantees at least one row whenever the table is visible.
        let l = std::net::TcpListener::bind("127.0.0.1:0");
        let out = Command::new("/usr/sbin/netstat").args(["-anv", "-f", "inet"]).output();
        drop(l);
        out.map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.starts_with("tcp"))).unwrap_or(false)
    })
}

/// Explicit opt-in for development builds that cannot verify loopback peers.
pub const ALLOW_UNVERIFIED_ENV: &str = "LUMENDISK_ALLOW_UNVERIFIED_LOOPBACK";

pub fn unverified_loopback_allowed() -> bool {
    std::env::var(ALLOW_UNVERIFIED_ENV).map(|v| v == "1").unwrap_or(false)
}

/// Refuse to serve at all when connections can't be verified and nobody opted in, with an
/// error people can act on (instead of mount_nfs timing out on refused connections).
pub fn loopback_access_check() -> Result<(), String> {
    if socket_table_visible() {
        return Ok(());
    }
    if unverified_loopback_allowed() {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!("lumendisk: WARNING: {ALLOW_UNVERIFIED_ENV}=1 — loopback connections are NOT verified (development build)")
        });
        return Ok(());
    }
    Err(crate::i18n::t(
        &format!("questa build di LumenDisk non è firmata con un Developer ID, quindi macOS non le permette di verificare chi si collega al server locale. Usa la versione ufficiale, oppure (solo per sviluppo) imposta {ALLOW_UNVERIFIED_ENV}=1"),
        &format!("this LumenDisk build is not signed with a Developer ID, so macOS doesn't let it verify who connects to its local server. Use the official release, or (development only) set {ALLOW_UNVERIFIED_ENV}=1"),
    ))
}

/// Who owns the client end of `peer` → `127.0.0.1:server_port`.
fn peer_owner(peer: std::net::SocketAddr, server_port: u16) -> Result<PeerOwner, String> {
    let run = |args: &[&str]| -> Option<String> {
        let o = Command::new("/usr/sbin/netstat").args(args).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    // The socket table can lag the accept() by a few milliseconds: retry for up to ~0.6 s.
    // The kernel NFS client simply reconnects if a connection is refused.
    let mut delay = 10;
    for _ in 0..7 {
        // Note: `netstat -p tcp` prints nothing when spawned from some processes on macOS 27,
        // so ask for the IPv4 table (falling back to everything) and filter rows ourselves.
        let out = run(&["-anv", "-f", "inet"])
            .filter(|o| o.lines().any(|l| l.starts_with("tcp")))
            .or_else(|| run(&["-anv"]))
            .ok_or_else(|| "cannot read the socket table (netstat failed)".to_string())?;
        if let Some(owner) = parse_netstat_owner(&out, peer.port(), server_port) {
            return Ok(owner);
        }
        std::thread::sleep(std::time::Duration::from_millis(delay));
        delay *= 2;
    }
    Err("socket owner not found".into())
}

/// The access policy for one volume's server.
pub fn security_policy() -> Arc<nfsserve::context::Security> {
    let me = unsafe { libc::geteuid() };
    Arc::new(nfsserve::context::Security { allowed_uids: vec![me, 0], single_mount: true, ..Default::default() })
}

#[cfg(test)]
mod panic_tests {
    use super::*;
    use crate::block::MemDev;
    use crate::vfs::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A FAT volume behind a layer that panics on one inode (a bug above the driver fence).
    struct Flaky {
        inner: Box<dyn FileSystem>,
        bad: Ino,
        calls: Arc<AtomicU32>,
    }

    macro_rules! fwd {
        ($s:ident, $e:expr) => {{
            $s.calls.fetch_add(1, Ordering::SeqCst);
            $e
        }};
    }

    impl FileSystem for Flaky {
        fn volume_info(&self) -> VolumeInfo {
            self.inner.volume_info()
        }
        fn statfs(&mut self) -> FsResult<FsStat> {
            fwd!(self, self.inner.statfs())
        }
        fn root_ino(&self) -> Ino {
            self.inner.root_ino()
        }
        fn lookup(&mut self, p: Ino, n: &str) -> FsResult<Ino> {
            fwd!(self, self.inner.lookup(p, n))
        }
        fn getattr(&mut self, i: Ino) -> FsResult<Attr> {
            if i == self.bad {
                panic!("simulated bug");
            }
            fwd!(self, self.inner.getattr(i))
        }
        fn setattr(&mut self, i: Ino, s: &SetAttr) -> FsResult<Attr> {
            fwd!(self, self.inner.setattr(i, s))
        }
        fn read(&mut self, i: Ino, o: u64, l: u32) -> FsResult<Vec<u8>> {
            fwd!(self, self.inner.read(i, o, l))
        }
        fn write(&mut self, i: Ino, o: u64, d: &[u8]) -> FsResult<u32> {
            fwd!(self, self.inner.write(i, o, d))
        }
        fn readdir(&mut self, d: Ino) -> FsResult<Vec<DirEntry>> {
            fwd!(self, self.inner.readdir(d))
        }
        fn create(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> {
            fwd!(self, self.inner.create(p, n, m, u, g))
        }
        fn mkdir(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> {
            fwd!(self, self.inner.mkdir(p, n, m, u, g))
        }
        fn symlink(&mut self, p: Ino, n: &str, t: &str, u: u32, g: u32) -> FsResult<Ino> {
            fwd!(self, self.inner.symlink(p, n, t, u, g))
        }
        fn readlink(&mut self, i: Ino) -> FsResult<String> {
            fwd!(self, self.inner.readlink(i))
        }
        fn unlink(&mut self, p: Ino, n: &str) -> FsResult<()> {
            fwd!(self, self.inner.unlink(p, n))
        }
        fn rmdir(&mut self, p: Ino, n: &str) -> FsResult<()> {
            fwd!(self, self.inner.rmdir(p, n))
        }
        fn rename(&mut self, a: Ino, b: &str, c: Ino, d: &str) -> FsResult<()> {
            fwd!(self, self.inner.rename(a, b, c, d))
        }
        fn sync(&mut self) -> FsResult<()> {
            fwd!(self, self.inner.sync())
        }
        fn dirty_state(&mut self) -> DirtyState {
            self.inner.dirty_state()
        }
        fn repair(&mut self) -> FsResult<String> {
            fwd!(self, self.inner.repair())
        }
    }

    /// After a caught panic the server answers EIO to everything, immediately, without
    /// entering the stack again; the lock stays usable for the runtime (sync / unmount).
    #[test]
    fn panic_fails_the_volume_fast() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).build().unwrap();
        let dev = Arc::new(MemDev::new(4 << 20));
        crate::vfs::fat_fs::format(dev.clone(), FsType::Fat16, "NFSP").unwrap();
        let d = MountDefaults { uid: 501, gid: 20, file_perm: 0o644, dir_perm: 0o755 };
        let mut inner = open_fs(dev, false, d).unwrap();
        let bad = inner.create(1, "bad", 0o644, 0, 0).unwrap();
        let good = inner.create(1, "good", 0o644, 0, 0).unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let fs = Arc::new(Mutex::new(Flaky { inner, bad, calls: calls.clone() }));
        let tracker = Arc::new(ChangeTracker::default());
        let bridge = NfsBridge::new(fs.clone(), false, 1, tracker.clone());
        rt.block_on(async {
            assert!(bridge.getattr(good).await.is_ok());
            assert!(matches!(bridge.getattr(bad).await, Err(nfsstat3::NFS3ERR_IO)));
            let before = calls.load(Ordering::SeqCst);
            let t0 = std::time::Instant::now();
            assert!(matches!(bridge.getattr(good).await, Err(nfsstat3::NFS3ERR_IO)));
            assert!(matches!(bridge.read(good, 0, 10).await, Err(nfsstat3::NFS3ERR_IO)));
            assert!(matches!(bridge.fsstat().await, Err(nfsstat3::NFS3ERR_IO)));
            assert!(t0.elapsed() < std::time::Duration::from_secs(1));
            assert_eq!(calls.load(Ordering::SeqCst), before, "the stack was used after the panic");
        });
        assert!(tracker.failure().unwrap().contains("simulated bug"));
        assert!(!fs.is_poisoned());
        assert!(fs.try_lock().is_ok());
    }
}

#[cfg(test)]
mod netstat_tests {
    use super::*;
    const SAMPLE: &str = "Active Internet connections (including servers)
Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)          rxbytes      txbytes  rhiwat  shiwat          process:pid    state  options           gencnt    flags   flags1 usecnt rtncnt fltrs
tcp4       0      0  127.0.0.1.11811        127.0.0.1.50102        ESTABLISHED         5336         2148  406464  146988         nfsproto:74671  00102 00000004 0000000000941c3e 00000081 01000800      2      0 000000
tcp4       0      0  127.0.0.1.50102        127.0.0.1.11811        ESTABLISHED         5960         2252 6291456 6291456      Google Chrome He:74714  00082 00000008 0000000000941c3d 00000882 04100900      2      0 000000
tcp4       0      0  127.0.0.1.50101        127.0.0.1.11811        ESTABLISHED         5960         2252 6291456 6291456      kernel_task:0      00082 00000008 0000000000941c3d 00000882 04100900      2      0 000000
";
    #[test]
    fn parses_owners() {
        assert_eq!(parse_netstat_owner(SAMPLE, 50101, 11811), Some(PeerOwner::Kernel));
        assert_eq!(parse_netstat_owner(SAMPLE, 50102, 11811), Some(PeerOwner::Process { name: "Google Chrome He".into(), pid: 74714 }));
        assert_eq!(parse_netstat_owner(SAMPLE, 1, 11811), None);
    }

    #[test]
    fn peer_filter_is_fail_closed() {
        use std::net::{SocketAddr, TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (_s, peer) = l.accept().unwrap();
        let ghost: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let remote: SocketAddr = "192.0.2.7:50000".parse().unwrap();
        // Never anything off-loopback, whatever the build or opt-in.
        for (vis, allow) in [(true, false), (true, true), (false, false), (false, true)] {
            assert!(!peer_is_trusted_with(remote, port, vis, allow));
        }
        // Table hidden (development build): refused unless explicitly allowed.
        assert!(!peer_is_trusted_with(peer, port, false, false));
        assert!(peer_is_trusted_with(peer, port, false, true));
        if socket_table_visible() {
            // Developer ID build: this very process (same uid) is identified and accepted…
            assert!(peer_is_trusted_with(peer, port, true, false));
            // …while a peer whose owner can't be found is refused, not waved through.
            assert!(!peer_is_trusted_with(ghost, port, true, false));
            assert!(!peer_is_trusted_with(ghost, port, true, true), "the opt-in never loosens a verifiable build");
        }
        drop(c);
    }
}


#[cfg(test)]
mod group_commit_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// Requests arriving while a round runs share the next one; a failed round fails only the
    /// requests it was meant to cover.
    #[test]
    fn concurrent_requests_share_rounds() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let fail_next = Arc::new(AtomicBool::new(false));
        let (c, f) = (calls.clone(), fail_next.clone());
        let g = GroupCommit::new(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(30));
            if f.swap(false, Ordering::SeqCst) {
                Err("disk unplugged".into())
            } else {
                Ok(())
            }
        }), Box::new(|| Ok(())));
        rt.block_on(async {
            let mut tasks = vec![];
            for _ in 0..64 {
                let g = g.clone();
                tasks.push(tokio::spawn(async move { g.wait().await }));
            }
            for t in tasks {
                t.await.unwrap().unwrap();
            }
        });
        let rounds = calls.load(Ordering::SeqCst);
        assert!((1..=3).contains(&rounds), "64 concurrent requests took {rounds} rounds");
        assert_eq!(g.stats(), (rounds as u64, 64, 0));
        // A failing round reports the error to its waiters; the next request succeeds again.
        fail_next.store(true, Ordering::SeqCst);
        rt.block_on(async {
            assert_eq!(g.wait().await, Err("disk unplugged".to_string()));
            assert_eq!(g.wait().await, Ok(()));
        });
    }

    /// The reply contract of every RPC: metadata operations are *written* (one write-through
    /// each, no full sync), FILE_SYNC / DATA_SYNC writes and COMMIT wait for a durable round,
    /// UNSTABLE writes for nothing.
    #[test]
    fn metadata_is_written_stable_writes_and_commit_are_durable() {
        use crate::block::MemDev;
        use crate::vfs::{FsType, MountDefaults};
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let dev = Arc::new(MemDev::new(8 << 20));
        crate::vfs::fat_fs::format(dev.clone(), FsType::Fat16, "RPC").unwrap();
        let fs = Arc::new(Mutex::new(crate::vfs::fat_fs::FatFs::open(dev, false, MountDefaults { uid: 501, gid: 20, file_perm: 0o644, dir_perm: 0o755 }).unwrap()));
        let (full, wr) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let (f2, w2) = (full.clone(), wr.clone());
        let g = GroupCommit::new(
            Box::new(move || {
                f2.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            Box::new(move || {
                w2.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        );
        let bridge = NfsBridge::new(fs, false, 1, Arc::new(ChangeTracker::default())).with_durability(Some(g));
        let counts = || (full.load(Ordering::SeqCst), wr.load(Ordering::SeqCst));
        let name = |n: &str| -> filename3 { n.as_bytes().into() };
        rt.block_on(async {
            let root = bridge.root_dir();
            let (d, _) = bridge.mkdir(root, &name("d")).await.unwrap();
            assert_eq!(counts(), (0, 1), "MKDIR: written, no full sync");
            let mode = |m| sattr3 { mode: set_mode3::mode(m), ..Default::default() };
            let (f, _) = bridge.create(d, &name("f"), mode(0o644)).await.unwrap();
            assert_eq!(counts(), (0, 2), "CREATE");
            bridge.setattr(f, sattr3 { size: set_size3::size(10), ..Default::default() }).await.unwrap();
            assert_eq!(counts(), (0, 3), "SETATTR that changes something");
            bridge.setattr(f, sattr3 { size: set_size3::size(10), ..Default::default() }).await.unwrap();
            assert_eq!(counts(), (0, 3), "SETATTR that changes nothing");
            let (_, how) = bridge.write_stable(f, 0, b"unstable", stable_how::UNSTABLE).await.unwrap();
            assert_eq!((how, counts()), (stable_how::UNSTABLE, (0, 3)), "UNSTABLE WRITE waits for nothing");
            bridge.commit(f, 0, 0).await.unwrap();
            assert_eq!(counts(), (1, 3), "COMMIT: one durable round");
            for stable in [stable_how::FILE_SYNC, stable_how::DATA_SYNC] {
                let before = counts().0;
                let (_, how) = bridge.write_stable(f, 0, b"stable", stable).await.unwrap();
                assert_eq!((how, counts().0), (stable_how::FILE_SYNC, before + 1), "{stable:?} WRITE: durable before the reply");
            }
            bridge.rename(d, &name("f"), d, &name("g")).await.unwrap();
            bridge.remove(d, &name("g")).await.unwrap();
            bridge.remove(root, &name("d")).await.unwrap();
            bridge.symlink(root, &name("l"), &b"x".as_slice().into(), &sattr3::default()).await.ok(); // FAT: unsupported
            assert_eq!(counts(), (3, 6), "RENAME, REMOVE, RMDIR: written");
            assert_eq!(bridge.write_verifier(), bridge.write_verifier());
        });
    }
}
