//! On-disk storage of persistent Shadow sessions (see `shadow.rs`).
//!
//! One directory per volume: `<support>/Shadow/<key>/`
//!
//! ```text
//! manifest.json   identity of the volume, timestamps, state (pending | committing), change
//!                 summary, lower-volume fingerprint, commit progress (advisory copy)
//! stage.bin       64 KiB content chunks of every staged file (append / copy-on-write)
//! journal.ldj     append-only operation journal: frames of CRC-protected JSON batches
//! plan.json       commit plan (written once when a commit starts)
//! progress.ldj    append-only commit checkpoints (frames, same format as the journal)
//! ```
//!
//! Frame layout (little endian): `"LDSJ" | payload_len u32 | crc32c(payload) u32 | payload`.
//! A frame whose header or CRC doesn't check out ends the log (torn tail after a crash); it is
//! cut off when the log is reopened for writing.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub const FORMAT_VERSION: u32 = 1;
const FRAME_MAGIC: &[u8; 4] = b"LDSJ";
const FRAME_HDR: u64 = 12;
const MAX_FRAME: u64 = 256 << 20;

pub const MANIFEST: &str = "manifest.json";
pub const STAGE: &str = "stage.bin";
pub const JOURNAL: &str = "journal.ldj";
pub const PLAN: &str = "plan.json";
pub const PROGRESS: &str = "progress.ldj";

fn crc32c(data: &[u8]) -> u32 {
    !crate::vfs::ext4_fs::crc32c_raw(!0, data)
}

/// Make a file durable, including the drive's write cache (`F_FULLFSYNC` on macOS).
pub fn full_sync(f: &File) -> io::Result<()> {
    // Unit tests simulate crashes in-process (the page cache survives them): skip the cache
    // flush so the exhaustive crash suites stay fast.
    if cfg!(test) {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: plain fcntl on a valid descriptor.
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
    }
    f.sync_all()
}

/// Ordering barrier: everything written to `f` so far reaches stable storage before anything
/// written (to any file of the same device) after this call returns (`F_BARRIERFSYNC` on
/// macOS). It does not by itself make the bytes durable — a later [`full_sync`] of any file on
/// the device does, because `F_FULLFSYNC` flushes the whole drive cache — but it costs a
/// fraction of it. Falls back to a full sync where barriers are unsupported.
pub fn barrier_sync(f: &File) -> io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: plain fcntl on a valid descriptor.
        if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_BARRIERFSYNC) } == 0 {
            return Ok(());
        }
    }
    full_sync(f)
}

/// fsync a directory so that creates / renames / unlinks inside it are durable.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    let d = File::open(dir)?;
    // F_FULLFSYNC is not supported on directories everywhere; a plain fsync orders the entry.
    // SAFETY: plain fsync on a valid descriptor.
    unsafe { libc::fsync(std::os::fd::AsRawFd::as_raw_fd(&d)) };
    Ok(())
}

/// Write `data` to `path` atomically (temp file + full sync + rename + dir sync).
pub fn write_atomic(path: &Path, data: &[u8], durable: bool) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let f = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        f.write_all_at(data, 0)?;
        if durable {
            full_sync(&f)?;
        }
    }
    std::fs::rename(&tmp, path)?;
    if durable {
        if let Some(d) = path.parent() {
            sync_dir(d)?;
        }
    }
    Ok(())
}

/// Append-only log of CRC-protected frames.
pub struct RecordLog {
    file: File,
    len: u64,
}

impl RecordLog {
    /// Open (creating if needed) and read every intact frame. A torn or corrupt tail is
    /// truncated away so that new frames follow the last good one.
    pub fn open(path: &Path) -> io::Result<(RecordLog, Vec<Vec<u8>>)> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let (frames, good) = Self::scan(&file)?;
        if file.metadata()?.len() != good {
            file.set_len(good)?;
            full_sync(&file)?;
        }
        Ok((RecordLog { file, len: good }, frames))
    }

    /// Read the intact frames of a log without modifying it.
    pub fn read(path: &Path) -> io::Result<Vec<Vec<u8>>> {
        let file = File::open(path)?;
        Ok(Self::scan(&file)?.0)
    }

    fn scan(file: &File) -> io::Result<(Vec<Vec<u8>>, u64)> {
        let size = file.metadata()?.len();
        let mut out = vec![];
        let mut off = 0u64;
        while off + FRAME_HDR <= size {
            let mut h = [0u8; FRAME_HDR as usize];
            file.read_exact_at(&mut h, off)?;
            if &h[..4] != FRAME_MAGIC {
                break;
            }
            let len = u32::from_le_bytes(h[4..8].try_into().unwrap()) as u64;
            let crc = u32::from_le_bytes(h[8..12].try_into().unwrap());
            if len > MAX_FRAME || off + FRAME_HDR + len > size {
                break;
            }
            let mut p = vec![0u8; len as usize];
            file.read_exact_at(&mut p, off + FRAME_HDR)?;
            if crc32c(&p) != crc {
                break;
            }
            out.push(p);
            off += FRAME_HDR + len;
        }
        Ok((out, off))
    }

    /// Append one frame (not yet durable: call [`RecordLog::sync`]).
    pub fn append(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut f = Vec::with_capacity(FRAME_HDR as usize + payload.len());
        f.extend_from_slice(FRAME_MAGIC);
        f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        f.extend_from_slice(&crc32c(payload).to_le_bytes());
        f.extend_from_slice(payload);
        if let Err(e) = self.file.write_all_at(&f, self.len) {
            // Never leave a half-written frame in front of the next one.
            let _ = self.file.set_len(self.len);
            return Err(e);
        }
        self.len += f.len() as u64;
        Ok(())
    }

    pub fn sync(&self) -> io::Result<()> {
        full_sync(&self.file)
    }

    /// A second descriptor of the log, to sync it without holding whatever owns the log.
    pub fn clone_file(&self) -> io::Result<File> {
        self.file.try_clone()
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Change counters kept in the manifest (advisory: refreshed every few seconds).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Counts {
    pub changes: u64,
    pub added: u64,
    pub modified: u64,
    pub deleted: u64,
    pub pending_bytes: i64,
}

/// Identity of the volume a session belongs to.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Identity {
    /// "local" (image file or device) or "image_url".
    pub kind: String,
    pub fs_type: String,
    pub uuid: String,
    pub label: String,
    pub source_path: String,
    pub partition: u32,
    /// Filesystem size in bytes (statfs total).
    pub size: u64,
    #[serde(default)]
    pub media_uuid: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub key: String,
    pub identity: Identity,
    pub created: i64,
    pub updated: i64,
    /// "pending" or "committing".
    pub state: String,
    /// Fingerprint of the lower volume when the session started (None: the driver has none).
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub counts: Counts,
    #[serde(default)]
    pub commit_applied: u64,
    #[serde(default)]
    pub commit_total: u64,
    #[serde(default)]
    pub commit_error: Option<String>,
    /// Set when a recovery found the disk modified elsewhere: only Discard is possible.
    #[serde(default)]
    pub drift: Option<String>,
}

impl Manifest {
    pub fn load(dir: &Path) -> Option<Manifest> {
        let s = std::fs::read(dir.join(MANIFEST)).ok()?;
        let m: Manifest = serde_json::from_slice(&s).ok()?;
        (m.format == FORMAT_VERSION).then_some(m)
    }

    pub fn save(&self, dir: &Path, durable: bool) -> io::Result<()> {
        let s = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_atomic(&dir.join(MANIFEST), &s, durable)
    }
}

/// Steps of an interrupted commit known to be applied (last checkpoint in progress.ldj).
pub fn checkpointed_steps(dir: &Path) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct C {
        step: u64,
    }
    let frames = RecordLog::read(&dir.join(PROGRESS)).ok()?;
    frames.iter().filter_map(|f| serde_json::from_slice::<C>(f).ok()).map(|c| c.step).max()
}

/// Directory holding every session.
pub fn sessions_root(support: &Path) -> PathBuf {
    support.join("Shadow")
}

/// Stable session key of a volume. Volumes with a filesystem UUID / serial are keyed by it
/// (plus type and size, so that two cards flashed from the same image don't collide unless
/// they are identical); volumes without one by source path, partition, label and size.
pub fn session_key(id: &Identity) -> String {
    use sha2::Digest;
    let basis = if id.kind == "image_url" {
        format!("url|{}|{}", id.source_path, id.partition)
    } else if !id.uuid.trim().is_empty() {
        format!("local|{}|{}|{}", id.fs_type, id.uuid, id.size)
    } else {
        format!("local|{}|{}|{}|{}|{}", id.fs_type, id.source_path, id.partition, id.label, id.size)
    };
    let d = sha2::Sha256::digest(basis.as_bytes());
    let hex: String = d[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{hex}", id.fs_type.to_ascii_lowercase())
}

/// Remove a session directory: renamed away first (one atomic step), then deleted.
pub fn remove_session_dir(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let trash = dir.with_file_name(format!(
        ".trash-{}-{}",
        dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::rename(dir, &trash)?;
    if let Some(p) = dir.parent() {
        let _ = sync_dir(p);
    }
    std::fs::remove_dir_all(&trash)
}

/// Leftovers of interrupted deletions.
pub fn clean_trash(root: &Path) {
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(".trash-") {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

/// Every session directory with a valid manifest.
pub fn list_sessions(support: &Path) -> Vec<(PathBuf, Manifest)> {
    let root = sessions_root(support);
    clean_trash(&root);
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() || e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if let Some(m) = Manifest::load(&p) {
                out.push((p, m));
            }
        }
    }
    out.sort_by(|a, b| b.1.updated.cmp(&a.1.updated));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_log_ignores_torn_tail() {
        let dir = std::env::temp_dir().join(format!("ld-rlog-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("j.ldj");
        {
            let (mut l, f) = RecordLog::open(&p).unwrap();
            assert!(f.is_empty());
            for i in 0..5u8 {
                l.append(&vec![i; 100 + i as usize]).unwrap();
            }
            l.sync().unwrap();
        }
        // tear the last frame
        let len = std::fs::metadata(&p).unwrap().len();
        OpenOptions::new().write(true).open(&p).unwrap().set_len(len - 7).unwrap();
        let (mut l, f) = RecordLog::open(&p).unwrap();
        assert_eq!(f.len(), 4);
        assert_eq!(f[3], vec![3u8; 103]);
        l.append(b"after").unwrap();
        drop(l);
        // garbage in the middle of a frame also ends the log
        let f = RecordLog::read(&p).unwrap();
        assert_eq!(f.len(), 5);
        assert_eq!(f[4], b"after");
        let mut raw = std::fs::read(&p).unwrap();
        raw[12 + 50] ^= 0xFF;
        std::fs::write(&p, &raw).unwrap();
        assert!(RecordLog::read(&p).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_are_stable_and_distinct() {
        let a = Identity { kind: "local".into(), fs_type: "EXT4".into(), uuid: "u-1".into(), size: 100, ..Default::default() };
        let mut b = a.clone();
        b.source_path = "/dev/disk9s1".into(); // the device node doesn't matter when there is a UUID
        assert_eq!(session_key(&a), session_key(&b));
        b.size = 200;
        assert_ne!(session_key(&a), session_key(&b));
        let n = Identity { kind: "local".into(), fs_type: "FAT16".into(), source_path: "/x.img".into(), partition: 1, ..Default::default() };
        let mut m = n.clone();
        m.partition = 2;
        assert_ne!(session_key(&n), session_key(&m));
        assert!(session_key(&n).starts_with("fat16-"));
    }
}
