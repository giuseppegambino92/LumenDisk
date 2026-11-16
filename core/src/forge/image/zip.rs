//! ZIP archives holding one disk image.
//!
//! Format reference: PKWARE APPNOTE.TXT 6.3.x — End of Central Directory record (4.3.16),
//! Zip64 EOCD locator / record (4.3.14, 4.3.15), Central Directory File Header (4.3.12),
//! Local File Header (4.3.7), Zip64 extended information extra field (4.5.3), compression
//! methods (4.4.5: 0 stored, 8 deflate, 12 bzip2, 93 zstd, 95 xz).
//!
//! Only the central directory is trusted for sizes (local headers may defer them to a data
//! descriptor). The archive must contain exactly one file that can be an image; metadata files
//! next to it (checksums, readme, macOS resource forks) are ignored.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;

use crate::forge::{ForgeError, ForgeResult};
use crate::i18n::t;

/// The image inside the archive.
#[derive(Clone, Debug)]
pub struct ZipEntry {
    pub name: String,
    pub method: u16,
    pub compressed: u64,
    pub size: u64,
    pub crc32: u32,
    /// Absolute offset of the entry's data in the archive.
    pub data_offset: u64,
}

impl ZipEntry {
    pub fn method_name(&self) -> &'static str {
        method_name(self.method)
    }
}

pub fn method_name(m: u16) -> &'static str {
    match m {
        0 => "stored",
        8 => "deflate",
        9 => "deflate64",
        12 => "bzip2",
        14 => "lzma",
        93 => "zstd",
        95 => "xz",
        99 => "aes",
        _ => "unknown",
    }
}

const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD64_LOC_SIG: u32 = 0x0706_4b50;
const EOCD64_SIG: u32 = 0x0606_4b50;
const CDH_SIG: u32 = 0x0201_4b50;
const LFH_SIG: u32 = 0x0403_4b50;
/// Central directories larger than this are refused (≈ 300 000 entries).
const MAX_CD: u64 = 32 << 20;

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn corrupt(what: &str) -> ForgeError {
    ForgeError::Invalid(t(&format!("archivio ZIP danneggiato ({what})"), &format!("damaged ZIP archive ({what})")))
}

fn read_at(f: &File, off: u64, len: usize) -> ForgeResult<Vec<u8>> {
    let mut v = vec![0u8; len];
    f.read_exact_at(&mut v, off).map_err(|_| corrupt("truncated"))?;
    Ok(v)
}

/// Names that are not the image: folders, macOS metadata, checksums and notes.
fn is_metadata(name: &str, size: u64) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    let lower = base.to_ascii_lowercase();
    name.ends_with('/')
        || name.starts_with("__MACOSX/")
        || base.starts_with('.')
        || (size < (1 << 20)
            && ["txt", "md", "sha256", "sha1", "md5", "sum", "sums", "asc", "sig", "gpg", "json", "html", "htm", "pdf", "nfo", "xml", "url", "license"]
                .iter()
                .any(|e| lower.ends_with(&format!(".{e}")) || lower == *e))
        || (size < (1 << 20) && (lower.starts_with("sha256sum") || lower.starts_with("md5sum") || lower.starts_with("sha1sum") || lower.starts_with("readme")))
}

/// Find the one image in the archive.
pub fn find_image(f: &File, file_size: u64) -> ForgeResult<ZipEntry> {
    // EOCD: 22 bytes + a comment of up to 65535 bytes, at the very end.
    let tail_len = file_size.min(22 + 65535);
    let tail = read_at(f, file_size - tail_len, tail_len as usize)?;
    let eocd = (0..tail.len().saturating_sub(21)).rev().find(|&i| le32(&tail, i) == EOCD_SIG).ok_or_else(|| corrupt("end of central directory not found"))?;
    let e = &tail[eocd..];
    let eocd_abs = file_size - tail_len + eocd as u64;
    let mut entries = u64::from(le16(e, 10));
    let mut cd_size = u64::from(le32(e, 12));
    let mut cd_off = u64::from(le32(e, 16));
    if le16(e, 4) != 0 || le16(e, 6) != 0 {
        if le16(e, 4) != 0xFFFF {
            return Err(ForgeError::Unsupported(t("archivi ZIP divisi in più parti non sono supportati", "multi-part (spanned) ZIP archives are not supported")));
        }
    }
    if entries == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF {
        // Zip64: the locator sits right before the EOCD.
        let loc_off = eocd_abs.checked_sub(20).ok_or_else(|| corrupt("zip64 locator"))?;
        let loc = read_at(f, loc_off, 20)?;
        if le32(&loc, 0) != EOCD64_LOC_SIG {
            return Err(corrupt("zip64 locator"));
        }
        let rec_off = le64(&loc, 8);
        if rec_off.checked_add(56).map_or(true, |e| e > loc_off) {
            return Err(corrupt("zip64 record"));
        }
        let rec = read_at(f, rec_off, 56)?;
        if le32(&rec, 0) != EOCD64_SIG {
            return Err(corrupt("zip64 record"));
        }
        entries = le64(&rec, 32);
        cd_size = le64(&rec, 40);
        cd_off = le64(&rec, 48);
    }
    if cd_size > MAX_CD || cd_off.checked_add(cd_size).map_or(true, |e| e > eocd_abs) {
        return Err(corrupt("central directory"));
    }
    let cd = read_at(f, cd_off, cd_size as usize)?;
    let mut pos = 0usize;
    let mut candidates = vec![];
    for _ in 0..entries {
        if pos + 46 > cd.len() || le32(&cd, pos) != CDH_SIG {
            return Err(corrupt("central directory entry"));
        }
        let h = &cd[pos..];
        let flags = le16(h, 8);
        let method = le16(h, 10);
        let crc32 = le32(h, 16);
        let mut csize = u64::from(le32(h, 20));
        let mut usize_ = u64::from(le32(h, 24));
        let (nl, xl, cl) = (le16(h, 28) as usize, le16(h, 30) as usize, le16(h, 32) as usize);
        let mut lho = u64::from(le32(h, 42));
        let end = pos + 46 + nl + xl + cl;
        if end > cd.len() {
            return Err(corrupt("central directory entry"));
        }
        let name = String::from_utf8_lossy(&cd[pos + 46..pos + 46 + nl]).into_owned();
        // Zip64 extra field: the 64-bit values of the fields set to 0xFFFFFFFF, in this order.
        let mut x = &cd[pos + 46 + nl..pos + 46 + nl + xl];
        while x.len() >= 4 {
            let (id, len) = (le16(x, 0), le16(x, 2) as usize);
            if 4 + len > x.len() {
                break;
            }
            if id == 0x0001 {
                let mut d = &x[4..4 + len];
                for field in [&mut usize_, &mut csize, &mut lho] {
                    if *field == 0xFFFF_FFFF && d.len() >= 8 {
                        *field = le64(d, 0);
                        d = &d[8..];
                    }
                }
            }
            x = &x[4 + len..];
        }
        pos = end;
        if is_metadata(&name, usize_) {
            continue;
        }
        candidates.push((name, flags, method, crc32, csize, usize_, lho));
    }
    let (name, flags, method, crc32, csize, size, lho) = match candidates.len() {
        0 => return Err(ForgeError::Invalid(t("l'archivio ZIP non contiene nessuna immagine", "the ZIP archive contains no image"))),
        1 => candidates.pop().unwrap(),
        n => {
            let names: Vec<&str> = candidates.iter().take(5).map(|c| c.0.as_str()).collect();
            return Err(ForgeError::Invalid(t(
                &format!("l'archivio ZIP contiene {n} file che potrebbero essere immagini ({}…): estrai quello giusto e scegli quello", names.join(", ")),
                &format!("the ZIP archive holds {n} files that could be images ({}…): extract it and choose the right one", names.join(", ")),
            )));
        }
    };
    if flags & 1 != 0 || method == 99 {
        return Err(ForgeError::Unsupported(t(&format!("{name} è cifrato nell'archivio ZIP"), &format!("{name} is encrypted in the ZIP archive"))));
    }
    if !matches!(method, 0 | 8 | 12 | 93 | 95) {
        return Err(ForgeError::Unsupported(t(
            &format!("{name}: metodo di compressione ZIP «{}» ({method}) non supportato: estrai l'archivio", method_name(method)),
            &format!("{name}: ZIP compression method \"{}\" ({method}) is not supported: extract the archive", method_name(method)),
        )));
    }
    if method == 0 && csize != size {
        return Err(corrupt("stored entry sizes differ"));
    }
    if lho.checked_add(30).map_or(true, |e| e > cd_off) {
        return Err(corrupt("local header offset"));
    }
    let lh = read_at(f, lho, 30)?;
    if le32(&lh, 0) != LFH_SIG {
        return Err(corrupt("local header"));
    }
    let data_offset = lho + 30 + u64::from(le16(&lh, 26)) + u64::from(le16(&lh, 28));
    if data_offset.checked_add(csize).map_or(true, |e| e > cd_off) {
        return Err(corrupt("entry data outside the archive"));
    }
    Ok(ZipEntry { name, method, compressed: csize, size, crc32, data_offset })
}

/// Checks the CRC-32 and size the central directory records once the entry is fully read.
pub struct CrcCheck<R> {
    inner: R,
    crc: crc32fast::Hasher,
    n: u64,
    want_crc: u32,
    want_len: u64,
}

impl<R: Read> CrcCheck<R> {
    pub fn new(inner: R, want_crc: u32, want_len: u64) -> Self {
        CrcCheck { inner, crc: crc32fast::Hasher::new(), n: 0, want_crc, want_len }
    }
}

impl<R: Read> Read for CrcCheck<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Never let a decoder produce more than the recorded size (+1 to detect it).
        let limit = self.want_len.saturating_add(1);
        let room = (limit - self.n.min(limit)).min(buf.len() as u64) as usize;
        let k = if room == 0 { 0 } else { self.inner.read(&mut buf[..room])? };
        self.n += k as u64;
        if self.n > self.want_len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "zip: entry is longer than recorded"));
        }
        self.crc.update(&buf[..k]);
        if k == 0 && !buf.is_empty() {
            if self.n != self.want_len {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "zip: entry is shorter than recorded"));
            }
            if self.crc.clone().finalize() != self.want_crc {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "zip: CRC-32 mismatch"));
            }
        }
        Ok(k)
    }
}
