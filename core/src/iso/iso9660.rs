//! ISO 9660 (ECMA-119, 4th edition) with Joliet and Rock Ridge (SUSP 1.12 / RRIP 1.12).
//!
//! - Volume descriptors start at logical sector 16 (ECMA-119 8.1): Primary (type 1, 8.4),
//!   Supplementary (type 2, 8.5; Joliet when its escape sequences name UCS-2 level 1–3),
//!   Boot Record (type 0, 8.2; see [`super::eltorito`]), terminator (type 255, 8.3).
//! - Directories are sequences of Directory Records (9.1) that never cross a logical sector;
//!   a zero length byte means "continue at the next sector" (6.8.1.1).
//! - Files larger than one extent (ISO level 3, files ≥ 4 GiB) are recorded as several records
//!   with the same identifier, all but the last flagged "multi-extent" (9.1.6 bit 7, 6.5.1).
//! - Interleaved files (9.1.7 / 9.1.8, file unit size + gap) are mapped unit by unit.
//! - Rock Ridge lives in the System Use field (9.1.13) as SUSP entries: SP (SUSP 5.3, also
//!   gives the bytes to skip), CE continuation areas (5.1), ST (5.4), ER (5.5) and the RRIP
//!   entries PX (4.1.1), SL (4.1.3), NM (4.1.4), CL / PL / RE (4.1.5), TF (4.1.6), ZF.

use std::collections::HashSet;

use super::{child_path, epoch, Data, Extent, Limits, Node, Parsed, View};
use crate::block::BlockDev;
use crate::forge::{ForgeError, ForgeResult};

/// Logical sector size of ISO 9660 media (ECMA-119 6.1.2: 2048 for CD-ROM).
pub(crate) const SECTOR: u64 = 2048;
/// First volume descriptor (ECMA-119 6.2.1: the System Area is sectors 0–15).
const VD_START: u64 = 16;
/// Volume descriptors scanned at most before giving up on a terminator.
const MAX_VDS: u64 = 64;
/// Bounds on Rock Ridge continuation areas of one directory record.
const MAX_CE_AREAS: usize = 32;
const MAX_CE_BYTES: usize = 256 * 1024;

fn invalid(msg: impl Into<String>) -> ForgeError {
    ForgeError::Invalid(format!("ISO 9660: {}", msg.into()))
}

fn io(e: std::io::Error) -> ForgeError {
    ForgeError::Io(format!("ISO 9660: {e}"))
}

/// Both-endian 32-bit field (ECMA-119 7.3.3): the little-endian half is used.
fn both32(b: &[u8], o: usize) -> u32 {
    super::le32(b, o)
}
fn both16(b: &[u8], o: usize) -> u16 {
    super::le16(b, o)
}

/// Whether sector 16 holds an ISO 9660 volume descriptor ("CD001", ECMA-119 8.1.2).
pub(crate) fn has_iso9660(dev: &dyn BlockDev) -> bool {
    dev.read_vec(VD_START * SECTOR + 1, 5).map(|v| &v[..] == b"CD001").unwrap_or(false)
}

/// The volume descriptors (type, 2048 bytes) up to the terminator.
pub(crate) fn volume_descriptors(dev: &dyn BlockDev) -> ForgeResult<Vec<(u8, Vec<u8>)>> {
    let mut out = vec![];
    for i in 0..MAX_VDS {
        let off = (VD_START + i) * SECTOR;
        if off + SECTOR > dev.size() {
            break;
        }
        let vd = dev.read_vec(off, SECTOR as usize).map_err(io)?;
        if &vd[1..6] != b"CD001" {
            break;
        }
        let t = vd[0];
        if t == 255 {
            break;
        }
        out.push((t, vd));
    }
    if out.is_empty() {
        return Err(invalid("no volume descriptors (not an ISO image)"));
    }
    Ok(out)
}

/// One Directory Record (ECMA-119 9.1), borrowed from the directory data.
struct Rec<'a> {
    /// Extended attribute record length in logical blocks (9.1.2).
    ear: u8,
    /// Location of extent (9.1.3).
    lba: u32,
    /// Data length (9.1.4).
    size: u32,
    /// Recording date and time (9.1.5).
    date: &'a [u8],
    /// File flags (9.1.6).
    flags: u8,
    /// File unit size / interleave gap size (9.1.7, 9.1.8).
    unit: u8,
    gap: u8,
    /// File identifier (9.1.11).
    name: &'a [u8],
    /// System Use (9.1.13).
    su: &'a [u8],
}

const F_HIDDEN: u8 = 0x01;
const F_DIR: u8 = 0x02;
const F_ASSOCIATED: u8 = 0x04;
const F_MULTI: u8 = 0x80;

fn parse_rec(b: &[u8]) -> Option<Rec<'_>> {
    let len = *b.first()? as usize;
    if len < 34 || len > b.len() {
        return None;
    }
    let b = &b[..len];
    let name_len = b[32] as usize;
    let name_end = 33 + name_len;
    if name_end > len {
        return None;
    }
    // Padding field (9.1.12): present when the identifier length is even.
    let su_start = (name_end + (name_len % 2 == 0) as usize).min(len);
    Some(Rec { ear: b[1], lba: both32(b, 2), size: both32(b, 10), date: &b[18..25], flags: b[25], unit: b[26], gap: b[27], name: &b[33..name_end], su: &b[su_start..] })
}

/// Seconds since the epoch of a 7-byte directory record date (ECMA-119 9.1.5).
fn date7(d: &[u8]) -> i64 {
    if d.len() < 7 || d.iter().all(|&x| x == 0) {
        return 0;
    }
    let tz = d[6] as i8 as i64 * 15;
    epoch(1900 + d[0] as i64, d[1] as u32, d[2] as u32, d[3] as u32, d[4] as u32, d[5] as u32, tz).unwrap_or(0)
}

/// Seconds since the epoch of a 17-byte digit date (ECMA-119 8.4.26.1).
fn date17(d: &[u8]) -> i64 {
    if d.len() < 17 {
        return 0;
    }
    let num = |a: usize, n: usize| -> Option<i64> {
        let s = std::str::from_utf8(d.get(a..a + n)?).ok()?;
        s.parse::<i64>().ok()
    };
    let (Some(y), Some(mo), Some(da), Some(h), Some(mi), Some(s)) = (num(0, 4), num(4, 2), num(6, 2), num(8, 2), num(10, 2), num(12, 2)) else { return 0 };
    if y == 0 {
        return 0;
    }
    let tz = d[16] as i8 as i64 * 15;
    epoch(y, mo as u32, da as u32, h as u32, mi as u32, s as u32, tz).unwrap_or(0)
}

/// Plain ISO 9660 identifier → name: drop the version (";1") and the separator dot that
/// ECMA-119 7.5.1 requires even without an extension ("README.;1" → "README").
fn iso_name(raw: &[u8]) -> String {
    let mut s: Vec<u8> = raw.iter().copied().take_while(|&c| c != 0).collect();
    if let Some(p) = s.iter().rposition(|&c| c == b';') {
        s.truncate(p);
    }
    if s.last() == Some(&b'.') {
        s.pop();
    }
    String::from_utf8_lossy(&s).into_owned()
}

/// Joliet identifier (UCS-2 big endian, Joliet spec 1995 §2): drop ";1" and trailing dots.
fn joliet_name(raw: &[u8]) -> String {
    let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    let mut s = String::from_utf16_lossy(&units);
    if let Some(p) = s.rfind(';') {
        if s[p + 1..].chars().all(|c| c.is_ascii_digit()) {
            s.truncate(p);
        }
    }
    while s.len() > 1 && s.ends_with('.') {
        s.pop();
    }
    s.trim_end_matches('\0').to_string()
}

/// What the Rock Ridge entries of one directory record say.
#[derive(Default)]
struct Rr {
    /// Any RRIP entry seen.
    present: bool,
    name: Option<Vec<u8>>,
    mode: Option<u32>,
    mtime: Option<i64>,
    /// SL component records, in order: (flags, content).
    sl: Vec<(u8, Vec<u8>)>,
    symlink: bool,
    /// RE: this directory was relocated; its record is hidden (RRIP 4.1.5.3).
    relocated: bool,
    /// CL: this "file" stands for the directory at this location (RRIP 4.1.5.1).
    child_link: Option<u32>,
    /// ZF: zisofs-compressed data.
    zisofs: bool,
}

impl Rr {
    fn symlink_target(&self) -> Option<String> {
        if !self.symlink {
            return None;
        }
        // RRIP 4.1.3.1: components joined by '/'; a component flagged CONTINUE goes on in the
        // next component record without a separator; ROOT gives a leading '/'.
        let mut out = String::new();
        let mut sep = false;
        for (flags, content) in &self.sl {
            if sep && !out.ends_with('/') {
                out.push('/');
            }
            sep = false;
            match flags & 0x0E {
                0x02 => out.push('.'),
                0x04 => out.push_str(".."),
                0x08 => out.push('/'),
                _ => out.push_str(&String::from_utf8_lossy(content)),
            }
            if flags & 0x01 == 0 {
                sep = true;
            }
            if out.len() > 4096 {
                break;
            }
        }
        Some(out)
    }
}

/// Context for System Use parsing.
struct SuCtx<'a> {
    dev: &'a dyn BlockDev,
    /// Logical block size.
    bs: u64,
    /// Bytes to skip at the start of each System Use field (SP LEN_SKP, SUSP 5.3).
    skip: usize,
}

/// Parse the SUSP entries of `su` (and of its continuation areas) into `rr`.
fn parse_su(ctx: &SuCtx<'_>, su: &[u8], rr: &mut Rr) {
    let mut area: Vec<u8> = su.get(ctx.skip..).unwrap_or(&[]).to_vec();
    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    let mut total = 0usize;
    for _ in 0..=MAX_CE_AREAS {
        let mut next: Option<(u32, u32, u32)> = None;
        let mut p = 0usize;
        while p + 4 <= area.len() {
            let sig = [area[p], area[p + 1]];
            let len = area[p + 2] as usize;
            if len < 4 || p + len > area.len() {
                break;
            }
            let e = &area[p..p + len];
            match &sig {
                b"CE" if len >= 28 => next = Some((both32(e, 4), both32(e, 12), both32(e, 20))),
                b"ST" => break,
                b"NM" if len >= 5 => {
                    rr.present = true;
                    let flags = e[4];
                    // CURRENT / PARENT names ("." / "..") are not names of this entry.
                    if flags & 0x06 == 0 {
                        let n = rr.name.get_or_insert_with(Vec::new);
                        if n.len() < 1024 {
                            n.extend_from_slice(&e[5..]);
                        }
                    }
                }
                b"PX" if len >= 36 => {
                    rr.present = true;
                    rr.mode = Some(both32(e, 4));
                }
                b"SL" if len >= 5 => {
                    rr.present = true;
                    rr.symlink = true;
                    let mut q = 5usize;
                    while q + 2 <= e.len() {
                        let cflags = e[q];
                        let clen = e[q + 1] as usize;
                        let Some(content) = e.get(q + 2..q + 2 + clen) else { break };
                        if rr.sl.len() < 1024 {
                            rr.sl.push((cflags, content.to_vec()));
                        }
                        q += 2 + clen;
                    }
                }
                b"TF" if len >= 5 => {
                    rr.present = true;
                    let flags = e[4];
                    let long = flags & 0x80 != 0;
                    let sz = if long { 17 } else { 7 };
                    // Order of the stamps (RRIP 4.1.6): creation, modify, access, attributes…
                    if flags & 0x02 != 0 {
                        let idx = (flags & 0x01) as usize;
                        let at = 5 + idx * sz;
                        if let Some(d) = e.get(at..at + sz) {
                            let t = if long { date17(d) } else { date7(d) };
                            if t != 0 {
                                rr.mtime = Some(t);
                            }
                        }
                    }
                }
                b"RE" => {
                    rr.present = true;
                    rr.relocated = true;
                }
                b"CL" if len >= 12 => {
                    rr.present = true;
                    rr.child_link = Some(both32(e, 4));
                }
                b"ZF" => {
                    rr.present = true;
                    rr.zisofs = true;
                }
                b"PN" | b"PL" | b"RR" | b"SF" => rr.present = true,
                _ => {}
            }
            p += len;
        }
        let Some((blk, off, len)) = next else { return };
        if !seen.insert((blk, off)) {
            return; // continuation loop
        }
        let len = len as usize;
        total = total.saturating_add(len);
        if len == 0 || total > MAX_CE_BYTES {
            return;
        }
        let Some(pos) = (blk as u64).checked_mul(ctx.bs).and_then(|x| x.checked_add(off as u64)) else { return };
        match ctx.dev.read_vec(pos, len) {
            Ok(v) => area = v,
            Err(_) => return,
        }
    }
}

/// A directory to list.
struct DirJob {
    path: String,
    lba: u32,
    size: u32,
    depth: usize,
}

struct Walker<'a> {
    dev: &'a dyn BlockDev,
    lim: &'a Limits,
    bs: u64,
    joliet: bool,
    rock: Option<usize>,
    nodes: Vec<Node>,
    extents: usize,
    visited: HashSet<u32>,
}

impl Walker<'_> {
    fn dir_data(&self, lba: u32, size: u32) -> ForgeResult<Vec<u8>> {
        let size = size as u64;
        if size > self.lim.max_dir_bytes {
            return Err(invalid(format!("directory at sector {lba} is too large ({size} bytes)")));
        }
        let pos = (lba as u64).checked_mul(self.bs).ok_or_else(|| invalid("directory location overflows"))?;
        if pos.checked_add(size).map_or(true, |e| e > self.dev.size()) {
            return Err(invalid(format!("directory at sector {lba} lies beyond the end of the image")));
        }
        self.dev.read_vec(pos, size as usize).map_err(io)
    }

    /// Size of the directory at `lba` from its own "." record (for Rock Ridge CL targets).
    fn dir_size_at(&self, lba: u32) -> Option<u32> {
        let pos = (lba as u64).checked_mul(self.bs)?;
        let b = self.dev.read_vec(pos, 256.min(self.bs as usize)).ok()?;
        let r = parse_rec(&b)?;
        (r.lba == lba && r.name == [0]).then_some(r.size)
    }

    /// Extents of a file made of the multi-extent records `parts` (ECMA-119 6.5.1).
    /// (size, extents); extents None when the data lies outside the image.
    fn file_extents(&mut self, parts: &[Part]) -> ForgeResult<(u64, Option<Vec<Extent>>)> {
        let mut out = vec![];
        let mut total = 0u64;
        for &Part { lba, size, ear, unit, gap } in parts {
            total = total.checked_add(size as u64).ok_or_else(|| invalid("file size overflows"))?;
            if size == 0 {
                continue;
            }
            let start_blk = lba as u64 + ear as u64;
            let start = start_blk.checked_mul(self.bs).ok_or_else(|| invalid("file location overflows"))?;
            if unit == 0 {
                out.push(Extent { pos: start, len: size as u64, sparse: false });
            } else {
                // Interleaved (ECMA-119 6.4.3): `unit` blocks of data, then `gap` blocks skipped.
                let unit_bytes = unit as u64 * self.bs;
                let stride = (unit as u64 + gap as u64) * self.bs;
                let mut left = size as u64;
                let mut pos = start;
                while left > 0 {
                    let n = left.min(unit_bytes);
                    out.push(Extent { pos, len: n, sparse: false });
                    left -= n;
                    pos = pos.checked_add(stride).ok_or_else(|| invalid("interleaved file overflows"))?;
                    if out.len() + self.extents > self.lim.max_extents {
                        return Err(invalid("too many extents"));
                    }
                }
            }
            if out.len() + self.extents > self.lim.max_extents {
                return Err(invalid("too many extents"));
            }
        }
        // Data must lie inside the image: a lying record would otherwise read past the end.
        if out.iter().any(|e| e.pos.checked_add(e.len).map_or(true, |end| end > self.dev.size())) {
            return Ok((total, None));
        }
        self.extents += out.len();
        Ok((total, Some(out)))
    }

    fn push(&mut self, n: Node) -> ForgeResult<()> {
        if self.nodes.len() >= self.lim.max_nodes {
            return Err(invalid("too many files"));
        }
        self.nodes.push(n);
        Ok(())
    }

    fn walk(&mut self, root_lba: u32, root_size: u32) -> ForgeResult<()> {
        let mut stack = vec![DirJob { path: "/".into(), lba: root_lba, size: root_size, depth: 0 }];
        self.visited.insert(root_lba);
        while let Some(job) = stack.pop() {
            let data = match self.dir_data(job.lba, job.size) {
                Ok(d) => d,
                Err(e) if job.depth == 0 => return Err(e),
                Err(_) => continue, // a broken subdirectory: keep the rest of the tree
            };
            let mut subdirs = vec![];
            // Records of one file (multi-extent parts) are grouped by identifier.
            let mut pending: Option<Pending> = None;
            let mut names_here: HashSet<String> = HashSet::new();
            let mut pos = 0usize;
            let mut index = 0usize;
            while pos < data.len() {
                let len = data[pos] as usize;
                let sector_end = ((pos as u64 / self.bs + 1) * self.bs) as usize;
                if len == 0 || pos + len > sector_end.min(data.len()) {
                    // 6.8.1.1: unused bytes up to the end of the sector.
                    pos = sector_end;
                    continue;
                }
                let Some(rec) = parse_rec(&data[pos..pos + len]) else {
                    pos = sector_end;
                    continue;
                };
                pos += len;
                index += 1;
                if index <= 2 && (rec.name == [0] || rec.name == [1]) {
                    continue; // "." and ".." (9.1.11)
                }
                if rec.name == [0] || rec.name == [1] {
                    continue;
                }
                if rec.flags & F_ASSOCIATED != 0 {
                    continue; // associated files are not part of the file tree (as on Linux)
                }
                let part = Part { lba: rec.lba, size: rec.size, ear: rec.ear, unit: rec.unit, gap: rec.gap };
                // The next part of a multi-extent file?
                if pending.as_ref().is_some_and(|p| p.raw == rec.name) {
                    if let Some(p) = pending.as_mut() {
                        p.parts.push(part);
                    }
                    if rec.flags & F_MULTI == 0 {
                        if let Some(p) = pending.take() {
                            self.finish(&job, p, &mut subdirs, &mut names_here)?;
                        }
                    }
                    continue;
                }
                // A multi-extent chain interrupted by another name: close it as it is.
                if let Some(p) = pending.take() {
                    self.finish(&job, p, &mut subdirs, &mut names_here)?;
                }
                let mut rr = Rr::default();
                if let Some(skip) = self.rock {
                    parse_su(&SuCtx { dev: self.dev, bs: self.bs, skip }, rec.su, &mut rr);
                }
                let p = Pending { raw: rec.name.to_vec(), parts: vec![part], rr, flags: rec.flags, date: date7(rec.date) };
                if rec.flags & F_MULTI != 0 && rec.flags & F_DIR == 0 {
                    pending = Some(p);
                    continue;
                }
                self.finish(&job, p, &mut subdirs, &mut names_here)?;
            }
            if let Some(p) = pending.take() {
                self.finish(&job, p, &mut subdirs, &mut names_here)?;
            }
            // Depth-first, in on-disc order.
            for d in subdirs.into_iter().rev() {
                stack.push(d);
            }
        }
        Ok(())
    }

    fn finish(&mut self, job: &DirJob, p: Pending, subdirs: &mut Vec<DirJob>, names: &mut HashSet<String>) -> ForgeResult<()> {
        let Pending { raw, parts, rr, flags, date } = p;
        if rr.relocated {
            return Ok(()); // RRIP 4.1.5.3: shown where its CL record is
        }
        let name = match (&rr.name, self.joliet) {
            (Some(n), _) if !n.is_empty() => String::from_utf8_lossy(n).into_owned(),
            (_, true) => joliet_name(&raw),
            _ => iso_name(&raw),
        };
        let Some(path) = child_path(&job.path, &name, self.lim) else { return Ok(()) };
        if !names.insert(path.clone()) {
            return Ok(()); // duplicate name in one directory (hostile): keep the first
        }
        let _ = flags & F_HIDDEN; // hidden files are listed, as Linux does by default
        let mtime = rr.mtime.unwrap_or(date);
        let mode = rr.mode.map(|m| m & 0o7777).unwrap_or(0);
        let (first_lba, first_size) = parts.first().map(|p| (p.lba, p.size)).unwrap_or((0, 0));
        // Directories: the flag, or a Rock Ridge child link to a relocated one.
        let dir_target = match rr.child_link {
            Some(cl) => self.dir_size_at(cl).map(|s| (cl, s)),
            None if flags & F_DIR != 0 => Some((first_lba, first_size)),
            None => None,
        };
        if rr.child_link.is_some() && dir_target.is_none() {
            return Ok(()); // a CL to nowhere
        }
        if let Some((lba, size)) = dir_target {
            self.push(Node { path: path.clone(), is_dir: true, size: 0, mode, mtime, symlink: None, data: Data::None })?;
            if job.depth + 1 >= self.lim.max_depth {
                return Err(invalid("directories nested too deeply"));
            }
            if self.visited.insert(lba) {
                subdirs.push(DirJob { path, lba, size, depth: job.depth + 1 });
            }
            // else: a directory reached twice (a loop or a hard-linked directory): listed once.
            return Ok(());
        }
        if let Some(target) = rr.symlink_target() {
            return self.push(Node { path, is_dir: false, size: target.len() as u64, mode, mtime, symlink: Some(target), data: Data::None });
        }
        let (size, ex) = self.file_extents(&parts)?;
        let data = match ex {
            _ if rr.zisofs => Data::Unsupported("zisofs-compressed file (Rock Ridge ZF): not supported".into()),
            None => Data::Unsupported("its data lies beyond the end of the image (damaged or truncated ISO)".into()),
            Some(ex) if ex.is_empty() => Data::None,
            Some(ex) => Data::Extents(ex),
        };
        self.push(Node { path, is_dir: false, size, mode, mtime, symlink: None, data })
    }
}

/// One extent record of a file (several for multi-extent files).
#[derive(Clone, Copy)]
struct Part {
    lba: u32,
    size: u32,
    ear: u8,
    unit: u8,
    gap: u8,
}

/// A file whose records are being collected.
struct Pending {
    raw: Vec<u8>,
    parts: Vec<Part>,
    rr: Rr,
    flags: u8,
    date: i64,
}

/// Joliet: a Supplementary Volume Descriptor whose escape sequences (ECMA-119 8.5.6) name
/// UCS-2 level 1, 2 or 3 ("%/@", "%/C", "%/E").
fn is_joliet(svd: &[u8]) -> bool {
    let esc = &svd[88..120];
    svd[7] & 1 == 0 && (0..30).any(|i| esc[i] == b'%' && esc[i + 1] == b'/' && matches!(esc[i + 2], b'@' | b'C' | b'E'))
}

/// Rock Ridge on the primary tree: the root's "." record starts its System Use field with an
/// SP entry (SUSP 5.3); returns the LEN_SKP it announces.
fn rock_ridge_skip(dev: &dyn BlockDev, bs: u64, root_lba: u32, root_size: u32) -> Option<usize> {
    let pos = (root_lba as u64).checked_mul(bs)?;
    let b = dev.read_vec(pos, (root_size as u64).min(bs) as usize).ok()?;
    let r = parse_rec(&b)?;
    let su = r.su;
    let sp = |s: &[u8]| s.len() >= 7 && &s[0..2] == b"SP" && s[2] >= 7 && s[4] == 0xBE && s[5] == 0xEF;
    if sp(su) {
        return Some(su[6] as usize);
    }
    // CD-ROM XA discs carry 14 bytes of XA system use before SUSP.
    if su.len() > 14 && &su[6..8] == b"XA" && sp(&su[14..]) {
        return Some(14 + su[14 + 6] as usize);
    }
    None
}

fn volume_id(vd: &[u8], joliet: bool) -> String {
    let raw = &vd[40..72];
    let s = if joliet { joliet_name(raw) } else { String::from_utf8_lossy(raw).into_owned() };
    s.trim_end_matches([' ', '\0']).to_string()
}

/// Parse the tree. `want`: None = the best view (Rock Ridge, else Joliet, else ISO 9660);
/// Some(view) = that one (an error if the image doesn't have it).
pub(crate) fn read(dev: &dyn BlockDev, lim: &Limits, want: Option<View>) -> ForgeResult<Parsed> {
    let vds = volume_descriptors(dev)?;
    let pvd = vds.iter().find(|(t, _)| *t == 1).map(|(_, v)| v.clone()).ok_or_else(|| invalid("no primary volume descriptor"))?;
    let svd = vds.iter().find(|(t, v)| *t == 2 && is_joliet(v)).map(|(_, v)| v.clone());
    // Logical block size (8.4.12): 512, 1024 or 2048.
    let bs = both16(&pvd, 128) as u64;
    if !matches!(bs, 512 | 1024 | 2048) {
        return Err(invalid(format!("logical block size {bs} not supported")));
    }
    let root = |vd: &[u8]| -> ForgeResult<(u32, u32)> {
        let r = parse_rec(&vd[156..190]).ok_or_else(|| invalid("bad root directory record"))?;
        Ok((r.lba, r.size))
    };
    let (plba, psize) = root(&pvd)?;
    let rock = rock_ridge_skip(dev, bs, plba, psize);
    let label = volume_id(&pvd, false);
    let view = match want {
        Some(View::RockRidge) => {
            if rock.is_none() {
                return Err(invalid("no Rock Ridge extensions"));
            }
            View::RockRidge
        }
        Some(View::Joliet) => {
            if svd.is_none() {
                return Err(invalid("no Joliet volume descriptor"));
            }
            View::Joliet
        }
        Some(View::Iso9660) => View::Iso9660,
        Some(View::Udf) => return Err(invalid("UDF is not an ISO 9660 view")),
        None if rock.is_some() => View::RockRidge,
        None if svd.is_some() => View::Joliet,
        None => View::Iso9660,
    };
    let (lba, size, joliet, rock) = match view {
        View::Joliet => {
            let s = svd.as_ref().ok_or_else(|| invalid("no Joliet volume descriptor"))?;
            let (l, z) = root(s)?;
            (l, z, true, None)
        }
        View::RockRidge => (plba, psize, false, rock),
        _ => (plba, psize, false, None),
    };
    let mut w = Walker { dev, lim, bs, joliet, rock, nodes: vec![], extents: 0, visited: HashSet::new() };
    w.walk(lba, size)?;
    let label = if label.is_empty() { svd.as_ref().map(|s| volume_id(s, true)).unwrap_or_default() } else { label };
    Ok(Parsed { view, label, nodes: w.nodes })
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn names_and_dates_decode_like_linux() {
        assert_eq!(iso_name(b"README.TXT;1"), "README.TXT");
        assert_eq!(iso_name(b"README.;1"), "README");
        assert_eq!(iso_name(b"DIR"), "DIR");
        let j: Vec<u8> = "Caffè.txt;1".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        assert_eq!(joliet_name(&j), "Caffè.txt");
        assert_eq!(date7(&[124, 1, 2, 3, 4, 5, 0]), 1_704_164_645); // 2024-01-02 03:04:05 UTC
        assert_eq!(date7(&[124, 1, 2, 3, 4, 5, 4]), 1_704_164_645 - 3600, "GMT+1");
        assert_eq!(date17(b"2024010203040500\x00"), 1_704_164_645);
        assert_eq!(date7(&[124, 13, 2, 3, 4, 5, 0]), 0, "invalid month");
    }

    #[test]
    fn symlink_components_join_like_rrip() {
        let mut rr = Rr { symlink: true, ..Default::default() };
        rr.sl = vec![(0x08, vec![]), (0, b"usr".to_vec()), (0x01, b"li".to_vec()), (0, b"b".to_vec()), (0x04, vec![]), (0, b"x".to_vec())];
        assert_eq!(rr.symlink_target().unwrap(), "/usr/lib/../x");
        rr.sl = vec![(0x02, vec![]), (0, b"a".to_vec())];
        assert_eq!(rr.symlink_target().unwrap(), "./a");
    }
}
