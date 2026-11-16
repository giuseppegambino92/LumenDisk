//! Writing an image: DD mode (with verification), and dispatch to ISO mode
//! ([`super::linux::write`]), Windows media ([`super::windows::write`]) and Raspberry Pi
//! customisation ([`super::rpi::customize`]).
//!
//! Owner: the image / DD workstream.

use serde_json::Value;

use super::job::JobCtx;
use super::plan::FlashRequest;
use super::{ForgeError, ForgeResult};

pub fn run(_r: &FlashRequest, _ctx: &JobCtx) -> ForgeResult<Value> {
    Err(ForgeError::Unsupported("flash: not implemented yet".into()))
}
