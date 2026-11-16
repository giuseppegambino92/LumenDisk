//! Forge: formatting, flashing and disk utilities (LumenDisk 1.1).
//!
//! Everything here runs in user space. Whole disks are opened through `authopen` (the system's
//! own privilege broker, see [`target`]); LumenDisk never runs privileged code itself.
//!
//! Layout of the subsystem:
//! - [`job`]: progress / cancel / log plumbing shared by every long operation.
//! - [`target`]: choosing a destination disk safely, unmounting, opening, ejecting.
//! - [`tree`]: a read-only tree of files (an ISO, a folder) and copying it into a volume.
//! - [`layout`]: writing GPT / MBR / APM partition tables.
//! - [`format`]: one request → partition table + filesystems (+ cluster sizes, labels).
//! - [`doctor`]: checking / repairing partition tables, MBR ⇄ GPT conversion.
//! - [`image`]: source images (raw, ISO, gz/xz/bz2/zip, VHD, DMG) as byte streams.
//! - [`flash`]: DD-style writing with verification.
//! - [`hash`]: streaming SHA-256 / SHA-1 / MD5.
//! - [`wipe`]: secure erase patterns.
//! - [`linux`]: Linux / BSD ISO mode (file copy, syslinux / GRUB, persistence).
//! - [`windows`]: Windows media (WIM split, UEFI:NTFS, boot records, autounattend).
//! - [`rpi`]: Raspberry Pi / SBC first-boot customisation.
//! - [`crypt`]: SHA-512 crypt (`$6$`) for Linux account passwords.
//! - [`plan`]: the high-level requests the UI sends (what to do with which source and target).

pub mod crypt;
pub mod doctor;
pub mod flash;
pub mod format;
pub mod hash;
pub mod image;
pub mod job;
pub mod layout;
pub mod linux;
pub mod plan;
pub mod rpi;
pub mod target;
pub mod tree;
pub mod windows;
pub mod wipe;

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::vfs::{FsError, FsType};

/// Errors of forge operations. `Display` is the user-facing message (already localised where
/// it matters: use `crate::i18n::t` when building `Invalid` / `Unsafe` messages).
#[derive(Debug)]
pub enum ForgeError {
    /// The user cancelled the job.
    Cancelled,
    /// I/O on the source or the target.
    Io(String),
    /// The request or the input is not valid (bad image, bad option combination…).
    Invalid(String),
    /// Valid, but not supported (yet / by this source).
    Unsupported(String),
    /// Refused to protect the user's data (system disk, internal disk, mounted volume in use…).
    Unsafe(String),
    /// The written data does not match the source.
    Mismatch(String),
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForgeError::Cancelled => f.write_str(&crate::i18n::t("operazione annullata", "operation cancelled")),
            ForgeError::Io(s) | ForgeError::Invalid(s) | ForgeError::Unsupported(s) | ForgeError::Unsafe(s) | ForgeError::Mismatch(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for ForgeError {}

impl From<std::io::Error> for ForgeError {
    fn from(e: std::io::Error) -> Self {
        ForgeError::Io(e.to_string())
    }
}

impl From<FsError> for ForgeError {
    fn from(e: FsError) -> Self {
        match e {
            FsError::Unsupported(s) => ForgeError::Unsupported(s),
            FsError::Invalid(s) => ForgeError::Invalid(s),
            other => ForgeError::Io(other.to_string()),
        }
    }
}

pub type ForgeResult<T> = Result<T, ForgeError>;

/// Every filesystem LumenDisk can create. The first eight are also mountable by LumenDisk
/// (they map to [`FsType`]); the others are created for use on Linux / BSD.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetFs {
    Ext2,
    Ext3,
    Ext4,
    Fat16,
    Fat32,
    Exfat,
    Ntfs,
    Btrfs,
    Xfs,
    F2fs,
    Ufs2,
}

impl TargetFs {
    pub const ALL: [TargetFs; 11] = [
        TargetFs::Ext2,
        TargetFs::Ext3,
        TargetFs::Ext4,
        TargetFs::Fat16,
        TargetFs::Fat32,
        TargetFs::Exfat,
        TargetFs::Ntfs,
        TargetFs::Btrfs,
        TargetFs::Xfs,
        TargetFs::F2fs,
        TargetFs::Ufs2,
    ];

    /// The LumenDisk driver for it, if it can be mounted by LumenDisk.
    pub fn mountable(self) -> Option<FsType> {
        Some(match self {
            TargetFs::Ext2 => FsType::Ext2,
            TargetFs::Ext3 => FsType::Ext3,
            TargetFs::Ext4 => FsType::Ext4,
            TargetFs::Fat16 => FsType::Fat16,
            TargetFs::Fat32 => FsType::Fat32,
            TargetFs::Exfat => FsType::ExFat,
            TargetFs::Ntfs => FsType::Ntfs,
            _ => return None,
        })
    }

    pub fn display_name(self) -> &'static str {
        match self {
            TargetFs::Ext2 => "ext2",
            TargetFs::Ext3 => "ext3",
            TargetFs::Ext4 => "ext4",
            TargetFs::Fat16 => "FAT16",
            TargetFs::Fat32 => "FAT32",
            TargetFs::Exfat => "exFAT",
            TargetFs::Ntfs => "NTFS",
            TargetFs::Btrfs => "Btrfs",
            TargetFs::Xfs => "XFS",
            TargetFs::F2fs => "F2FS",
            TargetFs::Ufs2 => "UFS2",
        }
    }
}

/// Options common to every `mkfs`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MkfsOptions {
    /// Volume label as the user typed it; each filesystem sanitises it (see
    /// `format::sanitize_label`).
    #[serde(default)]
    pub label: String,
    /// Allocation unit in bytes (cluster for FAT / exFAT / NTFS, block for ext / XFS / UFS,
    /// sector size is never changed). None: the filesystem's default for that size.
    #[serde(default)]
    pub cluster_size: Option<u32>,
    /// Filesystem UUID / serial. None: random.
    #[serde(default)]
    pub uuid: Option<[u8; 16]>,
}

/// Run in tests and when `LUMENDISK_FORGE_VIRTUAL_ONLY=1`: destructive operations accept only
/// virtual disks (attached disk images), never a physical one. Every test that writes to a
/// `/dev` node relies on this.
pub fn virtual_only() -> bool {
    cfg!(test) || std::env::var("LUMENDISK_FORGE_VIRTUAL_ONLY").map(|v| v == "1").unwrap_or(false)
}
