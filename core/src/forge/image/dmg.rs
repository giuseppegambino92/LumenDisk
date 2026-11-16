//! Apple disk images (UDIF: UDRO, UDZO, UDBZ, ULFO, ULMO, UDCO) as random-access devices.
//!
//! References (UDIF is undocumented by Apple; these are the established descriptions):
//! - Jonathan Levin, "Demystifying the DMG File Format" (newosxbook.com/DMG.html): the 512-byte
//!   big-endian `koly` trailer (UDIFResourceFile) and the `mish` block tables (BLKXTable).
//! - dmg2img / libdmg-hfsplus (`dmg.h`): chunk ("run") types — 0x00000000 zero fill,
//!   0x00000001 raw copy, 0x00000002 ignored (free space, reads as zeros), 0x80000004 ADC,
//!   0x80000005 zlib, 0x80000006 bzip2, 0x80000007 LZFSE, 0x80000008 LZMA (an .xz stream),
//!   0x7FFFFFFE comment, 0xFFFFFFFF terminator.
//!
//! koly fields used: DataForkOffset (u64 @24), DataForkLength (@32), XMLOffset (u64 @216),
//! XMLLength (u64 @224), SectorCount (u64 @492). The XML property list's `resource-fork` →
//! `blkx` array holds one base64 `Data` blob per table. mish: SectorNumber (u64 @8),
//! SectorCount (@16), DataOffset (@24), NumberOfBlockChunks (u32 @200), then 40-byte chunks:
//! type (u32), comment (u32), SectorNumber (u64, relative), SectorCount (u64),
//! CompressedOffset (u64, relative to DataOffset within the data fork), CompressedLength (u64).
//!
//! Every chunk is decoded on its own (bounded: at most [`MAX_CHUNK`] bytes each side), and a
//! few decoded chunks are cached so sequential reads decode each chunk once.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};

use base64::Engine;

use crate::block::BlockDev;
use crate::forge::{ForgeError, ForgeResult};
use crate::i18n::t;

const SECTOR: u64 = 512;
/// Largest chunk we accept (decoded or encoded). hdiutil writes 1 MiB chunks (2048 sectors).
pub const MAX_CHUNK: u64 = 64 << 20;
const MAX_XML: u64 = 64 << 20;
const MAX_CHUNKS: usize = 4 << 20;
const CACHE: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Zero,
    Raw,
    Adc,
    Zlib,
    Bzip2,
    Lzfse,
    Lzma,
}

impl Kind {
    fn from_u32(v: u32) -> Option<Option<Kind>> {
        Some(match v {
            0x0000_0000 | 0x0000_0002 => Some(Kind::Zero),
            0x0000_0001 => Some(Kind::Raw),
            0x8000_0004 => Some(Kind::Adc),
            0x8000_0005 => Some(Kind::Zlib),
            0x8000_0006 => Some(Kind::Bzip2),
            0x8000_0007 => Some(Kind::Lzfse),
            0x8000_0008 => Some(Kind::Lzma),
            0x7FFF_FFFE | 0xFFFF_FFFF => None,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Zero => "zero",
            Kind::Raw => "raw",
            Kind::Adc => "adc",
            Kind::Zlib => "zlib",
            Kind::Bzip2 => "bzip2",
            Kind::Lzfse => "lzfse",
            Kind::Lzma => "lzma",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Chunk {
    /// Byte range of the virtual disk.
    start: u64,
    len: u64,
    kind: Kind,
    /// Absolute byte range in the file.
    off: u64,
    clen: u64,
}

/// The parsed block tables of a DMG.
#[derive(Debug)]
pub struct DmgTable {
    pub size: u64,
    chunks: Vec<Chunk>,
}

impl DmgTable {
    /// Chunk kinds present, for the inspect report ("zlib" → UDZO, "lzfse" → ULFO…).
    pub fn kinds(&self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = vec![];
        for c in &self.chunks {
            if !v.contains(&c.kind.name()) {
                v.push(c.kind.name());
            }
        }
        v
    }

    /// The hdiutil format name these chunks correspond to.
    pub fn variant(&self) -> &'static str {
        let k = self.kinds();
        let has = |n: &str| k.contains(&n);
        if has("lzma") {
            "ULMO"
        } else if has("lzfse") {
            "ULFO"
        } else if has("bzip2") {
            "UDBZ"
        } else if has("zlib") {
            "UDZO"
        } else if has("adc") {
            "UDCO"
        } else {
            "UDRO"
        }
    }

    pub fn compressed(&self) -> bool {
        self.chunks.iter().any(|c| !matches!(c.kind, Kind::Zero | Kind::Raw))
    }
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

fn corrupt(what: &str) -> ForgeError {
    ForgeError::Invalid(t(&format!("immagine DMG danneggiata ({what})"), &format!("damaged DMG image ({what})")))
}

/// Whether the file ends with a `koly` trailer.
pub fn probe(f: &File, size: u64) -> bool {
    let mut b = [0u8; 12];
    size >= 512 && f.read_exact_at(&mut b, size - 512).is_ok() && &b[..4] == b"koly" && be32(&b, 8) == 512
}

/// Parse the koly trailer, the XML plist and every mish table.
pub fn open(f: &File, file_size: u64) -> ForgeResult<DmgTable> {
    let mut k = vec![0u8; 512];
    f.read_exact_at(&mut k, file_size - 512)?;
    if &k[0..4] != b"koly" || be32(&k, 8) != 512 {
        return Err(corrupt("koly trailer"));
    }
    let df_off = be64(&k, 24);
    let df_len = be64(&k, 32);
    let xml_off = be64(&k, 216);
    let xml_len = be64(&k, 224);
    let sectors = be64(&k, 492);
    let koly_at = file_size - 512;
    if df_off.checked_add(df_len).map_or(true, |e| e > koly_at) {
        return Err(corrupt("data fork outside the file"));
    }
    if xml_len == 0 {
        return Err(ForgeError::Unsupported(t(
            "DMG senza tabella XML (formato molto vecchio): convertilo con Utility Disco",
            "DMG without an XML block table (very old format): convert it with Disk Utility",
        )));
    }
    if xml_len > MAX_XML || xml_off.checked_add(xml_len).map_or(true, |e| e > koly_at) {
        return Err(corrupt("property list"));
    }
    let mut xml = vec![0u8; xml_len as usize];
    f.read_exact_at(&mut xml, xml_off)?;
    let xml = String::from_utf8_lossy(&xml);
    let size = sectors.checked_mul(SECTOR).ok_or_else(|| corrupt("sector count"))?;
    let mut chunks = vec![];
    for blob in blkx_blobs(&xml)? {
        parse_mish(&blob, df_off, df_len, size, &mut chunks)?;
        if chunks.len() > MAX_CHUNKS {
            return Err(corrupt("too many chunks"));
        }
    }
    if chunks.is_empty() && size > 0 {
        return Err(corrupt("no block table"));
    }
    chunks.sort_by_key(|c| c.start);
    for w in chunks.windows(2) {
        if w[0].start + w[0].len > w[1].start {
            return Err(corrupt("overlapping chunks"));
        }
    }
    Ok(DmgTable { size, chunks })
}

/// The base64 `<data>` blobs of the `blkx` array of the XML property list.
fn blkx_blobs(xml: &str) -> ForgeResult<Vec<Vec<u8>>> {
    let start = xml.find("<key>blkx</key>").ok_or_else(|| corrupt("blkx missing"))?;
    let rest = &xml[start..];
    let arr_start = rest.find("<array>").ok_or_else(|| corrupt("blkx array"))?;
    let arr_end = rest.find("</array>").ok_or_else(|| corrupt("blkx array"))?;
    if arr_end < arr_start {
        return Err(corrupt("blkx array"));
    }
    let mut body = &rest[arr_start..arr_end];
    let mut out = vec![];
    while let Some(i) = body.find("<data>") {
        let after = &body[i + 6..];
        let j = after.find("</data>").ok_or_else(|| corrupt("data element"))?;
        let b64: String = after[..j].chars().filter(|c| !c.is_whitespace()).collect();
        let raw = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()).map_err(|_| corrupt("base64"))?;
        // Tables other than mish (e.g. a "plst" blob) are skipped.
        if raw.len() >= 204 && &raw[0..4] == b"mish" {
            out.push(raw);
        }
        body = &after[j + 7..];
    }
    Ok(out)
}

fn parse_mish(b: &[u8], df_off: u64, df_len: u64, size: u64, out: &mut Vec<Chunk>) -> ForgeResult<()> {
    let first = be64(b, 8);
    let data_off = be64(b, 24);
    let n = be32(b, 200) as usize;
    if b.len() < 204 + n.checked_mul(40).ok_or_else(|| corrupt("chunk count"))? {
        return Err(corrupt("mish table truncated"));
    }
    for i in 0..n {
        let c = &b[204 + i * 40..204 + i * 40 + 40];
        let ty = be32(c, 0);
        let kind = match Kind::from_u32(ty) {
            Some(Some(k)) => k,
            Some(None) => continue, // comment / terminator
            None => return Err(ForgeError::Unsupported(t(&format!("DMG: tipo di blocco 0x{ty:08X} non supportato"), &format!("DMG: unsupported chunk type 0x{ty:08X}")))),
        };
        let sec = first.checked_add(be64(c, 8)).ok_or_else(|| corrupt("sector"))?;
        let cnt = be64(c, 16);
        let start = sec.checked_mul(SECTOR).ok_or_else(|| corrupt("sector"))?;
        let len = cnt.checked_mul(SECTOR).ok_or_else(|| corrupt("sector"))?;
        if start.checked_add(len).map_or(true, |e| e > size) {
            return Err(corrupt("chunk beyond the disk size"));
        }
        if len == 0 {
            continue;
        }
        let (off, clen) = if kind == Kind::Zero {
            (0, 0)
        } else {
            let rel = data_off.checked_add(be64(c, 24)).ok_or_else(|| corrupt("offset"))?;
            let clen = be64(c, 32);
            if rel.checked_add(clen).map_or(true, |e| e > df_len) {
                return Err(corrupt("chunk outside the data fork"));
            }
            (df_off + rel, clen)
        };
        // Zero and raw runs are never decoded into memory (UDRO uses runs of any length);
        // compressed ones are, whole.
        if !matches!(kind, Kind::Zero | Kind::Raw) && (len > MAX_CHUNK || clen > MAX_CHUNK) {
            return Err(ForgeError::Unsupported(t("DMG con blocchi troppo grandi", "DMG with oversized chunks")));
        }
        if kind == Kind::Raw && clen < len {
            return Err(corrupt("raw chunk shorter than its sectors"));
        }
        out.push(Chunk { start, len, kind, off, clen });
    }
    Ok(())
}

/// `Write` into a buffer that refuses to grow past `limit` bytes.
struct Bounded<'a> {
    out: &'a mut Vec<u8>,
    limit: usize,
}

impl io::Write for Bounded<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.out.len() + b.len() > self.limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk decodes to more than its sectors"));
        }
        self.out.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Apple Data Compression (ADC), as decoded by dmg2img's `adc.c`. Each op starts with a byte:
/// - `1nnnnnnn`: (n + 1) literal bytes follow;
/// - `00nnnndd dddddddd`: copy (n + 3) bytes from distance d + 1 (d: 10 bits);
/// - `01nnnnnn dddddddd dddddddd`: copy (n + 4) bytes from distance d + 1 (d: 16 bits).
/// Copies may overlap their own output (distance 1 repeats a byte).
pub fn adc_decode(input: &[u8], out: &mut Vec<u8>, limit: usize) -> io::Result<()> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "adc: damaged data");
    let mut i = 0usize;
    while i < input.len() {
        let b = input[i];
        if b & 0x80 != 0 {
            let n = (b & 0x7F) as usize + 1;
            let lit = input.get(i + 1..i + 1 + n).ok_or_else(bad)?;
            if out.len() + n > limit {
                return Err(bad());
            }
            out.extend_from_slice(lit);
            i += 1 + n;
            continue;
        }
        let (n, dist, step) = if b & 0x40 != 0 {
            let d = u16::from_be_bytes([*input.get(i + 1).ok_or_else(bad)?, *input.get(i + 2).ok_or_else(bad)?]);
            ((b & 0x3F) as usize + 4, d as usize + 1, 3)
        } else {
            let d = ((b as usize & 0x03) << 8) | *input.get(i + 1).ok_or_else(bad)? as usize;
            (((b >> 2) & 0x0F) as usize + 3, d + 1, 2)
        };
        if dist > out.len() || out.len() + n > limit {
            return Err(bad());
        }
        let from = out.len() - dist;
        for k in 0..n {
            let x = out[from + k];
            out.push(x);
        }
        i += step;
    }
    Ok(())
}

fn decode_chunk(f: &File, c: &Chunk) -> io::Result<Vec<u8>> {
    let mut input = vec![0u8; c.clen as usize];
    f.read_exact_at(&mut input, c.off)?;
    let want = c.len as usize;
    let mut out = Vec::with_capacity(want);
    let short = || io::Error::new(io::ErrorKind::InvalidData, "chunk decodes to fewer bytes than its sectors");
    match c.kind {
        Kind::Zero => out.resize(want, 0),
        Kind::Raw => {
            input.truncate(want);
            out = input;
        }
        Kind::Adc => adc_decode(&input, &mut out, want)?,
        Kind::Zlib => {
            flate2::read::ZlibDecoder::new(&input[..]).take(c.len + 1).read_to_end(&mut out)?;
        }
        Kind::Bzip2 => {
            bzip2::read::BzDecoder::new(&input[..]).take(c.len + 1).read_to_end(&mut out)?;
        }
        Kind::Lzma => {
            lzma_rust2::XzReader::new_mem_limit(&input[..], true, super::xz::MEM_LIMIT_KB).take(c.len + 1).read_to_end(&mut out)?;
        }
        Kind::Lzfse => {
            let mut dec = lzfse_rust::LzfseRingDecoder::default();
            dec.decode(&mut &input[..], &mut Bounded { out: &mut out, limit: want }).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("lzfse: {e}")))?;
        }
    }
    if out.len() > want {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk decodes to more than its sectors"));
    }
    if out.len() < want {
        return Err(short());
    }
    Ok(out)
}

/// Random access to the virtual disk of a DMG.
pub struct DmgDev {
    file: File,
    table: Arc<DmgTable>,
    cache: Mutex<VecDeque<(usize, Arc<Vec<u8>>)>>,
}

impl DmgDev {
    pub fn new(file: File, table: Arc<DmgTable>) -> Self {
        DmgDev { file, table, cache: Mutex::new(VecDeque::new()) }
    }

    fn chunk_data(&self, i: usize) -> io::Result<Arc<Vec<u8>>> {
        {
            let c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, d)) = c.iter().find(|(k, _)| *k == i) {
                return Ok(d.clone());
            }
        }
        let chunk = self.table.chunks[i];
        // Third-party decoders on hostile input: a panic becomes an I/O error.
        let d = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_chunk(&self.file, &chunk)))
            .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::InvalidData, "decoder failure on damaged data")))
            .map_err(|e| io::Error::new(e.kind(), format!("DMG chunk at byte {} ({}): {e}", chunk.start, chunk.kind.name())))?;
        let d = Arc::new(d);
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        c.push_back((i, d.clone()));
        while c.len() > CACHE {
            c.pop_front();
        }
        Ok(d)
    }
}

impl BlockDev for DmgDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset.checked_add(buf.len() as u64).filter(|&e| e <= self.table.size).ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read beyond the end of the DMG"))?;
        let chunks = &self.table.chunks;
        // First chunk ending after `offset`.
        let mut i = chunks.partition_point(|c| c.start + c.len <= offset);
        let mut pos = offset;
        while pos < end {
            let out = &mut buf[(pos - offset) as usize..];
            match chunks.get(i) {
                Some(c) if c.start <= pos => {
                    let within = pos - c.start;
                    let n = (c.len - within).min(end - pos) as usize;
                    match c.kind {
                        Kind::Zero => out[..n].fill(0),
                        Kind::Raw => self.file.read_exact_at(&mut out[..n], c.off + within)?,
                        _ => {
                            let d = self.chunk_data(i)?;
                            out[..n].copy_from_slice(&d[within as usize..within as usize + n]);
                        }
                    }
                    pos += n as u64;
                    i += 1;
                }
                // A gap no table describes: zeros.
                Some(c) => {
                    let n = (c.start.min(end) - pos) as usize;
                    out[..n].fill(0);
                    pos += n as u64;
                }
                None => {
                    out[..(end - pos) as usize].fill(0);
                    pos = end;
                }
            }
        }
        Ok(())
    }
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "source image is read-only"))
    }
    fn size(&self) -> u64 {
        self.table.size
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    fn read_only(&self) -> bool {
        true
    }
}

/// Test helpers: build UDIF images (koly + XML plist + mish tables) from raw data.
#[cfg(test)]
pub(crate) mod build {
    use super::*;
    use std::io::Write;

    /// Encode `data` as a DMG whose chunks (of `chunk` bytes) cycle through `kinds`; all-zero
    /// chunks become zero-fill runs.
    pub fn dmg(data: &[u8], chunk: usize, kinds: &[Kind]) -> Vec<u8> {
        assert_eq!(data.len() % 512, 0);
        let mut fork = vec![];
        let mut runs: Vec<(u32, u64, u64, u64, u64)> = vec![];
        for (i, c) in data.chunks(chunk).enumerate() {
            let sec = (i * chunk / 512) as u64;
            let cnt = (c.len() / 512) as u64;
            if c.iter().all(|&b| b == 0) {
                runs.push((if i % 2 == 0 { 0 } else { 2 }, sec, cnt, fork.len() as u64, 0));
                continue;
            }
            let kind = kinds[i % kinds.len()];
            let (ty, enc): (u32, Vec<u8>) = match kind {
                Kind::Raw | Kind::Zero => (1, c.to_vec()),
                Kind::Zlib => {
                    let mut e = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::default());
                    e.write_all(c).unwrap();
                    (0x8000_0005, e.finish().unwrap())
                }
                Kind::Bzip2 => {
                    let mut e = bzip2::write::BzEncoder::new(vec![], bzip2::Compression::default());
                    e.write_all(c).unwrap();
                    (0x8000_0006, e.finish().unwrap())
                }
                Kind::Lzfse => {
                    let mut out = vec![];
                    lzfse_rust::encode_bytes(c, &mut out).unwrap();
                    (0x8000_0007, out)
                }
                Kind::Lzma => {
                    let mut w = lzma_rust2::XzWriter::new(vec![], lzma_rust2::XzOptions::with_preset(1)).unwrap();
                    w.write_all(c).unwrap();
                    (0x8000_0008, w.finish().unwrap())
                }
                Kind::Adc => (0x8000_0004, adc_encode(c)),
            };
            runs.push((ty, sec, cnt, fork.len() as u64, enc.len() as u64));
            fork.extend(enc);
        }
        // One mish table for the whole disk (+ a comment run and the terminator).
        let mut m = vec![0u8; 204];
        m[0..4].copy_from_slice(b"mish");
        m[4..8].copy_from_slice(&1u32.to_be_bytes());
        m[16..24].copy_from_slice(&((data.len() / 512) as u64).to_be_bytes());
        runs.insert(0, (0x7FFF_FFFE, 0, 0, 0, 0));
        runs.push((0xFFFF_FFFF, (data.len() / 512) as u64, 0, fork.len() as u64, 0));
        m[200..204].copy_from_slice(&(runs.len() as u32).to_be_bytes());
        for (ty, sec, cnt, off, len) in runs {
            m.extend_from_slice(&ty.to_be_bytes());
            m.extend_from_slice(&0u32.to_be_bytes());
            m.extend_from_slice(&sec.to_be_bytes());
            m.extend_from_slice(&cnt.to_be_bytes());
            m.extend_from_slice(&off.to_be_bytes());
            m.extend_from_slice(&len.to_be_bytes());
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&m);
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>resource-fork</key><dict><key>blkx</key><array><dict><key>Attributes</key><string>0x0050</string><key>Data</key><data>\n{b64}\n</data><key>Name</key><string>whole disk</string></dict></array><key>plst</key><array><dict><key>Data</key><data>AAAA</data></dict></array></dict></dict></plist>\n"
        );
        let mut out = fork.clone();
        let xml_off = out.len() as u64;
        out.extend_from_slice(xml.as_bytes());
        let mut k = vec![0u8; 512];
        k[0..4].copy_from_slice(b"koly");
        k[4..8].copy_from_slice(&4u32.to_be_bytes());
        k[8..12].copy_from_slice(&512u32.to_be_bytes());
        k[32..40].copy_from_slice(&(fork.len() as u64).to_be_bytes());
        k[216..224].copy_from_slice(&xml_off.to_be_bytes());
        k[224..232].copy_from_slice(&(xml.len() as u64).to_be_bytes());
        k[492..500].copy_from_slice(&((data.len() / 512) as u64).to_be_bytes());
        out.extend(k);
        out
    }

    /// A simple ADC encoder: literal runs, plus 2-byte copies of repeated bytes.
    pub fn adc_encode(data: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        let mut lit: Vec<u8> = vec![];
        let flush = |lit: &mut Vec<u8>, out: &mut Vec<u8>| {
            for c in lit.chunks(128) {
                out.push(0x80 | (c.len() as u8 - 1));
                out.extend_from_slice(c);
            }
            lit.clear();
        };
        let mut i = 0;
        while i < data.len() {
            // run of the previous byte (distance 1), 3..=18 bytes: 2-byte form
            if i > 0 {
                let mut n = 0;
                while i + n < data.len() && n < 18 && data[i + n] == data[i - 1] {
                    n += 1;
                }
                if n >= 3 {
                    flush(&mut lit, &mut out);
                    out.push(((n as u8 - 3) << 2) & 0x3C);
                    out.push(0); // distance 0 + 1 = 1
                    i += n;
                    continue;
                }
            }
            lit.push(data[i]);
            i += 1;
        }
        flush(&mut lit, &mut out);
        out
    }
}
