//! NTFS on-disk structures: little-endian helpers, update-sequence fixups, runlists,
//! attribute / MFT record (de)serialisation, index entries and nodes, times, SDs.
//!
//! Every parser is bounds-checked and returns `FsError::Corrupt` on malformed input.
#![allow(dead_code)]

use crate::vfs::{FsError, FsResult, Timestamp};

pub const AT_STANDARD_INFORMATION: u32 = 0x10;
pub const AT_ATTRIBUTE_LIST: u32 = 0x20;
pub const AT_FILE_NAME: u32 = 0x30;
pub const AT_SECURITY_DESCRIPTOR: u32 = 0x50;
pub const AT_VOLUME_NAME: u32 = 0x60;
pub const AT_VOLUME_INFORMATION: u32 = 0x70;
pub const AT_DATA: u32 = 0x80;
pub const AT_INDEX_ROOT: u32 = 0x90;
pub const AT_INDEX_ALLOCATION: u32 = 0xA0;
pub const AT_BITMAP: u32 = 0xB0;
pub const AT_REPARSE_POINT: u32 = 0xC0;
pub const AT_END: u32 = 0xFFFF_FFFF;

pub const ATTR_COMPRESSION_MASK: u16 = 0x00FF;
pub const ATTR_ENCRYPTED: u16 = 0x4000;
pub const ATTR_SPARSE: u16 = 0x8000;

pub const MFT_IN_USE: u16 = 0x0001;
pub const MFT_IS_DIR: u16 = 0x0002;

pub const FILE_ATTR_READONLY: u32 = 0x0001;
pub const FILE_ATTR_HIDDEN: u32 = 0x0002;
pub const FILE_ATTR_SYSTEM: u32 = 0x0004;
pub const FILE_ATTR_ARCHIVE: u32 = 0x0020;
pub const FILE_ATTR_SPARSE: u32 = 0x0200;
pub const FILE_ATTR_REPARSE: u32 = 0x0400;
pub const FILE_ATTR_COMPRESSED: u32 = 0x0800;
pub const FILE_ATTR_ENCRYPTED: u32 = 0x4000;
pub const FILE_ATTR_DUP_DIR: u32 = 0x1000_0000;
pub const FILE_ATTR_DUP_VIEW_INDEX: u32 = 0x2000_0000;

pub const NS_POSIX: u8 = 0;
pub const NS_WIN32: u8 = 1;
pub const NS_DOS: u8 = 2;
pub const NS_WIN32_DOS: u8 = 3;

pub const IE_HAS_CHILD: u16 = 0x01;
pub const IE_LAST: u16 = 0x02;

pub fn corrupt<T>(msg: impl Into<String>) -> FsResult<T> {
    Err(FsError::Corrupt(msg.into()))
}

#[inline]
pub fn le16(b: &[u8], o: usize) -> FsResult<u16> {
    match b.get(o..o + 2) {
        Some(s) => Ok(u16::from_le_bytes([s[0], s[1]])),
        None => corrupt("short read (u16)"),
    }
}
#[inline]
pub fn le32(b: &[u8], o: usize) -> FsResult<u32> {
    match b.get(o..o + 4) {
        Some(s) => Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]])),
        None => corrupt("short read (u32)"),
    }
}
#[inline]
pub fn le64(b: &[u8], o: usize) -> FsResult<u64> {
    match b.get(o..o + 8) {
        Some(s) => {
            let mut a = [0u8; 8];
            a.copy_from_slice(s);
            Ok(u64::from_le_bytes(a))
        }
        None => corrupt("short read (u64)"),
    }
}
#[inline]
pub fn byte(b: &[u8], o: usize) -> FsResult<u8> {
    b.get(o).copied().ok_or_else(|| FsError::Corrupt("short read (u8)".into()))
}
#[inline]
pub fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn align8(v: usize) -> usize {
    (v + 7) & !7
}

pub fn utf16_bytes(name: &[u16]) -> Vec<u8> {
    name.iter().flat_map(|c| c.to_le_bytes()).collect()
}
pub fn bytes_utf16(b: &[u8]) -> Vec<u16> {
    b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}
pub fn to_utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

// ---------------------------------------------------------------------------------------------
// Time

const NTFS_EPOCH_OFFSET: i64 = 11_644_473_600; // seconds between 1601-01-01 and 1970-01-01

pub fn ts_to_ntfs(t: Timestamp) -> u64 {
    let secs = t.secs.saturating_add(NTFS_EPOCH_OFFSET);
    if secs < 0 {
        return 0;
    }
    (secs as u64).saturating_mul(10_000_000).saturating_add((t.nsecs / 100) as u64)
}
pub fn ntfs_to_ts(v: u64) -> Timestamp {
    let secs = (v / 10_000_000) as i64 - NTFS_EPOCH_OFFSET;
    Timestamp { secs, nsecs: ((v % 10_000_000) * 100) as u32 }
}
pub fn ntfs_now() -> u64 {
    ts_to_ntfs(Timestamp::now())
}

// ---------------------------------------------------------------------------------------------
// Update sequence (multi-sector transfer protection)

/// Verify and undo the fixups of a multi-sector protected structure read from disk.
pub fn fixup_read(buf: &mut [u8]) -> FsResult<()> {
    let usa_ofs = le16(buf, 4)? as usize;
    let usa_cnt = le16(buf, 6)? as usize;
    if usa_cnt == 0 || usa_ofs + usa_cnt * 2 > buf.len() || (usa_cnt - 1) * 512 > buf.len() || usa_ofs & 1 != 0 {
        return corrupt("bad update sequence array");
    }
    let usn = le16(buf, usa_ofs)?;
    for i in 1..usa_cnt {
        let end = i * 512;
        if buf[end - 2..end] != usn.to_le_bytes() {
            return corrupt("update sequence mismatch (torn write)");
        }
        let (a, b) = (buf[usa_ofs + 2 * i], buf[usa_ofs + 2 * i + 1]);
        buf[end - 2] = a;
        buf[end - 1] = b;
    }
    Ok(())
}

/// Apply fixups before writing. `usa_ofs`/`usa_cnt` must already be set in the header.
/// Uses (and stores) `usn` as the update sequence number.
pub fn fixup_write(buf: &mut [u8], usn: u16) {
    let usa_ofs = u16::from_le_bytes([buf[4], buf[5]]) as usize;
    let usa_cnt = u16::from_le_bytes([buf[6], buf[7]]) as usize;
    put16(buf, usa_ofs, usn);
    for i in 1..usa_cnt {
        let end = i * 512;
        if end > buf.len() {
            break;
        }
        buf[usa_ofs + 2 * i] = buf[end - 2];
        buf[usa_ofs + 2 * i + 1] = buf[end - 1];
        put16(buf, end - 2, usn);
    }
}

pub fn next_usn(u: u16) -> u16 {
    let n = u.wrapping_add(1);
    if n == 0 || n == 0xFFFF {
        1
    } else {
        n
    }
}

// ---------------------------------------------------------------------------------------------
// Runlists

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub vcn: u64,
    /// `None` = sparse (hole).
    pub lcn: Option<u64>,
    pub len: u64,
}

/// NTFS cannot address more than 2^48 clusters; anything larger is corruption.
pub const MAX_CLUSTERS: u64 = 1 << 48;

pub fn decode_runs(mp: &[u8], start_vcn: u64) -> FsResult<Vec<Run>> {
    if start_vcn > MAX_CLUSTERS {
        return corrupt("start vcn out of range");
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut lcn: i64 = 0;
    let mut vcn = start_vcn;
    while i < mp.len() && mp[i] != 0 {
        let h = mp[i];
        let lsz = (h & 0x0F) as usize;
        let osz = (h >> 4) as usize;
        i += 1;
        if lsz == 0 || lsz > 8 || osz > 8 || i + lsz + osz > mp.len() {
            return corrupt("bad mapping pairs");
        }
        let len = read_signed(&mp[i..i + lsz]);
        i += lsz;
        if len <= 0 {
            return corrupt("bad run length");
        }
        let run_lcn = if osz == 0 {
            None
        } else {
            let d = read_signed(&mp[i..i + osz]);
            i += osz;
            lcn = lcn.checked_add(d).ok_or_else(|| FsError::Corrupt("lcn overflow".into()))?;
            if lcn < 0 {
                return corrupt("negative lcn");
            }
            Some(lcn as u64)
        };
        out.push(Run { vcn, lcn: run_lcn, len: len as u64 });
        vcn = vcn.checked_add(len as u64).ok_or_else(|| FsError::Corrupt("vcn overflow".into()))?;
        if vcn > MAX_CLUSTERS || run_lcn.map_or(false, |l| l > MAX_CLUSTERS) {
            return corrupt("run beyond the maximum volume size");
        }
        if out.len() > 1_000_000 {
            return corrupt("runlist too long");
        }
    }
    Ok(out)
}

fn read_signed(b: &[u8]) -> i64 {
    let mut v: i64 = 0;
    for (k, &x) in b.iter().enumerate() {
        v |= (x as i64) << (8 * k);
    }
    let bits = 8 * b.len();
    if bits < 64 && b.last().map_or(false, |x| x & 0x80 != 0) {
        v |= -1i64 << bits;
    }
    v
}

fn signed_len(v: i64) -> usize {
    for n in 1..8 {
        let bits = 8 * n;
        let min = -(1i64 << (bits - 1));
        let max = (1i64 << (bits - 1)) - 1;
        if v >= min && v <= max {
            return n;
        }
    }
    8
}

pub fn encode_runs(runs: &[Run]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev: i64 = 0;
    for r in runs {
        let l = r.len as i64;
        let ls = signed_len(l);
        match r.lcn {
            Some(lcn) => {
                let d = lcn as i64 - prev;
                let os = signed_len(d);
                out.push((os << 4 | ls) as u8);
                out.extend_from_slice(&l.to_le_bytes()[..ls]);
                out.extend_from_slice(&d.to_le_bytes()[..os]);
                prev = lcn as i64;
            }
            None => {
                out.push(ls as u8);
                out.extend_from_slice(&l.to_le_bytes()[..ls]);
            }
        }
    }
    out.push(0);
    out
}

/// Append runs, merging with the last run when physically contiguous.
pub fn push_run(runs: &mut Vec<Run>, lcn: Option<u64>, len: u64) {
    if len == 0 {
        return;
    }
    if let Some(last) = runs.last_mut() {
        let contiguous = match (last.lcn, lcn) {
            (Some(a), Some(b)) => a + last.len == b,
            (None, None) => true,
            _ => false,
        };
        if contiguous {
            last.len += len;
            return;
        }
        let vcn = last.vcn + last.len;
        runs.push(Run { vcn, lcn, len });
    } else {
        runs.push(Run { vcn: 0, lcn, len });
    }
}

pub fn runs_end(runs: &[Run]) -> u64 {
    runs.last().map_or(0, |r| r.vcn + r.len)
}

// ---------------------------------------------------------------------------------------------
// Attributes

#[derive(Clone, Debug)]
pub struct NonRes {
    pub start_vcn: u64,
    pub runs: Vec<Run>,
    pub alloc: u64,
    pub size: u64,
    pub init: u64,
    pub comp_unit: u8,
    pub comp_size: u64,
    /// Last VCN as stored on disk (used when the runlist is empty / partial extents).
    pub last_vcn: i64,
}

#[derive(Clone, Debug)]
pub enum Body {
    Res { value: Vec<u8>, rflags: u8 },
    NonRes(NonRes),
}

#[derive(Clone, Debug)]
pub struct Attribute {
    pub ty: u32,
    pub name: Vec<u16>,
    pub flags: u16,
    pub instance: u16,
    pub body: Body,
}

impl Attribute {
    pub fn resident(ty: u32, name: &[u16], value: Vec<u8>) -> Self {
        Attribute { ty, name: name.to_vec(), flags: 0, instance: 0, body: Body::Res { value, rflags: 0 } }
    }
    pub fn nonresident(ty: u32, name: &[u16], runs: Vec<Run>, size: u64, cluster: u64) -> Self {
        let alloc = runs_end(&runs) * cluster;
        Attribute {
            ty,
            name: name.to_vec(),
            flags: 0,
            instance: 0,
            body: Body::NonRes(NonRes {
                start_vcn: 0,
                last_vcn: runs_end(&runs) as i64 - 1,
                runs,
                alloc,
                size,
                init: size,
                comp_unit: 0,
                comp_size: 0,
            }),
        }
    }
    pub fn is_named(&self, ty: u32, name: &[u16]) -> bool {
        self.ty == ty && self.name == name
    }
    pub fn value(&self) -> Option<&[u8]> {
        match &self.body {
            Body::Res { value, .. } => Some(value),
            _ => None,
        }
    }
    pub fn nonres(&self) -> Option<&NonRes> {
        match &self.body {
            Body::NonRes(n) => Some(n),
            _ => None,
        }
    }
    pub fn data_size(&self) -> u64 {
        match &self.body {
            Body::Res { value, .. } => value.len() as u64,
            Body::NonRes(n) => n.size,
        }
    }
    fn has_comp_size(&self) -> bool {
        self.flags & (ATTR_COMPRESSION_MASK | ATTR_SPARSE) != 0
    }

    /// Serialised length on disk.
    pub fn disk_len(&self) -> usize {
        match &self.body {
            Body::Res { value, .. } => align8(align8(0x18 + 2 * self.name.len()) + value.len()),
            Body::NonRes(n) => {
                let hdr = if self.has_comp_size() { 0x48 } else { 0x40 };
                align8(align8(hdr + 2 * self.name.len()) + encode_runs(&n.runs).len())
            }
        }
    }

    pub fn parse(b: &[u8]) -> FsResult<(Attribute, usize)> {
        let ty = le32(b, 0)?;
        let len = le32(b, 4)? as usize;
        if len < 0x18 || len > b.len() || len % 8 != 0 {
            return corrupt(format!("bad attribute length {len}"));
        }
        let b = &b[..len];
        let nonres = byte(b, 8)?;
        let name_len = byte(b, 9)? as usize;
        let name_off = le16(b, 10)? as usize;
        let flags = le16(b, 12)?;
        let instance = le16(b, 14)?;
        if name_off + 2 * name_len > len {
            return corrupt("attribute name out of bounds");
        }
        let name = bytes_utf16(&b[name_off..name_off + 2 * name_len]);
        let body = if nonres == 0 {
            let vlen = le32(b, 0x10)? as usize;
            let voff = le16(b, 0x14)? as usize;
            let rflags = byte(b, 0x16)?;
            if voff.checked_add(vlen).map_or(true, |e| e > len) {
                return corrupt("resident value out of bounds");
            }
            Body::Res { value: b[voff..voff + vlen].to_vec(), rflags }
        } else {
            if len < 0x40 {
                return corrupt("short non-resident attribute");
            }
            let start_vcn = le64(b, 0x10)?;
            let last_vcn = le64(b, 0x18)? as i64;
            let mp_off = le16(b, 0x20)? as usize;
            let comp_unit = byte(b, 0x22)?;
            let alloc = le64(b, 0x28)?;
            let size = le64(b, 0x30)?;
            let init = le64(b, 0x38)?;
            let comp_size = if flags & (ATTR_COMPRESSION_MASK | ATTR_SPARSE) != 0 && len >= 0x48 { le64(b, 0x40)? } else { 0 };
            if mp_off > len || start_vcn > i64::MAX as u64 {
                return corrupt("mapping pairs out of bounds");
            }
            let runs = decode_runs(&b[mp_off..], start_vcn)?;
            Body::NonRes(NonRes { start_vcn, runs, alloc, size, init, comp_unit, comp_size, last_vcn })
        };
        Ok((Attribute { ty, name, flags, instance, body }, len))
    }

    pub fn serialize(&self, out: &mut Vec<u8>) {
        let start = out.len();
        let len = self.disk_len();
        out.resize(start + len, 0);
        let b = &mut out[start..];
        put32(b, 0, self.ty);
        put32(b, 4, len as u32);
        b[9] = self.name.len() as u8;
        put16(b, 12, self.flags);
        put16(b, 14, self.instance);
        let nb = utf16_bytes(&self.name);
        match &self.body {
            Body::Res { value, rflags } => {
                b[8] = 0;
                put16(b, 10, 0x18);
                b[0x18..0x18 + nb.len()].copy_from_slice(&nb);
                let voff = align8(0x18 + nb.len());
                put32(b, 0x10, value.len() as u32);
                put16(b, 0x14, voff as u16);
                b[0x16] = *rflags;
                b[voff..voff + value.len()].copy_from_slice(value);
            }
            Body::NonRes(n) => {
                b[8] = 1;
                let hdr = if self.has_comp_size() { 0x48 } else { 0x40 };
                put16(b, 10, hdr as u16);
                b[hdr..hdr + nb.len()].copy_from_slice(&nb);
                let mp_off = align8(hdr + nb.len());
                let mp = encode_runs(&n.runs);
                let last_vcn = if n.runs.is_empty() { n.start_vcn as i64 - 1 } else { runs_end(&n.runs) as i64 - 1 };
                put64(b, 0x10, n.start_vcn);
                put64(b, 0x18, last_vcn as u64);
                put16(b, 0x20, mp_off as u16);
                b[0x22] = n.comp_unit;
                put64(b, 0x28, n.alloc);
                put64(b, 0x30, n.size);
                put64(b, 0x38, n.init);
                if hdr == 0x48 {
                    put64(b, 0x40, n.comp_size);
                }
                b[mp_off..mp_off + mp.len()].copy_from_slice(&mp);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// MFT records

#[derive(Clone, Debug)]
pub struct MftRecord {
    pub seq: u16,
    pub link_count: u16,
    pub flags: u16,
    pub base: u64,
    pub next_instance: u16,
    pub lsn: u64,
    pub usn: u16,
    pub attrs: Vec<Attribute>,
}

pub fn attrs_offset(rec_size: usize) -> usize {
    align8(0x30 + 2 * (1 + rec_size / 512))
}

impl MftRecord {
    pub fn new(seq: u16, flags: u16) -> Self {
        MftRecord { seq, link_count: 0, flags, base: 0, next_instance: 0, lsn: 0, usn: 0, attrs: vec![] }
    }

    /// Parse a record whose fixups have already been applied.
    pub fn parse(b: &[u8]) -> FsResult<MftRecord> {
        if b.len() < 0x30 || &b[0..4] != b"FILE" {
            return corrupt("bad MFT record magic");
        }
        let usa_ofs = le16(b, 4)? as usize;
        let lsn = le64(b, 8)?;
        let seq = le16(b, 0x10)?;
        let link_count = le16(b, 0x12)?;
        let aoff = le16(b, 0x14)? as usize;
        let flags = le16(b, 0x16)?;
        let used = le32(b, 0x18)? as usize;
        let base = le64(b, 0x20)?;
        let next_instance = le16(b, 0x28)?;
        let usn = le16(b, usa_ofs)?;
        if used > b.len() || aoff >= b.len() || aoff < 0x18 {
            return corrupt("bad MFT record header");
        }
        let mut attrs = Vec::new();
        let mut off = aoff;
        let lim = used.min(b.len());
        loop {
            if off + 4 > lim {
                return corrupt("attribute runs past record end");
            }
            let ty = le32(b, off)?;
            if ty == AT_END {
                break;
            }
            let (a, l) = Attribute::parse(&b[off..lim])?;
            attrs.push(a);
            off += l;
            if attrs.len() > 512 {
                return corrupt("too many attributes");
            }
        }
        Ok(MftRecord { seq, link_count, flags, base, next_instance, lsn, usn, attrs })
    }

    pub fn used_size(&self, rec_size: usize) -> usize {
        attrs_offset(rec_size) + self.attrs.iter().map(|a| a.disk_len()).sum::<usize>() + 8
    }

    pub fn fits(&self, rec_size: usize) -> bool {
        self.used_size(rec_size) <= rec_size
    }

    pub fn sort_attrs(&mut self) {
        self.attrs.sort_by(|a, b| (a.ty, &a.name).cmp(&(b.ty, &b.name)));
    }

    pub fn add_attr(&mut self, mut a: Attribute) {
        a.instance = self.next_instance;
        self.next_instance = self.next_instance.wrapping_add(1);
        self.attrs.push(a);
        self.sort_attrs();
    }

    /// Serialise with fixups. Returns `None` if the record overflows.
    pub fn serialize(&mut self, rec_no: u64, rec_size: usize) -> Option<Vec<u8>> {
        self.sort_attrs();
        if !self.fits(rec_size) {
            return None;
        }
        let mut b = vec![0u8; 0];
        b.resize(attrs_offset(rec_size), 0);
        b[0..4].copy_from_slice(b"FILE");
        put16(&mut b, 4, 0x30);
        put16(&mut b, 6, (1 + rec_size / 512) as u16);
        put64(&mut b, 8, self.lsn);
        put16(&mut b, 0x10, self.seq);
        put16(&mut b, 0x12, self.link_count);
        put16(&mut b, 0x14, attrs_offset(rec_size) as u16);
        put16(&mut b, 0x16, self.flags);
        put64(&mut b, 0x20, self.base);
        put16(&mut b, 0x28, self.next_instance);
        put32(&mut b, 0x2C, rec_no as u32);
        for a in &self.attrs {
            a.serialize(&mut b);
        }
        b.extend_from_slice(&AT_END.to_le_bytes());
        b.extend_from_slice(&[0u8; 4]);
        let used = b.len();
        b.resize(rec_size, 0);
        put32(&mut b, 0x18, used as u32);
        put32(&mut b, 0x1C, rec_size as u32);
        self.usn = next_usn(self.usn);
        fixup_write(&mut b, self.usn);
        Some(b)
    }
}

/// An empty, not-in-use but well-formed record (used when formatting / growing the MFT).
pub fn empty_record(rec_no: u64, rec_size: usize, seq: u16) -> Vec<u8> {
    let mut r = MftRecord::new(seq, 0);
    r.serialize(rec_no, rec_size).unwrap_or_else(|| vec![0u8; rec_size])
}

// ---------------------------------------------------------------------------------------------
// $STANDARD_INFORMATION / $FILE_NAME

#[derive(Clone, Debug, Default)]
pub struct StdInfo {
    pub crtime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atime: u64,
    pub attrs: u32,
}

impl StdInfo {
    pub fn parse(v: &[u8]) -> FsResult<StdInfo> {
        Ok(StdInfo { crtime: le64(v, 0)?, mtime: le64(v, 8)?, ctime: le64(v, 16)?, atime: le64(v, 24)?, attrs: le32(v, 32)? })
    }
    /// Patch the times/attrs into an existing value, keeping the rest.
    pub fn store(&self, v: &mut Vec<u8>) {
        if v.len() < 48 {
            v.resize(48, 0);
        }
        put64(v, 0, self.crtime);
        put64(v, 8, self.mtime);
        put64(v, 16, self.ctime);
        put64(v, 24, self.atime);
        put32(v, 32, self.attrs);
    }
    pub fn new_value(&self, security_id: u32) -> Vec<u8> {
        let mut v = vec![0u8; 72];
        self.store(&mut v);
        put32(&mut v, 52, security_id);
        v
    }
}

#[derive(Clone, Debug)]
pub struct FileName {
    pub parent: u64,
    pub crtime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atime: u64,
    pub alloc: u64,
    pub size: u64,
    pub flags: u32,
    pub reparse: u32,
    pub ns: u8,
    pub name: Vec<u16>,
}

impl FileName {
    pub fn parse(v: &[u8]) -> FsResult<FileName> {
        let nlen = byte(v, 0x40)? as usize;
        let ns = byte(v, 0x41)?;
        if v.len() < 0x42 + 2 * nlen {
            return corrupt("short $FILE_NAME");
        }
        Ok(FileName {
            parent: le64(v, 0)?,
            crtime: le64(v, 8)?,
            mtime: le64(v, 0x10)?,
            ctime: le64(v, 0x18)?,
            atime: le64(v, 0x20)?,
            alloc: le64(v, 0x28)?,
            size: le64(v, 0x30)?,
            flags: le32(v, 0x38)?,
            reparse: le32(v, 0x3C)?,
            ns,
            name: bytes_utf16(&v[0x42..0x42 + 2 * nlen]),
        })
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = vec![0u8; 0x42 + 2 * self.name.len()];
        put64(&mut v, 0, self.parent);
        put64(&mut v, 8, self.crtime);
        put64(&mut v, 0x10, self.mtime);
        put64(&mut v, 0x18, self.ctime);
        put64(&mut v, 0x20, self.atime);
        put64(&mut v, 0x28, self.alloc);
        put64(&mut v, 0x30, self.size);
        put32(&mut v, 0x38, self.flags);
        put32(&mut v, 0x3C, self.reparse);
        v[0x40] = self.name.len() as u8;
        v[0x41] = self.ns;
        v[0x42..].copy_from_slice(&utf16_bytes(&self.name));
        v
    }
}

pub fn mref_no(r: u64) -> u64 {
    r & 0x0000_FFFF_FFFF_FFFF
}
pub fn mref_seq(r: u64) -> u16 {
    (r >> 48) as u16
}
pub fn mk_ref(no: u64, seq: u16) -> u64 {
    no | ((seq as u64) << 48)
}

// ---------------------------------------------------------------------------------------------
// Index entries / nodes

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IEntry {
    /// For $I30 indexes the file reference; for view indexes the raw first 8 bytes.
    pub data: u64,
    pub key: Vec<u8>,
    /// Data following the key (view indexes only).
    pub extra: Vec<u8>,
    pub child: Option<u64>,
    pub last: bool,
}

impl IEntry {
    pub fn end(child: Option<u64>) -> Self {
        IEntry { data: 0, key: vec![], extra: vec![], child, last: true }
    }
    pub fn disk_len(&self) -> usize {
        align8(16 + self.key.len() + self.extra.len()) + if self.child.is_some() { 8 } else { 0 }
    }
    pub fn serialize(&self, out: &mut Vec<u8>) {
        let start = out.len();
        let len = self.disk_len();
        out.resize(start + len, 0);
        let b = &mut out[start..];
        put64(b, 0, self.data);
        put16(b, 8, len as u16);
        put16(b, 10, self.key.len() as u16);
        let mut flags = 0;
        if self.child.is_some() {
            flags |= IE_HAS_CHILD;
        }
        if self.last {
            flags |= IE_LAST;
        }
        put16(b, 12, flags);
        b[16..16 + self.key.len()].copy_from_slice(&self.key);
        b[16 + self.key.len()..16 + self.key.len() + self.extra.len()].copy_from_slice(&self.extra);
        if let Some(c) = self.child {
            put64(b, len - 8, c);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// Entries including the terminating END entry (always last).
    pub entries: Vec<IEntry>,
}

impl Node {
    pub fn empty_leaf() -> Self {
        Node { entries: vec![IEntry::end(None)] }
    }
    pub fn real(&self) -> usize {
        self.entries.len().saturating_sub(1)
    }
    pub fn is_leaf(&self) -> bool {
        self.entries.last().map_or(true, |e| e.child.is_none())
    }
    pub fn entries_len(&self) -> usize {
        self.entries.iter().map(|e| e.disk_len()).sum()
    }
    /// Parse the node header at `hdr` (entries_offset / index_length relative to it).
    pub fn parse(b: &[u8], hdr: usize) -> FsResult<Node> {
        let eoff = le32(b, hdr)? as usize;
        let ilen = le32(b, hdr + 4)? as usize;
        let start = hdr.checked_add(eoff).ok_or_else(|| FsError::Corrupt("index".into()))?;
        let end = hdr.checked_add(ilen).ok_or_else(|| FsError::Corrupt("index".into()))?;
        if end > b.len() || start > end {
            return corrupt("index node header out of bounds");
        }
        let mut entries = Vec::new();
        let mut off = start;
        loop {
            if off + 16 > end {
                return corrupt("index entry past node end");
            }
            let data = le64(b, off)?;
            let elen = le16(b, off + 8)? as usize;
            let klen = le16(b, off + 10)? as usize;
            let flags = le16(b, off + 12)?;
            let has_child = flags & IE_HAS_CHILD != 0;
            let min = 16 + if has_child { 8 } else { 0 };
            if elen < min || off + elen > end || (flags & IE_LAST == 0 && 16 + klen > elen - if has_child { 8 } else { 0 }) {
                return corrupt("bad index entry length");
            }
            let child = if has_child { Some(le64(b, off + elen - 8)?) } else { None };
            if flags & IE_LAST != 0 {
                entries.push(IEntry::end(child));
                break;
            }
            let key = b[off + 16..off + 16 + klen].to_vec();
            entries.push(IEntry { data, key, extra: vec![], child, last: false });
            off += elen;
            if entries.len() > 100_000 {
                return corrupt("too many index entries");
            }
        }
        Ok(Node { entries })
    }
    fn write_entries(&self, out: &mut Vec<u8>) {
        for e in &self.entries {
            e.serialize(out);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct IndexRootHdr {
    pub ty: u32,
    pub collation: u32,
    pub block_size: u32,
    pub clusters_per_block: u8,
}

pub fn parse_index_root(v: &[u8]) -> FsResult<(IndexRootHdr, Node)> {
    let h = IndexRootHdr { ty: le32(v, 0)?, collation: le32(v, 4)?, block_size: le32(v, 8)?, clusters_per_block: byte(v, 12)? };
    let n = Node::parse(v, 16)?;
    Ok((h, n))
}

pub fn index_root_value(h: &IndexRootHdr, n: &Node) -> Vec<u8> {
    let mut v = vec![0u8; 32];
    put32(&mut v, 0, h.ty);
    put32(&mut v, 4, h.collation);
    put32(&mut v, 8, h.block_size);
    v[12] = h.clusters_per_block;
    n.write_entries(&mut v);
    let ilen = (v.len() - 16) as u32;
    put32(&mut v, 16, 16);
    put32(&mut v, 20, ilen);
    put32(&mut v, 24, ilen);
    put32(&mut v, 28, if n.is_leaf() { 0 } else { 1 });
    v
}

pub const INDX_HDR: usize = 0x18;

pub fn indx_entries_off(block_size: usize) -> usize {
    align8(0x28 + 2 * (1 + block_size / 512))
}

/// Capacity (bytes available for entries) of an INDX block.
pub fn indx_capacity(block_size: usize) -> usize {
    block_size - indx_entries_off(block_size)
}

/// Parse an INDX block (fixups already applied).
pub fn parse_indx(b: &[u8]) -> FsResult<(u64, Node)> {
    if b.len() < 0x28 || &b[0..4] != b"INDX" {
        return corrupt("bad INDX magic");
    }
    let vcn = le64(b, 0x10)?;
    Ok((vcn, Node::parse(b, INDX_HDR)?))
}

pub fn serialize_indx(n: &Node, vcn: u64, block_size: usize, usn: u16) -> Option<Vec<u8>> {
    let eoff = indx_entries_off(block_size);
    if n.entries_len() > block_size - eoff {
        return None;
    }
    let mut b = vec![0u8; eoff];
    b[0..4].copy_from_slice(b"INDX");
    put16(&mut b, 4, 0x28);
    put16(&mut b, 6, (1 + block_size / 512) as u16);
    put64(&mut b, 0x10, vcn);
    n.write_entries(&mut b);
    let ilen = b.len() - INDX_HDR;
    b.resize(block_size, 0);
    put32(&mut b, INDX_HDR, (eoff - INDX_HDR) as u32);
    put32(&mut b, INDX_HDR + 4, ilen as u32);
    put32(&mut b, INDX_HDR + 8, (block_size - INDX_HDR) as u32);
    put32(&mut b, INDX_HDR + 12, if n.is_leaf() { 0 } else { 1 });
    fixup_write(&mut b, usn);
    Some(b)
}

// ---------------------------------------------------------------------------------------------
// Security descriptors

pub fn sid(auth: u8, subs: &[u32]) -> Vec<u8> {
    let mut v = vec![1u8, subs.len() as u8, 0, 0, 0, 0, 0, auth];
    for s in subs {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}
pub fn sid_everyone() -> Vec<u8> {
    sid(1, &[0])
}
pub fn sid_system() -> Vec<u8> {
    sid(5, &[18])
}
pub fn sid_admins() -> Vec<u8> {
    sid(5, &[32, 544])
}
pub fn sid_users() -> Vec<u8> {
    sid(5, &[32, 545])
}
pub fn sid_auth_users() -> Vec<u8> {
    sid(5, &[11])
}

/// Self-relative security descriptor with a DACL of ACCESS_ALLOWED ACEs `(flags, mask, sid)`.
pub fn build_sd(owner: &[u8], group: &[u8], aces: &[(u8, u32, Vec<u8>)]) -> Vec<u8> {
    let mut acl = vec![2u8, 0, 0, 0, 0, 0, 0, 0];
    for (flags, mask, s) in aces {
        let size = 8 + s.len();
        acl.push(0); // ACCESS_ALLOWED_ACE_TYPE
        acl.push(*flags);
        acl.extend_from_slice(&(size as u16).to_le_bytes());
        acl.extend_from_slice(&mask.to_le_bytes());
        acl.extend_from_slice(s);
    }
    let acl_len = acl.len() as u16;
    put16(&mut acl, 2, acl_len);
    put16(&mut acl, 4, aces.len() as u16);
    let mut sd = vec![0u8; 20];
    sd[0] = 1;
    put16(&mut sd, 2, 0x8004); // SE_SELF_RELATIVE | SE_DACL_PRESENT
    let dacl_off = 20;
    let owner_off = dacl_off + acl.len();
    let group_off = owner_off + owner.len();
    put32(&mut sd, 4, owner_off as u32);
    put32(&mut sd, 8, group_off as u32);
    put32(&mut sd, 16, dacl_off as u32);
    sd.extend_from_slice(&acl);
    sd.extend_from_slice(owner);
    sd.extend_from_slice(group);
    sd
}

/// "Everyone: full control" descriptor, as ntfs-3g attaches to new files.
pub fn sd_everyone() -> Vec<u8> {
    build_sd(&sid_admins(), &sid_admins(), &[(0x03, 0x001F_01FF, sid_everyone())])
}

pub fn hex(s: &str) -> Vec<u8> {
    let s: Vec<u8> = s.bytes().filter(|c| c.is_ascii_hexdigit()).collect();
    s.chunks(2)
        .map(|c| {
            let h = |x: u8| (x as char).to_digit(16).unwrap_or(0) as u8;
            h(c[0]) << 4 | h(*c.get(1).unwrap_or(&b'0'))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntfs_runlist_roundtrip() {
        let runs = vec![
            Run { vcn: 0, lcn: Some(0x1234), len: 0x80 },
            Run { vcn: 0x80, lcn: None, len: 16 },
            Run { vcn: 0x90, lcn: Some(0x20), len: 1 },
            Run { vcn: 0x91, lcn: Some(0x1_0000_0000), len: 0x7FFF },
        ];
        let enc = encode_runs(&runs);
        assert_eq!(decode_runs(&enc, 0).unwrap(), runs);
        assert!(decode_runs(&[0x11, 5], 0).is_err());
        assert!(decode_runs(&[0x99, 0, 0], 0).is_err());
    }

    #[test]
    fn ntfs_time_roundtrip() {
        let t = Timestamp { secs: 1_600_000_000, nsecs: 123_456_700 };
        assert_eq!(ntfs_to_ts(ts_to_ntfs(t)), t);
    }

    #[test]
    fn ntfs_record_roundtrip() {
        let mut r = MftRecord::new(3, MFT_IN_USE);
        r.add_attr(Attribute::resident(AT_STANDARD_INFORMATION, &[], StdInfo::default().new_value(0)));
        r.add_attr(Attribute::nonresident(AT_DATA, &[], vec![Run { vcn: 0, lcn: Some(100), len: 4 }], 15000, 4096));
        let mut b = r.serialize(77, 1024).unwrap();
        fixup_read(&mut b).unwrap();
        let p = MftRecord::parse(&b).unwrap();
        assert_eq!(p.attrs.len(), 2);
        assert_eq!(p.attrs[1].nonres().unwrap().size, 15000);
        assert_eq!(p.attrs[1].nonres().unwrap().runs, vec![Run { vcn: 0, lcn: Some(100), len: 4 }]);
        // corrupt input must not panic
        for i in 0..b.len() {
            let mut c = b.clone();
            c[i] ^= 0xFF;
            let _ = MftRecord::parse(&c);
            let _ = fixup_read(&mut c);
        }
    }
}
