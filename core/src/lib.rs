//! lumendisk-core: user-space filesystem engine (ext2/3/4, FAT12/16/32, exFAT, NTFS) exposed
//! to macOS through an ephemeral loopback NFSv3 server — no kexts, no MacFUSE.
//!
//! The C ABI (see `include/lumendisk_ffi.h`) exchanges JSON strings with the Swift app.

pub mod block;
pub mod forge;
#[doc(hidden)]
pub mod fuzzkit;
pub mod health;
pub mod i18n;
pub mod iso;
pub mod lan;
pub mod lock_inspector;
pub mod mkfs;
pub mod nfs_loopback;
pub mod remote;
pub mod vfs;
pub mod volume;
pub mod wal;

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::{json, Value};

use crate::volume::{runtime, MountRequest};

fn arg(p: *const c_char) -> Result<String, String> {
    if p.is_null() {
        return Err("null argument".into());
    }
    // SAFETY: the caller passes a valid NUL-terminated string.
    Ok(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

pub(crate) fn arg_str(p: *const c_char) -> Result<String, String> {
    arg(p)
}
pub(crate) fn ffi_call(f: impl FnOnce() -> Result<Value, String>) -> *mut c_char {
    ffi(f)
}
pub(crate) fn parse_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, String> {
    parse(s)
}

fn out(v: Value) -> *mut c_char {
    let s = serde_json::to_string(&v).unwrap_or_else(|_| "{\"ok\":false,\"error\":\"serialisation\"}".into());
    CString::new(s).unwrap_or_else(|_| CString::new("{\"ok\":false}").unwrap()).into_raw()
}

/// Run an FFI body: never unwind into Swift, always answer with JSON.
fn ffi(f: impl FnOnce() -> Result<Value, String>) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(mut v)) => {
            if let Value::Object(m) = &mut v {
                m.insert("ok".into(), Value::Bool(true));
            }
            out(v)
        }
        Ok(Err(e)) => out(json!({"ok": false, "error": e})),
        Err(p) => {
            let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
            out(json!({"ok": false, "error": format!("errore interno: {msg}")}))
        }
    }
}

fn parse<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, String> {
    serde_json::from_str(s).map_err(|e| crate::i18n::t(&format!("JSON non valido: {e}"), &format!("invalid JSON: {e}")))
}

#[no_mangle]
pub extern "C" fn ld_version() -> *mut c_char {
    ffi(|| Ok(json!({"version": env!("CARGO_PKG_VERSION")})))
}

#[no_mangle]
pub extern "C" fn ld_init(support_dir: *const c_char) -> *mut c_char {
    ffi(|| {
        let d = arg(support_dir)?;
        runtime().set_support_dir(std::path::Path::new(&d));
        Ok(json!({}))
    })
}

#[derive(serde::Deserialize)]
struct SourceReq {
    path: String,
    #[serde(default)]
    partition: u32,
    #[serde(default)]
    fd: Option<i32>,
}

#[no_mangle]
pub extern "C" fn ld_inspect_source(request_json: *const c_char) -> *mut c_char {
    ffi(|| {
        let r: SourceReq = parse(&arg(request_json)?)?;
        let rep = runtime().inspect_source(&r.path, r.fd)?;
        Ok(serde_json::to_value(rep).map_err(|e| e.to_string())?)
    })
}

#[no_mangle]
pub extern "C" fn ld_mount(request_json: *const c_char) -> *mut c_char {
    ffi(|| {
        let r: MountRequest = parse(&arg(request_json)?)?;
        let st = runtime().mount(r)?;
        Ok(json!({"volume": st}))
    })
}

#[no_mangle]
pub extern "C" fn ld_unmount(volume_id: *const c_char, force: bool) -> *mut c_char {
    ffi(|| {
        runtime().unmount(&arg(volume_id)?, force)?;
        Ok(json!({}))
    })
}

#[no_mangle]
pub extern "C" fn ld_unmount_all() -> *mut c_char {
    ffi(|| {
        let (n, failed) = runtime().unmount_all();
        Ok(json!({"unmounted": n, "failed": failed}))
    })
}

#[no_mangle]
pub extern "C" fn ld_list_volumes() -> *mut c_char {
    ffi(|| Ok(json!({"volumes": runtime().list()})))
}

#[no_mangle]
pub extern "C" fn ld_shadow_changes(volume_id: *const c_char) -> *mut c_char {
    ffi(|| Ok(json!({"changes": runtime().shadow_changes(&arg(volume_id)?)?})))
}

#[no_mangle]
pub extern "C" fn ld_shadow_commit(volume_id: *const c_char) -> *mut c_char {
    ffi(|| Ok(json!({"applied": runtime().shadow_commit(&arg(volume_id)?)?})))
}

#[no_mangle]
pub extern "C" fn ld_shadow_rollback(volume_id: *const c_char) -> *mut c_char {
    ffi(|| Ok(json!({"discarded": runtime().shadow_rollback(&arg(volume_id)?)?})))
}

#[no_mangle]
pub extern "C" fn ld_ghost_entries(volume_id: *const c_char) -> *mut c_char {
    ffi(|| Ok(json!({"entries": runtime().ghost_entries(&arg(volume_id)?)?})))
}

#[no_mangle]
pub extern "C" fn ld_ghost_set_rules(volume_id: *const c_char, rules: *const c_char) -> *mut c_char {
    ffi(|| {
        let r = parse(&arg(rules)?)?;
        runtime().ghost_set_rules(&arg(volume_id)?, r)?;
        Ok(json!({}))
    })
}

#[no_mangle]
pub extern "C" fn ld_ghost_set_enabled(volume_id: *const c_char, enabled: bool) -> *mut c_char {
    ffi(|| {
        runtime().ghost_set_enabled(&arg(volume_id)?, enabled)?;
        Ok(json!({}))
    })
}

#[no_mangle]
pub extern "C" fn ld_ghost_purge(volume_id: *const c_char) -> *mut c_char {
    ffi(|| {
        runtime().ghost_purge(&arg(volume_id)?)?;
        Ok(json!({}))
    })
}

#[derive(serde::Deserialize)]
struct UidReq {
    #[serde(default = "t")]
    enabled: bool,
    linux_uid: Option<u32>,
    linux_gid: Option<u32>,
}
fn t() -> bool {
    true
}

#[no_mangle]
pub extern "C" fn ld_set_uid_map(volume_id: *const c_char, map_json: *const c_char) -> *mut c_char {
    ffi(|| {
        let r: UidReq = parse(&arg(map_json)?)?;
        let m = runtime().set_uid_map(&arg(volume_id)?, r.enabled, r.linux_uid, r.linux_gid)?;
        Ok(json!({"uid_map": m}))
    })
}

#[no_mangle]
pub extern "C" fn ld_lock_inspect(mountpoint: *const c_char) -> *mut c_char {
    ffi(|| Ok(json!({"holders": lock_inspector::inspect(&arg(mountpoint)?)})))
}

#[no_mangle]
pub extern "C" fn ld_release_locks(mountpoint: *const c_char, pids_json: *const c_char) -> *mut c_char {
    ffi(|| {
        let mp = arg(mountpoint)?;
        let pids: Vec<i32> = parse(&arg(pids_json)?)?;
        // Only signal processes that really hold something on this mountpoint.
        let holders: Vec<i32> = lock_inspector::inspect(&mp).into_iter().map(|h| h.pid).collect();
        let targets: Vec<i32> = pids.into_iter().filter(|p| holders.contains(p)).collect();
        Ok(json!({"terminated": lock_inspector::release(&targets, 3000)}))
    })
}

#[no_mangle]
pub extern "C" fn ld_repair_source(request_json: *const c_char) -> *mut c_char {
    ffi(|| {
        let r: SourceReq = parse(&arg(request_json)?)?;
        Ok(json!({"report": runtime().repair_source(&r.path, r.partition, r.fd)?}))
    })
}

#[no_mangle]
pub extern "C" fn ld_repair_volume(volume_id: *const c_char) -> *mut c_char {
    ffi(|| {
        let (report, st) = runtime().repair_volume(&arg(volume_id)?)?;
        Ok(json!({"report": report, "volume": st}))
    })
}

#[derive(serde::Deserialize)]
struct CreateReq {
    path: String,
    fs: String,
    size_mb: u64,
    #[serde(default)]
    label: String,
}

#[no_mangle]
pub extern "C" fn ld_create_image(request_json: *const c_char) -> *mut c_char {
    ffi(|| {
        let r: CreateReq = parse(&arg(request_json)?)?;
        let label = if r.label.is_empty() { "LUMENDISK".to_string() } else { r.label };
        Ok(json!({"path": runtime().create_image(&r.path, &r.fs, r.size_mb, &label)?}))
    })
}

#[no_mangle]
pub extern "C" fn ld_free_string(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: `s` was produced by CString::into_raw in this library.
        unsafe { drop(CString::from_raw(s)) };
    }
}

// ───────────────────────────── Phase 2 (network & remote) ─────────────────────────────

pub mod ffi2;
