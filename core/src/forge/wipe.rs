//! Secure erase: zeros, random, DoD 5220.22-M (3 / 7 passes), Gutmann (35 passes).
//!
//! Owner: the Raspberry Pi / utilities workstream.

use serde_json::Value;

use super::job::JobCtx;
use super::plan::WipeRequest;
use super::{ForgeError, ForgeResult};

pub fn run(_r: &WipeRequest, _ctx: &JobCtx) -> ForgeResult<Value> {
    Err(ForgeError::Unsupported("wipe: not implemented yet".into()))
}
