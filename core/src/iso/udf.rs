//! UDF reader: ECMA-167 3rd edition (Parts 3 and 4) as profiled by OSTA UDF 1.02 – 2.60.
//!
//! Volume structure (ECMA-167 Part 3):
//! - Volume Recognition Sequence at byte 32768 (2/9.1: BEA01, NSR02 / NSR03, TEA01);
//! - Anchor Volume Descriptor Pointer at sector 256, N-1 or N-256 (3/8.4.2.1, UDF 2.2.3);
//! - Volume Descriptor Sequence (3/8.4): Primary (3/10.1), Implementation Use (3/10.4),
//!   Partition (3/10.5), Logical Volume (3/10.6, with its partition maps 3/10.7), Unallocated
//!   Space (3/10.8), Terminating (3/10.9) and Pointer (3/10.3) descriptors.
//!
//! Partitions: type 1 maps (physical), type 2 maps for Sparable partitions (UDF 2.2.9, the
//! sparing table 2.2.12 is honoured) and Metadata partitions (UDF 2.2.10: logical blocks of the
//! metadata partition are offsets in the metadata file, with its mirror file as a fallback).
//! Virtual partitions (VAT, CD-R) are not supported.
//!
//! File structure (ECMA-167 Part 4): File Set Descriptor (4/14.1), File Entries (4/14.9) and
//! Extended File Entries (4/14.17), ICB strategies 4 and 4096 (4/A.5, 4/14.7 Indirect Entry),
//! short / long / extended allocation descriptors (4/14.14) with Allocation Extent Descriptor
//! continuations (4/14.5), embedded data, File Identifier Descriptors (4/14.4), path components
//! of symbolic links (4/14.16), OSTA CS0 strings (UDF 2.1.1).
//!
//! Everything is untrusted: descriptor tags are verified (3/7.2: checksum, CRC, location), all
//! arithmetic is checked, recursion is replaced by an explicit stack, directory loops are
//! detected, and every allocation is bounded by [`Limits`].

use std::collections::HashSet;

use super::{child_path, epoch, le16, le32, le64, Data, Extent, Limits, Node, Parsed, View};
use crate::block::BlockDev;
use crate::forge::{ForgeError, ForgeResult};

// Tag identifiers (ECMA-167 3/7.2.1 and 4/7.2.1).
const TAG_PVD: u16 = 1;
const TAG_AVDP: u16 = 2;
const TAG_VDP: u16 = 3;
const TAG_PD: u16 = 5;
const TAG_LVD: u16 = 6;
const TAG_TD: u16 = 8;
const TAG_FSD: u16 = 256;
const TAG_FID: u16 = 257;
const TAG_AED: u16 = 258;
const TAG_IE: u16 = 259;
const TAG_FE: u16 = 261;
const TAG_EFE: u16 = 266;

/// Volume Descriptor Pointer hops and descriptors examined (3/8.4.1 allows chaining).
const MAX_VDS_DESCRIPTORS: usize = 1024;
const MAX_VDS_HOPS: usize = 16;
/// Indirect Entries followed for one ICB (strategy 4096, 4/A.5.2).
const MAX_ICB_INDIRECTIONS: usize = 64;
/// Allocation Extent Descriptors followed for one file (4/14.5).
const MAX_AEDS: usize = 65_536;
/// Partition maps in a Logical Volume Descriptor (3/10.6.13).
const MAX_PARTITION_MAPS: usize = 64;
/// Bytes of a symbolic link's path component data (4/14.16).
const MAX_SYMLINK: u64 = 64 * 1024;

fn invalid(msg: impl Into<String>) -> ForgeError {
    ForgeError::Invalid(format!("UDF: {}", msg.into()))
}

fn io(e: std::io::Error) -> ForgeError {
    ForgeError::Io(format!("UDF: {e}"))
}

// ───────────────────────────── descriptor tags ─────────────────────────────

/// CRC of ECMA-167 3/7.2.6: CRC-ITU-T (x^16 + x^12 + x^5 + 1), initial value 0, no reflection.
pub(crate) fn crc_itu_t(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Verify a descriptor tag (3/7.2): checksum of bytes 0–3 and 5–15 (7.2.3), descriptor version
/// 2 or 3 (7.2.2), the CRC over the `CRC length` bytes after the tag (7.2.6, checked when it
/// lies inside `b`, as Linux does) and, when given, the recorded tag location (7.2.8).
/// Returns the tag identifier.
fn check_tag(b: &[u8], location: Option<u32>) -> Option<u16> {
    if b.len() < 16 {
        return None;
    }
    let sum = b[..16].iter().enumerate().filter(|(i, _)| *i != 4).fold(0u8, |s, (_, &x)| s.wrapping_add(x));
    if sum != b[4] {
        return None;
    }
    let version = le16(b, 2);
    if version != 2 && version != 3 {
        return None;
    }
    let crc_len = le16(b, 10) as usize;
    if let Some(body) = b.get(16..16 + crc_len) {
        if crc_itu_t(body) != le16(b, 8) {
            return None;
        }
    }
    if let Some(loc) = location {
        if le32(b, 12) != loc {
            return None;
        }
    }
    Some(le16(b, 0))
}

// ───────────────────────────── strings and times ─────────────────────────────

/// OSTA Compressed Unicode (UDF 2.1.1): compression id 8 (or 254) = one byte per character,
/// 16 (or 255) = big-endian 16-bit units. None for an unknown compression id.
pub(crate) fn cs0(b: &[u8]) -> Option<String> {
    let (&id, rest) = b.split_first()?;
    match id {
        8 | 254 => Some(rest.iter().map(|&c| c as char).collect()),
        16 | 255 => {
            let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

/// A fixed-size dstring field (1/7.2.12): the last byte holds the used length.
fn dstring(b: &[u8]) -> String {
    let Some((&len, body)) = b.split_last() else { return String::new() };
    let len = (len as usize).min(body.len());
    if len == 0 {
        return String::new();
    }
    cs0(&body[..len]).map(|s| s.trim_end_matches(['\0', ' ']).to_string()).unwrap_or_default()
}

/// Timestamp (1/7.3) → seconds since the epoch. Type 1 timestamps carry a timezone offset in
/// minutes (12-bit signed, -2047 = unspecified).
fn timestamp(b: &[u8]) -> i64 {
    if b.len() < 12 {
        return 0;
    }
    let tt = le16(b, 0);
    let mut tz = (tt & 0x0FFF) as i64;
    if tz >= 0x800 {
        tz -= 0x1000;
    }
    let tz = if tt >> 12 == 1 && tz != -2047 && (-1440..=1440).contains(&tz) { tz } else { 0 };
    let year = le16(b, 2) as i16 as i64;
    if year == 0 {
        return 0;
    }
    epoch(year, b[4] as u32, b[5] as u32, b[6] as u32, b[7] as u32, b[8] as u32, tz).unwrap_or(0)
}

/// Permissions (4/14.9.5) and ICB flags (4/14.6.8) → Unix mode bits.
fn unix_mode(perm: u32, icb_flags: u16) -> u32 {
    let mut m = (perm & 0o7) | ((perm >> 2) & 0o70) | ((perm >> 4) & 0o700);
    if icb_flags & 0x40 != 0 {
        m |= 0o4000;
    }
    if icb_flags & 0x80 != 0 {
        m |= 0o2000;
    }
    if icb_flags & 0x100 != 0 {
        m |= 0o1000;
    }
    m
}

// ───────────────────────────── partitions ─────────────────────────────

/// A run of a metadata file: `count` blocks of the metadata partition starting at file block
/// `file_block` live at physical-partition block `phys` (or are not recorded).
#[derive(Clone, Copy, Debug)]
struct MetaRun {
    file_block: u64,
    count: u64,
    phys: Option<u32>,
}

#[derive(Clone, Debug)]
enum Partition {
    /// Type 1 map (3/10.7.2): partition start (absolute sector) and length (blocks).
    Physical { start: u64, len: u64 },
    /// Sparable partition (UDF 2.2.9) with its sparing table entries (UDF 2.2.12):
    /// (original packet start lbn, mapped absolute sector), sorted.
    Sparable { start: u64, len: u64, packet: u32, map: Vec<(u32, u32)> },
    /// Metadata partition (UDF 2.2.10) on top of the physical partition (start, len).
    Metadata { start: u64, len: u64, main: Option<Vec<MetaRun>>, mirror: Option<Vec<MetaRun>> },
}

/// Logical block address (4/7.1 lb_addr).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Lb {
    part: u16,
    lbn: u32,
}

/// One allocation descriptor, before mapping to the device.
#[derive(Clone, Copy, Debug)]
struct RawExt {
    /// Extent type (4/14.14.1.1): 0 recorded, 1 allocated not recorded, 2 not allocated.
    kind: u8,
    len: u64,
    at: Lb,
}

/// A parsed File Entry / Extended File Entry.
struct Icb {
    at: Lb,
    file_type: u8,
    flags: u16,
    info_len: u64,
    mode: u32,
    mtime: i64,
    /// Allocation descriptors (or embedded data) area.
    ads: Vec<u8>,
}

impl Icb {
    /// Allocation descriptor type (4/14.6.8 bits 0–2).
    fn ad_type(&self) -> u16 {
        self.flags & 7
    }
}

struct Vol<'a> {
    dev: &'a dyn BlockDev,
    bs: u64,
    parts: Vec<Partition>,
    lim: Limits,
    extents: usize,
}

impl<'a> Vol<'a> {
    fn read_block_at(&self, pos: u64) -> ForgeResult<Vec<u8>> {
        self.dev.read_vec(pos, self.bs as usize).map_err(io)
    }

    /// Device byte positions of `n` blocks starting at `lbn` of partition `part`, as runs of
    /// (position, blocks). `sparse` runs (unrecorded metadata) are `None`. The metadata mirror
    /// is used when `mirror`.
    fn map_range(&self, part: u16, lbn: u32, n: u64, mirror: bool) -> ForgeResult<Vec<(Option<u64>, u64)>> {
        let p = self.parts.get(part as usize).ok_or_else(|| invalid(format!("partition reference {part} out of range")))?;
        let end = (lbn as u64).checked_add(n).ok_or_else(|| invalid("block range overflows"))?;
        let phys = |start: u64, len: u64, lbn: u64, n: u64| -> ForgeResult<u64> {
            let e = lbn.checked_add(n).ok_or_else(|| invalid("block range overflows"))?;
            if e > len {
                return Err(invalid(format!("block {lbn} (+{n}) beyond its partition ({len} blocks)")));
            }
            start.checked_add(lbn).and_then(|s| s.checked_mul(self.bs)).ok_or_else(|| invalid("block address overflows"))
        };
        match p {
            Partition::Physical { start, len } => Ok(vec![(Some(phys(*start, *len, lbn as u64, n)?), n)]),
            Partition::Sparable { start, len, packet, map } => {
                // UDF 2.2.12: blocks of a packet listed in the sparing table are read from the
                // mapped location instead (same offset within the packet). Only the table
                // entries overlapping the range are visited, so huge extents stay cheap.
                let pk = (*packet).max(1) as u64;
                let mut out: Vec<(Option<u64>, u64)> = vec![];
                let mut b = lbn as u64;
                let first_packet = b - b % pk;
                let mut i = map.partition_point(|&(o, _)| (o as u64) < first_packet);
                while b < end {
                    let spared = map.get(i).filter(|&&(o, _)| (o as u64) < end).map(|&(o, m)| (o as u64, m as u64));
                    match spared {
                        Some((o, m)) if o <= b => {
                            // Inside a spared packet.
                            let run = (o + pk).min(end) - b;
                            let pos = m.checked_add(b - o).and_then(|s| s.checked_mul(self.bs)).ok_or_else(|| invalid("sparing address overflows"))?;
                            out.push((Some(pos), run));
                            b += run;
                            i += 1;
                        }
                        other => {
                            let stop = other.map_or(end, |(o, _)| o.min(end));
                            let run = stop - b;
                            out.push((Some(phys(*start, *len, b, run)?), run));
                            b = stop;
                        }
                    }
                    if out.len() > self.lim.max_extents {
                        return Err(invalid("too many extents"));
                    }
                }
                Ok(out)
            }
            Partition::Metadata { start, len, main, mirror: mir } => {
                let runs = if mirror { mir.as_ref() } else { main.as_ref().or(mir.as_ref()) };
                let runs = runs.ok_or_else(|| invalid("metadata partition without a readable metadata file"))?;
                let mut out: Vec<(Option<u64>, u64)> = vec![];
                let mut b = lbn as u64;
                // Runs are in file order, so a binary search finds the first one.
                let mut i = runs.partition_point(|r| r.file_block + r.count <= b);
                while b < end {
                    let r = runs.get(i).ok_or_else(|| invalid(format!("metadata block {b} beyond the metadata file")))?;
                    if r.file_block > b {
                        return Err(invalid(format!("metadata block {b} not mapped by the metadata file")));
                    }
                    let within = b - r.file_block;
                    let take = (r.count - within).min(end - b);
                    let pos = match r.phys {
                        Some(p) => Some(phys(*start, *len, p as u64 + within, take)?),
                        None => None,
                    };
                    out.push((pos, take));
                    if out.len() > self.lim.max_extents {
                        return Err(invalid("too many extents"));
                    }
                    b += take;
                    i += 1;
                }
                Ok(out)
            }
        }
    }

    /// Read the descriptor block at `at` and verify its tag (location = the logical block
    /// number, 3/7.2.8). For metadata partitions the mirror is tried when the main copy fails.
    fn read_desc(&self, at: Lb) -> ForgeResult<(u16, Vec<u8>)> {
        let mut last = invalid(format!("no valid descriptor at block {} of partition {}", at.lbn, at.part));
        let tries: &[bool] = match self.parts.get(at.part as usize) {
            Some(Partition::Metadata { .. }) => &[false, true],
            _ => &[false],
        };
        for &mirror in tries {
            let runs = match self.map_range(at.part, at.lbn, 1, mirror) {
                Ok(r) => r,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            let Some(&(Some(pos), _)) = runs.first() else { continue };
            let b = match self.read_block_at(pos) {
                Ok(b) => b,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            if let Some(id) = check_tag(&b, Some(at.lbn)) {
                return Ok((id, b));
            }
        }
        Err(last)
    }

    /// Read a File Entry / Extended File Entry, following Indirect Entries (strategy 4096).
    fn read_icb(&self, mut at: Lb) -> ForgeResult<Icb> {
        let mut seen = HashSet::new();
        for _ in 0..MAX_ICB_INDIRECTIONS {
            if !seen.insert(at) {
                return Err(invalid("ICB indirection loop"));
            }
            let (id, b) = self.read_desc(at)?;
            if id == TAG_IE {
                // 4/14.7: the indirect ICB (long_ad at 36) points at the next ICB extent.
                at = long_ad_lb(&b, 36);
                continue;
            }
            if id != TAG_FE && id != TAG_EFE {
                return Err(invalid(format!("expected a file entry at block {}, found tag {id}", at.lbn)));
            }
            // ICB tag (4/14.6) at 16: strategy type at 20.
            let strategy = le16(&b, 20);
            if strategy == 4096 {
                // 4/A.5.2 (strategy 4096): the entry may be followed by an Indirect Entry that
                // leads to a more recent one (Linux follows it the same way).
                if let Some(next) = at.lbn.checked_add(1) {
                    if let Ok((TAG_IE, ie)) = self.read_desc(Lb { part: at.part, lbn: next }) {
                        let target = long_ad_lb(&ie, 36);
                        if le32(&ie, 36) & 0x3FFF_FFFF != 0 && !seen.contains(&target) {
                            at = target;
                            continue;
                        }
                    }
                }
            } else if strategy != 4 {
                return Err(invalid(format!("unsupported ICB strategy {strategy}")));
            }
            return self.parse_fe(at, id, &b);
        }
        Err(invalid("too many ICB indirections"))
    }

    fn parse_fe(&self, at: Lb, id: u16, b: &[u8]) -> ForgeResult<Icb> {
        // FE (4/14.9): L_EA at 168, L_AD at 172, data at 176; mtime at 84.
        // EFE (4/14.17): L_EA at 208, L_AD at 212, data at 216; mtime at 92.
        let (base, l_ea, l_ad, mtime_off) = if id == TAG_FE { (176usize, le32(b, 168), le32(b, 172), 84) } else { (216, le32(b, 208), le32(b, 212), 92) };
        let start = base.checked_add(l_ea as usize).ok_or_else(|| invalid("file entry lengths overflow"))?;
        let end = start.checked_add(l_ad as usize).ok_or_else(|| invalid("file entry lengths overflow"))?;
        let ads = b.get(start..end).ok_or_else(|| invalid(format!("file entry at block {} longer than a block", at.lbn)))?.to_vec();
        let flags = le16(b, 34);
        Ok(Icb {
            at,
            file_type: b[27],
            flags,
            info_len: le64(b, 56),
            mode: unix_mode(le32(b, 44), flags),
            mtime: b.get(mtime_off..mtime_off + 12).map(timestamp).unwrap_or(0),
            ads,
        })
    }

    /// Allocation descriptors of an ICB, following AED continuations (4/14.5, 4/14.14.1.1
    /// type 3). Stops at a zero-length descriptor or once `info_len` is covered.
    fn raw_extents(&mut self, icb: &Icb) -> ForgeResult<Vec<RawExt>> {
        let size = match icb.ad_type() {
            0 => 8,
            1 => 16,
            2 => 20,
            t => return Err(invalid(format!("allocation descriptor type {t} has no descriptors"))),
        };
        let mut out = vec![];
        let mut area = icb.ads.clone();
        let mut covered = 0u64;
        let mut seen = HashSet::new();
        let mut hops = 0usize;
        'areas: loop {
            let mut p = 0usize;
            while p + size <= area.len() {
                let d = &area[p..p + size];
                p += size;
                let raw = le32(d, 0);
                let len = (raw & 0x3FFF_FFFF) as u64;
                let kind = (raw >> 30) as u8;
                if len == 0 {
                    break 'areas;
                }
                let at = match size {
                    // short_ad (4/14.14.1): position within the ICB's partition.
                    8 => Lb { part: icb.at.part, lbn: le32(d, 4) },
                    // long_ad (4/14.14.2): lb_addr at 4.
                    16 => long_ad_lb(d, 0),
                    // ext_ad (4/14.14.3): lb_addr at 12.
                    _ => Lb { part: le16(d, 16), lbn: le32(d, 12) },
                };
                if kind == 3 {
                    // Next extent of allocation descriptors: an AED.
                    hops += 1;
                    if hops > MAX_AEDS || !seen.insert(at) {
                        return Err(invalid("allocation extent descriptor loop"));
                    }
                    let (id, b) = self.read_desc(at)?;
                    if id != TAG_AED {
                        return Err(invalid(format!("expected an allocation extent descriptor, found tag {id}")));
                    }
                    let l_ad = le32(&b, 20) as usize;
                    area = b.get(24..24usize.saturating_add(l_ad)).ok_or_else(|| invalid("allocation extent descriptor longer than a block"))?.to_vec();
                    continue 'areas;
                }
                self.extents += 1;
                if self.extents > self.lim.max_extents {
                    return Err(invalid("too many allocation descriptors"));
                }
                out.push(RawExt { kind, len, at });
                covered = covered.saturating_add(len);
                if covered >= icb.info_len {
                    break 'areas;
                }
            }
            break;
        }
        Ok(out)
    }

    /// Where the bytes of a file are: embedded, or extents mapped to the device.
    fn file_data(&mut self, icb: &Icb) -> ForgeResult<Data> {
        if icb.ad_type() == 3 {
            // Embedded data (4/14.6.8): the descriptor area holds the file itself.
            let n = (icb.info_len.min(icb.ads.len() as u64)) as usize;
            return Ok(Data::Inline(icb.ads[..n].to_vec()));
        }
        let raw = self.raw_extents(icb)?;
        let mut out: Vec<Extent> = vec![];
        let mut left = icb.info_len;
        for r in raw {
            if left == 0 {
                break;
            }
            let len = r.len.min(left);
            left -= len;
            if r.kind != 0 {
                out.push(Extent { pos: 0, len, sparse: true });
                continue;
            }
            let blocks = len.div_ceil(self.bs);
            let mut remaining = len;
            for (pos, n) in self.map_range(r.at.part, r.at.lbn, blocks, false)? {
                let bytes = n.saturating_mul(self.bs).min(remaining);
                remaining -= bytes;
                match pos {
                    Some(pos) => {
                        if pos.checked_add(bytes).map_or(true, |e| e > self.dev.size()) {
                            return Err(invalid("file data beyond the end of the image"));
                        }
                        out.push(Extent { pos, len: bytes, sparse: false });
                    }
                    None => out.push(Extent { pos: 0, len: bytes, sparse: true }),
                }
            }
            if out.len() > self.lim.max_extents {
                return Err(invalid("too many extents"));
            }
        }
        Ok(Data::Extents(out))
    }

    /// The whole content of a (small) file: directories and symbolic links.
    fn read_small(&mut self, icb: &Icb, max: u64) -> ForgeResult<Vec<u8>> {
        if icb.info_len > max {
            return Err(invalid(format!("directory or link at block {} too large ({} bytes)", icb.at.lbn, icb.info_len)));
        }
        let mut out = vec![0u8; icb.info_len as usize];
        match self.file_data(icb)? {
            Data::None => {}
            Data::Inline(d) => out[..d.len()].copy_from_slice(&d),
            Data::Extents(ex) => {
                let mut off = 0usize;
                for e in ex {
                    let n = (e.len as usize).min(out.len() - off);
                    if !e.sparse {
                        self.dev.read_at(e.pos, &mut out[off..off + n]).map_err(io)?;
                    }
                    off += n;
                    if off >= out.len() {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    /// Load a metadata (or mirror) file (UDF 2.2.13): an (E)FE in the physical partition
    /// whose short_ads map the metadata partition onto it.
    fn metadata_runs(&mut self, phys_ref: u16, lbn: u32) -> Option<Vec<MetaRun>> {
        if lbn == u32::MAX {
            return None;
        }
        let icb = self.read_icb(Lb { part: phys_ref, lbn }).ok()?;
        if !matches!(icb.file_type, 250 | 251) || icb.ad_type() > 2 {
            return None;
        }
        let raw = self.raw_extents(&icb).ok()?;
        let mut runs = vec![];
        let mut fb = 0u64;
        for r in raw {
            let count = r.len.div_ceil(self.bs);
            if r.at.part != phys_ref && icb.ad_type() != 0 {
                return None;
            }
            runs.push(MetaRun { file_block: fb, count, phys: (r.kind == 0).then_some(r.at.lbn) });
            fb = fb.checked_add(count)?;
        }
        Some(runs)
    }
}

fn long_ad_lb(b: &[u8], o: usize) -> Lb {
    Lb { lbn: le32(b, o + 4), part: le16(b, o + 8) }
}

// ───────────────────────────── volume structure ─────────────────────────────

/// Find a valid Anchor Volume Descriptor Pointer (3/10.2): returns (block size, AVDP block).
fn find_anchor(dev: &dyn BlockDev) -> Option<(u64, Vec<u8>)> {
    for bs in [2048u64, 512, 4096, 1024] {
        let n = dev.size() / bs;
        if n < 257 {
            continue;
        }
        for sector in [256, n - 1, n - 256] {
            let Ok(b) = dev.read_vec(sector * bs, bs as usize) else { continue };
            if sector <= u32::MAX as u64 && check_tag(&b, Some(sector as u32)) == Some(TAG_AVDP) {
                return Some((bs, b));
            }
        }
    }
    None
}

/// Whether a Volume Recognition Sequence (ECMA-167 2/9.1) with an NSR descriptor is present.
fn has_nsr(dev: &dyn BlockDev, bs: u64) -> bool {
    let stride = bs.max(2048);
    let mut pos = 32768u64;
    for _ in 0..64 {
        let Ok(b) = dev.read_vec(pos, 7) else { return false };
        match &b[1..6] {
            b"NSR02" | b"NSR03" => return true,
            b"BEA01" | b"TEA01" | b"CD001" | b"CDW02" | b"BOOT2" => {}
            _ => return false,
        }
        pos += stride;
    }
    false
}

pub(crate) fn probe(dev: &dyn BlockDev) -> bool {
    find_anchor(dev).is_some_and(|(bs, _)| has_nsr(dev, bs))
}

/// What the Volume Descriptor Sequence gave.
#[derive(Default)]
struct Vds {
    pvd: Option<(u32, Vec<u8>)>,
    lvd: Option<(u32, Vec<u8>)>,
    /// Partition number → (VDS number, PD).
    pds: Vec<(u16, u32, Vec<u8>)>,
}

/// Read a VDS extent (3/8.4), following Volume Descriptor Pointers. The prevailing descriptor
/// of each kind is the one with the highest Volume Descriptor Sequence Number (3/8.4.2).
fn read_vds(dev: &dyn BlockDev, bs: u64, mut loc: u64, mut len: u64) -> Vds {
    let mut v = Vds::default();
    let mut count = 0usize;
    let mut seen = HashSet::new();
    for _ in 0..MAX_VDS_HOPS {
        if !seen.insert(loc) {
            break;
        }
        let blocks = len / bs;
        let mut next = None;
        for i in 0..blocks {
            count += 1;
            if count > MAX_VDS_DESCRIPTORS {
                return v;
            }
            let sector = loc + i;
            let Ok(b) = dev.read_vec(sector * bs, bs as usize) else { return v };
            let Some(id) = check_tag(&b, u32::try_from(sector).ok()) else { return v };
            let seq = le32(&b, 16);
            match id {
                TAG_PVD if v.pvd.as_ref().map_or(true, |(s, _)| seq >= *s) => v.pvd = Some((seq, b)),
                TAG_LVD if v.lvd.as_ref().map_or(true, |(s, _)| seq >= *s) => v.lvd = Some((seq, b)),
                TAG_PD => {
                    let num = le16(&b, 22);
                    match v.pds.iter_mut().find(|(n, _, _)| *n == num) {
                        Some(e) if seq >= e.1 => *e = (num, seq, b),
                        Some(_) => {}
                        None => v.pds.push((num, seq, b)),
                    }
                }
                TAG_VDP => {
                    // 3/10.3: next VDS extent at 20 (extent_ad: length, location).
                    next = Some((le32(&b, 24) as u64, le32(&b, 20) as u64));
                    break;
                }
                TAG_TD => return v,
                _ => {}
            }
        }
        match next {
            Some((l, n)) if n > 0 => {
                loc = l;
                len = n;
            }
            _ => break,
        }
    }
    v
}

/// Build the partition table from the LVD's partition maps (3/10.7, UDF 2.2.8 – 2.2.10).
fn partitions(dev: &dyn BlockDev, bs: u64, lvd: &[u8], pds: &[(u16, u32, Vec<u8>)]) -> ForgeResult<Vec<Partition>> {
    let map_len = le32(lvd, 264) as usize;
    let n_maps = le32(lvd, 268) as usize;
    if n_maps == 0 || n_maps > MAX_PARTITION_MAPS {
        return Err(invalid(format!("{n_maps} partition maps")));
    }
    let table = lvd.get(440..440usize.saturating_add(map_len)).ok_or_else(|| invalid("partition map table longer than the descriptor"))?;
    let pd_of = |num: u16| -> ForgeResult<(u64, u64)> {
        let (_, _, pd) = pds.iter().find(|(n, _, _)| *n == num).ok_or_else(|| invalid(format!("no partition descriptor for partition {num}")))?;
        // 3/10.5: starting location at 188, length at 192 (blocks).
        Ok((le32(pd, 188) as u64, le32(pd, 192) as u64))
    };
    let mut out = vec![];
    let mut p = 0usize;
    for _ in 0..n_maps {
        let ty = *table.get(p).ok_or_else(|| invalid("partition map table truncated"))?;
        let len = *table.get(p + 1).ok_or_else(|| invalid("partition map table truncated"))? as usize;
        let m = table.get(p..p + len).filter(|_| len >= 2).ok_or_else(|| invalid("partition map truncated"))?;
        match (ty, len) {
            (1, 6) => {
                let (start, plen) = pd_of(le16(m, 4))?;
                out.push(Partition::Physical { start, len: plen });
            }
            (2, 64) => {
                // Type 2 map (3/10.7.3): partition type identifier (regid) at 4.
                let ident = &m[5..28];
                let num = le16(m, 38);
                let (start, plen) = pd_of(num)?;
                if ident.starts_with(b"*UDF Metadata Partition") {
                    // UDF 2.2.10: metadata file / mirror file locations at 40 / 44.
                    out.push(Partition::Metadata { start, len: plen, main: None, mirror: None });
                } else if ident.starts_with(b"*UDF Sparable Partition") {
                    let packet = le16(m, 40) as u32;
                    let n_tables = (m[42] as usize).min(4);
                    let table_size = le32(m, 44) as u64;
                    let mut map = vec![];
                    for k in 0..n_tables {
                        if let Some(entries) = sparing_table(dev, bs, le32(m, 48 + 4 * k) as u64, table_size) {
                            map = entries;
                            break;
                        }
                    }
                    out.push(Partition::Sparable { start, len: plen, packet: if packet.is_power_of_two() { packet } else { 32 }, map });
                } else if ident.starts_with(b"*UDF Virtual Partition") {
                    return Err(ForgeError::Unsupported("UDF: virtual partitions (VAT, write-once discs) are not supported".into()));
                } else {
                    return Err(ForgeError::Unsupported(format!("UDF: unknown partition map type \"{}\"", String::from_utf8_lossy(ident).trim_end_matches('\0'))));
                }
            }
            _ => return Err(invalid(format!("partition map of type {ty} and length {len}"))),
        }
        p += len;
    }
    Ok(out)
}

/// Sparing table (UDF 2.2.12) at absolute sector `loc`: sorted (original, mapped) pairs.
fn sparing_table(dev: &dyn BlockDev, bs: u64, loc: u64, size: u64) -> Option<Vec<(u32, u32)>> {
    let size = size.clamp(bs, 1 << 20);
    let b = dev.read_vec(loc.checked_mul(bs)?, size as usize).ok()?;
    // Tag identifier 0 per UDF 2.2.12; the CRC may cover more than one block.
    check_tag(&b, u32::try_from(loc).ok())?;
    if !b.get(17..35)?.starts_with(b"*UDF Sparing Table") {
        return None;
    }
    let n = le16(&b, 48) as usize;
    let mut out = vec![];
    for i in 0..n {
        let o = 56 + i * 8;
        if o + 8 > b.len() {
            break;
        }
        let orig = le32(&b, o);
        if orig >= 0xFFFF_FFF0 {
            continue;
        }
        out.push((orig, le32(&b, o + 4)));
    }
    out.sort_unstable();
    Some(out)
}

/// Parse the UDF file set into a tree.
pub(crate) fn read(dev: &dyn BlockDev, lim: &Limits) -> ForgeResult<Parsed> {
    let (bs, avdp) = find_anchor(dev).ok_or_else(|| invalid("no anchor volume descriptor"))?;
    // 3/10.2: main VDS extent at 16, reserve VDS extent at 24 (extent_ad: length, location).
    let main = read_vds(dev, bs, le32(&avdp, 20) as u64, le32(&avdp, 16) as u64);
    let vds = if main.lvd.is_some() && !main.pds.is_empty() { main } else { read_vds(dev, bs, le32(&avdp, 28) as u64, le32(&avdp, 24) as u64) };
    let (_, lvd) = vds.lvd.as_ref().ok_or_else(|| invalid("no logical volume descriptor"))?;
    if le32(lvd, 212) as u64 != bs {
        return Err(invalid(format!("logical block size {} differs from the sector size {bs}", le32(lvd, 212))));
    }
    let parts = partitions(dev, bs, lvd, &vds.pds)?;
    let mut vol = Vol { dev, bs, parts, lim: *lim, extents: 0 };

    // Metadata partitions: load the metadata file and its mirror through the physical
    // partition with the same partition number (UDF 2.2.10).
    let n_maps = le32(lvd, 268) as usize;
    let table = lvd.get(440..).unwrap_or(&[]);
    let mut p = 0usize;
    for i in 0..n_maps.min(vol.parts.len()) {
        let len = table.get(p + 1).copied().unwrap_or(0) as usize;
        if matches!(vol.parts[i], Partition::Metadata { .. }) {
            let m = table.get(p..p + len).unwrap_or(&[]);
            let num = le16(m, 38);
            let (start, plen) = match &vol.parts[i] {
                Partition::Metadata { start, len, .. } => (*start, *len),
                _ => unreachable!(),
            };
            // A synthetic physical partition reference to read the metadata files with.
            let phys_ref = vol.parts.len() as u16;
            vol.parts.push(Partition::Physical { start, len: plen });
            let main = vol.metadata_runs(phys_ref, le32(m, 40));
            let mirror = vol.metadata_runs(phys_ref, le32(m, 44));
            vol.parts.pop();
            if main.is_none() && mirror.is_none() {
                return Err(invalid(format!("metadata partition {num}: neither the metadata file nor its mirror is readable")));
            }
            vol.parts[i] = Partition::Metadata { start, len: plen, main, mirror };
        }
        p += len;
    }

    // 3/10.6: logical volume identifier at 84 (dstring 128); File Set Descriptor long_ad at 248.
    let mut label = dstring(&lvd[84..212]);
    if label.is_empty() {
        if let Some((_, pvd)) = &vds.pvd {
            label = dstring(&pvd[24..56]);
        }
    }
    let fsd_at = long_ad_lb(lvd, 248);
    let (id, fsd) = vol.read_desc(fsd_at)?;
    if id != TAG_FSD {
        return Err(invalid(format!("expected the file set descriptor, found tag {id}")));
    }
    // 4/14.1: root directory ICB long_ad at 400.
    let root = long_ad_lb(&fsd, 400);
    let nodes = walk(&mut vol, root)?;
    Ok(Parsed { view: View::Udf, label, nodes })
}

/// Walk the directory hierarchy from `root` (explicit stack, parents before children).
fn walk(vol: &mut Vol<'_>, root: Lb) -> ForgeResult<Vec<Node>> {
    let lim = vol.lim;
    let mut nodes: Vec<Node> = vec![];
    let mut visited: HashSet<Lb> = HashSet::new();
    let root_icb = vol.read_icb(root)?;
    if root_icb.file_type != 4 {
        return Err(invalid("the root is not a directory"));
    }
    visited.insert(root_icb.at);
    let mut stack: Vec<(String, Icb, usize)> = vec![(String::new(), root_icb, 0)];
    while let Some((path, dir, depth)) = stack.pop() {
        let data = vol.read_small(&dir, lim.max_dir_bytes)?;
        let mut names: HashSet<String> = HashSet::new();
        let mut p = 0usize;
        while p < data.len() {
            // FID (4/14.4): characteristics at 18, L_FI at 19, ICB long_ad at 20, L_IU at 36,
            // implementation use at 38, then the identifier; padded to 4 bytes (4/14.4.9).
            let rest = &data[p..];
            if rest.len() < 38 {
                if rest.iter().all(|&b| b == 0) {
                    break;
                }
                return Err(invalid(format!("{}: truncated file identifier", display(&path))));
            }
            let l_fi = rest[19] as usize;
            let l_iu = le16(rest, 36) as usize;
            let len = (38 + l_iu + l_fi + 3) & !3;
            let Some(fid) = rest.get(..len.min(rest.len())).filter(|_| len <= rest.len()) else {
                return Err(invalid(format!("{}: file identifier past the end of the directory", display(&path))));
            };
            if check_tag(fid, None) != Some(TAG_FID) {
                if fid.iter().all(|&b| b == 0) {
                    break; // trailing zero padding
                }
                return Err(invalid(format!("{}: corrupt file identifier descriptor", display(&path))));
            }
            p += len;
            let chars = fid[18];
            // Bit 2: deleted, bit 3: parent, bit 4: metadata (stream) — not listed.
            if chars & (0x04 | 0x08 | 0x10) != 0 {
                continue;
            }
            let Some(name) = cs0(&fid[38 + l_iu..38 + l_iu + l_fi]) else { continue };
            let Some(child) = child_path(if path.is_empty() { "/" } else { &path }, &name, &lim) else { continue };
            if !names.insert(child.clone()) {
                continue; // duplicate name
            }
            let at = long_ad_lb(fid, 20);
            let icb = vol.read_icb(at).map_err(|e| invalid(format!("{child}: {e}")))?;
            if nodes.len() >= lim.max_nodes {
                return Err(invalid("too many files"));
            }
            match icb.file_type {
                // 4/14.6.6: 4 directory, 5 file, 12 symbolic link.
                4 => {
                    if depth + 1 > lim.max_depth {
                        return Err(invalid(format!("{child}: directories nested too deeply")));
                    }
                    if !visited.insert(icb.at) {
                        continue; // a directory reached twice: loop (or illegal hard link)
                    }
                    nodes.push(Node { path: child.clone(), is_dir: true, size: 0, mode: icb.mode, mtime: icb.mtime, symlink: None, data: Data::None });
                    stack.push((child, icb, depth + 1));
                }
                5 => {
                    let data = vol.file_data(&icb).map_err(|e| invalid(format!("{child}: {e}")))?;
                    nodes.push(Node { path: child, is_dir: false, size: icb.info_len, mode: icb.mode, mtime: icb.mtime, symlink: None, data });
                }
                12 => {
                    let raw = vol.read_small(&icb, MAX_SYMLINK)?;
                    let target = symlink_target(&raw);
                    nodes.push(Node { path: child, is_dir: false, size: target.len() as u64, mode: icb.mode, mtime: icb.mtime, symlink: Some(target), data: Data::None });
                }
                _ => {} // devices, FIFOs, sockets, streams: nothing to copy
            }
        }
    }
    Ok(nodes)
}

fn display(path: &str) -> &str {
    if path.is_empty() {
        "/"
    } else {
        path
    }
}

/// Path components of a symbolic link (4/14.16.1) → a POSIX path, as Linux renders them:
/// type 1 (with no identifier) and 2 → root, 3 → "..", 4 → ".", 5 → a name.
fn symlink_target(b: &[u8]) -> String {
    let mut out = String::new();
    let mut p = 0usize;
    while p + 4 <= b.len() {
        let ty = b[p];
        let l = b[p + 1] as usize;
        let Some(ident) = b.get(p + 4..p + 4 + l) else { break };
        p += 4 + l;
        match ty {
            1 if l > 0 => {}
            1 | 2 => {
                out.clear();
                out.push('/');
            }
            3 => out.push_str("../"),
            4 => out.push_str("./"),
            5 => {
                out.push_str(&cs0(ident).unwrap_or_default());
                out.push('/');
            }
            _ => {}
        }
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemDev;
    use crate::iso::testimg::udf::{AdKind, UdfBuilder};
    use std::sync::Arc;

    fn parse(img: Vec<u8>) -> ForgeResult<Parsed> {
        let dev = MemDev::from_vec(img);
        read(&dev, &Limits::default())
    }

    fn content(img: &[u8], n: &Node) -> Vec<u8> {
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::from_vec(img.to_vec()));
        let mut out = vec![0u8; n.size as usize];
        let mut off = 0usize;
        while off < out.len() {
            let k = super::super::read_node(dev.as_ref(), n, off as u64, &mut out[off..]).unwrap();
            assert!(k > 0);
            off += k;
        }
        out
    }

    fn pattern(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u32).wrapping_mul(2_654_435_761).rotate_left(seed as u32 % 31) as u8 ^ seed).collect()
    }

    fn sample(rev: u16, bs: u32) -> (UdfBuilder, Vec<(String, Vec<u8>)>) {
        sample_with(rev, bs, true)
    }

    /// `ext`: include a file with extended ADs (OSTA UDF 2.3.10 forbids them on interchange media
    /// and Linux can't read them, so the kernel comparison leaves them out).
    fn sample_with(rev: u16, bs: u32, ext: bool) -> (UdfBuilder, Vec<(String, Vec<u8>)>) {
        let mut b = UdfBuilder::new(rev, bs);
        b.label("Test Volume");
        let files = vec![
            ("/readme.txt".to_string(), b"hello UDF\n".to_vec()),
            ("/sources/install.wim".to_string(), pattern(300_000, 3)),
            ("/sources/boot/bcd".to_string(), pattern(5000, 9)),
            ("/efi/boot/bootx64.efi".to_string(), pattern(70_001, 1)),
            ("/Città/naïve 日本.txt".to_string(), pattern(123, 5)),
            ("/empty".to_string(), vec![]),
        ];
        for (p, d) in &files {
            b.file(p, d.clone());
        }
        b.file_with("/ad/short.bin", pattern(9000, 11), AdKind::Short);
        b.file_with("/ad/long.bin", pattern(9001, 12), AdKind::Long);
        if ext {
            b.file_with("/ad/ext.bin", pattern(9002, 13), AdKind::Ext);
        }
        b.file_with("/ad/inline.bin", pattern(40, 14), AdKind::Inline);
        b.dir("/emptydir");
        b.symlink("/link-abs", "/sources/install.wim");
        b.symlink("/ad/link-rel", "../readme.txt");
        let mut all = files;
        all.push(("/ad/short.bin".into(), pattern(9000, 11)));
        all.push(("/ad/long.bin".into(), pattern(9001, 12)));
        if ext {
            all.push(("/ad/ext.bin".into(), pattern(9002, 13)));
        }
        all.push(("/ad/inline.bin".into(), pattern(40, 14)));
        (b, all)
    }

    fn check_sample(rev: u16, bs: u32, meta: Option<bool>) {
        let (mut b, files) = sample(rev, bs);
        if let Some(m) = meta {
            b.metadata_partition(m);
        }
        let img = b.build();
        let dev = MemDev::from_vec(img.clone());
        assert!(probe(&dev), "rev {rev:x} bs {bs}");
        let p = parse(img.clone()).unwrap_or_else(|e| panic!("rev {rev:x} bs {bs}: {e}"));
        assert_eq!(p.label, "Test Volume");
        assert_eq!(p.view, View::Udf);
        for (path, data) in &files {
            let n = p.nodes.iter().find(|n| &n.path == path).unwrap_or_else(|| panic!("{path} missing (rev {rev:x} bs {bs}): {:?}", p.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()));
            assert_eq!(n.size, data.len() as u64, "{path}");
            assert_eq!(&content(&img, n), data, "{path} rev {rev:x} bs {bs}");
            assert_eq!(n.mode, 0o644, "{path}");
            assert_eq!(n.mtime, 1_700_000_000, "{path}");
        }
        // Parents before children; directories present.
        for (i, n) in p.nodes.iter().enumerate() {
            let parent = &n.path[..n.path.rfind('/').unwrap()];
            if !parent.is_empty() {
                let pi = p.nodes.iter().position(|m| m.path == parent).unwrap();
                assert!(pi < i && p.nodes[pi].is_dir, "{}", n.path);
            }
        }
        let d = p.nodes.iter().find(|n| n.path == "/emptydir").unwrap();
        assert!(d.is_dir && d.mode == 0o755);
        let l = p.nodes.iter().find(|n| n.path == "/link-abs").unwrap();
        assert_eq!(l.symlink.as_deref(), Some("/sources/install.wim"));
        let l = p.nodes.iter().find(|n| n.path == "/ad/link-rel").unwrap();
        assert_eq!(l.symlink.as_deref(), Some("../readme.txt"));
    }

    #[test]
    fn crc_matches_the_ecma_167_example() {
        // ECMA-167 3/7.2.6 example: the CRC of bytes 70 6A 77 is 0x3299.
        assert_eq!(crc_itu_t(&[0x70, 0x6A, 0x77]), 0x3299);
    }

    #[test]
    fn round_trips_every_revision_and_block_size() {
        for rev in [0x0102u16, 0x0150, 0x0201, 0x0250, 0x0260] {
            for bs in [512u32, 2048] {
                check_sample(rev, bs, None);
            }
        }
        check_sample(0x0201, 4096, None);
        check_sample(0x0201, 1024, None);
        // 2.50 without a metadata partition is legal too.
        check_sample(0x0250, 2048, Some(false));
    }

    #[test]
    fn metadata_partition_falls_back_to_the_mirror() {
        let (mut b, files) = sample(0x0250, 2048);
        b.metadata_partition(true);
        let (mut img, map) = b.build_with_map();
        let meta = map.meta_file.unwrap() as usize;
        img[meta + 5..meta + 16].fill(0xAA); // breaks the tag checksum
        let p = parse(img.clone()).unwrap();
        for (path, data) in &files {
            let n = p.nodes.iter().find(|n| &n.path == path).unwrap();
            assert_eq!(&content(&img, n), data);
        }
        // Damage a directory's ICB inside the main metadata area only: the mirror copy wins.
        let (mut b2, _) = sample(0x0260, 2048);
        b2.metadata_partition(true);
        let (mut img2, map2) = b2.build_with_map();
        let icb = map2.icb["/sources"] as usize;
        img2[icb + 2] ^= 0xFF;
        let p2 = parse(img2).unwrap();
        assert!(p2.nodes.iter().any(|n| n.path == "/sources/install.wim"));
        // Both damaged: an error, not a panic.
        let (mut img3, map3) = b.build_with_map();
        for o in [map3.meta_file.unwrap(), map3.mirror_file.unwrap()] {
            img3[o as usize + 1] ^= 0x55;
        }
        assert!(parse(img3).is_err());
    }

    #[test]
    fn files_over_4_gib_with_holes_and_data_beyond_4_gib() {
        for (rev, bs) in [(0x0250u16, 2048u32), (0x0201, 512), (0x0102, 2048)] {
            let mut b = UdfBuilder::new(rev, bs);
            let size = 5u64 << 30;
            let far = (4u64 << 30) + (1 << 29); // 4.5 GiB
            let pieces = vec![(0u64, pattern(10_000, 1)), (far, pattern(70_000, 2)), (size - 4096, pattern(4096, 3))];
            b.sparse_file("/big/huge.bin", size, pieces.clone());
            b.file("/small", b"x".to_vec());
            let img = b.build();
            assert!(img.len() < 64 << 20, "image stays small: {}", img.len());
            let p = parse(img.clone()).unwrap();
            let n = p.nodes.iter().find(|n| n.path == "/big/huge.bin").unwrap();
            assert_eq!(n.size, size);
            let dev = MemDev::from_vec(img.clone());
            for (off, data) in &pieces {
                let mut buf = vec![0u8; data.len()];
                let k = super::super::read_node(&dev, n, *off, &mut buf).unwrap();
                assert_eq!(k, data.len());
                assert_eq!(&buf, data, "rev {rev:x} at {off}");
            }
            // Holes read as zeros, including right after 4 GiB.
            let mut buf = vec![1u8; 8192];
            super::super::read_node(&dev, n, 4u64 << 30, &mut buf).unwrap();
            assert!(buf.iter().all(|&b| b == 0));
            let mut buf = vec![1u8; 100];
            super::super::read_node(&dev, n, far + 70_000, &mut buf).unwrap();
            assert!(buf.iter().all(|&b| b == 0));
            // Reading at the very end.
            let mut buf = vec![0u8; 10];
            assert_eq!(super::super::read_node(&dev, n, size - 10, &mut buf).unwrap(), 10);
            assert_eq!(&buf[..], &pieces[2].1[4086..]);
        }
    }

    #[test]
    fn fragmented_files_use_allocation_extent_descriptors() {
        for (rev, bs, ad) in [(0x0201u16, 512u32, AdKind::Long), (0x0201, 512, AdKind::Short), (0x0250, 512, AdKind::Ext), (0x0102, 2048, AdKind::Short)] {
            let mut b = UdfBuilder::new(rev, bs);
            b.fragment(1);
            let data = pattern(bs as usize * 300 + 17, 7);
            b.file_with("/frag.bin", data.clone(), ad);
            let (img, map) = b.build_with_map();
            assert!(map.aeds > 0, "AEDs used");
            let p = parse(img.clone()).unwrap();
            let n = p.nodes.iter().find(|n| n.path == "/frag.bin").unwrap();
            assert_eq!(content(&img, n), data, "{ad:?} bs {bs}");
        }
    }

    #[test]
    fn strategy_4096_indirect_entries_are_followed() {
        let mut b = UdfBuilder::new(0x0201, 2048);
        b.indirect_icbs(true);
        b.file("/a/b.txt", b"current version".to_vec());
        let img = b.build();
        let p = parse(img.clone()).unwrap();
        let n = p.nodes.iter().find(|n| n.path == "/a/b.txt").unwrap();
        assert_eq!(content(&img, n), b"current version");
    }

    #[test]
    fn deep_and_wide_trees() {
        let mut b = UdfBuilder::new(0x0250, 2048);
        let mut path = String::new();
        for i in 0..40 {
            path.push_str(&format!("/d{i}"));
        }
        b.file(&format!("{path}/leaf"), b"deep".to_vec());
        for i in 0..300 {
            b.file(&format!("/wide/file-with-a-longish-name-{i:04}.dat"), vec![i as u8; i % 7]);
        }
        let img = b.build();
        let p = parse(img.clone()).unwrap();
        assert!(p.nodes.iter().any(|n| n.path == format!("{path}/leaf")));
        assert_eq!(p.nodes.iter().filter(|n| n.path.starts_with("/wide/")).count(), 300);
        let lim = Limits { max_depth: 10, ..Limits::default() };
        assert!(read(&MemDev::from_vec(img.clone()), &lim).is_err());
        let lim = Limits { max_nodes: 50, ..Limits::default() };
        assert!(read(&MemDev::from_vec(img), &lim).is_err());
    }

    /// Re-write a descriptor's tag checksum and CRC after a deliberate change.
    fn retag(img: &mut [u8], off: usize, bs: usize) {
        let crc_len = le16(&img[off..], 10) as usize;
        let end = (off + 16 + crc_len).min(off + bs).min(img.len());
        let crc = crc_itu_t(&img[off + 16..end]);
        img[off + 8..off + 10].copy_from_slice(&crc.to_le_bytes());
        let sum = img[off..off + 16].iter().enumerate().filter(|(i, _)| *i != 4).fold(0u8, |s, (_, &x)| s.wrapping_add(x));
        img[off + 4] = sum;
    }

    #[test]
    fn loops_are_detected() {
        // A FID of /a/b pointing back at /a (an ancestor): skipped, no hang.
        let mut b = UdfBuilder::new(0x0201, 2048);
        b.dirs_inline(false);
        b.file("/a/b/c.txt", b"c".to_vec());
        let (mut img, map) = b.build_with_map();
        let dir_b = map.dir_data["/a/b"] as usize;
        let icb_a_lbn = map.icb_lbn["/a"];
        // Second FID of /a/b's data is c.txt (first is the parent FID): point it at /a, as a dir.
        let first_len = (38 + le16(&img[dir_b..], 36) as usize + img[dir_b + 19] as usize + 3) & !3;
        let fid = dir_b + first_len;
        img[fid + 18] = 0x02;
        img[fid + 24..fid + 28].copy_from_slice(&icb_a_lbn.to_le_bytes());
        retag(&mut img, fid, 2048);
        let p = parse(img).unwrap();
        assert!(p.nodes.iter().all(|n| !n.path.starts_with("/a/b/c.txt/")));

        // An AED pointing at itself.
        let mut b = UdfBuilder::new(0x0201, 512);
        b.fragment(1);
        b.file_with("/f", pattern(512 * 100, 1), AdKind::Long);
        let (mut img, map) = b.build_with_map();
        let aed = map.first_aed.unwrap() as usize;
        let aed_lbn = le32(&img[aed..], 12);
        // Last descriptor of the AED area is the continuation: point it at this AED.
        let l_ad = le32(&img[aed..], 20) as usize;
        let last = aed + 24 + l_ad - 16;
        let mut raw = le32(&img[last..], 0);
        if raw >> 30 != 3 {
            raw = (3 << 30) | 512;
        }
        img[last..last + 4].copy_from_slice(&raw.to_le_bytes());
        img[last + 4..last + 8].copy_from_slice(&aed_lbn.to_le_bytes());
        retag(&mut img, aed, 512);
        assert!(parse(img).is_err());
    }

    #[test]
    fn huge_lengths_and_truncation_are_refused_without_panics() {
        let (b, _) = sample(0x0250, 2048);
        let (img, map) = b.build_with_map();
        // Directory with an absurd information length.
        let mut bad = img.clone();
        let icb = map.icb["/sources"] as usize;
        bad[icb + 56..icb + 64].copy_from_slice(&u64::MAX.to_le_bytes());
        retag(&mut bad, icb, 2048);
        assert!(parse(bad).is_err());
        // Truncated images.
        for cut in [img.len() / 2, 300 * 2048, 257 * 2048, 40_000, 0] {
            let _ = parse(img[..cut].to_vec());
            let _ = probe(&MemDev::from_vec(img[..cut].to_vec()));
        }
    }

    #[test]
    fn random_corruption_never_panics() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for (rev, bs) in [(0x0250u16, 512u32), (0x0201, 512), (0x0102, 512)] {
            let (mut b, _) = sample(rev, bs);
            b.fragment(2);
            let (img, map) = b.build_with_map();
            // Corrupt only the metadata-bearing area (descriptors, ICBs, directories) so most
            // mutations hit structures, not file payload.
            let hot = map.hot_end as usize;
            for _ in 0..1500 {
                let mut m = img.clone();
                for _ in 0..(1 + rnd() % 8) {
                    let o = (rnd() as usize) % hot.min(m.len());
                    m[o] ^= (rnd() % 255 + 1) as u8;
                    // Sometimes keep tags valid so the parser goes deeper.
                    if rnd() % 2 == 0 {
                        let blk = o / bs as usize * bs as usize;
                        if check_tag(&m[blk..blk + 16.min(m.len() - blk)], None).is_none() && m.len() >= blk + bs as usize {
                            retag(&mut m, blk, bs as usize);
                        }
                    }
                }
                let dev: Arc<dyn BlockDev> = Arc::new(MemDev::from_vec(m));
                if let Ok(p) = read(dev.as_ref(), &Limits { max_dir_bytes: 1 << 20, max_extents: 100_000, ..Limits::default() }) {
                    for n in p.nodes.iter().take(50) {
                        let mut buf = vec![0u8; 4096];
                        let _ = super::super::read_node(dev.as_ref(), n, 0, &mut buf);
                        let _ = super::super::read_node(dev.as_ref(), n, n.size.saturating_sub(10), &mut buf);
                    }
                }
            }
        }
    }

    #[test]
    fn strings_times_and_modes() {
        assert_eq!(cs0(&[8, b'a', b'b']).as_deref(), Some("ab"));
        assert_eq!(cs0(&[16, 0x30, 0x42, 0, b'x']).as_deref(), Some("\u{3042}x"));
        assert_eq!(cs0(&[254, 0xE9]).as_deref(), Some("é"));
        assert_eq!(cs0(&[255, 0, b'z']).as_deref(), Some("z"));
        assert_eq!(cs0(&[7, b'a']), None);
        assert_eq!(cs0(&[]), None);
        assert_eq!(unix_mode((4 | 1) << 10 | 4 << 5 | 4, 0x40), 0o4544);
        // 2024-01-02 03:04:05 at UTC+01:00.
        let mut t = [0u8; 12];
        t[0..2].copy_from_slice(&(0x1000u16 | 60).to_le_bytes());
        t[2..4].copy_from_slice(&2024u16.to_le_bytes());
        t[4..9].copy_from_slice(&[1, 2, 3, 4, 5]);
        assert_eq!(timestamp(&t), 1_704_164_645 - 3600);
        assert_eq!(symlink_target(&[2, 0, 0, 0, 5, 3, 0, 0, 8, b'e', b't', 3, 0, 0, 0, 5, 2, 0, 0, 8, b'x']), "/et/../x");
    }

    /// Replace the partition map of both LVDs (2048-byte blocks, no metadata partition).
    fn set_maps(img: &mut [u8], maps: &[u8], count: u32) {
        for sector in [35usize, 51] {
            let o = sector * 2048;
            assert_eq!(le16(&img[o..], 0), TAG_LVD);
            img[o + 440..o + 2048].fill(0);
            img[o + 440..o + 440 + maps.len()].copy_from_slice(maps);
            img[o + 264..o + 268].copy_from_slice(&(maps.len() as u32).to_le_bytes());
            img[o + 268..o + 272].copy_from_slice(&count.to_le_bytes());
            img[o + 10..o + 12].copy_from_slice(&((440 + maps.len() - 16) as u16).to_le_bytes());
            retag(img, o, 2048);
        }
    }

    fn type2_map(ident: &[u8]) -> Vec<u8> {
        let mut m = vec![0u8; 64];
        m[0] = 2;
        m[1] = 64;
        m[5..5 + ident.len()].copy_from_slice(ident);
        m[36] = 1;
        m
    }

    #[test]
    fn sparable_partitions_follow_the_sparing_table_and_vat_is_refused() {
        let (mut b, files) = sample_with(0x0201, 2048, false);
        b.dirs_inline(false);
        let img0 = b.build();
        let p0 = parse(img0.clone()).unwrap();
        let n = p0.nodes.iter().find(|n| n.path == "/sources/install.wim").unwrap();
        let Data::Extents(ex) = &n.data else { panic!() };
        let lbn = ex[0].pos / 2048 - 257;
        let packet = lbn & !31;
        // Move that 32-block packet past the end of the image and zero the original.
        let mut img = img0.clone();
        let spare_at = (img.len() / 2048) as u64 + 1;
        let table_at = spare_at + 32;
        img.resize(((table_at + 1) * 2048) as usize, 0);
        let src = ((257 + packet) * 2048) as usize;
        let moved = img[src..src + 32 * 2048].to_vec();
        img[(spare_at * 2048) as usize..(spare_at * 2048) as usize + 32 * 2048].copy_from_slice(&moved);
        img[src..src + 32 * 2048].fill(0);
        // Sparing table (UDF 2.2.12) with one entry.
        let t = (table_at * 2048) as usize;
        img[t + 17..t + 35].copy_from_slice(b"*UDF Sparing Table");
        img[t + 48..t + 50].copy_from_slice(&1u16.to_le_bytes());
        img[t + 56..t + 60].copy_from_slice(&(packet as u32).to_le_bytes());
        img[t + 60..t + 64].copy_from_slice(&(spare_at as u32).to_le_bytes());
        img[t + 2..t + 4].copy_from_slice(&3u16.to_le_bytes());
        img[t + 10..t + 12].copy_from_slice(&(64u16 - 16).to_le_bytes());
        img[t + 12..t + 16].copy_from_slice(&(table_at as u32).to_le_bytes());
        retag(&mut img, t, 2048);
        // Without the sparing map the moved packet (which may hold descriptors too) is lost…
        if let Ok(p) = parse(img.clone()) {
            let n = p.nodes.iter().find(|n| n.path == "/sources/install.wim").unwrap();
            assert_ne!(content(&img, n), files.iter().find(|f| f.0 == "/sources/install.wim").unwrap().1);
        }
        // …with it, everything is back.
        let mut m = type2_map(b"*UDF Sparable Partition");
        m[40..42].copy_from_slice(&32u16.to_le_bytes());
        m[42] = 1;
        m[44..48].copy_from_slice(&2048u32.to_le_bytes());
        m[48..52].copy_from_slice(&(table_at as u32).to_le_bytes());
        set_maps(&mut img, &m, 1);
        let p = parse(img.clone()).unwrap();
        for (path, data) in &files {
            let n = p.nodes.iter().find(|n| &n.path == path).unwrap();
            assert_eq!(&content(&img, n), data, "{path}");
        }
        // Virtual partitions (VAT) are refused with a clear error.
        let mut img = img0;
        set_maps(&mut img, &[&[1u8, 6, 1, 0, 0, 0][..], &type2_map(b"*UDF Virtual Partition")].concat(), 2);
        match parse(img) {
            Err(ForgeError::Unsupported(m)) => assert!(m.contains("virtual"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    /// The builder's images agree with the Linux kernel's udf driver (Docker toolbox,
    /// privileged loop mount): same file list, same SHA-256 of every file, same link targets.
    #[test]
    #[ignore]
    fn it_builder_images_match_the_linux_kernel() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("ld-udf-kernel-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cases = vec![];
        for (rev, bs) in [(0x0102u16, 2048u32), (0x0201, 512), (0x0201, 2048), (0x0250, 2048), (0x0260, 512), (0x0250, 4096)] {
            let (mut b, _) = sample_with(rev, bs, false);
            b.fragment(3);
            if (rev, bs) == (0x0250, 2048) || (rev, bs) == (0x0201, 512) {
                // > 4 GiB, sparse, with recorded data beyond 4 GiB (the kernel hashes all of it).
                b.sparse_file("/huge.bin", (4u64 << 30) + (1 << 20), vec![(0, pattern(4096, 1)), (4u64 << 30, pattern(8192, 2))]);
            }
            b.file_with("/frag-long.bin", pattern(bs as usize * 50 + 3, 4), AdKind::Long);
            let name = format!("u{rev:04x}-{bs}.img");
            std::fs::write(dir.join(&name), b.build()).unwrap();
            cases.push(name);
        }
        for name in cases {
            let script = format!(
                "set -e; mkdir -p /m; mount -t udf -o ro,loop,utf8 /w/{name} /m; cd /m; \
                 find . -mindepth 1 | sort | while read f; do \
                   if [ -L \"$f\" ]; then echo \"L $f -> $(readlink \"$f\")\"; \
                   elif [ -d \"$f\" ]; then echo \"D $f\"; \
                   else echo \"F $f $(sha256sum < \"$f\" | cut -d' ' -f1)\"; fi; done; cd /; umount /m"
            );
            let out = Command::new("docker")
                .args(["run", "--rm", "--privileged", "-v", &format!("{}:/w", dir.display()), "lumendisk-toolbox", "sh", "-c", &script])
                .output()
                .expect("docker");
            let kernel = String::from_utf8_lossy(&out.stdout).to_string();
            assert!(out.status.success(), "{name}: {}{}", kernel, String::from_utf8_lossy(&out.stderr));
            let img = std::fs::read(dir.join(&name)).unwrap();
            let p = parse(img.clone()).unwrap();
            let mut ours: Vec<String> = p
                .nodes
                .iter()
                .map(|n| {
                    let rel = format!(".{}", n.path);
                    if let Some(t) = &n.symlink {
                        format!("L {rel} -> {t}")
                    } else if n.is_dir {
                        format!("D {rel}")
                    } else {
                        use sha2::Digest;
                        let dev = MemDev::from_vec(img.clone());
                        let mut h = sha2::Sha256::new();
                        let mut off = 0u64;
                        let mut buf = vec![0u8; 1 << 20];
                        while off < n.size {
                            let k = super::super::read_node(&dev, n, off, &mut buf).unwrap();
                            h.update(&buf[..k]);
                            off += k as u64;
                        }
                        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
                        format!("F {rel} {hex}")
                    }
                })
                .collect();
            // `sort` in the container uses byte order, like Rust's string order.
            ours.sort_by(|a, b| a[2..].split(' ').next().cmp(&b[2..].split(' ').next()));
            let mut theirs: Vec<String> = kernel.lines().map(str::to_string).collect();
            theirs.sort_by(|a, b| a[2..].split(' ').next().cmp(&b[2..].split(' ').next()));
            assert_eq!(ours, theirs, "{name}");
            eprintln!("{name}: {} entries agree with the kernel", ours.len());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
