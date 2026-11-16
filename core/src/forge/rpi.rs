//! Raspberry Pi / SBC first-boot customisation of a written image: hostname, user and
//! password, SSH, Wi-Fi, locale (firstrun.sh / userconf.txt, or cloud-init).
//!
//! Owner: the Raspberry Pi / utilities workstream.

use std::sync::Arc;

use serde_json::Value;

use super::job::JobCtx;
use super::plan::RpiOptions;
use super::{ForgeError, ForgeResult};
use crate::block::BlockDev;

/// What [`detect`] found on an image / disk (serialised into the inspect result).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct RpiInfo {
    /// "Raspberry Pi OS (bookworm)", "Ubuntu 24.04 (Raspberry Pi)"…
    pub os: String,
    /// "firstrun" (Raspberry Pi OS ≤ bookworm), "cloud-init" (Ubuntu, Raspberry Pi OS trixie+).
    pub method: String,
}

pub fn detect(_dev: Arc<dyn BlockDev>) -> ForgeResult<Option<RpiInfo>> {
    Ok(None)
}

pub fn customize(_dev: Arc<dyn BlockDev>, _opts: &RpiOptions, _ctx: &JobCtx) -> ForgeResult<Value> {
    Err(ForgeError::Unsupported("Raspberry Pi customisation: not implemented yet".into()))
}
