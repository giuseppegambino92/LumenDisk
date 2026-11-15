//! Remote storage: SFTP / WebDAV / LumenDisk-LAN backends behind the same loopback NFSv3
//! pipeline (Ghost Shield, Shadow), with a local metadata + read-ahead cache and an offline
//! write-back queue so Finder never blocks on the network ("zero beachball").
//!
//! * [`RemoteBackend`] — path-based protocol adapters (sftp.rs, webdav.rs).
//! * [`cache::RemoteVfs`] — turns a backend into a [`crate::vfs::FileSystem`].

use std::time::Duration;

use crate::vfs::{FileKind, Timestamp};

pub mod cache;
pub mod http;
pub mod sftp;
pub mod webdav;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteEntry {
    /// Last path component ("" for the root).
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    pub mtime: Timestamp,
    /// Permission bits (0o7777); backends without permissions report 0o644 / 0o755.
    pub perm: u16,
    /// Symlink target, when `kind == Symlink` and the backend reports it cheaply.
    pub symlink_target: Option<String>,
    /// Opaque content version reported by the server (WebDAV `getetag`, weak/strong marker
    /// stripped), when it has one. Persistent Shadow sessions use it to notice that a file
    /// was changed on the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteError {
    NotFound,
    Exists,
    NotEmpty,
    NotDir,
    IsDir,
    Perm,
    /// The request did not complete within the backend's current timeout.
    Timeout,
    /// Connection lost / refused / broken pipe.
    Disconnected(String),
    Unsupported(String),
    Other(String),
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl RemoteError {
    /// Errors that mean "the network is the problem" (→ go offline, keep queued work).
    pub fn is_network(&self) -> bool {
        matches!(self, RemoteError::Timeout | RemoteError::Disconnected(_))
    }
}

pub type RResult<T> = Result<T, RemoteError>;

/// A remote file tree addressed by absolute, '/'-separated paths relative to the export root
/// ("/" is the root, "/a/b.txt" a file). All methods are blocking and thread-safe; each call
/// must give up after the timeout set with `set_timeout` and return `RemoteError::Timeout`
/// (never hang). Implementations must transparently reconnect on the next call after a
/// `Disconnected` error.
pub trait RemoteBackend: Send + Sync {
    /// Human readable location, e.g. "sftp://pi@raspberrypi.local:22/home/pi".
    fn describe(&self) -> String;
    /// Per-request timeout for metadata operations (data transfers may use a multiple).
    fn set_timeout(&self, t: Duration);
    /// Cheap round trip; returns the measured latency.
    fn ping(&self) -> RResult<Duration>;
    /// lstat semantics (does not follow symlinks).
    fn stat(&self, path: &str) -> RResult<RemoteEntry>;
    /// Directory listing without "." and "..".
    fn list(&self, path: &str) -> RResult<Vec<RemoteEntry>>;
    /// Read up to `len` bytes at `offset` (short read only at EOF).
    fn read(&self, path: &str, offset: u64, len: u32) -> RResult<Vec<u8>>;
    /// Whether `write` at arbitrary offsets is supported. If false, callers upload whole files
    /// with `put_all`.
    fn supports_partial_write(&self) -> bool;
    /// Write `data` at `offset` into an existing file (extending it if needed).
    fn write(&self, path: &str, offset: u64, data: &[u8]) -> RResult<()>;
    /// Replace the whole content of `path` (creating it if needed).
    fn put_all(&self, path: &str, data: &[u8]) -> RResult<()>;
    /// Create an empty regular file (fails with Exists if present).
    fn create(&self, path: &str, perm: u16) -> RResult<()>;
    fn truncate(&self, path: &str, size: u64) -> RResult<()>;
    fn mkdir(&self, path: &str, perm: u16) -> RResult<()>;
    fn remove(&self, path: &str) -> RResult<()>;
    fn rmdir(&self, path: &str) -> RResult<()>;
    /// Rename, replacing an existing destination file / empty directory.
    fn rename(&self, from: &str, to: &str) -> RResult<()>;
    fn symlink(&self, path: &str, target: &str) -> RResult<()>;
    fn readlink(&self, path: &str) -> RResult<String>;
    fn set_perm(&self, path: &str, perm: u16) -> RResult<()>;
    fn set_mtime(&self, path: &str, mtime: Timestamp) -> RResult<()>;
    /// (total, free) bytes if known.
    fn statfs(&self) -> RResult<Option<(u64, u64)>>;
}

/// Join a parent path and a name.
pub fn join(parent: &str, name: &str) -> String {
    if parent == "/" || parent.is_empty() {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// Parent path of an absolute path ("/a/b" → "/a", "/a" → "/").
pub fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}
