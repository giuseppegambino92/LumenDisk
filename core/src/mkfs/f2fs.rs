//! mkfs.f2fs — owner: the F2FS / UFS2 workstream.

use std::sync::Arc;

use crate::block::BlockDev;
use crate::forge::MkfsOptions;
use crate::vfs::{FsError, FsResult};

pub fn format(_dev: Arc<dyn BlockDev>, _opts: &MkfsOptions) -> FsResult<()> {
    Err(FsError::Unsupported("f2fs: not implemented yet".into()))
}
