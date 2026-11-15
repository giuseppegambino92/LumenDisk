//! Disk Health & Speed (Phase 3).
//!
//! * [`benchmark`] measures real sequential read/write throughput and 4 KiB random-read
//!   latency **through the mounted LumenDisk volume** (≈3 s), bypassing the Mac's page cache
//!   with `F_NOCACHE`. It writes and then deletes one hidden temp file.
//! * [`disk_health`] reports hardware facts and the SMART status macOS exposes for a disk
//!   (`diskutil info`), plus a full `smartctl -j -a` report when smartmontools is installed.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BenchmarkResult {
    pub read_mbps: f64,
    pub write_mbps: f64,
    pub iops_4k: f64,
    pub latency_ms: f64,
    pub bytes_tested: u64,
    pub duration_ms: u64,
}

fn nocache(f: &File) {
    // SAFETY: plain fcntl on a valid descriptor.
    unsafe {
        libc::fcntl(f.as_raw_fd(), libc::F_NOCACHE, 1);
    }
}

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Run the speed test in `dir` (a mountpoint). Budget ≈ 1.2 s write + 1.0 s read + 0.8 s random.
pub fn benchmark(dir: &Path) -> Result<BenchmarkResult, String> {
    let t_all = Instant::now();
    // Never fill the user's disk: use at most half of the free space (and ≤ 1 GiB).
    let free = {
        let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: statvfs with a valid path and out-param.
        let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut sv) } == 0 {
            sv.f_bavail as u64 * sv.f_frsize as u64
        } else {
            u64::MAX
        }
    };
    let budget = (free / 2).min(1 << 30);
    if budget < 8 << 20 {
        return Err(crate::i18n::t(
            "spazio libero insufficiente per il test di velocità (servono almeno 16 MB liberi)",
            "not enough free space for the speed test (at least 16 MB must be free)",
        ));
    }
    let name = format!(".lumendisk-speedtest-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let path = dir.join(name);
    let res = (|| -> std::io::Result<BenchmarkResult> {
        const BLOCK: usize = 1 << 20;
        let mut buf = vec![0u8; BLOCK];
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for chunk in buf.chunks_mut(8) {
            let v = xorshift(&mut seed).to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
        // ── sequential write ──
        let mut f = OpenOptions::new().create_new(true).write(true).read(true).open(&path)?;
        nocache(&f);
        let t0 = Instant::now();
        let mut written: u64 = 0;
        let cap = budget.min(1024 * BLOCK as u64);
        let min_bytes = (16 * BLOCK as u64).min(cap);
        while (t0.elapsed() < Duration::from_millis(1200) || written < min_bytes) && written + BLOCK as u64 <= cap {
            buf[0] = buf[0].wrapping_add(1); // defeat dedup / compression shortcuts
            f.write_all(&buf)?;
            written += BLOCK as u64;
        }
        f.sync_all()?;
        let write_s = t0.elapsed().as_secs_f64();
        // ── sequential read ──
        f.seek(SeekFrom::Start(0))?;
        let t0 = Instant::now();
        let mut read: u64 = 0;
        while t0.elapsed() < Duration::from_millis(1000) && read < written {
            let n = f.read(&mut buf)?;
            if n == 0 {
                f.seek(SeekFrom::Start(0))?;
                continue;
            }
            read += n as u64;
        }
        let read_s = t0.elapsed().as_secs_f64();
        // ── 4 KiB random reads ──
        let blocks4k = (written / 4096).max(1);
        let mut small = vec![0u8; 4096];
        let t0 = Instant::now();
        let mut ops: u64 = 0;
        while t0.elapsed() < Duration::from_millis(800) {
            let off = (xorshift(&mut seed) % blocks4k) * 4096;
            f.read_exact_at(&mut small, off)?;
            ops += 1;
        }
        let rand_s = t0.elapsed().as_secs_f64();
        drop(f);
        let mb = 1024.0 * 1024.0;
        Ok(BenchmarkResult {
            write_mbps: written as f64 / mb / write_s.max(1e-6),
            read_mbps: read as f64 / mb / read_s.max(1e-6),
            iops_4k: ops as f64 / rand_s.max(1e-6),
            latency_ms: rand_s * 1000.0 / (ops.max(1) as f64),
            bytes_tested: written + read + ops * 4096,
            duration_ms: 0,
        })
    })();
    let _ = std::fs::remove_file(&path);
    let mut r = res.map_err(|e| crate::i18n::t(&format!("test di velocità non riuscito: {e}"), &format!("speed test failed: {e}")))?;
    r.duration_ms = t_all.elapsed().as_millis() as u64;
    Ok(r)
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct DiskHealth {
    pub bsd_name: String,
    pub media_name: Option<String>,
    pub protocol: Option<String>,
    pub solid_state: Option<bool>,
    pub smart_status: Option<String>,
    pub size: Option<u64>,
    pub removable: Option<bool>,
    pub internal: Option<bool>,
    pub location: Option<String>,
    pub smartctl: Option<serde_json::Value>,
}

fn plist_json(args: &[&str]) -> Option<serde_json::Value> {
    let out = Command::new("/usr/sbin/diskutil").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let mut child = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(&out.stdout).ok()?;
    let mut s = String::new();
    child.stdout.take()?.read_to_string(&mut s).ok()?;
    let _ = child.wait();
    serde_json::from_str(&s).ok()
}

/// Hardware / SMART report for `bsd` ("disk4" or "disk4s1"; "/dev/" prefix accepted).
pub fn disk_health(bsd: &str) -> Result<DiskHealth, String> {
    let bsd = bsd.trim_start_matches("/dev/").trim_start_matches('r');
    if !bsd.starts_with("disk") || !bsd[4..].chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("nome dispositivo non valido: {bsd}"));
    }
    let v = plist_json(&["info", "-plist", bsd]).ok_or_else(|| format!("{bsd}: dispositivo non trovato"))?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool());
    let whole = s("ParentWholeDisk").unwrap_or_else(|| bsd.to_string());
    // SMART is reported on the whole disk.
    let wv = if whole != bsd { plist_json(&["info", "-plist", &whole]) } else { None };
    let ws = |k: &str| wv.as_ref().and_then(|w| w.get(k)).and_then(|x| x.as_str()).map(|x| x.to_string());
    let mut h = DiskHealth {
        bsd_name: bsd.to_string(),
        media_name: ws("MediaName").or_else(|| s("MediaName")).or_else(|| s("IORegistryEntryName")),
        protocol: s("BusProtocol"),
        solid_state: b("SolidState"),
        smart_status: ws("SMARTStatus").or_else(|| s("SMARTStatus")),
        size: v.get("Size").or_else(|| v.get("TotalSize")).and_then(|x| x.as_u64()),
        removable: b("RemovableMediaOrExternalDevice").or_else(|| b("Removable")),
        internal: b("Internal"),
        location: s("DeviceLocation"),
        smartctl: None,
    };
    for p in ["/opt/homebrew/bin/smartctl", "/usr/local/bin/smartctl"] {
        if Path::new(p).exists() {
            if let Ok(o) = Command::new(p).args(["-j", "-a", &format!("/dev/{whole}")]).output() {
                if let Ok(j) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                    // Only keep it when smartctl actually reached the device.
                    if j.get("smart_status").is_some() || j.get("model_name").is_some() {
                        if h.smart_status.is_none() || h.smart_status.as_deref() == Some("Not Supported") {
                            if let Some(passed) = j.pointer("/smart_status/passed").and_then(|x| x.as_bool()) {
                                h.smart_status = Some(if passed { "Verified".into() } else { "Failing".into() });
                            }
                        }
                        h.smartctl = Some(j);
                    }
                }
            }
            break;
        }
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benchmark_on_a_temp_dir() {
        let d = std::env::temp_dir().join(format!("ld-bench-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let r = benchmark(&d).unwrap();
        assert!(r.write_mbps > 1.0 && r.read_mbps > 1.0 && r.iops_4k > 10.0, "{r:?}");
        assert!(r.duration_ms < 8000, "{r:?}");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0, "temp file must be removed");
        let _ = std::fs::remove_dir(&d);
    }

    #[test]
    fn health_of_the_boot_disk() {
        let h = disk_health("disk0").unwrap();
        assert_eq!(h.bsd_name, "disk0");
        assert!(h.protocol.is_some(), "{h:?}");
        assert!(disk_health("../etc").is_err());
    }
}
