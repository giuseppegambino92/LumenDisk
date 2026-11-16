//! Source images: raw IMG / ISO, gzip / xz / bzip2 / zstd / zip, VHD, DMG — opened as byte
//! streams or random-access devices, and [`inspect`]ed (type, size, OS, recommended write mode).
//!
//! Owner: the image / DD workstream.
//!
//! - [`Image::open`] recognises the format by its magic bytes (never by the extension) and
//!   parses what can be parsed cheaply: the XZ index (exact size), the ZIP central directory,
//!   the VHD footer / BAT, the DMG block tables.
//! - [`Image::pipe`] decodes the image on a thread of its own, ahead of the consumer, in
//!   fixed-size chunks through a bounded channel (DD writing, hashing).
//! - [`Image::device`] gives random access: natively for raw, VHD, DMG and stored ZIP entries;
//!   for compressed streams, by decompressing forward into an anonymous temporary file
//!   ([`lazy::LazyStreamDev`]) as far as reads require.
//!
//! Images are untrusted input: every size read from a header is checked against the file
//! before it is used, allocations are bounded, and third-party decoders run under
//! `catch_unwind` so a decoder bug on hostile data becomes an error, not a crash.

pub mod dmg;
pub mod isoinfo;
pub mod lazy;
pub mod vhd;
mod xz;
pub mod zip;

#[cfg(test)]
mod tests;

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use super::job::{JobCtx, Phase};
use super::plan::WriteMode;
use super::tree::SourceTree;
use super::{ForgeError, ForgeResult};
use crate::block::{BlockDev, FileDev, PartitionDev};
use crate::i18n::t;

pub use isoinfo::IsoInfo;

/// Container / compression of a source image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// IMG / ISO / BIN: the bytes are the disk.
    Raw,
    Gzip,
    Xz,
    Bzip2,
    Zstd,
    Zip,
    Vhd,
    Dmg,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Raw => "raw",
            Format::Gzip => "gzip",
            Format::Xz => "xz",
            Format::Bzip2 => "bzip2",
            Format::Zstd => "zstd",
            Format::Zip => "zip",
            Format::Vhd => "vhd",
            Format::Dmg => "dmg",
        }
    }
}

#[derive(Clone, Debug)]
enum Detail {
    None,
    Xz(Option<Arc<xz::XzLayout>>),
    Zip(zip::ZipEntry),
    Vhd(Arc<vhd::VhdInfo>),
    Dmg(Arc<dmg::DmgTable>),
}

/// An opened source image (cheap to clone: tables are shared).
#[derive(Clone, Debug)]
pub struct Image {
    pub path: PathBuf,
    pub format: Format,
    pub file_size: u64,
    /// Size of the disk image inside (decompressed), when known without decompressing.
    pub size: Option<u64>,
    /// A guess of the decompressed size (gzip trailer), only for progress estimates.
    pub size_hint: Option<u64>,
    detail: Detail,
}

/// Decompression / random-access chunk size of the pipe and of the flash buffers.
pub const CHUNK: usize = 4 << 20;
/// How much a quick inspection may decompress to look inside a compressed image.
pub const QUICK_BUDGET: u64 = 128 << 20;

fn unsupported(it: &str, en: &str) -> ForgeError {
    ForgeError::Unsupported(t(it, en))
}

fn zstd_magic(b: &[u8]) -> bool {
    b.len() >= 4 && (b[..4] == [0x28, 0xB5, 0x2F, 0xFD] || (b[1..4] == [0x2A, 0x4D, 0x18] && b[0] & 0xF0 == 0x50))
}

impl Image {
    /// Open `path` and recognise its format.
    pub fn open(path: impl AsRef<Path>) -> ForgeResult<Image> {
        let path = path.as_ref().to_path_buf();
        let f = File::open(&path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => ForgeError::Invalid(t(&format!("{}: file non trovato", path.display()), &format!("{}: file not found", path.display()))),
            _ => ForgeError::Io(format!("{}: {e}", path.display())),
        })?;
        let md = f.metadata()?;
        if !md.is_file() {
            return Err(ForgeError::Invalid(t(&format!("{} non è un file immagine", path.display()), &format!("{} is not an image file", path.display()))));
        }
        let file_size = md.len();
        let mut head = vec![0u8; file_size.min(64) as usize];
        f.read_exact_at(&mut head, 0)?;
        let mut img = Image { path, format: Format::Raw, file_size, size: Some(file_size), size_hint: None, detail: Detail::None };
        if head.len() >= 3 && head[..3] == [0x1F, 0x8B, 0x08] {
            img.format = Format::Gzip;
            img.size = None;
            // ISIZE: the size modulo 2^32 of the last member — a hint, never trusted.
            if file_size >= 18 {
                let mut tail = [0u8; 4];
                f.read_exact_at(&mut tail, file_size - 4)?;
                let isize = u64::from(u32::from_le_bytes(tail));
                img.size_hint = Some(if file_size < (1 << 32) && isize >= file_size / 2 { isize } else { file_size.saturating_mul(3) });
            }
        } else if head.len() >= 6 && head[..6] == [0xFD, b'7', b'z', b'X', b'Z', 0] {
            img.format = Format::Xz;
            let layout = xz::layout(&f, file_size).map_err(|e| ForgeError::Invalid(t(&format!("immagine XZ danneggiata: {e}"), &format!("damaged XZ image: {e}"))))?;
            img.size = layout.as_ref().map(|l| l.size);
            img.detail = Detail::Xz(layout.map(Arc::new));
        } else if head.len() >= 4 && &head[..3] == b"BZh" && (b'1'..=b'9').contains(&head[3]) {
            img.format = Format::Bzip2;
            img.size = None;
            img.size_hint = Some(file_size.saturating_mul(4));
        } else if zstd_magic(&head) {
            img.format = Format::Zstd;
            img.size = zstd_content_size(&f, file_size);
            img.size_hint = Some(file_size.saturating_mul(3));
        } else if head.len() >= 4 && (head[..4] == *b"PK\x03\x04" || head[..4] == *b"PK\x05\x06") {
            img.format = Format::Zip;
            let e = zip::find_image(&f, file_size)?;
            img.size = Some(e.size);
            img.detail = Detail::Zip(e);
        } else if head.len() >= 8 && &head[..8] == b"vhdxfile" {
            return Err(unsupported("VHDX non è supportato: convertilo in VHD o IMG (qemu-img convert)", "VHDX is not supported: convert it to VHD or IMG (qemu-img convert)"));
        } else if head.len() >= 4 && head[..4] == *b"QFI\xFB" {
            return Err(unsupported("le immagini QCOW2 non sono supportate: convertila in IMG (qemu-img convert)", "QCOW2 images are not supported: convert it to IMG (qemu-img convert)"));
        } else if head.len() >= 6 && head[..6] == [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C] {
            return Err(unsupported("archivio 7-Zip: estrai l'immagine e scegli quella", "7-Zip archive: extract the image and choose it"));
        } else if head.len() >= 4 && &head[..4] == b"Rar!" {
            return Err(unsupported("archivio RAR: estrai l'immagine e scegli quella", "RAR archive: extract the image and choose it"));
        } else if head.len() >= 8 && &head[..8] == b"encrcdsa" {
            return Err(unsupported("DMG cifrato: aprilo con Utility Disco e salvane una copia non cifrata", "encrypted DMG: open it with Disk Utility and save an unencrypted copy"));
        } else if head.len() >= 4 && &head[..4] == b"sprs" {
            return Err(unsupported("immagine sparse di macOS: convertila in DMG (UDZO / UDRO) con Utility Disco", "macOS sparse image: convert it to a DMG (UDZO / UDRO) with Disk Utility"));
        } else if vhd::probe(&f, file_size) {
            img.format = Format::Vhd;
            let info = vhd::open(&f, file_size)?;
            img.size = Some(info.size);
            img.detail = Detail::Vhd(Arc::new(info));
        } else if dmg::probe(&f, file_size) {
            img.format = Format::Dmg;
            let tab = dmg::open(&f, file_size)?;
            img.size = Some(tab.size);
            img.detail = Detail::Dmg(Arc::new(tab));
        }
        Ok(img)
    }

    /// The bytes of the file are not the bytes of the disk (compressed, or a container).
    pub fn is_container(&self) -> bool {
        self.format != Format::Raw
    }

    /// Compressed (the file is smaller than the disk it holds, in general).
    pub fn compressed(&self) -> bool {
        match &self.detail {
            Detail::Zip(e) => e.method != 0,
            Detail::Dmg(t) => t.compressed(),
            Detail::Vhd(_) => false,
            _ => !matches!(self.format, Format::Raw),
        }
    }

    /// Details of the container for the inspect report.
    pub fn describe(&self) -> Value {
        match &self.detail {
            Detail::Xz(Some(l)) => json!({"streams": l.streams.len(), "blocks": l.blocks, "parallel": l.parallel()}),
            Detail::Xz(None) => json!({"index": "too large to read"}),
            Detail::Zip(e) => json!({"entry": e.name, "method": e.method_name(), "compressed_size": e.compressed}),
            Detail::Vhd(v) => json!({"type": v.kind, "footer_copy_used": v.footer_from_copy}),
            Detail::Dmg(d) => json!({"variant": d.variant(), "chunk_types": d.kinds()}),
            Detail::None => Value::Null,
        }
    }

    /// Random access without decompressing anything, when the format allows it.
    pub fn native_device(&self) -> ForgeResult<Option<Arc<dyn BlockDev>>> {
        let file = || File::open(&self.path).map_err(ForgeError::from);
        Ok(Some(match &self.detail {
            Detail::None if self.format == Format::Raw => Arc::new(FileDev::open(&self.path, true)?),
            Detail::Vhd(v) => Arc::new(vhd::VhdDev::new(file()?, (**v).clone())),
            Detail::Dmg(d) => Arc::new(dmg::DmgDev::new(file()?, d.clone())),
            Detail::Zip(e) if e.method == 0 => {
                let base: Arc<dyn BlockDev> = Arc::new(FileDev::open(&self.path, true)?);
                Arc::new(PartitionDev::new(base, e.data_offset, e.size)?)
            }
            _ => return Ok(None),
        }))
    }

    /// Random access to the disk inside. Compressed streams are decompressed forward on demand
    /// into a temporary file (logged); with `full`, completely and up front (with progress in
    /// the current phase), as ISO / Windows modes need; with `budget`, never beyond that many
    /// bytes (quick inspection: reads further fail).
    pub fn device(&self, ctx: &JobCtx, full: bool, budget: Option<u64>) -> ForgeResult<Arc<dyn BlockDev>> {
        if let Some(d) = self.native_device()? {
            return Ok(d);
        }
        let pipe = self.pipe(1 << 20, 4);
        let dev = lazy::LazyStreamDev::new(Box::new(pipe), self.size, budget)?;
        if budget.is_none() {
            ctx.info(t(
                &format!("immagine {}: decompressione in un file temporaneo in {} per l'accesso casuale", self.format.name(), dev.temp_dir.display()),
                &format!("{} image: decompressing to a temporary file in {} for random access", self.format.name(), dev.temp_dir.display()),
            ));
        }
        if full {
            ctx.set_total(self.size.or(self.size_hint).unwrap_or(0));
            let total_known = self.size.is_some();
            let mut last = 0u64;
            dev.materialize(&mut |done| {
                ctx.advance(done - last).map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "cancelled"))?;
                last = done;
                if !total_known {
                    ctx.set_total(done.max(ctx.done()));
                }
                Ok(())
            })
            .map_err(|e| if ctx.is_cancelled() { ForgeError::Cancelled } else { damaged(self, e) })?;
        }
        Ok(Arc::new(dev))
    }

    /// Build the decoding reader in the current thread (decoders need not be `Send`).
    fn reader(&self, consumed: Arc<AtomicU64>) -> ForgeResult<Box<dyn Read>> {
        let file = File::open(&self.path)?;
        let counted = || -> ForgeResult<Counting<File>> { Ok(Counting { inner: File::open(&self.path)?, n: consumed.clone() }) };
        let buffered = |c: Counting<File>| BufReader::with_capacity(1 << 20, c);
        Ok(match (&self.detail, self.format) {
            (Detail::None, Format::Raw) => Box::new(counted()?.take(self.file_size)),
            (Detail::None, Format::Gzip) => Box::new(flate2::read::MultiGzDecoder::new(buffered(counted()?))),
            (Detail::None, Format::Bzip2) => Box::new(bzip2::read::MultiBzDecoder::new(buffered(counted()?))),
            (Detail::None, Format::Zstd) => Box::new(ZstdFrames::new(buffered(counted()?))),
            (Detail::Xz(Some(l)), _) if l.parallel() => {
                let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(2, 4);
                Box::new(xz::XzMtReader::new(file, l, workers, consumed))
            }
            (Detail::Xz(_), _) => Box::new(lzma_rust2::XzReader::new_mem_limit(buffered(counted()?), true, xz::MEM_LIMIT_KB)),
            (Detail::Zip(e), _) => {
                let mut c = counted()?;
                io::Seek::seek(&mut c.inner, io::SeekFrom::Start(e.data_offset))?;
                let raw = buffered(c).take(e.compressed);
                let dec: Box<dyn Read> = match e.method {
                    0 => Box::new(raw),
                    8 => Box::new(flate2::read::DeflateDecoder::new(raw)),
                    12 => Box::new(bzip2::read::MultiBzDecoder::new(raw)),
                    93 => Box::new(ZstdFrames::new(raw)),
                    95 => Box::new(lzma_rust2::XzReader::new_mem_limit(raw, true, xz::MEM_LIMIT_KB)),
                    m => return Err(ForgeError::Unsupported(format!("zip method {m}"))),
                };
                Box::new(zip::CrcCheck::new(dec, e.crc32, e.size))
            }
            (Detail::Vhd(_), _) | (Detail::Dmg(_), _) => {
                let dev = self.native_device()?.ok_or_else(|| ForgeError::Io("device".into()))?;
                Box::new(DevReader { dev, pos: 0, consumed })
            }
            _ => return Err(ForgeError::Io("unexpected image state".into())),
        })
    }

    /// Decode the image on a thread of its own, `depth` chunks of `chunk` bytes ahead.
    pub fn pipe(&self, chunk: usize, depth: usize) -> Pipe {
        let consumed = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = sync_channel::<io::Result<Vec<u8>>>(depth.max(1));
        let img = self.clone();
        let (c2, s2) = (consumed.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("lumendisk-image-reader".into()).spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> io::Result<()> {
                let mut rd = img.reader(c2).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                let mut produced = 0u64;
                loop {
                    if s2.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let mut buf = vec![0u8; chunk];
                    let mut n = 0;
                    while n < chunk {
                        match rd.read(&mut buf[n..]) {
                            Ok(0) => break,
                            Ok(k) => n += k,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(e) => return Err(io::Error::new(e.kind(), format!("{e} (after {} bytes)", produced + n as u64))),
                        }
                    }
                    buf.truncate(n);
                    produced += n as u64;
                    if let Some(size) = img.size {
                        if produced > size {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("the image is longer than its header says ({size} bytes)")));
                        }
                        if n < chunk && produced < size {
                            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("the image ends after {produced} of {size} bytes")));
                        }
                    }
                    if n == 0 {
                        return Ok(());
                    }
                    if tx.send(Ok(buf)).is_err() {
                        return Ok(());
                    }
                    if n < chunk {
                        return Ok(());
                    }
                }
            }));
            let err = match r {
                Ok(Ok(())) => return,
                Ok(Err(e)) => e,
                Err(_) => io::Error::new(io::ErrorKind::InvalidData, "decoder failure on damaged data"),
            };
            let _ = tx.send(Err(err));
        });
        Pipe { rx, cur: vec![], pos: 0, stop, consumed, size: self.size, done: false }
    }
}

/// An error while decoding `img`, as the user should read it.
pub fn damaged(img: &Image, e: io::Error) -> ForgeError {
    if e.kind() == io::ErrorKind::StorageFull {
        return ForgeError::Io(e.to_string());
    }
    let name = img.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    ForgeError::Invalid(t(
        &format!("{name}: immagine {} danneggiata o incompleta ({e})", img.format.name()),
        &format!("{name}: damaged or incomplete {} image ({e})", img.format.name()),
    ))
}

/// Counts the bytes read from the source file (progress when the decoded size is unknown).
struct Counting<R> {
    inner: R,
    n: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let k = self.inner.read(buf)?;
        self.n.fetch_add(k as u64, Ordering::Relaxed);
        Ok(k)
    }
}

/// Sequential reader over a random-access device.
struct DevReader {
    dev: Arc<dyn BlockDev>,
    pos: u64,
    consumed: Arc<AtomicU64>,
}

impl Read for DevReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (self.dev.size() - self.pos.min(self.dev.size())).min(buf.len() as u64) as usize;
        if n == 0 {
            return Ok(0);
        }
        self.dev.read_at(self.pos, &mut buf[..n])?;
        self.pos += n as u64;
        self.consumed.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// A reader that can look ahead a few bytes without consuming them.
struct Peek<R> {
    inner: R,
    pending: Vec<u8>,
}

impl<R: Read> Peek<R> {
    /// Up to `n` next bytes (fewer only at the end of the stream).
    fn peek(&mut self, n: usize) -> io::Result<&[u8]> {
        while self.pending.len() < n {
            let mut b = [0u8; 16];
            let k = self.inner.read(&mut b[..n - self.pending.len()])?;
            if k == 0 {
                break;
            }
            self.pending.extend_from_slice(&b[..k]);
        }
        Ok(&self.pending[..n.min(self.pending.len())])
    }
}

impl<R: Read> Read for Peek<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.pending.is_empty() {
            let n = buf.len().min(self.pending.len());
            buf[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            return Ok(n);
        }
        self.inner.read(buf)
    }
}

/// Frames of a zstd stream (RFC 8878 §3.1): zstd frames, possibly several (pzstd, `zstd -T`),
/// and skippable frames (magic 0x184D2A5?, u32 LE length) which are skipped. Each frame's
/// content checksum is verified when present. Windows larger than ruzstd's default limit
/// (128 MiB) are refused, which bounds memory.
struct ZstdFrames<R: Read> {
    state: Option<ZState<R>>,
}

enum ZState<R: Read> {
    Between(Peek<R>, ruzstd::decoding::FrameDecoder),
    In(ruzstd::decoding::StreamingDecoder<Peek<R>, ruzstd::decoding::FrameDecoder>),
}

impl<R: Read> ZstdFrames<R> {
    fn new(r: R) -> Self {
        ZstdFrames { state: Some(ZState::Between(Peek { inner: r, pending: vec![] }, ruzstd::decoding::FrameDecoder::new())) }
    }
}

fn zerr(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("zstd: {e}"))
}

impl<R: Read> Read for ZstdFrames<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.state.take() {
                None => return Ok(0),
                Some(ZState::In(mut d)) => {
                    let n = d.read(buf)?;
                    if n > 0 || buf.is_empty() {
                        self.state = Some(ZState::In(d));
                        return Ok(n);
                    }
                    if !d.decoder.is_finished() {
                        return Err(zerr("frame truncated"));
                    }
                    if let (Some(a), Some(b)) = (d.decoder.get_checksum_from_data(), d.decoder.get_calculated_checksum()) {
                        if a != b {
                            return Err(zerr("checksum mismatch"));
                        }
                    }
                    let (r, dec) = d.into_parts();
                    self.state = Some(ZState::Between(r, dec));
                }
                Some(ZState::Between(mut r, dec)) => {
                    let m = r.peek(4)?;
                    if m.is_empty() {
                        return Ok(0);
                    }
                    if m.len() < 4 {
                        return Err(zerr("trailing garbage"));
                    }
                    if m[1..4] == [0x2A, 0x4D, 0x18] && m[0] & 0xF0 == 0x50 {
                        let mut h = [0u8; 8];
                        r.read_exact(&mut h)?;
                        let len = u64::from(u32::from_le_bytes(h[4..8].try_into().unwrap()));
                        let skipped = io::copy(&mut (&mut r).take(len), &mut io::sink())?;
                        if skipped != len {
                            return Err(zerr("skippable frame truncated"));
                        }
                        self.state = Some(ZState::Between(r, dec));
                        continue;
                    }
                    if m[..4] != [0x28, 0xB5, 0x2F, 0xFD] {
                        return Err(zerr("not a zstd frame"));
                    }
                    let d = ruzstd::decoding::StreamingDecoder::new_with_decoder(r, dec).map_err(zerr)?;
                    self.state = Some(ZState::In(d));
                }
            }
        }
    }
}

/// Sum of the Frame_Content_Size of every frame (RFC 8878 §3.1.1.1), walking block headers
/// (§3.1.1.2) to find each frame's end. None if a frame doesn't declare its size.
fn zstd_content_size(f: &File, file_size: u64) -> Option<u64> {
    let rd = |off: u64, n: usize| -> Option<Vec<u8>> {
        let mut b = vec![0u8; n];
        f.read_exact_at(&mut b, off).ok()?;
        Some(b)
    };
    let mut pos = 0u64;
    let mut total = 0u64;
    let mut steps = 0u64;
    while pos < file_size {
        steps += 1;
        if steps > 50_000_000 {
            return None;
        }
        let m = rd(pos, 4)?;
        if m[1..4] == [0x2A, 0x4D, 0x18] && m[0] & 0xF0 == 0x50 {
            let len = u32::from_le_bytes(rd(pos + 4, 4)?.try_into().ok()?);
            pos = pos.checked_add(8 + u64::from(len))?;
            continue;
        }
        if m != [0x28, 0xB5, 0x2F, 0xFD] {
            return None;
        }
        let fhd = rd(pos + 4, 1)?[0];
        let fcs_flag = fhd >> 6;
        let single = fhd & 0x20 != 0;
        let checksum = fhd & 0x04 != 0;
        let did = [0u64, 1, 2, 4][(fhd & 3) as usize];
        let fcs_len = match fcs_flag {
            0 if single => 1,
            0 => return None,
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let mut p = pos + 5 + if single { 0 } else { 1 } + did;
        let raw = rd(p, fcs_len)?;
        let mut v = 0u64;
        for (i, b) in raw.iter().enumerate() {
            v |= u64::from(*b) << (8 * i);
        }
        if fcs_len == 2 {
            v += 256;
        }
        total = total.checked_add(v)?;
        p += fcs_len as u64;
        loop {
            steps += 1;
            if steps > 50_000_000 {
                return None;
            }
            let h = rd(p, 3)?;
            let bh = u32::from(h[0]) | u32::from(h[1]) << 8 | u32::from(h[2]) << 16;
            let last = bh & 1 != 0;
            let size = u64::from(bh >> 3);
            p += 3 + match (bh >> 1) & 3 {
                0 | 2 => size,
                1 => 1,
                _ => return None,
            };
            if last {
                break;
            }
        }
        if checksum {
            p += 4;
        }
        if p > file_size {
            return None;
        }
        pos = p;
    }
    Some(total)
}

/// Decoded image chunks from [`Image::pipe`]. Also a plain `Read`.
pub struct Pipe {
    rx: Receiver<io::Result<Vec<u8>>>,
    cur: Vec<u8>,
    pos: usize,
    stop: Arc<AtomicBool>,
    /// Bytes of the source file consumed so far.
    pub consumed: Arc<AtomicU64>,
    /// Decoded size, if known in advance.
    pub size: Option<u64>,
    done: bool,
}

impl Pipe {
    /// The next chunk (all chunks but the last are full); None at the end.
    pub fn next_chunk(&mut self) -> Option<io::Result<Vec<u8>>> {
        if self.done {
            return None;
        }
        match self.rx.recv() {
            Ok(Ok(c)) => Some(Ok(c)),
            Ok(Err(e)) => {
                self.done = true;
                Some(Err(e))
            }
            Err(_) => {
                self.done = true;
                None
            }
        }
    }
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.cur.len() {
            match self.next_chunk() {
                None => return Ok(0),
                Some(c) => {
                    self.cur = c?;
                    self.pos = 0;
                }
            }
        }
        let n = buf.len().min(self.cur.len() - self.pos);
        buf[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ───────────────────────────── inspection ─────────────────────────────

/// The file tree of an ISO image, for OS detection and ISO / Windows modes.
pub fn open_iso_tree(dev: Arc<dyn BlockDev>) -> ForgeResult<Option<Box<dyn SourceTree>>> {
    // TODO(iso workstream): hook up the ISO 9660 / UDF reader once it exists:
    //     return Ok(Some(Box::new(crate::iso::open(dev)?)));
    let _ = dev;
    Ok(None)
}

/// What [`inspect`] concluded; also what flash uses to resolve [`WriteMode::Auto`].
#[derive(Clone, Debug, Serialize)]
pub struct Inspection {
    pub path: String,
    pub file_name: String,
    pub format: Format,
    pub container: Value,
    pub file_size: u64,
    pub compressed: bool,
    /// Size of the disk image (decompressed); null when it can only be known by decompressing.
    pub size: Option<u64>,
    /// Rough estimate when `size` is null (progress only).
    pub size_estimate: Option<u64>,
    pub random_access: bool,
    pub partition_scheme: Option<String>,
    pub partitions: Vec<crate::block::PartitionInfo>,
    pub iso: Option<IsoInfo>,
    pub windows: Option<super::windows::WindowsInfo>,
    pub linux: Option<super::linux::LinuxInfo>,
    pub rpi: Option<super::rpi::RpiInfo>,
    /// A SHA256SUMS-style file next to the image lists it.
    pub checksum_file: Option<super::hash::SumsEntry>,
    pub recommended_mode: WriteMode,
    pub explanation: String,
    /// Things the quick inspection could not look at, and why.
    pub notes: Vec<String>,
}

/// The write mode for what was found, with a one-line reason for the user.
pub fn recommend(iso: Option<&IsoInfo>, windows: bool, rpi: bool) -> (WriteMode, String) {
    match iso {
        _ if windows => (
            WriteMode::Windows,
            t(
                "Supporto di installazione di Windows: i file vengono copiati su una chiavetta avviabile (UEFI e BIOS), con le opzioni di installazione scelte.",
                "Windows installation media: the files are copied to a bootable stick (UEFI and BIOS), with the setup options you choose.",
            ),
        ),
        Some(i) if i.hybrid => (
            WriteMode::Dd,
            t(
                "ISO ibrida: viene scritta byte per byte (modalità DD) ed è avviabile da USB così com'è.",
                "Hybrid ISO: it is written byte for byte (DD mode) and boots from USB as it is.",
            ),
        ),
        Some(_) => (
            WriteMode::Iso,
            t(
                "ISO non ibrida: scritta byte per byte non partirebbe da USB, quindi i suoi file vengono copiati su una chiavetta resa avviabile (modalità ISO).",
                "Non-hybrid ISO: written byte for byte it would not boot from USB, so its files are copied to a stick made bootable (ISO mode).",
            ),
        ),
        None if rpi => (
            WriteMode::Dd,
            t(
                "Immagine per Raspberry Pi: viene scritta byte per byte, poi si possono impostare nome, utente, Wi-Fi e SSH.",
                "Raspberry Pi image: it is written byte for byte, then hostname, user, Wi-Fi and SSH can be set.",
            ),
        ),
        None => (
            WriteMode::Dd,
            t("Immagine disco: viene scritta byte per byte (modalità DD).", "Disk image: it is written byte for byte (DD mode)."),
        ),
    }
}

/// Inspect a source image: a job (`Request::Inspect`), which may decompress a compressed
/// ISO completely to look inside it.
pub fn inspect(source: &str, ctx: &JobCtx) -> ForgeResult<Value> {
    let r = inspect_image(&Image::open(source)?, ctx, true)?;
    serde_json::to_value(r).map_err(|e| ForgeError::Io(e.to_string()))
}

/// Quick inspection for the UI's drop zone: never decompresses more than [`QUICK_BUDGET`],
/// never hashes.
pub fn inspect_quick(source: &str) -> ForgeResult<Value> {
    let r = inspect_image(&Image::open(source)?, &JobCtx::detached(false), false)?;
    serde_json::to_value(r).map_err(|e| ForgeError::Io(e.to_string()))
}

pub fn inspect_image(img: &Image, ctx: &JobCtx, deep: bool) -> ForgeResult<Inspection> {
    let mut notes = vec![];
    let native = img.native_device()?;
    let random_access = native.is_some();
    let dev = match native {
        Some(d) => d,
        None => img.device(ctx, false, (!deep).then_some(QUICK_BUDGET))?,
    };
    let note = |notes: &mut Vec<String>, what: &str, e: &dyn std::fmt::Display| {
        notes.push(format!("{what}: {e}"));
    };
    let partitions = match crate::block::parse_partitions(dev.as_ref()) {
        Ok(p) => p,
        Err(e) => {
            note(&mut notes, "partitions", &e);
            vec![]
        }
    };
    let partition_scheme = partitions.first().map(|p| p.scheme.clone());
    let iso = isoinfo::probe(dev.as_ref());
    let (mut windows, mut linux, mut rpi) = (None, None, None);
    if iso.is_some() {
        // The ISO's file tree is read all over the image: a compressed ISO is decompressed
        // completely first (deep inspection only).
        let tree_dev = if random_access {
            Some(dev.clone())
        } else if deep {
            ctx.phase(Phase::Extracting, img.size.unwrap_or(0));
            Some(img.device(ctx, true, None)?)
        } else {
            notes.push(t(
                "ISO compressa: il riconoscimento del sistema operativo richiede la decompressione completa",
                "compressed ISO: recognising the operating system needs a full decompression",
            ));
            None
        };
        if let Some(d) = tree_dev {
            match open_iso_tree(d) {
                Ok(Some(mut tree)) => {
                    match super::windows::detect(tree.as_mut()) {
                        Ok(w) => windows = w,
                        Err(e) => note(&mut notes, "windows", &e),
                    }
                    match super::linux::detect(tree.as_mut()) {
                        Ok(l) => linux = l,
                        Err(e) => note(&mut notes, "linux", &e),
                    }
                }
                Ok(None) => {}
                Err(e) => note(&mut notes, "iso", &e),
            }
        }
    } else {
        match super::rpi::detect(dev.clone()) {
            Ok(r) => rpi = r,
            Err(e) => note(&mut notes, "rpi", &e),
        }
    }
    let checksum_file = super::hash::find_sums_entry(&img.path);
    let (recommended_mode, explanation) = recommend(iso.as_ref(), windows.is_some(), rpi.is_some());
    Ok(Inspection {
        path: img.path.to_string_lossy().into_owned(),
        file_name: img.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        format: img.format,
        container: img.describe(),
        file_size: img.file_size,
        compressed: img.compressed(),
        size: img.size,
        size_estimate: if img.size.is_none() { img.size_hint } else { None },
        random_access,
        partition_scheme,
        partitions,
        iso,
        windows,
        linux,
        rpi,
        checksum_file,
        recommended_mode,
        explanation,
        notes,
    })
}
