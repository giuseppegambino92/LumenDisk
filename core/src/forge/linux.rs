//! Linux / BSD ISO mode: copy the ISO's files onto a fresh filesystem, make it bootable
//! (UEFI from the ISO's EFI loaders, BIOS via syslinux / GRUB), add a persistence partition.
//!
//! Owner: the ISO / Linux workstream.

use std::sync::Arc;

use serde_json::Value;

use super::job::JobCtx;
use super::plan::FlashRequest;
use super::target::OpenTarget;
use super::tree::SourceTree;
use super::{ForgeError, ForgeResult};
use crate::block::BlockDev;

/// What [`detect`] found in an ISO (serialised into the inspect result).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct LinuxInfo {
    pub distro: String,
    pub has_efi: bool,
    pub has_isolinux: bool,
    pub has_grub: bool,
    /// "casper" (Ubuntu), "live-boot" (Debian), … — None: no persistence support known.
    pub persistence: Option<String>,
}

pub fn detect(_tree: &mut dyn SourceTree) -> ForgeResult<Option<LinuxInfo>> {
    Ok(None)
}

pub fn write(_r: &FlashRequest, _src: Arc<dyn BlockDev>, _target: &OpenTarget, _ctx: &JobCtx) -> ForgeResult<Value> {
    Err(ForgeError::Unsupported("ISO mode: not implemented yet".into()))
}
