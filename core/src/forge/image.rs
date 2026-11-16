//! Source images: raw IMG / ISO, gz / xz / bz2 / zip, VHD, DMG — opened as byte streams or
//! random-access devices, and [`inspect`]ed (type, size, OS, recommended write mode).
//!
//! Owner: the image / DD workstream.

use serde_json::Value;

use super::job::JobCtx;
use super::{ForgeError, ForgeResult};

pub fn inspect(_source: &str, _ctx: &JobCtx) -> ForgeResult<Value> {
    Err(ForgeError::Unsupported("inspect: not implemented yet".into()))
}
