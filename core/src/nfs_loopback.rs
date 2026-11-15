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
}

impl ChangeTracker {
    /// The volume content was replaced wholesale (Shadow rollback/commit): forget overrides and
    /// make every directory look modified so the kernel drops its name / attribute caches.
    pub fn invalidate_all(&self) {
        self.changed.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.exact.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *self.dir_floor.lock().unwrap_or_else(|e| e.into_inner()) = Timestamp::now();
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
        NfsBridge { fs, read_only, secret, owner_uid, owner_gid, fsid, tracker }
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
    /// panicking driver turns into NFS3ERR_IO instead of taking the whole volume down.
    fn with<T>(&self, f: impl FnOnce(&mut S) -> Result<T, FsError>) -> Result<T, nfsstat3> {
        let run = || {
            let mut g = self.fs.lock().unwrap_or_else(|e| e.into_inner());
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut g))) {
                Ok(r) => r.map_err(|e| {
                    if std::env::var_os("LUMENDISK_TRACE").is_some() {
                        eprintln!("lumendisk-trace: {} -> {e:?}", std::any::type_name::<T>());
                    }
                    map_err(e)
                }),
                Err(_) => {
                    eprintln!("lumendisk: driver panic caught, returning EIO");
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
        let a = self.with(|fs| if s.is_empty() { fs.getattr(id) } else { fs.setattr(id, &s) })?;
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
        self.touch(&[dirid, a.ino]);
        Ok((a.ino, self.fattr(&a)))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        self.ro()?;
        let name = name_of(filename)?;
        let (uid, gid) = (self.owner_uid, self.owner_gid);
        let ino = self.with(|fs| fs.create(dirid, &name, 0o644, uid, gid))?;
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
    let export = format!("lumendisk-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let exp = export.clone();
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
/// `netstat -anv` (the only unprivileged way to see socket owners on recent macOS: the
/// pcblist sysctls are filtered for unentitled processes).
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

/// Defense-in-depth connection filter: refuse a peer that is positively identified as a
/// process of another (non-root) user. The primary protections are the random export path,
/// the single MOUNT handshake, 128-bit secret file handles and AUTH_SYS uid enforcement.
pub fn peer_is_trusted(peer: std::net::SocketAddr, server_port: u16) -> bool {
    if !peer.ip().is_loopback() {
        return false;
    }
    let me = unsafe { libc::geteuid() };
    for attempt in 0..3 {
        // Note: `netstat -p tcp` prints nothing when spawned from some processes on macOS 27,
        // so ask for the IPv4 table (falling back to everything) and filter rows ourselves.
        let run = |args: &[&str]| -> Option<String> {
            let o = Command::new("/usr/sbin/netstat").args(args).output().ok()?;
            o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        let out = match run(&["-anv", "-f", "inet"]).filter(|o| o.lines().any(|l| l.starts_with("tcp"))).or_else(|| run(&["-anv"])) {
            Some(o) => o,
            None => {
                eprintln!("lumendisk: netstat unavailable, falling back to AUTH_SYS checks only");
                return true;
            }
        };
        match parse_netstat_owner(&out, peer.port(), server_port) {
            Some(PeerOwner::Kernel) => return true,
            Some(PeerOwner::Process { pid, name }) => {
                let uid = uid_of_pid(pid);
                let ok = matches!(uid, Some(u) if u == me || u == 0);
                if !ok {
                    eprintln!("lumendisk: peer {peer} owned by {name}:{pid} uid {uid:?} → refused");
                }
                return ok;
            }
            None if attempt < 2 => std::thread::sleep(std::time::Duration::from_millis(30)),
            // Unsigned / non-platform processes get a filtered socket table on recent macOS, so
            // "not found" is not evidence of anything: fail open and rely on the export secret,
            // single MOUNT, 128-bit handle secret and AUTH_SYS uid checks.
            None => return true,
        }
    }
    false
}

/// The access policy for one volume's server.
pub fn security_policy() -> Arc<nfsserve::context::Security> {
    let me = unsafe { libc::geteuid() };
    Arc::new(nfsserve::context::Security { allowed_uids: vec![me, 0], single_mount: true, ..Default::default() })
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
}

