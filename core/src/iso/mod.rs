//! Read-only ISO 9660 (+ Joliet, Rock Ridge, El Torito) and UDF (Windows ISOs) as a
//! [`crate::forge::tree::SourceTree`].
//!
//! Owner: the ISO / Linux workstream.
//!
//! An optical image can carry several views of (normally) the same files:
//! - plain ISO 9660 (ECMA-119): upper-case 8.3 / 31-char names;
//! - Joliet (a Supplementary Volume Descriptor with UCS-2 names);
//! - Rock Ridge (RRIP / SUSP System Use entries in the ISO 9660 records: POSIX names,
//!   permissions, symlinks, deep directories);
//! - UDF (ECMA-167 + OSTA UDF), on "bridge" discs next to ISO 9660, or alone. Windows ISOs keep
//!   their real file set only in UDF (the ISO 9660 view is a stub with a README).
//!
//! [`open`] parses every view present and keeps the richest one ([`View`]). Parsing builds an
//! in-memory index of the whole tree (paths → extents on the device); reads then go straight to
//! the device. Everything read from the image is untrusted: every length and location is
//! checked, recursion and allocation are bounded ([`Limits`]), and directory graphs that loop
//! are detected.

pub mod eltorito;
mod iso9660;
#[doc(hidden)]
pub mod testimg;
mod udf;

use std::collections::HashMap;
use std::sync::Arc;

use crate::block::BlockDev;
use crate::forge::tree::{SourceTree, TreeEntry};
use crate::forge::{ForgeError, ForgeResult};

pub use eltorito::{BootCatalog, BootEntry, BootMedia, BootPlatform};

/// Which on-disc structures the tree was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    Iso9660,
    Joliet,
    RockRidge,
    Udf,
}

/// Bounds applied while parsing untrusted images.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// Total entries (files + directories) in a tree.
    pub max_nodes: usize,
    /// Directory nesting depth.
    pub max_depth: usize,
    /// Size of one directory's data read into memory at once.
    pub max_dir_bytes: u64,
    /// Total allocation descriptors / extents kept for the whole tree.
    pub max_extents: usize,
    /// Bytes of one path.
    pub max_path: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_nodes: 2_000_000, max_depth: 256, max_dir_bytes: 64 << 20, max_extents: 8_000_000, max_path: 4096 }
    }
}

/// A piece of a file's data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Extent {
    /// Byte offset of the data on the device (ignored when `sparse`).
    pub pos: u64,
    /// Bytes of the file this extent covers.
    pub len: u64,
    /// Not recorded on the medium: reads as zeros (UDF "not recorded" extents, ISO 9660
    /// interleave gaps are *not* sparse, they are simply skipped).
    pub sparse: bool,
}

/// Where a file's bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Data {
    /// Directories, symlinks, empty files.
    None,
    /// In file order; the lengths add up to (at least) the file size.
    Extents(Vec<Extent>),
    /// Small files stored inside their descriptor (UDF embedded data).
    Inline(Vec<u8>),
    /// Listed, but its data can't be read (why).
    Unsupported(String),
}

/// One entry of a parsed tree (the root is not included).
#[derive(Clone, Debug)]
pub(crate) struct Node {
    /// Absolute, `/`-separated, no trailing slash.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix permission bits (Rock Ridge / UDF), 0 if unknown.
    pub mode: u32,
    /// Seconds since the epoch, 0 if unknown.
    pub mtime: i64,
    pub symlink: Option<String>,
    pub data: Data,
}

/// The result of parsing one view.
#[derive(Clone, Debug)]
pub(crate) struct Parsed {
    pub view: View,
    pub label: String,
    /// Parents before their children.
    pub nodes: Vec<Node>,
}

/// An opened ISO / UDF image.
pub struct IsoTree {
    dev: Arc<dyn BlockDev>,
    view: View,
    label: String,
    nodes: Vec<Node>,
    /// Exact path → index into `nodes`.
    index: HashMap<String, usize>,
    /// Lower-cased path → index (first one wins), for case-insensitive lookups.
    index_ci: HashMap<String, usize>,
    boot: Option<BootCatalog>,
}

impl std::fmt::Debug for IsoTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IsoTree").field("view", &self.view).field("label", &self.label).field("nodes", &self.nodes.len()).finish()
    }
}

impl IsoTree {
    fn new(dev: Arc<dyn BlockDev>, p: Parsed, boot: Option<BootCatalog>) -> Self {
        let mut index = HashMap::with_capacity(p.nodes.len());
        let mut index_ci = HashMap::with_capacity(p.nodes.len());
        for (i, n) in p.nodes.iter().enumerate() {
            index.entry(n.path.clone()).or_insert(i);
            index_ci.entry(n.path.to_lowercase()).or_insert(i);
        }
        IsoTree { dev, view: p.view, label: p.label, nodes: p.nodes, index, index_ci, boot }
    }

    /// The view the tree was read from.
    pub fn view(&self) -> View {
        self.view
    }

    /// The El Torito boot catalog, if the image has one.
    pub fn boot_catalog(&self) -> Option<&BootCatalog> {
        self.boot.as_ref()
    }

    /// Number of entries (files, directories, links), root excluded.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The entry at `path` (exact match first, then ignoring case).
    pub fn entry(&self, path: &str) -> Option<TreeEntry> {
        self.node(path).map(to_entry)
    }

    fn node(&self, path: &str) -> Option<&Node> {
        let p = normalize(path);
        self.index.get(&p).or_else(|| self.index_ci.get(&p.to_lowercase())).map(|&i| &self.nodes[i])
    }

    /// Read a whole (small) file; `max` bounds the allocation.
    pub fn read_all(&self, path: &str, max: u64) -> ForgeResult<Vec<u8>> {
        let n = self.node(path).ok_or_else(|| ForgeError::Invalid(format!("{path}: not found in the image")))?;
        if n.is_dir {
            return Err(ForgeError::Invalid(format!("{path}: is a directory")));
        }
        if n.size > max {
            return Err(ForgeError::Invalid(format!("{path}: too large ({} bytes)", n.size)));
        }
        let mut out = vec![0u8; n.size as usize];
        let mut off = 0usize;
        while off < out.len() {
            let k = read_node(self.dev.as_ref(), n, off as u64, &mut out[off..])?;
            if k == 0 {
                return Err(ForgeError::Io(format!("{path}: data ends early")));
            }
            off += k;
        }
        Ok(out)
    }

    /// The file whose data starts at byte `pos` of the image (an El Torito boot image is
    /// normally also a file of the tree), if any.
    pub fn path_at(&self, pos: u64) -> Option<&str> {
        self.nodes.iter().find(|n| matches!(&n.data, Data::Extents(e) if e.first().is_some_and(|x| x.pos == pos && !x.sparse))).map(|n| n.path.as_str())
    }

    /// The device the image is read from.
    pub fn device(&self) -> &Arc<dyn BlockDev> {
        &self.dev
    }
}

fn normalize(path: &str) -> String {
    let mut s = String::with_capacity(path.len() + 1);
    for c in path.split('/').filter(|c| !c.is_empty()) {
        s.push('/');
        s.push_str(c);
    }
    s
}

fn to_entry(n: &Node) -> TreeEntry {
    TreeEntry { path: n.path.clone(), is_dir: n.is_dir, size: if n.is_dir { 0 } else { n.size }, mode: n.mode, mtime: n.mtime, symlink: n.symlink.clone() }
}

/// Read file bytes of `n` at `offset` into `buf`; returns how many (0 at the end).
fn read_node(dev: &dyn BlockDev, n: &Node, offset: u64, buf: &mut [u8]) -> ForgeResult<usize> {
    if n.is_dir || n.symlink.is_some() || offset >= n.size || buf.is_empty() {
        return Ok(0);
    }
    let want = (buf.len() as u64).min(n.size - offset) as usize;
    match &n.data {
        Data::Unsupported(why) => Err(ForgeError::Unsupported(format!("{}: {why}", n.path))),
        Data::None => {
            // A file with a size but no data (hostile): reads as zeros.
            buf[..want].fill(0);
            Ok(want)
        }
        Data::Inline(d) => {
            let start = offset as usize;
            let avail = d.len().saturating_sub(start).min(want);
            buf[..avail].copy_from_slice(&d[start..start + avail]);
            buf[avail..want].fill(0);
            Ok(want)
        }
        Data::Extents(ex) => {
            // Find the extent holding `offset` (extents are few; a linear scan is fine, but a
            // binary search over cumulative offsets keeps huge fragmented files fast).
            let mut done = 0usize;
            let mut file_pos = 0u64;
            let mut i = 0usize;
            while i < ex.len() && done < want {
                let e = &ex[i];
                let e_end = file_pos.saturating_add(e.len);
                let cur = offset + done as u64;
                if cur < e_end {
                    let within = cur - file_pos;
                    let k = ((e.len - within).min((want - done) as u64)) as usize;
                    let dst = &mut buf[done..done + k];
                    if e.sparse {
                        dst.fill(0);
                    } else {
                        let at = e.pos.checked_add(within).ok_or_else(|| ForgeError::Invalid("extent beyond the device".into()))?;
                        dev.read_at(at, dst).map_err(|err| ForgeError::Io(format!("{}: {err}", n.path)))?;
                    }
                    done += k;
                }
                file_pos = e_end;
                i += 1;
            }
            if done < want {
                // Extents shorter than the recorded size: the rest reads as zeros (as Linux does).
                buf[done..want].fill(0);
            }
            Ok(want)
        }
    }
}

impl SourceTree for IsoTree {
    fn entries(&mut self) -> ForgeResult<Vec<TreeEntry>> {
        Ok(self.nodes.iter().map(to_entry).collect())
    }

    fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> ForgeResult<usize> {
        let n = self.node(path).ok_or_else(|| ForgeError::Invalid(format!("{path}: not found in the image")))?;
        read_node(self.dev.as_ref(), n, offset, buf)
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}

/// Number of regular files in a parsed view (used to pick the richest one).
fn file_count(p: &Parsed) -> usize {
    p.nodes.iter().filter(|n| !n.is_dir).count()
}

/// Open an ISO / UDF image, choosing the richest view: UDF when its tree has more files than
/// the ISO 9660 one (Windows media), otherwise Rock Ridge, Joliet or plain ISO 9660 names.
pub fn open(dev: Arc<dyn BlockDev>) -> ForgeResult<IsoTree> {
    let lim = Limits::default();
    let iso = iso9660::read(dev.as_ref(), &lim, None);
    let udf = if udf::probe(dev.as_ref()) { Some(udf::read(dev.as_ref(), &lim)) } else { None };
    let boot = eltorito::read(dev.as_ref()).ok().flatten();
    let chosen = match (iso, udf) {
        (Ok(i), Some(Ok(u))) => {
            if file_count(&u) > file_count(&i) {
                u
            } else {
                i
            }
        }
        (Ok(i), _) => i,
        (Err(_), Some(Ok(u))) => u,
        (Err(e), Some(Err(ue))) => return Err(ForgeError::Invalid(format!("{e}; UDF: {ue}"))),
        (Err(e), None) => return Err(e),
    };
    Ok(IsoTree::new(dev, chosen, boot))
}

/// Open one specific view (tests, diagnostics). `View::Iso9660` ignores Rock Ridge and Joliet.
pub fn open_view(dev: Arc<dyn BlockDev>, view: View) -> ForgeResult<IsoTree> {
    let lim = Limits::default();
    let p = match view {
        View::Udf => {
            if !udf::probe(dev.as_ref()) {
                return Err(ForgeError::Invalid("no UDF file system in the image".into()));
            }
            udf::read(dev.as_ref(), &lim)?
        }
        other => {
            let p = iso9660::read(dev.as_ref(), &lim, Some(other))?;
            if p.view != other {
                return Err(ForgeError::Invalid(format!("the image has no {other:?} view")));
            }
            p
        }
    };
    let boot = eltorito::read(dev.as_ref()).ok().flatten();
    Ok(IsoTree::new(dev, p, boot))
}

/// Whether the image carries a UDF file system.
pub fn has_udf(dev: &dyn BlockDev) -> bool {
    udf::probe(dev)
}

/// Whether the image is an "isohybrid": an ISO 9660 image that also starts with an MBR and/or
/// GPT partition table, so it can be written byte for byte (DD mode) to a USB stick and boot.
pub fn is_hybrid(dev: &dyn BlockDev) -> bool {
    if !iso9660::has_iso9660(dev) {
        return false;
    }
    let Ok(mbr) = dev.read_vec(0, 512) else { return false };
    let gpt = dev.read_vec(512, 8).map(|h| &h[..] == b"EFI PART").unwrap_or(false);
    let mbr_parts = mbr[510] == 0x55
        && mbr[511] == 0xAA
        && (0..4).any(|i| {
            let e = &mbr[446 + i * 16..462 + i * 16];
            let count = u32::from_le_bytes([e[12], e[13], e[14], e[15]]);
            e[4] != 0 && count != 0 && (e[0] == 0 || e[0] == 0x80)
        });
    gpt || mbr_parts
}

/// The El Torito boot catalog of an image (None when it is not bootable as an optical disc).
pub fn boot_info(dev: &dyn BlockDev) -> ForgeResult<Option<BootCatalog>> {
    eltorito::read(dev)
}

// ───────────────────────────── shared decoding helpers ─────────────────────────────

pub(crate) fn le16(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).unwrap_or(0)
}
pub(crate) fn le32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).unwrap_or(0)
}
pub(crate) fn le64(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap_or([0; 8]))).unwrap_or(0)
}
pub(crate) fn be16(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map(|s| u16::from_be_bytes([s[0], s[1]])).unwrap_or(0)
}

/// Days from 1970-01-01 to `y-m-d` (proleptic Gregorian), for timestamps of any origin.
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Seconds since the epoch of a broken-down UTC time; None if a field is out of range.
pub(crate) fn epoch(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32, tz_minutes: i64) -> Option<i64> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h as i64 * 3600 + mi as i64 * 60 + s as i64 - tz_minutes * 60)
}

/// Join `parent` and `name` into a tree path; refuses names that can't be a path component.
pub(crate) fn child_path(parent: &str, name: &str, lim: &Limits) -> Option<String> {
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let clean: String = name.chars().map(|c| if c == '/' || c == '\0' { '_' } else { c }).collect();
    let p = if parent == "/" || parent.is_empty() { format!("/{clean}") } else { format!("{parent}/{clean}") };
    (p.len() <= lim.max_path).then_some(p)
}

#[cfg(test)]
mod tests;
