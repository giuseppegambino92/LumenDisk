//! Destination disks: which ones may be written, unmounting, opening, ejecting.
//!
//! **Safety policy** ([`check_policy`]): a destructive operation only ever targets a *whole*
//! disk that is
//! - not the startup disk nor a disk backing it (APFS physical stores of `/` and of the data
//!   volume),
//! - writable,
//! - and either an attached disk image, or removable / external media (USB, Thunderbolt, SD).
//!
//! Internal fixed disks are refused, always. With [`super::virtual_only`] (tests, and
//! `LUMENDISK_FORGE_VIRTUAL_ONLY=1`) only attached disk images are accepted.
//!
//! Information comes from `diskutil info -plist` (no private frameworks); whole disks are
//! opened as `/dev/rdiskN` through [`crate::block::open_source`], which falls back to the
//! system's `authopen` (admin / Touch ID prompt) when LumenDisk has no access.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ForgeError, ForgeResult};
use crate::block::BlockDev;
use crate::i18n::t;

/// What the UI shows about a candidate disk.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiskInfo {
    /// `disk4`
    pub bsd: String,
    pub media_name: String,
    pub size: u64,
    pub block_size: u32,
    pub internal: bool,
    pub removable: bool,
    pub ejectable: bool,
    pub whole: bool,
    pub writable: bool,
    /// An attached disk image (`hdiutil attach`): BusProtocol "Disk Image".
    pub disk_image: bool,
    /// "USB", "Secure Digital", "Thunderbolt", "Disk Image", …
    pub protocol: String,
    /// The startup disk, or a disk backing it.
    pub boot: bool,
    /// Volume names currently mounted from this disk.
    pub mounted: Vec<String>,
    /// Whether [`check_policy`] lets LumenDisk write to it, and why not.
    pub allowed: bool,
    pub refusal: Option<String>,
}

fn bsd_name(dev: &str) -> String {
    let d = dev.trim_start_matches("/dev/");
    d.strip_prefix('r').filter(|s| s.starts_with("disk")).unwrap_or(d).to_string()
}

fn whole_of(bsd: &str) -> String {
    // disk4s1 → disk4, disk3s1s1 → disk3
    let rest = &bsd[4.min(bsd.len())..];
    let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    format!("disk{n}")
}

fn plist_json(args: &[&str]) -> ForgeResult<Value> {
    let out = Command::new("/usr/sbin/diskutil").args(args).output()?;
    if !out.status.success() {
        return Err(ForgeError::Io(format!("diskutil {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())));
    }
    let mut conv = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    {
        use std::io::Write;
        conv.stdin.take().unwrap().write_all(&out.stdout)?;
    }
    let o = conv.wait_with_output()?;
    serde_json::from_slice(&o.stdout).map_err(|e| ForgeError::Io(format!("diskutil plist: {e}")))
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}
fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Whole disks that hold the running system: never written, whatever the user picks.
pub fn boot_disks() -> HashSet<String> {
    let mut out = HashSet::new();
    for vol in ["/", "/System/Volumes/Data"] {
        let Ok(v) = plist_json(&["info", "-plist", vol]) else { continue };
        let parent = s(&v, "ParentWholeDisk");
        if !parent.is_empty() {
            out.insert(parent);
        }
        if let Some(stores) = v.get("APFSPhysicalStores").and_then(Value::as_array) {
            for st in stores {
                let store = s(st, "APFSPhysicalStore");
                if !store.is_empty() {
                    out.insert(whole_of(&store));
                }
            }
        }
    }
    if out.is_empty() {
        // Could not determine it: be conservative, the first internal disk is assumed to be it.
        out.insert("disk0".into());
    }
    out
}

fn mounted_volumes(whole: &str) -> Vec<String> {
    let Ok(v) = plist_json(&["list", "-plist", whole]) else { return vec![] };
    let mut out = vec![];
    for p in v.get("AllDisksAndPartitions").and_then(Value::as_array).into_iter().flatten() {
        let mp = s(p, "MountPoint");
        if !mp.is_empty() {
            out.push(mp);
        }
        for q in p.get("Partitions").and_then(Value::as_array).into_iter().flatten() {
            let mp = s(q, "MountPoint");
            if !mp.is_empty() {
                out.push(mp);
            }
        }
    }
    out
}

/// `diskutil info` of `dev` (`disk4`, `/dev/disk4`, `/dev/rdisk4`), with the policy applied.
pub fn disk_info(dev: &str) -> ForgeResult<DiskInfo> {
    let bsd = bsd_name(dev);
    let v = plist_json(&["info", "-plist", &bsd])?;
    let protocol = s(&v, "BusProtocol");
    let mut info = DiskInfo {
        bsd: bsd.clone(),
        media_name: s(&v, "MediaName"),
        size: v.get("Size").or_else(|| v.get("TotalSize")).and_then(Value::as_u64).unwrap_or(0),
        block_size: v.get("DeviceBlockSize").and_then(Value::as_u64).unwrap_or(512) as u32,
        internal: b(&v, "Internal"),
        removable: b(&v, "RemovableMedia") || b(&v, "Removable"),
        ejectable: b(&v, "Ejectable"),
        whole: b(&v, "WholeDisk"),
        writable: b(&v, "WritableMedia") || b(&v, "Writable"),
        disk_image: protocol == "Disk Image",
        protocol,
        boot: boot_disks().contains(&bsd),
        mounted: mounted_volumes(&bsd),
        allowed: false,
        refusal: None,
    };
    // APFS containers and other synthesized disks are never a destination.
    let synthesized = v.get("APFSPhysicalStores").is_some() || s(&v, "VirtualOrPhysical") == "Virtual" && !info.disk_image;
    let verdict = if synthesized { Err(ForgeError::Unsafe(t("disco sintetizzato (contenitore APFS): scegli il disco fisico", "synthesized disk (APFS container): choose the physical disk"))) } else { check_policy(&info) };
    match verdict {
        Ok(()) => info.allowed = true,
        Err(e) => info.refusal = Some(e.to_string()),
    }
    Ok(info)
}

/// See the module documentation.
pub fn check_policy(d: &DiskInfo) -> ForgeResult<()> {
    let refuse = |it: &str, en: &str| Err(ForgeError::Unsafe(t(it, en)));
    if !d.whole {
        return refuse("scegli un disco intero, non una partizione", "choose a whole disk, not a partition");
    }
    if d.boot {
        return refuse("è il disco di avvio di questo Mac", "it is this Mac's startup disk");
    }
    if super::virtual_only() && !d.disk_image {
        return refuse(
            "modalità di prova: sono ammesse solo immagini disco collegate",
            "test mode: only attached disk images are allowed",
        );
    }
    if !d.writable {
        return refuse("il supporto è protetto da scrittura", "the media is write-protected");
    }
    let external_media = d.disk_image || d.removable || d.ejectable || !d.internal;
    if !external_media {
        return refuse("è un disco interno del Mac", "it is an internal disk of this Mac");
    }
    Ok(())
}

/// Every whole disk, each with its verdict. The UI lists the allowed ones first.
pub fn list_disks() -> ForgeResult<Vec<DiskInfo>> {
    let v = plist_json(&["list", "-plist"])?;
    let mut out = vec![];
    for d in v.get("WholeDisks").and_then(Value::as_array).into_iter().flatten() {
        if let Some(name) = d.as_str() {
            if let Ok(i) = disk_info(name) {
                out.push(i);
            }
        }
    }
    out.sort_by_key(|d| (!d.allowed, d.bsd.clone()));
    Ok(out)
}

static CLAIMED: once_cell::sync::Lazy<Mutex<HashSet<String>>> = once_cell::sync::Lazy::new(|| Mutex::new(HashSet::new()));

/// One job at a time per disk: held for the whole operation, released on drop.
pub struct Claim(String);

impl Drop for Claim {
    fn drop(&mut self) {
        CLAIMED.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

pub fn claim(key: &str) -> ForgeResult<Claim> {
    let mut c = CLAIMED.lock().unwrap_or_else(|e| e.into_inner());
    if !c.insert(key.to_string()) {
        return Err(ForgeError::Unsafe(t("un'altra operazione sta già usando questo disco", "another operation is already using this disk")));
    }
    Ok(Claim(key.to_string()))
}

/// Unmount every volume of a whole disk (`diskutil unmountDisk`). With `force`, open files
/// don't prevent it: only after the user confirmed.
pub fn unmount_disk(bsd: &str, force: bool) -> ForgeResult<()> {
    let bsd = bsd_name(bsd);
    let mut args = vec!["unmountDisk"];
    if force {
        args.push("force");
    }
    let dev = format!("/dev/{bsd}");
    args.push(&dev);
    let out = Command::new("/usr/sbin/diskutil").args(&args).output()?;
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string() + String::from_utf8_lossy(&out.stdout).trim();
    Err(ForgeError::Unsafe(t(
        &format!("impossibile smontare {bsd}: un'app sta usando i suoi file ({msg})"),
        &format!("could not unmount {bsd}: an app is using its files ({msg})"),
    )))
}

/// Eject a whole disk (`diskutil eject`), so it can be unplugged.
pub fn eject(bsd: &str) -> ForgeResult<()> {
    let out = Command::new("/usr/sbin/diskutil").args(["eject", &format!("/dev/{}", bsd_name(bsd))]).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ForgeError::Io(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}

/// Ask macOS to look at the disk again and mount what it recognises (after writing it).
pub fn mount_disk(bsd: &str) -> ForgeResult<()> {
    let out = Command::new("/usr/sbin/diskutil").args(["mountDisk", &format!("/dev/{}", bsd_name(bsd))]).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ForgeError::Io(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}

/// Where a job writes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetSpec {
    /// A whole disk (`disk4`).
    Disk { bsd: String },
    /// A regular file, created or truncated to `size` (writing an image instead of a disk).
    File { path: PathBuf, size: Option<u64> },
}

/// An open destination. Holds the claim for as long as it lives.
pub struct OpenTarget {
    pub dev: Arc<dyn BlockDev>,
    /// Some for disks.
    pub info: Option<DiskInfo>,
    pub path: String,
    _claim: Claim,
}

impl OpenTarget {
    pub fn size(&self) -> u64 {
        self.dev.size()
    }
}

/// Check, claim, unmount and open a destination for writing. Every destructive job starts here.
pub fn open_for_write(spec: &TargetSpec, ctx: &super::job::JobCtx) -> ForgeResult<OpenTarget> {
    match spec {
        TargetSpec::Disk { bsd } => {
            let info = disk_info(bsd)?;
            if let Some(r) = &info.refusal {
                return Err(ForgeError::Unsafe(r.clone()));
            }
            let claim = claim(&info.bsd)?;
            ctx.phase(super::job::Phase::Unmounting, 0);
            if !info.mounted.is_empty() {
                ctx.info(format!("unmounting {}", info.mounted.join(", ")));
            }
            unmount_disk(&info.bsd, false)?;
            let path = format!("/dev/r{}", info.bsd);
            let dev = crate::block::open_source(&path, false, None)?;
            if dev.size() == 0 {
                return Err(ForgeError::Io(t("il disco non riporta una dimensione", "the disk reports no size")));
            }
            ctx.info(format!("{} ({}, {} bytes) opened for writing", info.bsd, info.media_name, dev.size()));
            Ok(OpenTarget { dev, info: Some(info), path, _claim: claim })
        }
        TargetSpec::File { path, size } => {
            let claim = claim(&path.to_string_lossy())?;
            let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(size.is_some()).open(path)?;
            if let Some(sz) = size {
                f.set_len(*sz)?;
            }
            drop(f);
            let dev = crate::block::open_source(&path.to_string_lossy(), false, None)?;
            Ok(OpenTarget { dev, info: None, path: path.to_string_lossy().into_owned(), _claim: claim })
        }
    }
}

/// Tests: attach `image` as a disk with `hdiutil attach -nomount` and return `diskN`.
/// The caller detaches it with [`detach`]. Only ever used on images the test created.
pub fn attach_image_for_tests(image: &Path) -> ForgeResult<String> {
    let out = Command::new("/usr/bin/hdiutil")
        .args(["attach", "-nomount", "-imagekey", "diskimage-class=CRawDiskImage"])
        .arg(image)
        .output()?;
    if !out.status.success() {
        return Err(ForgeError::Io(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.split_whitespace().next().unwrap_or_default();
    Ok(bsd_name(first))
}

pub fn detach(bsd: &str) -> ForgeResult<()> {
    let out = Command::new("/usr/bin/hdiutil").args(["detach", &format!("/dev/{}", bsd_name(bsd))]).output()?;
    if out.status.success() {
        return Ok(());
    }
    let out = Command::new("/usr/bin/hdiutil").args(["detach", "-force", &format!("/dev/{}", bsd_name(bsd))]).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(ForgeError::Io(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb() -> DiskInfo {
        DiskInfo { bsd: "disk9".into(), whole: true, writable: true, removable: true, ejectable: true, protocol: "USB".into(), ..Default::default() }
    }

    #[test]
    fn names_are_normalised() {
        assert_eq!(bsd_name("/dev/rdisk4"), "disk4");
        assert_eq!(bsd_name("/dev/disk4s1"), "disk4s1");
        assert_eq!(bsd_name("disk12"), "disk12");
        assert_eq!(whole_of("disk3s1s1"), "disk3");
        assert_eq!(whole_of("disk12s2"), "disk12");
    }

    #[test]
    fn policy_refuses_everything_but_external_media_and_images() {
        // Under cfg(test) only disk images pass, whatever else is true.
        assert!(check_policy(&usb()).is_err(), "tests never accept a physical disk");
        let img = DiskInfo { disk_image: true, protocol: "Disk Image".into(), internal: true, removable: false, ejectable: true, ..usb() };
        assert!(check_policy(&img).is_ok());
        assert!(check_policy(&DiskInfo { whole: false, ..img.clone() }).is_err(), "partitions");
        assert!(check_policy(&DiskInfo { boot: true, ..img.clone() }).is_err(), "startup disk");
        assert!(check_policy(&DiskInfo { writable: false, ..img.clone() }).is_err(), "write-protected");
    }

    #[test]
    fn the_startup_disk_is_found_and_refused() {
        let boot = boot_disks();
        assert!(!boot.is_empty());
        for d in &boot {
            let i = disk_info(d).unwrap();
            assert!(!i.allowed, "{d} must never be a destination");
        }
    }

    #[test]
    fn one_job_per_disk() {
        let a = claim("test-disk-x").unwrap();
        assert!(claim("test-disk-x").is_err());
        drop(a);
        assert!(claim("test-disk-x").is_ok());
    }
}
