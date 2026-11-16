//! Forge C ABI (LumenDisk 1.1): jobs (format, flash, wipe, hash, doctor, inspect), candidate
//! disks, and quick image inspection. JSON in / JSON out, like the rest of the ABI (see
//! `include/lumendisk_ffi.h`).

use std::ffi::c_char;

use serde_json::json;

use crate::forge::{image, job, plan, target};
use crate::{arg_str, ffi_call, parse_json};

/// Start a request (`plan::Request`, tagged by "op") as a job: {"ok":true,"job":u64}.
#[no_mangle]
pub extern "C" fn ld_forge_start(request_json: *const c_char) -> *mut c_char {
    ffi_call(|| {
        let req: plan::Request = parse_json(&arg_str(request_json)?)?;
        Ok(json!({"job": plan::start(req)}))
    })
}

/// A job's snapshot, with the log lines after `since`.
#[no_mangle]
pub extern "C" fn ld_forge_status(id: u64, since: u64) -> *mut c_char {
    ffi_call(|| {
        let s = job::status(id, since).ok_or_else(|| crate::i18n::t(&format!("operazione {id} sconosciuta"), &format!("unknown job {id}")))?;
        serde_json::to_value(s).map_err(|e| e.to_string())
    })
}

#[no_mangle]
pub extern "C" fn ld_forge_cancel(id: u64) -> *mut c_char {
    ffi_call(|| Ok(json!({"cancelled": job::cancel(id)})))
}

#[no_mangle]
pub extern "C" fn ld_forge_forget(id: u64) -> *mut c_char {
    ffi_call(|| Ok(json!({"forgotten": job::forget(id)})))
}

#[no_mangle]
pub extern "C" fn ld_forge_disks() -> *mut c_char {
    ffi_call(|| Ok(json!({"disks": target::list_disks().map_err(|e| e.to_string())?})))
}

/// Quick, synchronous inspection of an image (drop zone): never hashes, decompresses at most
/// `image::QUICK_BUDGET` bytes.
#[no_mangle]
pub extern "C" fn ld_forge_inspect(path: *const c_char) -> *mut c_char {
    ffi_call(|| image::inspect_quick(&arg_str(path)?).map_err(|e| e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString};

    fn call(p: *mut c_char) -> serde_json::Value {
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        crate::ld_free_string(p);
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn jobs_run_through_the_c_abi() {
        let dir = std::env::temp_dir().join(format!("ld-forge-ffi-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.img");
        std::fs::write(&src, vec![7u8; 1 << 20]).unwrap();
        let out = dir.join("b.img");
        let v = call(ld_forge_inspect(CString::new(src.to_string_lossy().as_bytes()).unwrap().as_ptr()));
        assert_eq!(v["ok"], true);
        assert_eq!(v["format"], "raw");
        assert_eq!(v["recommended_mode"], "dd");
        let req = json!({"op": "flash", "source": src, "target": {"kind": "file", "path": out, "size": null}, "eject": false});
        let v = call(ld_forge_start(CString::new(req.to_string()).unwrap().as_ptr()));
        assert_eq!(v["ok"], true);
        let id = v["job"].as_u64().unwrap();
        let mut last = json!(null);
        for _ in 0..1000 {
            last = call(ld_forge_status(id, 0));
            if last["state"] != "running" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(last["ok"], true);
        assert_eq!(last["state"], "succeeded", "{last}");
        assert_eq!(last["result"]["verification"]["ok"], true);
        assert_eq!(std::fs::read(&out).unwrap(), vec![7u8; 1 << 20]);
        assert_eq!(call(ld_forge_forget(id))["forgotten"], true);
        assert_eq!(call(ld_forge_status(id, 0))["ok"], false);
        assert_eq!(call(ld_forge_cancel(id))["cancelled"], false);
        let bad = call(ld_forge_start(CString::new("{\"op\":\"nope\"}").unwrap().as_ptr()));
        assert_eq!(bad["ok"], false);
        let bad = call(ld_forge_inspect(CString::new("/nonexistent/x.iso").unwrap().as_ptr()));
        assert_eq!(bad["ok"], false);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
