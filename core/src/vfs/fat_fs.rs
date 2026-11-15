//! FAT12 / FAT16 / FAT32 driver with VFAT long file names — pure user space.
//!
//! * The FAT type is derived from the cluster count (Microsoft spec: < 4085 → FAT12,
//!   < 65525 → FAT16, otherwise FAT32). A BPB with a FAT32 layout (`BPB_FATSz16 == 0`)
//!   is treated as FAT32 even when it has fewer clusters, like Linux and dosfstools do.
//! * The active FAT is cached in memory; modified sectors are written through to every FAT
//!   copy at the end of each mutating operation.
//! * Inode numbers come from an in-memory table keyed by (parent inode, directory-entry
//!   offset); they are stable for the mount lifetime and follow renames.
//! * Dirty-bit doctor: FAT[1] clean-shutdown / hard-error bits (FAT16/32) and the BPB
//!   "reserved / NT" byte bit 0. The driver flags the volume dirty while it has unsynced
//!   mutations (as Windows/Linux do) and clears the flags on `sync()` / drop.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use super::*;
use crate::block::BlockDev;

const ROOT: Ino = 1;
const A_RO: u8 = 0x01;
const A_VOL: u8 = 0x08;
const A_DIR: u8 = 0x10;
const A_ARCH: u8 = 0x20;
const A_LFN: u8 = 0x0F;
const NT_LOWER_BASE: u8 = 0x08;
const NT_LOWER_EXT: u8 = 0x10;
const MAX_FILE: u64 = 0xFFFF_FFFF;
const MAX_DIR_BYTES: u64 = 65536 * 32;
const LFN_POS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
const SHORT_SPECIAL: &[u8] = b"$%'-_@~`!(){}^#&";
const CHAIN_CACHE: usize = 64;

/// Upper half (0x80..=0xFF) of code page 437, used to display short names.
const CP437_HI: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{A0}";

// ───────────────────────────── small helpers ─────────────────────────────

pub(crate) fn rd16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub(crate) fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
pub(crate) fn rd64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}
pub(crate) fn wr16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes())
}
pub(crate) fn wr32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes())
}
pub(crate) fn wr64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes())
}
fn corrupt(s: impl Into<String>) -> FsError {
    FsError::Corrupt(s.into())
}

/// Zero-fill file payload (ordered-mode data write, kept out of the journal).
pub(crate) fn zero_dev_data(dev: &dyn BlockDev, off: u64, len: u64) -> FsResult<()> {
    let chunk = vec![0u8; len.min(1 << 20) as usize];
    let mut done = 0u64;
    while done < len {
        let n = (len - done).min(chunk.len() as u64) as usize;
        dev.write_data_at(off + done, &chunk[..n])?;
        done += n as u64;
    }
    Ok(())
}

/// Write `len` zero bytes at `off` in chunks.
pub(crate) fn zero_dev(dev: &dyn BlockDev, off: u64, len: u64) -> FsResult<()> {
    let chunk = vec![0u8; len.min(1 << 20) as usize];
    let mut done = 0u64;
    while done < len {
        let n = (len - done).min(chunk.len() as u64) as usize;
        dev.write_at(off + done, &chunk[..n])?;
        done += n as u64;
    }
    Ok(())
}

/// Simple case folding used for case-insensitive comparison of long names.
fn fold(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut u = c.to_uppercase();
            match (u.next(), u.next()) {
                (Some(x), None) => x,
                _ => c,
            }
        })
        .collect()
}

// ───────────────────────────── time conversion ─────────────────────────────

pub(crate) struct Tm {
    pub year: i64,
    pub mon: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
    /// Offset from UTC in seconds (local time = UTC + gmtoff).
    pub gmtoff: i64,
}

pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub(crate) fn civil_to_unix(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    days_from_civil(y, mo, d) * 86400 + h as i64 * 3600 + mi as i64 * 60 + s as i64
}

pub(crate) fn utc_tm(secs: i64) -> Tm {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    Tm {
        year: if m <= 2 { y + 1 } else { y },
        mon: m,
        day: d,
        hour: (rem / 3600) as u32,
        min: (rem / 60 % 60) as u32,
        sec: (rem % 60) as u32,
        gmtoff: 0,
    }
}

pub(crate) fn local_tm(secs: i64) -> Tm {
    if super::deterministic() {
        return utc_tm(secs);
    }
    let t = secs as libc::time_t;
    // SAFETY: zeroed tm is a valid out-param; localtime_r is thread safe.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::localtime_r(&t, &mut tm) };
    if r.is_null() {
        return utc_tm(secs);
    }
    Tm {
        year: tm.tm_year as i64 + 1900,
        mon: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        min: tm.tm_min as u32,
        sec: tm.tm_sec.clamp(0, 59) as u32,
        gmtoff: tm.tm_gmtoff as i64,
    }
}

pub(crate) fn local_to_unix(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    if super::deterministic() {
        return civil_to_unix(y, mo, d, h, mi, s);
    }
    // SAFETY: zeroed tm is valid; mktime only reads/normalises it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = (y - 1900) as i32;
    tm.tm_mon = mo as i32 - 1;
    tm.tm_mday = d as i32;
    tm.tm_hour = h as i32;
    tm.tm_min = mi as i32;
    tm.tm_sec = s as i32;
    tm.tm_isdst = -1;
    let r = unsafe { libc::mktime(&mut tm) };
    if r == -1 {
        civil_to_unix(y, mo, d, h, mi, s)
    } else {
        r as i64
    }
}

/// Unix time → (DOS date, DOS time, 10 ms units 0..199), local time.
fn to_dos(t: Timestamp) -> (u16, u16, u8) {
    let tm = local_tm(t.secs);
    if tm.year < 1980 {
        return ((1 << 5) | 1, 0, 0);
    }
    if tm.year > 2107 {
        return (
            (127 << 9) | (12 << 5) | 31,
            (23 << 11) | (59 << 5) | 29,
            199,
        );
    }
    let date = (((tm.year - 1980) as u16) << 9) | ((tm.mon as u16) << 5) | tm.day as u16;
    let time = ((tm.hour as u16) << 11) | ((tm.min as u16) << 5) | (tm.sec as u16 / 2);
    let tenths = ((tm.sec % 2) * 100 + t.nsecs / 10_000_000).min(199) as u8;
    (date, time, tenths)
}

fn from_dos(date: u16, time: u16) -> Timestamp {
    let y = 1980 + (date >> 9) as i64;
    let mo = ((date >> 5) & 15) as u32;
    let d = (date & 31) as u32;
    if mo == 0 || mo > 12 || d == 0 {
        return Timestamp::default();
    }
    let h = ((time >> 11) as u32).min(23);
    let mi = (((time >> 5) & 63) as u32).min(59);
    let s = (((time & 31) * 2) as u32).min(59);
    Timestamp {
        secs: local_to_unix(y, mo, d, h, mi, s),
        nsecs: 0,
    }
}

// ───────────────────────────── BPB ─────────────────────────────

#[derive(Clone, Debug)]
struct Bpb {
    kind: FsType,
    bps: u32,
    cs: u32,
    reserved: u32,
    nfats: u32,
    fat_sectors: u32,
    clusters: u32,
    root_start: u64,
    root_bytes: u64,
    data_start: u64,
    root_cluster: u32,
    fsinfo: u32,
    backup_boot: u32,
    active_fat: u32,
    mirror: bool,
    serial: Option<u32>,
    label: Option<[u8; 11]>,
    /// Offset of the "reserved / NT" byte whose bit 0 is the dirty flag.
    flag_off: Option<usize>,
}

impl Bpb {
    fn fat32(&self) -> bool {
        self.kind == FsType::Fat32
    }
    fn fat_start(&self, i: u32) -> u64 {
        (self.reserved as u64 + i as u64 * self.fat_sectors as u64) * self.bps as u64
    }
    fn fat_bytes_needed(&self) -> u64 {
        let n = self.clusters as u64 + 2;
        match self.kind {
            FsType::Fat12 => (n * 3 + 1) / 2,
            FsType::Fat16 => n * 2,
            _ => n * 4,
        }
    }
}

fn parse_bpb(b: &[u8]) -> Result<Bpb, String> {
    if b.len() < 512 {
        return Err("short boot sector".into());
    }
    let oem = &b[3..11];
    if oem == b"EXFAT   " || oem == b"NTFS    " {
        return Err("exFAT/NTFS boot sector".into());
    }
    if b[510] != 0x55 || b[511] != 0xAA {
        return Err("missing 0x55AA boot signature".into());
    }
    let bps = rd16(b, 11) as u32;
    if !matches!(bps, 512 | 1024 | 2048 | 4096) {
        return Err(format!("invalid bytes per sector {bps}"));
    }
    let spc = b[13] as u32;
    if spc == 0 || !spc.is_power_of_two() {
        return Err(format!("invalid sectors per cluster {spc}"));
    }
    let cs = bps * spc;
    if cs > 256 * 1024 {
        return Err(format!("cluster size {cs} too large"));
    }
    let reserved = rd16(b, 14) as u32;
    if reserved == 0 {
        return Err("zero reserved sectors".into());
    }
    let nfats = b[16] as u32;
    if nfats == 0 || nfats > 4 {
        return Err(format!("invalid number of FATs {nfats}"));
    }
    let root_entries = rd16(b, 17) as u32;
    let tot16 = rd16(b, 19) as u64;
    let fat16sz = rd16(b, 22) as u32;
    let tot32 = rd32(b, 32) as u64;
    let total = if tot16 != 0 { tot16 } else { tot32 };
    let fat_sectors = if fat16sz != 0 { fat16sz } else { rd32(b, 36) };
    if total == 0 || fat_sectors == 0 {
        return Err("zero total sectors or FAT size".into());
    }
    let root_dir_sectors = (root_entries * 32).div_ceil(bps);
    let first_data = reserved as u64 + nfats as u64 * fat_sectors as u64 + root_dir_sectors as u64;
    if first_data >= total {
        return Err("data area starts beyond end of volume".into());
    }
    let clusters64 = (total - first_data) / spc as u64;
    let kind = if fat16sz == 0 {
        if root_entries != 0 {
            return Err("FAT32 layout with a fixed root directory".into());
        }
        FsType::Fat32
    } else if clusters64 < 4085 {
        FsType::Fat12
    } else if clusters64 < 65525 {
        FsType::Fat16
    } else {
        return Err("too many clusters for a FAT12/16 layout".into());
    };
    if kind != FsType::Fat32 && root_entries == 0 {
        return Err("FAT12/16 without root directory entries".into());
    }
    if clusters64 == 0 || clusters64 > 0x0FFF_FFF5 {
        return Err("invalid cluster count".into());
    }
    let clusters = clusters64 as u32;
    let mut bpb = Bpb {
        kind,
        bps,
        cs,
        reserved,
        nfats,
        fat_sectors,
        clusters,
        root_start: (reserved as u64 + nfats as u64 * fat_sectors as u64) * bps as u64,
        root_bytes: root_entries as u64 * 32,
        data_start: first_data * bps as u64,
        root_cluster: 0,
        fsinfo: 0,
        backup_boot: 0,
        active_fat: 0,
        mirror: true,
        serial: None,
        label: None,
        flag_off: None,
    };
    if (fat_sectors as u64 * bps as u64) < bpb.fat_bytes_needed() {
        return Err("FAT too small for cluster count".into());
    }
    let sig_off = if kind == FsType::Fat32 {
        if rd16(b, 42) != 0 {
            return Err("unsupported FAT32 version".into());
        }
        bpb.root_cluster = rd32(b, 44) & 0x0FFF_FFFF;
        if bpb.root_cluster < 2 || bpb.root_cluster >= clusters + 2 {
            return Err("invalid FAT32 root cluster".into());
        }
        let fsinfo = rd16(b, 48) as u32;
        bpb.fsinfo = if fsinfo > 0 && fsinfo < reserved {
            fsinfo
        } else {
            0
        };
        let bk = rd16(b, 50) as u32;
        bpb.backup_boot = if bk > 0 && bk < reserved { bk } else { 0 };
        let ext = rd16(b, 40);
        bpb.mirror = ext & 0x80 == 0;
        bpb.active_fat = if bpb.mirror { 0 } else { (ext & 0x0F) as u32 };
        if bpb.active_fat >= nfats {
            return Err("active FAT index out of range".into());
        }
        64
    } else {
        36
    };
    let ext_sig = b[sig_off + 2];
    if ext_sig == 0x29 || ext_sig == 0x28 {
        bpb.serial = Some(rd32(b, sig_off + 3));
        bpb.flag_off = Some(sig_off + 1);
        if ext_sig == 0x29 {
            let mut l = [0u8; 11];
            l.copy_from_slice(&b[sig_off + 7..sig_off + 18]);
            bpb.label = Some(l);
        }
    }
    Ok(bpb)
}

pub fn probe(dev: &dyn BlockDev) -> Option<FsType> {
    if dev.size() < 512 {
        return None;
    }
    let b = dev.read_vec(0, 512).ok()?;
    parse_bpb(&b).ok().map(|b| b.kind)
}

// ───────────────────────────── names ─────────────────────────────

fn lfn_checksum(s: &[u8]) -> u8 {
    let mut sum: u8 = 0;
    for &c in s.iter().take(11) {
        sum = (if sum & 1 != 0 { 0x80u8 } else { 0 })
            .wrapping_add(sum >> 1)
            .wrapping_add(c);
    }
    sum
}

fn oem_char(b: u8) -> char {
    if b < 0x80 {
        b as char
    } else {
        CP437_HI.chars().nth((b - 0x80) as usize).unwrap_or('_')
    }
}

fn short_display(e: &[u8]) -> String {
    let mut base: Vec<u8> = e[0..8].to_vec();
    if base[0] == 0x05 {
        base[0] = 0xE5;
    }
    while base.last() == Some(&b' ') {
        base.pop();
    }
    let mut ext: Vec<u8> = e[8..11].to_vec();
    while ext.last() == Some(&b' ') {
        ext.pop();
    }
    let conv = |v: &[u8], lower: bool| -> String {
        v.iter()
            .map(|&b| {
                let c = oem_char(b);
                if lower {
                    c.to_ascii_lowercase()
                } else {
                    c
                }
            })
            .collect()
    };
    let mut s = conv(&base, e[12] & NT_LOWER_BASE != 0);
    if !ext.is_empty() {
        s.push('.');
        s.push_str(&conv(&ext, e[12] & NT_LOWER_EXT != 0));
    }
    s
}

fn label_text(l: &[u8]) -> String {
    let s: String = l.iter().map(|&b| oem_char(b)).collect();
    let s = s.trim_end().to_string();
    if s == "NO NAME" {
        String::new()
    } else {
        s
    }
}

fn short_char_ok(c: u8) -> bool {
    c.is_ascii_alphanumeric() || SHORT_SPECIAL.contains(&c)
}

/// Validate a long name; returns its UTF-16 form.
fn valid_long_name(name: &str) -> FsResult<Vec<u16>> {
    check_name(name, 1024)?;
    if name
        .chars()
        .any(|c| (c as u32) < 0x20 || "\"*/:<>?\\|".contains(c))
    {
        return Err(FsError::InvalidName);
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(FsError::InvalidName);
    }
    let u: Vec<u16> = name.encode_utf16().collect();
    if u.len() > 255 {
        return Err(FsError::NameTooLong);
    }
    Ok(u)
}

/// If `name` is a valid 8.3 name whose base and extension are each single-case, return the
/// upper-case short name and the NT lower-case flags (so no LFN entries are needed).
fn fit_83(name: &str) -> Option<([u8; 11], u8)> {
    if !name.is_ascii() {
        return None;
    }
    let (base, ext) = match name.find('.') {
        Some(p) => (&name[..p], &name[p + 1..]),
        None => (name, ""),
    };
    if ext.contains('.') || base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }
    if name.contains('.') && ext.is_empty() {
        return None;
    }
    if !base.bytes().chain(ext.bytes()).all(short_char_ok) {
        return None;
    }
    let case = |s: &str| -> Option<bool> {
        let lo = s.bytes().any(|c| c.is_ascii_lowercase());
        let up = s.bytes().any(|c| c.is_ascii_uppercase());
        if lo && up {
            None
        } else {
            Some(lo)
        }
    };
    let lb = case(base)?;
    let le = case(ext)?;
    let mut s = [b' '; 11];
    for (i, c) in base.bytes().enumerate() {
        s[i] = c.to_ascii_uppercase();
    }
    for (i, c) in ext.bytes().enumerate() {
        s[8 + i] = c.to_ascii_uppercase();
    }
    let flags = if lb { NT_LOWER_BASE } else { 0 } | if le { NT_LOWER_EXT } else { 0 };
    Some((s, flags))
}

fn make_short(base: &[u8], ext: &[u8]) -> [u8; 11] {
    let mut s = [b' '; 11];
    for (i, &c) in base.iter().take(8).enumerate() {
        s[i] = c;
    }
    for (i, &c) in ext.iter().take(3).enumerate() {
        s[8 + i] = c;
    }
    s
}

/// Generate a unique short name (with a `~N` numeric tail when the conversion is lossy).
fn gen_short(name: &str, taken: &dyn Fn(&[u8; 11]) -> bool) -> [u8; 11] {
    let trimmed = name.trim_start_matches('.');
    let mut lossy = trimmed.len() != name.len();
    let (b, e) = match trimmed.rfind('.') {
        Some(p) => (&trimmed[..p], &trimmed[p + 1..]),
        None => (trimmed, ""),
    };
    let mut conv = |s: &str| -> Vec<u8> {
        let mut out = vec![];
        for c in s.chars() {
            if c == ' ' || c == '.' {
                lossy = true;
                continue;
            }
            if c.is_ascii() {
                let u = c.to_ascii_uppercase() as u8;
                if short_char_ok(u) {
                    out.push(u);
                    continue;
                }
            }
            out.push(b'_');
            lossy = true;
        }
        out
    };
    let mut bb = conv(b);
    let mut eb = conv(e);
    if bb.is_empty() {
        bb = vec![b'_'];
        lossy = true;
    }
    if bb.len() > 8 || eb.len() > 3 {
        lossy = true;
    }
    eb.truncate(3);
    if !lossy {
        let s = make_short(&bb, &eb);
        if !taken(&s) {
            return s;
        }
    }
    let with_tail = |basis: &[u8], n: u32| {
        let tail = format!("~{n}");
        let keep = basis.len().min(8 - tail.len());
        let mut base = basis[..keep].to_vec();
        base.extend(tail.bytes());
        make_short(&base, &eb)
    };
    for n in 1..=4 {
        let s = with_tail(&bb, n);
        if !taken(&s) {
            return s;
        }
    }
    // Like Windows: after a few collisions switch to a hashed basis so generation stays
    // O(1) even with thousands of similar names.
    let mut h: u32 = 0x811C_9DC5;
    for u in name.encode_utf16() {
        h = (h ^ u as u32).wrapping_mul(0x0100_0193);
    }
    let mut hb: Vec<u8> = bb.iter().take(2).copied().collect();
    hb.extend(format!("{:04X}", (h ^ (h >> 16)) & 0xFFFF).bytes());
    for n in 1..=9 {
        let s = with_tail(&hb, n);
        if !taken(&s) {
            return s;
        }
    }
    let mut last = with_tail(&bb, 5);
    for n in 5..=999_999u32 {
        last = with_tail(&bb, n);
        if !taken(&last) {
            return last;
        }
    }
    last
}

/// Upper-cased 8.3 form of `name` (for matching against short names), if it has one.
fn short_key(name: &str) -> Option<[u8; 11]> {
    if !name.is_ascii() || name.starts_with('.') {
        return None;
    }
    let (base, ext) = match name.find('.') {
        Some(p) => (&name[..p], &name[p + 1..]),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || ext.contains('.') {
        return None;
    }
    Some(make_short(
        base.to_ascii_uppercase().as_bytes(),
        ext.to_ascii_uppercase().as_bytes(),
    ))
}

/// Cached directory: raw bytes plus an index of live entries by folded long name and by
/// short name. Kept up to date by `insert_set` / `delete_slots` / `store`.
struct DCache {
    raw: Vec<u8>,
    ents: std::collections::BTreeMap<u64, DEnt>,
    by_name: HashMap<String, Vec<u64>>,
    by_short: HashMap<[u8; 11], Vec<u64>>,
}

impl DCache {
    fn new(raw: Vec<u8>) -> Self {
        let mut c = DCache {
            raw,
            ents: Default::default(),
            by_name: HashMap::new(),
            by_short: HashMap::new(),
        };
        for e in parse_dir(&c.raw) {
            c.add(e);
        }
        c
    }
    fn short_of(e: &DEnt) -> [u8; 11] {
        let mut s = [0u8; 11];
        s.copy_from_slice(&e.ent[0..11]);
        s
    }
    fn add(&mut self, e: DEnt) {
        self.by_name.entry(fold(&e.name)).or_default().push(e.off);
        self.by_short
            .entry(Self::short_of(&e))
            .or_default()
            .push(e.off);
        self.ents.insert(e.off, e);
    }
    fn remove(&mut self, off: u64) {
        let Some(e) = self.ents.remove(&off) else {
            return;
        };
        let k = fold(&e.name);
        if let Some(v) = self.by_name.get_mut(&k) {
            v.retain(|&o| o != off);
            if v.is_empty() {
                self.by_name.remove(&k);
            }
        }
        let s = Self::short_of(&e);
        if let Some(v) = self.by_short.get_mut(&s) {
            v.retain(|&o| o != off);
            if v.is_empty() {
                self.by_short.remove(&s);
            }
        }
    }
}

/// LFN entries (if requested) followed by the short entry.
fn build_set(name: &[u16], short_ent: &[u8; 32], lfn: bool) -> Vec<u8> {
    if !lfn {
        return short_ent.to_vec();
    }
    let chk = lfn_checksum(&short_ent[0..11]);
    let n = name.len().div_ceil(13);
    let mut out = Vec::with_capacity((n + 1) * 32);
    for seq in (1..=n).rev() {
        let mut e = [0u8; 32];
        e[0] = seq as u8 | if seq == n { 0x40 } else { 0 };
        e[11] = A_LFN;
        e[13] = chk;
        for (k, &p) in LFN_POS.iter().enumerate() {
            let idx = (seq - 1) * 13 + k;
            let u = match idx.cmp(&name.len()) {
                std::cmp::Ordering::Less => name[idx],
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 0xFFFF,
            };
            wr16(&mut e, p, u);
        }
        out.extend_from_slice(&e);
    }
    out.extend_from_slice(short_ent);
    out
}

#[derive(Clone, Debug)]
struct DEnt {
    /// Long name if a valid LFN precedes the short entry, else the displayed short name.
    name: String,
    /// Offset of the short entry.
    off: u64,
    /// Offset of the first slot (first LFN entry, or `off`).
    start: u64,
    ent: [u8; 32],
}

struct Lfn {
    next: u8,
    chk: u8,
    start: u64,
    units: Vec<u16>,
}

fn is_dot(e: &[u8]) -> bool {
    &e[0..11] == b".          " || &e[0..11] == b"..         "
}

fn parse_dir(buf: &[u8]) -> Vec<DEnt> {
    let mut out = vec![];
    let mut lfn: Option<Lfn> = None;
    for (i, e) in buf.chunks_exact(32).enumerate() {
        let off = i as u64 * 32;
        match e[0] {
            0x00 => break,
            0xE5 => {
                lfn = None;
                continue;
            }
            _ => {}
        }
        let attr = e[11];
        if attr & 0x3F == A_LFN {
            let ord = e[0];
            let seq = ord & 0x1F;
            if ord & 0x40 != 0 {
                if seq == 0 || seq > 20 {
                    lfn = None;
                    continue;
                }
                lfn = Some(Lfn {
                    next: seq,
                    chk: e[13],
                    start: off,
                    units: vec![0; seq as usize * 13],
                });
            }
            match lfn.as_mut() {
                Some(l) if l.next == seq && seq >= 1 && l.chk == e[13] => {
                    let base = (seq as usize - 1) * 13;
                    for (k, &p) in LFN_POS.iter().enumerate() {
                        l.units[base + k] = rd16(e, p);
                    }
                    l.next -= 1;
                }
                _ => lfn = None,
            }
            continue;
        }
        let pending = lfn.take();
        if attr & A_VOL != 0 || is_dot(e) {
            continue;
        }
        let mut ent = [0u8; 32];
        ent.copy_from_slice(e);
        let mut name = None;
        let mut start = off;
        if let Some(l) = pending {
            if l.next == 0 && l.chk == lfn_checksum(&ent[0..11]) {
                let end = l
                    .units
                    .iter()
                    .position(|&u| u == 0 || u == 0xFFFF)
                    .unwrap_or(l.units.len());
                let s = String::from_utf16_lossy(&l.units[..end]);
                if !s.is_empty() {
                    name = Some(s);
                    start = l.start;
                }
            }
        }
        let name = name.unwrap_or_else(|| short_display(&ent));
        out.push(DEnt {
            name,
            off,
            start,
            ent,
        });
    }
    out
}

fn ent_cluster(e: &[u8], fat32: bool) -> u32 {
    let lo = rd16(e, 26) as u32;
    let hi = if fat32 { rd16(e, 20) as u32 } else { 0 };
    (hi << 16) | lo
}
fn set_ent_cluster(e: &mut [u8], c: u32, fat32: bool) {
    wr16(e, 26, c as u16);
    wr16(e, 20, if fat32 { (c >> 16) as u16 } else { 0 });
}
fn set_mtime(e: &mut [u8], t: Timestamp) {
    let (d, tm, _) = to_dos(t);
    wr16(e, 22, tm);
    wr16(e, 24, d);
}
fn set_all_times(e: &mut [u8], t: Timestamp) {
    let (d, tm, ten) = to_dos(t);
    e[13] = ten;
    wr16(e, 14, tm);
    wr16(e, 16, d);
    wr16(e, 18, d);
    wr16(e, 22, tm);
    wr16(e, 24, d);
}

fn new_ent(short: [u8; 11], nt: u8, attr: u8, t: Timestamp) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0..11].copy_from_slice(&short);
    e[11] = attr;
    e[12] = nt;
    set_all_times(&mut e, t);
    e
}

// ───────────────────────────── driver ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Dir {
    /// FAT12/16 fixed root directory region.
    Fixed,
    Chain(u32),
}

struct Node {
    parent: Ino,
    off: u64,
    ent: [u8; 32],
}

pub struct FatFs {
    dev: Arc<dyn BlockDev>,
    ro: bool,
    defaults: MountDefaults,
    b: Bpb,
    fat: Vec<u8>,
    fat_loaded: usize,
    fat_dirty: BTreeSet<u32>,
    free: u32,
    next_free: u32,
    nodes: HashMap<Ino, Node>,
    by_loc: HashMap<(Ino, u64), Ino>,
    next_ino: Ino,
    chains: HashMap<u32, Vec<u32>>,
    /// Raw directory contents, kept coherent by `write_dir` / `grow_dir`.
    dcache: HashMap<Dir, DCache>,
    /// Dirty state found on disk at open (or None after a repair).
    dirty_reason: Option<String>,
    /// We flagged the volume dirty for our own pending mutations.
    marked: bool,
    label: String,
    root_time: Timestamp,
}

impl FatFs {
    pub fn open(
        dev: Arc<dyn BlockDev>,
        read_only: bool,
        defaults: MountDefaults,
    ) -> FsResult<FatFs> {
        let ro = read_only || dev.read_only();
        if dev.size() < 512 {
            return Err(corrupt("device too small"));
        }
        let s0 = dev.read_vec(0, 512)?;
        let b = parse_bpb(&s0).map_err(FsError::Corrupt)?;
        let vol_end = b.data_start + b.clusters as u64 * b.cs as u64;
        if vol_end > dev.size() && !ro {
            return Err(corrupt("volume extends beyond the end of the device"));
        }
        let need = b.fat_bytes_needed();
        let secs = need.div_ceil(b.bps as u64);
        let fat_loaded = (secs * b.bps as u64) as usize;
        let mut fat = dev.read_vec(b.fat_start(b.active_fat), fat_loaded)?;
        fat.push(0);
        let mut fs = FatFs {
            dev,
            ro,
            defaults,
            b,
            fat,
            fat_loaded,
            fat_dirty: BTreeSet::new(),
            free: 0,
            next_free: 2,
            nodes: HashMap::new(),
            by_loc: HashMap::new(),
            next_ino: 2,
            chains: HashMap::new(),
            dcache: HashMap::new(),
            dirty_reason: None,
            marked: false,
            label: String::new(),
            root_time: Timestamp::default(),
        };
        fs.free = fs.count_free();
        if fs.b.fat32() && fs.b.fsinfo != 0 {
            if let Some((_, nf)) = fs.read_fsinfo()? {
                if nf >= 2 && nf < fs.b.clusters + 2 {
                    fs.next_free = nf;
                }
            }
        }
        // Dirty-state inspection.
        let mut reasons = vec![];
        if let Some(o) = fs.b.flag_off {
            if s0[o] & 0x01 != 0 {
                reasons.push(
                    "boot-sector dirty flag is set (volume was not unmounted cleanly)".to_string(),
                );
            }
        }
        if let Some((clean, herr)) = fs.fat1_masks() {
            let v = fs.fat_get(1);
            if v & clean == 0 {
                reasons.push(
                    "FAT[1] clean-shutdown bit is clear (volume was not unmounted cleanly)"
                        .to_string(),
                );
            }
            if v & herr == 0 {
                reasons.push("FAT[1] hard-error bit indicates disk I/O errors".to_string());
            }
        }
        if !reasons.is_empty() {
            fs.dirty_reason = Some(reasons.join("; "));
        }
        // Volume label: root-directory label entry wins over the BPB copy.
        let root = fs.dir_of(ROOT)?;
        let buf = fs.read_dir(root)?;
        let mut label = None;
        for e in buf.chunks_exact(32) {
            if e[0] == 0 {
                break;
            }
            if e[0] != 0xE5 && e[11] & 0x3F != A_LFN && e[11] & A_VOL != 0 {
                label = Some(label_text(&e[0..11]));
                fs.root_time = from_dos(rd16(e, 24), rd16(e, 22));
                break;
            }
        }
        fs.label = label
            .or_else(|| fs.b.label.map(|l| label_text(&l)))
            .unwrap_or_default();
        Ok(fs)
    }

    // ── FAT table ──

    fn fat1_masks(&self) -> Option<(u32, u32)> {
        match self.b.kind {
            FsType::Fat16 => Some((0x8000, 0x4000)),
            FsType::Fat32 => Some((0x0800_0000, 0x0400_0000)),
            _ => None,
        }
    }
    fn eoc(&self) -> u32 {
        match self.b.kind {
            FsType::Fat12 => 0xFFF,
            FsType::Fat16 => 0xFFFF,
            _ => 0x0FFF_FFFF,
        }
    }
    fn is_eoc(&self, v: u32) -> bool {
        match self.b.kind {
            FsType::Fat12 => v >= 0xFF8,
            FsType::Fat16 => v >= 0xFFF8,
            _ => v >= 0x0FFF_FFF8,
        }
    }
    fn bad(&self) -> u32 {
        self.eoc() - 8
    }
    fn fat_get(&self, n: u32) -> u32 {
        if n as u64 >= self.b.clusters as u64 + 2 {
            return self.bad();
        }
        let n = n as usize;
        match self.b.kind {
            FsType::Fat12 => {
                let v = rd16(&self.fat, n + n / 2) as u32;
                if n & 1 == 1 {
                    v >> 4
                } else {
                    v & 0xFFF
                }
            }
            FsType::Fat16 => rd16(&self.fat, n * 2) as u32,
            _ => rd32(&self.fat, n * 4) & 0x0FFF_FFFF,
        }
    }
    fn fat_set(&mut self, n: u32, v: u32) {
        if n as u64 >= self.b.clusters as u64 + 2 {
            return;
        }
        if v == 0 {
            let cs = self.b.cs as u64;
            self.dev.note_freed(self.cpos(n), cs);
        }
        let old = self.fat_get(n);
        let bps = self.b.bps as usize;
        let i = n as usize;
        match self.b.kind {
            FsType::Fat12 => {
                let o = i + i / 2;
                let w = rd16(&self.fat, o);
                let w = if i & 1 == 1 {
                    (w & 0x000F) | ((v as u16) << 4)
                } else {
                    (w & 0xF000) | (v as u16 & 0x0FFF)
                };
                wr16(&mut self.fat, o, w);
                self.fat_dirty.insert((o / bps) as u32);
                self.fat_dirty.insert(((o + 1) / bps) as u32);
            }
            FsType::Fat16 => {
                wr16(&mut self.fat, i * 2, v as u16);
                self.fat_dirty.insert((i * 2 / bps) as u32);
            }
            _ => {
                let w = rd32(&self.fat, i * 4);
                wr32(&mut self.fat, i * 4, (w & 0xF000_0000) | (v & 0x0FFF_FFFF));
                self.fat_dirty.insert((i * 4 / bps) as u32);
            }
        }
        if n >= 2 {
            if old == 0 && v != 0 {
                self.free = self.free.saturating_sub(1);
            } else if old != 0 && v == 0 {
                self.free += 1;
            }
        }
    }
    fn count_free(&self) -> u32 {
        (2..self.b.clusters + 2)
            .filter(|&c| self.fat_get(c) == 0)
            .count() as u32
    }
    fn fat_copies(&self) -> Vec<u32> {
        if self.b.mirror {
            (0..self.b.nfats).collect()
        } else {
            vec![self.b.active_fat]
        }
    }
    fn flush_fat(&mut self) -> FsResult<()> {
        if self.fat_dirty.is_empty() {
            return Ok(());
        }
        let secs: Vec<u32> = std::mem::take(&mut self.fat_dirty).into_iter().collect();
        let bps = self.b.bps as usize;
        let mut runs: Vec<(u32, u32)> = vec![];
        for s in secs.iter().copied() {
            if (s as usize) * bps >= self.fat_loaded {
                continue;
            }
            match runs.last_mut() {
                Some(r) if r.0 + r.1 == s => r.1 += 1,
                _ => runs.push((s, 1)),
            }
        }
        for (s, n) in runs {
            let a = s as usize * bps;
            let e = ((s + n) as usize * bps).min(self.fat_loaded);
            for copy in self.fat_copies() {
                if let Err(err) = self
                    .dev
                    .write_at(self.b.fat_start(copy) + a as u64, &self.fat[a..e])
                {
                    self.fat_dirty.extend(secs.iter().copied());
                    return Err(err.into());
                }
            }
        }
        Ok(())
    }

    fn read_fsinfo(&self) -> FsResult<Option<(u32, u32)>> {
        if self.b.fsinfo == 0 {
            return Ok(None);
        }
        let s = self
            .dev
            .read_vec(self.b.fsinfo as u64 * self.b.bps as u64, 512)?;
        if rd32(&s, 0) != 0x4161_5252 || rd32(&s, 484) != 0x6141_7272 {
            return Ok(None);
        }
        Ok(Some((rd32(&s, 488), rd32(&s, 492))))
    }
    fn write_fsinfo(&self) -> FsResult<()> {
        if !self.b.fat32() || self.b.fsinfo == 0 {
            return Ok(());
        }
        let mut targets = vec![self.b.fsinfo];
        if self.b.backup_boot != 0 && self.b.backup_boot + self.b.fsinfo < self.b.reserved {
            targets.push(self.b.backup_boot + self.b.fsinfo);
        }
        for sec in targets {
            let off = sec as u64 * self.b.bps as u64;
            let mut s = self.dev.read_vec(off, 512)?;
            if rd32(&s, 0) != 0x4161_5252 || rd32(&s, 484) != 0x6141_7272 {
                continue;
            }
            if rd32(&s, 488) == self.free && rd32(&s, 492) == self.next_free {
                continue;
            }
            wr32(&mut s, 488, self.free);
            wr32(&mut s, 492, self.next_free);
            self.dev.write_at(off, &s)?;
        }
        Ok(())
    }

    // ── cluster chains ──

    fn build_chain(&self, first: u32) -> FsResult<Vec<u32>> {
        let mut v = vec![];
        if first == 0 {
            return Ok(v);
        }
        let mut c = first;
        loop {
            if c < 2 || c >= self.b.clusters + 2 {
                return Err(corrupt(format!(
                    "cluster {c} out of range in chain starting at {first}"
                )));
            }
            v.push(c);
            if v.len() > self.b.clusters as usize {
                return Err(corrupt(format!("cluster chain loop at {first}")));
            }
            let n = self.fat_get(c);
            if self.is_eoc(n) {
                break;
            }
            if n == self.bad() {
                return Err(corrupt(format!("bad cluster in chain starting at {first}")));
            }
            c = n;
        }
        Ok(v)
    }
    fn take_chain(&mut self, first: u32) -> FsResult<Vec<u32>> {
        if let Some(v) = self.chains.remove(&first) {
            return Ok(v);
        }
        self.build_chain(first)
    }
    fn put_chain(&mut self, chain: Vec<u32>) {
        if let Some(&first) = chain.first() {
            if self.chains.len() >= CHAIN_CACHE {
                self.chains.clear();
            }
            self.chains.insert(first, chain);
        }
    }
    fn find_free(&self, start: u32) -> Option<u32> {
        let hi = self.b.clusters + 2;
        let s = if start >= 2 && start < hi { start } else { 2 };
        (s..hi).chain(2..s).find(|&c| self.fat_get(c) == 0)
    }
    fn extend_chain(&mut self, chain: &mut Vec<u32>, n: u32) -> FsResult<()> {
        if n == 0 {
            return Ok(());
        }
        if self.free < n {
            return Err(FsError::NoSpace);
        }
        let old_last = chain.last().copied();
        let mut prev = old_last;
        let mut hint = prev.map(|p| p + 1).unwrap_or(self.next_free);
        let mut added = Vec::with_capacity(n as usize);
        let eoc = self.eoc();
        for _ in 0..n {
            let Some(c) = self.find_free(hint) else {
                for &c in &added {
                    self.fat_set(c, 0);
                }
                if let Some(l) = old_last {
                    self.fat_set(l, eoc);
                }
                return Err(FsError::NoSpace);
            };
            self.fat_set(c, eoc);
            if let Some(p) = prev {
                self.fat_set(p, c);
            }
            prev = Some(c);
            added.push(c);
            hint = c + 1;
        }
        self.next_free = if hint >= self.b.clusters + 2 { 2 } else { hint };
        chain.extend(added);
        Ok(())
    }
    fn truncate_chain(&mut self, chain: &mut Vec<u32>, keep: usize) {
        if keep >= chain.len() {
            return;
        }
        for i in keep..chain.len() {
            self.fat_set(chain[i], 0);
        }
        if keep > 0 {
            let eoc = self.eoc();
            self.fat_set(chain[keep - 1], eoc);
        }
        chain.truncate(keep);
    }
    fn cpos(&self, c: u32) -> u64 {
        self.b.data_start + (c as u64 - 2) * self.b.cs as u64
    }
    /// Map a byte range of a chain to device ranges, merging adjacent clusters.
    fn runs(&self, chain: &[u32], off: u64, len: u64) -> FsResult<Vec<(u64, u64)>> {
        let cs = self.b.cs as u64;
        if off + len > chain.len() as u64 * cs {
            return Err(corrupt("cluster chain shorter than file size"));
        }
        let mut out: Vec<(u64, u64)> = vec![];
        let mut pos = off;
        let end = off + len;
        while pos < end {
            let ci = (pos / cs) as usize;
            let within = pos % cs;
            let n = (cs - within).min(end - pos);
            let d = self.cpos(chain[ci]) + within;
            match out.last_mut() {
                Some(l) if l.0 + l.1 == d => l.1 += n,
                _ => out.push((d, n)),
            }
            pos += n;
        }
        Ok(out)
    }
    fn read_chain(&self, chain: &[u32], off: u64, len: u64) -> FsResult<Vec<u8>> {
        let mut out = vec![0u8; len as usize];
        let mut p = 0usize;
        for (d, n) in self.runs(chain, off, len)? {
            self.dev.read_at(d, &mut out[p..p + n as usize])?;
            p += n as usize;
        }
        Ok(out)
    }
    fn write_chain(&self, chain: &[u32], off: u64, data: &[u8]) -> FsResult<()> {
        let mut p = 0usize;
        for (d, n) in self.runs(chain, off, data.len() as u64)? {
            self.dev.write_at(d, &data[p..p + n as usize])?;
            p += n as usize;
        }
        Ok(())
    }
    /// File payload: ordered-mode data write (not journaled).
    fn write_file_chain(&self, chain: &[u32], off: u64, data: &[u8]) -> FsResult<()> {
        let mut p = 0usize;
        for (d, n) in self.runs(chain, off, data.len() as u64)? {
            self.dev.write_data_at(d, &data[p..p + n as usize])?;
            p += n as usize;
        }
        Ok(())
    }
    /// Zero-fill of file payload (only ever used for regular files).
    fn zero_chain(&self, chain: &[u32], off: u64, len: u64) -> FsResult<()> {
        for (d, n) in self.runs(chain, off, len)? {
            zero_dev_data(self.dev.as_ref(), d, n)?;
        }
        Ok(())
    }

    // ── directories ──

    fn dir_of(&self, ino: Ino) -> FsResult<Dir> {
        if ino == ROOT {
            return Ok(if self.b.fat32() {
                Dir::Chain(self.b.root_cluster)
            } else {
                Dir::Fixed
            });
        }
        let n = self.nodes.get(&ino).ok_or(FsError::Stale)?;
        if n.ent[11] & A_DIR == 0 {
            return Err(FsError::NotDir);
        }
        let c = ent_cluster(&n.ent, self.b.fat32());
        if c == 0 {
            return Err(corrupt("directory without a cluster"));
        }
        Ok(Dir::Chain(c))
    }
    /// Cluster number recorded in `..` entries for a directory inode (0 for the root).
    fn dotdot_cluster(&self, ino: Ino) -> FsResult<u32> {
        if ino == ROOT {
            return Ok(0);
        }
        match self.dir_of(ino)? {
            Dir::Chain(c) => Ok(c),
            Dir::Fixed => Ok(0),
        }
    }
    /// Parsed + raw view of a directory, built on first use and maintained incrementally.
    fn dc(&mut self, d: Dir) -> FsResult<&mut DCache> {
        if !self.dcache.contains_key(&d) {
            let raw = self.read_dir_disk(d)?;
            if self.dcache.len() >= 32 {
                self.dcache.clear();
            }
            self.dcache.insert(d, DCache::new(raw));
        }
        Ok(self.dcache.get_mut(&d).expect("just inserted"))
    }
    fn read_dir(&mut self, d: Dir) -> FsResult<Vec<u8>> {
        Ok(self.dc(d)?.raw.clone())
    }
    fn read_dir_disk(&mut self, d: Dir) -> FsResult<Vec<u8>> {
        match d {
            Dir::Fixed => Ok(self
                .dev
                .read_vec(self.b.root_start, self.b.root_bytes as usize)?),
            Dir::Chain(f) => {
                let ch = self.take_chain(f)?;
                let len = (ch.len() as u64 * self.b.cs as u64).min(MAX_DIR_BYTES * 4);
                let r = self.read_chain(&ch, 0, len);
                self.put_chain(ch);
                r
            }
        }
    }
    /// Raw write into a directory; keeps the cached raw bytes in sync (not the parsed index).
    fn write_dir(&mut self, d: Dir, off: u64, data: &[u8]) -> FsResult<()> {
        let r = self.write_dir_disk(d, off, data);
        match (r.is_ok(), self.dcache.get_mut(&d)) {
            (true, Some(c)) if off as usize + data.len() <= c.raw.len() => {
                c.raw[off as usize..off as usize + data.len()].copy_from_slice(data);
            }
            _ => {
                self.dcache.remove(&d);
            }
        }
        r
    }
    fn write_dir_disk(&mut self, d: Dir, off: u64, data: &[u8]) -> FsResult<()> {
        match d {
            Dir::Fixed => {
                if off + data.len() as u64 > self.b.root_bytes {
                    return Err(corrupt("write beyond fixed root directory"));
                }
                self.dev.write_at(self.b.root_start + off, data)?;
                Ok(())
            }
            Dir::Chain(f) => {
                let ch = self.take_chain(f)?;
                let r = self.write_chain(&ch, off, data);
                self.put_chain(ch);
                r
            }
        }
    }
    fn grow_dir(&mut self, d: Dir) -> FsResult<()> {
        let Dir::Chain(f) = d else {
            return Err(FsError::NoSpace);
        };
        let mut ch = self.take_chain(f)?;
        if (ch.len() as u64 + 1) * self.b.cs as u64 > MAX_DIR_BYTES {
            self.put_chain(ch);
            return Err(FsError::NoSpace);
        }
        if let Err(e) = self.extend_chain(&mut ch, 1) {
            self.put_chain(ch);
            return Err(e);
        }
        let last = *ch.last().unwrap();
        let r = zero_dev(self.dev.as_ref(), self.cpos(last), self.b.cs as u64);
        let total = ch.len() * self.b.cs as usize;
        self.put_chain(ch);
        match (r.is_ok(), self.dcache.get_mut(&d)) {
            (true, Some(c)) if c.raw.len() + self.b.cs as usize == total => c.raw.resize(total, 0),
            _ => {
                self.dcache.remove(&d);
            }
        }
        r
    }
    /// Write an entry set into free slots (growing the directory if needed) and index it
    /// under `name`. Returns the offset of the short entry.
    fn insert_set(&mut self, d: Dir, set: &[u8], name: &str) -> FsResult<u64> {
        let need = set.len() / 32;
        let (mut run_start, mut run_len, nslots) = {
            let buf = &self.dc(d)?.raw;
            let nslots = buf.len() / 32;
            let mut run_start = 0usize;
            let mut run_len = 0usize;
            let mut ended = false;
            for i in 0..nslots {
                let b0 = buf[i * 32];
                let free = ended || b0 == 0 || b0 == 0xE5;
                if b0 == 0 {
                    ended = true;
                }
                if free {
                    if run_len == 0 {
                        run_start = i;
                    }
                    run_len += 1;
                    if run_len == need {
                        break;
                    }
                } else {
                    run_len = 0;
                }
            }
            (run_start, run_len, nslots)
        };
        if run_len < need {
            if run_len == 0 {
                run_start = nslots;
            }
            let missing = (need - run_len) as u64 * 32;
            let clusters = missing.div_ceil(self.b.cs as u64);
            for _ in 0..clusters {
                self.grow_dir(d)?;
            }
            run_len = need;
        }
        let _ = run_len;
        self.write_dir(d, run_start as u64 * 32, set)?;
        let off = (run_start + need - 1) as u64 * 32;
        let mut ent = [0u8; 32];
        ent.copy_from_slice(&set[set.len() - 32..]);
        if let Some(c) = self.dcache.get_mut(&d) {
            c.add(DEnt {
                name: name.to_string(),
                off,
                start: run_start as u64 * 32,
                ent,
            });
        }
        Ok(off)
    }
    fn delete_slots(&mut self, d: Dir, start: u64, off: u64) -> FsResult<()> {
        let mut o = start;
        while o <= off {
            self.write_dir(d, o, &[0xE5])?;
            o += 32;
        }
        if let Some(c) = self.dcache.get_mut(&d) {
            c.remove(off);
        }
        Ok(())
    }
    fn find(&mut self, parent: Ino, name: &str) -> FsResult<(Dir, Option<DEnt>)> {
        let d = self.dir_of(parent)?;
        let key = fold(name);
        let sk = short_key(name);
        let c = self.dc(d)?;
        let off = c
            .by_name
            .get(&key)
            .and_then(|v| v.first())
            .or_else(|| sk.and_then(|k| c.by_short.get(&k)).and_then(|v| v.first()))
            .copied();
        Ok((d, off.and_then(|o| c.ents.get(&o).cloned())))
    }
    /// Short name + NT flags + whether LFN entries are needed for `name` in a directory.
    fn short_for(
        &mut self,
        name: &str,
        d: Dir,
        exclude: Option<u64>,
    ) -> FsResult<([u8; 11], u8, bool)> {
        let c = self.dc(d)?;
        let taken = |s: &[u8; 11]| {
            c.by_short
                .get(s)
                .is_some_and(|v| v.iter().any(|&o| Some(o) != exclude))
        };
        if let Some((s, f)) = fit_83(name) {
            if taken(&s) {
                return Err(FsError::Exists);
            }
            return Ok((s, f, false));
        }
        Ok((gen_short(name, &taken), 0, true))
    }

    // ── inode table ──

    fn intern(&mut self, parent: Ino, off: u64, ent: [u8; 32]) -> Ino {
        if let Some(&i) = self.by_loc.get(&(parent, off)) {
            return i;
        }
        let i = self.next_ino;
        self.next_ino += 1;
        self.nodes.insert(i, Node { parent, off, ent });
        self.by_loc.insert((parent, off), i);
        i
    }
    fn forget(&mut self, parent: Ino, off: u64) {
        if let Some(i) = self.by_loc.remove(&(parent, off)) {
            self.nodes.remove(&i);
        }
    }
    fn ent(&self, ino: Ino) -> FsResult<[u8; 32]> {
        Ok(self.nodes.get(&ino).ok_or(FsError::Stale)?.ent)
    }
    fn store(&mut self, ino: Ino, ent: [u8; 32]) -> FsResult<()> {
        let (parent, off) = {
            let n = self.nodes.get_mut(&ino).ok_or(FsError::Stale)?;
            n.ent = ent;
            (n.parent, n.off)
        };
        let d = self.dir_of(parent)?;
        self.write_dir(d, off, &ent)?;
        if let Some(e) = self.dcache.get_mut(&d).and_then(|c| c.ents.get_mut(&off)) {
            e.ent = ent;
        }
        Ok(())
    }

    fn attr_of(&self, ino: Ino) -> FsResult<Attr> {
        let cs = self.b.cs as u64;
        if ino == ROOT {
            let size = match self.dir_of(ROOT)? {
                Dir::Fixed => self.b.root_bytes,
                Dir::Chain(c) => self.build_chain(c)?.len() as u64 * cs,
            };
            return Ok(Attr {
                ino,
                kind: FileKind::Dir,
                size,
                perm: self.defaults.dir_perm,
                nlink: 2,
                uid: self.defaults.uid,
                gid: self.defaults.gid,
                atime: self.root_time,
                mtime: self.root_time,
                ctime: self.root_time,
                used_bytes: size,
            });
        }
        let e = self.ent(ino)?;
        let dir = e[11] & A_DIR != 0;
        let (size, used) = if dir {
            let n = self.build_chain(ent_cluster(&e, self.b.fat32()))?.len() as u64 * cs;
            (n, n)
        } else {
            let s = rd32(&e, 28) as u64;
            (s, s.div_ceil(cs) * cs)
        };
        let mut perm = if dir {
            self.defaults.dir_perm
        } else {
            self.defaults.file_perm
        };
        if e[11] & A_RO != 0 {
            perm &= !0o222;
        }
        let mtime = from_dos(rd16(&e, 24), rd16(&e, 22));
        let adate = rd16(&e, 18);
        let atime = if adate != 0 {
            from_dos(adate, 0)
        } else {
            mtime
        };
        Ok(Attr {
            ino,
            kind: if dir { FileKind::Dir } else { FileKind::File },
            size,
            perm,
            nlink: if dir { 2 } else { 1 },
            uid: self.defaults.uid,
            gid: self.defaults.gid,
            atime,
            mtime,
            ctime: mtime,
            used_bytes: used,
        })
    }

    // ── mutation bracket ──

    fn begin(&mut self) -> FsResult<()> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        if !self.marked && self.dirty_reason.is_none() {
            self.set_dirty_flags(true)?;
            self.marked = true;
        }
        Ok(())
    }
    fn end<T>(&mut self, r: FsResult<T>) -> FsResult<T> {
        let f = self.flush_fat();
        match r {
            Ok(v) => f.map(|_| v),
            Err(e) => Err(e),
        }
    }
    fn set_dirty_flags(&mut self, dirty: bool) -> FsResult<()> {
        if let Some(o) = self.b.flag_off {
            let mut b = [0u8; 1];
            self.dev.read_at(o as u64, &mut b)?;
            let nb = if dirty { b[0] | 1 } else { b[0] & !1 };
            if nb != b[0] {
                self.dev.write_at(o as u64, &[nb])?;
            }
        }
        if let Some((clean, _)) = self.fat1_masks() {
            let v = self.fat_get(1);
            let nv = if dirty { v & !clean } else { v | clean };
            if nv != v {
                self.fat_set(1, nv);
            }
        }
        self.flush_fat()
    }

    fn file_ent(&self, ino: Ino) -> FsResult<[u8; 32]> {
        if ino == ROOT {
            return Err(FsError::IsDir);
        }
        let e = self.ent(ino)?;
        if e[11] & A_DIR != 0 {
            return Err(FsError::IsDir);
        }
        Ok(e)
    }

    fn resize(&mut self, ino: Ino, new: u64) -> FsResult<()> {
        let mut e = self.file_ent(ino)?;
        if new > MAX_FILE {
            return Err(FsError::Invalid(
                "FAT files are limited to 4 GiB - 1 byte".into(),
            ));
        }
        let fat32 = self.b.fat32();
        let size = rd32(&e, 28) as u64;
        if new == size {
            return Ok(());
        }
        let first = ent_cluster(&e, fat32);
        let mut chain = self.take_chain(first)?;
        let need = new.div_ceil(self.b.cs as u64) as usize;
        if need < chain.len() {
            self.truncate_chain(&mut chain, need);
        } else if need > chain.len() {
            let add = (need - chain.len()) as u32;
            self.extend_chain(&mut chain, add)?;
        }
        if new > size {
            self.zero_chain(&chain, size, new - size)?;
        }
        set_ent_cluster(&mut e, chain.first().copied().unwrap_or(0), fat32);
        wr32(&mut e, 28, new as u32);
        e[11] |= A_ARCH;
        set_mtime(&mut e, Timestamp::now());
        self.put_chain(chain);
        self.store(ino, e)
    }

    fn write_inner(&mut self, ino: Ino, off: u64, data: &[u8]) -> FsResult<u32> {
        let mut e = self.file_ent(ino)?;
        if data.is_empty() {
            return Ok(0);
        }
        let end = off
            .checked_add(data.len() as u64)
            .ok_or(FsError::Invalid("offset overflow".into()))?;
        if end > MAX_FILE {
            return Err(FsError::Invalid(
                "FAT files are limited to 4 GiB - 1 byte".into(),
            ));
        }
        let fat32 = self.b.fat32();
        let size = rd32(&e, 28) as u64;
        let first = ent_cluster(&e, fat32);
        let mut chain = self.take_chain(first)?;
        let new_size = size.max(end);
        let need = new_size.div_ceil(self.b.cs as u64) as usize;
        if need > chain.len() {
            let add = (need - chain.len()) as u32;
            self.extend_chain(&mut chain, add)?;
        }
        if off > size {
            self.zero_chain(&chain, size, off - size)?;
        }
        self.write_file_chain(&chain, off, data)?;
        set_ent_cluster(&mut e, chain[0], fat32);
        wr32(&mut e, 28, new_size as u32);
        e[11] |= A_ARCH;
        set_mtime(&mut e, Timestamp::now());
        self.put_chain(chain);
        self.store(ino, e)?;
        Ok(data.len() as u32)
    }

    fn dir_empty(&mut self, ino: Ino) -> FsResult<bool> {
        let d = self.dir_of(ino)?;
        Ok(self.dc(d)?.ents.is_empty())
    }

    /// Free an entry's clusters, mark its slots deleted and drop its inode.
    fn remove_entry(&mut self, parent: Ino, d: Dir, de: &DEnt) -> FsResult<()> {
        let ent = self
            .by_loc
            .get(&(parent, de.off))
            .and_then(|i| self.nodes.get(i))
            .map(|n| n.ent)
            .unwrap_or(de.ent);
        let first = ent_cluster(&ent, self.b.fat32());
        if first != 0 {
            self.dcache.remove(&Dir::Chain(first));
            let mut ch = self.take_chain(first)?;
            self.truncate_chain(&mut ch, 0);
        }
        self.delete_slots(d, de.start, de.off)?;
        self.forget(parent, de.off);
        Ok(())
    }

    fn create_inner(&mut self, parent: Ino, name: &str, perm: u16, dir: bool) -> FsResult<Ino> {
        let u = valid_long_name(name)?;
        let (d, found) = self.find(parent, name)?;
        if found.is_some() {
            return Err(FsError::Exists);
        }
        let (short, nt, lfn) = self.short_for(name, d, None)?;
        let now = Timestamp::now();
        let fat32 = self.b.fat32();
        let mut attr = if dir { A_DIR } else { A_ARCH };
        if !dir && perm & 0o200 == 0 {
            attr |= A_RO;
        }
        let mut ent = new_ent(short, if lfn { 0 } else { nt }, attr, now);
        let mut chain = vec![];
        if dir {
            let up = self.dotdot_cluster(parent)?;
            self.extend_chain(&mut chain, 1)?;
            let c = chain[0];
            let mut blk = vec![0u8; self.b.cs as usize];
            let mut dot = new_ent(*b".          ", 0, A_DIR, now);
            set_ent_cluster(&mut dot, c, fat32);
            let mut dotdot = new_ent(*b"..         ", 0, A_DIR, now);
            set_ent_cluster(&mut dotdot, up, fat32);
            blk[0..32].copy_from_slice(&dot);
            blk[32..64].copy_from_slice(&dotdot);
            if let Err(e) = self.dev.write_at(self.cpos(c), &blk) {
                self.truncate_chain(&mut chain, 0);
                return Err(e.into());
            }
            set_ent_cluster(&mut ent, c, fat32);
        }
        let set = build_set(&u, &ent, lfn);
        let off = match self.insert_set(d, &set, name) {
            Ok(o) => o,
            Err(e) => {
                self.truncate_chain(&mut chain, 0);
                return Err(e);
            }
        };
        self.put_chain(chain);
        Ok(self.intern(parent, off, ent))
    }

    fn rename_inner(&mut self, fp: Ino, fname: &str, tp: Ino, tname: &str) -> FsResult<()> {
        let tu = valid_long_name(tname)?;
        let (fd, src) = self.find(fp, fname)?;
        let src = src.ok_or(FsError::NotFound)?;
        let src_ino = self.intern(fp, src.off, src.ent);
        let src_ent = self.ent(src_ino)?;
        let is_dir = src_ent[11] & A_DIR != 0;
        if fp == tp && src.name == tname {
            return Ok(());
        }
        if is_dir {
            let mut p = tp;
            loop {
                if p == src_ino {
                    return Err(FsError::Invalid(
                        "cannot move a directory into itself".into(),
                    ));
                }
                if p == ROOT {
                    break;
                }
                p = self.nodes.get(&p).ok_or(FsError::Stale)?.parent;
            }
        }
        let (td, dst) = self.find(tp, tname)?;
        if let Some(dst) = dst {
            if !(tp == fp && dst.off == src.off) {
                let dst_dir = dst.ent[11] & A_DIR != 0;
                if dst_dir && !is_dir {
                    return Err(FsError::IsDir);
                }
                if !dst_dir && is_dir {
                    return Err(FsError::NotDir);
                }
                if dst_dir {
                    let dino = self.intern(tp, dst.off, dst.ent);
                    if !self.dir_empty(dino)? {
                        return Err(FsError::NotEmpty);
                    }
                }
                self.remove_entry(tp, td, &dst)?;
            }
        }
        let exclude = if tp == fp { Some(src.off) } else { None };
        let (short, nt, lfn) = self.short_for(tname, td, exclude)?;
        let mut ent = src_ent;
        ent[0..11].copy_from_slice(&short);
        ent[12] = (ent[12] & !(NT_LOWER_BASE | NT_LOWER_EXT)) | if lfn { 0 } else { nt };
        let set = build_set(&tu, &ent, lfn);
        let new_off = self.insert_set(td, &set, tname)?;
        self.delete_slots(fd, src.start, src.off)?;
        self.by_loc.remove(&(fp, src.off));
        if let Some(n) = self.nodes.get_mut(&src_ino) {
            n.parent = tp;
            n.off = new_off;
            n.ent = ent;
        }
        self.by_loc.insert((tp, new_off), src_ino);
        if is_dir && fp != tp {
            let up = self.dotdot_cluster(tp)?;
            let d = Dir::Chain(ent_cluster(&ent, self.b.fat32()));
            let buf = self.read_dir(d)?;
            if buf.len() >= 64 && &buf[32..43] == b"..         " {
                let mut dd = [0u8; 32];
                dd.copy_from_slice(&buf[32..64]);
                set_ent_cluster(&mut dd, up, self.b.fat32());
                self.write_dir(d, 32, &dd)?;
            }
        }
        Ok(())
    }

    fn repair_inner(&mut self) -> FsResult<String> {
        self.flush_fat()?;
        let mut rep: Vec<String> = vec![];
        let bps = self.b.bps as u64;
        if let Some(o) = self.b.flag_off {
            let mut secs = vec![0u64];
            if self.b.backup_boot != 0 {
                secs.push(self.b.backup_boot as u64);
            }
            for (i, s) in secs.into_iter().enumerate() {
                let mut b = [0u8; 1];
                self.dev.read_at(s * bps + o as u64, &mut b)?;
                if b[0] & 1 != 0 {
                    self.dev.write_at(s * bps + o as u64, &[b[0] & !1])?;
                    rep.push(if i == 0 {
                        "Cleared the boot-sector dirty flag.".to_string()
                    } else {
                        "Cleared the dirty flag in the backup boot sector.".to_string()
                    });
                }
            }
        }
        if let Some((clean, herr)) = self.fat1_masks() {
            let v = self.fat_get(1);
            if v & clean == 0 {
                rep.push("Set the FAT[1] clean-shutdown bit.".into());
            }
            if v & herr == 0 {
                rep.push("Cleared the FAT[1] hard-error indication.".into());
            }
            if v & (clean | herr) != clean | herr {
                self.fat_set(1, v | clean | herr);
            }
        }
        let real = self.count_free();
        if real != self.free {
            self.free = real;
        }
        if self.next_free < 2 || self.next_free >= self.b.clusters + 2 {
            self.next_free = 2;
        }
        if self.b.fat32() {
            match self.read_fsinfo()? {
                Some((stored, _)) => {
                    if stored != real {
                        let was = if stored == 0xFFFF_FFFF {
                            "unknown".to_string()
                        } else {
                            stored.to_string()
                        };
                        rep.push(format!(
                            "FSInfo free-cluster count was {was}, recomputed as {real}."
                        ));
                    }
                    self.write_fsinfo()?;
                }
                None => rep.push("FSInfo sector missing or invalid; free count not stored.".into()),
            }
        }
        self.flush_fat()?;
        let active = self.b.active_fat;
        for copy in 0..self.b.nfats {
            if copy == active {
                continue;
            }
            let other = self.dev.read_vec(self.b.fat_start(copy), self.fat_loaded)?;
            let mine = &self.fat[..self.fat_loaded];
            let diff = other
                .chunks(bps as usize)
                .zip(mine.chunks(bps as usize))
                .filter(|(a, b)| a != b)
                .count();
            if diff > 0 {
                self.dev.write_at(self.b.fat_start(copy), mine)?;
                rep.push(format!(
                    "FAT #{} differed from FAT #{} in {diff} sector(s); rewritten from FAT #{}.",
                    copy + 1,
                    active + 1,
                    active + 1
                ));
            }
        }
        self.dev.flush()?;
        self.dirty_reason = None;
        self.marked = false;
        self.chains.clear();
        self.dcache.clear();
        if rep.is_empty() {
            Ok(format!(
                "{} volume is clean: no dirty flags set, free-cluster count ({real}) and FAT copies are consistent.",
                self.b.kind.label()
            ))
        } else {
            rep.push(format!("{} free clusters of {}.", real, self.b.clusters));
            Ok(rep.join("\n"))
        }
    }
}

impl Drop for FatFs {
    fn drop(&mut self) {
        if !self.ro {
            let _ = self.sync();
        }
    }
}

impl FileSystem for FatFs {
    fn volume_info(&self) -> VolumeInfo {
        VolumeInfo {
            fs_type: self.b.kind,
            label: self.label.clone(),
            uuid: self
                .b
                .serial
                .map(|s| format!("{:04X}-{:04X}", s >> 16, s & 0xFFFF))
                .unwrap_or_default(),
            read_only: self.ro,
        }
    }
    fn statfs(&mut self) -> FsResult<FsStat> {
        let cs = self.b.cs as u64;
        Ok(FsStat {
            block_size: self.b.cs,
            total_bytes: self.b.clusters as u64 * cs,
            free_bytes: self.free as u64 * cs,
            total_inodes: self.b.clusters as u64,
            free_inodes: self.free as u64,
        })
    }
    fn root_ino(&self) -> Ino {
        ROOT
    }
    fn lookup(&mut self, parent: Ino, name: &str) -> FsResult<Ino> {
        let (_, e) = self.find(parent, name)?;
        let e = e.ok_or(FsError::NotFound)?;
        Ok(self.intern(parent, e.off, e.ent))
    }
    fn getattr(&mut self, ino: Ino) -> FsResult<Attr> {
        self.attr_of(ino)
    }
    fn setattr(&mut self, ino: Ino, set: &SetAttr) -> FsResult<Attr> {
        self.begin()?;
        let r = (|| {
            if ino == ROOT {
                return Ok(());
            }
            if let Some(sz) = set.size {
                self.resize(ino, sz)?;
            }
            let mut e = self.ent(ino)?;
            let orig = e;
            if let Some(p) = set.perm {
                if e[11] & A_DIR == 0 {
                    if p & 0o200 == 0 {
                        e[11] |= A_RO;
                    } else {
                        e[11] &= !A_RO;
                    }
                }
            }
            if let Some(t) = set.mtime {
                set_mtime(&mut e, t);
            }
            if let Some(t) = set.atime {
                wr16(&mut e, 18, to_dos(t).0);
            }
            if e != orig {
                self.store(ino, e)?;
            }
            Ok(())
        })();
        self.end(r)?;
        self.attr_of(ino)
    }
    fn read(&mut self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>> {
        let e = self.file_ent(ino)?;
        let size = rd32(&e, 28) as u64;
        if offset >= size || len == 0 {
            return Ok(vec![]);
        }
        let n = (len as u64).min(size - offset);
        let chain = self.take_chain(ent_cluster(&e, self.b.fat32()))?;
        let r = self.read_chain(&chain, offset, n);
        self.put_chain(chain);
        r
    }
    fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<u32> {
        self.begin()?;
        let r = self.write_inner(ino, offset, data);
        self.end(r)
    }
    fn readdir(&mut self, dir: Ino) -> FsResult<Vec<DirEntry>> {
        let d = self.dir_of(dir)?;
        let ents: Vec<DEnt> = self.dc(d)?.ents.values().cloned().collect();
        let mut out = Vec::with_capacity(ents.len());
        for e in ents {
            let ino = self.intern(dir, e.off, e.ent);
            let kind = if e.ent[11] & A_DIR != 0 {
                FileKind::Dir
            } else {
                FileKind::File
            };
            out.push(DirEntry {
                name: e.name,
                ino,
                kind,
            });
        }
        Ok(out)
    }
    fn create(
        &mut self,
        parent: Ino,
        name: &str,
        perm: u16,
        _uid: u32,
        _gid: u32,
    ) -> FsResult<Ino> {
        self.begin()?;
        let r = self.create_inner(parent, name, perm, false);
        self.end(r)
    }
    fn mkdir(&mut self, parent: Ino, name: &str, perm: u16, _uid: u32, _gid: u32) -> FsResult<Ino> {
        self.begin()?;
        let r = self.create_inner(parent, name, perm, true);
        self.end(r)
    }
    fn symlink(&mut self, _: Ino, _: &str, _: &str, _: u32, _: u32) -> FsResult<Ino> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        Err(FsError::Unsupported("FAT has no symbolic links".into()))
    }
    fn readlink(&mut self, ino: Ino) -> FsResult<String> {
        self.attr_of(ino)?;
        Err(FsError::Invalid("not a symbolic link".into()))
    }
    fn unlink(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.begin()?;
        let r = (|| {
            let (d, de) = self.find(parent, name)?;
            let de = de.ok_or(FsError::NotFound)?;
            if de.ent[11] & A_DIR != 0 {
                return Err(FsError::IsDir);
            }
            self.remove_entry(parent, d, &de)
        })();
        self.end(r)
    }
    fn rmdir(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.begin()?;
        let r = (|| {
            let (d, de) = self.find(parent, name)?;
            let de = de.ok_or(FsError::NotFound)?;
            if de.ent[11] & A_DIR == 0 {
                return Err(FsError::NotDir);
            }
            let ino = self.intern(parent, de.off, de.ent);
            if !self.dir_empty(ino)? {
                return Err(FsError::NotEmpty);
            }
            self.remove_entry(parent, d, &de)
        })();
        self.end(r)
    }
    fn rename(
        &mut self,
        from_parent: Ino,
        from_name: &str,
        to_parent: Ino,
        to_name: &str,
    ) -> FsResult<()> {
        self.begin()?;
        let r = self.rename_inner(from_parent, from_name, to_parent, to_name);
        self.end(r)
    }
    fn sync(&mut self) -> FsResult<()> {
        if self.ro {
            return Ok(());
        }
        self.flush_fat()?;
        if self.marked {
            // Only after our own mutations: a mount that just reads writes nothing.
            self.write_fsinfo()?;
            self.set_dirty_flags(false)?;
            self.marked = false;
        }
        self.dev.flush()?;
        Ok(())
    }
    fn fingerprint(&mut self) -> Option<String> {
        // Boot sector (incl. the dirty flag), the whole FAT (any allocation / free) and the
        // root directory entries. FSInfo is advisory and left out.
        let boot = self.dev.read_vec(0, 512).ok()?;
        let fat = self.fat.clone();
        let root = listing_bytes(self, ROOT);
        Some(digest_parts([&boot[..], &fat[..], &root[..]]))
    }

    fn dirty_state(&mut self) -> DirtyState {
        match &self.dirty_reason {
            Some(r) => DirtyState::Dirty(r.clone()),
            None => DirtyState::Clean,
        }
    }
    fn repair(&mut self) -> FsResult<String> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        self.repair_inner()
    }
}

// ───────────────────────────── mkfs ─────────────────────────────

fn sanitize_label(label: &str) -> Option<[u8; 11]> {
    let mut v: Vec<u8> = label
        .chars()
        .map(|c| {
            let u = c.to_ascii_uppercase();
            if u.is_ascii() && (short_char_ok(u as u8) || u == ' ') {
                u as u8
            } else {
                b'_'
            }
        })
        .collect();
    v.truncate(11);
    while v.last() == Some(&b' ') {
        v.pop();
    }
    if v.is_empty() {
        return None;
    }
    let mut l = [b' '; 11];
    l[..v.len()].copy_from_slice(&v);
    Some(l)
}

/// FAT size (sectors) and cluster count for a candidate geometry.
fn calc_geometry(kind: FsType, ts: u64, reserved: u64, rds: u64, spc: u64) -> Option<(u64, u64)> {
    let bits: u64 = match kind {
        FsType::Fat12 => 12,
        FsType::Fat16 => 16,
        _ => 32,
    };
    let mut fatsz = 1u64;
    loop {
        let meta = reserved + rds + 2 * fatsz;
        if meta >= ts {
            return None;
        }
        let clusters = (ts - meta) / spc;
        let need = ((clusters + 2) * bits).div_ceil(8).div_ceil(512);
        if need <= fatsz {
            return Some((fatsz, clusters));
        }
        fatsz = need;
    }
}

pub fn format(dev: Arc<dyn BlockDev>, fs: FsType, label: &str) -> FsResult<()> {
    if dev.read_only() {
        return Err(FsError::ReadOnly);
    }
    let kind = fs;
    if !matches!(kind, FsType::Fat12 | FsType::Fat16 | FsType::Fat32) {
        return Err(FsError::Invalid(format!(
            "{} is not a FAT type",
            kind.label()
        )));
    }
    let ts = dev.size() / 512;
    let ts = if kind == FsType::Fat32 {
        ts.min(u32::MAX as u64)
    } else {
        ts
    };
    if ts > u32::MAX as u64 {
        return Err(FsError::Invalid(format!(
            "volume too large for {}",
            kind.label()
        )));
    }
    let (reserved, root_entries): (u64, u64) = match kind {
        FsType::Fat32 => (32, 0),
        FsType::Fat16 => (1, 512),
        _ => (1, if ts <= 5760 { 224 } else { 512 }),
    };
    let rds = root_entries * 32 / 512;
    let (min_c, max_c): (u64, u64) = match kind {
        FsType::Fat12 => (1, 4084),
        FsType::Fat16 => (4085, 65524),
        _ => (65525, 0x0FFF_FFF5),
    };
    let preferred: u64 = match kind {
        FsType::Fat12 => 1,
        FsType::Fat16 => match ts {
            0..=32680 => 2,
            32681..=262144 => 4,
            262145..=524288 => 8,
            524289..=1048576 => 16,
            1048577..=2097152 => 32,
            _ => 64,
        },
        _ => match ts {
            0..=532480 => 1,
            532481..=16777216 => 8,
            16777217..=33554432 => 16,
            33554433..=67108864 => 32,
            _ => 64,
        },
    };
    let mut cands = vec![preferred];
    cands.extend(
        [1u64, 2, 4, 8, 16, 32, 64]
            .iter()
            .copied()
            .filter(|&s| s != preferred),
    );
    let mut geo = None;
    for spc in cands {
        if let Some((fatsz, cl)) = calc_geometry(kind, ts, reserved, rds, spc) {
            if cl >= min_c && cl <= max_c {
                geo = Some((spc, fatsz, cl));
                break;
            }
        }
    }
    let Some((spc, fatsz, clusters)) = geo else {
        return Err(FsError::Invalid(match kind {
            FsType::Fat32 => "volume too small for FAT32: at least 65525 clusters are needed (about 33 MiB with 512-byte clusters)".into(),
            FsType::Fat16 => "volume size out of range for FAT16 (about 2.1 MiB – 2 GiB)".into(),
            _ => "volume size out of range for FAT12 (up to about 127 MiB)".into(),
        }));
    };
    let lab = sanitize_label(label);
    let now = Timestamp::now();
    let serial = (now.secs as u32).rotate_left(16) ^ now.nsecs;
    let fat32 = kind == FsType::Fat32;

    let mut bs = vec![0u8; 512];
    bs[0] = 0xEB;
    bs[1] = if fat32 { 0x58 } else { 0x3C };
    bs[2] = 0x90;
    bs[3..11].copy_from_slice(b"MSWIN4.1");
    wr16(&mut bs, 11, 512);
    bs[13] = spc as u8;
    wr16(&mut bs, 14, reserved as u16);
    bs[16] = 2;
    wr16(&mut bs, 17, root_entries as u16);
    if !fat32 && ts < 65536 {
        wr16(&mut bs, 19, ts as u16);
    } else {
        wr32(&mut bs, 32, ts as u32);
    }
    bs[21] = 0xF8;
    if !fat32 {
        wr16(&mut bs, 22, fatsz as u16);
    }
    wr16(&mut bs, 24, 32);
    wr16(&mut bs, 26, 64);
    let sig_off = if fat32 {
        wr32(&mut bs, 36, fatsz as u32);
        wr32(&mut bs, 44, 2);
        wr16(&mut bs, 48, 1);
        wr16(&mut bs, 50, 6);
        64
    } else {
        36
    };
    bs[sig_off] = 0x80;
    bs[sig_off + 2] = 0x29;
    wr32(&mut bs, sig_off + 3, serial);
    bs[sig_off + 7..sig_off + 18].copy_from_slice(&lab.unwrap_or(*b"NO NAME    "));
    bs[sig_off + 18..sig_off + 26].copy_from_slice(match kind {
        FsType::Fat12 => b"FAT12   ",
        FsType::Fat16 => b"FAT16   ",
        _ => b"FAT32   ",
    });
    // Boot code: "int 18h" (no bootable OS) followed by a halt loop.
    let code = sig_off + 26;
    bs[code..code + 5].copy_from_slice(&[0xFA, 0xCD, 0x18, 0xEB, 0xFE]);
    bs[510] = 0x55;
    bs[511] = 0xAA;

    zero_dev(dev.as_ref(), 0, reserved * 512)?;
    dev.write_at(0, &bs)?;
    let clusters32 = clusters as u32;
    if fat32 {
        let mut fsi = vec![0u8; 512];
        wr32(&mut fsi, 0, 0x4161_5252);
        wr32(&mut fsi, 484, 0x6141_7272);
        wr32(&mut fsi, 488, clusters32 - 1);
        wr32(&mut fsi, 492, 3);
        wr32(&mut fsi, 508, 0xAA55_0000);
        dev.write_at(512, &fsi)?;
        dev.write_at(6 * 512, &bs)?;
        dev.write_at(7 * 512, &fsi)?;
    }
    let mut head = vec![0u8; 512];
    match kind {
        FsType::Fat12 => head[0..3].copy_from_slice(&[0xF8, 0xFF, 0xFF]),
        FsType::Fat16 => head[0..4].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF]),
        _ => {
            wr32(&mut head, 0, 0x0FFF_FFF8);
            wr32(&mut head, 4, 0x0FFF_FFFF);
            wr32(&mut head, 8, 0x0FFF_FFFF);
        }
    }
    for i in 0..2u64 {
        let start = (reserved + i * fatsz) * 512;
        zero_dev(dev.as_ref(), start, fatsz * 512)?;
        dev.write_at(start, &head)?;
    }
    let root_off = (reserved + 2 * fatsz) * 512;
    let root_len = if fat32 { spc * 512 } else { rds * 512 };
    zero_dev(dev.as_ref(), root_off, root_len)?;
    if let Some(l) = lab {
        let e = new_ent(l, 0, A_VOL, now);
        dev.write_at(root_off, &e)?;
    }
    dev.flush()?;
    Ok(())
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::block::{FileDev, MemDev};
    use crate::vfs::testutil::{check_persisted, conformance, pattern};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub(crate) fn defaults() -> MountDefaults {
        MountDefaults {
            uid: 501,
            gid: 20,
            file_perm: 0o644,
            dir_perm: 0o755,
        }
    }

    fn mem(size: usize) -> (Arc<MemDev>, Arc<dyn BlockDev>) {
        let m = Arc::new(MemDev::new(size));
        let d: Arc<dyn BlockDev> = m.clone();
        (m, d)
    }

    fn roundtrip(kind: FsType, size: usize) {
        let (_m, dev) = mem(size);
        format(dev.clone(), kind, "Lumen Test").unwrap();
        assert_eq!(probe(dev.as_ref()), Some(kind));
        assert_eq!(crate::vfs::probe(dev.as_ref()), Some(kind));
        {
            let mut fs = FatFs::open(dev.clone(), false, defaults()).unwrap();
            assert_eq!(fs.volume_info().label, "LUMEN TEST");
            assert_eq!(fs.dirty_state(), DirtyState::Clean);
            conformance(&mut fs, false);
            assert!(matches!(
                fs.symlink(1, "l", "x", 0, 0),
                Err(FsError::Unsupported(_))
            ));
        }
        {
            let mut fs = FatFs::open(dev.clone(), true, defaults()).unwrap();
            assert_eq!(fs.dirty_state(), DirtyState::Clean);
            check_persisted(&mut fs);
            assert_eq!(fs.create(1, "x", 0o644, 0, 0), Err(FsError::ReadOnly));
            // free count reconciles with a full FAT scan
            assert_eq!(fs.free, fs.count_free());
            if kind == FsType::Fat32 {
                assert_eq!(fs.read_fsinfo().unwrap().unwrap().0, fs.free);
            }
        }
    }

    #[test]
    fn fat12_conformance() {
        roundtrip(FsType::Fat12, 8 << 20);
    }
    #[test]
    fn fat16_conformance() {
        roundtrip(FsType::Fat16, 48 << 20);
    }
    #[test]
    fn fat32_conformance() {
        roundtrip(FsType::Fat32, 64 << 20);
    }

    #[test]
    fn fat32_too_small_is_invalid() {
        let (_m, dev) = mem(16 << 20);
        assert!(matches!(
            format(dev, FsType::Fat32, ""),
            Err(FsError::Invalid(_))
        ));
    }

    #[test]
    fn cp437_table_complete() {
        assert_eq!(CP437_HI.chars().count(), 128);
    }

    #[test]
    fn probe_rejects_exfat_and_ntfs() {
        let mut b = vec![0u8; 512];
        b[0] = 0xEB;
        b[3..11].copy_from_slice(b"EXFAT   ");
        b[510] = 0x55;
        b[511] = 0xAA;
        let (_m, dev) = mem(1 << 20);
        dev.write_at(0, &b).unwrap();
        assert_eq!(probe(dev.as_ref()), None);
        b[3..11].copy_from_slice(b"NTFS    ");
        wr16(&mut b, 11, 512);
        b[13] = 8;
        dev.write_at(0, &b).unwrap();
        assert_eq!(probe(dev.as_ref()), None);
    }

    #[test]
    fn lfn_checksum_known_value() {
        // Short name "FILENA~1TXT" → checksum computed with the spec algorithm.
        let mut sum: u8 = 0;
        for &c in b"FILENA~1TXT" {
            sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(c);
        }
        assert_eq!(lfn_checksum(b"FILENA~1TXT"), sum);
    }

    fn raw_root(fs: &mut FatFs) -> Vec<u8> {
        let d = fs.dir_of(ROOT).unwrap();
        fs.read_dir(d).unwrap()
    }

    #[test]
    fn names_nt_flags_lfn_and_tails() {
        let (_m, dev) = mem(16 << 20);
        format(dev.clone(), FsType::Fat16, "NAMES").unwrap();
        let mut fs = FatFs::open(dev.clone(), false, defaults()).unwrap();
        fs.create(1, "lower.txt", 0o644, 0, 0).unwrap();
        fs.create(1, "UPPER.TXT", 0o644, 0, 0).unwrap();
        fs.create(1, "Mixed.Txt", 0o644, 0, 0).unwrap();
        fs.create(1, "a very long file name.text", 0o644, 0, 0)
            .unwrap();
        fs.create(1, "a very long file name.texture", 0o644, 0, 0)
            .unwrap();
        let buf = raw_root(&mut fs);
        let ents: Vec<&[u8]> = buf.chunks_exact(32).take_while(|e| e[0] != 0).collect();
        // label + lower(1) + UPPER(1) + Mixed(LFN+short) + (2 LFN + short) + (3 LFN + short)
        assert_eq!(ents.len(), 1 + 1 + 1 + 2 + 3 + 4);
        assert_eq!(&ents[1][0..11], b"LOWER   TXT");
        assert_eq!(ents[1][12], NT_LOWER_BASE | NT_LOWER_EXT);
        assert_eq!(&ents[2][0..11], b"UPPER   TXT");
        assert_eq!(ents[2][12], 0);
        assert_eq!(ents[3][11], A_LFN);
        assert_eq!(&ents[4][0..11], b"MIXED   TXT");
        assert_eq!(ents[3][13], lfn_checksum(&ents[4][0..11]));
        assert_eq!(&ents[7][0..11], b"AVERYL~1TEX");
        assert_eq!(&ents[11][0..11], b"AVERYL~2TEX");
        assert_eq!(ents[8][0], 0x43);
        assert_eq!(ents[10][0], 0x01);
        let mut names: Vec<String> = fs.readdir(1).unwrap().into_iter().map(|e| e.name).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "Mixed.Txt",
                "UPPER.TXT",
                "a very long file name.text",
                "a very long file name.texture",
                "lower.txt"
            ]
        );
        // case-insensitive lookup preserving case, short-name lookup
        let a = fs.lookup(1, "MIXED.TXT").unwrap();
        assert_eq!(fs.lookup(1, "mixed.txt").unwrap(), a);
        assert_eq!(fs.create(1, "LOWER.TXT", 0o644, 0, 0), Err(FsError::Exists));
        let l = fs.lookup(1, "A VERY LONG FILE NAME.TEXTURE").unwrap();
        assert_eq!(fs.lookup(1, "averyl~2.tex").unwrap(), l);
        // case-only rename keeps the inode
        fs.rename(1, "lower.txt", 1, "Lower.TXT").unwrap();
        let names: Vec<String> = fs.readdir(1).unwrap().into_iter().map(|e| e.name).collect();
        assert!(
            names.contains(&"Lower.TXT".to_string()) && !names.contains(&"lower.txt".to_string())
        );
        assert_eq!(
            fs.create(1, "bad:name", 0o644, 0, 0),
            Err(FsError::InvalidName)
        );
        assert_eq!(
            fs.create(1, &"x".repeat(256), 0o644, 0, 0),
            Err(FsError::NameTooLong)
        );
    }

    #[test]
    fn inode_stability_dotdot_and_readonly_attr() {
        let (_m, dev) = mem(64 << 20);
        format(dev.clone(), FsType::Fat32, "").unwrap();
        let mut fs = FatFs::open(dev.clone(), false, defaults()).unwrap();
        let a = fs.mkdir(1, "a", 0o755, 0, 0).unwrap();
        let b = fs.mkdir(1, "b", 0o755, 0, 0).unwrap();
        let sub = fs.mkdir(a, "sub", 0o755, 0, 0).unwrap();
        let e1 = fs.create(1, "empty1", 0o644, 0, 0).unwrap();
        let e2 = fs.create(1, "empty2", 0o644, 0, 0).unwrap();
        assert_ne!(e1, e2);
        fs.rename(a, "sub", b, "moved").unwrap();
        assert_eq!(fs.lookup(b, "moved").unwrap(), sub);
        fs.rename(1, "empty1", b, "e1 renamed").unwrap();
        assert_eq!(fs.lookup(b, "e1 renamed").unwrap(), e1);
        assert_eq!(fs.getattr(e1).unwrap().size, 0);
        // `..` of the moved dir now points at b's cluster
        let Dir::Chain(c) = fs.dir_of(sub).unwrap() else {
            panic!()
        };
        let Dir::Chain(bc) = fs.dir_of(b).unwrap() else {
            panic!()
        };
        let buf = fs.read_dir(Dir::Chain(c)).unwrap();
        assert_eq!(ent_cluster(&buf[32..64], true), bc);
        // moving into the root sets `..` to 0
        fs.rename(b, "moved", 1, "top").unwrap();
        let buf = fs.read_dir(Dir::Chain(c)).unwrap();
        assert_eq!(ent_cluster(&buf[32..64], true), 0);
        assert!(matches!(
            fs.rename(1, "a", a, "x"),
            Err(FsError::Invalid(_))
        ));
        // read-only attribute
        let at = fs
            .setattr(
                e2,
                &SetAttr {
                    perm: Some(0o444),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(at.perm & 0o222, 0);
        assert_ne!(fs.ent(e2).unwrap()[11] & A_RO, 0);
        let at = fs
            .setattr(
                e2,
                &SetAttr {
                    perm: Some(0o644),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(at.perm, 0o644);
        fs.sync().unwrap();
    }

    #[test]
    fn dirty_flag_lifecycle_and_doctor() {
        for (kind, size) in [
            (FsType::Fat16, 32usize << 20),
            (FsType::Fat32, 64 << 20),
            (FsType::Fat12, 4 << 20),
        ] {
            let (m, dev) = mem(size);
            format(dev.clone(), kind, "DIRTY").unwrap();
            let flag_off = if kind == FsType::Fat32 { 65 } else { 37 };
            {
                let mut fs = FatFs::open(dev.clone(), false, defaults()).unwrap();
                let f = fs.create(1, "f.bin", 0o644, 0, 0).unwrap();
                fs.write(f, 0, &pattern(10000, 1)).unwrap();
                // while there are unsynced mutations the volume is flagged dirty on disk
                assert_eq!(m.snapshot()[flag_off] & 1, 1);
                fs.sync().unwrap();
                assert_eq!(m.snapshot()[flag_off] & 1, 0);
            }
            // simulate an unclean shutdown + a bad FSInfo count + diverging FAT copy
            let fs0 = FatFs::open(dev.clone(), true, defaults()).unwrap();
            let b = fs0.b.clone();
            drop(fs0);
            let mut s0 = m.snapshot()[..512].to_vec();
            s0[flag_off] |= 1;
            dev.write_at(0, &s0).unwrap();
            match kind {
                FsType::Fat16 => dev
                    .write_at(b.fat_start(0) + 2, &0x7FFFu16.to_le_bytes())
                    .unwrap(),
                FsType::Fat32 => {
                    dev.write_at(b.fat_start(0) + 4, &0x07FF_FFFFu32.to_le_bytes())
                        .unwrap();
                    dev.write_at(512 + 488, &12345u32.to_le_bytes()).unwrap();
                }
                _ => {}
            }
            dev.write_at(b.fat_start(1) + 512 + 100, &[0xAB; 4])
                .unwrap();
            let mut fs = FatFs::open(dev.clone(), false, defaults()).unwrap();
            let DirtyState::Dirty(reason) = fs.dirty_state() else {
                panic!("not detected as dirty")
            };
            assert!(reason.contains("dirty flag"), "{reason}");
            if kind != FsType::Fat12 {
                assert!(reason.contains("FAT[1]"), "{reason}");
            }
            // dirty volumes are left alone by normal mutations (no auto-clear on sync)
            fs.sync().unwrap();
            assert_eq!(m.snapshot()[flag_off] & 1, 1);
            if kind == FsType::Fat32 {
                dev.write_at(512 + 488, &12345u32.to_le_bytes()).unwrap();
            }
            let rep = fs.repair().unwrap();
            assert!(rep.contains("Cleared the boot-sector dirty flag"), "{rep}");
            assert!(rep.contains("FAT #2 differed"), "{rep}");
            if kind == FsType::Fat32 {
                assert!(rep.contains("FSInfo free-cluster count was 12345"), "{rep}");
            }
            assert_eq!(fs.dirty_state(), DirtyState::Clean);
            drop(fs);
            let mut fs = FatFs::open(dev.clone(), false, defaults()).unwrap();
            assert_eq!(fs.dirty_state(), DirtyState::Clean);
            let snap = m.snapshot();
            let fl = b.fat_loaded_len();
            assert_eq!(
                &snap[b.fat_start(0) as usize..b.fat_start(0) as usize + fl],
                &snap[b.fat_start(1) as usize..b.fat_start(1) as usize + fl]
            );
            assert!(fs.repair().unwrap().contains("clean"));
        }
    }

    impl Bpb {
        fn fat_loaded_len(&self) -> usize {
            (self.fat_bytes_needed().div_ceil(self.bps as u64) * self.bps as u64) as usize
        }
    }

    #[test]
    fn corrupt_input_never_panics() {
        let (m, dev) = mem(8 << 20);
        format(dev.clone(), FsType::Fat12, "X").unwrap();
        {
            let mut fs = FatFs::open(dev.clone(), false, defaults()).unwrap();
            let d = fs.mkdir(1, "dir", 0o755, 0, 0).unwrap();
            for i in 0..20 {
                let f = fs
                    .create(d, &format!("some long file name {i}"), 0o644, 0, 0)
                    .unwrap();
                fs.write(f, 0, &pattern(3000, i)).unwrap();
            }
        }
        let clean = m.snapshot();
        let mut seed = 12345u32;
        for round in 0..200 {
            let img = clean.clone();
            let dev2 = Arc::new(MemDev::new(img.len()));
            dev2.write_at(0, &img).unwrap();
            // scribble over the first 160 KiB (boot sector, FATs, root dir, first data)
            for _ in 0..(1 + round % 40) {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let off = (seed as usize) % (160 * 1024);
                dev2.write_at(off as u64, &[(seed >> 8) as u8]).unwrap();
            }
            let d2: Arc<dyn BlockDev> = dev2;
            let _ = probe(d2.as_ref());
            if let Ok(mut fs) = FatFs::open(d2, false, defaults()) {
                if let Ok(ents) = fs.readdir(1) {
                    for e in ents {
                        let _ = fs.getattr(e.ino);
                        let _ = fs.read(e.ino, 0, 10000);
                        if let Ok(sub) = fs.readdir(e.ino) {
                            for s in sub {
                                let _ = fs.read(s.ino, 0, 5000);
                                let _ = fs.write(s.ino, 100, b"xyz");
                            }
                        }
                    }
                }
                let _ = fs.create(1, "new file name", 0o644, 0, 0);
                let _ = fs.repair();
            }
        }
    }

    // ── integration tests against real tools (run with --ignored) ──

    pub(crate) fn it_dir() -> PathBuf {
        let p = std::env::var("LUMENDISK_IT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/lumendisk-it")
            });
        std::fs::create_dir_all(&p).unwrap();
        p.canonicalize().unwrap()
    }

    /// Run a shell command in the toolbox container with `dir` mounted at /w.
    pub(crate) fn docker(dir: &Path, privileged: bool, script: &str) -> (bool, String) {
        let mut c = Command::new("docker");
        c.args(["run", "--rm"]);
        if privileged {
            c.arg("--privileged");
        }
        c.args([
            "-e",
            "MTOOLS_SKIP_CHECK=1",
            "-e",
            "LANG=C.UTF-8",
            "-e",
            "LC_ALL=C.UTF-8",
            "-v",
        ]);
        c.arg(format!("{}:/w", dir.display()));
        c.args(["lumendisk-toolbox", "sh", "-c", script]);
        let o = c.output().expect("docker not available");
        let mut s = String::from_utf8_lossy(&o.stdout).to_string();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        (o.status.success(), s)
    }

    pub(crate) fn make_image(path: &Path, size: u64) {
        let _ = std::fs::remove_file(path);
        let f = std::fs::File::create(path).unwrap();
        f.set_len(size).unwrap();
    }

    const LONG_NAME: &str = "Questo è un nome di file molto lungo – ünïcödé 文件.txt";
    const LONG_DIR: &str = "Cartella con nome lungo";

    /// Write extra names that the external tools must see.
    pub(crate) fn write_interop_files(fs: &mut dyn FileSystem) {
        let k = fs.lookup(1, "keep").unwrap();
        let d = fs.mkdir(k, LONG_DIR, 0o755, 0, 0).unwrap();
        let f = fs.create(d, LONG_NAME, 0o644, 0, 0).unwrap();
        fs.write(f, 0, b"contenuto unicode\n").unwrap();
        let f = fs.create(k, "lower.txt", 0o644, 0, 0).unwrap();
        fs.write(f, 0, b"lower case 8.3\n").unwrap();
        let f = fs.create(k, "MiXeD.TxT", 0o644, 0, 0).unwrap();
        fs.write(f, 0, b"mixed\n").unwrap();
        // many similar long names: numeric then hashed short-name tails must stay unique
        let many = fs.mkdir(k, "many", 0o755, 0, 0).unwrap();
        for i in 0..60 {
            let f = fs
                .create(many, &format!("similar long name {i:02}.txt"), 0o644, 0, 0)
                .unwrap();
            fs.write(f, 0, format!("n{i}").as_bytes()).unwrap();
        }
        for i in (0..60).step_by(3) {
            fs.unlink(many, &format!("SIMILAR LONG NAME {i:02}.TXT"))
                .unwrap();
        }
        // directory move across parents (`..` must be fixed)
        let m = fs.mkdir(1, "movable", 0o755, 0, 0).unwrap();
        let f = fs.create(m, "inside.txt", 0o644, 0, 0).unwrap();
        fs.write(f, 0, b"inside\n").unwrap();
        fs.rename(1, "movable", d, "moved dir").unwrap();
        fs.sync().unwrap();
    }

    fn fat_it(kind: FsType, mkfs: &str, size: u64, name: &str) {
        let dir = it_dir();
        let img = dir.join(name);
        make_image(&img, size);
        let (ok, out) = docker(&dir, false, &format!("{mkfs} /w/{name} && mcopy -i /w/{name} /etc/os-release ::/OS-RELEASE.TXT && mmd -i /w/{name} '::/Dir From Mtools' && mcopy -i /w/{name} /etc/hostname '::/Dir From Mtools/Long Name From Mtools.txt'"));
        assert!(ok, "mkfs failed: {out}");
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
        assert_eq!(probe(dev.as_ref()), Some(kind));
        {
            let mut fs = FatFs::open(dev.clone(), false, defaults()).unwrap();
            assert_eq!(fs.dirty_state(), DirtyState::Clean);
            // files written by mtools are readable
            let o = fs.lookup(1, "os-release.txt").unwrap();
            assert!(String::from_utf8(fs.read(o, 0, 4096).unwrap())
                .unwrap()
                .contains("debian"));
            let dm = fs.lookup(1, "dir from mtools").unwrap();
            fs.lookup(dm, "LONG NAME FROM MTOOLS.TXT").unwrap();
            conformance(&mut fs, false);
            write_interop_files(&mut fs);
        }
        {
            let mut fs = FatFs::open(dev.clone(), true, defaults()).unwrap();
            check_persisted(&mut fs);
        }
        drop(dev);
        let script = format!(
            "fsck.fat -n /w/{n}; echo FSCK_RC=$?; \
             mdir -/ -a -i /w/{n} ::/; \
             echo ---; mtype -i /w/{n} ::/keep/sub/hello.txt; echo; \
             echo ---; mtype -i /w/{n} '::/keep/{d}/{l}'; \
             echo ---; mtype -i /w/{n} '::/keep/{d}/moved dir/inside.txt'; \
             echo ---; mtype -i /w/{n} ::/keep/lower.txt; \
             mcopy -n -i /w/{n} ::/keep/persist.bin /tmp/p.bin && md5sum /tmp/p.bin; \
             mkdir /m && mount -o loop,ro,utf8 /w/{n} /m && ls -la '/m/keep/{d}/moved dir/..' && ls /m/keep && cat '/m/keep/{d}/{l}' && umount /m",
            n = name,
            d = LONG_DIR,
            l = LONG_NAME
        );
        let (_, out) = docker(&dir, true, &script);
        println!("{out}");
        assert!(
            out.contains("FSCK_RC=0"),
            "fsck.fat reported problems:\n{out}"
        );
        for want in [
            "hello lumendisk",
            "contenuto unicode",
            "inside",
            "lower case 8.3",
            LONG_NAME,
            LONG_DIR,
            "lower.txt",
            "MiXeD.TxT",
            "moved dir",
            "persist.bin",
            "similar long name 59.txt",
        ] {
            assert!(
                out.contains(want),
                "missing {want:?} in tool output:\n{out}"
            );
        }
        let md5 = {
            let p = pattern(300 * 1024, 7);
            std::fs::write(dir.join("expect.bin"), &p).unwrap();
            let o = Command::new("md5")
                .arg("-q")
                .arg(dir.join("expect.bin"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        assert!(out.contains(&md5), "persist.bin checksum mismatch:\n{out}");
        // macOS fsck_msdos as a second oracle
        let o = Command::new("/sbin/fsck_msdos")
            .arg("-n")
            .arg(&img)
            .output()
            .unwrap();
        let s =
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        println!("{s}");
        assert!(o.status.success(), "fsck_msdos failed:\n{s}");
    }

    #[test]
    #[ignore]
    fn it_real_mkfs_fat12() {
        fat_it(
            FsType::Fat12,
            "mkfs.fat -F 12 -n LUMEN12",
            8 << 20,
            "real12.img",
        );
    }
    #[test]
    #[ignore]
    fn it_real_mkfs_fat16() {
        fat_it(
            FsType::Fat16,
            "mkfs.fat -F 16 -n LUMEN16",
            64 << 20,
            "real16.img",
        );
    }
    #[test]
    #[ignore]
    fn it_real_mkfs_fat32() {
        fat_it(
            FsType::Fat32,
            "mkfs.fat -F 32 -n LUMEN32",
            128 << 20,
            "real32.img",
        );
    }

    /// Our own formatter's output must satisfy the upstream checkers too.
    fn own_format_it(kind: FsType, size: u64, name: &str) {
        let dir = it_dir();
        let img = dir.join(name);
        make_image(&img, size);
        {
            let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
            format(dev.clone(), kind, "LumenFmt").unwrap();
            let mut fs = FatFs::open(dev.clone(), false, defaults()).unwrap();
            conformance(&mut fs, false);
            write_interop_files(&mut fs);
        }
        let (_, out) = docker(
            &dir,
            false,
            &format!("fsck.fat -v -n /w/{name}; echo FSCK_RC=$?; mdir -/ -i /w/{name} ::/; mtype -i /w/{name} '::/keep/{LONG_DIR}/{LONG_NAME}'"),
        );
        println!("{out}");
        assert!(out.contains("FSCK_RC=0"), "{out}");
        assert!(out.contains("LUMENFMT"), "{out}");
        assert!(
            out.contains(LONG_NAME) && out.contains("contenuto unicode"),
            "{out}"
        );
        let o = Command::new("/sbin/fsck_msdos")
            .arg("-n")
            .arg(&img)
            .output()
            .unwrap();
        let s =
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        println!("{s}");
        assert!(o.status.success(), "fsck_msdos failed:\n{s}");
    }

    #[test]
    #[ignore]
    fn it_own_format_fat12() {
        own_format_it(FsType::Fat12, 8 << 20, "own12.img");
    }
    #[test]
    #[ignore]
    fn it_own_format_fat16() {
        own_format_it(FsType::Fat16, 64 << 20, "own16.img");
    }
    #[test]
    #[ignore]
    fn it_own_format_fat32() {
        own_format_it(FsType::Fat32, 128 << 20, "own32.img");
    }

    #[test]
    #[ignore]
    fn it_newfs_msdos_fat32() {
        let dir = it_dir();
        let img = dir.join("newfs32.img");
        make_image(&img, 96 << 20);
        let o = Command::new("/sbin/newfs_msdos")
            .args([
                "-F",
                "32",
                "-S",
                "512",
                "-s",
                &((96u64 << 20) / 512).to_string(),
                "-v",
                "NEWFS",
            ])
            .arg(&img)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        {
            let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
            let mut fs = FatFs::open(dev, false, defaults()).unwrap();
            assert_eq!(fs.volume_info().label, "NEWFS");
            conformance(&mut fs, false);
        }
        let o = Command::new("/sbin/fsck_msdos")
            .arg("-n")
            .arg(&img)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
        let (_, out) = docker(&dir, false, "fsck.fat -n /w/newfs32.img; echo FSCK_RC=$?");
        assert!(out.contains("FSCK_RC=0"), "{out}");
    }

    #[test]
    #[ignore]
    fn it_dirty_doctor_real_fsck() {
        let dir = it_dir();
        for (kind, mkfs, size, name) in [
            (FsType::Fat16, "-F 16", 64u64 << 20, "dirty16.img"),
            (FsType::Fat32, "-F 32", 128 << 20, "dirty32.img"),
        ] {
            let img = dir.join(name);
            make_image(&img, size);
            let (ok, out) = docker(&dir, false, &format!("mkfs.fat {mkfs} /w/{name}"));
            assert!(ok, "{out}");
            let flag_off = if kind == FsType::Fat32 { 65u64 } else { 37 };
            {
                let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
                let mut fs = FatFs::open(dev.clone(), false, defaults()).unwrap();
                let f = fs.create(1, "data.bin", 0o644, 0, 0).unwrap();
                fs.write(f, 0, &pattern(200_000, 9)).unwrap();
                fs.sync().unwrap();
                let b = fs.b.clone();
                std::mem::forget(fs); // simulate a crash: no further writes
                let mut s = [0u8; 1];
                dev.read_at(flag_off, &mut s).unwrap();
                dev.write_at(flag_off, &[s[0] | 1]).unwrap();
                if kind == FsType::Fat32 {
                    dev.write_at(512 + 488, &7u32.to_le_bytes()).unwrap();
                }
                dev.write_at(b.fat_start(1) + 64, &[0x55; 8]).unwrap();
            }
            let (_, out) = docker(
                &dir,
                false,
                &format!("fsck.fat -n /w/{name}; echo FSCK_RC=$?"),
            );
            println!("{out}");
            assert!(
                !out.contains("FSCK_RC=0"),
                "fsck should flag the damaged volume:\n{out}"
            );
            {
                let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
                let mut fs = FatFs::open(dev, false, defaults()).unwrap();
                assert!(matches!(fs.dirty_state(), DirtyState::Dirty(_)));
                let rep = fs.repair().unwrap();
                println!("{rep}");
                assert_eq!(fs.dirty_state(), DirtyState::Clean);
            }
            let (_, out) = docker(
                &dir,
                false,
                &format!("fsck.fat -n /w/{name}; echo FSCK_RC=$?"),
            );
            println!("{out}");
            assert!(out.contains("FSCK_RC=0"), "{out}");
            let o = Command::new("/sbin/fsck_msdos")
                .arg("-n")
                .arg(&img)
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
        }
    }

    /// Rough throughput check (run with `--ignored --nocapture`, ideally `--release`).
    #[test]
    #[ignore]
    fn it_perf_smoke() {
        let dir = it_dir();
        let img = dir.join("perf32.img");
        make_image(&img, 2 << 30);
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
        format(dev.clone(), FsType::Fat32, "PERF").unwrap();
        let mut fs = FatFs::open(dev, false, defaults()).unwrap();
        let t = std::time::Instant::now();
        let f = fs.create(1, "big.bin", 0o644, 0, 0).unwrap();
        let chunk = pattern(1 << 20, 5);
        for i in 0..512u64 {
            fs.write(f, i << 20, &chunk).unwrap();
        }
        fs.sync().unwrap();
        let w = t.elapsed();
        let t = std::time::Instant::now();
        for i in 0..512u64 {
            assert_eq!(fs.read(f, i << 20, 1 << 20).unwrap().len(), 1 << 20);
        }
        let r = t.elapsed();
        let t = std::time::Instant::now();
        let d = fs.mkdir(1, "many", 0o755, 0, 0).unwrap();
        for i in 0..5000 {
            fs.create(d, &format!("a file with a long name {i}.txt"), 0o644, 0, 0)
                .unwrap();
        }
        for i in (0..5000).step_by(7) {
            fs.lookup(d, &format!("A FILE WITH A LONG NAME {i}.TXT"))
                .unwrap();
        }
        let c = t.elapsed();
        println!("512 MiB write {w:?}, read {r:?}; 5000 creates + 715 lookups {c:?}");
        drop(fs);
        let _ = std::fs::remove_file(&img);
    }
}
