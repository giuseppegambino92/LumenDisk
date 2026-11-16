//! Random access to a compressed stream (gzip, xz, bzip2, zstd, deflated ZIP entry): the stream
//! is decompressed *forward, on demand* into an anonymous temporary file in the user's temp
//! directory, and reads are served from it. Reading the first megabytes (partition table, ISO
//! descriptors, a boot partition) only decompresses that far; [`LazyStreamDev::materialize`]
//! decompresses everything (ISO / Windows modes, which read the whole tree).
//!
//! The temporary file is unlinked as soon as it is created: it disappears when the device is
//! dropped, and even if the process dies.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::block::BlockDev;

struct State {
    stream: Box<dyn Read + Send>,
    temp: File,
    produced: u64,
    eof: bool,
}

pub struct LazyStreamDev {
    st: Mutex<State>,
    /// Exact size once known (from the container, or after reaching the end).
    size: AtomicU64,
    exact: AtomicU64,
    /// At most this many bytes are decompressed (quick inspection); None = no limit.
    budget: Option<u64>,
    pub temp_dir: PathBuf,
}

/// Bytes available on the filesystem holding `dir`.
pub fn free_space(dir: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: statvfs fills the zeroed struct for a valid NUL-terminated path.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return None;
        }
        Some(s.f_bavail as u64 * s.f_frsize as u64)
    }
}

/// Provisional size reported while the real one is unknown (reads past the end of the stream
/// fail with UnexpectedEof).
const UNKNOWN: u64 = 1 << 50;

impl LazyStreamDev {
    pub fn new(stream: Box<dyn Read + Send>, size: Option<u64>, budget: Option<u64>) -> io::Result<Self> {
        let temp_dir = std::env::temp_dir();
        if let (Some(sz), Some(free)) = (size, free_space(&temp_dir)) {
            if budget.is_none() && sz > free {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    crate::i18n::t(
                        &format!("spazio insufficiente in {} per decomprimere l'immagine ({} MB necessari, {} MB liberi)", temp_dir.display(), sz >> 20, free >> 20),
                        &format!("not enough space in {} to decompress the image ({} MB needed, {} MB free)", temp_dir.display(), sz >> 20, free >> 20),
                    ),
                ));
            }
        }
        let path = temp_dir.join(format!("lumendisk-image-{}.tmp", uuid::Uuid::new_v4().simple()));
        let temp = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
        let _ = std::fs::remove_file(&path);
        Ok(LazyStreamDev {
            st: Mutex::new(State { stream, temp, produced: 0, eof: false }),
            size: AtomicU64::new(size.unwrap_or(UNKNOWN)),
            exact: AtomicU64::new(size.is_some() as u64),
            budget,
            temp_dir,
        })
    }

    pub fn size_known(&self) -> bool {
        self.exact.load(Ordering::Relaxed) != 0
    }

    /// Decompress until `upto` bytes are available (or the stream ends).
    fn ensure(&self, st: &mut State, upto: u64, progress: &mut dyn FnMut(u64) -> io::Result<()>) -> io::Result<()> {
        if let Some(b) = self.budget {
            if upto > b && !st.eof {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, format!("would need decompressing more than {} MB", b >> 20)));
            }
        }
        let mut buf = vec![0u8; 1 << 20];
        while st.produced < upto && !st.eof {
            let n = st.stream.read(&mut buf)?;
            if n == 0 {
                st.eof = true;
                let known = self.size.load(Ordering::Relaxed);
                if self.size_known() && known != st.produced {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("the image ends after {} of {} bytes", st.produced, known)));
                }
                self.size.store(st.produced, Ordering::Relaxed);
                self.exact.store(1, Ordering::Relaxed);
                break;
            }
            if self.size_known() && st.produced + n as u64 > self.size.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "the image is longer than its header says"));
            }
            st.temp.write_all_at(&buf[..n], st.produced)?;
            st.produced += n as u64;
            progress(st.produced)?;
        }
        Ok(())
    }

    /// Decompress the whole stream; returns the final size.
    pub fn materialize(&self, progress: &mut dyn FnMut(u64) -> io::Result<()>) -> io::Result<u64> {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        self.ensure(&mut st, u64::MAX, progress)?;
        Ok(st.produced)
    }
}

impl BlockDev for LazyStreamDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset.checked_add(buf.len() as u64).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "offset"))?;
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        self.ensure(&mut st, end, &mut |_| Ok(()))?;
        if end > st.produced {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("read beyond the end of the image ({end} > {})", st.produced)));
        }
        st.temp.read_exact_at(buf, offset)
    }
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "source image is read-only"))
    }
    fn size(&self) -> u64 {
        self.size.load(Ordering::Relaxed)
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    fn read_only(&self) -> bool {
        true
    }
}
