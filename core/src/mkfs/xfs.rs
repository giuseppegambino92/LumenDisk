//! mkfs.xfs — owner: the Btrfs / XFS workstream.

use std::sync::Arc;

use crate::block::BlockDev;
use crate::forge::MkfsOptions;
use crate::vfs::{FsError, FsResult};

pub const MIN_DEV_SIZE: u64 = 300 << 20;

pub fn format(dev: Arc<dyn BlockDev>, opts: &MkfsOptions) -> FsResult<()> {
    format_with_sector(dev, opts, 512)
}

pub fn format_with_sector(_dev: Arc<dyn BlockDev>, _opts: &MkfsOptions, _sector: u32) -> FsResult<()> {
    Err(FsError::Unsupported("xfs: not implemented yet".into()))
}
