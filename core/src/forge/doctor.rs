//! Partition table doctor: check, repair (GPT from its backup, CRCs, protective MBR),
//! MBR ⇄ GPT conversion in place.
//!
//! Owner: the partitioning workstream.

use serde_json::Value;

use super::job::JobCtx;
use super::plan::DoctorRequest;
use super::{ForgeError, ForgeResult};

pub fn run(_r: &DoctorRequest, _ctx: &JobCtx) -> ForgeResult<Value> {
    Err(ForgeError::Unsupported("doctor: not implemented yet".into()))
}
