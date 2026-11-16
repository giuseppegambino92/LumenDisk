//! ISO 9660 (ECMA-119) with Joliet and Rock Ridge (SUSP / RRIP).

use super::{Limits, Parsed, View};
use crate::block::BlockDev;
use crate::forge::{ForgeError, ForgeResult};

pub(crate) fn has_iso9660(_dev: &dyn BlockDev) -> bool {
    false
}

pub(crate) fn read(_dev: &dyn BlockDev, _lim: &Limits, _want: Option<View>) -> ForgeResult<Parsed> {
    Err(ForgeError::Unsupported("ISO 9660: not implemented yet".into()))
}
