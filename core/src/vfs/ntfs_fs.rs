//! NTFS driver (pure user space, no journal replay).
//!
//! * Read: boot sector, $MFT with fixups, $ATTRIBUTE_LIST, resident / non-resident (sparse,
//!   LZNT1-compressed) attributes, $I30 B+tree directories with $UpCase collation.
//! * Symlinks: created as IO_REPARSE_TAG_SYMLINK reparse points (relative, or absolute as
//!   `\??\C:\...`; the WSL IO_REPARSE_TAG_LX_SYMLINK only for targets that are not valid
//!   Windows paths) and registered in
//!   `$Extend/$Reparse:$R`; Windows symlinks/junctions, WSL and Interix (ntfs-3g) symlinks
//!   are read. See `ntfs/reparse.rs` for the exact mapping.
//! * Write: file/dir create, unlink, rmdir, rename, write, truncate, times. MFT records come
//!   from $MFT:$BITMAP (the MFT grows on demand), clusters from $Bitmap, directory indexes
//!   are real B+trees (root push-down, node split, merge on delete). $MFTMirr is kept in sync.
//!   $LogFile is never written during normal operation.
//! * Journal doctor: volume dirty flag, hibernated Windows, unclean $LogFile; `repair()` acts
//!   like `ntfsfix`.
//!
//! Layout of the sub-modules (under `vfs/ntfs/`): `layout` (on-disk structures), `upcase`
//! (default $UpCase table), `index` (directory / view index B+trees), `mkfs` (formatter),
//! `reparse` (symlink reparse formats).

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;

use super::*;
use crate::block::BlockDev;

#[path = "ntfs/index.rs"]
mod index;
#[path = "ntfs/layout.rs"]
mod layout;
#[path = "ntfs/mkfs.rs"]
mod mkfs;
#[path = "ntfs/reparse.rs"]
mod reparse;
#[path = "ntfs/upcase.rs"]
mod upcase;

use index::Coll;
use layout::*;
use reparse::*;

/// First MFT record handed out to user files (same policy as ntfs-3g).
const FIRST_USER_RECORD: u64 = 64;
/// Records 0..24 belong to the system; never modify them through the VFS API.
const FIRST_NON_SYSTEM: u64 = 24;
const ROOT_INO: u64 = 5;
/// Largest $DATA kept resident in the MFT record.
const RESIDENT_MAX: usize = 600;
const MAX_NAME_UNITS: usize = 255;
const DUP_FLUSH_THRESHOLD: usize = 256;

pub fn probe(dev: &dyn BlockDev) -> bool {
    let mut b = [0u8; 512];
    if dev.size() < 512 * 16 || dev.read_at(0, &mut b).is_err() {
        return false;
    }
    Boot::parse(&b, dev.size()).is_ok()
}

pub fn format(dev: Arc<dyn BlockDev>, label: &str) -> FsResult<()> {
    mkfs::format(dev, label)
}

// ---------------------------------------------------------------------------------------------
// Boot sector

#[derive(Clone, Copy, Debug)]
struct Boot {
    cluster: u64,
    total_sectors: u64,
    bps: u64,
    mft_lcn: u64,
    mirr_lcn: u64,
    rec_size: usize,
    index_block: u32,
    serial: u64,
}

impl Boot {
    fn parse(b: &[u8], dev_size: u64) -> FsResult<Boot> {
        if b.len() < 512 || &b[3..11] != b"NTFS    " {
            return corrupt("not an NTFS boot sector");
        }
        if b[510] != 0x55 || b[511] != 0xAA {
            return corrupt("missing boot signature");
        }
        let bps = le16(b, 11)? as u64;
        if !(256..=4096).contains(&bps) || !bps.is_power_of_two() {
            return corrupt("bad bytes per sector");
        }
        let spc_raw = b[13];
        let spc: u64 = if spc_raw <= 0x80 {
            spc_raw as u64
        } else {
            let sh = 256 - spc_raw as u32;
            if sh > 20 {
                return corrupt("bad sectors per cluster");
            }
            1u64 << sh
        };
        if spc == 0 || !spc.is_power_of_two() {
            return corrupt("bad sectors per cluster");
        }
        let cluster = bps * spc;
        if cluster > 2 * 1024 * 1024 {
            return corrupt("cluster too large");
        }
        let size_of = |v: i8| -> FsResult<u64> {
            if v > 0 {
                Ok(v as u64 * cluster)
            } else if v < 0 && v >= -31 {
                Ok(1u64 << (-(v as i32)))
            } else {
                corrupt("bad record size")
            }
        };
        let rec_size = size_of(b[0x40] as i8)?;
        let index_block = size_of(b[0x44] as i8)?;
        if !(512..=65536).contains(&rec_size) || rec_size % 512 != 0 {
            return corrupt("bad MFT record size");
        }
        if !(512..=65536).contains(&index_block) || index_block % 512 != 0 {
            return corrupt("bad index block size");
        }
        let total_sectors = le64(b, 0x28)?;
        let mft_lcn = le64(b, 0x30)?;
        let mirr_lcn = le64(b, 0x38)?;
        let total_clusters = total_sectors / spc;
        if total_clusters == 0 || mft_lcn >= total_clusters || mirr_lcn >= total_clusters {
            return corrupt("MFT location outside the volume");
        }
        if total_sectors.saturating_mul(bps) > dev_size {
            return corrupt("volume larger than the device (truncated image?)");
        }
        Ok(Boot {
            cluster,
            total_sectors,
            bps,
            mft_lcn,
            mirr_lcn,
            rec_size: rec_size as usize,
            index_block: index_block as u32,
            serial: le64(b, 0x48)?,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// In-memory inode (base record + extension records)

struct Rec {
    no: u64,
    r: MftRecord,
    dirty: bool,
}

struct Inode {
    no: u64,
    recs: Vec<Rec>,
    has_al: bool,
}

type Loc = (usize, usize);

impl Inode {
    fn base(&self) -> &MftRecord {
        &self.recs[0].r
    }
    fn seq(&self) -> u16 {
        self.recs[0].r.seq
    }
    fn mref(&self) -> u64 {
        mk_ref(self.no, self.seq())
    }
    fn is_dir(&self) -> bool {
        self.base().flags & MFT_IS_DIR != 0
    }
    fn find_all(&self, ty: u32, name: &[u16]) -> Vec<Loc> {
        let mut v = vec![];
        for (ri, r) in self.recs.iter().enumerate() {
            for (ai, a) in r.r.attrs.iter().enumerate() {
                if a.is_named(ty, name) {
                    v.push((ri, ai));
                }
            }
        }
        v
    }
    fn find(&self, ty: u32, name: &[u16]) -> Option<Loc> {
        let all = self.find_all(ty, name);
        all.iter()
            .copied()
            .find(|&(r, a)| self.recs[r].r.attrs[a].nonres().map_or(true, |n| n.start_vcn == 0))
            .or(all.first().copied())
    }
    fn attr(&self, l: Loc) -> &Attribute {
        &self.recs[l.0].r.attrs[l.1]
    }
    fn attr_mut(&mut self, l: Loc) -> &mut Attribute {
        self.recs[l.0].dirty = true;
        &mut self.recs[l.0].r.attrs[l.1]
    }
    fn file_names(&self) -> Vec<(Loc, FileName)> {
        let mut v = vec![];
        for l in self.find_all(AT_FILE_NAME, &[]) {
            if let Some(val) = self.attr(l).value() {
                if let Ok(f) = FileName::parse(val) {
                    v.push((l, f));
                }
            }
        }
        v
    }
    fn std_info(&self) -> StdInfo {
        self.find(AT_STANDARD_INFORMATION, &[])
            .and_then(|l| self.attr(l).value().and_then(|v| StdInfo::parse(v).ok()))
            .unwrap_or_default()
    }
    fn set_std_info(&mut self, si: &StdInfo) {
        if let Some(l) = self.find(AT_STANDARD_INFORMATION, &[]) {
            if let Body::Res { value, .. } = &mut self.attr_mut(l).body {
                si.store(value);
            }
        }
    }
    /// Remove an attribute (caller frees clusters).
    fn remove(&mut self, l: Loc) -> Attribute {
        self.recs[l.0].dirty = true;
        self.recs[l.0].r.attrs.remove(l.1)
    }
    /// Add an attribute to the base record (store_inode relocates it if needed).
    fn add(&mut self, a: Attribute) {
        self.recs[0].dirty = true;
        self.recs[0].r.add_attr(a);
    }
}

/// A resolved data stream.
enum Stream {
    Res(Vec<u8>),
    NonRes { runs: Vec<Run>, size: u64, init: u64, flags: u16, comp_unit: u8 },
}

impl Stream {
    fn size(&self) -> u64 {
        match self {
            Stream::Res(v) => v.len() as u64,
            Stream::NonRes { size, .. } => *size,
        }
    }
}

// ---------------------------------------------------------------------------------------------

pub struct NtfsFs {
    dev: Arc<dyn BlockDev>,
    ro: bool,
    defaults: MountDefaults,
    cluster: u64,
    total_clusters: u64,
    rec_size: usize,
    index_block: u32,
    serial: u64,
    mft_lcn: u64,
    mft_runs: Vec<Run>,
    mft_size: u64,
    mirr_runs: Vec<Run>,
    mirr_count: u64,
    mft_bitmap: Vec<u8>,
    mft_hint: u64,
    vbm: Option<Vec<u8>>,
    vbm_runs: Vec<Run>,
    free_clusters: u64,
    data_hint: u64,
    upcase: Vec<u16>,
    label: String,
    version: (u8, u8),
    dup_dirty: HashSet<u64>,
    ro_reason: Option<String>,
    /// Record number of `$Extend/$Reparse` (looked up lazily; `Some(None)` = absent).
    reparse_ino: Option<Option<u64>>,
    /// Relative symlinks created before their target existed (typical of `cp -R`): if the
    /// target later turns out to be a directory, they are re-created as directory records so
    /// Windows and ntfs-3g follow them.
    pending_dir_links: Vec<(Ino, String, String)>,
}

/// What `create_node` makes.
enum NewNode {
    File,
    Dir,
    /// `$REPARSE_POINT` tag and value; `true` = directory record (link to a directory).
    Symlink(u32, Vec<u8>, bool),
}

fn ceil_div(a: u64, b: u64) -> u64 {
    a.div_ceil(b)
}

/// File payload goes through `write_data_at` (ordered mode), metadata through `write_at`.
fn dev_write(dev: &dyn BlockDev, off: u64, buf: &[u8], payload: bool) -> std::io::Result<()> {
    if payload {
        dev.write_data_at(off, buf)
    } else {
        dev.write_at(off, buf)
    }
}

impl NtfsFs {
    /// Re-create file-record relative symlinks whose target has become a directory.
    fn fix_pending_dir_links(&mut self) {
        let pending = std::mem::take(&mut self.pending_dir_links);
        for (parent, name, target) in pending {
            let Ok(ino) = self.lookup(parent, &name) else { continue };
            if self.readlink(ino).ok().as_deref() != Some(target.as_str()) {
                continue; // renamed, replaced or removed meanwhile
            }
            if !self.rel_target_is_dir(parent, &target) {
                // Target still missing (or a file): keep watching while it may appear.
                if self.lookup_target_exists(parent, &target) {
                    continue; // resolved to a file: nothing to do
                }
                self.pending_dir_links.push((parent, name, target));
                continue;
            }
            let (tag, value) = build_symlink(&target);
            if self.unlink(parent, &name).is_ok() {
                let _ = self.create_node(parent, &name, NewNode::Symlink(tag, value, true));
            }
        }
    }

    fn lookup_target_exists(&mut self, dir: Ino, target: &str) -> bool {
        let mut cur = dir;
        for comp in target.split('/').filter(|c| !c.is_empty() && *c != ".") {
            if comp == ".." {
                return true; // can't cheaply resolve upwards here; stop watching
            }
            match self.lookup(cur, comp) {
                Ok(i) => cur = i,
                Err(_) => return false,
            }
        }
        true
    }

    pub fn open(dev: Arc<dyn BlockDev>, read_only: bool, defaults: MountDefaults) -> FsResult<NtfsFs> {
        let bs = dev.read_vec(0, 512)?;
        let boot = Boot::parse(&bs, dev.size())?;
        let total_clusters = boot.total_sectors * boot.bps / boot.cluster;
        let mut fs = NtfsFs {
            dev: dev.clone(),
            ro: read_only || dev.read_only(),
            defaults,
            cluster: boot.cluster,
            total_clusters,
            rec_size: boot.rec_size,
            index_block: boot.index_block,
            serial: boot.serial,
            mft_lcn: boot.mft_lcn,
            mft_runs: vec![Run { vcn: 0, lcn: Some(boot.mft_lcn), len: ceil_div(16 * boot.rec_size as u64, boot.cluster) }],
            mft_size: 16 * boot.rec_size as u64,
            mirr_runs: vec![],
            mirr_count: 0,
            mft_bitmap: vec![],
            mft_hint: FIRST_USER_RECORD,
            vbm: None,
            vbm_runs: vec![],
            free_clusters: 0,
            data_hint: 0,
            upcase: vec![],
            label: String::new(),
            version: (0, 0),
            dup_dirty: HashSet::new(),
            ro_reason: None,
            reparse_ino: None,
            pending_dir_links: Vec::new(),
        };
        fs.load_mft()?;
        // $MFTMirr
        let mirr = fs.load_inode(1)?;
        if let Some(Stream::NonRes { runs, size, .. }) = fs.stream(&mirr, AT_DATA, &[])? {
            fs.mirr_runs = runs;
            fs.mirr_count = (size / fs.rec_size as u64).min(fs.mft_size / fs.rec_size as u64);
        } else {
            fs.mirr_runs = vec![Run { vcn: 0, lcn: Some(boot.mirr_lcn), len: 1 }];
            fs.mirr_count = (boot.cluster / boot.rec_size as u64).clamp(1, 4);
        }
        // $Volume
        let vol = fs.load_inode(3)?;
        if let Some(l) = vol.find(AT_VOLUME_NAME, &[]) {
            if let Some(v) = vol.attr(l).value() {
                fs.label = String::from_utf16_lossy(&bytes_utf16(v));
            }
        }
        if let Some(l) = vol.find(AT_VOLUME_INFORMATION, &[]) {
            if let Some(v) = vol.attr(l).value() {
                fs.version = (byte(v, 8)?, byte(v, 9)?);
            }
        }
        if fs.version.0 < 3 && !fs.ro {
            fs.ro = true; // NTFS 1.x/2.x: read-only support only
            fs.ro_reason = Some(format!("NTFS {}.{} volumes (pre-Windows 2000) are supported read-only", fs.version.0, fs.version.1));
        }
        // $UpCase
        let up = fs.load_inode(10)?;
        let mut table = match fs.stream(&up, AT_DATA, &[])? {
            Some(s) => {
                let n = s.size().min(131072);
                bytes_utf16(&fs.read_stream(&s, 0, n)?)
            }
            None => vec![],
        };
        if table.len() < 128 {
            table = upcase::default_upcase();
        }
        fs.upcase = table;
        // $Bitmap location (contents loaded lazily)
        let bm = fs.load_inode(6)?;
        match fs.stream(&bm, AT_DATA, &[])? {
            Some(Stream::NonRes { runs, size, .. }) if size.saturating_mul(8) >= total_clusters => fs.vbm_runs = runs,
            _ => return corrupt("$Bitmap missing or too small"),
        }
        // Keep new data out of the MFT zone (first 1/8th after the MFT).
        fs.data_hint = (fs.mft_lcn + total_clusters / 8).min(total_clusters.saturating_sub(1));
        // Root must be a directory.
        let root = fs.load_inode(ROOT_INO)?;
        if !root.is_dir() {
            return corrupt("root is not a directory");
        }
        Ok(fs)
    }

    /// Read record 0 directly and build the MFT runlist (following $ATTRIBUTE_LIST if any).
    fn load_mft(&mut self) -> FsResult<()> {
        let rs = self.rec_size;
        let mut raw = self.dev.read_vec(self.mft_lcn * self.cluster, rs)?;
        fixup_read(&mut raw)?;
        let r0 = MftRecord::parse(&raw)?;
        let mut extents: Vec<NonRes> = r0.attrs.iter().filter(|a| a.is_named(AT_DATA, &[])).filter_map(|a| a.nonres().cloned()).collect();
        let first = extents.iter().find(|n| n.start_vcn == 0).cloned().ok_or_else(|| FsError::Corrupt("$MFT has no $DATA".into()))?;
        self.mft_size = first.size;
        self.mft_runs = first.runs.clone();
        if let Some(al) = r0.attrs.iter().find(|a| a.ty == AT_ATTRIBUTE_LIST) {
            let alv = match &al.body {
                Body::Res { value, .. } => value.clone(),
                Body::NonRes(n) => {
                    let mut v = vec![0u8; n.size.min(1 << 20) as usize];
                    self.read_runs(&n.runs, 0, &mut v)?;
                    v
                }
            };
            let mut seen = HashSet::new();
            for e in parse_attr_list(&alv)? {
                if e.ty == AT_DATA && e.name.is_empty() && mref_no(e.mref) != 0 && seen.insert(mref_no(e.mref)) {
                    let r = self.read_record(mref_no(e.mref))?;
                    for a in r.attrs.iter().filter(|a| a.is_named(AT_DATA, &[])) {
                        if let Some(n) = a.nonres() {
                            if n.start_vcn != 0 {
                                extents.push(n.clone());
                            }
                        }
                    }
                    let mut all: Vec<&NonRes> = extents.iter().collect();
                    all.sort_by_key(|n| n.start_vcn);
                    self.mft_runs = all.iter().flat_map(|n| n.runs.iter().copied()).collect();
                }
            }
        }
        if self.mft_size < 16 * rs as u64 || self.mft_size > runs_end(&self.mft_runs).saturating_mul(self.cluster) || self.mft_size > self.dev.size() {
            return corrupt("bad $MFT size");
        }
        let m = self.load_inode(0)?;
        let bm = match self.stream(&m, AT_BITMAP, &[])? {
            Some(s) => {
                // One bit per record: no larger than the records this device could hold.
                let n = s.size().min(align8((self.dev.size() / rs as u64).div_ceil(8) as usize) as u64 + 8);
                self.read_stream(&s, 0, n)?
            }
            None => return corrupt("$MFT has no $BITMAP"),
        };
        self.mft_bitmap = bm;
        Ok(())
    }

    // -- raw I/O ---------------------------------------------------------------------------

    fn check_lcn(&self, lcn: u64, len: u64) -> FsResult<()> {
        if lcn.checked_add(len).map_or(true, |e| e > self.total_clusters + 1) {
            return corrupt(format!("cluster run {lcn}+{len} outside the volume"));
        }
        Ok(())
    }

    /// Visit the physical segments backing `[off, off+len)` of a run-mapped stream.
    fn map_runs(&self, runs: &[Run], off: u64, len: u64, mut f: impl FnMut(u64, Option<u64>, u64) -> FsResult<()>) -> FsResult<()> {
        let cs = self.cluster;
        // Byte offset of a VCN. Runs may reach 2^48 clusters (layout::MAX_CLUSTERS), which
        // times a 2 MiB cluster exceeds u64: saturate (such a VCN is never reachable).
        let at = |vcn: u64| vcn.saturating_mul(cs);
        let mut pos = off;
        let end = off.checked_add(len).ok_or_else(|| FsError::Corrupt("access beyond 2^64 bytes".into()))?;
        let mut idx = runs.partition_point(|r| at(r.vcn + r.len) <= pos);
        while pos < end {
            let r = match runs.get(idx) {
                Some(r) if at(r.vcn) <= pos => *r,
                _ => return corrupt("read beyond mapped runs"),
            };
            let run_end = at(r.vcn + r.len);
            let n = (run_end.min(end)) - pos;
            let phys = match r.lcn {
                Some(l) => {
                    self.check_lcn(l, r.len)?;
                    Some(l * cs + (pos - at(r.vcn)))
                }
                None => None,
            };
            f(pos - off, phys, n)?;
            pos += n;
            idx += 1;
        }
        Ok(())
    }

    fn read_runs(&self, runs: &[Run], off: u64, buf: &mut [u8]) -> FsResult<()> {
        let dev = self.dev.clone();
        self.map_runs(runs, off, buf.len() as u64, |o, phys, n| {
            let dst = &mut buf[o as usize..(o + n) as usize];
            match phys {
                Some(p) => dev.read_at(p, dst)?,
                None => dst.fill(0),
            }
            Ok(())
        })
    }

    /// Write metadata (MFT records, index blocks, bitmaps, $LogFile, ...) through the runs.
    fn write_runs(&self, runs: &[Run], off: u64, data: &[u8]) -> FsResult<()> {
        self.write_runs_as(runs, off, data, false)
    }

    /// Write through the runs; `payload` = regular-file content (see [`Self::is_payload`]),
    /// which goes through `BlockDev::write_data_at` (kept out of the journal's redo log).
    fn write_runs_as(&self, runs: &[Run], off: u64, data: &[u8], payload: bool) -> FsResult<()> {
        let dev = self.dev.clone();
        self.map_runs(runs, off, data.len() as u64, |o, phys, n| match phys {
            Some(p) => Ok(dev_write(dev.as_ref(), p, &data[o as usize..(o + n) as usize], payload)?),
            None => corrupt("write into a sparse run"),
        })
    }

    /// Is the attribute at `l` the non-resident content of a regular file? Only the unnamed
    /// $DATA of a non-system, non-directory, non-reparse-point inode qualifies. Everything
    /// else (MFT records, $INDEX_ALLOCATION, $BITMAP, $ATTRIBUTE_LIST, $REPARSE_POINT,
    /// $Bitmap, $MFTMirr, $LogFile, named streams) is metadata and must stay journaled.
    fn is_payload(ino: &Inode, l: Loc) -> bool {
        let a = ino.attr(l);
        a.ty == AT_DATA
            && a.name.is_empty()
            && ino.no >= FIRST_NON_SYSTEM
            && !ino.is_dir()
            && ino.find(AT_REPARSE_POINT, &[]).is_none()
    }

    fn read_record(&self, no: u64) -> FsResult<MftRecord> {
        let mut raw = self.read_record_raw(no)?;
        fixup_read(&mut raw)?;
        MftRecord::parse(&raw)
    }

    fn read_record_raw(&self, no: u64) -> FsResult<Vec<u8>> {
        let rs = self.rec_size as u64;
        if no.checked_add(1).and_then(|n| n.checked_mul(rs)).map_or(true, |e| e > self.mft_size) {
            return Err(FsError::NotFound);
        }
        let mut raw = vec![0u8; self.rec_size];
        self.read_runs(&self.mft_runs, no * rs, &mut raw)?;
        Ok(raw)
    }

    fn write_record(&mut self, no: u64, r: &mut MftRecord) -> FsResult<()> {
        let raw = r.serialize(no, self.rec_size).ok_or(FsError::NoSpace)?;
        self.write_record_raw(no, &raw)
    }

    fn write_record_raw(&mut self, no: u64, raw: &[u8]) -> FsResult<()> {
        let rs = self.rec_size as u64;
        if no.checked_add(1).and_then(|n| n.checked_mul(rs)).map_or(true, |e| e > self.mft_size) {
            return corrupt("record beyond $MFT");
        }
        self.write_runs(&self.mft_runs, no * rs, raw)?;
        if no < self.mirr_count {
            self.write_runs(&self.mirr_runs, no * rs, raw)?;
        }
        Ok(())
    }

    // -- inodes ------------------------------------------------------------------------------

    fn load_inode(&self, no: u64) -> FsResult<Inode> {
        let base = self.read_record(no)?;
        if base.flags & MFT_IN_USE == 0 || mref_no(base.base) != 0 {
            return Err(FsError::NotFound);
        }
        let mut ino = Inode { no, recs: vec![Rec { no, r: base, dirty: false }], has_al: false };
        let al = ino.recs[0].r.attrs.iter().find(|a| a.ty == AT_ATTRIBUTE_LIST).cloned();
        if let Some(al) = al {
            ino.has_al = true;
            let v = match &al.body {
                Body::Res { value, .. } => value.clone(),
                Body::NonRes(n) => {
                    let mut v = vec![0u8; n.size.min(1 << 20) as usize];
                    self.read_runs(&n.runs, 0, &mut v)?;
                    v
                }
            };
            let mut seen = HashSet::new();
            seen.insert(no);
            for e in parse_attr_list(&v)? {
                let rn = mref_no(e.mref);
                if seen.insert(rn) {
                    let r = self.read_record(rn)?;
                    if r.flags & MFT_IN_USE == 0 || mref_no(r.base) != no {
                        return corrupt(format!("attribute list of {no} points to foreign record {rn}"));
                    }
                    ino.recs.push(Rec { no: rn, r, dirty: false });
                }
            }
        }
        Ok(ino)
    }

    fn store_inode(&mut self, ino: &mut Inode) -> FsResult<()> {
        // link count
        let links = ino.find_all(AT_FILE_NAME, &[]).len() as u16;
        if ino.recs[0].r.link_count != links && ino.no >= FIRST_NON_SYSTEM {
            ino.recs[0].r.link_count = links;
            ino.recs[0].dirty = true;
        }
        if ino.recs.iter().any(|r| r.dirty && !r.r.fits(self.rec_size)) {
            self.relayout(ino)?;
        }
        // drop empty extension records
        let mut i = 1;
        while i < ino.recs.len() {
            if ino.recs[i].r.attrs.is_empty() {
                let rec = ino.recs.remove(i);
                self.free_record(rec.no)?;
                ino.recs[0].dirty = true;
            } else {
                i += 1;
            }
        }
        if ino.has_al && ino.recs.iter().any(|r| r.dirty) {
            self.rebuild_attr_list(ino)?;
        }
        for k in 0..ino.recs.len() {
            if ino.recs[k].dirty {
                let no = ino.recs[k].no;
                let mut r = ino.recs[k].r.clone();
                self.write_record(no, &mut r)?;
                ino.recs[k].r = r;
                ino.recs[k].dirty = false;
            }
        }
        if ino.no == 0 {
            if let Some(Stream::NonRes { runs, size, .. }) = self.stream(ino, AT_DATA, &[])? {
                self.mft_runs = runs;
                self.mft_size = size;
            }
        }
        Ok(())
    }

    /// Move attributes out of overflowing records into extension records (creating an
    /// $ATTRIBUTE_LIST when needed).
    fn relayout(&mut self, ino: &mut Inode) -> FsResult<()> {
        if ino.no < FIRST_NON_SYSTEM {
            return Err(FsError::NoSpace);
        }
        let rs = self.rec_size;
        for _ in 0..64 {
            let Some(ri) = ino.recs.iter().position(|r| !r.r.fits(rs)) else { return Ok(()) };
            if !ino.has_al {
                ino.has_al = true;
                ino.recs[0].r.add_attr(Attribute::resident(AT_ATTRIBUTE_LIST, &[], vec![]));
                ino.recs[0].dirty = true;
                self.rebuild_attr_list(ino)?;
                continue;
            }
            // pick the largest movable attribute
            let pinned = |ri: usize, a: &Attribute| ri == 0 && matches!(a.ty, AT_STANDARD_INFORMATION | AT_ATTRIBUTE_LIST | AT_INDEX_ROOT);
            let cand = ino.recs[ri]
                .r
                .attrs
                .iter()
                .enumerate()
                .filter(|(_, a)| !pinned(ri, a))
                .max_by_key(|(_, a)| a.disk_len())
                .map(|(i, a)| (i, a.disk_len()));
            let Some((ai, alen)) = cand else { return Err(FsError::NoSpace) };
            if attrs_offset(rs) + alen + 8 > rs {
                return Err(FsError::NoSpace); // attribute cannot fit any record
            }
            if ino.recs[ri].r.attrs.len() == 1 && ri != 0 {
                return Err(FsError::NoSpace);
            }
            let a = ino.recs[ri].r.attrs.remove(ai);
            ino.recs[ri].dirty = true;
            let target = (1..ino.recs.len()).find(|&k| k != ri && ino.recs[k].r.used_size(rs) + alen <= rs);
            let k = match target {
                Some(k) => k,
                None => {
                    let no = self.alloc_record()?;
                    let seq = self.record_seq(no);
                    let mut r = MftRecord::new(seq, MFT_IN_USE);
                    r.base = ino.mref();
                    ino.recs.push(Rec { no, r, dirty: true });
                    ino.recs.len() - 1
                }
            };
            ino.recs[k].r.add_attr(a);
            ino.recs[k].dirty = true;
            self.rebuild_attr_list(ino)?;
        }
        Err(FsError::NoSpace)
    }

    fn rebuild_attr_list(&mut self, ino: &mut Inode) -> FsResult<()> {
        let mut ents: Vec<(u32, Vec<u16>, u64, u64, u16)> = vec![];
        for r in &ino.recs {
            for a in &r.r.attrs {
                if a.ty == AT_ATTRIBUTE_LIST {
                    continue;
                }
                let svcn = a.nonres().map_or(0, |n| n.start_vcn);
                ents.push((a.ty, a.name.clone(), svcn, mk_ref(r.no, r.r.seq), a.instance));
            }
        }
        ents.sort_by(|a, b| (a.0, &a.1, a.2).cmp(&(b.0, &b.1, b.2)));
        let mut v = vec![];
        for (ty, name, svcn, mref, inst) in ents {
            let len = align8(0x1A + 2 * name.len());
            let s = v.len();
            v.resize(s + len, 0);
            put32(&mut v[s..], 0, ty);
            put16(&mut v[s..], 4, len as u16);
            v[s + 6] = name.len() as u8;
            v[s + 7] = 0x1A;
            put64(&mut v[s..], 8, svcn);
            put64(&mut v[s..], 16, mref);
            put16(&mut v[s..], 24, inst);
            v[s + 0x1A..s + 0x1A + 2 * name.len()].copy_from_slice(&utf16_bytes(&name));
        }
        let l = ino.find(AT_ATTRIBUTE_LIST, &[]).ok_or_else(|| FsError::Corrupt("attribute list vanished".into()))?;
        if ino.attr(l).nonres().is_some() {
            // Non-resident attribute list: rewrite in place.
            self.resize_attr(ino, AT_ATTRIBUTE_LIST, &[], v.len() as u64, false)?;
            self.write_attr(ino, AT_ATTRIBUTE_LIST, &[], 0, &v, false)?;
        } else if let Body::Res { value, .. } = &mut ino.attr_mut(l).body {
            *value = v;
        }
        Ok(())
    }

    fn record_seq(&self, no: u64) -> u16 {
        match self.read_record(no) {
            Ok(r) if r.seq != 0 => r.seq,
            _ => 1,
        }
    }

    // -- MFT allocation ------------------------------------------------------------------------

    fn mft_records(&self) -> u64 {
        self.mft_size / self.rec_size as u64
    }

    fn mft_bit(&self, no: u64) -> bool {
        self.mft_bitmap.get((no / 8) as usize).map_or(true, |b| b & (1 << (no % 8)) != 0)
    }

    fn set_mft_bit(&mut self, no: u64, v: bool) -> FsResult<()> {
        let byte_i = (no / 8) as usize;
        if byte_i >= self.mft_bitmap.len() {
            return corrupt("MFT bitmap too small");
        }
        if v {
            self.mft_bitmap[byte_i] |= 1 << (no % 8);
        } else {
            self.mft_bitmap[byte_i] &= !(1 << (no % 8));
        }
        let b = [self.mft_bitmap[byte_i]];
        let mut m = self.load_inode(0)?;
        self.write_attr(&mut m, AT_BITMAP, &[], byte_i as u64, &b, true)?;
        self.store_inode(&mut m)
    }

    fn alloc_record(&mut self) -> FsResult<u64> {
        for _ in 0..4 {
            let n = self.mft_records();
            let start = self.mft_hint.max(FIRST_USER_RECORD);
            let found = (start..n).chain(FIRST_USER_RECORD..start.min(n)).find(|&i| !self.mft_bit(i));
            if let Some(i) = found {
                self.set_mft_bit(i, true)?;
                self.mft_hint = i + 1;
                return Ok(i);
            }
            self.extend_mft()?;
        }
        Err(FsError::NoSpace)
    }

    fn extend_mft(&mut self) -> FsResult<()> {
        let rs = self.rec_size as u64;
        let mut m = self.load_inode(0)?;
        let l = m.find(AT_DATA, &[]).ok_or_else(|| FsError::Corrupt("$MFT has no $DATA".into()))?;
        if m.find_all(AT_DATA, &[]).len() != 1 {
            return Err(FsError::Unsupported("growing a fragmented $MFT with an attribute list".into()));
        }
        let grow = ceil_div(64 * rs, self.cluster).max(1);
        let old_records = self.mft_records();
        let (hint, cur_clusters) = match m.attr(l).nonres() {
            Some(n) => (n.runs.iter().rev().find_map(|r| r.lcn.map(|x| x + r.len)).unwrap_or(self.mft_lcn), runs_end(&n.runs)),
            None => return corrupt("resident $MFT data"),
        };
        let got = self.alloc_clusters(grow, hint)?;
        let cs = self.cluster;
        let new_size;
        {
            let Body::NonRes(n) = &mut m.attr_mut(l).body else { unreachable!() };
            for &(lcn, len) in &got {
                push_run(&mut n.runs, Some(lcn), len);
            }
            n.alloc = (cur_clusters + grow).checked_mul(cs).filter(|&b| b <= self.dev.size()).ok_or_else(|| FsError::Corrupt("$MFT larger than the volume".into()))?;
            new_size = n.alloc / rs * rs;
            n.size = new_size;
            n.init = new_size;
            self.mft_runs = n.runs.clone();
        }
        self.mft_size = new_size;
        for r in old_records..new_size / rs {
            let raw = empty_record(r, self.rec_size, 1);
            self.write_runs(&self.mft_runs.clone(), r * rs, &raw)?;
        }
        let need = align8(ceil_div(new_size / rs, 8) as usize);
        if self.mft_bitmap.len() < need {
            self.resize_attr(&mut m, AT_BITMAP, &[], need as u64, true)?;
            self.mft_bitmap.resize(need, 0);
        }
        // keep $MFT's own $FILE_NAME duplicate info roughly right
        if let Some((fl, mut f)) = m.file_names().into_iter().next() {
            f.alloc = (cur_clusters + grow) * cs;
            f.size = new_size;
            if let Body::Res { value, .. } = &mut m.attr_mut(fl).body {
                *value = f.to_bytes();
            }
        }
        self.store_inode(&mut m)
    }

    fn free_record(&mut self, no: u64) -> FsResult<()> {
        if let Ok(mut r) = self.read_record(no) {
            r.flags &= !MFT_IN_USE;
            r.seq = r.seq.wrapping_add(1);
            if r.seq == 0 {
                r.seq = 1;
            }
            self.write_record(no, &mut r)?;
        }
        self.set_mft_bit(no, false)?;
        if no < self.mft_hint {
            self.mft_hint = no.max(FIRST_USER_RECORD);
        }
        Ok(())
    }

    fn free_inode(&mut self, ino: Inode) -> FsResult<()> {
        if let Some(tag) = self.reparse_tag_of(&ino)? {
            self.r_remove(tag, ino.mref())?;
        }
        for r in &ino.recs {
            for a in &r.r.attrs {
                if let Some(n) = a.nonres() {
                    let runs = n.runs.clone();
                    self.free_runs(&runs)?;
                }
            }
        }
        for r in ino.recs.iter().skip(1) {
            self.free_record(r.no)?;
        }
        self.dup_dirty.remove(&ino.no);
        self.free_record(ino.no)
    }

    // -- cluster bitmap --------------------------------------------------------------------------

    fn vbm(&mut self) -> FsResult<&mut Vec<u8>> {
        if self.vbm.is_none() {
            let n = ceil_div(self.total_clusters, 8) as usize;
            let mut v = vec![0u8; n];
            self.read_runs(&self.vbm_runs, 0, &mut v)?;
            let full = (self.total_clusters / 8) as usize;
            let mut used: u64 = v[..full].iter().map(|b| b.count_ones() as u64).sum();
            for c in (full as u64 * 8)..self.total_clusters {
                if v[(c / 8) as usize] & (1 << (c % 8)) != 0 {
                    used += 1;
                }
            }
            let free = self.total_clusters - used;
            self.free_clusters = free;
            self.vbm = Some(v);
        }
        Ok(self.vbm.as_mut().unwrap())
    }

    fn flush_vbm(&mut self, first: u64, last: u64) -> FsResult<()> {
        let (a, b) = ((first / 8) as usize, (last / 8) as usize);
        let bytes = self.vbm()?[a..=b].to_vec();
        self.write_runs(&self.vbm_runs.clone(), a as u64, &bytes)
    }

    fn set_clusters(&mut self, lcn: u64, len: u64, used: bool) -> FsResult<()> {
        if len == 0 {
            return Ok(());
        }
        self.check_lcn(lcn, len)?;
        if !used {
            self.dev.note_freed(lcn * self.cluster, len * self.cluster);
        }
        let total = self.total_clusters;
        let v = self.vbm()?;
        let mut changed = 0u64;
        for c in lcn..(lcn + len).min(total) {
            let (i, m) = ((c / 8) as usize, 1u8 << (c % 8));
            let was = v[i] & m != 0;
            if was != used {
                changed += 1;
                if used {
                    v[i] |= m
                } else {
                    v[i] &= !m
                }
            }
        }
        if used {
            self.free_clusters -= changed;
        } else {
            self.free_clusters += changed;
        }
        self.flush_vbm(lcn, (lcn + len - 1).min(total - 1))
    }

    fn free_runs(&mut self, runs: &[Run]) -> FsResult<()> {
        for r in runs {
            if let Some(l) = r.lcn {
                self.set_clusters(l, r.len, false)?;
            }
        }
        Ok(())
    }

    /// Allocate `n` clusters, preferring one contiguous extent at/after `hint`.
    fn alloc_clusters(&mut self, n: u64, hint: u64) -> FsResult<Vec<(u64, u64)>> {
        if n == 0 {
            return Ok(vec![]);
        }
        let total = self.total_clusters;
        self.vbm()?;
        if self.free_clusters < n {
            return Err(FsError::NoSpace);
        }
        let v = self.vbm.as_ref().unwrap();
        let is_free = |c: u64| v[(c / 8) as usize] & (1 << (c % 8)) == 0;
        let hint = if hint >= total { 0 } else { hint };
        // pass 1: first contiguous run of n clusters starting the scan at hint (wrapping)
        let mut found = None;
        let mut fragments: Vec<(u64, u64)> = vec![];
        let mut need = n;
        'outer: for (a, b) in [(hint, total), (0, hint)] {
            let mut c = a;
            while c < b {
                if v[(c / 8) as usize] == 0xFF && c % 8 == 0 {
                    c += 8;
                    continue;
                }
                if !is_free(c) {
                    c += 1;
                    continue;
                }
                let s = c;
                while c < b && is_free(c) && c - s < n {
                    c += 1;
                }
                if c - s >= n {
                    found = Some(s);
                    break 'outer;
                }
                if need > 0 {
                    let take = (c - s).min(need);
                    fragments.push((s, take));
                    need -= take;
                }
            }
        }
        let out = match found {
            Some(s) => vec![(s, n)],
            None if need == 0 => fragments,
            None => return Err(FsError::NoSpace),
        };
        for &(l, len) in &out {
            self.set_clusters(l, len, true)?;
        }
        if let Some(&(l, len)) = out.last() {
            if hint == self.data_hint || l >= self.data_hint {
                self.data_hint = l + len;
            }
        }
        Ok(out)
    }

    // -- streams -----------------------------------------------------------------------------

    fn stream(&self, ino: &Inode, ty: u32, name: &[u16]) -> FsResult<Option<Stream>> {
        let locs = ino.find_all(ty, name);
        if locs.is_empty() {
            return Ok(None);
        }
        let first = ino.attr(ino.find(ty, name).unwrap());
        match &first.body {
            Body::Res { value, .. } => Ok(Some(Stream::Res(value.clone()))),
            Body::NonRes(n0) => {
                let mut ext: Vec<&NonRes> = locs.iter().filter_map(|&l| ino.attr(l).nonres()).collect();
                ext.sort_by_key(|n| n.start_vcn);
                let mut runs = vec![];
                for e in ext {
                    if e.start_vcn != runs_end(&runs) {
                        return corrupt("attribute extents are not contiguous");
                    }
                    runs.extend_from_slice(&e.runs);
                }
                if runs_end(&runs).saturating_mul(self.cluster) < n0.size.min(n0.alloc) {
                    return corrupt("runlist shorter than attribute size");
                }
                Ok(Some(Stream::NonRes { runs, size: n0.size, init: n0.init.min(n0.size), flags: first.flags, comp_unit: n0.comp_unit }))
            }
        }
    }

    fn read_stream(&self, s: &Stream, off: u64, len: u64) -> FsResult<Vec<u8>> {
        match s {
            Stream::Res(v) => {
                let a = (off as usize).min(v.len());
                let b = (off.saturating_add(len) as usize).min(v.len());
                Ok(v[a..b].to_vec())
            }
            Stream::NonRes { runs, size, init, flags, comp_unit } => {
                if off >= *size {
                    return Ok(vec![]);
                }
                let end = off.saturating_add(len).min(*size);
                let mut out = vec![0u8; (end - off) as usize];
                if flags & ATTR_ENCRYPTED != 0 {
                    return Err(FsError::Unsupported("EFS-encrypted file".into()));
                }
                let iend = end.min(*init);
                if off < iend {
                    if flags & ATTR_COMPRESSION_MASK != 0 && *comp_unit != 0 {
                        self.read_compressed(runs, *comp_unit, off, &mut out[..(iend - off) as usize])?;
                    } else {
                        self.read_runs(runs, off, &mut out[..(iend - off) as usize])?;
                    }
                }
                Ok(out)
            }
        }
    }

    fn read_compressed(&self, runs: &[Run], comp_unit: u8, off: u64, out: &mut [u8]) -> FsResult<()> {
        // Windows only writes 16-cluster units with clusters <= 4 KiB; bound the unit buffer.
        let cs = self.cluster;
        if comp_unit > 8 || (cs << comp_unit) > 1 << 20 {
            return corrupt("bad compression unit");
        }
        let cu_cl = 1u64 << comp_unit;
        let cu_bytes = cu_cl * cs;
        let end = off + out.len() as u64;
        let mut cu = off / cu_bytes;
        while cu * cu_bytes < end {
            let v0 = cu * cu_cl;
            // physical layout of this compression unit
            let mut segs: Vec<(Option<u64>, u64)> = vec![];
            let mut vcn = v0;
            while vcn < v0 + cu_cl {
                let i = runs.partition_point(|r| r.vcn + r.len <= vcn);
                let Some(r) = runs.get(i).filter(|r| r.vcn <= vcn) else { break };
                let n = (r.vcn + r.len).min(v0 + cu_cl) - vcn;
                segs.push((r.lcn.map(|l| l + (vcn - r.vcn)), n));
                vcn += n;
            }
            let allocated: u64 = segs.iter().filter(|s| s.0.is_some()).map(|s| s.1).sum();
            let mut buf = vec![0u8; cu_bytes as usize];
            if allocated == 0 {
                // hole
            } else if allocated >= cu_cl {
                let mut o = 0usize;
                for (l, n) in &segs {
                    if let Some(l) = l {
                        self.check_lcn(*l, *n)?;
                        self.dev.read_at(l * cs, &mut buf[o..o + (*n * cs) as usize])?;
                    }
                    o += (*n * cs) as usize;
                }
            } else {
                let mut comp = Vec::with_capacity((allocated * cs) as usize);
                for (l, n) in &segs {
                    if let Some(l) = l {
                        self.check_lcn(*l, *n)?;
                        comp.extend_from_slice(&self.dev.read_vec(l * cs, (*n * cs) as usize)?);
                    }
                }
                lznt1_decompress(&comp, &mut buf)?;
            }
            let cs_off = cu * cu_bytes;
            let a = off.max(cs_off);
            let b = end.min(cs_off + cu_bytes);
            out[(a - off) as usize..(b - off) as usize].copy_from_slice(&buf[(a - cs_off) as usize..(b - cs_off) as usize]);
            cu += 1;
        }
        Ok(())
    }

    fn single_loc(&self, ino: &Inode, ty: u32, name: &[u16]) -> FsResult<Loc> {
        let all = ino.find_all(ty, name);
        match all.len() {
            0 => Err(FsError::NotFound),
            1 => {
                let l = all[0];
                let a = ino.attr(l);
                if a.flags & (ATTR_COMPRESSION_MASK | ATTR_ENCRYPTED) != 0 {
                    return Err(FsError::Unsupported("writing compressed or encrypted attributes".into()));
                }
                Ok(l)
            }
            _ => Err(FsError::Unsupported("modifying an attribute split across several MFT records".into())),
        }
    }

    /// Make sure at least `bytes` are allocated (and holes in `[from, bytes)` are filled).
    fn ensure_alloc(&mut self, ino: &mut Inode, l: Loc, from: u64, bytes: u64) -> FsResult<()> {
        let cs = self.cluster;
        let need = ceil_div(bytes, cs);
        let (have, hint, has_holes) = {
            let n = ino.attr(l).nonres().unwrap();
            let hint = n.runs.iter().rev().find_map(|r| r.lcn.map(|x| x + r.len)).unwrap_or(self.data_hint);
            let v0 = from / cs;
            let holes = n.runs.iter().any(|r| r.lcn.is_none() && r.vcn + r.len > v0 && r.vcn < need);
            (runs_end(&n.runs), hint, holes)
        };
        if has_holes {
            let payload = Self::is_payload(ino, l);
            self.fill_holes(ino, l, from / cs, need, payload)?;
        }
        if need > have {
            let got = self.alloc_clusters(need - have, hint)?;
            let sparse = ino.attr(l).flags & ATTR_SPARSE != 0;
            let a = ino.attr_mut(l);
            let Body::NonRes(n) = &mut a.body else { unreachable!() };
            for &(lcn, len) in &got {
                push_run(&mut n.runs, Some(lcn), len);
            }
            n.alloc = need * cs;
            if sparse {
                n.comp_size += (need - have) * cs;
            }
        }
        Ok(())
    }

    /// Allocate real clusters for sparse runs intersecting `[v0, v1)` (zero-filled).
    fn fill_holes(&mut self, ino: &mut Inode, l: Loc, v0: u64, v1: u64, payload: bool) -> FsResult<()> {
        let cs = self.cluster;
        let old = ino.attr(l).nonres().unwrap().runs.clone();
        let mut new = vec![];
        let mut added = 0;
        for r in old {
            if r.lcn.is_some() || r.vcn + r.len <= v0 || r.vcn >= v1 {
                push_run(&mut new, r.lcn, r.len);
                continue;
            }
            let a = r.vcn.max(v0);
            let b = (r.vcn + r.len).min(v1);
            push_run(&mut new, None, a - r.vcn);
            for (lcn, len) in self.alloc_clusters(b - a, self.data_hint)? {
                let zeros = vec![0u8; (len * cs) as usize];
                dev_write(self.dev.as_ref(), lcn * cs, &zeros, payload)?;
                push_run(&mut new, Some(lcn), len);
                added += len;
            }
            push_run(&mut new, None, r.vcn + r.len - b);
        }
        let a = ino.attr_mut(l);
        let Body::NonRes(n) = &mut a.body else { unreachable!() };
        n.runs = new;
        n.comp_size += added * cs;
        Ok(())
    }

    fn write_zeros(&self, runs: &[Run], from: u64, to: u64, payload: bool) -> FsResult<()> {
        let chunk = 1 << 20;
        let zeros = vec![0u8; chunk];
        let mut p = from;
        while p < to {
            let n = ((to - p) as usize).min(chunk);
            // skip sparse runs (they read as zeros anyway)
            let dev = self.dev.clone();
            self.map_runs(runs, p, n as u64, |o, phys, len| {
                if let Some(ph) = phys {
                    dev_write(dev.as_ref(), ph, &zeros[o as usize..(o + len) as usize], payload)?;
                }
                Ok(())
            })?;
            p += n as u64;
        }
        Ok(())
    }

    fn make_nonresident(&mut self, ino: &mut Inode, l: Loc, content: &[u8]) -> FsResult<()> {
        let cs = self.cluster;
        let ncl = ceil_div(content.len() as u64, cs);
        let got = self.alloc_clusters(ncl, self.data_hint)?;
        let mut runs = vec![];
        for &(lcn, len) in &got {
            push_run(&mut runs, Some(lcn), len);
        }
        if ncl > 0 {
            let mut padded = content.to_vec();
            padded.resize((ncl * cs) as usize, 0);
            if let Err(e) = self.write_runs_as(&runs, 0, &padded, Self::is_payload(ino, l)) {
                let _ = self.free_runs(&runs);
                return Err(e);
            }
        }
        let a = ino.attr_mut(l);
        let flags = a.flags;
        a.body = Body::NonRes(NonRes {
            start_vcn: 0,
            last_vcn: ncl as i64 - 1,
            runs,
            alloc: ncl * cs,
            size: content.len() as u64,
            init: content.len() as u64,
            comp_unit: 0,
            comp_size: if flags & ATTR_SPARSE != 0 { ncl * cs } else { 0 },
        });
        Ok(())
    }

    /// Replace a resident value if it stays within limits and the record still fits.
    fn try_set_resident(&mut self, ino: &mut Inode, l: Loc, v: Vec<u8>) -> bool {
        let a = &ino.recs[l.0].r.attrs[l.1];
        let old_len = a.data_size() as usize;
        let limit = if a.ty == AT_DATA { RESIDENT_MAX } else { usize::MAX };
        if v.len() > limit && v.len() > old_len {
            return false;
        }
        let Body::Res { value, .. } = &mut ino.recs[l.0].r.attrs[l.1].body else { return false };
        let old = std::mem::replace(value, v);
        if old.len() >= ino.recs[l.0].r.attrs[l.1].data_size() as usize || ino.recs[l.0].r.fits(self.rec_size) {
            ino.recs[l.0].dirty = true;
            return true;
        }
        if let Body::Res { value, .. } = &mut ino.recs[l.0].r.attrs[l.1].body {
            *value = old;
        }
        false
    }

    fn write_attr(&mut self, ino: &mut Inode, ty: u32, name: &[u16], off: u64, data: &[u8], resident_ok: bool) -> FsResult<()> {
        let l = self.single_loc(ino, ty, name)?;
        let end = off.checked_add(data.len() as u64).ok_or(FsError::Invalid("offset overflow".into()))?;
        if end > (1u64 << 48) {
            return Err(FsError::NoSpace);
        }
        if let Some(value) = ino.attr(l).value() {
            let mut v = value.to_vec();
            let new_len = v.len().max(end as usize);
            v.resize(new_len, 0);
            v[off as usize..end as usize].copy_from_slice(data);
            if resident_ok && self.try_set_resident(ino, l, v.clone()) {
                return Ok(());
            }
            return self.make_nonresident(ino, l, &v);
        }
        if data.is_empty() {
            return Ok(());
        }
        self.ensure_alloc(ino, l, off, end)?;
        let (runs, init, size) = {
            let n = ino.attr(l).nonres().unwrap();
            (n.runs.clone(), n.init, n.size)
        };
        let payload = Self::is_payload(ino, l);
        if off > init {
            self.write_zeros(&runs, init, off, payload)?;
        }
        self.write_runs_as(&runs, off, data, payload)?;
        if end > init || end > size {
            let a = ino.attr_mut(l);
            let Body::NonRes(n) = &mut a.body else { unreachable!() };
            n.init = n.init.max(end);
            n.size = n.size.max(end);
        }
        Ok(())
    }

    fn resize_attr(&mut self, ino: &mut Inode, ty: u32, name: &[u16], new_size: u64, resident_ok: bool) -> FsResult<()> {
        let l = self.single_loc(ino, ty, name)?;
        if new_size > (1u64 << 48) {
            return Err(FsError::NoSpace);
        }
        let cs = self.cluster;
        if let Some(value) = ino.attr(l).value() {
            let shrinking = (new_size as usize) <= value.len();
            let mut v = value.to_vec();
            if new_size as usize == v.len() {
                return Ok(());
            }
            v.resize(new_size as usize, 0);
            if (shrinking || resident_ok) && self.try_set_resident(ino, l, v.clone()) {
                return Ok(());
            }
            return self.make_nonresident(ino, l, &v);
        }
        let (size, alloc_cl) = {
            let n = ino.attr(l).nonres().unwrap();
            (n.size, runs_end(&n.runs))
        };
        if new_size == size {
            return Ok(());
        }
        if new_size < size {
            let keep = ceil_div(new_size, cs);
            if keep < alloc_cl {
                let runs = ino.attr(l).nonres().unwrap().runs.clone();
                let mut kept = vec![];
                let mut freed = vec![];
                let mut freed_real = 0;
                for r in runs {
                    if r.vcn + r.len <= keep {
                        kept.push(r);
                    } else if r.vcn >= keep {
                        freed.push(r);
                    } else {
                        let k = keep - r.vcn;
                        kept.push(Run { vcn: r.vcn, lcn: r.lcn, len: k });
                        freed.push(Run { vcn: keep, lcn: r.lcn.map(|x| x + k), len: r.len - k });
                    }
                }
                for r in &freed {
                    if r.lcn.is_some() {
                        freed_real += r.len;
                    }
                }
                self.free_runs(&freed)?;
                let a = ino.attr_mut(l);
                let Body::NonRes(n) = &mut a.body else { unreachable!() };
                n.runs = kept;
                n.alloc = keep * cs;
                n.comp_size = n.comp_size.saturating_sub(freed_real * cs);
            }
            let a = ino.attr_mut(l);
            let Body::NonRes(n) = &mut a.body else { unreachable!() };
            n.size = new_size;
            n.init = n.init.min(new_size);
        } else {
            if ino.attr(l).flags & ATTR_SPARSE == 0 {
                self.ensure_alloc(ino, l, size, new_size)?;
            }
            let a = ino.attr_mut(l);
            let Body::NonRes(n) = &mut a.body else { unreachable!() };
            n.size = new_size;
            if n.alloc < new_size {
                // sparse attribute: extend with a hole
                let have = runs_end(&n.runs);
                let need = ceil_div(new_size, cs);
                push_run(&mut n.runs, None, need - have);
                n.alloc = need * cs;
            }
        }
        Ok(())
    }

    // -- helpers for the VFS layer -----------------------------------------------------------------

    fn writable(&self) -> FsResult<()> {
        if self.ro {
            Err(FsError::ReadOnly)
        } else {
            Ok(())
        }
    }

    fn up(&self, c: u16) -> u16 {
        self.upcase.get(c as usize).copied().unwrap_or(c)
    }

    /// Case-insensitive order (upcase table), ties broken by length.
    fn cmp_ci(&self, a: &[u16], b: &[u16]) -> Ordering {
        for (x, y) in a.iter().zip(b.iter()) {
            match self.up(*x).cmp(&self.up(*y)) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        a.len().cmp(&b.len())
    }

    /// Full $I30 collation: case-insensitive first, then binary.
    fn cmp_full(&self, a: &[u16], b: &[u16]) -> Ordering {
        self.cmp_ci(a, b).then_with(|| a.cmp(b))
    }

    fn encode_name(&self, name: &str) -> FsResult<Vec<u16>> {
        check_name(name, 1020)?;
        let u = to_utf16(name);
        if u.len() > MAX_NAME_UNITS {
            return Err(FsError::NameTooLong);
        }
        Ok(u)
    }

    fn data_sizes(&self, ino: &Inode) -> (u64, u64) {
        if ino.is_dir() {
            return (0, 0);
        }
        match ino.find(AT_DATA, &[]).map(|l| ino.attr(l)) {
            Some(a) => match &a.body {
                Body::Res { value, .. } => (value.len() as u64, align8(value.len()) as u64),
                Body::NonRes(n) => (n.size, if a.flags & (ATTR_SPARSE | ATTR_COMPRESSION_MASK) != 0 { n.comp_size } else { n.alloc }),
            },
            None => (0, 0),
        }
    }

    fn make_file_name(&self, ino: &Inode, parent_ref: u64, name: &[u16]) -> FileName {
        let si = ino.std_info();
        let (size, alloc) = self.data_sizes(ino);
        // Like ntfs-3g's default, new names go into the POSIX namespace: a lone WIN32 name
        // without a DOS alias cannot be unlinked by ntfs-3g (it insists on the DOS twin).
        let ns = NS_POSIX;
        FileName {
            parent: parent_ref,
            crtime: si.crtime,
            mtime: si.mtime,
            ctime: si.ctime,
            atime: si.atime,
            alloc,
            size,
            flags: (si.attrs & !FILE_ATTR_DUP_DIR) | if ino.is_dir() { FILE_ATTR_DUP_DIR } else { 0 },
            // DUPLICATED_INFORMATION.ReparsePointTag, as Windows and ntfs-3g keep it
            reparse: if si.attrs & FILE_ATTR_REPARSE != 0 { self.reparse_tag_of(ino).ok().flatten().unwrap_or(0) } else { 0 },
            ns,
            name: name.to_vec(),
        }
    }

    fn attr_of(&self, ino: &Inode) -> FsResult<Attr> {
        let si = ino.std_info();
        let names = ino.file_names();
        let nlink = names.iter().filter(|(_, f)| f.ns != NS_DOS).count().max(1) as u32;
        let (kind, size, used, perm) = if let Some(t) = self.symlink_target(ino)? {
            (FileKind::Symlink, t.len() as u64, self.data_sizes(ino).1, 0o777)
        } else if ino.is_dir() {
            let sz = ino
                .find(AT_INDEX_ALLOCATION, I30)
                .map(|l| ino.attr(l).data_size())
                .or_else(|| ino.find(AT_INDEX_ROOT, I30).map(|l| ino.attr(l).data_size()))
                .unwrap_or(0);
            (FileKind::Dir, sz, sz, self.defaults.dir_perm)
        } else {
            let (s, u) = self.data_sizes(ino);
            let mut p = self.defaults.file_perm;
            if si.attrs & FILE_ATTR_READONLY != 0 {
                p &= !0o222;
            }
            (FileKind::File, s, u, p)
        };
        Ok(Attr {
            ino: ino.no,
            kind,
            size,
            perm: perm & 0o7777,
            nlink,
            uid: self.defaults.uid,
            gid: self.defaults.gid,
            atime: ntfs_to_ts(si.atime),
            mtime: ntfs_to_ts(si.mtime),
            ctime: ntfs_to_ts(si.ctime),
            used_bytes: used,
        })
    }

    // -- reparse points / symlinks --------------------------------------------------------------

    /// Raw `$REPARSE_POINT` value, if the inode has one.
    fn reparse_value(&self, ino: &Inode) -> FsResult<Option<Vec<u8>>> {
        match self.stream(ino, AT_REPARSE_POINT, &[])? {
            None => Ok(None),
            Some(s) => {
                let n = s.size().min(16 * 1024 + 24);
                Ok(Some(self.read_stream(&s, 0, n)?))
            }
        }
    }

    fn reparse_tag_of(&self, ino: &Inode) -> FsResult<Option<u32>> {
        Ok(self.reparse_value(ino)?.and_then(|v| tag_of(&v)))
    }

    /// POSIX target if the inode is a symlink (reparse symlink / junction / WSL / Interix).
    fn symlink_target(&self, ino: &Inode) -> FsResult<Option<String>> {
        if let Some(v) = self.reparse_value(ino)? {
            return Ok(parse_symlink(&v));
        }
        // Interix symlink (what ntfs-3g's `ln -s` creates): SYSTEM file, "IntxLNK\1" + UTF-16LE.
        if ino.is_dir() || ino.no < FIRST_NON_SYSTEM || ino.std_info().attrs & FILE_ATTR_SYSTEM == 0 {
            return Ok(None);
        }
        let Some(s) = self.stream(ino, AT_DATA, &[])? else { return Ok(None) };
        let sz = s.size();
        if sz <= INTERIX_MAGIC.len() as u64 || sz > (INTERIX_MAGIC.len() + 2 * MAX_TARGET + 2) as u64 {
            return Ok(None);
        }
        match self.read_stream(&s, 0, INTERIX_MAGIC.len() as u64) {
            Ok(h) if h[..] == INTERIX_MAGIC[..] => {}
            _ => return Ok(None),
        }
        Ok(self.read_stream(&s, 0, sz).ok().and_then(|d| parse_interix(&d)))
    }

    fn kind_of(&self, ino: &Inode) -> FsResult<FileKind> {
        Ok(if self.symlink_target(ino)?.is_some() {
            FileKind::Symlink
        } else if ino.is_dir() {
            FileKind::Dir
        } else {
            FileKind::File
        })
    }

    /// Resolve a relative symlink target from `dir` the way ntfs-3g does (case-insensitive,
    /// `.`/`..`, stopping at the first reparse point). True if it ends on a real directory.
    fn rel_target_is_dir(&mut self, dir: Ino, target: &str) -> bool {
        let mut cur = dir;
        let parts: Vec<&str> = target.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
        for (i, p) in parts.iter().enumerate() {
            let next = if *p == ".." { self.lookup(cur, "..") } else { self.lookup_child(cur, p).map(|(e, _)| mref_no(e.data)) };
            let Ok(next) = next else { return false };
            let Ok(n) = self.load_inode(next) else { return false };
            if n.find(AT_REPARSE_POINT, &[]).is_some() || !n.is_dir() {
                return false; // through another link (type not checked) or a file
            }
            if i + 1 == parts.len() {
                return true;
            }
            cur = next;
        }
        false
    }

    /// Record number of `$Extend/$Reparse` (None on volumes without it).
    fn reparse_index(&mut self) -> FsResult<Option<u64>> {
        if let Some(v) = self.reparse_ino {
            return Ok(v);
        }
        let found = match self.dir_find(11, &to_utf16(REPARSE_NAME), false) {
            Ok(Some(e)) => {
                let no = mref_no(e.data);
                match self.load_inode(no) {
                    Ok(i) if i.find(AT_INDEX_ROOT, R_INDEX).is_some() => Some(no),
                    _ => None,
                }
            }
            Ok(None) | Err(FsError::NotDir) | Err(FsError::NotFound) => None,
            Err(e) => return Err(e),
        };
        self.reparse_ino = Some(found);
        Ok(found)
    }

    fn r_insert(&mut self, tag: u32, mref: u64) -> FsResult<()> {
        let Some(no) = self.reparse_index()? else { return Ok(()) };
        match self.view_insert(no, R_INDEX, Coll::Ulongs, R_ENTRY_DATA, r_key(tag, mref)) {
            Ok(()) | Err(FsError::Exists) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn r_remove(&mut self, tag: u32, mref: u64) -> FsResult<()> {
        let Some(no) = self.reparse_index()? else { return Ok(()) };
        match self.view_remove(no, R_INDEX, Coll::Ulongs, &r_key(tag, mref)) {
            Ok(()) | Err(FsError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn user_inode(&self, ino: Ino) -> FsResult<Inode> {
        if ino == 0 || ino > (1 << 48) {
            return Err(FsError::Stale);
        }
        match self.load_inode(ino) {
            Err(FsError::NotFound) => Err(FsError::Stale),
            r => r,
        }
    }

    fn touch(&mut self, ino: &mut Inode, data_changed: bool) {
        let now = ntfs_now();
        let mut si = ino.std_info();
        si.ctime = now;
        if data_changed {
            si.mtime = now;
            if !ino.is_dir() {
                si.attrs |= FILE_ATTR_ARCHIVE;
            }
        }
        ino.set_std_info(&si);
    }

    fn touch_dir(&mut self, dir: Ino) -> FsResult<()> {
        let mut d = self.load_inode(dir)?;
        self.touch(&mut d, true);
        self.store_inode(&mut d)?;
        self.mark_dup(dir)
    }

    fn mark_dup(&mut self, ino: Ino) -> FsResult<()> {
        if ino == ROOT_INO || ino < FIRST_NON_SYSTEM {
            return Ok(());
        }
        self.dup_dirty.insert(ino);
        if self.dup_dirty.len() > DUP_FLUSH_THRESHOLD {
            self.flush_dup()?;
        }
        Ok(())
    }

    /// Push $STANDARD_INFORMATION times and data sizes into every $FILE_NAME of the inode and
    /// into the corresponding parent index entries.
    fn sync_dup(&mut self, no: Ino) -> FsResult<()> {
        let mut ino = match self.load_inode(no) {
            Ok(i) => i,
            Err(FsError::NotFound) => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut updates = vec![];
        for (l, f) in ino.file_names() {
            let mut nf = self.make_file_name(&ino, f.parent, &f.name);
            nf.ns = f.ns;
            if nf.flags & FILE_ATTR_REPARSE == 0 {
                nf.reparse = f.reparse; // packed EA size when not a reparse point
            }
            const FROM_SI: u32 = FILE_ATTR_ARCHIVE | FILE_ATTR_READONLY | FILE_ATTR_SYSTEM | FILE_ATTR_REPARSE | FILE_ATTR_DUP_DIR;
            nf.flags = (f.flags & !FROM_SI) | (nf.flags & FROM_SI);
            let bytes = nf.to_bytes();
            if ino.attr(l).value() != Some(&bytes[..]) {
                if let Body::Res { value, .. } = &mut ino.attr_mut(l).body {
                    *value = bytes.clone();
                }
            }
            updates.push((mref_no(f.parent), bytes));
        }
        self.store_inode(&mut ino)?;
        for (parent, key) in updates {
            match self.dir_update_key(parent, key, no) {
                Ok(()) | Err(FsError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn flush_dup(&mut self) -> FsResult<()> {
        let mut v: Vec<u64> = self.dup_dirty.drain().collect();
        v.sort_unstable();
        for no in v {
            self.sync_dup(no)?;
        }
        Ok(())
    }

    /// Find `name` in directory `dir`: exact (full collation) match first, then case-insensitive.
    fn find_entry(&mut self, dir: Ino, name: &[u16]) -> FsResult<Option<IEntry>> {
        if let Some(e) = self.dir_find(dir, name, true)? {
            return Ok(Some(e));
        }
        self.dir_find(dir, name, false)
    }

    fn lookup_child(&mut self, parent: Ino, name: &str) -> FsResult<(IEntry, FileName)> {
        let u = to_utf16(name);
        if u.is_empty() || u.len() > MAX_NAME_UNITS {
            return Err(if u.is_empty() { FsError::InvalidName } else { FsError::NameTooLong });
        }
        let e = self.find_entry(parent, &u)?.ok_or(FsError::NotFound)?;
        if parent == ROOT_INO && mref_no(e.data) < 16 {
            return Err(FsError::NotFound);
        }
        let f = FileName::parse(&e.key)?;
        Ok((e, f))
    }

    fn create_node(&mut self, parent: Ino, name: &str, kind: NewNode) -> FsResult<Ino> {
        let dir = matches!(kind, NewNode::Dir | NewNode::Symlink(_, _, true));
        self.writable()?;
        let uname = self.encode_name(name)?;
        let p = self.user_inode(parent)?;
        if !p.is_dir() {
            return Err(FsError::NotDir);
        }
        if parent != ROOT_INO && parent < FIRST_NON_SYSTEM {
            return Err(FsError::Perm);
        }
        if self.find_entry(parent, &uname)?.is_some() {
            return Err(FsError::Exists);
        }
        let no = self.alloc_record()?;
        let seq = self.record_seq(no);
        let now = ntfs_now();
        let attrs = match kind {
            NewNode::Dir => 0,
            NewNode::File => FILE_ATTR_ARCHIVE,
            NewNode::Symlink(_, _, true) => FILE_ATTR_REPARSE,
            NewNode::Symlink(..) => FILE_ATTR_ARCHIVE | FILE_ATTR_REPARSE,
        };
        let si = StdInfo { crtime: now, mtime: now, ctime: now, atime: now, attrs };
        let mut r = MftRecord::new(seq, MFT_IN_USE | if dir { MFT_IS_DIR } else { 0 });
        r.link_count = 1;
        r.add_attr(Attribute::resident(AT_STANDARD_INFORMATION, &[], si.new_value(0)));
        let mut ino = Inode { no, recs: vec![Rec { no, r, dirty: true }], has_al: false };
        let mut reparse_tag = None;
        match kind {
            NewNode::Dir => {
                let hdr = self.new_root_hdr();
                ino.add(Attribute::resident(AT_INDEX_ROOT, I30, index_root_value(&hdr, &Node::empty_leaf())));
            }
            NewNode::File => ino.add(Attribute::resident(AT_DATA, &[], vec![])),
            NewNode::Symlink(tag, value, is_dir) => {
                if is_dir {
                    // like `mklink /D`: an empty directory carrying the reparse point
                    let hdr = self.new_root_hdr();
                    ino.add(Attribute::resident(AT_INDEX_ROOT, I30, index_root_value(&hdr, &Node::empty_leaf())));
                } else {
                    ino.add(Attribute::resident(AT_DATA, &[], vec![]));
                }
                ino.add(Attribute::resident(AT_REPARSE_POINT, &[], value));
                reparse_tag = Some(tag);
            }
        }
        let fname = self.make_file_name(&ino, p.mref(), &uname);
        let key = fname.to_bytes();
        let mut fa = Attribute::resident(AT_FILE_NAME, &[], key.clone());
        fa.body = Body::Res { value: key.clone(), rflags: 1 };
        ino.add(fa);
        ino.add(Attribute::resident(AT_SECURITY_DESCRIPTOR, &[], sd_everyone()));
        let mut res = Ok(());
        if reparse_tag.is_some() && !ino.recs[0].r.fits(self.rec_size) {
            // very long target: keep the reparse data out of the record
            let l = ino.find(AT_REPARSE_POINT, &[]).unwrap();
            let v = ino.attr(l).value().unwrap_or(&[]).to_vec();
            res = self.make_nonresident(&mut ino, l, &v);
        }
        let mref = ino.mref();
        let res = res.and_then(|_| self.store_inode(&mut ino)).and_then(|_| self.dir_insert(parent, key, mref));
        if let Err(e) = res {
            for r in &ino.recs {
                for a in &r.r.attrs {
                    if let Some(n) = a.nonres() {
                        let runs = n.runs.clone();
                        let _ = self.free_runs(&runs);
                    }
                }
            }
            for r in ino.recs.iter().skip(1) {
                let _ = self.free_record(r.no);
            }
            let _ = self.free_record(no);
            return Err(e);
        }
        if let Some(tag) = reparse_tag {
            if let Err(e) = self.r_insert(tag, mref) {
                let _ = self.dir_remove(parent, &uname, no);
                let _ = self.free_inode(ino);
                return Err(e);
            }
        }
        self.touch_dir(parent)?;
        Ok(no)
    }

    /// Remove the directory entry `name` of `child` in `parent` (plus its WIN32/DOS twin) from
    /// the parent index and from the inode. The caller stores or frees the inode.
    fn remove_name(&mut self, parent: Ino, child: &mut Inode, name: &[u16]) -> FsResult<()> {
        let names = child.file_names();
        let target = names
            .iter()
            .find(|(_, f)| mref_no(f.parent) == parent && f.name == name)
            .or_else(|| names.iter().find(|(_, f)| mref_no(f.parent) == parent && self.cmp_ci(&f.name, name) == Ordering::Equal))
            .cloned();
        let mut to_remove: Vec<(Loc, FileName)> = vec![];
        match target {
            Some(t) => {
                if t.1.ns == NS_WIN32 || t.1.ns == NS_DOS {
                    let other = if t.1.ns == NS_WIN32 { NS_DOS } else { NS_WIN32 };
                    for (l, f) in &names {
                        if f.ns == other && mref_no(f.parent) == parent {
                            to_remove.push((*l, f.clone()));
                        }
                    }
                }
                to_remove.push(t);
            }
            None => {
                // index entry without matching $FILE_NAME: just drop the entry
                return self.dir_remove(parent, name, child.no);
            }
        }
        for (_, f) in &to_remove {
            match self.dir_remove(parent, &f.name, child.no) {
                Ok(()) | Err(FsError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        let mut locs: Vec<Loc> = to_remove.iter().map(|(l, _)| *l).collect();
        locs.sort();
        for l in locs.into_iter().rev() {
            child.remove(l);
        }
        Ok(())
    }

    fn is_ancestor(&mut self, anc: Ino, mut d: Ino) -> FsResult<bool> {
        for _ in 0..4096 {
            if d == anc {
                return Ok(true);
            }
            if d == ROOT_INO {
                return Ok(false);
            }
            let i = self.load_inode(d)?;
            let p = i.file_names().iter().find(|(_, f)| f.ns != NS_DOS).map(|(_, f)| mref_no(f.parent));
            match p {
                Some(p) if p != d => d = p,
                _ => return Ok(false),
            }
        }
        corrupt("directory loop")
    }

    fn delete_child(&mut self, parent: Ino, name: &str, want_dir: bool) -> FsResult<()> {
        self.writable()?;
        let (e, _) = self.lookup_child(parent, name)?;
        let no = mref_no(e.data);
        if no < FIRST_NON_SYSTEM {
            return Err(FsError::Perm);
        }
        let mut child = self.load_inode(no)?;
        // A Windows directory symlink / junction is a directory record, but a symlink to us.
        if (self.kind_of(&child)? == FileKind::Dir) != want_dir {
            return Err(if want_dir { FsError::NotDir } else { FsError::IsDir });
        }
        if want_dir && !self.dir_is_empty(no)? {
            return Err(FsError::NotEmpty);
        }
        let ename = FileName::parse(&e.key)?.name;
        self.remove_name(parent, &mut child, &ename)?;
        if child.find_all(AT_FILE_NAME, &[]).is_empty() {
            self.free_inode(child)?;
        } else {
            self.touch(&mut child, false);
            self.store_inode(&mut child)?;
            self.mark_dup(no)?;
        }
        self.touch_dir(parent)
    }

    // -- journal doctor ----------------------------------------------------------------------------

    fn volume_flags(&self) -> FsResult<u16> {
        let v = self.load_inode(3)?;
        let l = v.find(AT_VOLUME_INFORMATION, &[]).ok_or_else(|| FsError::Corrupt("no $VOLUME_INFORMATION".into()))?;
        le16(v.attr(l).value().unwrap_or(&[]), 10)
    }

    fn hibernated(&mut self) -> FsResult<bool> {
        let u = to_utf16("hiberfil.sys");
        let Some(e) = self.dir_find(ROOT_INO, &u, false)? else { return Ok(false) };
        let ino = self.load_inode(mref_no(e.data))?;
        let Some(s) = self.stream(&ino, AT_DATA, &[])? else { return Ok(false) };
        if s.size() < 4096 {
            return Ok(false);
        }
        let h = self.read_stream(&s, 0, 4)?;
        Ok(matches!(&h[..], b"hibr" | b"HIBR" | b"wake" | b"WAKE"))
    }

    /// `Ok(None)` = clean, `Ok(Some(reason))` = needs repair.
    fn logfile_state(&self) -> FsResult<Option<String>> {
        let lf = self.load_inode(2)?;
        let Some(s) = self.stream(&lf, AT_DATA, &[])? else { return Ok(Some("$LogFile has no data".into())) };
        let head = self.read_stream(&s, 0, 8192.min(s.size()))?;
        if head.len() < 512 {
            return Ok(Some("$LogFile too small".into()));
        }
        if head.iter().all(|&b| b == 0xFF) {
            return Ok(None); // freshly reset log (mkntfs / ntfsfix / ntfs-3g)
        }
        let mut best: Option<(u64, bool)> = None;
        let mut page = 0usize;
        while page + 512 <= head.len() {
            let magic = &head[page..page + 4];
            if magic == b"RSTR" || magic == b"CHKD" {
                let psize = le32(&head, page + 0x10).unwrap_or(4096) as usize;
                let psize = if (512..=8192).contains(&psize) && psize.is_power_of_two() { psize } else { 4096 };
                let mut p = head[page..(page + psize).min(head.len())].to_vec();
                if p.len() == psize && fixup_read(&mut p).is_ok() {
                    if let Ok(ra) = le16(&p, 0x18) {
                        let ra = ra as usize;
                        if let (Ok(lsn), Ok(in_use), Ok(flags)) = (le64(&p, ra), le16(&p, ra + 0x0C), le16(&p, ra + 0x0E)) {
                            let clean = in_use == 0xFFFF || flags & 0x0002 != 0;
                            if best.map_or(true, |(l, _)| lsn >= l) {
                                best = Some((lsn, clean));
                            }
                        }
                    }
                }
                page += psize;
                if page >= 2 * psize {
                    break;
                }
            } else {
                break;
            }
        }
        match best {
            Some((_, true)) => Ok(None),
            Some((_, false)) => Ok(Some("$LogFile contains unreplayed transactions (Windows was not shut down cleanly)".into())),
            None => Ok(Some("$LogFile has no valid restart area".into())),
        }
    }
}

impl Drop for NtfsFs {
    fn drop(&mut self) {
        if !self.ro {
            let _ = self.flush_dup();
            let _ = self.dev.flush();
        }
    }
}

pub(crate) const I30: &[u16] = &[0x24, 0x49, 0x33, 0x30]; // "$I30"


struct AlEntry {
    ty: u32,
    name: Vec<u16>,
    mref: u64,
}

fn parse_attr_list(v: &[u8]) -> FsResult<Vec<AlEntry>> {
    let mut out = vec![];
    let mut off = 0;
    while off + 0x1A <= v.len() {
        let ty = le32(v, off)?;
        let len = le16(v, off + 4)? as usize;
        let nlen = byte(v, off + 6)? as usize;
        let noff = byte(v, off + 7)? as usize;
        if len < 0x1A || off + len > v.len() || noff + 2 * nlen > len {
            return corrupt("bad attribute list entry");
        }
        let mref = le64(v, off + 0x10)?;
        out.push(AlEntry { ty, name: bytes_utf16(&v[off + noff..off + noff + 2 * nlen]), mref });
        off += len;
    }
    Ok(out)
}

fn lznt1_decompress(src: &[u8], dst: &mut [u8]) -> FsResult<()> {
    let mut sp = 0usize;
    let mut dp = 0usize;
    while sp + 2 <= src.len() && dp < dst.len() {
        let h = u16::from_le_bytes([src[sp], src[sp + 1]]);
        sp += 2;
        if h == 0 {
            break;
        }
        let csize = (h & 0x0FFF) as usize + 1;
        if sp + csize > src.len() {
            return corrupt("LZNT1 chunk overflows");
        }
        let chunk = &src[sp..sp + csize];
        sp += csize;
        let cstart = dp;
        let cend = (dp + 4096).min(dst.len());
        if h & 0x8000 == 0 {
            let n = csize.min(cend - dp);
            dst[dp..dp + n].copy_from_slice(&chunk[..n]);
            dp += n;
        } else {
            let mut i = 0;
            'chunk: while i < chunk.len() {
                let flags = chunk[i];
                i += 1;
                for bit in 0..8 {
                    if i >= chunk.len() || dp >= cend {
                        break 'chunk;
                    }
                    if flags & (1 << bit) == 0 {
                        dst[dp] = chunk[i];
                        dp += 1;
                        i += 1;
                    } else {
                        if i + 2 > chunk.len() {
                            return corrupt("LZNT1 truncated token");
                        }
                        let t = u16::from_le_bytes([chunk[i], chunk[i + 1]]) as usize;
                        i += 2;
                        let pos = dp - cstart;
                        if pos == 0 {
                            return corrupt("LZNT1 back-reference at chunk start");
                        }
                        let mut lbits = 12usize;
                        let mut p = pos - 1;
                        while p >= 0x10 {
                            lbits -= 1;
                            p >>= 1;
                        }
                        let disp = (t >> lbits) + 1;
                        let len = (t & ((1 << lbits) - 1)) + 3;
                        if disp > pos {
                            return corrupt("LZNT1 bad displacement");
                        }
                        for _ in 0..len {
                            if dp >= cend {
                                break;
                            }
                            dst[dp] = dst[dp - disp];
                            dp += 1;
                        }
                    }
                }
            }
        }
        // a chunk always stands for 4096 bytes of output unless it is the last one
        if dp < cstart + 4096 && sp + 2 <= src.len() && u16::from_le_bytes([src[sp], src[sp + 1]]) != 0 {
            dp = (cstart + 4096).min(dst.len());
        }
    }
    Ok(())
}

/// Greedy LZNT1 compressor (4 KiB chunks). Only used to build test volumes: the driver itself
/// never writes compressed files.
fn lznt1_compress(src: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    for chunk in src.chunks(4096) {
        let mut body = vec![];
        let mut pos = 0usize;
        while pos < chunk.len() {
            let flag_at = body.len();
            body.push(0u8);
            for bit in 0..8 {
                if pos >= chunk.len() {
                    break;
                }
                let mut lbits = 12usize;
                let mut p = pos.saturating_sub(1);
                while p >= 0x10 {
                    lbits -= 1;
                    p >>= 1;
                }
                let (max_disp, max_len) = (1usize << (16 - lbits), (1usize << lbits) + 2);
                let mut best = (0usize, 0usize);
                for disp in 1..=pos.min(max_disp) {
                    let mut l = 0;
                    while l < max_len && pos + l < chunk.len() && chunk[pos + l] == chunk[pos + l - disp] {
                        l += 1;
                    }
                    if l > best.1 {
                        best = (disp, l);
                    }
                }
                if best.1 >= 3 {
                    let t = ((best.0 - 1) << lbits) | (best.1 - 3);
                    body.extend_from_slice(&(t as u16).to_le_bytes());
                    body[flag_at] |= 1 << bit;
                    pos += best.1;
                } else {
                    body.push(chunk[pos]);
                    pos += 1;
                }
            }
        }
        if body.len() < chunk.len() {
            out.extend_from_slice(&(0xB000u16 | (body.len() as u16 - 1)).to_le_bytes());
            out.extend_from_slice(&body);
        } else {
            out.extend_from_slice(&(0x3000u16 | (chunk.len() as u16 - 1)).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out
}

/// Value of an `IO_REPARSE_TAG_MOUNT_POINT` (junction) reparse point (test volumes).
#[doc(hidden)]
pub fn debug_junction_value(subst: &str, print: &str) -> Vec<u8> {
    let s = utf16_bytes(&to_utf16(subst));
    let p = utf16_bytes(&to_utf16(print));
    let mut d = vec![0u8; 8];
    put16(&mut d, 2, s.len() as u16 + 2);
    put16(&mut d, 4, s.len() as u16 + 2);
    put16(&mut d, 6, p.len() as u16);
    d.extend_from_slice(&s);
    d.extend_from_slice(&[0, 0]);
    d.extend_from_slice(&p);
    d.extend_from_slice(&[0, 0]);
    let mut v = vec![0u8; 8];
    put32(&mut v, 0, IO_REPARSE_TAG_MOUNT_POINT);
    put16(&mut v, 4, d.len() as u16);
    v.extend_from_slice(&d);
    v
}

// ─────────────── test hooks (seed images for fuzzkit.rs; not a stable API) ───────────────

#[doc(hidden)]
impl NtfsFs {
    /// Make `ino` (an empty file) an LZNT1-compressed file holding `data`, laid out like
    /// Windows does it: 16-cluster compression units, each either stored raw, compressed
    /// (followed by a sparse run) or entirely sparse.
    pub fn debug_make_compressed(&mut self, ino: Ino, data: &[u8]) -> FsResult<()> {
        self.writable()?;
        let cs = self.cluster;
        if cs > 4096 {
            return Err(FsError::Unsupported("compression needs clusters <= 4 KiB".into()));
        }
        let cu_bytes = 16 * cs as usize;
        let mut runs: Vec<Run> = vec![];
        let mut allocated = 0u64;
        for unit in data.chunks(cu_bytes) {
            let vcn = runs_end(&runs);
            let packed = lznt1_compress(unit);
            let (bytes, stored) = if unit.iter().all(|&b| b == 0) {
                (vec![], 0)
            } else if packed.len() as u64 <= 15 * cs {
                let n = ceil_div(packed.len() as u64, cs);
                (packed, n)
            } else {
                (unit.to_vec(), 16)
            };
            if stored > 0 {
                let got = self.alloc_clusters(stored, self.data_hint)?;
                let mut v = bytes;
                v.resize((stored * cs) as usize, 0);
                let mut o = 0usize;
                for (lcn, len) in got {
                    self.dev.write_at(lcn * cs, &v[o..o + (len * cs) as usize])?;
                    o += (len * cs) as usize;
                    runs.push(Run { vcn: runs_end(&runs), lcn: Some(lcn), len });
                }
                allocated += stored * cs;
            }
            if stored < 16 {
                runs.push(Run { vcn: runs_end(&runs), lcn: None, len: vcn + 16 - runs_end(&runs) });
            }
        }
        let mut i = self.load_inode(ino)?;
        let l = self.single_loc(&i, AT_DATA, &[])?;
        let mut a = Attribute::nonresident(AT_DATA, &[], runs, data.len() as u64, cs);
        a.flags = 0x0001;
        a.instance = i.attr(l).instance;
        if let Body::NonRes(n) = &mut a.body {
            n.comp_unit = 4;
            n.comp_size = allocated;
        }
        *i.attr_mut(l) = a;
        let mut si = i.std_info();
        si.attrs |= FILE_ATTR_COMPRESSED;
        i.set_std_info(&si);
        self.store_inode(&mut i)?;
        self.sync_dup(ino)
    }

    /// Turn a node into a reparse point with an arbitrary value (what Windows creates).
    pub fn debug_set_reparse(&mut self, ino: Ino, value: Vec<u8>) -> FsResult<()> {
        self.writable()?;
        let tag = tag_of(&value).ok_or_else(|| FsError::Invalid("short reparse value".into()))?;
        let mut i = self.load_inode(ino)?;
        i.add(Attribute::resident(AT_REPARSE_POINT, &[], value));
        let mut si = i.std_info();
        si.attrs |= FILE_ATTR_REPARSE;
        i.set_std_info(&si);
        self.store_inode(&mut i)?;
        self.r_insert(tag, i.mref())?;
        self.sync_dup(ino)
    }
}

// ---------------------------------------------------------------------------------------------

impl FileSystem for NtfsFs {
    fn volume_info(&self) -> VolumeInfo {
        VolumeInfo { fs_type: FsType::Ntfs, label: self.label.clone(), uuid: format!("{:016X}", self.serial), read_only: self.ro }
    }

    fn statfs(&mut self) -> FsResult<FsStat> {
        self.vbm()?;
        let free_bytes = self.free_clusters * self.cluster;
        let n = self.mft_records();
        let used_recs = (0..n).filter(|&i| self.mft_bit(i)).count() as u64;
        let extra = free_bytes / self.rec_size as u64;
        Ok(FsStat {
            block_size: self.cluster as u32,
            total_bytes: self.total_clusters * self.cluster,
            free_bytes,
            total_inodes: n + extra,
            free_inodes: n - used_recs + extra,
        })
    }

    fn root_ino(&self) -> Ino {
        ROOT_INO
    }

    fn read_only_reason(&self) -> Option<String> {
        self.ro_reason.clone()
    }

    fn lookup(&mut self, parent: Ino, name: &str) -> FsResult<Ino> {
        let p = self.user_inode(parent)?;
        if !p.is_dir() {
            return Err(FsError::NotDir);
        }
        if name == "." {
            return Ok(parent);
        }
        if name == ".." {
            if parent == ROOT_INO {
                return Ok(ROOT_INO);
            }
            return p.file_names().iter().find(|(_, f)| f.ns != NS_DOS).map(|(_, f)| mref_no(f.parent)).ok_or(FsError::NotFound);
        }
        let (e, _) = self.lookup_child(parent, name)?;
        Ok(mref_no(e.data))
    }

    fn getattr(&mut self, ino: Ino) -> FsResult<Attr> {
        let i = self.user_inode(ino)?;
        self.attr_of(&i)
    }

    fn setattr(&mut self, ino: Ino, set: &SetAttr) -> FsResult<Attr> {
        let mut i = self.user_inode(ino)?;
        if !set.is_empty() {
            self.writable()?;
        }
        if set.size.is_none() && set.atime.is_none() && set.mtime.is_none() {
            return self.attr_of(&i); // perm/uid/gid are not stored on NTFS
        }
        if ino < FIRST_NON_SYSTEM && ino != ROOT_INO {
            return Err(FsError::Perm);
        }
        if let Some(sz) = set.size {
            match self.kind_of(&i)? {
                FileKind::Dir => return Err(FsError::IsDir),
                FileKind::Symlink => return Err(FsError::Invalid("cannot truncate a symlink".into())),
                FileKind::File => {}
            }
            if i.find(AT_DATA, &[]).is_none() {
                i.add(Attribute::resident(AT_DATA, &[], vec![]));
            }
            self.resize_attr(&mut i, AT_DATA, &[], sz, true)?;
            self.touch(&mut i, true);
        }
        let mut si = i.std_info();
        if let Some(t) = set.atime {
            si.atime = ts_to_ntfs(t);
        }
        if let Some(t) = set.mtime {
            si.mtime = ts_to_ntfs(t);
        }
        si.ctime = ntfs_now();
        i.set_std_info(&si);
        self.store_inode(&mut i)?;
        self.mark_dup(ino)?;
        self.attr_of(&i)
    }

    fn read(&mut self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>> {
        let i = self.user_inode(ino)?;
        if let Some(t) = self.symlink_target(&i)? {
            // like ext4: the "content" of a symlink is its target
            let b = t.as_bytes();
            let a = (offset.min(b.len() as u64)) as usize;
            let e = (offset.saturating_add(len as u64).min(b.len() as u64)) as usize;
            return Ok(b[a..e].to_vec());
        }
        if i.is_dir() {
            return Err(FsError::IsDir);
        }
        match self.stream(&i, AT_DATA, &[])? {
            Some(s) => self.read_stream(&s, offset, len as u64),
            None => Ok(vec![]),
        }
    }

    fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<u32> {
        self.writable()?;
        let mut i = self.user_inode(ino)?;
        if self.symlink_target(&i)?.is_some() {
            return Err(FsError::Invalid("cannot write to a symlink".into()));
        }
        if i.is_dir() {
            return Err(FsError::IsDir);
        }
        if ino < FIRST_NON_SYSTEM {
            return Err(FsError::Perm);
        }
        if i.find(AT_DATA, &[]).is_none() {
            i.add(Attribute::resident(AT_DATA, &[], vec![]));
        }
        self.write_attr(&mut i, AT_DATA, &[], offset, data, true)?;
        self.touch(&mut i, true);
        self.store_inode(&mut i)?;
        self.mark_dup(ino)?;
        Ok(data.len() as u32)
    }

    fn readdir(&mut self, dir: Ino) -> FsResult<Vec<DirEntry>> {
        let d = self.user_inode(dir)?;
        if !d.is_dir() {
            return Err(FsError::NotDir);
        }
        let mut out = vec![];
        for e in self.dir_list(dir)? {
            let Ok(f) = FileName::parse(&e.key) else { continue };
            let no = mref_no(e.data);
            if f.ns == NS_DOS || (dir == ROOT_INO && no < 16) || no == dir {
                continue;
            }
            let mut kind = if f.flags & FILE_ATTR_DUP_DIR != 0 { FileKind::Dir } else { FileKind::File };
            if f.flags & (FILE_ATTR_REPARSE | FILE_ATTR_SYSTEM) != 0 {
                // possibly a symlink (reparse point or Interix): ask the inode
                if let Ok(i) = self.load_inode(no) {
                    kind = self.kind_of(&i).unwrap_or(kind);
                }
            }
            out.push(DirEntry { name: String::from_utf16_lossy(&f.name), ino: no, kind });
        }
        Ok(out)
    }

    fn create(&mut self, parent: Ino, name: &str, _perm: u16, _uid: u32, _gid: u32) -> FsResult<Ino> {
        self.create_node(parent, name, NewNode::File)
    }

    fn mkdir(&mut self, parent: Ino, name: &str, _perm: u16, _uid: u32, _gid: u32) -> FsResult<Ino> {
        self.create_node(parent, name, NewNode::Dir)
    }

    fn symlink(&mut self, parent: Ino, name: &str, target: &str, _uid: u32, _gid: u32) -> FsResult<Ino> {
        self.writable()?;
        if target.is_empty() || target.contains('\0') {
            return Err(FsError::Invalid("empty or NUL-containing symlink target".into()));
        }
        if target.len() > MAX_TARGET {
            return Err(FsError::NameTooLong);
        }
        let (tag, value) = build_symlink(target);
        // Windows and ntfs-3g only follow a relative symlink whose record type (file/dir)
        // matches its target, so a link to an existing directory becomes a directory record.
        // Absolute targets live in the host namespace: always a file record.
        let relative = is_relative_symlink(tag, target);
        let is_dir = relative && self.rel_target_is_dir(parent, target);
        let ino = self.create_node(parent, name, NewNode::Symlink(tag, value, is_dir))?;
        if relative && !is_dir {
            self.pending_dir_links.push((parent, name.to_string(), target.to_string()));
        }
        Ok(ino)
    }

    fn readlink(&mut self, ino: Ino) -> FsResult<String> {
        let i = self.user_inode(ino)?;
        self.symlink_target(&i)?.ok_or_else(|| FsError::Invalid("not a symlink".into()))
    }

    fn unlink(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.delete_child(parent, name, false)
    }

    fn rmdir(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.delete_child(parent, name, true)
    }

    fn rename(&mut self, from_parent: Ino, from_name: &str, to_parent: Ino, to_name: &str) -> FsResult<()> {
        self.writable()?;
        let new_name = self.encode_name(to_name)?;
        let (se, sf) = self.lookup_child(from_parent, from_name)?;
        let src = mref_no(se.data);
        if src < FIRST_NON_SYSTEM {
            return Err(FsError::Perm);
        }
        let tp = self.user_inode(to_parent)?;
        if !tp.is_dir() {
            return Err(FsError::NotDir);
        }
        if to_parent != ROOT_INO && to_parent < FIRST_NON_SYSTEM {
            return Err(FsError::Perm);
        }
        let src_ino = self.load_inode(src)?;
        let src_dir = self.kind_of(&src_ino)? == FileKind::Dir;
        if src_dir && self.is_ancestor(src, to_parent)? {
            return Err(FsError::Invalid("cannot move a directory into itself".into()));
        }
        match self.lookup_child(to_parent, to_name) {
            Ok((de, _)) => {
                let dst = mref_no(de.data);
                if dst == src {
                    if from_parent == to_parent && sf.name == new_name {
                        return Ok(()); // same name: no-op
                    }
                    if from_parent != to_parent {
                        return Ok(()); // hard links to the same file: POSIX no-op
                    }
                    // case-only rename: fall through
                } else {
                    let d = self.load_inode(dst)?;
                    if self.kind_of(&d)? == FileKind::Dir {
                        if !src_dir {
                            return Err(FsError::IsDir);
                        }
                        self.delete_child(to_parent, to_name, true)?;
                    } else {
                        if src_dir {
                            return Err(FsError::NotDir);
                        }
                        self.delete_child(to_parent, to_name, false)?;
                    }
                }
            }
            Err(FsError::NotFound) => {}
            Err(e) => return Err(e),
        }
        self.flush_dup_one(src)?;
        let mut s = self.load_inode(src)?;
        self.remove_name(from_parent, &mut s, &sf.name)?;
        let tp = self.load_inode(to_parent)?;
        let fname = self.make_file_name(&s, tp.mref(), &new_name);
        let key = fname.to_bytes();
        s.add(Attribute { ty: AT_FILE_NAME, name: vec![], flags: 0, instance: 0, body: Body::Res { value: key.clone(), rflags: 1 } });
        self.touch(&mut s, false);
        self.store_inode(&mut s)?;
        self.dir_insert(to_parent, key, s.mref())?;
        self.touch_dir(from_parent)?;
        if to_parent != from_parent {
            self.touch_dir(to_parent)?;
        }
        Ok(())
    }

    fn sync(&mut self) -> FsResult<()> {
        if !self.ro {
            self.fix_pending_dir_links();
            self.flush_dup()?;
        }
        self.dev.flush()?;
        Ok(())
    }

    fn fingerprint(&mut self) -> Option<String> {
        // Windows rewrites $LogFile's restart area and $Volume on every mount, and the MFT
        // records / MFT bitmap on every change; LumenDisk never writes them while reading.
        let mut parts = vec![self.dev.read_vec(0, 512).ok()?];
        for no in 0..16 {
            parts.push(self.read_record_raw(no).unwrap_or_default());
        }
        parts.push(self.mft_bitmap.clone());
        if let Ok(lf) = self.load_inode(2) {
            if let Ok(Some(st)) = self.stream(&lf, AT_DATA, &[]) {
                parts.push(self.read_stream(&st, 0, 65536.min(st.size())).unwrap_or_default());
            }
        }
        Some(digest_parts(parts.iter().map(|p| &p[..])))
    }

    fn dirty_state(&mut self) -> DirtyState {
        let mut reasons = vec![];
        match self.volume_flags() {
            Ok(f) if f & 0x0001 != 0 => reasons.push("the NTFS volume dirty flag is set".to_string()),
            Ok(_) => {}
            Err(e) => reasons.push(format!("cannot read $Volume: {e}")),
        }
        match self.hibernated() {
            Ok(true) => reasons.push("Windows is hibernated (hiberfil.sys is active / Fast Startup)".to_string()),
            Ok(false) => {}
            Err(e) => reasons.push(format!("cannot inspect hiberfil.sys: {e}")),
        }
        match self.logfile_state() {
            Ok(None) => {}
            Ok(Some(r)) => reasons.push(r),
            Err(e) => reasons.push(format!("cannot read $LogFile: {e}")),
        }
        if reasons.is_empty() {
            DirtyState::Clean
        } else {
            DirtyState::Dirty(reasons.join("; "))
        }
    }

    fn repair(&mut self) -> FsResult<String> {
        self.writable()?;
        let mut report = vec![];
        // 1. volume dirty flag
        let mut v = self.load_inode(3)?;
        if let Some(l) = v.find(AT_VOLUME_INFORMATION, &[]) {
            let mut changed = false;
            if let Body::Res { value, .. } = &mut v.attr_mut(l).body {
                if value.len() >= 12 {
                    let f = u16::from_le_bytes([value[10], value[11]]);
                    if f & 0x0001 != 0 {
                        put16(value, 10, f & !0x0001);
                        changed = true;
                    }
                }
            }
            if changed {
                self.store_inode(&mut v)?;
                report.push("Cleared the volume dirty flag.".to_string());
            }
        }
        // 2. $LogFile reset (like ntfsfix: fill with 0xFF)
        if self.logfile_state()?.is_some() {
            let lf = self.load_inode(2)?;
            if let Some(Stream::NonRes { runs, size, .. }) = self.stream(&lf, AT_DATA, &[])? {
                // A corrupt $LogFile must not make the reset overwrite the boot sector or
                // the MFT (or its mirror): those runs would be cross-linked.
                let hits = |l: u64, n: u64, other: &[Run]| other.iter().any(|r| r.lcn.map_or(false, |o| l < o.saturating_add(r.len) && o < l.saturating_add(n)));
                if runs.iter().any(|r| r.lcn.map_or(false, |l| l == 0 || hits(l, r.len, &self.mft_runs) || hits(l, r.len, &self.mirr_runs))) {
                    return corrupt("$LogFile is cross-linked with the boot sector or the MFT; run chkdsk");
                }
                let size = size.min(runs_end(&runs).saturating_mul(self.cluster));
                let chunk = vec![0xFFu8; 1 << 20];
                let mut p = 0;
                while p < size {
                    let n = (size - p).min(chunk.len() as u64);
                    self.write_runs(&runs, p, &chunk[..n as usize])?;
                    p += n;
                }
                report.push(format!("Reset $LogFile ({} KiB) to the empty state.", size / 1024));
            }
        }
        // 3. $MFTMirr consistency
        let mut fixed_mirr = 0;
        for no in 0..self.mirr_count {
            let rs = self.rec_size as u64;
            let main = self.read_record_raw(no)?;
            let mut mirr = vec![0u8; self.rec_size];
            self.read_runs(&self.mirr_runs, no * rs, &mut mirr)?;
            if main != mirr {
                self.write_runs(&self.mirr_runs.clone(), no * rs, &main)?;
                fixed_mirr += 1;
            }
        }
        if fixed_mirr > 0 {
            report.push(format!("Resynchronised {fixed_mirr} $MFTMirr record(s)."));
        }
        // 4. hibernation cannot be fixed safely
        if self.hibernated().unwrap_or(false) {
            report.push("Windows is hibernated: boot Windows and shut it down completely (disable Fast Startup); the hibernation file was left untouched, so the volume stays read-only.".to_string());
        }
        self.dev.flush()?;
        if report.is_empty() {
            report.push("Volume is clean; nothing to repair.".to_string());
        }
        Ok(report.join("\n"))
    }
}

impl NtfsFs {
    fn flush_dup_one(&mut self, no: Ino) -> FsResult<()> {
        if self.dup_dirty.remove(&no) {
            self.sync_dup(no)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "ntfs/tests.rs"]
mod tests;
