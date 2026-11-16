//! UDF (ECMA-167 3rd edition + OSTA UDF 1.02 – 2.60) reader.

use super::{Limits, Parsed};
use crate::block::BlockDev;
use crate::forge::{ForgeError, ForgeResult};

/// Whether the image has a UDF Volume Recognition Sequence and an Anchor Volume Descriptor.
pub(crate) fn probe(_dev: &dyn BlockDev) -> bool {
    false
}

/// Parse the UDF file set into a tree.
pub(crate) fn read(_dev: &dyn BlockDev, _lim: &Limits) -> ForgeResult<Parsed> {
    Err(ForgeError::Unsupported("UDF: not implemented yet".into()))
}
