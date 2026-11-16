//! Streaming checksums (SHA-256, SHA-1, MD5) of images and disks.
//!
//! Owner: the Raspberry Pi / utilities workstream.

use serde_json::Value;

use super::job::JobCtx;
use super::plan::HashRequest;
use super::{ForgeError, ForgeResult};

pub fn run(_r: &HashRequest, _ctx: &JobCtx) -> ForgeResult<Value> {
    Err(ForgeError::Unsupported("hash: not implemented yet".into()))
}
