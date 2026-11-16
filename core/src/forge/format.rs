//! One [`FormatRequest`] → partition table + filesystems.
//!
//! Owner: the partitioning workstream (layouts, cluster sizes, label rules, orchestration).

use std::sync::Arc;

use serde_json::Value;

use super::job::JobCtx;
use super::plan::FormatRequest;
use super::{ForgeError, ForgeResult, MkfsOptions, TargetFs};
use crate::block::BlockDev;

/// Create filesystem `fs` on `dev` (a whole partition / device view).
pub fn mkfs(dev: Arc<dyn BlockDev>, fs: TargetFs, opts: &MkfsOptions) -> ForgeResult<()> {
    use crate::vfs::{self, FsType};
    let label = sanitize_label(fs, &opts.label);
    match fs {
        TargetFs::Ext2 => vfs::format(dev, FsType::Ext2, &label)?,
        TargetFs::Ext3 => vfs::format(dev, FsType::Ext3, &label)?,
        TargetFs::Ext4 => vfs::format(dev, FsType::Ext4, &label)?,
        TargetFs::Fat16 => vfs::format(dev, FsType::Fat16, &label)?,
        TargetFs::Fat32 => vfs::format(dev, FsType::Fat32, &label)?,
        TargetFs::Exfat => vfs::format(dev, FsType::ExFat, &label)?,
        TargetFs::Ntfs => vfs::format(dev, FsType::Ntfs, &label)?,
        TargetFs::Btrfs => crate::mkfs::btrfs::format(dev, &MkfsOptions { label, ..opts.clone() })?,
        TargetFs::Xfs => crate::mkfs::xfs::format(dev, &MkfsOptions { label, ..opts.clone() })?,
        TargetFs::F2fs => crate::mkfs::f2fs::format(dev, &MkfsOptions { label, ..opts.clone() })?,
        TargetFs::Ufs2 => crate::mkfs::ufs::format(dev, &MkfsOptions { label, ..opts.clone() })?,
    }
    Ok(())
}

/// The label as `fs` can store it (length, character set, case).
pub fn sanitize_label(fs: TargetFs, label: &str) -> String {
    let max = match fs {
        TargetFs::Fat16 | TargetFs::Fat32 => 11,
        TargetFs::Exfat => 15,
        TargetFs::Ntfs => 32,
        TargetFs::Ext2 | TargetFs::Ext3 | TargetFs::Ext4 => 16,
        TargetFs::Xfs => 12,
        TargetFs::Btrfs => 255,
        TargetFs::F2fs => 512,
        TargetFs::Ufs2 => 31,
    };
    let mut out: String = label.chars().filter(|c| !c.is_control()).collect();
    if fs == TargetFs::Ufs2 {
        // newfs -L accepts only these (FreeBSD sbin/newfs/newfs.c).
        out = out.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    }
    if matches!(fs, TargetFs::Fat16 | TargetFs::Fat32) {
        out = out.to_uppercase().chars().filter(|c| c.is_ascii() && !"\"*+,./:;<=>?[\\]|".contains(*c)).collect();
    }
    let mut s = String::new();
    for c in out.chars() {
        if s.len() + c.len_utf8() > max {
            break;
        }
        s.push(c);
    }
    s
}

pub fn run(_r: &FormatRequest, _ctx: &JobCtx) -> ForgeResult<Value> {
    Err(ForgeError::Unsupported("format: not implemented yet".into()))
}
