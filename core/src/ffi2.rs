//! Phase 2 C ABI: remote mounts, image streaming, LAN share, Wi-Fi portal.
use std::ffi::c_char;

use serde_json::json;

use crate::volume::{runtime, ImageUrlRequest, RemoteMountRequest};
use crate::{arg_str, ffi_call, parse_json};

#[no_mangle]
pub extern "C" fn ld_mount_remote(r: *const c_char) -> *mut c_char {
    ffi_call(|| {
        let req: RemoteMountRequest = parse_json(&arg_str(r)?)?;
        Ok(json!({"volume": runtime().mount_remote(req)?}))
    })
}

#[no_mangle]
pub extern "C" fn ld_probe_remote(r: *const c_char) -> *mut c_char {
    ffi_call(|| {
        let req: RemoteMountRequest = parse_json(&arg_str(r)?)?;
        let (lat, n) = runtime().probe_remote(&req)?;
        Ok(json!({"latency_ms": lat, "entries": n}))
    })
}

#[no_mangle]
pub extern "C" fn ld_remote_sync_now(id: *const c_char) -> *mut c_char {
    ffi_call(|| {
        let (synced, pending) = runtime().remote_sync_now(&arg_str(id)?)?;
        Ok(json!({"synced": synced, "pending": pending}))
    })
}

#[no_mangle]
pub extern "C" fn ld_inspect_image_url(r: *const c_char) -> *mut c_char {
    ffi_call(|| {
        let req: ImageUrlRequest = parse_json(&arg_str(r)?)?;
        let (rep, fetched) = runtime().inspect_image_url(&req)?;
        let mut v = serde_json::to_value(rep).map_err(|e| e.to_string())?;
        v["fetched_bytes"] = json!(fetched);
        Ok(v)
    })
}

#[no_mangle]
pub extern "C" fn ld_mount_image_url(r: *const c_char) -> *mut c_char {
    ffi_call(|| {
        let req: ImageUrlRequest = parse_json(&arg_str(r)?)?;
        Ok(json!({"volume": runtime().mount_image_url(req)?}))
    })
}

#[no_mangle]
pub extern "C" fn ld_share_start(id: *const c_char) -> *mut c_char {
    ffi_call(|| Ok(json!({"share": runtime().share_start(&arg_str(id)?)?})))
}

#[no_mangle]
pub extern "C" fn ld_share_stop(id: *const c_char) -> *mut c_char {
    ffi_call(|| {
        runtime().share_stop(&arg_str(id)?)?;
        Ok(json!({}))
    })
}

#[no_mangle]
pub extern "C" fn ld_portal_start(id: *const c_char) -> *mut c_char {
    ffi_call(|| Ok(json!({"portal": runtime().portal_start(&arg_str(id)?)?})))
}

#[no_mangle]
pub extern "C" fn ld_portal_stop(id: *const c_char) -> *mut c_char {
    ffi_call(|| {
        runtime().portal_stop(&arg_str(id)?)?;
        Ok(json!({}))
    })
}

// ───────────── Phase 3 ─────────────

#[no_mangle]
pub extern "C" fn ld_benchmark(id: *const c_char) -> *mut c_char {
    ffi_call(|| {
        let v = runtime().get(&arg_str(id)?)?;
        if v.nfs_read_only {
            return Err(crate::i18n::t("il volume è in sola lettura: il test di scrittura non è possibile", "the volume is read-only: a write test is not possible"));
        }
        let r = crate::health::benchmark(&v.mountpoint)?;
        Ok(json!({"result": r}))
    })
}

#[no_mangle]
pub extern "C" fn ld_disk_health(bsd: *const c_char) -> *mut c_char {
    ffi_call(|| Ok(json!({"health": crate::health::disk_health(&arg_str(bsd)?)?})))
}

/// Language of core messages: "it" (default) or "en".
#[no_mangle]
pub extern "C" fn ld_set_language(code: *const c_char) -> *mut c_char {
    ffi_call(|| {
        crate::i18n::set_language(&arg_str(code)?);
        Ok(json!({"language": if crate::i18n::is_english() { "en" } else { "it" }}))
    })
}
