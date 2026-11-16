//! Windows installation media: FAT32 + split WIM, or NTFS + UEFI:NTFS; BIOS boot records;
//! autounattend.xml (hardware-check bypass, local account, unattended OOBE).
//!
//! Owner: the Windows workstream.

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
pub struct WindowsInfo {
    /// "Windows 11", "Windows 10", "Windows 8.1", "Windows 7", "Windows Server 2022"…
    pub version: String,
    /// "x64", "arm64", "x86".
    pub arch: String,
    /// `/sources/install.wim` or `/sources/install.esd`.
    pub install_image: String,
    pub install_image_size: u64,
    pub editions: Vec<String>,
}

pub fn detect(_tree: &mut dyn SourceTree) -> ForgeResult<Option<WindowsInfo>> {
    Ok(None)
}

pub fn write(_r: &FlashRequest, _src: Arc<dyn BlockDev>, _target: &OpenTarget, _ctx: &JobCtx) -> ForgeResult<Value> {
    Err(ForgeError::Unsupported("Windows media: not implemented yet".into()))
}
