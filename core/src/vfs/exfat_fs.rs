//! exFAT driver — pure user space.
//!
//! * Boot region (main + backup) with boot checksum; VolumeFlags / PercentInUse are
//!   excluded from the checksum and are the only boot fields updated at runtime.
//! * Allocation bitmap and FAT are cached in memory; dirty sectors are written through at
//!   the end of each mutating operation.
//! * Directory entry sets (File 0x85 / Stream 0xC0 / FileName 0xC1) with SetChecksum and
//!   NameHash computed through the volume's up-case table.
//! * Files are allocated contiguously with the NoFatChain flag whenever possible; a file that
//!   can no longer grow contiguously is converted to a FAT chain.
//! * Dirty-bit doctor: VolumeFlags.VolumeDirty (and MediaFailure). The driver sets
//!   VolumeDirty while it has unsynced mutations and clears it on `sync()` / drop.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use super::fat_fs::{
    civil_to_unix, local_tm, local_to_unix, rd16, rd32, rd64, wr16, wr32, wr64, zero_dev,
};
use super::*;
use crate::block::BlockDev;

const ROOT: Ino = 1;
const T_BITMAP: u8 = 0x81;
const T_UPCASE: u8 = 0x82;
const T_LABEL: u8 = 0x83;
const T_FILE: u8 = 0x85;
const T_STREAM: u8 = 0xC0;
const T_NAME: u8 = 0xC1;
const A_RO: u16 = 0x01;
const A_DIR: u16 = 0x10;
const A_ARCH: u16 = 0x20;
const F_ALLOC: u8 = 0x01;
const F_NOFAT: u8 = 0x02;
const EOC: u32 = 0xFFFF_FFFF;
const VF_DIRTY: u16 = 0x0002;
const VF_MEDIA: u16 = 0x0004;
const MAX_DIR_BYTES: u64 = 256 << 20;
const CHAIN_CACHE: usize = 64;

fn corrupt(s: impl Into<String>) -> FsError {
    FsError::Corrupt(s.into())
}

// ───────────────────────────── checksums ─────────────────────────────

fn boot_checksum(region: &[u8]) -> u32 {
    let mut c: u32 = 0;
    for (i, &b) in region.iter().enumerate() {
        if i == 106 || i == 107 || i == 112 {
            continue;
        }
        c = c.rotate_right(1).wrapping_add(b as u32);
    }
    c
}

fn set_checksum(set: &[u8]) -> u16 {
    let mut c: u16 = 0;
    for (i, &b) in set.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        c = c.rotate_right(1).wrapping_add(b as u16);
    }
    c
}

fn table_checksum(t: &[u8]) -> u32 {
    let mut c: u32 = 0;
    for &b in t {
        c = c.rotate_right(1).wrapping_add(b as u32);
    }
    c
}

fn name_hash(up: &[u16]) -> u16 {
    let mut h: u16 = 0;
    for &u in up {
        for b in u.to_le_bytes() {
            h = h.rotate_right(1).wrapping_add(b as u16);
        }
    }
    h
}

/// Decompress an up-case table (0xFFFF n = n identity mappings).
fn decode_upcase(t: &[u8]) -> Vec<u16> {
    let mut map: Vec<u16> = (0..=0xFFFFu32).map(|c| c as u16).collect();
    let mut idx = 0usize;
    let mut i = 0usize;
    while i + 1 < t.len() && idx < 0x10000 {
        let v = rd16(t, i);
        i += 2;
        if v == 0xFFFF && i + 1 < t.len() && idx < 0xFFFF {
            idx += rd16(t, i) as usize;
            i += 2;
            continue;
        }
        map[idx] = v;
        idx += 1;
    }
    map
}

fn upcase_bytes() -> Vec<u8> {
    UPCASE_TABLE.iter().flat_map(|u| u.to_le_bytes()).collect()
}

// ───────────────────────────── timestamps ─────────────────────────────

fn ts_fields(t: Timestamp) -> (u32, u8, u8) {
    let tm = local_tm(t.secs);
    let off = (tm.gmtoff / 900).clamp(-64, 63) as i8;
    let utc = 0x80 | (off as u8 & 0x7F);
    if tm.year < 1980 {
        return ((1 << 21) | (1 << 16), 0, utc);
    }
    if tm.year > 2107 {
        return (
            (127 << 25) | (12 << 21) | (31 << 16) | (23 << 11) | (59 << 5) | 29,
            199,
            utc,
        );
    }
    let p = (((tm.year - 1980) as u32) << 25)
        | (tm.mon << 21)
        | (tm.day << 16)
        | (tm.hour << 11)
        | (tm.min << 5)
        | (tm.sec / 2);
    let ms = ((tm.sec % 2) * 100 + t.nsecs / 10_000_000).min(199) as u8;
    (p, ms, utc)
}

fn ts_from(p: u32, ms: u8, utc: u8) -> Timestamp {
    let y = 1980 + (p >> 25) as i64;
    let mo = (p >> 21) & 15;
    let d = (p >> 16) & 31;
    let h = ((p >> 11) & 31).min(23);
    let mi = ((p >> 5) & 63).min(59);
    let s = ((p & 31) * 2).min(59);
    if mo == 0 || mo > 12 || d == 0 {
        return Timestamp::default();
    }
    let mut secs = if utc & 0x80 != 0 {
        let off = (((utc << 1) as i8) >> 1) as i64;
        civil_to_unix(y, mo, d, h, mi, s) - off * 900
    } else {
        local_to_unix(y, mo, d, h, mi, s)
    };
    let ms = ms.min(199) as u32;
    secs += (ms / 100) as i64;
    Timestamp {
        secs,
        nsecs: (ms % 100) * 10_000_000,
    }
}

fn set_mtime(set: &mut [u8], t: Timestamp) {
    let (p, ms, u) = ts_fields(t);
    wr32(set, 12, p);
    set[21] = ms;
    set[23] = u;
}
fn set_atime(set: &mut [u8], t: Timestamp) {
    let (p, _, u) = ts_fields(t);
    wr32(set, 16, p);
    set[24] = u;
}

// ───────────────────────────── boot sector ─────────────────────────────

#[derive(Clone, Debug)]
struct Boot {
    bps: u64,
    cs: u64,
    fat_offset: u64,
    fat_length: u64,
    heap: u64,
    clusters: u32,
    root: u32,
    serial: u32,
    active: u8,
}

impl Boot {
    fn fat_start(&self) -> u64 {
        (self.fat_offset + self.active as u64 * self.fat_length) * self.bps
    }
}

fn parse_boot(b: &[u8]) -> Result<Boot, String> {
    if b.len() < 512 || &b[3..11] != b"EXFAT   " || b[510] != 0x55 || b[511] != 0xAA {
        return Err("not an exFAT boot sector".into());
    }
    let bps_shift = b[108] as u32;
    let spc_shift = b[109] as u32;
    if !(9..=12).contains(&bps_shift) || spc_shift > 25 - bps_shift {
        return Err("invalid sector / cluster size".into());
    }
    let bps = 1u64 << bps_shift;
    let cs = bps << spc_shift;
    let vol_len = rd64(b, 72);
    let fat_offset = rd32(b, 80) as u64;
    let fat_length = rd32(b, 84) as u64;
    let heap = rd32(b, 88) as u64;
    let clusters = rd32(b, 92);
    let root = rd32(b, 96);
    let nfats = b[110];
    if nfats == 0 || nfats > 2 {
        return Err("invalid number of FATs".into());
    }
    if fat_offset < 24 || fat_length == 0 || heap < fat_offset + fat_length * nfats as u64 {
        return Err("invalid FAT / heap layout".into());
    }
    if clusters == 0 || clusters > 0xFFFF_FFF5 || heap + ((clusters as u64) << spc_shift) > vol_len
    {
        return Err("invalid cluster count".into());
    }
    if fat_length * bps < (clusters as u64 + 2) * 4 {
        return Err("FAT too small".into());
    }
    if root < 2 || root >= clusters + 2 {
        return Err("invalid root cluster".into());
    }
    let flags = rd16(b, 106);
    let active = if nfats == 2 && flags & 1 != 0 { 1 } else { 0 };
    Ok(Boot {
        bps,
        cs,
        fat_offset,
        fat_length,
        heap,
        clusters,
        root,
        serial: rd32(b, 100),
        active,
    })
}

pub fn probe(dev: &dyn BlockDev) -> bool {
    if dev.size() < 512 {
        return false;
    }
    match dev.read_vec(0, 512) {
        Ok(b) => parse_boot(&b).is_ok(),
        Err(_) => false,
    }
}

// ───────────────────────────── directory parsing ─────────────────────────────

#[derive(Clone, Debug)]
struct XEnt {
    off: u64,
    set: Vec<u8>,
    name: String,
    units: Vec<u16>,
}

fn parse_set(buf: &[u8], i: usize) -> Option<XEnt> {
    let sc = buf[i + 1] as usize;
    if !(2..=18).contains(&sc) || i + (1 + sc) * 32 > buf.len() {
        return None;
    }
    let set = &buf[i..i + (1 + sc) * 32];
    if set[32] != T_STREAM || set_checksum(set) != rd16(set, 2) {
        return None;
    }
    let nl = set[35] as usize;
    if nl == 0 || nl.div_ceil(15) > sc - 1 {
        return None;
    }
    let mut units = Vec::with_capacity(nl);
    for k in 0..nl.div_ceil(15) {
        let e = &set[64 + k * 32..96 + k * 32];
        if e[0] != T_NAME {
            return None;
        }
        for j in 0..15 {
            if units.len() < nl {
                units.push(rd16(e, 2 + 2 * j));
            }
        }
    }
    Some(XEnt {
        off: i as u64,
        set: set.to_vec(),
        name: String::from_utf16_lossy(&units),
        units,
    })
}

fn parse_dir(buf: &[u8]) -> Vec<XEnt> {
    let mut out = vec![];
    let mut i = 0usize;
    while i + 32 <= buf.len() {
        let t = buf[i];
        if t == 0 {
            break;
        }
        if t == T_FILE {
            if let Some(x) = parse_set(buf, i) {
                i += x.set.len();
                out.push(x);
                continue;
            }
        }
        i += 32;
    }
    out
}

/// Cached directory: raw bytes plus an index of entry sets by up-cased name.
/// Kept up to date by `insert_set` / `delete_set` / `store`.
struct XCache {
    raw: Vec<u8>,
    ents: std::collections::BTreeMap<u64, XEnt>,
    by_name: HashMap<Vec<u16>, Vec<u64>>,
}

impl XCache {
    fn new(raw: Vec<u8>, upcase: &[u16]) -> Self {
        let mut c = XCache {
            raw,
            ents: Default::default(),
            by_name: HashMap::new(),
        };
        for e in parse_dir(&c.raw) {
            let key = e.units.iter().map(|&u| upcase[u as usize]).collect();
            c.add(e, key);
        }
        c
    }
    fn add(&mut self, e: XEnt, key: Vec<u16>) {
        self.by_name.entry(key).or_default().push(e.off);
        self.ents.insert(e.off, e);
    }
    fn remove(&mut self, off: u64, key: Option<Vec<u16>>) {
        if self.ents.remove(&off).is_none() {
            return;
        }
        match key.as_ref().and_then(|k| self.by_name.get_mut(k)) {
            Some(v) if v.contains(&off) => {
                v.retain(|&o| o != off);
                if v.is_empty() {
                    self.by_name.remove(key.as_ref().unwrap());
                }
            }
            _ => self.by_name.retain(|_, v| {
                v.retain(|&o| o != off);
                !v.is_empty()
            }),
        }
    }
}

fn make_set(file: &[u8], stream: &[u8], name: &[u16], hash: u16) -> Vec<u8> {
    let nn = name.len().div_ceil(15);
    let sc = 1 + nn;
    let mut s = vec![0u8; 32 * (1 + sc)];
    s[0..32].copy_from_slice(&file[0..32]);
    s[0] = T_FILE;
    s[1] = sc as u8;
    s[32..64].copy_from_slice(&stream[0..32]);
    s[32] = T_STREAM;
    s[32 + 3] = name.len() as u8;
    wr16(&mut s, 32 + 4, hash);
    for k in 0..nn {
        let e = &mut s[64 + 32 * k..96 + 32 * k];
        e[0] = T_NAME;
        e[1] = 0;
        for j in 0..15 {
            let idx = k * 15 + j;
            wr16(e, 2 + 2 * j, if idx < name.len() { name[idx] } else { 0 });
        }
    }
    let c = set_checksum(&s);
    wr16(&mut s, 2, c);
    s
}

fn valid_name(name: &str) -> FsResult<Vec<u16>> {
    check_name(name, 1024)?;
    if name
        .chars()
        .any(|c| (c as u32) < 0x20 || "\"*/:<>?\\|".contains(c))
    {
        return Err(FsError::InvalidName);
    }
    let u: Vec<u16> = name.encode_utf16().collect();
    if u.len() > 255 {
        return Err(FsError::NameTooLong);
    }
    Ok(u)
}

// Stream-extension accessors on a full entry set.
fn s_attr(s: &[u8]) -> u16 {
    rd16(s, 4)
}
fn s_flags(s: &[u8]) -> u8 {
    s[33]
}
fn s_vdl(s: &[u8]) -> u64 {
    rd64(s, 40)
}
fn s_first(s: &[u8]) -> u32 {
    rd32(s, 52)
}
fn s_len(s: &[u8]) -> u64 {
    rd64(s, 56)
}
fn s_set_alloc(s: &mut [u8], first: u32, nofat: bool, vdl: u64, len: u64) {
    s[33] = F_ALLOC | if nofat && first != 0 { F_NOFAT } else { 0 };
    wr64(s, 40, vdl);
    wr32(s, 52, first);
    wr64(s, 56, len);
}

#[derive(Clone, Copy, Debug)]
struct Ext {
    first: u32,
    nofat: bool,
}

// ───────────────────────────── driver ─────────────────────────────

struct XNode {
    parent: Ino,
    off: u64,
    set: Vec<u8>,
}

pub struct ExFatFs {
    dev: Arc<dyn BlockDev>,
    ro: bool,
    defaults: MountDefaults,
    b: Boot,
    fat: Vec<u8>,
    fat_dirty: BTreeSet<u64>,
    bitmap: Vec<u8>,
    bm_clusters: Vec<u32>,
    bm_dirty: BTreeSet<u64>,
    upcase: Vec<u16>,
    up_clusters: Vec<u32>,
    free: u32,
    hint: u32,
    nodes: HashMap<Ino, XNode>,
    by_loc: HashMap<(Ino, u64), Ino>,
    next_ino: Ino,
    chains: HashMap<u32, Vec<u32>>,
    /// Raw directory contents by directory inode, kept coherent by `dir_write` / `dir_grow`.
    dcache: HashMap<Ino, XCache>,
    dirty_reason: Option<String>,
    marked: bool,
    label: String,
}

impl ExFatFs {
    pub fn open(
        dev: Arc<dyn BlockDev>,
        read_only: bool,
        defaults: MountDefaults,
    ) -> FsResult<ExFatFs> {
        let ro = read_only || dev.read_only();
        if dev.size() < 512 {
            return Err(corrupt("device too small"));
        }
        let s0 = dev.read_vec(0, 512)?;
        let b = parse_boot(&s0).map_err(FsError::Corrupt)?;
        if (b.heap + b.clusters as u64 * (b.cs / b.bps)) * b.bps > dev.size() && !ro {
            return Err(corrupt("volume extends beyond the end of the device"));
        }
        let region = dev.read_vec(0, (b.bps * 12) as usize)?;
        let chk = boot_checksum(&region[..(b.bps * 11) as usize]);
        if rd32(&region, (b.bps * 11) as usize) != chk {
            return Err(corrupt("boot region checksum mismatch"));
        }
        let fat_bytes = ((b.clusters as u64 + 2) * 4).div_ceil(b.bps) * b.bps;
        let fat = dev.read_vec(b.fat_start(), fat_bytes as usize)?;
        let flags = rd16(&s0, 106);
        let mut fs = ExFatFs {
            dev,
            ro,
            defaults,
            b,
            fat,
            fat_dirty: BTreeSet::new(),
            bitmap: vec![],
            bm_clusters: vec![],
            bm_dirty: BTreeSet::new(),
            upcase: vec![],
            up_clusters: vec![],
            free: 0,
            hint: 2,
            nodes: HashMap::new(),
            by_loc: HashMap::new(),
            next_ino: 2,
            chains: HashMap::new(),
            dcache: HashMap::new(),
            dirty_reason: None,
            marked: false,
            label: String::new(),
        };
        let root_buf = {
            let (x, n) = fs.dir_ext(ROOT)?;
            fs.read_ext(x, n, 0, n * fs.b.cs)?
        };
        let mut bm = None;
        let mut up = None;
        for e in root_buf.chunks_exact(32) {
            match e[0] {
                0 => break,
                T_BITMAP if bm.is_none() && (e[1] & 1) as u8 == fs.b.active => {
                    bm = Some((rd32(e, 20), rd64(e, 24)))
                }
                T_UPCASE if up.is_none() => up = Some((rd32(e, 20), rd64(e, 24), rd32(e, 4))),
                T_LABEL => {
                    let n = (e[1] as usize).min(11);
                    let u: Vec<u16> = (0..n).map(|k| rd16(e, 2 + 2 * k)).collect();
                    fs.label = String::from_utf16_lossy(&u);
                }
                _ => {}
            }
        }
        let (bm_first, bm_len) = bm.ok_or_else(|| corrupt("allocation bitmap entry missing"))?;
        if bm_len < (fs.b.clusters as u64).div_ceil(8)
            || bm_len > (fs.b.clusters as u64).div_ceil(8) + fs.b.cs
        {
            return Err(corrupt("allocation bitmap has the wrong size"));
        }
        fs.bm_clusters = fs.system_chain(bm_first, bm_len)?;
        let data = fs.read_clusters(&fs.bm_clusters, bm_len)?;
        fs.bitmap = data;
        let (up_first, up_len, up_chk) =
            up.ok_or_else(|| corrupt("up-case table entry missing"))?;
        if up_len == 0 || up_len > 128 * 1024 {
            return Err(corrupt("up-case table has an invalid size"));
        }
        fs.up_clusters = fs.system_chain(up_first, up_len)?;
        let t = fs.read_clusters(&fs.up_clusters, up_len)?;
        if table_checksum(&t) != up_chk {
            return Err(corrupt("up-case table checksum mismatch"));
        }
        fs.upcase = decode_upcase(&t);
        fs.free = fs.count_free();
        let mut reasons = vec![];
        if flags & VF_DIRTY != 0 {
            reasons.push("VolumeDirty flag is set (volume was not unmounted cleanly)".to_string());
        }
        if flags & VF_MEDIA != 0 {
            reasons.push("MediaFailure flag is set (the medium reported I/O errors)".to_string());
        }
        if !reasons.is_empty() {
            fs.dirty_reason = Some(reasons.join("; "));
        }
        Ok(fs)
    }

    // ── FAT + bitmap ──

    fn count_free(&self) -> u32 {
        let n = self.b.clusters as usize;
        let full = n / 8;
        let mut used: u64 = self.bitmap[..full]
            .iter()
            .map(|b| b.count_ones() as u64)
            .sum();
        if n % 8 != 0 {
            used += (self.bitmap[full] & ((1u8 << (n % 8)) - 1)).count_ones() as u64;
        }
        (n as u64 - used) as u32
    }
    fn fat_get(&self, c: u32) -> u32 {
        if c as u64 >= self.b.clusters as u64 + 2 {
            return 0xFFFF_FFF7;
        }
        rd32(&self.fat, c as usize * 4)
    }
    fn fat_set(&mut self, c: u32, v: u32) {
        if c as u64 >= self.b.clusters as u64 + 2 {
            return;
        }
        wr32(&mut self.fat, c as usize * 4, v);
        self.fat_dirty.insert(c as u64 * 4 / self.b.bps);
    }
    fn bm_get(&self, c: u32) -> bool {
        if c < 2 {
            return true;
        }
        let i = (c - 2) as usize;
        self.bitmap
            .get(i / 8)
            .map_or(true, |b| b >> (i % 8) & 1 != 0)
    }
    fn bm_set(&mut self, c: u32, v: bool) {
        if c < 2 || c as u64 >= self.b.clusters as u64 + 2 {
            return;
        }
        let i = (c - 2) as usize;
        let old = self.bm_get(c);
        if old == v {
            return;
        }
        if !v {
            let cs = self.b.cs as u64;
            self.dev.note_freed(self.cpos(c), cs);
        }
        if v {
            self.bitmap[i / 8] |= 1 << (i % 8);
            self.free = self.free.saturating_sub(1);
        } else {
            self.bitmap[i / 8] &= !(1 << (i % 8));
            self.free += 1;
        }
        self.bm_dirty.insert((i / 8) as u64 / self.b.bps);
    }
    fn cpos(&self, c: u32) -> u64 {
        self.b.heap * self.b.bps + (c as u64 - 2) * self.b.cs
    }
    fn valid(&self, c: u32) -> bool {
        c >= 2 && (c as u64) < self.b.clusters as u64 + 2
    }
    /// Clusters of a bitmap / up-case table (FAT chain, or contiguous if the FAT is empty).
    fn system_chain(&self, first: u32, len: u64) -> FsResult<Vec<u32>> {
        let n = len.div_ceil(self.b.cs) as u32;
        if !self.valid(first) || first as u64 + n as u64 > self.b.clusters as u64 + 2 {
            return Err(corrupt("system structure outside the cluster heap"));
        }
        if self.fat_get(first) == 0 {
            return Ok((first..first + n).collect());
        }
        self.chain_n(first, n as u64)
    }
    fn read_clusters(&self, ch: &[u32], len: u64) -> FsResult<Vec<u8>> {
        let mut out = vec![0u8; len as usize];
        let cs = self.b.cs as usize;
        for (i, &c) in ch.iter().enumerate() {
            let a = i * cs;
            if a >= out.len() {
                break;
            }
            let e = (a + cs).min(out.len());
            self.dev.read_at(self.cpos(c), &mut out[a..e])?;
        }
        Ok(out)
    }
    fn flush_meta(&mut self) -> FsResult<()> {
        let bps = self.b.bps;
        let fsecs: Vec<u64> = std::mem::take(&mut self.fat_dirty).into_iter().collect();
        for s in fsecs {
            let a = (s * bps) as usize;
            if a >= self.fat.len() {
                continue;
            }
            let e = (a + bps as usize).min(self.fat.len());
            self.dev
                .write_at(self.b.fat_start() + a as u64, &self.fat[a..e])?;
        }
        let bsecs: Vec<u64> = std::mem::take(&mut self.bm_dirty).into_iter().collect();
        for s in bsecs {
            let a = (s * bps) as usize;
            if a >= self.bitmap.len() {
                continue;
            }
            let e = (a + bps as usize).min(self.bitmap.len());
            let ci = a / self.b.cs as usize;
            let within = (a % self.b.cs as usize) as u64;
            let Some(&c) = self.bm_clusters.get(ci) else {
                continue;
            };
            self.dev
                .write_at(self.cpos(c) + within, &self.bitmap[a..e])?;
        }
        Ok(())
    }

    fn chain_n(&self, first: u32, n: u64) -> FsResult<Vec<u32>> {
        let mut v = Vec::with_capacity(n.min(1 << 20) as usize);
        let mut c = first;
        while (v.len() as u64) < n {
            if !self.valid(c) {
                return Err(corrupt(format!(
                    "cluster {c:#x} out of range in chain starting at {first}"
                )));
            }
            v.push(c);
            if v.len() as u64 == n {
                break;
            }
            if v.len() > self.b.clusters as usize {
                return Err(corrupt("cluster chain loop"));
            }
            c = self.fat_get(c);
            if c >= 0xFFFF_FFF7 {
                return Err(corrupt(format!(
                    "cluster chain starting at {first} is shorter than the file"
                )));
            }
        }
        Ok(v)
    }
    fn take_chain(&mut self, first: u32, n: u64) -> FsResult<Vec<u32>> {
        if let Some(v) = self.chains.remove(&first) {
            if v.len() as u64 == n {
                return Ok(v);
            }
        }
        self.chain_n(first, n)
    }
    fn put_chain(&mut self, ch: Vec<u32>) {
        if let Some(&f) = ch.first() {
            if self.chains.len() >= CHAIN_CACHE {
                self.chains.clear();
            }
            self.chains.insert(f, ch);
        }
    }
    fn root_len(&mut self) -> FsResult<u64> {
        let mut c = self.b.root;
        let mut n = 0u64;
        loop {
            if !self.valid(c) {
                return Err(corrupt("root directory chain out of range"));
            }
            n += 1;
            if n > self.b.clusters as u64 || n * self.b.cs > MAX_DIR_BYTES {
                return Err(corrupt("root directory chain loop"));
            }
            let nx = self.fat_get(c);
            if nx >= 0xFFFF_FFF8 {
                break;
            }
            if nx == 0xFFFF_FFF7 || nx < 2 {
                return Err(corrupt("broken root directory chain"));
            }
            c = nx;
        }
        Ok(n)
    }

    /// Device ranges of bytes `[off, off+len)` of a stream with `alloc` clusters.
    fn runs(&mut self, x: Ext, alloc: u64, off: u64, len: u64) -> FsResult<Vec<(u64, u64)>> {
        let cs = self.b.cs;
        if len == 0 {
            return Ok(vec![]);
        }
        if off + len > alloc * cs {
            return Err(corrupt("access beyond allocated clusters"));
        }
        if x.nofat {
            if !self.valid(x.first) || x.first as u64 + alloc > self.b.clusters as u64 + 2 {
                return Err(corrupt("contiguous stream outside the cluster heap"));
            }
            return Ok(vec![(self.cpos(x.first) + off, len)]);
        }
        let ch = self.take_chain(x.first, alloc)?;
        let mut out: Vec<(u64, u64)> = vec![];
        let mut pos = off;
        let end = off + len;
        while pos < end {
            let ci = (pos / cs) as usize;
            let within = pos % cs;
            let n = (cs - within).min(end - pos);
            let d = self.cpos(ch[ci]) + within;
            match out.last_mut() {
                Some(l) if l.0 + l.1 == d => l.1 += n,
                _ => out.push((d, n)),
            }
            pos += n;
        }
        self.put_chain(ch);
        Ok(out)
    }
    fn read_ext(&mut self, x: Ext, alloc: u64, off: u64, len: u64) -> FsResult<Vec<u8>> {
        let mut out = vec![0u8; len as usize];
        let mut p = 0usize;
        for (d, n) in self.runs(x, alloc, off, len)? {
            self.dev.read_at(d, &mut out[p..p + n as usize])?;
            p += n as usize;
        }
        Ok(out)
    }
    /// `payload`: file content (ordered-mode data write) vs directory entries (journaled).
    fn write_ext(&mut self, x: Ext, alloc: u64, off: u64, data: &[u8], payload: bool) -> FsResult<()> {
        let mut p = 0usize;
        for (d, n) in self.runs(x, alloc, off, data.len() as u64)? {
            if payload {
                self.dev.write_data_at(d, &data[p..p + n as usize])?;
            } else {
                self.dev.write_at(d, &data[p..p + n as usize])?;
            }
            p += n as usize;
        }
        Ok(())
    }
    fn zero_ext(&mut self, x: Ext, alloc: u64, off: u64, len: u64, payload: bool) -> FsResult<()> {
        for (d, n) in self.runs(x, alloc, off, len)? {
            if payload {
                super::fat_fs::zero_dev_data(self.dev.as_ref(), d, n)?;
            } else {
                zero_dev(self.dev.as_ref(), d, n)?;
            }
        }
        Ok(())
    }

    fn find_run(&self, k: u32, hint: u32) -> Option<u32> {
        let hi = self.b.clusters + 2;
        let hint = if hint >= 2 && hint < hi { hint } else { 2 };
        for (lo, end) in [
            (hint, hi),
            (2, (hint as u64 + k as u64).min(hi as u64) as u32),
        ] {
            let mut c = lo;
            let mut run = 0u32;
            let mut start = c;
            while c < end {
                let i = (c - 2) as usize;
                if i % 8 == 0 && c + 8 <= end && self.bitmap[i / 8] == 0xFF {
                    run = 0;
                    c += 8;
                    continue;
                }
                if self.bm_get(c) {
                    run = 0;
                } else {
                    if run == 0 {
                        start = c;
                    }
                    run += 1;
                    if run == k {
                        return Some(start);
                    }
                }
                c += 1;
            }
        }
        None
    }

    /// Resize a stream's allocation from `have` to `need` clusters.
    fn realloc(&mut self, x: &mut Ext, have: u64, need: u64) -> FsResult<()> {
        if need == have {
            return Ok(());
        }
        if need < have {
            if x.nofat {
                for c in x.first + need as u32..x.first + have as u32 {
                    self.bm_set(c, false);
                }
            } else {
                let mut ch = self.take_chain(x.first, have)?;
                for i in need as usize..ch.len() {
                    let c = ch[i];
                    self.fat_set(c, 0);
                    self.bm_set(c, false);
                }
                if need > 0 {
                    self.fat_set(ch[need as usize - 1], EOC);
                }
                ch.truncate(need as usize);
                self.put_chain(ch);
            }
            if need == 0 {
                x.first = 0;
                x.nofat = false;
            }
            return Ok(());
        }
        let k64 = need - have;
        if k64 > self.free as u64 {
            return Err(FsError::NoSpace);
        }
        let k = k64 as u32;
        if have == 0 {
            if let Some(s) = self.find_run(k, self.hint) {
                for c in s..s + k {
                    self.bm_set(c, true);
                }
                x.first = s;
                x.nofat = true;
                self.hint = s + k;
                return Ok(());
            }
            let mut ch = vec![];
            self.extend_fat_chain(&mut ch, k)?;
            x.first = ch[0];
            x.nofat = false;
            self.put_chain(ch);
            return Ok(());
        }
        if x.nofat {
            let end = x.first as u64 + have;
            if end + k64 <= self.b.clusters as u64 + 2
                && (end..end + k64).all(|c| !self.bm_get(c as u32))
            {
                for c in end..end + k64 {
                    self.bm_set(c as u32, true);
                }
                self.hint = (end + k64) as u32;
                return Ok(());
            }
            // No room to stay contiguous: convert to a FAT chain.
            let ch: Vec<u32> = (x.first..end as u32).collect();
            for w in ch.windows(2) {
                self.fat_set(w[0], w[1]);
            }
            self.fat_set(end as u32 - 1, EOC);
            x.nofat = false;
            self.put_chain(ch);
        }
        let mut ch = self.take_chain(x.first, have)?;
        self.extend_fat_chain(&mut ch, k)?;
        self.put_chain(ch);
        Ok(())
    }
    fn extend_fat_chain(&mut self, ch: &mut Vec<u32>, k: u32) -> FsResult<()> {
        let old_last = ch.last().copied();
        let mut prev = old_last;
        let mut hint = prev.map(|p| p + 1).unwrap_or(self.hint);
        let mut added = vec![];
        for _ in 0..k {
            let Some(c) = self.find_run(1, hint) else {
                for &c in &added {
                    self.fat_set(c, 0);
                    self.bm_set(c, false);
                }
                if let Some(l) = old_last {
                    self.fat_set(l, EOC);
                }
                return Err(FsError::NoSpace);
            };
            self.bm_set(c, true);
            self.fat_set(c, EOC);
            if let Some(p) = prev {
                self.fat_set(p, c);
            }
            prev = Some(c);
            added.push(c);
            hint = c + 1;
        }
        self.hint = hint;
        ch.extend(added);
        Ok(())
    }

    // ── directories ──

    /// (extent, allocated clusters) of a directory inode.
    fn dir_ext(&mut self, ino: Ino) -> FsResult<(Ext, u64)> {
        if ino == ROOT {
            let n = self.root_len()?;
            return Ok((
                Ext {
                    first: self.b.root,
                    nofat: false,
                },
                n,
            ));
        }
        let n = self.nodes.get(&ino).ok_or(FsError::Stale)?;
        if s_attr(&n.set) & A_DIR == 0 {
            return Err(FsError::NotDir);
        }
        let len = s_len(&n.set);
        let first = s_first(&n.set);
        if first == 0 || len == 0 {
            return Err(corrupt("directory without clusters"));
        }
        if len > MAX_DIR_BYTES {
            return Err(corrupt("directory too large"));
        }
        Ok((
            Ext {
                first,
                nofat: s_flags(&n.set) & F_NOFAT != 0,
            },
            len.div_ceil(self.b.cs),
        ))
    }
    /// Parsed + raw view of a directory, built on first use and maintained incrementally.
    fn dc(&mut self, ino: Ino) -> FsResult<&mut XCache> {
        if !self.dcache.contains_key(&ino) {
            let (x, n) = self.dir_ext(ino)?;
            let raw = self.read_ext(x, n, 0, n * self.b.cs)?;
            let c = XCache::new(raw, &self.upcase);
            if self.dcache.len() >= 32 {
                self.dcache.clear();
            }
            self.dcache.insert(ino, c);
        }
        Ok(self.dcache.get_mut(&ino).expect("just inserted"))
    }
    fn dir_write(&mut self, ino: Ino, off: u64, data: &[u8]) -> FsResult<()> {
        let r = self
            .dir_ext(ino)
            .and_then(|(x, n)| self.write_ext(x, n, off, data, false));
        match (r.is_ok(), self.dcache.get_mut(&ino)) {
            (true, Some(c)) if off as usize + data.len() <= c.raw.len() => {
                c.raw[off as usize..off as usize + data.len()].copy_from_slice(data);
            }
            _ => {
                self.dcache.remove(&ino);
            }
        }
        r
    }
    fn dir_grow(&mut self, ino: Ino) -> FsResult<()> {
        let (mut x, n) = self.dir_ext(ino)?;
        if (n + 1) * self.b.cs > MAX_DIR_BYTES {
            return Err(FsError::NoSpace);
        }
        self.realloc(&mut x, n, n + 1)?;
        let r = (|| {
            self.zero_ext(x, n + 1, n * self.b.cs, self.b.cs, false)?;
            if ino != ROOT {
                let len = (n + 1) * self.b.cs;
                let mut set = self.nodes.get(&ino).ok_or(FsError::Stale)?.set.clone();
                s_set_alloc(&mut set, x.first, x.nofat, len, len);
                self.store(ino, set)?;
            }
            Ok(())
        })();
        let total = ((n + 1) * self.b.cs) as usize;
        match (r.is_ok(), self.dcache.get_mut(&ino)) {
            (true, Some(c)) if c.raw.len() + self.b.cs as usize == total => c.raw.resize(total, 0),
            _ => {
                self.dcache.remove(&ino);
            }
        }
        r
    }
    /// Write an entry set into free slots (growing the directory if needed) and index it.
    fn insert_set(&mut self, dir: Ino, set: &[u8]) -> FsResult<u64> {
        let need = set.len() / 32;
        let (mut run_start, run_len, nslots) = {
            let buf = &self.dc(dir)?.raw;
            let nslots = buf.len() / 32;
            let (mut run_start, mut run_len, mut ended) = (0usize, 0usize, false);
            for i in 0..nslots {
                let t = buf[i * 32];
                if t == 0 {
                    ended = true;
                }
                if ended || t & 0x80 == 0 {
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
            let clusters = ((need - run_len) as u64 * 32).div_ceil(self.b.cs);
            for _ in 0..clusters {
                self.dir_grow(dir)?;
            }
        }
        let off = run_start as u64 * 32;
        self.dir_write(dir, off, set)?;
        if let Some(e) = parse_set(set, 0) {
            let key = self.up(&e.units);
            if let Some(c) = self.dcache.get_mut(&dir) {
                c.add(XEnt { off, ..e }, key);
            }
        }
        Ok(off)
    }
    fn delete_set(&mut self, dir: Ino, off: u64, set: &[u8]) -> FsResult<()> {
        let mut del = set.to_vec();
        for k in 0..set.len() / 32 {
            del[k * 32] &= 0x7F;
        }
        self.dir_write(dir, off, &del)?;
        let key = parse_set(set, 0).map(|e| self.up(&e.units));
        if let Some(c) = self.dcache.get_mut(&dir) {
            c.remove(off, key);
        }
        Ok(())
    }
    fn up(&self, u: &[u16]) -> Vec<u16> {
        u.iter().map(|&c| self.upcase[c as usize]).collect()
    }
    fn find(&mut self, parent: Ino, name: &str) -> FsResult<Option<XEnt>> {
        let key = self.up(&name.encode_utf16().collect::<Vec<u16>>());
        let c = self.dc(parent)?;
        Ok(c.by_name
            .get(&key)
            .and_then(|v| v.first())
            .and_then(|o| c.ents.get(o))
            .cloned())
    }

    // ── inode table ──

    fn intern(&mut self, parent: Ino, off: u64, set: &[u8]) -> Ino {
        if let Some(&i) = self.by_loc.get(&(parent, off)) {
            return i;
        }
        let i = self.next_ino;
        self.next_ino += 1;
        self.nodes.insert(
            i,
            XNode {
                parent,
                off,
                set: set.to_vec(),
            },
        );
        self.by_loc.insert((parent, off), i);
        i
    }
    fn forget(&mut self, parent: Ino, off: u64) {
        if let Some(i) = self.by_loc.remove(&(parent, off)) {
            self.nodes.remove(&i);
        }
    }
    fn set_of(&self, ino: Ino) -> FsResult<Vec<u8>> {
        Ok(self.nodes.get(&ino).ok_or(FsError::Stale)?.set.clone())
    }
    fn store(&mut self, ino: Ino, mut set: Vec<u8>) -> FsResult<()> {
        let c = set_checksum(&set);
        wr16(&mut set, 2, c);
        let (parent, off) = {
            let n = self.nodes.get_mut(&ino).ok_or(FsError::Stale)?;
            n.set = set.clone();
            (n.parent, n.off)
        };
        self.dir_write(parent, off, &set)?;
        if let Some(e) = self
            .dcache
            .get_mut(&parent)
            .and_then(|c| c.ents.get_mut(&off))
        {
            e.set = set;
        }
        Ok(())
    }
    fn file_set(&self, ino: Ino) -> FsResult<Vec<u8>> {
        if ino == ROOT {
            return Err(FsError::IsDir);
        }
        let s = self.set_of(ino)?;
        if s_attr(&s) & A_DIR != 0 {
            return Err(FsError::IsDir);
        }
        Ok(s)
    }

    fn attr_of(&mut self, ino: Ino) -> FsResult<Attr> {
        if ino == ROOT {
            let n = self.root_len()? * self.b.cs;
            return Ok(Attr {
                ino,
                kind: FileKind::Dir,
                size: n,
                perm: self.defaults.dir_perm,
                nlink: 2,
                uid: self.defaults.uid,
                gid: self.defaults.gid,
                atime: Timestamp::default(),
                mtime: Timestamp::default(),
                ctime: Timestamp::default(),
                used_bytes: n,
            });
        }
        let s = self.set_of(ino)?;
        let dir = s_attr(&s) & A_DIR != 0;
        let size = s_len(&s);
        let mut perm = if dir {
            self.defaults.dir_perm
        } else {
            self.defaults.file_perm
        };
        if s_attr(&s) & A_RO != 0 {
            perm &= !0o222;
        }
        let mtime = ts_from(rd32(&s, 12), s[21], s[23]);
        let atime = ts_from(rd32(&s, 16), 0, s[24]);
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
            used_bytes: size.div_ceil(self.b.cs) * self.b.cs,
        })
    }

    // ── mutation bracket / flags ──

    fn begin(&mut self) -> FsResult<()> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        if !self.marked && self.dirty_reason.is_none() {
            self.set_volume_flags(VF_DIRTY, true)?;
            self.marked = true;
        }
        Ok(())
    }
    fn end<T>(&mut self, r: FsResult<T>) -> FsResult<T> {
        let f = self.flush_meta();
        match r {
            Ok(v) => f.map(|_| v),
            Err(e) => Err(e),
        }
    }
    fn set_volume_flags(&mut self, mask: u16, on: bool) -> FsResult<()> {
        let mut b = [0u8; 2];
        self.dev.read_at(106, &mut b)?;
        let v = u16::from_le_bytes(b);
        let nv = if on { v | mask } else { v & !mask };
        if nv != v {
            self.dev.write_at(106, &nv.to_le_bytes())?;
        }
        Ok(())
    }
    fn write_percent(&mut self) -> FsResult<()> {
        let used = self.b.clusters as u64 - self.free as u64;
        let pct = (used * 100 / self.b.clusters as u64) as u8;
        let mut b = [0u8; 1];
        self.dev.read_at(112, &mut b)?;
        if b[0] != pct {
            self.dev.write_at(112, &[pct])?;
        }
        Ok(())
    }

    fn resize(&mut self, ino: Ino, new: u64) -> FsResult<()> {
        let mut s = self.file_set(ino)?;
        let size = s_len(&s);
        if new == size {
            return Ok(());
        }
        let cs = self.b.cs;
        let mut x = Ext {
            first: s_first(&s),
            nofat: s_flags(&s) & F_NOFAT != 0,
        };
        self.realloc(&mut x, size.div_ceil(cs), new.div_ceil(cs))?;
        let vdl = s_vdl(&s).min(new);
        s_set_alloc(&mut s, x.first, x.nofat, vdl, new);
        let a = s_attr(&s) | A_ARCH;
        wr16(&mut s, 4, a);
        set_mtime(&mut s, Timestamp::now());
        self.store(ino, s)
    }

    fn write_inner(&mut self, ino: Ino, off: u64, data: &[u8]) -> FsResult<u32> {
        let mut s = self.file_set(ino)?;
        if data.is_empty() {
            return Ok(0);
        }
        let end = off
            .checked_add(data.len() as u64)
            .ok_or(FsError::Invalid("offset overflow".into()))?;
        let cs = self.b.cs;
        let size = s_len(&s);
        let vdl = s_vdl(&s).min(size);
        let new_size = size.max(end);
        let mut x = Ext {
            first: s_first(&s),
            nofat: s_flags(&s) & F_NOFAT != 0,
        };
        let alloc = new_size.div_ceil(cs);
        self.realloc(&mut x, size.div_ceil(cs), alloc)?;
        if off > vdl {
            self.zero_ext(x, alloc, vdl, off - vdl, true)?;
        }
        self.write_ext(x, alloc, off, data, true)?;
        s_set_alloc(&mut s, x.first, x.nofat, vdl.max(end), new_size);
        let a = s_attr(&s) | A_ARCH;
        wr16(&mut s, 4, a);
        set_mtime(&mut s, Timestamp::now());
        self.store(ino, s)?;
        Ok(data.len() as u32)
    }

    fn dir_empty(&mut self, ino: Ino) -> FsResult<bool> {
        Ok(self.dc(ino)?.ents.is_empty())
    }

    fn remove_entry(&mut self, parent: Ino, e: &XEnt) -> FsResult<()> {
        let set = self
            .by_loc
            .get(&(parent, e.off))
            .and_then(|i| self.nodes.get(i))
            .map(|n| n.set.clone())
            .unwrap_or(e.set.clone());
        let first = s_first(&set);
        if first != 0 {
            let mut x = Ext {
                first,
                nofat: s_flags(&set) & F_NOFAT != 0,
            };
            self.realloc(&mut x, s_len(&set).div_ceil(self.b.cs), 0)?;
        }
        self.delete_set(parent, e.off, &set)?;
        if let Some(&i) = self.by_loc.get(&(parent, e.off)) {
            self.dcache.remove(&i);
        }
        self.forget(parent, e.off);
        Ok(())
    }

    fn create_inner(&mut self, parent: Ino, name: &str, perm: u16, dir: bool) -> FsResult<Ino> {
        let u = valid_name(name)?;
        if self.find(parent, name)?.is_some() {
            return Err(FsError::Exists);
        }
        let now = Timestamp::now();
        let (p, ms, utc) = ts_fields(now);
        let mut file = [0u8; 32];
        let mut attr = if dir { A_DIR } else { A_ARCH };
        if !dir && perm & 0o200 == 0 {
            attr |= A_RO;
        }
        wr16(&mut file, 4, attr);
        wr32(&mut file, 8, p);
        wr32(&mut file, 12, p);
        wr32(&mut file, 16, p);
        file[20] = ms;
        file[21] = ms;
        file[22] = utc;
        file[23] = utc;
        file[24] = utc;
        let mut stream = [0u8; 32];
        stream[1] = F_ALLOC;
        let mut x = Ext {
            first: 0,
            nofat: false,
        };
        if dir {
            self.realloc(&mut x, 0, 1)?;
            if let Err(e) = self.zero_ext(x, 1, 0, self.b.cs, false) {
                let _ = self.realloc(&mut x, 1, 0);
                return Err(e);
            }
        }
        let hash = name_hash(&self.up(&u));
        let mut set = make_set(&file, &stream, &u, hash);
        if dir {
            let cs = self.b.cs;
            s_set_alloc(&mut set, x.first, x.nofat, cs, cs);
            let c = set_checksum(&set);
            wr16(&mut set, 2, c);
        }
        let off = match self.insert_set(parent, &set) {
            Ok(o) => o,
            Err(e) => {
                if dir {
                    let _ = self.realloc(&mut x, 1, 0);
                }
                return Err(e);
            }
        };
        Ok(self.intern(parent, off, &set))
    }

    fn rename_inner(&mut self, fp: Ino, fname: &str, tp: Ino, tname: &str) -> FsResult<()> {
        let tu = valid_name(tname)?;
        self.dir_ext(tp)?;
        let src = self.find(fp, fname)?.ok_or(FsError::NotFound)?;
        let src_ino = self.intern(fp, src.off, &src.set);
        let src_set = self.set_of(src_ino)?;
        let is_dir = s_attr(&src_set) & A_DIR != 0;
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
        if let Some(dst) = self.find(tp, tname)? {
            if !(tp == fp && dst.off == src.off) {
                let dst_dir = s_attr(&dst.set) & A_DIR != 0;
                if dst_dir && !is_dir {
                    return Err(FsError::IsDir);
                }
                if !dst_dir && is_dir {
                    return Err(FsError::NotDir);
                }
                if dst_dir {
                    let dino = self.intern(tp, dst.off, &dst.set);
                    if !self.dir_empty(dino)? {
                        return Err(FsError::NotEmpty);
                    }
                }
                self.remove_entry(tp, &dst)?;
            }
        }
        let hash = name_hash(&self.up(&tu));
        let set = make_set(&src_set[0..32], &src_set[32..64], &tu, hash);
        let new_off = self.insert_set(tp, &set)?;
        self.delete_set(fp, src.off, &src_set)?;
        self.by_loc.remove(&(fp, src.off));
        if let Some(n) = self.nodes.get_mut(&src_ino) {
            n.parent = tp;
            n.off = new_off;
            n.set = set;
        }
        self.by_loc.insert((tp, new_off), src_ino);
        Ok(())
    }

    /// Walk the whole tree and return the cluster-in-use map implied by the metadata.
    fn compute_usage(&mut self, problems: &mut Vec<String>) -> FsResult<Vec<u8>> {
        let mut used = vec![0u8; self.bitmap.len()];
        let mark = |c: u32, used: &mut Vec<u8>| {
            if c < 2 {
                return;
            }
            let i = (c - 2) as usize;
            if let Some(b) = used.get_mut(i / 8) {
                *b |= 1 << (i % 8);
            }
        };
        for &c in self.bm_clusters.iter().chain(self.up_clusters.iter()) {
            mark(c, &mut used);
        }
        let rn = self.root_len()?;
        let root = Ext {
            first: self.b.root,
            nofat: false,
        };
        for c in self.chain_n(self.b.root, rn)? {
            mark(c, &mut used);
        }
        let mut queue: VecDeque<(Ext, u64, String)> = VecDeque::new();
        queue.push_back((root, rn, "/".into()));
        // A directory whose clusters are reached twice (e.g. a subdirectory pointing back at
        // the root) is a loop: walk it once.
        let mut seen_dirs = std::collections::HashSet::new();
        seen_dirs.insert(self.b.root);
        let mut dirs = 0usize;
        while let Some((x, n, path)) = queue.pop_front() {
            dirs += 1;
            if dirs > 1_000_000 {
                return Err(corrupt("directory tree too large or looping"));
            }
            let buf = match self.read_ext(x, n, 0, n * self.b.cs) {
                Ok(b) => b,
                Err(e) => {
                    problems.push(format!("cannot read directory {path}: {e}"));
                    continue;
                }
            };
            for e in parse_dir(&buf) {
                let first = s_first(&e.set);
                let len = s_len(&e.set);
                if first == 0 || len == 0 {
                    continue;
                }
                let alloc = len.div_ceil(self.b.cs);
                let nofat = s_flags(&e.set) & F_NOFAT != 0;
                let clusters: FsResult<Vec<u32>> = if nofat {
                    if self.valid(first) && first as u64 + alloc <= self.b.clusters as u64 + 2 {
                        Ok((first..first + alloc as u32).collect())
                    } else {
                        Err(corrupt("out of range"))
                    }
                } else {
                    self.chain_n(first, alloc)
                };
                let p = format!("{}{}", path, e.name);
                match clusters {
                    Ok(cl) => {
                        for c in cl {
                            mark(c, &mut used);
                        }
                        if s_attr(&e.set) & A_DIR != 0 && len <= MAX_DIR_BYTES {
                            if seen_dirs.insert(first) {
                                queue.push_back((Ext { first, nofat }, alloc, format!("{p}/")));
                            } else {
                                problems.push(format!("directory {p} shares its clusters with another directory (loop)"));
                            }
                        }
                    }
                    Err(err) => {
                        problems.push(format!("cannot follow the allocation of {p}: {err}"))
                    }
                }
            }
        }
        Ok(used)
    }

    fn repair_inner(&mut self) -> FsResult<String> {
        self.flush_meta()?;
        let mut rep: Vec<String> = vec![];
        let mut b = [0u8; 2];
        self.dev.read_at(106, &mut b)?;
        let flags = u16::from_le_bytes(b);
        if flags & VF_DIRTY != 0 {
            rep.push("Cleared the VolumeDirty flag.".into());
        }
        if flags & VF_MEDIA != 0 {
            rep.push("Cleared the MediaFailure flag (check the medium for bad sectors).".into());
        }
        let mut problems = vec![];
        let used = self.compute_usage(&mut problems)?;
        let (mut leaked, mut missing) = (0u32, 0u32);
        for c in 2..self.b.clusters + 2 {
            let i = (c - 2) as usize;
            let want = used[i / 8] >> (i % 8) & 1 != 0;
            let have = self.bm_get(c);
            if want != have {
                if have {
                    leaked += 1;
                } else {
                    missing += 1;
                }
                // Only release leaked clusters when every chain could be followed.
                if want || problems.is_empty() {
                    self.bm_set(c, want);
                }
            }
        }
        if missing > 0 {
            rep.push(format!(
                "Marked {missing} in-use cluster(s) that the allocation bitmap listed as free."
            ));
        }
        if leaked > 0 {
            if problems.is_empty() {
                rep.push(format!(
                    "Released {leaked} leaked cluster(s) not referenced by any file."
                ));
            } else {
                rep.push(format!("Found {leaked} unreferenced cluster(s); left allocated because of the problems below."));
            }
        }
        rep.extend(problems);
        self.flush_meta()?;
        self.set_volume_flags(VF_DIRTY | VF_MEDIA, false)?;
        let mut p = [0u8; 1];
        self.dev.read_at(112, &mut p)?;
        let pct =
            ((self.b.clusters as u64 - self.free as u64) * 100 / self.b.clusters as u64) as u8;
        if p[0] != pct && p[0] != 0xFF {
            rep.push(format!("PercentInUse updated from {}% to {pct}%.", p[0]));
        }
        self.write_percent()?;
        self.dev.flush()?;
        self.dirty_reason = None;
        self.marked = false;
        self.chains.clear();
        self.dcache.clear();
        if rep.is_empty() {
            Ok(format!("exFAT volume is clean: flags clear and the allocation bitmap matches the directory tree ({} free clusters).", self.free))
        } else {
            rep.push(format!(
                "{} free clusters of {}.",
                self.free, self.b.clusters
            ));
            Ok(rep.join("\n"))
        }
    }
}

impl Drop for ExFatFs {
    fn drop(&mut self) {
        if !self.ro {
            let _ = self.sync();
        }
    }
}

impl FileSystem for ExFatFs {
    fn volume_info(&self) -> VolumeInfo {
        VolumeInfo {
            fs_type: FsType::ExFat,
            label: self.label.clone(),
            uuid: format!("{:04X}-{:04X}", self.b.serial >> 16, self.b.serial & 0xFFFF),
            read_only: self.ro,
        }
    }
    fn statfs(&mut self) -> FsResult<FsStat> {
        Ok(FsStat {
            block_size: self.b.cs as u32,
            total_bytes: self.b.clusters as u64 * self.b.cs,
            free_bytes: self.free as u64 * self.b.cs,
            total_inodes: self.b.clusters as u64,
            free_inodes: self.free as u64,
        })
    }
    fn root_ino(&self) -> Ino {
        ROOT
    }
    fn lookup(&mut self, parent: Ino, name: &str) -> FsResult<Ino> {
        let e = self.find(parent, name)?.ok_or(FsError::NotFound)?;
        Ok(self.intern(parent, e.off, &e.set))
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
            let mut s = self.set_of(ino)?;
            let orig = s.clone();
            if let Some(p) = set.perm {
                let a = s_attr(&s);
                if a & A_DIR == 0 {
                    wr16(&mut s, 4, if p & 0o200 == 0 { a | A_RO } else { a & !A_RO });
                }
            }
            if let Some(t) = set.mtime {
                set_mtime(&mut s, t);
            }
            if let Some(t) = set.atime {
                set_atime(&mut s, t);
            }
            if s != orig {
                self.store(ino, s)?;
            }
            Ok(())
        })();
        self.end(r)?;
        self.attr_of(ino)
    }
    fn read(&mut self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>> {
        let s = self.file_set(ino)?;
        let size = s_len(&s);
        if offset >= size || len == 0 {
            return Ok(vec![]);
        }
        let n = (len as u64).min(size - offset);
        let vdl = s_vdl(&s).min(size);
        let x = Ext {
            first: s_first(&s),
            nofat: s_flags(&s) & F_NOFAT != 0,
        };
        let alloc = size.div_ceil(self.b.cs);
        let disk_end = (offset + n).min(vdl);
        let mut out = if disk_end > offset {
            self.read_ext(x, alloc, offset, disk_end - offset)?
        } else {
            vec![]
        };
        out.resize(n as usize, 0);
        Ok(out)
    }
    fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<u32> {
        self.begin()?;
        let r = self.write_inner(ino, offset, data);
        self.end(r)
    }
    fn readdir(&mut self, dir: Ino) -> FsResult<Vec<DirEntry>> {
        let ents: Vec<XEnt> = self.dc(dir)?.ents.values().cloned().collect();
        let mut out = Vec::with_capacity(ents.len());
        for e in ents {
            let ino = self.intern(dir, e.off, &e.set);
            let kind = if s_attr(&e.set) & A_DIR != 0 {
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
        Err(FsError::Unsupported("exFAT has no symbolic links".into()))
    }
    fn readlink(&mut self, ino: Ino) -> FsResult<String> {
        self.attr_of(ino)?;
        Err(FsError::Invalid("not a symbolic link".into()))
    }
    fn unlink(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.begin()?;
        let r = (|| {
            let e = self.find(parent, name)?.ok_or(FsError::NotFound)?;
            if s_attr(&e.set) & A_DIR != 0 {
                return Err(FsError::IsDir);
            }
            self.remove_entry(parent, &e)
        })();
        self.end(r)
    }
    fn rmdir(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.begin()?;
        let r = (|| {
            let e = self.find(parent, name)?.ok_or(FsError::NotFound)?;
            if s_attr(&e.set) & A_DIR == 0 {
                return Err(FsError::NotDir);
            }
            let ino = self.intern(parent, e.off, &e.set);
            if !self.dir_empty(ino)? {
                return Err(FsError::NotEmpty);
            }
            self.remove_entry(parent, &e)
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
        self.flush_meta()?;
        self.write_percent()?;
        if self.marked {
            self.set_volume_flags(VF_DIRTY, false)?;
            self.marked = false;
        }
        self.dev.flush()?;
        Ok(())
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

pub fn format(dev: Arc<dyn BlockDev>, label: &str) -> FsResult<()> {
    if dev.read_only() {
        return Err(FsError::ReadOnly);
    }
    let size = dev.size();
    let vol = size / 512;
    if vol < 2048 {
        return Err(FsError::Invalid(
            "volume too small for exFAT (minimum 1 MiB)".into(),
        ));
    }
    let cs: u64 = if size <= 256 << 20 {
        4096
    } else if size <= 32 << 30 {
        32768
    } else {
        131072
    };
    let spc = cs / 512;
    let fat_offset = if vol >= 65536 {
        2048
    } else {
        24u64.div_ceil(spc) * spc
    };
    let mut count = (vol - fat_offset) / spc;
    let (mut fat_len, mut heap);
    loop {
        fat_len = ((count + 2) * 4).div_ceil(512);
        heap = (fat_offset + fat_len).div_ceil(spc) * spc;
        if heap >= vol {
            return Err(FsError::Invalid("volume too small for exFAT".into()));
        }
        let nc = (vol - heap) / spc;
        if nc >= count {
            break;
        }
        count = nc;
    }
    let count = count.min(0xFFFF_FFF5);
    let bm_len = count.div_ceil(8);
    let bm_cl = bm_len.div_ceil(cs);
    let up = upcase_bytes();
    let up_cl = (up.len() as u64).div_ceil(cs);
    if count < bm_cl + up_cl + 2 {
        return Err(FsError::Invalid("volume too small for exFAT".into()));
    }
    let bm_first = 2u64;
    let up_first = bm_first + bm_cl;
    let root = up_first + up_cl;
    let used = bm_cl + up_cl + 1;

    let now = Timestamp::now();
    let serial = (now.secs as u32).rotate_left(13) ^ now.nsecs ^ 0x4C55_4D45;
    let mut bs = vec![0u8; 512];
    bs[0..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
    bs[3..11].copy_from_slice(b"EXFAT   ");
    wr64(&mut bs, 72, vol);
    wr32(&mut bs, 80, fat_offset as u32);
    wr32(&mut bs, 84, fat_len as u32);
    wr32(&mut bs, 88, heap as u32);
    wr32(&mut bs, 92, count as u32);
    wr32(&mut bs, 96, root as u32);
    wr32(&mut bs, 100, serial);
    wr16(&mut bs, 104, 0x0100);
    bs[108] = 9;
    bs[109] = spc.trailing_zeros() as u8;
    bs[110] = 1;
    bs[111] = 0x80;
    bs[112] = (used * 100 / count) as u8;
    for b in &mut bs[120..510] {
        *b = 0xF4;
    }
    bs[510] = 0x55;
    bs[511] = 0xAA;
    let mut region = vec![0u8; 512 * 12];
    region[..512].copy_from_slice(&bs);
    for s in 1..=8 {
        region[s * 512 + 510] = 0x55;
        region[s * 512 + 511] = 0xAA;
    }
    let chk = boot_checksum(&region[..512 * 11]);
    for k in 0..128 {
        wr32(&mut region, 11 * 512 + k * 4, chk);
    }

    zero_dev(dev.as_ref(), 0, fat_offset * 512)?;
    zero_dev(dev.as_ref(), fat_offset * 512, fat_len * 512)?;
    // FAT: media + reserved entries, then chains for bitmap, up-case table and root.
    let mut fat = vec![0u8; ((root + 1) * 4) as usize];
    wr32(&mut fat, 0, 0xFFFF_FFF8);
    wr32(&mut fat, 4, 0xFFFF_FFFF);
    for (first, n) in [(bm_first, bm_cl), (up_first, up_cl), (root, 1)] {
        for c in first..first + n {
            let v = if c + 1 < first + n {
                (c + 1) as u32
            } else {
                EOC
            };
            wr32(&mut fat, (c * 4) as usize, v);
        }
    }
    dev.write_at(fat_offset * 512, &fat)?;
    let cpos = |c: u64| heap * 512 + (c - 2) * cs;
    zero_dev(dev.as_ref(), cpos(bm_first), (bm_cl + up_cl + 1) * cs)?;
    let mut bm = vec![0u8; used.div_ceil(8) as usize];
    for i in 0..used as usize {
        bm[i / 8] |= 1 << (i % 8);
    }
    dev.write_at(cpos(bm_first), &bm)?;
    dev.write_at(cpos(up_first), &up)?;
    let mut rootd = vec![0u8; 96];
    rootd[0] = T_LABEL;
    let lab: Vec<u16> = label.encode_utf16().take(11).collect();
    rootd[1] = lab.len() as u8;
    for (k, &u) in lab.iter().enumerate() {
        wr16(&mut rootd, 2 + 2 * k, u);
    }
    rootd[32] = T_BITMAP;
    wr32(&mut rootd, 32 + 20, bm_first as u32);
    wr64(&mut rootd, 32 + 24, bm_len);
    rootd[64] = T_UPCASE;
    wr32(&mut rootd, 64 + 4, table_checksum(&up));
    wr32(&mut rootd, 64 + 20, up_first as u32);
    wr64(&mut rootd, 64 + 24, up.len() as u64);
    dev.write_at(cpos(root), &rootd)?;
    dev.write_at(0, &region)?;
    dev.write_at(12 * 512, &region)?;
    dev.flush()?;
    Ok(())
}

/// The standard compressed exFAT up-case table (exFAT spec §7.2.5.1, checksum 0xE619D30D).
#[rustfmt::skip]
const UPCASE_TABLE: [u16; 2918] = [
    0x0000, 0x0001, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0007, 0x0008, 0x0009, 0x000A, 0x000B, 0x000C, 0x000D, 0x000E, 0x000F,
    0x0010, 0x0011, 0x0012, 0x0013, 0x0014, 0x0015, 0x0016, 0x0017, 0x0018, 0x0019, 0x001A, 0x001B, 0x001C, 0x001D, 0x001E, 0x001F,
    0x0020, 0x0021, 0x0022, 0x0023, 0x0024, 0x0025, 0x0026, 0x0027, 0x0028, 0x0029, 0x002A, 0x002B, 0x002C, 0x002D, 0x002E, 0x002F,
    0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036, 0x0037, 0x0038, 0x0039, 0x003A, 0x003B, 0x003C, 0x003D, 0x003E, 0x003F,
    0x0040, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047, 0x0048, 0x0049, 0x004A, 0x004B, 0x004C, 0x004D, 0x004E, 0x004F,
    0x0050, 0x0051, 0x0052, 0x0053, 0x0054, 0x0055, 0x0056, 0x0057, 0x0058, 0x0059, 0x005A, 0x005B, 0x005C, 0x005D, 0x005E, 0x005F,
    0x0060, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047, 0x0048, 0x0049, 0x004A, 0x004B, 0x004C, 0x004D, 0x004E, 0x004F,
    0x0050, 0x0051, 0x0052, 0x0053, 0x0054, 0x0055, 0x0056, 0x0057, 0x0058, 0x0059, 0x005A, 0x007B, 0x007C, 0x007D, 0x007E, 0x007F,
    0x0080, 0x0081, 0x0082, 0x0083, 0x0084, 0x0085, 0x0086, 0x0087, 0x0088, 0x0089, 0x008A, 0x008B, 0x008C, 0x008D, 0x008E, 0x008F,
    0x0090, 0x0091, 0x0092, 0x0093, 0x0094, 0x0095, 0x0096, 0x0097, 0x0098, 0x0099, 0x009A, 0x009B, 0x009C, 0x009D, 0x009E, 0x009F,
    0x00A0, 0x00A1, 0x00A2, 0x00A3, 0x00A4, 0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00A9, 0x00AA, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00AF,
    0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7, 0x00B8, 0x00B9, 0x00BA, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00BF,
    0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00C4, 0x00C5, 0x00C6, 0x00C7, 0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC, 0x00CD, 0x00CE, 0x00CF,
    0x00D0, 0x00D1, 0x00D2, 0x00D3, 0x00D4, 0x00D5, 0x00D6, 0x00D7, 0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x00DD, 0x00DE, 0x00DF,
    0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00C4, 0x00C5, 0x00C6, 0x00C7, 0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC, 0x00CD, 0x00CE, 0x00CF,
    0x00D0, 0x00D1, 0x00D2, 0x00D3, 0x00D4, 0x00D5, 0x00D6, 0x00F7, 0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x00DD, 0x00DE, 0x0178,
    0x0100, 0x0100, 0x0102, 0x0102, 0x0104, 0x0104, 0x0106, 0x0106, 0x0108, 0x0108, 0x010A, 0x010A, 0x010C, 0x010C, 0x010E, 0x010E,
    0x0110, 0x0110, 0x0112, 0x0112, 0x0114, 0x0114, 0x0116, 0x0116, 0x0118, 0x0118, 0x011A, 0x011A, 0x011C, 0x011C, 0x011E, 0x011E,
    0x0120, 0x0120, 0x0122, 0x0122, 0x0124, 0x0124, 0x0126, 0x0126, 0x0128, 0x0128, 0x012A, 0x012A, 0x012C, 0x012C, 0x012E, 0x012E,
    0x0130, 0x0131, 0x0132, 0x0132, 0x0134, 0x0134, 0x0136, 0x0136, 0x0138, 0x0139, 0x0139, 0x013B, 0x013B, 0x013D, 0x013D, 0x013F,
    0x013F, 0x0141, 0x0141, 0x0143, 0x0143, 0x0145, 0x0145, 0x0147, 0x0147, 0x0149, 0x014A, 0x014A, 0x014C, 0x014C, 0x014E, 0x014E,
    0x0150, 0x0150, 0x0152, 0x0152, 0x0154, 0x0154, 0x0156, 0x0156, 0x0158, 0x0158, 0x015A, 0x015A, 0x015C, 0x015C, 0x015E, 0x015E,
    0x0160, 0x0160, 0x0162, 0x0162, 0x0164, 0x0164, 0x0166, 0x0166, 0x0168, 0x0168, 0x016A, 0x016A, 0x016C, 0x016C, 0x016E, 0x016E,
    0x0170, 0x0170, 0x0172, 0x0172, 0x0174, 0x0174, 0x0176, 0x0176, 0x0178, 0x0179, 0x0179, 0x017B, 0x017B, 0x017D, 0x017D, 0x017F,
    0x0243, 0x0181, 0x0182, 0x0182, 0x0184, 0x0184, 0x0186, 0x0187, 0x0187, 0x0189, 0x018A, 0x018B, 0x018B, 0x018D, 0x018E, 0x018F,
    0x0190, 0x0191, 0x0191, 0x0193, 0x0194, 0x01F6, 0x0196, 0x0197, 0x0198, 0x0198, 0x023D, 0x019B, 0x019C, 0x019D, 0x0220, 0x019F,
    0x01A0, 0x01A0, 0x01A2, 0x01A2, 0x01A4, 0x01A4, 0x01A6, 0x01A7, 0x01A7, 0x01A9, 0x01AA, 0x01AB, 0x01AC, 0x01AC, 0x01AE, 0x01AF,
    0x01AF, 0x01B1, 0x01B2, 0x01B3, 0x01B3, 0x01B5, 0x01B5, 0x01B7, 0x01B8, 0x01B8, 0x01BA, 0x01BB, 0x01BC, 0x01BC, 0x01BE, 0x01F7,
    0x01C0, 0x01C1, 0x01C2, 0x01C3, 0x01C4, 0x01C5, 0x01C4, 0x01C7, 0x01C8, 0x01C7, 0x01CA, 0x01CB, 0x01CA, 0x01CD, 0x01CD, 0x01CF,
    0x01CF, 0x01D1, 0x01D1, 0x01D3, 0x01D3, 0x01D5, 0x01D5, 0x01D7, 0x01D7, 0x01D9, 0x01D9, 0x01DB, 0x01DB, 0x018E, 0x01DE, 0x01DE,
    0x01E0, 0x01E0, 0x01E2, 0x01E2, 0x01E4, 0x01E4, 0x01E6, 0x01E6, 0x01E8, 0x01E8, 0x01EA, 0x01EA, 0x01EC, 0x01EC, 0x01EE, 0x01EE,
    0x01F0, 0x01F1, 0x01F2, 0x01F1, 0x01F4, 0x01F4, 0x01F6, 0x01F7, 0x01F8, 0x01F8, 0x01FA, 0x01FA, 0x01FC, 0x01FC, 0x01FE, 0x01FE,
    0x0200, 0x0200, 0x0202, 0x0202, 0x0204, 0x0204, 0x0206, 0x0206, 0x0208, 0x0208, 0x020A, 0x020A, 0x020C, 0x020C, 0x020E, 0x020E,
    0x0210, 0x0210, 0x0212, 0x0212, 0x0214, 0x0214, 0x0216, 0x0216, 0x0218, 0x0218, 0x021A, 0x021A, 0x021C, 0x021C, 0x021E, 0x021E,
    0x0220, 0x0221, 0x0222, 0x0222, 0x0224, 0x0224, 0x0226, 0x0226, 0x0228, 0x0228, 0x022A, 0x022A, 0x022C, 0x022C, 0x022E, 0x022E,
    0x0230, 0x0230, 0x0232, 0x0232, 0x0234, 0x0235, 0x0236, 0x0237, 0x0238, 0x0239, 0x2C65, 0x023B, 0x023B, 0x023D, 0x2C66, 0x023F,
    0x0240, 0x0241, 0x0241, 0x0243, 0x0244, 0x0245, 0x0246, 0x0246, 0x0248, 0x0248, 0x024A, 0x024A, 0x024C, 0x024C, 0x024E, 0x024E,
    0x0250, 0x0251, 0x0252, 0x0181, 0x0186, 0x0255, 0x0189, 0x018A, 0x0258, 0x018F, 0x025A, 0x0190, 0x025C, 0x025D, 0x025E, 0x025F,
    0x0193, 0x0261, 0x0262, 0x0194, 0x0264, 0x0265, 0x0266, 0x0267, 0x0197, 0x0196, 0x026A, 0x2C62, 0x026C, 0x026D, 0x026E, 0x019C,
    0x0270, 0x0271, 0x019D, 0x0273, 0x0274, 0x019F, 0x0276, 0x0277, 0x0278, 0x0279, 0x027A, 0x027B, 0x027C, 0x2C64, 0x027E, 0x027F,
    0x01A6, 0x0281, 0x0282, 0x01A9, 0x0284, 0x0285, 0x0286, 0x0287, 0x01AE, 0x0244, 0x01B1, 0x01B2, 0x0245, 0x028D, 0x028E, 0x028F,
    0x0290, 0x0291, 0x01B7, 0x0293, 0x0294, 0x0295, 0x0296, 0x0297, 0x0298, 0x0299, 0x029A, 0x029B, 0x029C, 0x029D, 0x029E, 0x029F,
    0x02A0, 0x02A1, 0x02A2, 0x02A3, 0x02A4, 0x02A5, 0x02A6, 0x02A7, 0x02A8, 0x02A9, 0x02AA, 0x02AB, 0x02AC, 0x02AD, 0x02AE, 0x02AF,
    0x02B0, 0x02B1, 0x02B2, 0x02B3, 0x02B4, 0x02B5, 0x02B6, 0x02B7, 0x02B8, 0x02B9, 0x02BA, 0x02BB, 0x02BC, 0x02BD, 0x02BE, 0x02BF,
    0x02C0, 0x02C1, 0x02C2, 0x02C3, 0x02C4, 0x02C5, 0x02C6, 0x02C7, 0x02C8, 0x02C9, 0x02CA, 0x02CB, 0x02CC, 0x02CD, 0x02CE, 0x02CF,
    0x02D0, 0x02D1, 0x02D2, 0x02D3, 0x02D4, 0x02D5, 0x02D6, 0x02D7, 0x02D8, 0x02D9, 0x02DA, 0x02DB, 0x02DC, 0x02DD, 0x02DE, 0x02DF,
    0x02E0, 0x02E1, 0x02E2, 0x02E3, 0x02E4, 0x02E5, 0x02E6, 0x02E7, 0x02E8, 0x02E9, 0x02EA, 0x02EB, 0x02EC, 0x02ED, 0x02EE, 0x02EF,
    0x02F0, 0x02F1, 0x02F2, 0x02F3, 0x02F4, 0x02F5, 0x02F6, 0x02F7, 0x02F8, 0x02F9, 0x02FA, 0x02FB, 0x02FC, 0x02FD, 0x02FE, 0x02FF,
    0x0300, 0x0301, 0x0302, 0x0303, 0x0304, 0x0305, 0x0306, 0x0307, 0x0308, 0x0309, 0x030A, 0x030B, 0x030C, 0x030D, 0x030E, 0x030F,
    0x0310, 0x0311, 0x0312, 0x0313, 0x0314, 0x0315, 0x0316, 0x0317, 0x0318, 0x0319, 0x031A, 0x031B, 0x031C, 0x031D, 0x031E, 0x031F,
    0x0320, 0x0321, 0x0322, 0x0323, 0x0324, 0x0325, 0x0326, 0x0327, 0x0328, 0x0329, 0x032A, 0x032B, 0x032C, 0x032D, 0x032E, 0x032F,
    0x0330, 0x0331, 0x0332, 0x0333, 0x0334, 0x0335, 0x0336, 0x0337, 0x0338, 0x0339, 0x033A, 0x033B, 0x033C, 0x033D, 0x033E, 0x033F,
    0x0340, 0x0341, 0x0342, 0x0343, 0x0344, 0x0345, 0x0346, 0x0347, 0x0348, 0x0349, 0x034A, 0x034B, 0x034C, 0x034D, 0x034E, 0x034F,
    0x0350, 0x0351, 0x0352, 0x0353, 0x0354, 0x0355, 0x0356, 0x0357, 0x0358, 0x0359, 0x035A, 0x035B, 0x035C, 0x035D, 0x035E, 0x035F,
    0x0360, 0x0361, 0x0362, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369, 0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F,
    0x0370, 0x0371, 0x0372, 0x0373, 0x0374, 0x0375, 0x0376, 0x0377, 0x0378, 0x0379, 0x037A, 0x03FD, 0x03FE, 0x03FF, 0x037E, 0x037F,
    0x0380, 0x0381, 0x0382, 0x0383, 0x0384, 0x0385, 0x0386, 0x0387, 0x0388, 0x0389, 0x038A, 0x038B, 0x038C, 0x038D, 0x038E, 0x038F,
    0x0390, 0x0391, 0x0392, 0x0393, 0x0394, 0x0395, 0x0396, 0x0397, 0x0398, 0x0399, 0x039A, 0x039B, 0x039C, 0x039D, 0x039E, 0x039F,
    0x03A0, 0x03A1, 0x03A2, 0x03A3, 0x03A4, 0x03A5, 0x03A6, 0x03A7, 0x03A8, 0x03A9, 0x03AA, 0x03AB, 0x0386, 0x0388, 0x0389, 0x038A,
    0x03B0, 0x0391, 0x0392, 0x0393, 0x0394, 0x0395, 0x0396, 0x0397, 0x0398, 0x0399, 0x039A, 0x039B, 0x039C, 0x039D, 0x039E, 0x039F,
    0x03A0, 0x03A1, 0x03A3, 0x03A3, 0x03A4, 0x03A5, 0x03A6, 0x03A7, 0x03A8, 0x03A9, 0x03AA, 0x03AB, 0x038C, 0x038E, 0x038F, 0x03CF,
    0x03D0, 0x03D1, 0x03D2, 0x03D3, 0x03D4, 0x03D5, 0x03D6, 0x03D7, 0x03D8, 0x03D8, 0x03DA, 0x03DA, 0x03DC, 0x03DC, 0x03DE, 0x03DE,
    0x03E0, 0x03E0, 0x03E2, 0x03E2, 0x03E4, 0x03E4, 0x03E6, 0x03E6, 0x03E8, 0x03E8, 0x03EA, 0x03EA, 0x03EC, 0x03EC, 0x03EE, 0x03EE,
    0x03F0, 0x03F1, 0x03F9, 0x03F3, 0x03F4, 0x03F5, 0x03F6, 0x03F7, 0x03F7, 0x03F9, 0x03FA, 0x03FA, 0x03FC, 0x03FD, 0x03FE, 0x03FF,
    0x0400, 0x0401, 0x0402, 0x0403, 0x0404, 0x0405, 0x0406, 0x0407, 0x0408, 0x0409, 0x040A, 0x040B, 0x040C, 0x040D, 0x040E, 0x040F,
    0x0410, 0x0411, 0x0412, 0x0413, 0x0414, 0x0415, 0x0416, 0x0417, 0x0418, 0x0419, 0x041A, 0x041B, 0x041C, 0x041D, 0x041E, 0x041F,
    0x0420, 0x0421, 0x0422, 0x0423, 0x0424, 0x0425, 0x0426, 0x0427, 0x0428, 0x0429, 0x042A, 0x042B, 0x042C, 0x042D, 0x042E, 0x042F,
    0x0410, 0x0411, 0x0412, 0x0413, 0x0414, 0x0415, 0x0416, 0x0417, 0x0418, 0x0419, 0x041A, 0x041B, 0x041C, 0x041D, 0x041E, 0x041F,
    0x0420, 0x0421, 0x0422, 0x0423, 0x0424, 0x0425, 0x0426, 0x0427, 0x0428, 0x0429, 0x042A, 0x042B, 0x042C, 0x042D, 0x042E, 0x042F,
    0x0400, 0x0401, 0x0402, 0x0403, 0x0404, 0x0405, 0x0406, 0x0407, 0x0408, 0x0409, 0x040A, 0x040B, 0x040C, 0x040D, 0x040E, 0x040F,
    0x0460, 0x0460, 0x0462, 0x0462, 0x0464, 0x0464, 0x0466, 0x0466, 0x0468, 0x0468, 0x046A, 0x046A, 0x046C, 0x046C, 0x046E, 0x046E,
    0x0470, 0x0470, 0x0472, 0x0472, 0x0474, 0x0474, 0x0476, 0x0476, 0x0478, 0x0478, 0x047A, 0x047A, 0x047C, 0x047C, 0x047E, 0x047E,
    0x0480, 0x0480, 0x0482, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0488, 0x0489, 0x048A, 0x048A, 0x048C, 0x048C, 0x048E, 0x048E,
    0x0490, 0x0490, 0x0492, 0x0492, 0x0494, 0x0494, 0x0496, 0x0496, 0x0498, 0x0498, 0x049A, 0x049A, 0x049C, 0x049C, 0x049E, 0x049E,
    0x04A0, 0x04A0, 0x04A2, 0x04A2, 0x04A4, 0x04A4, 0x04A6, 0x04A6, 0x04A8, 0x04A8, 0x04AA, 0x04AA, 0x04AC, 0x04AC, 0x04AE, 0x04AE,
    0x04B0, 0x04B0, 0x04B2, 0x04B2, 0x04B4, 0x04B4, 0x04B6, 0x04B6, 0x04B8, 0x04B8, 0x04BA, 0x04BA, 0x04BC, 0x04BC, 0x04BE, 0x04BE,
    0x04C0, 0x04C1, 0x04C1, 0x04C3, 0x04C3, 0x04C5, 0x04C5, 0x04C7, 0x04C7, 0x04C9, 0x04C9, 0x04CB, 0x04CB, 0x04CD, 0x04CD, 0x04C0,
    0x04D0, 0x04D0, 0x04D2, 0x04D2, 0x04D4, 0x04D4, 0x04D6, 0x04D6, 0x04D8, 0x04D8, 0x04DA, 0x04DA, 0x04DC, 0x04DC, 0x04DE, 0x04DE,
    0x04E0, 0x04E0, 0x04E2, 0x04E2, 0x04E4, 0x04E4, 0x04E6, 0x04E6, 0x04E8, 0x04E8, 0x04EA, 0x04EA, 0x04EC, 0x04EC, 0x04EE, 0x04EE,
    0x04F0, 0x04F0, 0x04F2, 0x04F2, 0x04F4, 0x04F4, 0x04F6, 0x04F6, 0x04F8, 0x04F8, 0x04FA, 0x04FA, 0x04FC, 0x04FC, 0x04FE, 0x04FE,
    0x0500, 0x0500, 0x0502, 0x0502, 0x0504, 0x0504, 0x0506, 0x0506, 0x0508, 0x0508, 0x050A, 0x050A, 0x050C, 0x050C, 0x050E, 0x050E,
    0x0510, 0x0510, 0x0512, 0x0512, 0x0514, 0x0515, 0x0516, 0x0517, 0x0518, 0x0519, 0x051A, 0x051B, 0x051C, 0x051D, 0x051E, 0x051F,
    0x0520, 0x0521, 0x0522, 0x0523, 0x0524, 0x0525, 0x0526, 0x0527, 0x0528, 0x0529, 0x052A, 0x052B, 0x052C, 0x052D, 0x052E, 0x052F,
    0x0530, 0x0531, 0x0532, 0x0533, 0x0534, 0x0535, 0x0536, 0x0537, 0x0538, 0x0539, 0x053A, 0x053B, 0x053C, 0x053D, 0x053E, 0x053F,
    0x0540, 0x0541, 0x0542, 0x0543, 0x0544, 0x0545, 0x0546, 0x0547, 0x0548, 0x0549, 0x054A, 0x054B, 0x054C, 0x054D, 0x054E, 0x054F,
    0x0550, 0x0551, 0x0552, 0x0553, 0x0554, 0x0555, 0x0556, 0x0557, 0x0558, 0x0559, 0x055A, 0x055B, 0x055C, 0x055D, 0x055E, 0x055F,
    0x0560, 0x0531, 0x0532, 0x0533, 0x0534, 0x0535, 0x0536, 0x0537, 0x0538, 0x0539, 0x053A, 0x053B, 0x053C, 0x053D, 0x053E, 0x053F,
    0x0540, 0x0541, 0x0542, 0x0543, 0x0544, 0x0545, 0x0546, 0x0547, 0x0548, 0x0549, 0x054A, 0x054B, 0x054C, 0x054D, 0x054E, 0x054F,
    0x0550, 0x0551, 0x0552, 0x0553, 0x0554, 0x0555, 0x0556, 0xFFFF, 0x17F6, 0x2C63, 0x1D7E, 0x1D7F, 0x1D80, 0x1D81, 0x1D82, 0x1D83,
    0x1D84, 0x1D85, 0x1D86, 0x1D87, 0x1D88, 0x1D89, 0x1D8A, 0x1D8B, 0x1D8C, 0x1D8D, 0x1D8E, 0x1D8F, 0x1D90, 0x1D91, 0x1D92, 0x1D93,
    0x1D94, 0x1D95, 0x1D96, 0x1D97, 0x1D98, 0x1D99, 0x1D9A, 0x1D9B, 0x1D9C, 0x1D9D, 0x1D9E, 0x1D9F, 0x1DA0, 0x1DA1, 0x1DA2, 0x1DA3,
    0x1DA4, 0x1DA5, 0x1DA6, 0x1DA7, 0x1DA8, 0x1DA9, 0x1DAA, 0x1DAB, 0x1DAC, 0x1DAD, 0x1DAE, 0x1DAF, 0x1DB0, 0x1DB1, 0x1DB2, 0x1DB3,
    0x1DB4, 0x1DB5, 0x1DB6, 0x1DB7, 0x1DB8, 0x1DB9, 0x1DBA, 0x1DBB, 0x1DBC, 0x1DBD, 0x1DBE, 0x1DBF, 0x1DC0, 0x1DC1, 0x1DC2, 0x1DC3,
    0x1DC4, 0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCA, 0x1DCB, 0x1DCC, 0x1DCD, 0x1DCE, 0x1DCF, 0x1DD0, 0x1DD1, 0x1DD2, 0x1DD3,
    0x1DD4, 0x1DD5, 0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1, 0x1DE2, 0x1DE3,
    0x1DE4, 0x1DE5, 0x1DE6, 0x1DE7, 0x1DE8, 0x1DE9, 0x1DEA, 0x1DEB, 0x1DEC, 0x1DED, 0x1DEE, 0x1DEF, 0x1DF0, 0x1DF1, 0x1DF2, 0x1DF3,
    0x1DF4, 0x1DF5, 0x1DF6, 0x1DF7, 0x1DF8, 0x1DF9, 0x1DFA, 0x1DFB, 0x1DFC, 0x1DFD, 0x1DFE, 0x1DFF, 0x1E00, 0x1E00, 0x1E02, 0x1E02,
    0x1E04, 0x1E04, 0x1E06, 0x1E06, 0x1E08, 0x1E08, 0x1E0A, 0x1E0A, 0x1E0C, 0x1E0C, 0x1E0E, 0x1E0E, 0x1E10, 0x1E10, 0x1E12, 0x1E12,
    0x1E14, 0x1E14, 0x1E16, 0x1E16, 0x1E18, 0x1E18, 0x1E1A, 0x1E1A, 0x1E1C, 0x1E1C, 0x1E1E, 0x1E1E, 0x1E20, 0x1E20, 0x1E22, 0x1E22,
    0x1E24, 0x1E24, 0x1E26, 0x1E26, 0x1E28, 0x1E28, 0x1E2A, 0x1E2A, 0x1E2C, 0x1E2C, 0x1E2E, 0x1E2E, 0x1E30, 0x1E30, 0x1E32, 0x1E32,
    0x1E34, 0x1E34, 0x1E36, 0x1E36, 0x1E38, 0x1E38, 0x1E3A, 0x1E3A, 0x1E3C, 0x1E3C, 0x1E3E, 0x1E3E, 0x1E40, 0x1E40, 0x1E42, 0x1E42,
    0x1E44, 0x1E44, 0x1E46, 0x1E46, 0x1E48, 0x1E48, 0x1E4A, 0x1E4A, 0x1E4C, 0x1E4C, 0x1E4E, 0x1E4E, 0x1E50, 0x1E50, 0x1E52, 0x1E52,
    0x1E54, 0x1E54, 0x1E56, 0x1E56, 0x1E58, 0x1E58, 0x1E5A, 0x1E5A, 0x1E5C, 0x1E5C, 0x1E5E, 0x1E5E, 0x1E60, 0x1E60, 0x1E62, 0x1E62,
    0x1E64, 0x1E64, 0x1E66, 0x1E66, 0x1E68, 0x1E68, 0x1E6A, 0x1E6A, 0x1E6C, 0x1E6C, 0x1E6E, 0x1E6E, 0x1E70, 0x1E70, 0x1E72, 0x1E72,
    0x1E74, 0x1E74, 0x1E76, 0x1E76, 0x1E78, 0x1E78, 0x1E7A, 0x1E7A, 0x1E7C, 0x1E7C, 0x1E7E, 0x1E7E, 0x1E80, 0x1E80, 0x1E82, 0x1E82,
    0x1E84, 0x1E84, 0x1E86, 0x1E86, 0x1E88, 0x1E88, 0x1E8A, 0x1E8A, 0x1E8C, 0x1E8C, 0x1E8E, 0x1E8E, 0x1E90, 0x1E90, 0x1E92, 0x1E92,
    0x1E94, 0x1E94, 0x1E96, 0x1E97, 0x1E98, 0x1E99, 0x1E9A, 0x1E9B, 0x1E9C, 0x1E9D, 0x1E9E, 0x1E9F, 0x1EA0, 0x1EA0, 0x1EA2, 0x1EA2,
    0x1EA4, 0x1EA4, 0x1EA6, 0x1EA6, 0x1EA8, 0x1EA8, 0x1EAA, 0x1EAA, 0x1EAC, 0x1EAC, 0x1EAE, 0x1EAE, 0x1EB0, 0x1EB0, 0x1EB2, 0x1EB2,
    0x1EB4, 0x1EB4, 0x1EB6, 0x1EB6, 0x1EB8, 0x1EB8, 0x1EBA, 0x1EBA, 0x1EBC, 0x1EBC, 0x1EBE, 0x1EBE, 0x1EC0, 0x1EC0, 0x1EC2, 0x1EC2,
    0x1EC4, 0x1EC4, 0x1EC6, 0x1EC6, 0x1EC8, 0x1EC8, 0x1ECA, 0x1ECA, 0x1ECC, 0x1ECC, 0x1ECE, 0x1ECE, 0x1ED0, 0x1ED0, 0x1ED2, 0x1ED2,
    0x1ED4, 0x1ED4, 0x1ED6, 0x1ED6, 0x1ED8, 0x1ED8, 0x1EDA, 0x1EDA, 0x1EDC, 0x1EDC, 0x1EDE, 0x1EDE, 0x1EE0, 0x1EE0, 0x1EE2, 0x1EE2,
    0x1EE4, 0x1EE4, 0x1EE6, 0x1EE6, 0x1EE8, 0x1EE8, 0x1EEA, 0x1EEA, 0x1EEC, 0x1EEC, 0x1EEE, 0x1EEE, 0x1EF0, 0x1EF0, 0x1EF2, 0x1EF2,
    0x1EF4, 0x1EF4, 0x1EF6, 0x1EF6, 0x1EF8, 0x1EF8, 0x1EFA, 0x1EFB, 0x1EFC, 0x1EFD, 0x1EFE, 0x1EFF, 0x1F08, 0x1F09, 0x1F0A, 0x1F0B,
    0x1F0C, 0x1F0D, 0x1F0E, 0x1F0F, 0x1F08, 0x1F09, 0x1F0A, 0x1F0B, 0x1F0C, 0x1F0D, 0x1F0E, 0x1F0F, 0x1F18, 0x1F19, 0x1F1A, 0x1F1B,
    0x1F1C, 0x1F1D, 0x1F16, 0x1F17, 0x1F18, 0x1F19, 0x1F1A, 0x1F1B, 0x1F1C, 0x1F1D, 0x1F1E, 0x1F1F, 0x1F28, 0x1F29, 0x1F2A, 0x1F2B,
    0x1F2C, 0x1F2D, 0x1F2E, 0x1F2F, 0x1F28, 0x1F29, 0x1F2A, 0x1F2B, 0x1F2C, 0x1F2D, 0x1F2E, 0x1F2F, 0x1F38, 0x1F39, 0x1F3A, 0x1F3B,
    0x1F3C, 0x1F3D, 0x1F3E, 0x1F3F, 0x1F38, 0x1F39, 0x1F3A, 0x1F3B, 0x1F3C, 0x1F3D, 0x1F3E, 0x1F3F, 0x1F48, 0x1F49, 0x1F4A, 0x1F4B,
    0x1F4C, 0x1F4D, 0x1F46, 0x1F47, 0x1F48, 0x1F49, 0x1F4A, 0x1F4B, 0x1F4C, 0x1F4D, 0x1F4E, 0x1F4F, 0x1F50, 0x1F59, 0x1F52, 0x1F5B,
    0x1F54, 0x1F5D, 0x1F56, 0x1F5F, 0x1F58, 0x1F59, 0x1F5A, 0x1F5B, 0x1F5C, 0x1F5D, 0x1F5E, 0x1F5F, 0x1F68, 0x1F69, 0x1F6A, 0x1F6B,
    0x1F6C, 0x1F6D, 0x1F6E, 0x1F6F, 0x1F68, 0x1F69, 0x1F6A, 0x1F6B, 0x1F6C, 0x1F6D, 0x1F6E, 0x1F6F, 0x1FBA, 0x1FBB, 0x1FC8, 0x1FC9,
    0x1FCA, 0x1FCB, 0x1FDA, 0x1FDB, 0x1FF8, 0x1FF9, 0x1FEA, 0x1FEB, 0x1FFA, 0x1FFB, 0x1F7E, 0x1F7F, 0x1F88, 0x1F89, 0x1F8A, 0x1F8B,
    0x1F8C, 0x1F8D, 0x1F8E, 0x1F8F, 0x1F88, 0x1F89, 0x1F8A, 0x1F8B, 0x1F8C, 0x1F8D, 0x1F8E, 0x1F8F, 0x1F98, 0x1F99, 0x1F9A, 0x1F9B,
    0x1F9C, 0x1F9D, 0x1F9E, 0x1F9F, 0x1F98, 0x1F99, 0x1F9A, 0x1F9B, 0x1F9C, 0x1F9D, 0x1F9E, 0x1F9F, 0x1FA8, 0x1FA9, 0x1FAA, 0x1FAB,
    0x1FAC, 0x1FAD, 0x1FAE, 0x1FAF, 0x1FA8, 0x1FA9, 0x1FAA, 0x1FAB, 0x1FAC, 0x1FAD, 0x1FAE, 0x1FAF, 0x1FB8, 0x1FB9, 0x1FB2, 0x1FBC,
    0x1FB4, 0x1FB5, 0x1FB6, 0x1FB7, 0x1FB8, 0x1FB9, 0x1FBA, 0x1FBB, 0x1FBC, 0x1FBD, 0x1FBE, 0x1FBF, 0x1FC0, 0x1FC1, 0x1FC2, 0x1FC3,
    0x1FC4, 0x1FC5, 0x1FC6, 0x1FC7, 0x1FC8, 0x1FC9, 0x1FCA, 0x1FCB, 0x1FC3, 0x1FCD, 0x1FCE, 0x1FCF, 0x1FD8, 0x1FD9, 0x1FD2, 0x1FD3,
    0x1FD4, 0x1FD5, 0x1FD6, 0x1FD7, 0x1FD8, 0x1FD9, 0x1FDA, 0x1FDB, 0x1FDC, 0x1FDD, 0x1FDE, 0x1FDF, 0x1FE8, 0x1FE9, 0x1FE2, 0x1FE3,
    0x1FE4, 0x1FEC, 0x1FE6, 0x1FE7, 0x1FE8, 0x1FE9, 0x1FEA, 0x1FEB, 0x1FEC, 0x1FED, 0x1FEE, 0x1FEF, 0x1FF0, 0x1FF1, 0x1FF2, 0x1FF3,
    0x1FF4, 0x1FF5, 0x1FF6, 0x1FF7, 0x1FF8, 0x1FF9, 0x1FFA, 0x1FFB, 0x1FF3, 0x1FFD, 0x1FFE, 0x1FFF, 0x2000, 0x2001, 0x2002, 0x2003,
    0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200A, 0x200B, 0x200C, 0x200D, 0x200E, 0x200F, 0x2010, 0x2011, 0x2012, 0x2013,
    0x2014, 0x2015, 0x2016, 0x2017, 0x2018, 0x2019, 0x201A, 0x201B, 0x201C, 0x201D, 0x201E, 0x201F, 0x2020, 0x2021, 0x2022, 0x2023,
    0x2024, 0x2025, 0x2026, 0x2027, 0x2028, 0x2029, 0x202A, 0x202B, 0x202C, 0x202D, 0x202E, 0x202F, 0x2030, 0x2031, 0x2032, 0x2033,
    0x2034, 0x2035, 0x2036, 0x2037, 0x2038, 0x2039, 0x203A, 0x203B, 0x203C, 0x203D, 0x203E, 0x203F, 0x2040, 0x2041, 0x2042, 0x2043,
    0x2044, 0x2045, 0x2046, 0x2047, 0x2048, 0x2049, 0x204A, 0x204B, 0x204C, 0x204D, 0x204E, 0x204F, 0x2050, 0x2051, 0x2052, 0x2053,
    0x2054, 0x2055, 0x2056, 0x2057, 0x2058, 0x2059, 0x205A, 0x205B, 0x205C, 0x205D, 0x205E, 0x205F, 0x2060, 0x2061, 0x2062, 0x2063,
    0x2064, 0x2065, 0x2066, 0x2067, 0x2068, 0x2069, 0x206A, 0x206B, 0x206C, 0x206D, 0x206E, 0x206F, 0x2070, 0x2071, 0x2072, 0x2073,
    0x2074, 0x2075, 0x2076, 0x2077, 0x2078, 0x2079, 0x207A, 0x207B, 0x207C, 0x207D, 0x207E, 0x207F, 0x2080, 0x2081, 0x2082, 0x2083,
    0x2084, 0x2085, 0x2086, 0x2087, 0x2088, 0x2089, 0x208A, 0x208B, 0x208C, 0x208D, 0x208E, 0x208F, 0x2090, 0x2091, 0x2092, 0x2093,
    0x2094, 0x2095, 0x2096, 0x2097, 0x2098, 0x2099, 0x209A, 0x209B, 0x209C, 0x209D, 0x209E, 0x209F, 0x20A0, 0x20A1, 0x20A2, 0x20A3,
    0x20A4, 0x20A5, 0x20A6, 0x20A7, 0x20A8, 0x20A9, 0x20AA, 0x20AB, 0x20AC, 0x20AD, 0x20AE, 0x20AF, 0x20B0, 0x20B1, 0x20B2, 0x20B3,
    0x20B4, 0x20B5, 0x20B6, 0x20B7, 0x20B8, 0x20B9, 0x20BA, 0x20BB, 0x20BC, 0x20BD, 0x20BE, 0x20BF, 0x20C0, 0x20C1, 0x20C2, 0x20C3,
    0x20C4, 0x20C5, 0x20C6, 0x20C7, 0x20C8, 0x20C9, 0x20CA, 0x20CB, 0x20CC, 0x20CD, 0x20CE, 0x20CF, 0x20D0, 0x20D1, 0x20D2, 0x20D3,
    0x20D4, 0x20D5, 0x20D6, 0x20D7, 0x20D8, 0x20D9, 0x20DA, 0x20DB, 0x20DC, 0x20DD, 0x20DE, 0x20DF, 0x20E0, 0x20E1, 0x20E2, 0x20E3,
    0x20E4, 0x20E5, 0x20E6, 0x20E7, 0x20E8, 0x20E9, 0x20EA, 0x20EB, 0x20EC, 0x20ED, 0x20EE, 0x20EF, 0x20F0, 0x20F1, 0x20F2, 0x20F3,
    0x20F4, 0x20F5, 0x20F6, 0x20F7, 0x20F8, 0x20F9, 0x20FA, 0x20FB, 0x20FC, 0x20FD, 0x20FE, 0x20FF, 0x2100, 0x2101, 0x2102, 0x2103,
    0x2104, 0x2105, 0x2106, 0x2107, 0x2108, 0x2109, 0x210A, 0x210B, 0x210C, 0x210D, 0x210E, 0x210F, 0x2110, 0x2111, 0x2112, 0x2113,
    0x2114, 0x2115, 0x2116, 0x2117, 0x2118, 0x2119, 0x211A, 0x211B, 0x211C, 0x211D, 0x211E, 0x211F, 0x2120, 0x2121, 0x2122, 0x2123,
    0x2124, 0x2125, 0x2126, 0x2127, 0x2128, 0x2129, 0x212A, 0x212B, 0x212C, 0x212D, 0x212E, 0x212F, 0x2130, 0x2131, 0x2132, 0x2133,
    0x2134, 0x2135, 0x2136, 0x2137, 0x2138, 0x2139, 0x213A, 0x213B, 0x213C, 0x213D, 0x213E, 0x213F, 0x2140, 0x2141, 0x2142, 0x2143,
    0x2144, 0x2145, 0x2146, 0x2147, 0x2148, 0x2149, 0x214A, 0x214B, 0x214C, 0x214D, 0x2132, 0x214F, 0x2150, 0x2151, 0x2152, 0x2153,
    0x2154, 0x2155, 0x2156, 0x2157, 0x2158, 0x2159, 0x215A, 0x215B, 0x215C, 0x215D, 0x215E, 0x215F, 0x2160, 0x2161, 0x2162, 0x2163,
    0x2164, 0x2165, 0x2166, 0x2167, 0x2168, 0x2169, 0x216A, 0x216B, 0x216C, 0x216D, 0x216E, 0x216F, 0x2160, 0x2161, 0x2162, 0x2163,
    0x2164, 0x2165, 0x2166, 0x2167, 0x2168, 0x2169, 0x216A, 0x216B, 0x216C, 0x216D, 0x216E, 0x216F, 0x2180, 0x2181, 0x2182, 0x2183,
    0x2183, 0xFFFF, 0x034B, 0x24B6, 0x24B7, 0x24B8, 0x24B9, 0x24BA, 0x24BB, 0x24BC, 0x24BD, 0x24BE, 0x24BF, 0x24C0, 0x24C1, 0x24C2,
    0x24C3, 0x24C4, 0x24C5, 0x24C6, 0x24C7, 0x24C8, 0x24C9, 0x24CA, 0x24CB, 0x24CC, 0x24CD, 0x24CE, 0x24CF, 0xFFFF, 0x0746, 0x2C00,
    0x2C01, 0x2C02, 0x2C03, 0x2C04, 0x2C05, 0x2C06, 0x2C07, 0x2C08, 0x2C09, 0x2C0A, 0x2C0B, 0x2C0C, 0x2C0D, 0x2C0E, 0x2C0F, 0x2C10,
    0x2C11, 0x2C12, 0x2C13, 0x2C14, 0x2C15, 0x2C16, 0x2C17, 0x2C18, 0x2C19, 0x2C1A, 0x2C1B, 0x2C1C, 0x2C1D, 0x2C1E, 0x2C1F, 0x2C20,
    0x2C21, 0x2C22, 0x2C23, 0x2C24, 0x2C25, 0x2C26, 0x2C27, 0x2C28, 0x2C29, 0x2C2A, 0x2C2B, 0x2C2C, 0x2C2D, 0x2C2E, 0x2C5F, 0x2C60,
    0x2C60, 0x2C62, 0x2C63, 0x2C64, 0x2C65, 0x2C66, 0x2C67, 0x2C67, 0x2C69, 0x2C69, 0x2C6B, 0x2C6B, 0x2C6D, 0x2C6E, 0x2C6F, 0x2C70,
    0x2C71, 0x2C72, 0x2C73, 0x2C74, 0x2C75, 0x2C75, 0x2C77, 0x2C78, 0x2C79, 0x2C7A, 0x2C7B, 0x2C7C, 0x2C7D, 0x2C7E, 0x2C7F, 0x2C80,
    0x2C80, 0x2C82, 0x2C82, 0x2C84, 0x2C84, 0x2C86, 0x2C86, 0x2C88, 0x2C88, 0x2C8A, 0x2C8A, 0x2C8C, 0x2C8C, 0x2C8E, 0x2C8E, 0x2C90,
    0x2C90, 0x2C92, 0x2C92, 0x2C94, 0x2C94, 0x2C96, 0x2C96, 0x2C98, 0x2C98, 0x2C9A, 0x2C9A, 0x2C9C, 0x2C9C, 0x2C9E, 0x2C9E, 0x2CA0,
    0x2CA0, 0x2CA2, 0x2CA2, 0x2CA4, 0x2CA4, 0x2CA6, 0x2CA6, 0x2CA8, 0x2CA8, 0x2CAA, 0x2CAA, 0x2CAC, 0x2CAC, 0x2CAE, 0x2CAE, 0x2CB0,
    0x2CB0, 0x2CB2, 0x2CB2, 0x2CB4, 0x2CB4, 0x2CB6, 0x2CB6, 0x2CB8, 0x2CB8, 0x2CBA, 0x2CBA, 0x2CBC, 0x2CBC, 0x2CBE, 0x2CBE, 0x2CC0,
    0x2CC0, 0x2CC2, 0x2CC2, 0x2CC4, 0x2CC4, 0x2CC6, 0x2CC6, 0x2CC8, 0x2CC8, 0x2CCA, 0x2CCA, 0x2CCC, 0x2CCC, 0x2CCE, 0x2CCE, 0x2CD0,
    0x2CD0, 0x2CD2, 0x2CD2, 0x2CD4, 0x2CD4, 0x2CD6, 0x2CD6, 0x2CD8, 0x2CD8, 0x2CDA, 0x2CDA, 0x2CDC, 0x2CDC, 0x2CDE, 0x2CDE, 0x2CE0,
    0x2CE0, 0x2CE2, 0x2CE2, 0x2CE4, 0x2CE5, 0x2CE6, 0x2CE7, 0x2CE8, 0x2CE9, 0x2CEA, 0x2CEB, 0x2CEC, 0x2CED, 0x2CEE, 0x2CEF, 0x2CF0,
    0x2CF1, 0x2CF2, 0x2CF3, 0x2CF4, 0x2CF5, 0x2CF6, 0x2CF7, 0x2CF8, 0x2CF9, 0x2CFA, 0x2CFB, 0x2CFC, 0x2CFD, 0x2CFE, 0x2CFF, 0x10A0,
    0x10A1, 0x10A2, 0x10A3, 0x10A4, 0x10A5, 0x10A6, 0x10A7, 0x10A8, 0x10A9, 0x10AA, 0x10AB, 0x10AC, 0x10AD, 0x10AE, 0x10AF, 0x10B0,
    0x10B1, 0x10B2, 0x10B3, 0x10B4, 0x10B5, 0x10B6, 0x10B7, 0x10B8, 0x10B9, 0x10BA, 0x10BB, 0x10BC, 0x10BD, 0x10BE, 0x10BF, 0x10C0,
    0x10C1, 0x10C2, 0x10C3, 0x10C4, 0x10C5, 0xFFFF, 0xD21B, 0xFF21, 0xFF22, 0xFF23, 0xFF24, 0xFF25, 0xFF26, 0xFF27, 0xFF28, 0xFF29,
    0xFF2A, 0xFF2B, 0xFF2C, 0xFF2D, 0xFF2E, 0xFF2F, 0xFF30, 0xFF31, 0xFF32, 0xFF33, 0xFF34, 0xFF35, 0xFF36, 0xFF37, 0xFF38, 0xFF39,
    0xFF3A, 0xFF5B, 0xFF5C, 0xFF5D, 0xFF5E, 0xFF5F, 0xFF60, 0xFF61, 0xFF62, 0xFF63, 0xFF64, 0xFF65, 0xFF66, 0xFF67, 0xFF68, 0xFF69,
    0xFF6A, 0xFF6B, 0xFF6C, 0xFF6D, 0xFF6E, 0xFF6F, 0xFF70, 0xFF71, 0xFF72, 0xFF73, 0xFF74, 0xFF75, 0xFF76, 0xFF77, 0xFF78, 0xFF79,
    0xFF7A, 0xFF7B, 0xFF7C, 0xFF7D, 0xFF7E, 0xFF7F, 0xFF80, 0xFF81, 0xFF82, 0xFF83, 0xFF84, 0xFF85, 0xFF86, 0xFF87, 0xFF88, 0xFF89,
    0xFF8A, 0xFF8B, 0xFF8C, 0xFF8D, 0xFF8E, 0xFF8F, 0xFF90, 0xFF91, 0xFF92, 0xFF93, 0xFF94, 0xFF95, 0xFF96, 0xFF97, 0xFF98, 0xFF99,
    0xFF9A, 0xFF9B, 0xFF9C, 0xFF9D, 0xFF9E, 0xFF9F, 0xFFA0, 0xFFA1, 0xFFA2, 0xFFA3, 0xFFA4, 0xFFA5, 0xFFA6, 0xFFA7, 0xFFA8, 0xFFA9,
    0xFFAA, 0xFFAB, 0xFFAC, 0xFFAD, 0xFFAE, 0xFFAF, 0xFFB0, 0xFFB1, 0xFFB2, 0xFFB3, 0xFFB4, 0xFFB5, 0xFFB6, 0xFFB7, 0xFFB8, 0xFFB9,
    0xFFBA, 0xFFBB, 0xFFBC, 0xFFBD, 0xFFBE, 0xFFBF, 0xFFC0, 0xFFC1, 0xFFC2, 0xFFC3, 0xFFC4, 0xFFC5, 0xFFC6, 0xFFC7, 0xFFC8, 0xFFC9,
    0xFFCA, 0xFFCB, 0xFFCC, 0xFFCD, 0xFFCE, 0xFFCF, 0xFFD0, 0xFFD1, 0xFFD2, 0xFFD3, 0xFFD4, 0xFFD5, 0xFFD6, 0xFFD7, 0xFFD8, 0xFFD9,
    0xFFDA, 0xFFDB, 0xFFDC, 0xFFDD, 0xFFDE, 0xFFDF, 0xFFE0, 0xFFE1, 0xFFE2, 0xFFE3, 0xFFE4, 0xFFE5, 0xFFE6, 0xFFE7, 0xFFE8, 0xFFE9,
    0xFFEA, 0xFFEB, 0xFFEC, 0xFFED, 0xFFEE, 0xFFEF, 0xFFF0, 0xFFF1, 0xFFF2, 0xFFF3, 0xFFF4, 0xFFF5, 0xFFF6, 0xFFF7, 0xFFF8, 0xFFF9,
    0xFFFA, 0xFFFB, 0xFFFC, 0xFFFD, 0xFFFE, 0xFFFF,
];

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{FileDev, MemDev};
    use crate::vfs::fat_fs::tests::{defaults, docker, it_dir, make_image, write_interop_files};
    use crate::vfs::testutil::{check_persisted, conformance, pattern};
    use std::process::Command;

    fn mem(size: usize) -> (Arc<MemDev>, Arc<dyn BlockDev>) {
        let m = Arc::new(MemDev::new(size));
        let d: Arc<dyn BlockDev> = m.clone();
        (m, d)
    }

    #[test]
    fn upcase_table_is_standard() {
        assert_eq!(table_checksum(&upcase_bytes()), 0xE619_D30D);
        let m = decode_upcase(&upcase_bytes());
        assert_eq!(m['a' as usize], 'A' as u16);
        assert_eq!(m['ü' as usize], 'Ü' as u16);
        assert_eq!(m['ж' as usize], 'Ж' as u16);
        assert_eq!(m['A' as usize], 'A' as u16);
    }

    #[test]
    fn exfat_conformance() {
        let (_m, dev) = mem(32 << 20);
        format(dev.clone(), "Lumen exFAT").unwrap();
        assert!(probe(dev.as_ref()));
        assert_eq!(crate::vfs::fat_fs::probe(dev.as_ref()), None);
        assert_eq!(crate::vfs::probe(dev.as_ref()), Some(FsType::ExFat));
        {
            let mut fs = ExFatFs::open(dev.clone(), false, defaults()).unwrap();
            assert_eq!(fs.volume_info().label, "Lumen exFAT");
            assert_eq!(fs.dirty_state(), DirtyState::Clean);
            conformance(&mut fs, false);
            assert!(matches!(
                fs.symlink(1, "l", "x", 0, 0),
                Err(FsError::Unsupported(_))
            ));
        }
        let mut fs = ExFatFs::open(dev.clone(), true, defaults()).unwrap();
        assert_eq!(fs.dirty_state(), DirtyState::Clean);
        check_persisted(&mut fs);
        assert_eq!(fs.mkdir(1, "x", 0o755, 0, 0), Err(FsError::ReadOnly));
        // bitmap agrees with the directory tree
        let mut p = vec![];
        let used = fs.compute_usage(&mut p).unwrap();
        assert!(p.is_empty());
        assert_eq!(used, fs.bitmap);
    }

    #[test]
    fn case_insensitive_and_hash() {
        let (_m, dev) = mem(8 << 20);
        format(dev.clone(), "").unwrap();
        let mut fs = ExFatFs::open(dev.clone(), false, defaults()).unwrap();
        let f = fs.create(1, "Ünïcödé Straße.TXT", 0o644, 0, 0).unwrap();
        assert_eq!(fs.lookup(1, "üNÏCÖDÉ STRAßE.txt").unwrap(), f);
        assert_eq!(
            fs.create(1, "ünïcödé straße.txt", 0o644, 0, 0),
            Err(FsError::Exists)
        );
        let s = fs.set_of(f).unwrap();
        let u: Vec<u16> = "Ünïcödé Straße.TXT".encode_utf16().collect();
        assert_eq!(rd16(&s, 36), name_hash(&fs.up(&u)));
        assert_eq!(rd16(&s, 2), set_checksum(&s));
        let names: Vec<String> = fs.readdir(1).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["Ünïcödé Straße.TXT"]);
        fs.rename(1, "Ünïcödé Straße.TXT", 1, "ünïcödé straße.txt")
            .unwrap();
        assert_eq!(fs.lookup(1, "X").unwrap_err(), FsError::NotFound);
        assert_eq!(fs.readdir(1).unwrap()[0].name, "ünïcödé straße.txt");
        assert_eq!(fs.lookup(1, "ÜNÏCÖDÉ STRAßE.TXT").unwrap(), f);
        let long: String = "é".repeat(255);
        fs.create(1, &long, 0o644, 0, 0).unwrap();
        assert!(fs.lookup(1, &long.to_uppercase()).is_ok());
        assert_eq!(
            fs.create(1, &"x".repeat(256), 0o644, 0, 0),
            Err(FsError::NameTooLong)
        );
    }

    #[test]
    fn nofatchain_conversion() {
        let (_m, dev) = mem(16 << 20);
        format(dev.clone(), "").unwrap();
        let mut fs = ExFatFs::open(dev.clone(), false, defaults()).unwrap();
        let cs = fs.b.cs as usize;
        let a = fs.create(1, "a", 0o644, 0, 0).unwrap();
        let b = fs.create(1, "b", 0o644, 0, 0).unwrap();
        let pa = pattern(cs * 3, 1);
        fs.write(a, 0, &pa[..cs]).unwrap();
        fs.write(b, 0, &pattern(cs, 2)).unwrap();
        assert_ne!(s_flags(&fs.set_of(a).unwrap()) & F_NOFAT, 0);
        // a cannot grow in place any more (b follows it) → converted to a FAT chain
        fs.write(a, cs as u64, &pa[cs..]).unwrap();
        let s = fs.set_of(a).unwrap();
        assert_eq!(s_flags(&s) & F_NOFAT, 0);
        assert_eq!(fs.read(a, 0, (cs * 3) as u32).unwrap(), pa);
        assert_eq!(fs.chain_n(s_first(&s), 3).unwrap().len(), 3);
        // shrinking a chained file frees its tail
        let free0 = fs.free;
        fs.setattr(
            a,
            &SetAttr {
                size: Some(cs as u64),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fs.free, free0 + 2);
        assert_eq!(fs.read(a, 0, cs as u32 * 2).unwrap(), &pa[..cs]);
        // a contiguous file extends contiguously while space follows it
        let c = fs.create(1, "c", 0o644, 0, 0).unwrap();
        for i in 0..8 {
            fs.write(c, (i * cs) as u64, &pattern(cs, 10 + i as u32))
                .unwrap();
        }
        assert_ne!(s_flags(&fs.set_of(c).unwrap()) & F_NOFAT, 0);
        // truncate-up keeps ValidDataLength; the gap reads back as zeros
        fs.setattr(
            b,
            &SetAttr {
                size: Some(cs as u64 * 2),
                ..Default::default()
            },
        )
        .unwrap();
        let s = fs.set_of(b).unwrap();
        assert_eq!(s_vdl(&s), cs as u64);
        assert_eq!(fs.read(b, cs as u64, cs as u32).unwrap(), vec![0u8; cs]);
        fs.sync().unwrap();
        let mut p = vec![];
        let used = fs.compute_usage(&mut p).unwrap();
        assert_eq!(used, fs.bitmap);
    }

    #[test]
    fn dirty_doctor() {
        let (m, dev) = mem(16 << 20);
        format(dev.clone(), "D").unwrap();
        {
            let mut fs = ExFatFs::open(dev.clone(), false, defaults()).unwrap();
            let f = fs.create(1, "f", 0o644, 0, 0).unwrap();
            fs.write(f, 0, &pattern(50_000, 3)).unwrap();
            assert_ne!(
                rd16(&m.snapshot(), 106) & VF_DIRTY,
                0,
                "dirty while unsynced"
            );
            fs.sync().unwrap();
            assert_eq!(rd16(&m.snapshot(), 106) & VF_DIRTY, 0, "clean after sync");
        }
        // simulate a crash: dirty flag + a leaked cluster + a lost allocation bit
        let fs0 = ExFatFs::open(dev.clone(), true, defaults()).unwrap();
        let (bm0, f_first) = {
            let mut fs0 = fs0;
            let f = fs0.lookup(1, "f").unwrap();
            (fs0.bm_clusters[0], s_first(&fs0.set_of(f).unwrap()))
        };
        let snap = m.snapshot();
        let b = parse_boot(&snap).unwrap();
        let bm_off = b.heap * b.bps + (bm0 as u64 - 2) * b.cs;
        let mut bmb = snap[bm_off as usize..bm_off as usize + 512].to_vec();
        let leak = 1000usize;
        bmb[leak / 8] |= 1 << (leak % 8);
        let fi = (f_first - 2) as usize;
        bmb[fi / 8] &= !(1 << (fi % 8));
        dev.write_at(bm_off, &bmb).unwrap();
        dev.write_at(106, &VF_DIRTY.to_le_bytes()).unwrap();
        let mut fs = ExFatFs::open(dev.clone(), false, defaults()).unwrap();
        let DirtyState::Dirty(r) = fs.dirty_state() else {
            panic!("dirty not detected")
        };
        assert!(r.contains("VolumeDirty"));
        let rep = fs.repair().unwrap();
        assert!(rep.contains("Cleared the VolumeDirty flag"), "{rep}");
        assert!(rep.contains("Marked 1 in-use"), "{rep}");
        assert!(rep.contains("Released 1 leaked"), "{rep}");
        drop(fs);
        let mut fs = ExFatFs::open(dev.clone(), false, defaults()).unwrap();
        assert_eq!(fs.dirty_state(), DirtyState::Clean);
        assert!(fs.repair().unwrap().contains("clean"));
        // boot checksum still valid (VolumeFlags is excluded from it)
        let snap = m.snapshot();
        assert_eq!(boot_checksum(&snap[..512 * 11]), rd32(&snap, 512 * 11));
    }

    #[test]
    fn corrupt_input_never_panics() {
        let (m, dev) = mem(8 << 20);
        format(dev.clone(), "X").unwrap();
        {
            let mut fs = ExFatFs::open(dev.clone(), false, defaults()).unwrap();
            let d = fs.mkdir(1, "dir", 0o755, 0, 0).unwrap();
            for i in 0..20 {
                let f = fs
                    .create(d, &format!("some long file name {i}"), 0o644, 0, 0)
                    .unwrap();
                fs.write(f, 0, &pattern(9000, i)).unwrap();
            }
        }
        let clean = m.snapshot();
        let b = parse_boot(&clean).unwrap();
        let heap = (b.heap * b.bps) as usize;
        let mut seed = 777u32;
        for round in 0..300 {
            let d2 = Arc::new(MemDev::new(clean.len()));
            d2.write_at(0, &clean).unwrap();
            for _ in 0..(1 + round % 30) {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                // hit the FAT, bitmap / up-case / root and the first directory clusters
                let region = if seed & 1 == 0 {
                    (b.fat_offset * b.bps) as usize
                } else {
                    heap
                };
                let off = region + (seed as usize >> 1) % (64 * 1024);
                d2.write_at(off as u64, &[(seed >> 9) as u8]).unwrap();
            }
            let d2: Arc<dyn BlockDev> = d2;
            if let Ok(mut fs) = ExFatFs::open(d2, false, defaults()) {
                if let Ok(ents) = fs.readdir(1) {
                    for e in ents {
                        let _ = fs.getattr(e.ino);
                        if let Ok(sub) = fs.readdir(e.ino) {
                            for s in sub {
                                let _ = fs.read(s.ino, 0, 20000);
                                let _ = fs.write(s.ino, 5000, b"xyz");
                                let _ = fs.setattr(
                                    s.ino,
                                    &SetAttr {
                                        size: Some(1),
                                        ..Default::default()
                                    },
                                );
                            }
                        }
                    }
                }
                let _ = fs.create(1, "new", 0o644, 0, 0);
                let _ = fs.repair();
            }
        }
    }

    // ── integration tests against real tools (run with --ignored) ──

    fn fsck_ok(dir: &std::path::Path, name: &str) -> String {
        let (_, out) = docker(
            dir,
            false,
            &format!("fsck.exfat -n -v /w/{name}; echo FSCK_RC=$?"),
        );
        println!("{out}");
        assert!(
            out.contains("FSCK_RC=0"),
            "fsck.exfat reported problems:\n{out}"
        );
        let (ok, s) = macos_fsck_exfat(&dir.join(name));
        println!("{s}");
        assert!(ok, "fsck_exfat failed:\n{s}");
        out
    }

    /// macOS fsck_exfat needs a device node: attach the image read-only without mounting.
    fn macos_fsck_exfat(img: &std::path::Path) -> (bool, String) {
        let o = Command::new("hdiutil")
            .args([
                "attach",
                "-nomount",
                "-readonly",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
            ])
            .arg(img)
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&o.stdout).to_string();
        let Some(dev) = out
            .split_whitespace()
            .find(|w| w.starts_with("/dev/disk"))
            .map(str::to_string)
        else {
            return (
                false,
                format!(
                    "hdiutil attach failed: {out}{}",
                    String::from_utf8_lossy(&o.stderr)
                ),
            );
        };
        let f = Command::new("/sbin/fsck_exfat")
            .arg("-n")
            .arg(dev.replace("/dev/disk", "/dev/rdisk"))
            .output()
            .unwrap();
        let _ = Command::new("hdiutil").args(["detach", &dev]).output();
        let s =
            String::from_utf8_lossy(&f.stdout).to_string() + &String::from_utf8_lossy(&f.stderr);
        (f.status.success() && s.contains("appears to be OK"), s)
    }

    fn kernel_check(dir: &std::path::Path, name: &str) {
        let script = format!(
            "mkdir /m && mount -o loop,ro,iocharset=utf8 /w/{name} /m && find /m | sort && \
             cat /m/keep/sub/hello.txt && echo && cat '/m/keep/Cartella con nome lungo/'Questo* && \
             cat '/m/keep/Cartella con nome lungo/moved dir/inside.txt' && md5sum /m/keep/persist.bin && umount /m"
        );
        let (ok, out) = docker(dir, true, &script);
        println!("{out}");
        assert!(ok, "kernel mount failed:\n{out}");
        let p = pattern(300 * 1024, 7);
        std::fs::write(dir.join("expect.bin"), &p).unwrap();
        let o = Command::new("md5")
            .arg("-q")
            .arg(dir.join("expect.bin"))
            .output()
            .unwrap();
        let md5 = String::from_utf8_lossy(&o.stdout).trim().to_string();
        for want in [
            "hello lumendisk",
            "contenuto unicode",
            "inside",
            "Questo è un nome di file molto lungo – ünïcödé 文件.txt",
            "/m/keep/lower.txt",
            "/m/keep/MiXeD.TxT",
            md5.as_str(),
        ] {
            assert!(out.contains(want), "missing {want:?}:\n{out}");
        }
    }

    fn exfat_it(size: u64, name: &str, real_mkfs: bool) {
        let dir = it_dir();
        let img = dir.join(name);
        make_image(&img, size);
        if real_mkfs {
            // Real mkfs.exfat, then let the Linux kernel driver write some files we must read.
            let (ok, out) = docker(
                &dir,
                true,
                &format!(
                    "mkfs.exfat -L REALEX /w/{name} >/dev/null && mkdir /m && mount -o loop,iocharset=utf8 /w/{name} /m && \
                     head -c 3000000 /dev/urandom > /m/kernel.bin && mkdir '/m/Kernel Dir' && \
                     printf 'from linux' > '/m/Kernel Dir/Ärger mit Umlauten.txt' && \
                     for i in 1 2 3 4 5 6 7 8; do head -c 20000 /dev/urandom > /m/frag$i; done && \
                     rm /m/frag2 /m/frag4 /m/frag6 && head -c 200000 /dev/urandom >> /m/frag1 && \
                     md5sum /m/kernel.bin /m/frag1 && umount /m"
                ),
            );
            assert!(ok, "{out}");
            let sums: Vec<&str> = out
                .lines()
                .filter(|l| l.contains("/m/"))
                .map(|l| l.split_whitespace().next().unwrap())
                .collect();
            let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
            let mut fs = ExFatFs::open(dev.clone(), false, defaults()).unwrap();
            assert_eq!(fs.volume_info().label, "REALEX");
            for (file, sum) in [("kernel.bin", sums[0]), ("frag1", sums[1])] {
                let f = fs.lookup(1, file).unwrap();
                let data = fs.read(f, 0, 10 << 20).unwrap();
                std::fs::write(dir.join("got.bin"), &data).unwrap();
                let o = Command::new("md5")
                    .arg("-q")
                    .arg(dir.join("got.bin"))
                    .output()
                    .unwrap();
                assert_eq!(
                    String::from_utf8_lossy(&o.stdout).trim(),
                    sum,
                    "{file} differs"
                );
            }
            let kd = fs.lookup(1, "kernel dir").unwrap();
            let f = fs.lookup(kd, "ärger MIT umlauten.TXT").unwrap();
            assert_eq!(fs.read(f, 0, 100).unwrap(), b"from linux");
            // modify kernel-written fragmented file: append + truncate
            let f1 = fs.lookup(1, "frag1").unwrap();
            fs.write(f1, 220_000, b"tail").unwrap();
            fs.setattr(
                f1,
                &SetAttr {
                    size: Some(100_000),
                    ..Default::default()
                },
            )
            .unwrap();
            fs.unlink(1, "kernel.bin").unwrap();
            conformance(&mut fs, false);
            write_interop_files(&mut fs);
        } else {
            let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
            format(dev.clone(), "LumenFmt").unwrap();
            let mut fs = ExFatFs::open(dev.clone(), false, defaults()).unwrap();
            conformance(&mut fs, false);
            write_interop_files(&mut fs);
        }
        {
            let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, true).unwrap());
            let mut fs = ExFatFs::open(dev, true, defaults()).unwrap();
            check_persisted(&mut fs);
        }
        fsck_ok(&dir, name);
        kernel_check(&dir, name);
        let (_, out) = docker(&dir, false, &format!("exfatlabel /w/{name}"));
        println!("{out}");
        assert!(
            out.contains(if real_mkfs { "REALEX" } else { "LumenFmt" }),
            "{out}"
        );
    }

    #[test]
    #[ignore]
    fn it_real_mkfs_exfat() {
        exfat_it(64 << 20, "realex.img", true);
    }
    #[test]
    #[ignore]
    fn it_own_format_exfat_small() {
        exfat_it(48 << 20, "ownex.img", false);
    }
    #[test]
    #[ignore]
    fn it_own_format_exfat_32k_clusters() {
        exfat_it(400 << 20, "ownex32k.img", false);
    }

    #[test]
    #[ignore]
    fn it_dirty_doctor_real_fsck() {
        let dir = it_dir();
        let name = "dirtyex.img";
        let img = dir.join(name);
        make_image(&img, 64 << 20);
        let (ok, out) = docker(&dir, false, &format!("mkfs.exfat /w/{name}"));
        assert!(ok, "{out}");
        {
            let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
            let mut fs = ExFatFs::open(dev.clone(), false, defaults()).unwrap();
            let f = fs.create(1, "data.bin", 0o644, 0, 0).unwrap();
            fs.write(f, 0, &pattern(500_000, 4)).unwrap();
            fs.sync().unwrap();
            let bm0 = fs.bm_clusters[0];
            let b = fs.b.clone();
            let first = s_first(&fs.set_of(f).unwrap());
            std::mem::forget(fs); // crash: nothing more is written
            dev.write_at(106, &VF_DIRTY.to_le_bytes()).unwrap();
            // lose the allocation bit of data.bin's first cluster and leak 8 others
            let base = b.heap * b.bps + (bm0 as u64 - 2) * b.cs;
            let i = (first - 2) as u64;
            let mut byte = [0u8; 1];
            dev.read_at(base + i / 8, &mut byte).unwrap();
            dev.write_at(base + i / 8, &[byte[0] & !(1 << (i % 8))])
                .unwrap();
            dev.write_at(base + 1500, &[0xFF]).unwrap();
        }
        let (_, out) = docker(
            &dir,
            false,
            &format!("fsck.exfat -n /w/{name}; echo FSCK_RC=$?"),
        );
        println!("{out}");
        assert!(
            !out.contains("FSCK_RC=0"),
            "fsck.exfat should flag the volume:\n{out}"
        );
        let (ok, s) = macos_fsck_exfat(&img);
        println!("{s}");
        assert!(!ok, "fsck_exfat should flag the volume");
        {
            let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
            let mut fs = ExFatFs::open(dev, false, defaults()).unwrap();
            assert!(matches!(fs.dirty_state(), DirtyState::Dirty(_)));
            println!("{}", fs.repair().unwrap());
            assert_eq!(fs.dirty_state(), DirtyState::Clean);
        }
        fsck_ok(&dir, name);
    }

    /// Rough throughput check (run with `--ignored --nocapture`, ideally `--release`).
    #[test]
    #[ignore]
    fn it_perf_smoke() {
        let dir = it_dir();
        let img = dir.join("perfex.img");
        make_image(&img, 2 << 30);
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
        format(dev.clone(), "PERF").unwrap();
        let mut fs = ExFatFs::open(dev, false, defaults()).unwrap();
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
