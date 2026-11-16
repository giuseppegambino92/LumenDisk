//! UDF image builder for tests (ECMA-167 3rd edition / OSTA UDF 1.02 – 2.60).
//!
//! Produces complete, kernel-mountable images: Volume Recognition Sequence, Anchor Volume
//! Descriptor Pointers at 256 and N-1, main + reserve Volume Descriptor Sequences (PVD, IUVD, PD,
//! LVD, USD, TD), a closed Logical Volume Integrity Descriptor, the File Set Descriptor, File
//! Entries (UDF < 2.00) or Extended File Entries, File Identifier Descriptors, and for UDF 2.50+
//! a Metadata Partition (metadata file + mirror, UDF 2.2.10 / 2.2.13). Files can use short, long
//! or extended allocation descriptors or embedded data, be fragmented (forcing Allocation Extent
//! Descriptors), be sparse (not-recorded extents, for files > 4 GiB), and ICBs can use strategy
//! 4096 with Indirect Entries. The partition is read-only (access type 1), so no space bitmaps.

use std::collections::HashMap;

/// Allocation descriptor flavour of a file (ECMA-167 4/14.14, 4/14.6.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdKind {
    Short,
    Long,
    Ext,
    /// Embedded in the file entry (only when it fits; otherwise falls back to the default).
    Inline,
}

/// Where the interesting structures of a built image are (absolute byte offsets), so tests can
/// damage them.
#[derive(Clone, Debug, Default)]
pub struct UdfMap {
    pub block_size: u32,
    /// Absolute sector of the partition start.
    pub partition_start: u64,
    /// The (E)FE of each path ("/" = root), in the main metadata area for 2.50+.
    pub icb: HashMap<String, u64>,
    /// Logical block number of each path's ICB within its partition (as FIDs record it).
    pub icb_lbn: HashMap<String, u32>,
    /// Directory data (FIDs) of directories not embedded in their ICB.
    pub dir_data: HashMap<String, u64>,
    pub meta_file: Option<u64>,
    pub mirror_file: Option<u64>,
    /// Number of Allocation Extent Descriptors written, and the first one.
    pub aeds: usize,
    pub first_aed: Option<u64>,
    /// End of the region holding descriptors, ICBs and directories (file data mostly follows).
    pub hot_end: u64,
}

#[derive(Clone, Debug)]
enum Kind {
    Dir,
    File(Vec<u8>),
    Sparse { size: u64, pieces: Vec<(u64, Vec<u8>)> },
    Link(String),
}

#[derive(Clone, Debug)]
struct Ent {
    name: String,
    parent: usize,
    children: Vec<usize>,
    kind: Kind,
    ad: Option<AdKind>,
}

/// Builder. Paths are absolute ("/a/b.txt"); missing parent directories are created.
#[derive(Clone, Debug)]
pub struct UdfBuilder {
    rev: u16,
    bs: u32,
    label: String,
    metadata: bool,
    fragment: Option<u32>,
    dirs_inline: bool,
    indirect: bool,
    mtime: i64,
    ents: Vec<Ent>,
}

/// Space a block is allocated in: the ICB partition (metadata partition for 2.50+ images with
/// one) or the data space (physical partition).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sp {
    Icb,
    Data,
}

#[derive(Clone, Copy, Debug)]
struct Loc {
    sp: Sp,
    lbn: u32,
}

/// One extent of a file being laid out.
#[derive(Clone, Debug)]
struct Ext {
    /// 0 recorded, 1 allocated-not-recorded, 2 not allocated (4/14.14.1.1).
    kind: u8,
    len: u32,
    loc: Option<Loc>,
    /// Bytes to write there (recorded extents).
    bytes: Vec<u8>,
}

/// Layout of one node.
#[derive(Clone, Debug, Default)]
struct Lay {
    icb: Option<Loc>,
    /// Strategy 4096: the stale entry, the IE and the current entry.
    indirect: bool,
    ad: Option<AdKind>,
    inline: Option<Vec<u8>>,
    exts: Vec<Ext>,
    aeds: Vec<Loc>,
    size: u64,
    uid: u64,
}

const MAX_EXT: u64 = (1 << 30) - 4096;

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// Fill the 16-byte descriptor tag (3/7.2): CRC over `crc_len` bytes after it, then checksum.
fn tag(b: &mut [u8], id: u16, version: u16, loc: u32, crc_len: usize) {
    put16(b, 0, id);
    put16(b, 2, version);
    b[5] = 0;
    put16(b, 6, 1); // serial
    let crc = crate::iso::udf::crc_itu_t(&b[16..16 + crc_len]);
    put16(b, 8, crc);
    put16(b, 10, crc_len as u16);
    put32(b, 12, loc);
    b[4] = 0;
    b[4] = b[..16].iter().fold(0u8, |s, &x| s.wrapping_add(x));
}

/// OSTA CS0 (UDF 2.1.1): compression id 8 when every character fits a byte, else 16.
fn cs0(s: &str) -> Vec<u8> {
    if s.chars().all(|c| (c as u32) < 256) {
        let mut v = vec![8u8];
        v.extend(s.chars().map(|c| c as u8));
        v
    } else {
        let mut v = vec![16u8];
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_be_bytes());
        }
        v
    }
}

/// dstring of `field` bytes (1/7.2.12).
fn dstring(b: &mut [u8], s: &str) {
    let n = b.len();
    if s.is_empty() {
        return;
    }
    let mut c = cs0(s);
    c.truncate(n - 1);
    b[..c.len()].copy_from_slice(&c);
    b[n - 1] = c.len() as u8;
}

/// charspec CS0 "OSTA Compressed Unicode" (UDF 2.1.2).
fn charspec(b: &mut [u8]) {
    b[0] = 0;
    b[1..24].copy_from_slice(b"OSTA Compressed Unicode");
}

/// regid (1/7.4) with an identifier and suffix.
fn regid(b: &mut [u8], ident: &str, suffix: &[u8]) {
    b[0] = 0;
    let id = ident.as_bytes();
    b[1..1 + id.len().min(23)].copy_from_slice(&id[..id.len().min(23)]);
    b[24..24 + suffix.len().min(8)].copy_from_slice(&suffix[..suffix.len().min(8)]);
}

/// Timestamp (1/7.3) of `t` expressed as local time at UTC+01:00.
fn timestamp(b: &mut [u8], t: i64) {
    let local = t + 3600;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    put16(b, 0, 0x1000 | 60);
    put16(b, 2, y as u16);
    b[4] = m as u8;
    b[5] = d as u8;
    b[6] = (secs / 3600) as u8;
    b[7] = (secs / 60 % 60) as u8;
    b[8] = (secs % 60) as u8;
}

fn perms(mode: u32) -> u32 {
    // 4/14.9.5: other bits 0–4, group 5–9, owner 10–14 (execute, write, read, attr, delete).
    let o = mode & 7;
    let g = (mode >> 3) & 7;
    let u = (mode >> 6) & 7;
    o | 0x18 | (g | 0x18) << 5 | (u | 0x18) << 10
}

impl UdfBuilder {
    /// `revision`: 0x0102, 0x0150, 0x0200, 0x0201, 0x0250, 0x0260. `block_size`: 512 – 4096.
    pub fn new(revision: u16, block_size: u32) -> Self {
        assert!(matches!(block_size, 512 | 1024 | 2048 | 4096));
        let root = Ent { name: String::new(), parent: 0, children: vec![], kind: Kind::Dir, ad: None };
        UdfBuilder { rev: revision, bs: block_size, label: "LumenDisk UDF".into(), metadata: revision >= 0x0250, fragment: None, dirs_inline: true, indirect: false, mtime: 1_700_000_000, ents: vec![root] }
    }
    pub fn label(&mut self, l: &str) -> &mut Self {
        self.label = l.into();
        self
    }
    /// Use a metadata partition (UDF 2.50+ only; default on for 2.50+).
    pub fn metadata_partition(&mut self, on: bool) -> &mut Self {
        self.metadata = on;
        self
    }
    /// Split file data into extents of at most `blocks` blocks, with a gap between them.
    pub fn fragment(&mut self, blocks: u32) -> &mut Self {
        self.fragment = Some(blocks.max(1));
        self
    }
    /// Embed small directories in their ICB (default true).
    pub fn dirs_inline(&mut self, on: bool) -> &mut Self {
        self.dirs_inline = on;
        self
    }
    /// Regular files get strategy-4096 ICBs: a stale entry followed by an Indirect Entry
    /// pointing at the current one.
    pub fn indirect_icbs(&mut self, on: bool) -> &mut Self {
        self.indirect = on;
        self
    }

    fn ensure_dir(&mut self, path: &str) -> usize {
        let mut cur = 0usize;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur = match self.ents[cur].children.iter().copied().find(|&c| self.ents[c].name == comp) {
                Some(c) => c,
                None => self.add(cur, comp, Kind::Dir, None),
            };
        }
        cur
    }
    fn add(&mut self, parent: usize, name: &str, kind: Kind, ad: Option<AdKind>) -> usize {
        let i = self.ents.len();
        self.ents.push(Ent { name: name.into(), parent, children: vec![], kind, ad });
        self.ents[parent].children.push(i);
        i
    }
    fn add_path(&mut self, path: &str, kind: Kind, ad: Option<AdKind>) {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let p = self.ensure_dir(dir);
        self.add(p, name, kind, ad);
    }
    pub fn dir(&mut self, path: &str) -> &mut Self {
        self.ensure_dir(path);
        self
    }
    pub fn file(&mut self, path: &str, data: Vec<u8>) -> &mut Self {
        self.add_path(path, Kind::File(data), None);
        self
    }
    pub fn file_with(&mut self, path: &str, data: Vec<u8>, ad: AdKind) -> &mut Self {
        self.add_path(path, Kind::File(data), Some(ad));
        self
    }
    /// A file of `size` bytes, zero except `recorded` pieces (file offset, bytes); offsets are
    /// rounded down to the block size. Gaps become not-recorded extents.
    pub fn sparse_file(&mut self, path: &str, size: u64, recorded: Vec<(u64, Vec<u8>)>) -> &mut Self {
        self.add_path(path, Kind::Sparse { size, pieces: recorded }, None);
        self
    }
    pub fn symlink(&mut self, path: &str, target: &str) -> &mut Self {
        self.add_path(path, Kind::Link(target.into()), None);
        self
    }

    pub fn build(&self) -> Vec<u8> {
        self.build_with_map().0
    }

    fn efe(&self) -> bool {
        self.rev >= 0x0200
    }
    fn meta(&self) -> bool {
        self.metadata && self.rev >= 0x0250
    }
    fn version(&self) -> u16 {
        if self.rev >= 0x0200 {
            3
        } else {
            2
        }
    }
    fn base(&self) -> usize {
        if self.efe() {
            216
        } else {
            176
        }
    }

    fn path_of(&self, mut i: usize) -> String {
        if i == 0 {
            return "/".into();
        }
        let mut parts = vec![];
        while i != 0 {
            parts.push(self.ents[i].name.clone());
            i = self.ents[i].parent;
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    fn fid_len(name: &str) -> usize {
        (38 + if name.is_empty() { 0 } else { cs0(name).len() } + 3) & !3
    }

    /// Symlink path components (4/14.16).
    fn link_data(target: &str) -> Vec<u8> {
        let mut v = vec![];
        if target.starts_with('/') {
            v.extend_from_slice(&[2, 0, 0, 0]);
        }
        for c in target.split('/').filter(|c| !c.is_empty()) {
            match c {
                "." => v.extend_from_slice(&[4, 0, 0, 0]),
                ".." => v.extend_from_slice(&[3, 0, 0, 0]),
                name => {
                    let n = cs0(name);
                    v.extend_from_slice(&[5, n.len() as u8, 0, 0]);
                    v.extend_from_slice(&n);
                }
            }
        }
        v
    }

    pub fn build_with_map(&self) -> (Vec<u8>, UdfMap) {
        let bs = self.bs as u64;
        let meta = self.meta();
        let ad_size = |k: AdKind| match k {
            AdKind::Short => 8usize,
            AdKind::Long => 16,
            AdKind::Ext => 20,
            AdKind::Inline => 0,
        };
        let default_ad = if meta { AdKind::Long } else { AdKind::Short };
        // ── pass 1: allocate ──
        let mut next = [0u32; 2]; // [icb space, data space]
        let mut alloc = |sp: Sp, n: u64| -> Loc {
            let sp = if meta { sp } else { Sp::Icb };
            let k = (sp == Sp::Data) as usize;
            let l = Loc { sp, lbn: next[k] };
            next[k] += n as u32;
            l
        };
        let fsd = alloc(Sp::Icb, 1);
        debug_assert_eq!(fsd.lbn, 0);
        let mut lay: Vec<Lay> = vec![Lay::default(); self.ents.len()];
        // Breadth-first order.
        let mut order = vec![0usize];
        let mut qi = 0;
        while qi < order.len() {
            let i = order[qi];
            qi += 1;
            order.extend(self.ents[i].children.iter().copied());
        }
        let cap_icb = (bs as usize) - self.base();
        for &i in &order {
            let e = &self.ents[i];
            let l = &mut lay[i];
            l.uid = if i == 0 { 0 } else { 15 + i as u64 };
            let is_file = matches!(e.kind, Kind::File(_) | Kind::Sparse { .. });
            l.indirect = self.indirect && is_file;
            l.icb = Some(alloc(Sp::Icb, if l.indirect { 3 } else { 1 }));
            // Content to place.
            let (bytes, sp_default): (Option<Vec<u8>>, AdKind) = match &e.kind {
                Kind::Dir => {
                    let n: usize = Self::fid_len("") + e.children.iter().map(|&c| Self::fid_len(&self.ents[c].name)).sum::<usize>();
                    l.size = n as u64;
                    if self.dirs_inline && n <= cap_icb {
                        l.inline = Some(vec![]); // filled in pass 2
                        continue;
                    }
                    l.ad = Some(AdKind::Short);
                    let loc = alloc(Sp::Icb, (n as u64).div_ceil(bs));
                    l.exts.push(Ext { kind: 0, len: n as u32, loc: Some(loc), bytes: vec![] });
                    (None, AdKind::Short)
                }
                Kind::Link(t) => {
                    let d = Self::link_data(t);
                    l.size = d.len() as u64;
                    if d.len() <= cap_icb {
                        l.inline = Some(d);
                        continue;
                    }
                    (Some(d), AdKind::Short)
                }
                Kind::File(d) => {
                    l.size = d.len() as u64;
                    let want = e.ad.unwrap_or(default_ad);
                    if want == AdKind::Inline && d.len() <= cap_icb {
                        l.inline = Some(d.clone());
                        continue;
                    }
                    (Some(d.clone()), if want == AdKind::Inline { default_ad } else { want })
                }
                Kind::Sparse { size, .. } => {
                    l.size = *size;
                    (None, e.ad.filter(|&a| a != AdKind::Inline).unwrap_or(default_ad))
                }
            };
            let ad = sp_default;
            l.ad = Some(ad);
            // Short ADs address the ICB's partition; long / ext ADs the data space.
            let sp = if ad == AdKind::Short { Sp::Icb } else { Sp::Data };
            let frag = self.fragment.map(|f| f as u64 * bs).unwrap_or(MAX_EXT).min(MAX_EXT);
            let mut place = |data: &[u8], exts: &mut Vec<Ext>| {
                let mut off = 0usize;
                while off < data.len() {
                    let n = (data.len() - off).min(frag as usize);
                    let loc = alloc(sp, (n as u64).div_ceil(bs));
                    if self.fragment.is_some() {
                        alloc(sp, 1); // gap: extents are not contiguous
                    }
                    exts.push(Ext { kind: 0, len: n as u32, loc: Some(loc), bytes: data[off..off + n].to_vec() });
                    off += n;
                }
            };
            match (&e.kind, bytes) {
                (Kind::Sparse { size, pieces }, _) => {
                    let mut pieces: Vec<(u64, Vec<u8>)> = pieces.iter().map(|(o, d)| (o / bs * bs, { let mut v = vec![0u8; (o % bs) as usize]; v.extend_from_slice(d); v })).collect();
                    pieces.sort_by_key(|p| p.0);
                    let mut pos = 0u64;
                    let mut hole_kind = 2u8;
                    for (o, mut d) in pieces {
                        if o < pos || o >= *size {
                            continue;
                        }
                        // Pad to whole blocks (zeros = the hole's content) unless at the end.
                        let room = size - o;
                        let padded = (d.len() as u64).div_ceil(bs) * bs;
                        d.resize(padded.min(room) as usize, 0);
                        let mut hole = o - pos;
                        while hole > 0 {
                            let n = hole.min(MAX_EXT);
                            l.exts.push(Ext { kind: hole_kind, len: n as u32, loc: None, bytes: vec![] });
                            hole_kind = 3 - hole_kind; // alternate types 2 and 1
                            hole -= n;
                        }
                        place(&d, &mut l.exts);
                        pos = o + d.len() as u64;
                    }
                    let mut hole = size - pos;
                    while hole > 0 {
                        let n = hole.min(MAX_EXT);
                        l.exts.push(Ext { kind: 2, len: n as u32, loc: None, bytes: vec![] });
                        hole -= n;
                    }
                }
                (_, Some(d)) => place(&d, &mut l.exts),
                _ => {}
            }
            // Allocation Extent Descriptors when the ADs don't fit the ICB.
            let asz = ad_size(ad);
            let n = l.exts.len();
            let cap0 = cap_icb / asz;
            let cap_a = (bs as usize - 24) / asz;
            if n > cap0 {
                let mut left = n - (cap0 - 1);
                loop {
                    l.aeds.push(alloc(Sp::Icb, 1));
                    if left <= cap_a {
                        break;
                    }
                    left -= cap_a - 1;
                }
            }
        }
        let m_blocks = next[0] as u64;
        let d_blocks = next[1] as u64;

        // ── physical layout ──
        let part_start: u64 = 257;
        let stride = bs.max(2048);
        let vds_main = (32768 + 3 * stride).div_ceil(bs).div_ceil(16) * 16;
        let vds_res = vds_main + 16;
        let lvid_at = vds_res + 16;
        assert!(lvid_at + 2 < 256);
        // Metadata area: two runs with a one-block gap, then the mirror; then data.
        let half = m_blocks.div_ceil(2);
        let (meta_runs, mirror_at, data_base, plen) = if meta {
            let runs = vec![(0u64, half, 2u64), (half, m_blocks - half, 2 + half + 1)];
            let mirror = 2 + m_blocks + 1;
            let data_base = mirror + m_blocks;
            (runs, mirror, data_base, data_base + d_blocks)
        } else {
            (vec![(0, m_blocks, 0)], 0, 0, m_blocks)
        };
        let plen = plen.max(64);
        let total = part_start + plen + 1;
        let mut img = vec![0u8; (total * bs) as usize];
        let mut map = UdfMap { block_size: self.bs, partition_start: part_start, ..Default::default() };
        let icb_ref: u16 = if meta { 1 } else { 0 };
        // Absolute byte offset of a block in a space (main metadata copy).
        let abs = |l: Loc| -> u64 {
            match l.sp {
                Sp::Data => (part_start + data_base + l.lbn as u64) * bs,
                Sp::Icb => {
                    let b = l.lbn as u64;
                    let (fb, _, phys) = meta_runs.iter().copied().find(|&(fb, n, _)| b >= fb && b < fb + n).unwrap_or((0, 0, 0));
                    (part_start + phys + (b - fb)) * bs
                }
            }
        };
        // (partref, lbn) as recorded in long/ext ADs.
        let addr = |l: Loc| -> (u16, u32) {
            match l.sp {
                Sp::Icb => (icb_ref, l.lbn),
                Sp::Data => (0, (data_base + l.lbn as u64) as u32),
            }
        };
        let ver = self.version();
        let suffix_dom = { let r = self.rev.to_le_bytes(); [r[0], r[1], 0] };
        let suffix_udf = { let r = self.rev.to_le_bytes(); [r[0], r[1], 6, 0] };

        // ── VRS (2/9.1) ──
        let nsr: &[u8; 5] = if self.rev >= 0x0200 { b"NSR03" } else { b"NSR02" };
        for (k, id) in [b"BEA01", nsr, b"TEA01"].iter().enumerate() {
            let o = (32768 + k as u64 * stride) as usize;
            img[o + 1..o + 6].copy_from_slice(*id);
            img[o + 6] = 1;
        }

        // ── file structures (pass 2) ──
        let write_block = |img: &mut Vec<u8>, at: u64, b: &[u8]| img[at as usize..at as usize + b.len()].copy_from_slice(b);
        // FSD (4/14.1).
        {
            let mut b = vec![0u8; bs as usize];
            timestamp(&mut b[16..28], self.mtime);
            put16(&mut b, 28, 3);
            put16(&mut b, 30, 3);
            put32(&mut b, 32, 1);
            put32(&mut b, 36, 1);
            charspec(&mut b[48..112]);
            dstring(&mut b[112..240], &self.label);
            charspec(&mut b[240..304]);
            dstring(&mut b[304..336], &self.label);
            let root = lay[0].icb.unwrap();
            put32(&mut b, 400, self.bs);
            put32(&mut b, 404, root.lbn);
            put16(&mut b, 408, icb_ref);
            regid(&mut b[416..448], "*OSTA UDF Compliant", &suffix_dom);
            tag(&mut b, 256, ver, 0, 512 - 16);
            write_block(&mut img, abs(fsd), &b);
        }
        let mut n_files = 0u32;
        let mut n_dirs = 0u32;
        for &i in &order {
            let e = &self.ents[i];
            let l = &lay[i];
            let path = self.path_of(i);
            let icb = l.icb.unwrap();
            let real = if l.indirect { Loc { sp: Sp::Icb, lbn: icb.lbn + 2 } } else { icb };
            // Directory content: parent FID + children (4/14.4).
            let mut content: Option<Vec<u8>> = None;
            if let Kind::Dir = e.kind {
                n_dirs += 1;
                let mut d: Vec<u8> = vec![];
                let parent_icb = lay[e.parent].icb.unwrap();
                let fid = |name: &str, chars: u8, target: Loc, uid: u64, d: &mut Vec<Vec<u8>>| {
                    let id = if name.is_empty() { vec![] } else { cs0(name) };
                    let len = (38 + id.len() + 3) & !3;
                    let mut f = vec![0u8; len];
                    put16(&mut f, 16, 1);
                    f[18] = chars;
                    f[19] = id.len() as u8;
                    put32(&mut f, 20, self.bs);
                    put32(&mut f, 24, target.lbn);
                    put16(&mut f, 28, icb_ref);
                    put32(&mut f, 32, uid as u32); // UDF 2.3.4.3: unique id in the impl use
                    f[38..38 + id.len()].copy_from_slice(&id);
                    d.push(f);
                };
                let mut fids: Vec<Vec<u8>> = vec![];
                fid("", 0x0A, parent_icb, lay[e.parent].uid, &mut fids);
                for &c in &e.children {
                    let ch = &self.ents[c];
                    let chars = if matches!(ch.kind, Kind::Dir) { 0x02 } else { 0 };
                    fid(&ch.name, chars, lay[c].icb.unwrap(), lay[c].uid, &mut fids);
                }
                // Tag each FID with the block (of its partition) it lies in.
                let dir_start = l.exts.first().and_then(|x| x.loc).unwrap_or(real);
                for mut f in fids {
                    let pos = d.len() as u64;
                    let blk = if l.inline.is_some() { real.lbn } else { dir_start.lbn + (pos / bs) as u32 };
                    let len = f.len();
                    tag(&mut f, 257, ver, blk, len - 16);
                    d.extend_from_slice(&f);
                }
                content = Some(d);
            } else {
                n_files += 1;
            }
            // Embedded data, or ADs (+ AEDs) and the extents' bytes.
            let (ad_type, ad_area): (u16, Vec<u8>) = if let Some(inl) = &l.inline {
                (3, if let Some(c) = &content { c.clone() } else { inl.clone() })
            } else if let Some(ad) = l.ad {
                let asz = ad_size(ad);
                let mut descs: Vec<Vec<u8>> = vec![];
                for x in &l.exts {
                    let mut d = vec![0u8; asz];
                    put32(&mut d, 0, (x.kind as u32) << 30 | x.len);
                    let (pref, lbn) = x.loc.map(addr).unwrap_or((0, 0));
                    match ad {
                        AdKind::Short => put32(&mut d, 4, x.loc.map(|l| l.lbn).unwrap_or(0)),
                        AdKind::Long => {
                            put32(&mut d, 4, lbn);
                            put16(&mut d, 8, pref);
                        }
                        _ => {
                            put32(&mut d, 4, if x.kind == 0 { x.len } else { 0 });
                            put32(&mut d, 8, x.len);
                            put32(&mut d, 12, lbn);
                            put16(&mut d, 16, pref);
                        }
                    }
                    descs.push(d);
                    // Recorded bytes (directories: their FIDs, which span the extent).
                    if let Some(loc) = x.loc {
                        let src: &[u8] = content.as_deref().unwrap_or(&x.bytes);
                        if content.is_some() {
                            map.dir_data.insert(path.clone(), abs(loc));
                        }
                        for (bi, piece) in src.chunks(bs as usize).enumerate() {
                            write_block(&mut img, abs(Loc { sp: loc.sp, lbn: loc.lbn + bi as u32 }), piece);
                        }
                    }
                }
                // Split into the ICB area and AEDs.
                let cap0 = cap_icb / asz;
                let cap_a = (bs as usize - 24) / asz;
                let cont = |to: Loc| -> Vec<u8> {
                    let mut d = vec![0u8; asz];
                    let area = bs as u32;
                    put32(&mut d, 0, 3 << 30 | area);
                    match ad {
                        AdKind::Short => put32(&mut d, 4, to.lbn),
                        AdKind::Long => {
                            put32(&mut d, 4, to.lbn);
                            put16(&mut d, 8, icb_ref);
                        }
                        _ => {
                            put32(&mut d, 8, area);
                            put32(&mut d, 12, to.lbn);
                            put16(&mut d, 16, icb_ref);
                        }
                    }
                    d
                };
                let mut rest: &[Vec<u8>] = &descs;
                let mut first: Vec<u8> = vec![];
                if l.aeds.is_empty() {
                    for d in rest {
                        first.extend_from_slice(d);
                    }
                } else {
                    for d in &rest[..cap0 - 1] {
                        first.extend_from_slice(d);
                    }
                    first.extend_from_slice(&cont(l.aeds[0]));
                    rest = &rest[cap0 - 1..];
                    let mut prev = real.lbn;
                    for (ai, &a) in l.aeds.iter().enumerate() {
                        let last = ai + 1 == l.aeds.len();
                        let take = if last { rest.len() } else { cap_a - 1 };
                        let mut area = vec![];
                        for d in &rest[..take] {
                            area.extend_from_slice(d);
                        }
                        if !last {
                            area.extend_from_slice(&cont(l.aeds[ai + 1]));
                        }
                        rest = &rest[take..];
                        let mut b = vec![0u8; bs as usize];
                        put32(&mut b, 16, prev);
                        put32(&mut b, 20, area.len() as u32);
                        b[24..24 + area.len()].copy_from_slice(&area);
                        tag(&mut b, 258, ver, a.lbn, 8 + area.len());
                        let at = abs(a);
                        map.aeds += 1;
                        map.first_aed.get_or_insert(at);
                        write_block(&mut img, at, &b);
                        prev = a.lbn;
                    }
                }
                (match ad {
                    AdKind::Short => 0,
                    AdKind::Long => 1,
                    _ => 2,
                }, first)
            } else {
                (3, vec![])
            };
            let (ftype, mode) = match e.kind {
                Kind::Dir => (4u8, 0o755),
                Kind::Link(_) => (12, 0o777),
                _ => (5, 0o644),
            };
            let recorded: u64 = l.exts.iter().filter(|x| x.kind == 0).map(|x| (x.len as u64).div_ceil(bs)).sum();
            let links = if let Kind::Dir = e.kind { 1 + e.children.iter().filter(|&&c| matches!(self.ents[c].kind, Kind::Dir)).count() as u16 } else { 1 };
            let strategy = if l.indirect { 4096 } else { 4 };
            let fe = self.file_entry(real.lbn, ftype, mode, l.size, recorded, l.uid, links, ad_type, &ad_area, strategy, lay[e.parent].icb.unwrap().lbn, icb_ref);
            let at = abs(real);
            write_block(&mut img, at, &fe);
            if l.indirect {
                // Stale entry at icb (empty file), Indirect Entry at icb+1 → current at icb+2.
                let stale = self.file_entry(icb.lbn, ftype, mode, 0, 0, l.uid, 1, 3, &[], 4096, 0, icb_ref);
                write_block(&mut img, abs(icb), &stale);
                let mut ie = vec![0u8; bs as usize];
                put16(&mut ie, 20, 4096);
                put16(&mut ie, 24, 2);
                ie[27] = 3; // file type: indirect entry
                put32(&mut ie, 36, self.bs);
                put32(&mut ie, 40, real.lbn);
                put16(&mut ie, 44, icb_ref);
                tag(&mut ie, 259, ver, icb.lbn + 1, 52 - 16);
                write_block(&mut img, abs(Loc { sp: Sp::Icb, lbn: icb.lbn + 1 }), &ie);
            }
            map.icb.insert(path.clone(), at);
            map.icb_lbn.insert(path.clone(), icb.lbn);
        }

        // ── metadata file + mirror (UDF 2.2.13), physical partition blocks 0 and 1 ──
        if meta {
            for (k, ftype) in [(0u32, 250u8), (1, 251)] {
                let mut ads = vec![];
                let runs: Vec<(u64, u64)> = if k == 0 { meta_runs.iter().map(|&(_, n, p)| (n, p)).collect() } else { vec![(m_blocks, mirror_at)] };
                for (n, p) in runs {
                    if n == 0 {
                        continue;
                    }
                    let mut d = vec![0u8; 8];
                    put32(&mut d, 0, (n * bs) as u32);
                    put32(&mut d, 4, p as u32);
                    ads.extend_from_slice(&d);
                }
                let fe = self.file_entry(k, ftype, 0, m_blocks * bs, m_blocks, 0, 1, 0, &ads, 4, 0, 0);
                let at = (part_start + k as u64) * bs;
                write_block(&mut img, at, &fe);
                if k == 0 {
                    map.meta_file = Some(at);
                } else {
                    map.mirror_file = Some(at);
                }
            }
            // Mirror area: a copy of the metadata partition's blocks.
            for b in 0..m_blocks {
                let src = abs(Loc { sp: Sp::Icb, lbn: b as u32 }) as usize;
                let dst = ((part_start + mirror_at + b) * bs) as usize;
                let blk = img[src..src + bs as usize].to_vec();
                img[dst..dst + bs as usize].copy_from_slice(&blk);
            }
        }
        map.hot_end = if meta { (part_start + data_base) * bs } else { (part_start + m_blocks) * bs };

        // ── volume descriptors ──
        let blk = |n: u64| (n * bs) as usize;
        let vd_count = 6u64;
        for start in [vds_main, vds_res] {
            // PVD (3/10.1).
            let mut b = vec![0u8; bs as usize];
            put32(&mut b, 16, 0);
            dstring(&mut b[24..56], &self.label);
            put16(&mut b, 56, 1);
            put16(&mut b, 58, 1);
            put16(&mut b, 60, 2);
            put16(&mut b, 62, 3);
            put32(&mut b, 64, 1);
            put32(&mut b, 68, 1);
            dstring(&mut b[72..200], "0123456789ABCDEFLumenDisk");
            charspec(&mut b[200..264]);
            charspec(&mut b[264..328]);
            timestamp(&mut b[376..388], self.mtime);
            regid(&mut b[388..420], "*LumenDisk", &[]);
            tag(&mut b, 1, ver, start as u32, 512 - 16);
            img[blk(start)..blk(start + 1)].copy_from_slice(&b);
            // IUVD (3/10.4, UDF 2.2.7).
            let mut b = vec![0u8; bs as usize];
            put32(&mut b, 16, 1);
            regid(&mut b[20..52], "*UDF LV Info", &suffix_udf);
            charspec(&mut b[52..116]);
            dstring(&mut b[116..244], &self.label);
            regid(&mut b[352..384], "*LumenDisk", &[]);
            tag(&mut b, 4, ver, (start + 1) as u32, 512 - 16);
            img[blk(start + 1)..blk(start + 2)].copy_from_slice(&b);
            // PD (3/10.5).
            let mut b = vec![0u8; bs as usize];
            put32(&mut b, 16, 2);
            put16(&mut b, 20, 1);
            put16(&mut b, 22, 0);
            regid(&mut b[24..56], if self.rev >= 0x0200 { "+NSR03" } else { "+NSR02" }, &[]);
            put32(&mut b, 184, 1); // read-only
            put32(&mut b, 188, part_start as u32);
            put32(&mut b, 192, plen as u32);
            regid(&mut b[196..228], "*LumenDisk", &[]);
            tag(&mut b, 5, ver, (start + 2) as u32, 512 - 16);
            img[blk(start + 2)..blk(start + 3)].copy_from_slice(&b);
            // LVD (3/10.6) with its partition maps (3/10.7, UDF 2.2.10).
            let mut b = vec![0u8; bs as usize];
            put32(&mut b, 16, 3);
            charspec(&mut b[20..84]);
            dstring(&mut b[84..212], &self.label);
            put32(&mut b, 212, self.bs);
            regid(&mut b[216..248], "*OSTA UDF Compliant", &suffix_dom);
            put32(&mut b, 248, self.bs);
            put32(&mut b, 252, fsd.lbn);
            put16(&mut b, 256, icb_ref);
            regid(&mut b[272..304], "*LumenDisk", &[]);
            put32(&mut b, 432, (2 * bs) as u32);
            put32(&mut b, 436, lvid_at as u32);
            let mut maps = vec![1u8, 6, 1, 0, 0, 0];
            if meta {
                let mut m = vec![0u8; 64];
                m[0] = 2;
                m[1] = 64;
                regid(&mut m[4..36], "*UDF Metadata Partition", &suffix_udf);
                put16(&mut m, 36, 1);
                put16(&mut m, 38, 0);
                put32(&mut m, 40, 0);
                put32(&mut m, 44, 1);
                put32(&mut m, 48, u32::MAX);
                put32(&mut m, 52, 32);
                put16(&mut m, 56, 1);
                maps.extend_from_slice(&m);
            }
            put32(&mut b, 264, maps.len() as u32);
            put32(&mut b, 268, if meta { 2 } else { 1 });
            b[440..440 + maps.len()].copy_from_slice(&maps);
            tag(&mut b, 6, ver, (start + 3) as u32, 440 + maps.len() - 16);
            img[blk(start + 3)..blk(start + 4)].copy_from_slice(&b);
            // USD (3/10.8), TD (3/10.9).
            let mut b = vec![0u8; bs as usize];
            put32(&mut b, 16, 4);
            tag(&mut b, 7, ver, (start + 4) as u32, 24 - 16);
            img[blk(start + 4)..blk(start + 5)].copy_from_slice(&b);
            let mut b = vec![0u8; bs as usize];
            tag(&mut b, 8, ver, (start + 5) as u32, 512 - 16);
            img[blk(start + 5)..blk(start + 6)].copy_from_slice(&b);
        }
        // LVID (3/10.10, UDF 2.2.6) + TD.
        {
            let n_maps = if meta { 2usize } else { 1 };
            let mut b = vec![0u8; bs as usize];
            timestamp(&mut b[16..28], self.mtime);
            put32(&mut b, 28, 1); // closed
            put64(&mut b, 40, 16 + self.ents.len() as u64);
            put32(&mut b, 72, n_maps as u32);
            put32(&mut b, 76, 46);
            for k in 0..n_maps {
                put32(&mut b, 80 + 4 * n_maps + 4 * k, plen as u32);
            }
            let iu = 80 + 8 * n_maps;
            regid(&mut b[iu..iu + 32], "*LumenDisk", &[]);
            put32(&mut b, iu + 32, n_files);
            put32(&mut b, iu + 36, n_dirs);
            put16(&mut b, iu + 40, self.rev);
            put16(&mut b, iu + 42, self.rev);
            put16(&mut b, iu + 44, self.rev);
            tag(&mut b, 9, ver, lvid_at as u32, iu + 46 - 16);
            img[blk(lvid_at)..blk(lvid_at + 1)].copy_from_slice(&b);
            let mut b = vec![0u8; bs as usize];
            tag(&mut b, 8, ver, (lvid_at + 1) as u32, 512 - 16);
            img[blk(lvid_at + 1)..blk(lvid_at + 2)].copy_from_slice(&b);
        }
        // AVDPs (3/10.2) at 256 and at the last block.
        for at in [256u64, total - 1] {
            let mut b = vec![0u8; bs as usize];
            put32(&mut b, 16, (vd_count * bs) as u32);
            put32(&mut b, 20, vds_main as u32);
            put32(&mut b, 24, (vd_count * bs) as u32);
            put32(&mut b, 28, vds_res as u32);
            tag(&mut b, 2, ver, at as u32, 512 - 16);
            img[blk(at)..blk(at + 1)].copy_from_slice(&b);
        }
        (img, map)
    }

    /// A File Entry (4/14.9) or Extended File Entry (4/14.17) block.
    #[allow(clippy::too_many_arguments)]
    fn file_entry(&self, loc: u32, ftype: u8, mode: u32, size: u64, recorded: u64, uid: u64, links: u16, ad_type: u16, ads: &[u8], strategy: u16, parent: u32, pref: u16) -> Vec<u8> {
        let bs = self.bs as usize;
        let mut b = vec![0u8; bs];
        let base = self.base();
        // ICB tag (4/14.6).
        put16(&mut b, 20, strategy);
        put16(&mut b, 24, if strategy == 4096 { 2 } else { 1 });
        b[27] = ftype;
        put32(&mut b, 28, parent);
        put16(&mut b, 32, pref);
        put16(&mut b, 34, ad_type);
        put32(&mut b, 36, 0);
        put32(&mut b, 40, 0);
        put32(&mut b, 44, perms(mode));
        put16(&mut b, 48, links);
        put64(&mut b, 56, size);
        let (rec, times, ckpt, impl_at, uid_at, lea_at) = if self.efe() { (72, [80, 92, 104, 116], 128, 168, 200, 208) } else { (64, [72, 84, 96, 96], 108, 128, 160, 168) };
        if self.efe() {
            put64(&mut b, 64, size);
        }
        put64(&mut b, rec, recorded);
        for t in times {
            timestamp(&mut b[t..t + 12], self.mtime);
        }
        put32(&mut b, ckpt, 1);
        regid(&mut b[impl_at..impl_at + 32], "*LumenDisk", &[]);
        put64(&mut b, uid_at, uid);
        put32(&mut b, lea_at, 0);
        put32(&mut b, lea_at + 4, ads.len() as u32);
        assert!(base + ads.len() <= bs, "allocation descriptors overflow the ICB");
        b[base..base + ads.len()].copy_from_slice(ads);
        let id = if self.efe() { 266 } else { 261 };
        tag(&mut b, id, self.version(), loc, base + ads.len() - 16);
        b
    }
}
