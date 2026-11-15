//! Thread-safe runtime manager: assembles each volume's VFS stack, runs its loopback NFS
//! server, mounts / unmounts it and exposes status for the UI and the CLI.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};

use crate::block::{self, BlockDev, CachedRangeDev, PartitionDev};
use crate::remote::cache::{RemoteConfig, RemoteHandle, RemoteHealth, RemoteVfs};
use crate::remote::RemoteBackend;
use crate::wal::{Journaled, WalDev, WalStats};
use crate::nfs_loopback::{self, ChangeTracker, GroupCommit, LoopbackServer, NfsBridge};
use crate::vfs::ghost::{GhostEntry, GhostRules, GhostShield};
use crate::vfs::shadow::{Durable, ShadowChange, ShadowOverlay, ShadowSummary};
use crate::vfs::shadow_store::{self, Identity, Manifest};
use crate::vfs::uidmap::{self, PermissionBridge, SharedUidMap, UidMap};
use crate::vfs::*;

// ───────────────────────────── stack ─────────────────────────────

/// What sits under Ghost Shield: the (bridged/guarded) driver, or a Shadow overlay over it.
pub enum Lower {
    Direct(Box<dyn FileSystem>),
    Shadow(ShadowOverlay),
}

macro_rules! delegate {
    ($self:ident, $f:ident ( $($a:expr),* )) => {
        match $self {
            Lower::Direct(x) => x.$f($($a),*),
            Lower::Shadow(x) => x.$f($($a),*),
        }
    };
}

impl FileSystem for Lower {
    fn volume_info(&self) -> VolumeInfo {
        delegate!(self, volume_info())
    }
    fn statfs(&mut self) -> FsResult<FsStat> {
        delegate!(self, statfs())
    }
    fn root_ino(&self) -> Ino {
        delegate!(self, root_ino())
    }
    fn lookup(&mut self, p: Ino, n: &str) -> FsResult<Ino> {
        delegate!(self, lookup(p, n))
    }
    fn getattr(&mut self, i: Ino) -> FsResult<Attr> {
        delegate!(self, getattr(i))
    }
    fn setattr(&mut self, i: Ino, s: &SetAttr) -> FsResult<Attr> {
        delegate!(self, setattr(i, s))
    }
    fn read(&mut self, i: Ino, o: u64, l: u32) -> FsResult<Vec<u8>> {
        delegate!(self, read(i, o, l))
    }
    fn write(&mut self, i: Ino, o: u64, d: &[u8]) -> FsResult<u32> {
        delegate!(self, write(i, o, d))
    }
    fn readdir(&mut self, d: Ino) -> FsResult<Vec<DirEntry>> {
        delegate!(self, readdir(d))
    }
    fn create(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> {
        delegate!(self, create(p, n, m, u, g))
    }
    fn mkdir(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> {
        delegate!(self, mkdir(p, n, m, u, g))
    }
    fn symlink(&mut self, p: Ino, n: &str, t: &str, u: u32, g: u32) -> FsResult<Ino> {
        delegate!(self, symlink(p, n, t, u, g))
    }
    fn readlink(&mut self, i: Ino) -> FsResult<String> {
        delegate!(self, readlink(i))
    }
    fn unlink(&mut self, p: Ino, n: &str) -> FsResult<()> {
        delegate!(self, unlink(p, n))
    }
    fn rmdir(&mut self, p: Ino, n: &str) -> FsResult<()> {
        delegate!(self, rmdir(p, n))
    }
    fn rename(&mut self, a: Ino, b: &str, c: Ino, d: &str) -> FsResult<()> {
        delegate!(self, rename(a, b, c, d))
    }
    fn sync(&mut self) -> FsResult<()> {
        delegate!(self, sync())
    }
    fn dirty_state(&mut self) -> DirtyState {
        delegate!(self, dirty_state())
    }
    fn repair(&mut self) -> FsResult<String> {
        delegate!(self, repair())
    }
    fn fingerprint(&mut self) -> Option<String> {
        delegate!(self, fingerprint())
    }
}

pub type Stack = GhostShield<Lower>;

/// Group commit of a persistent Shadow session: stage barrier and journal `F_FULLFSYNC` run
/// without the volume lock (see [`ShadowOverlay::durable_begin`]).
fn shadow_group_commit(fs: &Arc<Mutex<Stack>>) -> Arc<GroupCommit> {
    let fs = fs.clone();
    GroupCommit::new(Box::new(move || {
        let e = |e: FsError| format!("Shadow journal: {e}");
        let mut step = match &mut lock_stack(&fs).inner {
            Lower::Shadow(s) => s.durable_begin().map_err(e)?,
            Lower::Direct(_) => return Ok(()),
        };
        loop {
            step = match step {
                Durable::Done => return Ok(()),
                Durable::Stage(job) => {
                    let r = job.sync_stage();
                    match &mut lock_stack(&fs).inner {
                        Lower::Shadow(s) => s.durable_after_stage(job, r).map_err(e)?,
                        Lower::Direct(_) => return Ok(()),
                    }
                }
                Durable::Journal(job) => {
                    let r = job.sync();
                    match &mut lock_stack(&fs).inner {
                        Lower::Shadow(s) => s.durable_after_journal(job, r).map_err(e)?,
                        Lower::Direct(_) => {}
                    }
                    return Ok(());
                }
            }
        }
    }))
}

// ───────────────────────────── requests / status ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum MountMode {
    #[default]
    ReadWrite,
    Shadow,
    ReadOnly,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UidMapRequest {
    #[serde(default = "yes")]
    pub enabled: bool,
    pub linux_uid: Option<u32>,
    pub linux_gid: Option<u32>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MountRequest {
    pub path: String,
    #[serde(default)]
    pub partition: u32,
    #[serde(default)]
    pub mode: MountMode,
    #[serde(default = "yes")]
    pub ghost_shield: bool,
    #[serde(default)]
    pub ghost_rules: Option<GhostRules>,
    #[serde(default)]
    pub uid_map: Option<UidMapRequest>,
    #[serde(default)]
    pub mount_root: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub fd: Option<i32>,
    /// .icns shown as the Finder volume icon (virtualised by Ghost Shield).
    #[serde(default)]
    pub volume_icon: Option<String>,
    /// DiskArbitration media UUID of the partition, stored in a persistent Shadow session so
    /// the app recognises the disk when it is connected again.
    #[serde(default)]
    pub media_uuid: Option<String>,
    /// Resume this Shadow session (ShadowSession.key): refuse the mount if the source does not
    /// hold it.
    #[serde(default)]
    pub expect_session: Option<String>,
}

/// A persistent Shadow session as listed by `ld_shadow_sessions`.
#[derive(Clone, Debug, Serialize)]
pub struct ShadowSessionInfo {
    pub key: String,
    pub name: String,
    pub label: String,
    pub fs_type: String,
    pub uuid: String,
    pub source_path: String,
    pub partition: u32,
    pub kind: String,
    pub media_uuid: Option<String>,
    pub size: u64,
    pub created: i64,
    pub updated: i64,
    pub state: String,
    pub changes: u64,
    pub added: u64,
    pub modified: u64,
    pub deleted: u64,
    pub pending_bytes: i64,
    pub commit_applied: u64,
    pub commit_total: u64,
    pub commit_error: Option<String>,
    pub connected: bool,
    pub mounted_volume_id: Option<String>,
    pub fingerprint_ok: Option<bool>,
    pub fingerprint_problem: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteMountRequest {
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub identity_file: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub pairing_code: Option<String>,
    /// Advanced / tests: alternative ssh config & known_hosts, extra ssh options.
    #[serde(default)]
    pub ssh_config_file: Option<String>,
    #[serde(default)]
    pub known_hosts_file: Option<String>,
    #[serde(default)]
    pub extra_ssh_options: Vec<String>,
    /// LAN shares: SHA-256 of the sharing Mac's TLS certificate (Bonjour TXT `fp=`).
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub mode: MountMode,
    #[serde(default = "yes")]
    pub ghost_shield: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mount_root: Option<String>,
    #[serde(default)]
    pub volume_icon: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageUrlRequest {
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub identity_file: Option<String>,
    #[serde(default)]
    pub partition: u32,
    #[serde(default = "shadow_mode")]
    pub mode: MountMode,
    #[serde(default = "yes")]
    pub ghost_shield: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mount_root: Option<String>,
    #[serde(default)]
    pub volume_icon: Option<String>,
}

fn shadow_mode() -> MountMode {
    MountMode::Shadow
}

#[derive(Clone, Debug, Serialize)]
pub struct GhostStatus {
    pub enabled: bool,
    pub blocked_count: u64,
    pub vault_bytes: u64,
    pub rules: GhostRules,
}

#[derive(Clone, Debug, Serialize)]
pub struct VolumeStatus {
    pub id: String,
    pub name: String,
    pub source_path: String,
    pub partition: u32,
    pub fs_type: String,
    pub label: String,
    pub uuid: String,
    pub mode: MountMode,
    pub mountpoint: String,
    pub port: u16,
    pub read_only: bool,
    pub forced_read_only_reason: Option<String>,
    pub dirty: DirtyState,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub ghost: GhostStatus,
    pub shadow: Option<ShadowSummary>,
    pub uid_map: UidMap,
    pub mounted_at: i64,
    pub kind: VolumeKind,
    pub remote: Option<RemoteHealth>,
    pub share: Option<ShareStatus>,
    pub portal: Option<PortalStatus>,
    pub journal: WalStats,
    /// BSD name when mounted from a physical device ("disk4s1").
    pub device: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeKind {
    Local,
    ImageUrl,
    Sftp,
    Webdav,
    Lan,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShareStatus {
    pub scheme: &'static str,
    pub fingerprint: String,
    pub port: u16,
    pub addresses: Vec<String>,
    pub pairing_code: String,
    pub service_name: String,
    pub clients: u32,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PortalStatus {
    /// https:// (encrypted; self-signed, phones warn once).
    pub url: String,
    /// http:// compatibility address (unencrypted): only while explicitly enabled.
    pub http_url: Option<String>,
    pub fingerprint: String,
    pub pin: String,
    pub port: u16,
    pub uploads: u64,
    pub downloads: u64,
}

/// A stack ready to be published through NFS.
pub struct Prepared {
    pub layered: Box<dyn FileSystem>,
    pub info: VolumeInfo,
    pub forced_ro: Option<String>,
    pub uid_map: SharedUidMap,
    pub kind: VolumeKind,
    pub wal: Option<Arc<WalDev>>,
    pub remote: Option<RemoteHandle>,
    pub range: Option<Arc<CachedRangeDev>>,
    pub part: Option<block::PartitionInfo>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PartitionReport {
    #[serde(flatten)]
    pub part: block::PartitionInfo,
    pub fs_type: Option<String>,
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub dirty: Option<DirtyState>,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceReport {
    pub path: String,
    pub size: u64,
    pub partitions: Vec<PartitionReport>,
}

pub struct Volume {
    pub id: String,
    pub name: String,
    pub req: MountRequest,
    pub fs: Arc<Mutex<Stack>>,
    server: LoopbackServer,
    pub mountpoint: PathBuf,
    pub info: VolumeInfo,
    pub forced_ro: Option<String>,
    pub nfs_read_only: bool,
    pub uid_map: SharedUidMap,
    pub mounted_at: i64,
    tracker: Arc<ChangeTracker>,
    sync_task: tokio::task::JoinHandle<()>,
    pub kind: VolumeKind,
    pub wal: Option<Arc<WalDev>>,
    pub remote: Option<RemoteHandle>,
    pub range: Option<Arc<CachedRangeDev>>,
    pub share: Mutex<Option<(crate::lan::LanServer, String)>>,
    pub portal: Mutex<Option<(crate::lan::LanServer, String, String)>>,
    /// Persistent Shadow session of this volume.
    pub session_key: Option<String>,
}

pub struct Runtime {
    rt: tokio::runtime::Runtime,
    support: RwLock<PathBuf>,
    vols: Mutex<BTreeMap<String, Arc<Volume>>>,
}

static RUNTIME: OnceCell<Runtime> = OnceCell::new();

pub fn default_support_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/Application Support/LumenDisk")
}

pub fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| Runtime {
        rt: tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().thread_name("lumendisk-nfs").build().expect("tokio"),
        support: RwLock::new(default_support_dir()),
        vols: Mutex::new(BTreeMap::new()),
    })
}

pub fn lock_stack(fs: &Arc<Mutex<Stack>>) -> std::sync::MutexGuard<'_, Stack> {
    fs.lock().unwrap_or_else(|e| e.into_inner())
}

fn fs_type_from_str(s: &str) -> Result<FsType, String> {
    Ok(match s.to_ascii_uppercase().as_str() {
        "EXT2" => FsType::Ext2,
        "EXT3" => FsType::Ext3,
        "EXT4" => FsType::Ext4,
        "FAT12" => FsType::Fat12,
        "FAT16" => FsType::Fat16,
        "FAT32" | "FAT" | "VFAT" => FsType::Fat32,
        "EXFAT" => FsType::ExFat,
        "NTFS" => FsType::Ntfs,
        o => return Err(format!("unknown filesystem '{o}'")),
    })
}

/// Open the device and select the requested partition (0 = whole device).
pub fn open_partition(path: &str, partition: u32, read_only: bool, fd: Option<i32>) -> Result<(Arc<dyn BlockDev>, Option<block::PartitionInfo>), String> {
    let dev = block::open_source(path, read_only, fd).map_err(|e| format!("{path}: {e}"))?;
    if partition == 0 {
        return Ok((dev, None));
    }
    let parts = block::parse_partitions(dev.as_ref()).map_err(|e| e.to_string())?;
    let p = parts.into_iter().find(|p| p.index == partition).ok_or_else(|| format!("partition {partition} not found"))?;
    let pd = PartitionDev::new(dev, p.start, p.size).map_err(|e| e.to_string())?;
    Ok((Arc::new(pd), Some(p)))
}

impl Runtime {
    pub fn set_support_dir(&self, p: &Path) {
        let _ = std::fs::create_dir_all(p.join("GhostVault"));
        let _ = std::fs::create_dir_all(p.join("Shadow"));
        *self.support.write().unwrap() = p.to_path_buf();
    }

    pub fn support(&self) -> PathBuf {
        self.support.read().unwrap().clone()
    }

    pub fn inspect_source(&self, path: &str, fd: Option<i32>) -> Result<SourceReport, String> {
        let dev = block::open_source(path, true, fd).map_err(|e| format!("{path}: {e}"))?;
        self.inspect_dev(path, dev)
    }

    pub fn inspect_dev(&self, path: &str, dev: Arc<dyn BlockDev>) -> Result<SourceReport, String> {
        let mut parts = block::parse_partitions(dev.as_ref()).map_err(|e| e.to_string())?;
        if parts.is_empty() {
            parts.push(block::PartitionInfo {
                index: 0,
                start: 0,
                size: dev.size(),
                scheme: "NONE".into(),
                type_name: "Volume".into(),
                type_id: String::new(),
                name: String::new(),
            });
        }
        let mut out = vec![];
        for p in parts {
            let pd: Arc<dyn BlockDev> = if p.index == 0 { dev.clone() } else { Arc::new(PartitionDev::new(dev.clone(), p.start, p.size).map_err(|e| e.to_string())?) };
            let mut r = PartitionReport { part: p, fs_type: None, label: None, uuid: None, dirty: None, total_bytes: None, free_bytes: None };
            if let Some(t) = probe(pd.as_ref()) {
                r.fs_type = Some(t.label().to_string());
                let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open_fs(pd.clone(), true, MountDefaults::default())));
                if let Ok(Ok(mut fs)) = opened {
                    let vi = fs.volume_info();
                    r.label = Some(vi.label);
                    r.uuid = Some(vi.uuid);
                    r.dirty = Some(fs.dirty_state());
                    if let Ok(st) = fs.statfs() {
                        r.total_bytes = Some(st.total_bytes);
                        r.free_bytes = Some(st.free_bytes);
                    }
                }
            }
            out.push(r);
        }
        Ok(SourceReport { path: path.into(), size: dev.size(), partitions: out })
    }

    pub fn mount(&self, req: MountRequest) -> Result<VolumeStatus, String> {
        let p = self.prepare_local(&req)?;
        self.publish(req, p)
    }

    pub fn wal_key(dev: &Arc<dyn BlockDev>, path: &str, partition: u32) -> String {
        let probe = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open_fs(dev.clone(), true, MountDefaults::default())));
        match probe {
            Ok(Ok(fs)) => {
                let vi = fs.volume_info();
                if !vi.uuid.is_empty() {
                    return format!("{}-{}", vi.fs_type.label(), vi.uuid);
                }
            }
            _ => {}
        }
        use sha2::Digest;
        let d = sha2::Sha256::digest(format!("{path}#{partition}").as_bytes());
        d[..10].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Replay a WAL left behind by a crash (works even for read-only mounts / repairs).
    fn replay_pending_wal(&self, path: &str, partition: u32, fd: Option<i32>) -> Result<u64, String> {
        let (dev, _) = open_partition(path, partition, true, fd)?;
        let key = Self::wal_key(&dev, path, partition);
        let wp = crate::wal::wal_path(&self.support(), &key);
        if !wp.exists() {
            return Ok(0);
        }
        drop(dev);
        let (rw, _) = open_partition(path, partition, false, fd)?;
        let w = WalDev::open(rw, &wp, &key).map_err(|e| format!("journal: {e}"))?;
        let n = w.stats().replayed_at_mount;
        w.flush_all().map_err(|e| e.to_string())?;
        drop(w);
        Ok(n)
    }

    pub fn prepare_local(&self, req: &MountRequest) -> Result<Prepared, String> {
        let want_ro = req.mode == MountMode::ReadOnly;
        if want_ro {
            // Never leave a crashed journal unapplied, even when mounting read-only.
            let _ = self.replay_pending_wal(&req.path, req.partition, req.fd);
        }
        let (dev, part) = match open_partition(&req.path, req.partition, want_ro, req.fd) {
            Ok(x) => x,
            Err(e) if !want_ro && e.contains("ermission") => {
                return Err(crate::i18n::t(&format!("{e} — accesso in scrittura negato al dispositivo"), &format!("{e} — write access to the device was denied")));
            }
            Err(e) => return Err(e),
        };
        let defaults = MountDefaults::default();
        // Crash-safe write-ahead journal for every writable local volume.
        let mut wal = None;
        let fs_dev: Arc<dyn BlockDev> = if !want_ro && !dev.read_only() && probe(dev.as_ref()).is_some() {
            let key = Self::wal_key(&dev, &req.path, req.partition);
            let wp = crate::wal::wal_path(&self.support(), &key);
            let w = WalDev::open(dev.clone(), &wp, &key).map_err(|e| format!("journal: {e}"))?;
            wal = Some(w.clone());
            w
        } else {
            dev.clone()
        };
        let mut driver = open_fs(fs_dev, want_ro, defaults).map_err(|e| e.to_string())?;
        if let Some(w) = &wal {
            driver = Box::new(Journaled { inner: driver, wal: w.clone() });
        }
        let info = driver.volume_info();
        let mut forced_ro = driver.read_only_reason();
        if !want_ro && forced_ro.is_none() {
            if let DirtyState::Dirty(r) = driver.dirty_state() {
                forced_ro = Some(crate::i18n::t(&format!("Volume da riparare: {r}"), &format!("Volume needs repair: {r}")));
            }
        }
        if !want_ro && forced_ro.is_none() && info.read_only {
            forced_ro = Some(crate::i18n::t("il driver ha aperto il volume in sola lettura", "the driver opened the volume read-only"));
        }
        let mut layered: Box<dyn FileSystem> = if forced_ro.is_some() || want_ro { Box::new(ReadOnlyGuard { inner: driver }) } else { driver };
        let shared_map = Self::bridge(&mut layered, &info, req.uid_map.clone(), defaults);
        Ok(Prepared { layered, info, forced_ro, uid_map: shared_map, kind: VolumeKind::Local, wal, remote: None, range: None, part })
    }

    /// Feature 3 (UID/GID bridge) for POSIX filesystems.
    fn bridge(layered: &mut Box<dyn FileSystem>, info: &VolumeInfo, req: Option<UidMapRequest>, defaults: MountDefaults) -> SharedUidMap {
        let (mac_uid, mac_gid) = (defaults.uid, defaults.gid);
        let ureq = req.unwrap_or(UidMapRequest { enabled: true, linux_uid: None, linux_gid: None });
        let map = if info.fs_type.has_posix_owners() {
            let (du, dg) = match (ureq.linux_uid, ureq.linux_gid) {
                (Some(u), Some(g)) => (u, g),
                (u, g) => {
                    let (a, b) = uidmap::detect_owner(layered.as_mut());
                    (u.unwrap_or(a), g.unwrap_or(b))
                }
            };
            UidMap { enabled: ureq.enabled, mac_uid, mac_gid, linux_uid: du, linux_gid: dg }
        } else {
            UidMap { enabled: false, mac_uid, mac_gid, linux_uid: mac_uid, linux_gid: mac_gid }
        };
        let shared: SharedUidMap = Arc::new(RwLock::new(map));
        if info.fs_type.has_posix_owners() {
            let inner = std::mem::replace(layered, Box::new(NullFs));
            *layered = Box::new(PermissionBridge { inner, map: shared.clone() });
        }
        shared
    }

    /// Wrap a prepared stack in Shadow/Ghost Shield, serve it over loopback NFS and mount it.
    pub fn publish(&self, req: MountRequest, p: Prepared) -> Result<VolumeStatus, String> {
        let defaults = MountDefaults::default();
        let Prepared { layered, info, forced_ro, uid_map, kind, wal, remote, range, part } = p;
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let support = self.support();
        let mut layered = layered;
        let identity = Self::shadow_identity(kind, &req, &info, layered.as_mut());
        let mut session_key = None;
        let lower = if req.mode == MountMode::Shadow {
            match identity {
                Some(identity) => {
                    // Persistent session: restores pending changes / finishes an interrupted commit.
                    let key = shadow_store::session_key(&identity);
                    if let Some(want) = &req.expect_session {
                        if *want != key {
                            return Err(crate::i18n::t(
                                "Questo disco non corrisponde alla sessione Shadow salvata.",
                                "This disk doesn't match the saved Shadow session.",
                            ));
                        }
                    }
                    if self.vols.lock().unwrap().values().any(|v| v.session_key.as_deref() == Some(key.as_str())) {
                        return Err(crate::i18n::t("Questo volume è già montato.", "This volume is already mounted."));
                    }
                    let dir = shadow_store::sessions_root(&support).join(&key);
                    let mut ov = ShadowOverlay::open_session(layered, dir, identity)?;
                    if ov.commit_pending() {
                        if let Err(e) = ov.commit() {
                            eprintln!("lumendisk: resuming the interrupted Shadow commit: {e}");
                        }
                    }
                    session_key = Some(key);
                    Lower::Shadow(ov)
                }
                None => {
                    let stage = shadow_store::sessions_root(&support).join(format!(".mem-{id}.stage"));
                    Lower::Shadow(ShadowOverlay::new(layered, stage).map_err(|e| e.to_string())?)
                }
            }
        } else {
            if req.mode == MountMode::ReadWrite && forced_ro.is_none() {
                if let Some(identity) = &identity {
                    let dir = shadow_store::sessions_root(&support).join(shadow_store::session_key(identity));
                    if let Some(m) = Manifest::load(&dir) {
                        return Err(Self::pending_session_message(&m));
                    }
                }
            }
            Lower::Direct(layered)
        };
        let vault_key = if info.uuid.is_empty() { id.clone() } else {
            use sha2::Digest;
            let d = sha2::Sha256::digest(info.uuid.as_bytes());
            d[..12].iter().map(|b| format!("{b:02x}")).collect()
        };
        let vault = support.join("GhostVault").join(format!("{vault_key}.db"));
        let mut stack = GhostShield::new(lower, req.ghost_shield, req.ghost_rules.unwrap_or_default(), defaults, Some(vault));
        if let Some(icon) = req.volume_icon.as_ref().and_then(|p| std::fs::read(p).ok()) {
            stack.install_volume_icon(icon);
        }
        let persistent_shadow = matches!(&stack.inner, Lower::Shadow(s) if s.is_persistent());
        let fs = Arc::new(Mutex::new(stack));

        let want_ro = req.mode == MountMode::ReadOnly;
        let nfs_ro = want_ro || (forced_ro.is_some() && req.mode != MountMode::Shadow);
        let fsid = u64::from_str_radix(&id[..12], 16).unwrap_or(0x1d);
        let tracker = Arc::new(ChangeTracker::default());
        let group = if nfs_ro {
            None
        } else if persistent_shadow {
            Some(shadow_group_commit(&fs))
        } else if req.mode != MountMode::Shadow {
            wal.clone().map(|w| GroupCommit::new(Box::new(move || w.make_durable().map_err(|e| format!("journal: {e}")))))
        } else {
            None // in-memory Shadow overlay: nothing is meant to survive
        };
        let bridge = NfsBridge::new(fs.clone(), nfs_ro, fsid, tracker.clone()).with_durability(group.clone());
        let server = nfs_loopback::start_server(&self.rt, bridge).map_err(|e| format!("NFS server: {e}"))?;

        let root = req.mount_root.as_ref().map(PathBuf::from).unwrap_or_else(nfs_loopback::default_mount_root);
        let name = req
            .name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .or_else(|| (!info.label.trim().is_empty()).then(|| info.label.trim().to_string()))
            .or_else(|| part.as_ref().map(|p| if p.name.is_empty() { format!("{} {}", info.fs_type.label(), p.index) } else { p.name.clone() }))
            .unwrap_or_else(|| format!("{} Volume", info.fs_type.label()));
        let mountpoint = match nfs_loopback::pick_mountpoint(&root, &name) {
            Ok(m) => m,
            Err(e) => {
                server.stop();
                return Err(format!("mountpoint: {e}"));
            }
        };
        if let Err(e) = nfs_loopback::mount_nfs(server.port, &server.export, &mountpoint, nfs_ro) {
            server.stop();
            let _ = std::fs::remove_dir(&mountpoint);
            return Err(e);
        }
        let fs2 = fs.clone();
        let sync_task = self.rt.spawn(async move {
            let mut n: u32 = 0;
            loop {
                tokio::time::sleep(crate::vfs::shadow::FLUSH_INTERVAL).await;
                n = n.wrapping_add(1);
                let fs3 = fs2.clone();
                if n % 30 == 0 {
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = lock_stack(&fs3).sync();
                    })
                    .await;
                    continue;
                }
                // Group commit of what was written UNSTABLE meanwhile (and not COMMITted yet),
                // sharing rounds with the stable requests: the cache flushes run without the
                // volume lock, so NFS requests keep flowing.
                if let Some(g) = &group {
                    let _ = g.wait().await;
                }
                let _ = tokio::task::spawn_blocking(move || {
                    if let Lower::Shadow(s) = &mut lock_stack(&fs3).inner {
                        s.refresh_manifest();
                    }
                })
                .await;
            }
        });
        let display_name = mountpoint.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or(name);
        let vol = Arc::new(Volume {
            id: id.clone(),
            name: display_name,
            req,
            fs,
            server,
            mountpoint,
            info,
            forced_ro,
            nfs_read_only: nfs_ro,
            uid_map,
            mounted_at: Timestamp::now().secs,
            tracker,
            sync_task,
            kind,
            wal,
            remote,
            range,
            share: Mutex::new(None),
            portal: Mutex::new(None),
            session_key,
        });
        self.vols.lock().unwrap().insert(id, vol.clone());
        Ok(self.status(&vol))
    }

    // ───────────── Phase 2: remote volumes ─────────────

    fn remote_backend(&self, r: &RemoteMountRequest) -> Result<(Arc<dyn RemoteBackend>, VolumeKind, String), String> {
        match r.kind.as_str() {
            "sftp" => {
                let mut cfg = crate::remote::sftp::SftpConfig::from_url(&r.url).map_err(|e| e.to_string())?;
                cfg.password = r.password.clone().filter(|p| !p.is_empty());
                cfg.identity_file = r.identity_file.clone().filter(|p| !p.is_empty());
                if r.ssh_config_file.is_some() {
                    cfg.ssh_config_file = r.ssh_config_file.clone();
                }
                if r.known_hosts_file.is_some() {
                    cfg.known_hosts_file = r.known_hosts_file.clone();
                }
                cfg.extra_ssh_options.extend(r.extra_ssh_options.iter().cloned());
                let host = cfg.host.clone();
                let b = crate::remote::sftp::SftpBackend::connect(cfg).map_err(|e| format!("SFTP: {e}"))?;
                Ok((Arc::new(b), VolumeKind::Sftp, host))
            }
            "webdav" | "lan" => {
                let lan = r.kind == "lan";
                let bearer = if lan {
                    let code = r.pairing_code.clone().filter(|c| !c.trim().is_empty()).ok_or_else(|| crate::i18n::t("codice di abbinamento mancante", "pairing code missing"))?;
                    Some(crate::lan::token_from_pairing_code(&code))
                } else {
                    None
                };
                let cfg = crate::remote::webdav::WebDavConfig {
                    url: r.url.clone(),
                    username: r.username.clone(),
                    password: r.password.clone(),
                    bearer,
                    fingerprint: r.fingerprint.clone(),
                };
                let b = crate::remote::webdav::WebDavBackend::connect(cfg).map_err(|e| format!("WebDAV: {e}"))?;
                let host = r.url.split("://").nth(1).unwrap_or(&r.url).split(['/', ':']).next().unwrap_or("Server").to_string();
                Ok((Arc::new(b), if lan { VolumeKind::Lan } else { VolumeKind::Webdav }, host))
            }
            o => Err(crate::i18n::t(&format!("tipo di server sconosciuto: {o}"), &format!("unknown server type: {o}"))),
        }
    }

    fn remote_cache_dir(&self, url: &str) -> PathBuf {
        use sha2::Digest;
        let d = sha2::Sha256::digest(url.as_bytes());
        let key: String = d[..10].iter().map(|b| format!("{b:02x}")).collect();
        self.support().join("RemoteCache").join(key)
    }

    pub fn probe_remote(&self, r: &RemoteMountRequest) -> Result<(u32, u32), String> {
        let (b, _, _) = self.remote_backend(r)?;
        let lat = b.ping().map_err(|e| e.to_string())?;
        let n = b.list("/").map_err(|e| e.to_string())?.len() as u32;
        Ok((lat.as_millis() as u32, n))
    }

    pub fn mount_remote(&self, r: RemoteMountRequest) -> Result<VolumeStatus, String> {
        let (backend, kind, host) = self.remote_backend(&r)?;
        let defaults = MountDefaults::default();
        let label = r.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
            let tail = r.url.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            if tail.is_empty() || tail.contains(':') || r.url.trim_end_matches('/').matches('/').count() <= 2 { host.clone() } else { format!("{tail} ({host})") }
        });
        let cfg = RemoteConfig {
            meta_timeout: std::time::Duration::from_millis(400),
            data_timeout: std::time::Duration::from_secs(10),
            attr_ttl: std::time::Duration::from_secs(3),
            ping_interval: std::time::Duration::from_secs(2),
            cache_dir: self.remote_cache_dir(&r.url),
            uid: defaults.uid,
            gid: defaults.gid,
            label: label.clone(),
            read_only: r.mode == MountMode::ReadOnly,
            queue_dir: Some(self.support().join("OfflineQueue").join(self.remote_cache_dir(&r.url).file_name().unwrap())),
        };
        let vfs = RemoteVfs::new(backend, cfg).map_err(|e| e.to_string())?;
        let handle = vfs.handle();
        let info = VolumeInfo { fs_type: FsType::Ext4, label: label.clone(), uuid: r.url.clone(), read_only: r.mode == MountMode::ReadOnly };
        let layered: Box<dyn FileSystem> = if r.mode == MountMode::ReadOnly { Box::new(ReadOnlyGuard { inner: Box::new(vfs) }) } else { Box::new(vfs) };
        let uid_map = Arc::new(RwLock::new(UidMap { enabled: false, mac_uid: defaults.uid, mac_gid: defaults.gid, linux_uid: defaults.uid, linux_gid: defaults.gid }));
        let req = MountRequest {
            path: r.url.clone(),
            partition: 0,
            mode: r.mode,
            ghost_shield: r.ghost_shield,
            ghost_rules: None,
            uid_map: None,
            mount_root: r.mount_root.clone(),
            name: Some(label),
            fd: None,
            volume_icon: r.volume_icon.clone(),
            media_uuid: None,
            expect_session: None,
        };
        let p = Prepared { layered, info, forced_ro: None, uid_map, kind, wal: None, remote: Some(handle), range: None, part: None };
        self.publish(req, p)
    }

    pub fn remote_sync_now(&self, id: &str) -> Result<(u64, u64), String> {
        let v = self.get(id)?;
        let h = v.remote.clone().ok_or_else(|| crate::i18n::t("non è un volume remoto", "not a remote volume"))?;
        // make sure the stack pushed its latest state down first
        let _ = lock_stack(&v.fs).sync();
        let n = h.sync_now(std::time::Duration::from_secs(20));
        Ok((n, h.pending()))
    }

    // ───────────── Phase 2: remote images (HTTP Range / SFTP) ─────────────

    fn range_source(&self, r: &ImageUrlRequest) -> Result<Box<dyn block::RangeSource>, String> {
        if r.url.starts_with("sftp://") {
            let mut cfg = crate::remote::sftp::SftpConfig::from_url(&r.url).map_err(|e| e.to_string())?;
            cfg.password = r.password.clone().filter(|p| !p.is_empty());
            cfg.identity_file = r.identity_file.clone().filter(|p| !p.is_empty());
            Ok(Box::new(crate::remote::sftp::SftpRangeSource::open(cfg).map_err(|e| format!("SFTP: {e}"))?))
        } else {
            Ok(Box::new(crate::remote::http::HttpRangeSource::open(&r.url, r.username.as_deref(), r.password.as_deref()).map_err(|e| e.to_string())?))
        }
    }

    pub fn inspect_image_url(&self, r: &ImageUrlRequest) -> Result<(SourceReport, u64), String> {
        let src = self.range_source(r)?;
        let dev = Arc::new(CachedRangeDev::new(src, 64));
        let rep = self.inspect_dev(&r.url, dev.clone())?;
        Ok((rep, dev.stats().fetched_bytes))
    }

    pub fn mount_image_url(&self, mut r: ImageUrlRequest) -> Result<VolumeStatus, String> {
        if r.mode == MountMode::ReadWrite {
            r.mode = MountMode::Shadow; // the remote file is never modified
        }
        let src = self.range_source(&r)?;
        let range = Arc::new(CachedRangeDev::new(src, 256));
        let base: Arc<dyn BlockDev> = range.clone();
        let (dev, part): (Arc<dyn BlockDev>, Option<block::PartitionInfo>) = if r.partition == 0 {
            (base, None)
        } else {
            let parts = block::parse_partitions(base.as_ref()).map_err(|e| e.to_string())?;
            let p = parts.into_iter().find(|p| p.index == r.partition).ok_or_else(|| crate::i18n::t(&format!("partizione {} non trovata", r.partition), &format!("partition {} not found", r.partition)))?;
            (Arc::new(PartitionDev::new(base, p.start, p.size).map_err(|e| e.to_string())?), Some(p))
        };
        let defaults = MountDefaults::default();
        let driver = open_fs(dev, true, defaults).map_err(|e| e.to_string())?;
        let info = driver.volume_info();
        let mut layered: Box<dyn FileSystem> = Box::new(ReadOnlyGuard { inner: driver });
        let uid_map = Self::bridge(&mut layered, &info, None, defaults);
        let req = MountRequest {
            path: r.url.clone(),
            partition: r.partition,
            mode: r.mode,
            ghost_shield: r.ghost_shield,
            ghost_rules: None,
            uid_map: None,
            mount_root: r.mount_root.clone(),
            name: r.name.clone(),
            fd: None,
            volume_icon: r.volume_icon.clone(),
            media_uuid: None,
            expect_session: None,
        };
        let p = Prepared {
            layered,
            info,
            forced_ro: Some(crate::i18n::t("immagine remota: le modifiche restano in Shadow Mode sul Mac", "remote image: changes stay in Shadow Mode on this Mac")),
            uid_map,
            kind: VolumeKind::ImageUrl,
            wal: None,
            remote: None,
            range: Some(range),
            part,
        };
        self.publish(req, p)
    }

    // ───────────── Phase 2: LAN share & Wi-Fi portal ─────────────

    pub fn share_start(&self, id: &str) -> Result<ShareStatus, String> {
        let v = self.get(id)?;
        {
            let g = v.share.lock().unwrap();
            if g.is_some() {
                drop(g);
                return self.share_status(&v).ok_or_else(|| crate::i18n::t("condivisione non attiva", "sharing is not active"));
            }
        }
        let code = crate::lan::new_pairing_code();
        let token = crate::lan::token_from_pairing_code(&code);
        let srv = crate::lan::LanServer::start(v.fs.clone(), crate::lan::LanMode::Share { token }, &v.name, v.nfs_read_only).map_err(|e| e.to_string())?;
        *v.share.lock().unwrap() = Some((srv, code));
        self.share_status(&v).ok_or_else(|| crate::i18n::t("condivisione non attiva", "sharing is not active"))
    }

    pub fn share_stop(&self, id: &str) -> Result<(), String> {
        let v = self.get(id)?;
        if let Some((srv, _)) = v.share.lock().unwrap().take() {
            srv.stop();
        }
        Ok(())
    }

    fn share_status(&self, v: &Volume) -> Option<ShareStatus> {
        let g = v.share.lock().unwrap();
        let (srv, code) = g.as_ref()?;
        let st = srv.stats();
        Some(ShareStatus {
            scheme: "https",
            fingerprint: srv.fingerprint(),
            port: srv.port(),
            addresses: crate::lan::lan_addresses(),
            pairing_code: code.clone(),
            service_name: v.name.clone(),
            clients: st.clients,
            bytes_sent: st.bytes_sent,
            bytes_received: st.bytes_received,
        })
    }

    pub fn portal_start(&self, id: &str) -> Result<PortalStatus, String> {
        let v = self.get(id)?;
        if v.portal.lock().unwrap().is_none() {
            let pin = crate::lan::new_pin();
            let srv = crate::lan::LanServer::start(v.fs.clone(), crate::lan::LanMode::Portal { pin: pin.clone() }, &v.name, v.nfs_read_only).map_err(|e| e.to_string())?;
            let host = crate::lan::lan_addresses().into_iter().next().ok_or_else(|| crate::i18n::t("nessuna rete Wi-Fi/Ethernet attiva", "no active Wi-Fi/Ethernet network"))?;
            let url = format!("https://{host}:{}/", srv.port());
            *v.portal.lock().unwrap() = Some((srv, pin, url));
        }
        self.portal_status(&v).ok_or_else(|| crate::i18n::t("portale non attivo", "portal is not active"))
    }

    /// Explicit, insecure opt-in: also serve the portal over unencrypted HTTP (or stop doing so).
    pub fn portal_set_plain_http(&self, id: &str, enabled: bool) -> Result<PortalStatus, String> {
        let v = self.get(id)?;
        {
            let g = v.portal.lock().unwrap();
            let (srv, _, _) = g.as_ref().ok_or_else(|| crate::i18n::t("portale non attivo", "portal is not active"))?;
            srv.set_plain_http(enabled).map_err(|e| e.to_string())?;
        }
        self.portal_status(&v).ok_or_else(|| crate::i18n::t("portale non attivo", "portal is not active"))
    }

    pub fn portal_stop(&self, id: &str) -> Result<(), String> {
        let v = self.get(id)?;
        if let Some((srv, _, _)) = v.portal.lock().unwrap().take() {
            srv.stop();
        }
        Ok(())
    }

    fn portal_status(&self, v: &Volume) -> Option<PortalStatus> {
        let g = v.portal.lock().unwrap();
        let (srv, pin, url) = g.as_ref()?;
        let st = srv.stats();
        let host = url.trim_start_matches("https://").split(':').next().unwrap_or("").to_string();
        Some(PortalStatus {
            url: url.clone(),
            http_url: srv.http_port().map(|p| format!("http://{host}:{p}/")),
            fingerprint: srv.fingerprint(),
            pin: pin.clone(),
            port: srv.port(),
            uploads: st.uploads,
            downloads: st.downloads,
        })
    }

    pub fn status(&self, v: &Volume) -> VolumeStatus {
        let mut g = lock_stack(&v.fs);
        let st = g.statfs().unwrap_or_default();
        let dirty = g.dirty_state();
        let shadow = match &mut g.inner {
            Lower::Shadow(s) => Some(s.summary()),
            Lower::Direct(_) => None,
        };
        let ghost = GhostStatus { enabled: g.enabled, blocked_count: g.blocked_total, vault_bytes: g.vault_bytes(), rules: g.rules };
        VolumeStatus {
            id: v.id.clone(),
            name: v.name.clone(),
            source_path: v.req.path.clone(),
            partition: v.req.partition,
            fs_type: v.info.fs_type.label().into(),
            label: v.info.label.clone(),
            uuid: v.info.uuid.clone(),
            mode: v.req.mode,
            mountpoint: v.mountpoint.to_string_lossy().into_owned(),
            port: v.server.port,
            // After an internal error the server answers EIO to everything (nfs_loopback).
            read_only: v.nfs_read_only || v.tracker.failure().is_some(),
            forced_read_only_reason: v.tracker.failure().map(|w| format!("internal error, volume disabled until unmounted: {w}")).or_else(|| v.forced_ro.clone()),
            dirty,
            total_bytes: st.total_bytes,
            free_bytes: st.free_bytes,
            ghost,
            shadow,
            uid_map: *v.uid_map.read().unwrap(),
            mounted_at: v.mounted_at,
            kind: v.kind,
            remote: match (&v.remote, &v.range) {
                (Some(h), _) => Some(h.health()),
                (None, Some(r)) => {
                    let s = r.stats();
                    Some(RemoteHealth {
                        url: v.req.path.clone(),
                        online: true,
                        latency_ms: None,
                        pending_ops: 0,
                        pending_files: vec![],
                        last_sync: None,
                        last_error: None,
                        cache_bytes: s.cached_bytes,
                        fetched_bytes: s.fetched_bytes,
                        remote_size: Some(s.size),
                    })
                }
                _ => None,
            },
            share: self.share_status(v),
            portal: self.portal_status(v),
            journal: v.wal.as_ref().map(|w| w.stats()).unwrap_or_default(),
            device: v.req.path.strip_prefix("/dev/").map(|d| d.trim_start_matches('r').to_string()),
        }
    }

    pub fn get(&self, id: &str) -> Result<Arc<Volume>, String> {
        self.vols.lock().unwrap().get(id).cloned().ok_or_else(|| crate::i18n::t(&format!("volume '{id}' non montato"), &format!("volume '{id}' is not mounted")))
    }

    pub fn list(&self) -> Vec<VolumeStatus> {
        let vols: Vec<Arc<Volume>> = self.vols.lock().unwrap().values().cloned().collect();
        // Drop volumes whose mount disappeared behind our back (e.g. `umount` from Terminal).
        let mut out = vec![];
        for v in vols {
            if !nfs_loopback::is_mountpoint(&v.mountpoint) {
                let _ = self.teardown(&v);
                continue;
            }
            out.push(self.status(&v));
        }
        out
    }

    fn teardown(&self, v: &Arc<Volume>) -> Result<(), String> {
        v.sync_task.abort();
        v.server.stop();
        if let Some((s, _)) = v.share.lock().unwrap().take() {
            s.stop();
        }
        if let Some((s, _, _)) = v.portal.lock().unwrap().take() {
            s.stop();
        }
        let r = lock_stack(&v.fs).sync().map_err(|e| e.to_string());
        if let Some(w) = &v.wal {
            let _ = w.flush_all();
        }
        let _ = std::fs::remove_dir(&v.mountpoint);
        self.vols.lock().unwrap().remove(&v.id);
        r
    }

    pub fn unmount(&self, id: &str, force: bool) -> Result<(), String> {
        let v = self.get(id)?;
        {
            let mut g = lock_stack(&v.fs);
            if let Lower::Shadow(s) = &mut g.inner {
                if s.commit_pending() && !force {
                    return Err(crate::i18n::t("L'applicazione delle modifiche Shadow si è interrotta: riprova (Commit) o abbandona il resto (Rollback) prima di espellere.", "Applying the Shadow changes was interrupted: retry (Commit) or abandon the rest (Rollback) before ejecting."));
                }
                if s.has_changes() && !s.changes().is_empty() && !force {
                    return Err(crate::i18n::t("Ci sono modifiche Shadow non applicate: applicale (Commit) o scartale (Rollback) prima di espellere.", "There are pending Shadow changes: apply (Commit) or discard (Rollback) them before ejecting."));
                }
            }
            g.sync().map_err(|e| e.to_string())?;
        }
        if let Some(h) = &v.remote {
            if h.pending() > 0 {
                // Try briefly; anything left stays in the persistent offline queue and is
                // uploaded automatically the next time this remote is mounted.
                h.sync_now(std::time::Duration::from_secs(5));
                h.persist_queue().map_err(|e| crate::i18n::t(&format!("salvataggio della coda offline: {e}"), &format!("saving the offline queue: {e}")))?;
            }
        }
        nfs_loopback::unmount(&v.mountpoint, force).map_err(|e| crate::i18n::t(&format!("Il volume è in uso: {e}"), &format!("The volume is in use: {e}")))?;
        self.teardown(&v)
    }

    pub fn unmount_all(&self) -> (u32, Vec<String>) {
        let ids: Vec<String> = self.vols.lock().unwrap().keys().cloned().collect();
        let mut ok = 0;
        let mut failed = vec![];
        for id in ids {
            match self.unmount(&id, false) {
                Ok(()) => ok += 1,
                Err(e) => failed.push(format!("{id}: {e}")),
            }
        }
        (ok, failed)
    }

    pub fn shadow_changes(&self, id: &str) -> Result<Vec<ShadowChange>, String> {
        let v = self.get(id)?;
        let mut g = lock_stack(&v.fs);
        match &mut g.inner {
            Lower::Shadow(s) => Ok(s.changes()),
            _ => Err(crate::i18n::t("il volume non è in Shadow Mode", "the volume is not in Shadow Mode")),
        }
    }

    pub fn shadow_commit(&self, id: &str) -> Result<u64, String> {
        let v = self.get(id)?;
        let mut g = lock_stack(&v.fs);
        match &mut g.inner {
            Lower::Shadow(s) => {
                let r = s.commit().map_err(|e| e.to_string());
                v.tracker.invalidate_all();
                r
            }
            _ => Err(crate::i18n::t("il volume non è in Shadow Mode", "the volume is not in Shadow Mode")),
        }
    }

    pub fn shadow_rollback(&self, id: &str) -> Result<u64, String> {
        let v = self.get(id)?;
        let mut g = lock_stack(&v.fs);
        match &mut g.inner {
            Lower::Shadow(s) => {
                let n = s.rollback();
                v.tracker.invalidate_all();
                Ok(n)
            }
            _ => Err(crate::i18n::t("il volume non è in Shadow Mode", "the volume is not in Shadow Mode")),
        }
    }

    pub fn ghost_entries(&self, id: &str) -> Result<Vec<GhostEntry>, String> {
        let v = self.get(id)?;
        let g = lock_stack(&v.fs);
        Ok(g.entries())
    }

    pub fn ghost_set_rules(&self, id: &str, rules: GhostRules) -> Result<(), String> {
        let v = self.get(id)?;
        lock_stack(&v.fs).rules = rules;
        Ok(())
    }

    pub fn ghost_set_enabled(&self, id: &str, on: bool) -> Result<(), String> {
        let v = self.get(id)?;
        lock_stack(&v.fs).enabled = on;
        Ok(())
    }

    pub fn ghost_purge(&self, id: &str) -> Result<(), String> {
        let v = self.get(id)?;
        let mut g = lock_stack(&v.fs);
        g.purge();
        g.save_vault()
    }

    pub fn set_uid_map(&self, id: &str, enabled: bool, linux_uid: Option<u32>, linux_gid: Option<u32>) -> Result<UidMap, String> {
        let v = self.get(id)?;
        if !v.info.fs_type.has_posix_owners() {
            return Err("la mappatura UID/GID si applica solo ai volumi ext2/3/4".into());
        }
        let mut m = v.uid_map.write().unwrap();
        m.enabled = enabled;
        if let Some(u) = linux_uid {
            m.linux_uid = u;
        }
        if let Some(g) = linux_gid {
            m.linux_gid = g;
        }
        Ok(*m)
    }

    pub fn repair_source(&self, path: &str, partition: u32, fd: Option<i32>) -> Result<String, String> {
        if self.vols.lock().unwrap().values().any(|v| v.req.path == path && v.req.partition == partition) {
            return Err(crate::i18n::t("il volume è montato: usa la riparazione dal pannello del volume", "the volume is mounted: repair it from the volume panel"));
        }
        let replayed = self.replay_pending_wal(path, partition, fd).unwrap_or(0);
        let (dev, _) = open_partition(path, partition, false, fd)?;
        let mut fs = open_fs(dev, false, MountDefaults::default()).map_err(|e| e.to_string())?;
        // A repair writes to the disk: it would invalidate saved Shadow changes.
        let req = MountRequest {
            path: path.into(),
            partition,
            mode: MountMode::ReadWrite,
            ghost_shield: false,
            ghost_rules: None,
            uid_map: None,
            mount_root: None,
            name: None,
            fd: None,
            volume_icon: None,
            media_uuid: None,
            expect_session: None,
        };
        let info = fs.volume_info();
        if let Some(id) = Self::shadow_identity(VolumeKind::Local, &req, &info, fs.as_mut()) {
            let dir = shadow_store::sessions_root(&self.support()).join(shadow_store::session_key(&id));
            if let Some(m) = Manifest::load(&dir) {
                return Err(Self::pending_session_message(&m));
            }
        }
        if replayed > 0 {
            eprintln!("lumendisk: replayed {replayed} journal record(s) before repair");
        }
        let report = fs.repair().map_err(|e| e.to_string())?;
        fs.sync().map_err(|e| e.to_string())?;
        Ok(report)
    }

    /// Repair a mounted (forced read-only) volume and remount it read/write.
    pub fn repair_volume(&self, id: &str) -> Result<(String, VolumeStatus), String> {
        let v = self.get(id)?;
        let mut req = v.req.clone();
        req.name = Some(v.name.clone());
        req.mount_root = v.mountpoint.parent().map(|p| p.to_string_lossy().into_owned());
        if req.fd.is_some() {
            return Err(crate::i18n::t("riparazione di un dispositivo autorizzato: smonta e ricollega il disco", "repairing an authorised device: eject and reconnect the disk"));
        }
        self.unmount(id, false)?;
        let report = self.repair_source(&req.path, req.partition, None)?;
        if req.mode == MountMode::ReadOnly {
            req.mode = MountMode::ReadWrite;
        }
        let st = self.mount(req)?;
        Ok((report, st))
    }

    pub fn create_image(&self, path: &str, fs: &str, size_mb: u64, label: &str) -> Result<String, String> {
        let t = fs_type_from_str(fs)?;
        if size_mb < 8 || size_mb > 1024 * 1024 {
            return Err(crate::i18n::t("dimensione non valida (8 MB – 1 TB)", "invalid size (8 MB – 1 TB)"));
        }
        if Path::new(path).exists() {
            return Err(crate::i18n::t(&format!("{path} esiste già"), &format!("{path} already exists")));
        }
        let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
        f.set_len(size_mb * 1024 * 1024).map_err(|e| e.to_string())?;
        drop(f);
        let dev: Arc<dyn BlockDev> = Arc::new(block::FileDev::open(Path::new(path), false).map_err(|e| e.to_string())?);
        if let Err(e) = format(dev, t, label) {
            let _ = std::fs::remove_file(path);
            return Err(e.to_string());
        }
        Ok(path.to_string())
    }
}

impl Runtime {
    /// Identity of a volume for persistent Shadow sessions (None: remote filesystems, whose
    /// Shadow changes stay in memory).
    fn shadow_identity(kind: VolumeKind, req: &MountRequest, info: &VolumeInfo, fs: &mut dyn FileSystem) -> Option<Identity> {
        let kind = match kind {
            VolumeKind::Local => "local",
            VolumeKind::ImageUrl => "image_url",
            _ => return None,
        };
        Some(Identity {
            kind: kind.into(),
            fs_type: info.fs_type.label().into(),
            uuid: info.uuid.clone(),
            label: info.label.clone(),
            // Absolute, so that "is it connected?" works from any working directory.
            source_path: if kind == "local" { std::fs::canonicalize(&req.path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| req.path.clone()) } else { req.path.clone() },
            partition: req.partition,
            size: fs.statfs().map(|s| s.total_bytes).unwrap_or(0),
            media_uuid: req.media_uuid.clone(),
        })
    }

    fn session_display_name(m: &Manifest) -> String {
        let l = m.identity.label.trim();
        if l.is_empty() {
            format!("{} Volume", m.identity.fs_type)
        } else {
            l.to_string()
        }
    }

    fn pending_session_message(m: &Manifest) -> String {
        let name = Self::session_display_name(m);
        if m.state == "committing" {
            crate::i18n::t(
                &format!("L'applicazione delle modifiche Shadow su {name} si è interrotta ({} di {} passi già applicati): montalo in Shadow Mode per completarla, oppure abbandona il resto.", m.commit_applied, m.commit_total),
                &format!("Applying the Shadow changes to {name} was interrupted ({} of {} steps already applied): mount it in Shadow Mode to finish, or abandon the rest.", m.commit_applied, m.commit_total),
            )
        } else {
            let n = m.counts.changes;
            crate::i18n::t(
                &format!("Ci sono {n} modifiche Shadow non applicate su {name}: montalo in Shadow Mode per riprenderle, oppure scartale prima di montarlo in lettura/scrittura."),
                &format!("There are {n} unapplied Shadow changes on {name}: mount it in Shadow Mode to resume them, or discard them before mounting it read/write."),
            )
        }
    }

    /// Every saved Shadow session, mounted or not.
    pub fn shadow_sessions(&self) -> Vec<ShadowSessionInfo> {
        let support = self.support();
        let mounted: Vec<(String, String)> = self.vols.lock().unwrap().values().filter_map(|v| v.session_key.clone().map(|k| (k, v.id.clone()))).collect();
        let mut out = vec![];
        for (dir, m) in shadow_store::list_sessions(&support) {
            let key = dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let mounted_id = mounted.iter().find(|(k, _)| *k == key).map(|x| x.1.clone());
            // A live session: report its current counters.
            let (mut counts, mut state, mut applied, mut total, mut cerr) =
                (m.counts.clone(), m.state.clone(), m.commit_applied, m.commit_total, m.commit_error.clone());
            if let Some(id) = &mounted_id {
                if let Ok(v) = self.get(id) {
                    let mut g = lock_stack(&v.fs);
                    if let Lower::Shadow(s) = &mut g.inner {
                        let sm = s.summary();
                        counts = shadow_store::Counts { changes: sm.added + sm.modified + sm.deleted, added: sm.added, modified: sm.modified, deleted: sm.deleted, pending_bytes: sm.pending_bytes };
                        state = sm.state.to_string();
                        (applied, total, cerr) = (sm.commit_applied, sm.commit_total, sm.commit_error);
                    }
                }
            }
            if mounted_id.is_none() && state == "committing" {
                applied = shadow_store::checkpointed_steps(&dir).unwrap_or(applied).max(applied);
            }
            let connected = m.identity.kind == "local" && Path::new(&m.identity.source_path).exists();
            let (fingerprint_ok, fingerprint_problem) = match (&m.drift, mounted_id.is_some()) {
                (Some(d), _) => (Some(false), Some(d.clone())),
                (None, true) => (Some(true), None),
                (None, false) if connected && m.state == "pending" => match Self::check_fingerprint(&m) {
                    Some(true) => (Some(true), None),
                    Some(false) => (Some(false), Some(crate::i18n::t("Il disco è stato modificato altrove dopo l'inizio della sessione Shadow.", "The disk was modified elsewhere after the Shadow session started."))),
                    None => (None, None),
                },
                _ => (None, None),
            };
            out.push(ShadowSessionInfo {
                key,
                name: Self::session_display_name(&m),
                label: m.identity.label.clone(),
                fs_type: m.identity.fs_type.clone(),
                uuid: m.identity.uuid.clone(),
                source_path: m.identity.source_path.clone(),
                partition: m.identity.partition,
                kind: m.identity.kind.clone(),
                media_uuid: m.identity.media_uuid.clone(),
                size: m.identity.size,
                created: m.created,
                updated: m.updated,
                state,
                changes: counts.changes,
                added: counts.added,
                modified: counts.modified,
                deleted: counts.deleted,
                pending_bytes: counts.pending_bytes,
                commit_applied: applied,
                commit_total: total,
                commit_error: cerr,
                connected,
                mounted_volume_id: mounted_id,
                fingerprint_ok,
                fingerprint_problem,
            });
        }
        out
    }

    /// Compare the saved fingerprint with the disk, when the source is readable without
    /// authorisation (image files). None: can't tell.
    fn check_fingerprint(m: &Manifest) -> Option<bool> {
        let want = m.fingerprint.as_ref()?;
        let (dev, _) = open_partition(&m.identity.source_path, m.identity.partition, true, None).ok()?;
        let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| open_fs(dev, true, MountDefaults::default())));
        let mut fs = opened.ok()?.ok()?;
        let vi = fs.volume_info();
        if vi.uuid != m.identity.uuid || vi.fs_type.label() != m.identity.fs_type {
            return Some(false);
        }
        Some(fs.fingerprint().as_ref() == Some(want))
    }

    /// Delete a saved session. If its volume is mounted this is a Shadow rollback (which, while
    /// a commit is interrupted, abandons only the steps not applied yet).
    pub fn shadow_discard_session(&self, key: &str) -> Result<u64, String> {
        if key.is_empty() || key.contains('/') || key.starts_with('.') {
            return Err(crate::i18n::t("sessione non valida", "invalid session"));
        }
        let mounted = self.vols.lock().unwrap().values().find(|v| v.session_key.as_deref() == Some(key)).map(|v| v.id.clone());
        if let Some(id) = mounted {
            return self.shadow_rollback(&id);
        }
        let dir = shadow_store::sessions_root(&self.support()).join(key);
        let m = Manifest::load(&dir).ok_or_else(|| crate::i18n::t("sessione Shadow non trovata", "Shadow session not found"))?;
        shadow_store::remove_session_dir(&dir).map_err(|e| e.to_string())?;
        Ok(if m.state == "committing" { m.commit_total.saturating_sub(m.commit_applied) } else { m.counts.changes })
    }
}

/// Placeholder used while re-boxing a stack layer.
struct NullFs;
impl FileSystem for NullFs {
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
