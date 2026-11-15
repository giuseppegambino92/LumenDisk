//! Unified VFS layer.
//!
//! Every on-disk driver (ext2/3/4, FAT12/16/32, exFAT, NTFS) implements [`FileSystem`],
//! an inode-oriented, synchronous API that maps 1:1 onto NFSv3 semantics.
//!
//! Stacking order for a mounted volume (top → bottom):
//!
//! ```text
//!  NFSv3 adapter (nfs_loopback.rs)
//!    └─ GhostShield   (ghost.rs)   – virtualises .DS_Store, ._*, .Spotlight-V100, .Trashes, .fseventsd
//!        └─ ShadowOverlay (shadow.rs) – optional CoW write-back overlay with commit / rollback
//!            └─ PermissionBridge (uidmap.rs) – Linux UID/GID <-> macOS UID/GID translation
//!                └─ ReadOnlyGuard (when mounted read-only or the volume is dirty)
//!                    └─ driver (ext4_fs / fat_fs / exfat_fs / ntfs_fs)
//! ```

use std::fmt;
use std::sync::Arc;

use crate::block::BlockDev;

pub mod ext4_fs;
pub mod exfat_fs;
pub mod fat_fs;
pub mod ghost;
pub mod ntfs_fs;
pub mod shadow;
pub mod uidmap;

/// Inode / file identifier. Must be stable for the lifetime of a mount and never 0.
pub type Ino = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct Timestamp {
    pub secs: i64,
    pub nsecs: u32,
}

impl Timestamp {
    pub fn now() -> Self {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Timestamp { secs: d.as_secs() as i64, nsecs: d.subsec_nanos() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Attr {
    pub ino: Ino,
    pub kind: FileKind,
    /// Logical size in bytes (symlink: length of target).
    pub size: u64,
    /// Permission bits only (0o7777); the file type lives in `kind`.
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: Timestamp,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
    /// Bytes actually allocated on disk.
    pub used_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SetAttr {
    pub perm: Option<u16>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Truncate / extend.
    pub size: Option<u64>,
    pub atime: Option<Timestamp>,
    pub mtime: Option<Timestamp>,
}

impl SetAttr {
    pub fn is_empty(&self) -> bool {
        self.perm.is_none()
            && self.uid.is_none()
            && self.gid.is_none()
            && self.size.is_none()
            && self.atime.is_none()
            && self.mtime.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub ino: Ino,
    pub kind: FileKind,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct FsStat {
    pub block_size: u32,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FsType {
    Ext2,
    Ext3,
    Ext4,
    Fat12,
    Fat16,
    Fat32,
    ExFat,
    Ntfs,
}

impl FsType {
    pub fn label(&self) -> &'static str {
        match self {
            FsType::Ext2 => "EXT2",
            FsType::Ext3 => "EXT3",
            FsType::Ext4 => "EXT4",
            FsType::Fat12 => "FAT12",
            FsType::Fat16 => "FAT16",
            FsType::Fat32 => "FAT32",
            FsType::ExFat => "EXFAT",
            FsType::Ntfs => "NTFS",
        }
    }
    /// Filesystems that have real POSIX ownership on disk.
    pub fn has_posix_owners(&self) -> bool {
        matches!(self, FsType::Ext2 | FsType::Ext3 | FsType::Ext4)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct VolumeInfo {
    pub fs_type: FsType,
    pub label: String,
    /// Stable identifier (ext4 UUID, FAT/exFAT serial, NTFS serial) as a string.
    pub uuid: String,
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", content = "reason")]
pub enum DirtyState {
    Clean,
    /// Volume flagged dirty / hibernated / needs journal recovery. LumenDisk mounts such
    /// volumes read-only until the Journal Doctor repairs them.
    Dirty(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Exists,
    NotDir,
    IsDir,
    NotEmpty,
    NoSpace,
    ReadOnly,
    NameTooLong,
    InvalidName,
    Unsupported(String),
    Corrupt(String),
    Io(String),
    Perm,
    Invalid(String),
    Stale,
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::NotFound => write!(f, "not found"),
            FsError::Exists => write!(f, "already exists"),
            FsError::NotDir => write!(f, "not a directory"),
            FsError::IsDir => write!(f, "is a directory"),
            FsError::NotEmpty => write!(f, "directory not empty"),
            FsError::NoSpace => write!(f, "no space left on volume"),
            FsError::ReadOnly => write!(f, "read-only volume"),
            FsError::NameTooLong => write!(f, "name too long"),
            FsError::InvalidName => write!(f, "invalid name"),
            FsError::Unsupported(s) => write!(f, "unsupported: {s}"),
            FsError::Corrupt(s) => write!(f, "corrupt filesystem: {s}"),
            FsError::Io(s) => write!(f, "I/O error: {s}"),
            FsError::Perm => write!(f, "operation not permitted"),
            FsError::Invalid(s) => write!(f, "invalid argument: {s}"),
            FsError::Stale => write!(f, "stale file handle"),
        }
    }
}

impl std::error::Error for FsError {}

impl From<std::io::Error> for FsError {
    fn from(e: std::io::Error) -> Self {
        FsError::Io(e.to_string())
    }
}

pub type FsResult<T> = Result<T, FsError>;

/// The unified, inode-based filesystem interface.
///
/// Conventions every implementation must honour:
/// * `readdir` never returns `.` or `..`.
/// * `read` returns fewer bytes than requested only at EOF (empty vec at/after EOF).
/// * `write` extends the file (zero-filling holes) when writing past EOF.
/// * `rename` replaces an existing destination file, or an existing *empty* destination
///   directory (POSIX semantics); renaming onto itself is a no-op.
/// * `unlink` fails with `IsDir` for directories; `rmdir` fails with `NotDir` for files and
///   `NotEmpty` for non-empty directories.
/// * Case-insensitive filesystems (FAT, exFAT, NTFS) do case-insensitive `lookup` and keep
///   the case given at creation time.
/// * Filesystems without POSIX ownership report uid/gid passed at mount time
///   (see `MountDefaults`) and ignore uid/gid in `create`/`setattr`.
/// * Every mutating method on a volume opened read-only returns `FsError::ReadOnly`.
/// * Data must be durable on the block device after `sync()` returns.
pub trait FileSystem: Send {
    fn volume_info(&self) -> VolumeInfo;
    fn statfs(&mut self) -> FsResult<FsStat>;
    fn root_ino(&self) -> Ino;

    fn lookup(&mut self, parent: Ino, name: &str) -> FsResult<Ino>;
    fn getattr(&mut self, ino: Ino) -> FsResult<Attr>;
    fn setattr(&mut self, ino: Ino, set: &SetAttr) -> FsResult<Attr>;
    fn read(&mut self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>>;
    fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<u32>;
    fn readdir(&mut self, dir: Ino) -> FsResult<Vec<DirEntry>>;

    fn create(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino>;
    fn mkdir(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino>;
    fn symlink(&mut self, parent: Ino, name: &str, target: &str, uid: u32, gid: u32) -> FsResult<Ino>;
    fn readlink(&mut self, ino: Ino) -> FsResult<String>;
    fn unlink(&mut self, parent: Ino, name: &str) -> FsResult<()>;
    fn rmdir(&mut self, parent: Ino, name: &str) -> FsResult<()>;
    fn rename(&mut self, from_parent: Ino, from_name: &str, to_parent: Ino, to_name: &str) -> FsResult<()>;

    /// Flush all cached state to the block device.
    fn sync(&mut self) -> FsResult<()>;

    /// Journal / dirty-bit inspection (Feature 6).
    fn dirty_state(&mut self) -> DirtyState;
    /// Clear the dirty flag / reconcile allocation info. Returns a human readable report.
    fn repair(&mut self) -> FsResult<String>;

    /// Why the driver opened the volume read-only on its own (unsupported on-disk features),
    /// if it did. Default: never.
    fn read_only_reason(&self) -> Option<String> {
        None
    }
}

/// Defaults applied by drivers that have no on-disk ownership (FAT, exFAT, NTFS).
#[derive(Clone, Copy, Debug)]
pub struct MountDefaults {
    pub uid: u32,
    pub gid: u32,
    pub file_perm: u16,
    pub dir_perm: u16,
}

impl Default for MountDefaults {
    fn default() -> Self {
        // SAFETY: getuid/getgid are always safe.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        MountDefaults { uid, gid, file_perm: 0o644, dir_perm: 0o755 }
    }
}

/// Probe a block device / partition and return the filesystem type found, if any.
pub fn probe(dev: &dyn BlockDev) -> Option<FsType> {
    if let Some(t) = ext4_fs::probe(dev) {
        return Some(t);
    }
    if ntfs_fs::probe(dev) {
        return Some(FsType::Ntfs);
    }
    if exfat_fs::probe(dev) {
        return Some(FsType::ExFat);
    }
    fat_fs::probe(dev)
}

/// Open the right driver for a device.
pub fn open_fs(dev: Arc<dyn BlockDev>, read_only: bool, defaults: MountDefaults) -> FsResult<Box<dyn FileSystem>> {
    match probe(dev.as_ref()) {
        Some(FsType::Ext2 | FsType::Ext3 | FsType::Ext4) => Ok(Box::new(ext4_fs::Ext4Fs::open(dev, read_only)?)),
        Some(FsType::Ntfs) => Ok(Box::new(ntfs_fs::NtfsFs::open(dev, read_only, defaults)?)),
        Some(FsType::ExFat) => Ok(Box::new(exfat_fs::ExFatFs::open(dev, read_only, defaults)?)),
        Some(FsType::Fat12 | FsType::Fat16 | FsType::Fat32) => {
            Ok(Box::new(fat_fs::FatFs::open(dev, read_only, defaults)?))
        }
        None => Err(FsError::Unsupported("no supported filesystem found".into())),
    }
}

/// Format a device with the requested filesystem (used by the "Crea Disco Virtuale Test" wizard).
pub fn format(dev: Arc<dyn BlockDev>, fs: FsType, label: &str) -> FsResult<()> {
    match fs {
        FsType::Ext2 | FsType::Ext3 | FsType::Ext4 => ext4_fs::format(dev, fs, label),
        FsType::Ntfs => ntfs_fs::format(dev, label),
        FsType::ExFat => exfat_fs::format(dev, label),
        FsType::Fat12 | FsType::Fat16 | FsType::Fat32 => fat_fs::format(dev, fs, label),
    }
}

/// Wrapper that rejects every mutation. Used for read-only mounts and dirty volumes.
pub struct ReadOnlyGuard {
    pub inner: Box<dyn FileSystem>,
}

impl FileSystem for ReadOnlyGuard {
    fn volume_info(&self) -> VolumeInfo {
        let mut v = self.inner.volume_info();
        v.read_only = true;
        v
    }
    fn statfs(&mut self) -> FsResult<FsStat> {
        self.inner.statfs()
    }
    fn root_ino(&self) -> Ino {
        self.inner.root_ino()
    }
    fn lookup(&mut self, parent: Ino, name: &str) -> FsResult<Ino> {
        self.inner.lookup(parent, name)
    }
    fn getattr(&mut self, ino: Ino) -> FsResult<Attr> {
        self.inner.getattr(ino)
    }
    fn setattr(&mut self, _: Ino, _: &SetAttr) -> FsResult<Attr> {
        Err(FsError::ReadOnly)
    }
    fn read(&mut self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>> {
        self.inner.read(ino, offset, len)
    }
    fn write(&mut self, _: Ino, _: u64, _: &[u8]) -> FsResult<u32> {
        Err(FsError::ReadOnly)
    }
    fn readdir(&mut self, dir: Ino) -> FsResult<Vec<DirEntry>> {
        self.inner.readdir(dir)
    }
    fn create(&mut self, _: Ino, _: &str, _: u16, _: u32, _: u32) -> FsResult<Ino> {
        Err(FsError::ReadOnly)
    }
    fn mkdir(&mut self, _: Ino, _: &str, _: u16, _: u32, _: u32) -> FsResult<Ino> {
        Err(FsError::ReadOnly)
    }
    fn symlink(&mut self, _: Ino, _: &str, _: &str, _: u32, _: u32) -> FsResult<Ino> {
        Err(FsError::ReadOnly)
    }
    fn readlink(&mut self, ino: Ino) -> FsResult<String> {
        self.inner.readlink(ino)
    }
    fn unlink(&mut self, _: Ino, _: &str) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }
    fn rmdir(&mut self, _: Ino, _: &str) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }
    fn rename(&mut self, _: Ino, _: &str, _: Ino, _: &str) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }
    fn sync(&mut self) -> FsResult<()> {
        Ok(())
    }
    fn dirty_state(&mut self) -> DirtyState {
        self.inner.dirty_state()
    }
    fn repair(&mut self) -> FsResult<String> {
        Err(FsError::ReadOnly)
    }
}

/// Shared helper: validate a single path component.
pub fn check_name(name: &str, max_len: usize) -> FsResult<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(FsError::InvalidName);
    }
    if name.len() > max_len {
        return Err(FsError::NameTooLong);
    }
    Ok(())
}

#[doc(hidden)]
pub mod testutil;
