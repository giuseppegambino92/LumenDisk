//! VHD (Virtual PC / Hyper-V "VHD", not VHDX): fixed and dynamic disks.
//!
//! Format reference: Microsoft "Virtual Hard Disk Image Format Specification", version 1.0
//! (October 2006):
//! - Hard Disk Footer (512 bytes, big-endian; 511 in images made before Virtual PC 2004) at the
//!   end of the file, and a copy at offset 0 for dynamic / differencing disks. Cookie
//!   "conectix", Data Offset (u64 @16), Current Size (u64 @48), Disk Type (u32 @60: 2 fixed,
//!   3 dynamic, 4 differencing), Checksum (u32 @64: one's complement of the byte sum of the
//!   footer with the checksum field taken as zero).
//! - Dynamic Disk Header (1024 bytes at Data Offset): cookie "cxsparse", Table Offset
//!   (u64 @16), Max Table Entries (u32 @28), Block Size (u32 @32), Checksum (u32 @36).
//! - Block Allocation Table: big-endian u32 sector numbers, 0xFFFFFFFF = unallocated (reads
//!   as zeros). Each allocated block = sector bitmap (one bit per 512-byte sector, padded to a
//!   sector boundary) followed by the block's data; a clear bit is a sector never written,
//!   which reads as zeros.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;

use crate::block::BlockDev;
use crate::forge::{ForgeError, ForgeResult};
use crate::i18n::t;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VhdKind {
    Fixed,
    Dynamic,
}

#[derive(Clone, Debug)]
pub struct VhdInfo {
    pub kind: VhdKind,
    /// Size of the virtual disk.
    pub size: u64,
    /// Fixed: the data is bytes 0..size of the file. Dynamic: the BAT.
    bat: Vec<u32>,
    block_size: u64,
    bitmap_len: u64,
    /// The footer used was the copy at the start of the file (the one at the end is damaged).
    pub footer_from_copy: bool,
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

fn corrupt(what: &str) -> ForgeError {
    ForgeError::Invalid(t(&format!("immagine VHD danneggiata ({what})"), &format!("damaged VHD image ({what})")))
}

/// One's complement of the sum of all bytes, the checksum field (`at..at+4`) counted as 0.
fn checksum(b: &[u8], at: usize) -> u32 {
    let sum = b.iter().enumerate().filter(|(i, _)| !(at..at + 4).contains(i)).fold(0u32, |s, (_, &x)| s.wrapping_add(x as u32));
    !sum
}

fn valid_footer(b: &[u8]) -> bool {
    b.len() >= 511 && &b[0..8] == b"conectix" && checksum(&b[..b.len().min(512)], 64) == be32(b, 64)
}

/// Whether the file ends with (or starts with, for dynamic disks) a VHD footer.
pub fn probe(f: &File, size: u64) -> bool {
    footer(f, size).map(|o| o.is_some()).unwrap_or(false)
}

fn footer(f: &File, size: u64) -> io::Result<Option<(Vec<u8>, bool)>> {
    for len in [512u64, 511] {
        if size >= len {
            let mut b = vec![0u8; len as usize];
            f.read_exact_at(&mut b, size - len)?;
            if valid_footer(&b) {
                return Ok(Some((b, false)));
            }
        }
    }
    if size >= 512 + 1024 {
        let mut b = vec![0u8; 512];
        f.read_exact_at(&mut b, 0)?;
        if valid_footer(&b) && be32(&b, 60) != 2 {
            return Ok(Some((b, true)));
        }
    }
    Ok(None)
}

/// Parse the footer (and, for dynamic disks, the header and BAT).
pub fn open(f: &File, file_size: u64) -> ForgeResult<VhdInfo> {
    let (ft, from_copy) = footer(f, file_size)?.ok_or_else(|| corrupt("footer"))?;
    let size = be64(&ft, 48);
    let disk_type = be32(&ft, 60);
    match disk_type {
        2 => {
            // Fixed: the data precedes the footer.
            let data_len = file_size - ft.len() as u64;
            if size > data_len {
                return Err(corrupt("size larger than the file"));
            }
            Ok(VhdInfo { kind: VhdKind::Fixed, size, bat: vec![], block_size: 0, bitmap_len: 0, footer_from_copy: from_copy })
        }
        3 => {
            let hdr_off = be64(&ft, 16);
            if hdr_off.checked_add(1024).map_or(true, |e| e > file_size) {
                return Err(corrupt("dynamic header offset"));
            }
            let mut h = vec![0u8; 1024];
            f.read_exact_at(&mut h, hdr_off)?;
            if &h[0..8] != b"cxsparse" || checksum(&h, 36) != be32(&h, 36) {
                return Err(corrupt("dynamic header"));
            }
            let bat_off = be64(&h, 16);
            let entries = u64::from(be32(&h, 28));
            let block_size = u64::from(be32(&h, 32));
            if !block_size.is_power_of_two() || !(4096..=(256 << 20)).contains(&block_size) {
                return Err(corrupt("block size"));
            }
            // Every sector of the virtual disk must belong to a BAT entry.
            if entries.checked_mul(block_size).map_or(true, |cap| cap < size) || entries > (1 << 24) {
                return Err(corrupt("block table size"));
            }
            let bat_len = entries * 4;
            if bat_off.checked_add(bat_len).map_or(true, |e| e > file_size) {
                return Err(corrupt("block table outside the file"));
            }
            let mut raw = vec![0u8; bat_len as usize];
            f.read_exact_at(&mut raw, bat_off)?;
            let bat: Vec<u32> = raw.chunks_exact(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect();
            let bitmap_len = (block_size / 512).div_ceil(8).div_ceil(512) * 512;
            for &e in &bat {
                if e != u32::MAX && (u64::from(e) * 512).checked_add(bitmap_len + block_size).map_or(true, |end| end > file_size) {
                    return Err(corrupt("block outside the file"));
                }
            }
            Ok(VhdInfo { kind: VhdKind::Dynamic, size, bat, block_size, bitmap_len, footer_from_copy: from_copy })
        }
        4 => Err(ForgeError::Unsupported(t(
            "VHD differenziale: contiene solo le modifiche rispetto a un disco padre. Uniscilo al padre (merge) e scegli il risultato",
            "differencing VHD: it only holds changes against a parent disk. Merge it into its parent and choose the result",
        ))),
        other => Err(corrupt(&format!("disk type {other}"))),
    }
}

/// Random access to the virtual disk of a VHD.
pub struct VhdDev {
    file: File,
    info: VhdInfo,
    /// Last bitmap read: (block index, bitmap).
    bitmap: Mutex<Option<(usize, Vec<u8>)>>,
}

impl VhdDev {
    pub fn new(file: File, info: VhdInfo) -> Self {
        VhdDev { file, info, bitmap: Mutex::new(None) }
    }

    fn read_block_part(&self, block: usize, within: u64, out: &mut [u8]) -> io::Result<()> {
        let e = self.info.bat[block];
        if e == u32::MAX {
            out.fill(0);
            return Ok(());
        }
        let base = u64::from(e) * 512;
        self.file.read_exact_at(out, base + self.info.bitmap_len + within)?;
        // Sectors whose bitmap bit is clear were never written: zeros (spec, "Sector Bitmap").
        let mut cache = self.bitmap.lock().unwrap_or_else(|e| e.into_inner());
        if cache.as_ref().map(|(b, _)| *b) != Some(block) {
            let mut bm = vec![0u8; self.info.bitmap_len as usize];
            self.file.read_exact_at(&mut bm, base)?;
            *cache = Some((block, bm));
        }
        let bm = &cache.as_ref().unwrap().1;
        if bm.iter().all(|&b| b == 0xFF) {
            return Ok(());
        }
        let mut pos = 0usize;
        while pos < out.len() {
            let abs = within + pos as u64;
            let sector = (abs / 512) as usize;
            let in_sector = (abs % 512) as usize;
            let n = (512 - in_sector).min(out.len() - pos);
            if bm.get(sector / 8).map_or(true, |b| b & (0x80 >> (sector % 8)) == 0) {
                out[pos..pos + n].fill(0);
            }
            pos += n;
        }
        Ok(())
    }
}

impl BlockDev for VhdDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if offset.checked_add(buf.len() as u64).map_or(true, |e| e > self.info.size) {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read beyond the end of the VHD"));
        }
        if self.info.kind == VhdKind::Fixed {
            return self.file.read_exact_at(buf, offset);
        }
        let bs = self.info.block_size;
        let mut done = 0usize;
        while done < buf.len() {
            let abs = offset + done as u64;
            let block = (abs / bs) as usize;
            let within = abs % bs;
            let n = ((bs - within) as usize).min(buf.len() - done);
            self.read_block_part(block, within, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "source image is read-only"))
    }
    fn size(&self) -> u64 {
        self.info.size
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    fn read_only(&self) -> bool {
        true
    }
}

/// Test helpers: build VHDs the way the specification describes them.
#[cfg(test)]
pub(crate) mod build {
    use super::*;

    pub fn footer(size: u64, disk_type: u32, data_offset: u64) -> Vec<u8> {
        let mut f = vec![0u8; 512];
        f[0..8].copy_from_slice(b"conectix");
        f[8..12].copy_from_slice(&2u32.to_be_bytes());
        f[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        f[16..24].copy_from_slice(&data_offset.to_be_bytes());
        f[28..32].copy_from_slice(b"ldsk");
        f[40..48].copy_from_slice(&size.to_be_bytes());
        f[48..56].copy_from_slice(&size.to_be_bytes());
        f[60..64].copy_from_slice(&disk_type.to_be_bytes());
        let c = checksum(&f, 64);
        f[64..68].copy_from_slice(&c.to_be_bytes());
        f
    }

    pub fn fixed(data: &[u8]) -> Vec<u8> {
        let mut v = data.to_vec();
        v.extend(footer(data.len() as u64, 2, u64::MAX));
        v
    }

    /// Dynamic VHD with `block_size` blocks; all-zero blocks are left unallocated, and in
    /// allocated blocks all-zero sectors get a clear bitmap bit (and garbage data, to prove
    /// the bitmap is honoured).
    pub fn dynamic(data: &[u8], block_size: u64) -> Vec<u8> {
        let size = data.len() as u64;
        let entries = size.div_ceil(block_size);
        let bitmap_len = (block_size / 512).div_ceil(8).div_ceil(512) * 512;
        let ft = footer(size, 3, 512);
        let mut out = ft.clone();
        let mut h = vec![0u8; 1024];
        h[0..8].copy_from_slice(b"cxsparse");
        h[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
        let bat_off = 512 + 1024u64;
        h[16..24].copy_from_slice(&bat_off.to_be_bytes());
        h[24..28].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        h[28..32].copy_from_slice(&(entries as u32).to_be_bytes());
        h[32..36].copy_from_slice(&(block_size as u32).to_be_bytes());
        let c = checksum(&h, 36);
        h[36..40].copy_from_slice(&c.to_be_bytes());
        out.extend(h);
        let bat_len = (entries * 4).div_ceil(512) * 512;
        let mut bat = vec![0xFFu8; bat_len as usize];
        let mut blocks = vec![];
        let mut next = (bat_off + bat_len) / 512;
        for b in 0..entries {
            let start = (b * block_size) as usize;
            let chunk = &data[start..(start + block_size as usize).min(data.len())];
            if chunk.iter().all(|&x| x == 0) {
                continue;
            }
            bat[(b * 4) as usize..(b * 4 + 4) as usize].copy_from_slice(&(next as u32).to_be_bytes());
            let mut bm = vec![0u8; bitmap_len as usize];
            let mut body = vec![0xA5u8; block_size as usize];
            for (s, sec) in chunk.chunks(512).enumerate() {
                if sec.iter().any(|&x| x != 0) {
                    bm[s / 8] |= 0x80 >> (s % 8);
                    body[s * 512..s * 512 + sec.len()].copy_from_slice(sec);
                }
            }
            blocks.extend(bm);
            blocks.extend(body);
            next += (bitmap_len + block_size) / 512;
        }
        out.extend(bat);
        out.extend(blocks);
        out.extend(ft);
        out
    }
}
