//! WIM (Windows Imaging Format) files: the header, the blob ("lookup") table, the XML data,
//! and splitting a WIM into `.swm` parts the way `Dism /Split-Image` and `wimlib-imagex split`
//! do, streaming from a [`SourceTree`] straight into the target volume.
//!
//! References (field names follow wimlib):
//! - wimlib `include/wimlib/header.h` (`struct wim_header_disk`, `WIM_HDR_FLAG_*`,
//!   `WIM_VERSION_DEFAULT` / `WIM_VERSION_SOLID`), `include/wimlib/resource.h`
//!   (`struct wim_reshdr_disk`, `WIM_RESHDR_FLAG_*`), `src/blob_table.c`
//!   (`struct blob_descriptor_disk`, 50 bytes), `src/split.c` and `src/write.c`
//!   (`write_wim_part`), `src/xml.c` (UTF-16LE XML with a BOM);
//! - Microsoft, "Windows Imaging File Format (WIM)" (the WIMGAPI whitepaper).
//!
//! A WIM read from an ISO is untrusted: every offset and size is checked against the file
//! size with checked arithmetic, and the tables we load into memory are bounded.
//!
//! Splitting (wimlib `split.c`, checked against `wimlib-imagex split` 1.14.4): every part
//! is a complete WIM file — 208-byte header (`SPANNED` flag, its part number, the part
//! count, the GUID shared by every part, the full image count), the resources stored in that
//! part copied byte for byte (compressed resources stay compressed), a blob table listing only
//! the blobs of that part (each with that part's number), and the XML data, identical in every
//! part. The metadata resources (one per image, which also decide the image order) all go to
//! part 1. Resources are never cut in two, so a part can only be split off between resources:
//! solid (ESD / LZMS) WIMs, whose resources pack many files together and are often several GiB,
//! can't be split this way.

use sha2::Digest;

use super::super::job::JobCtx;
use super::super::tree::SourceTree;
use super::super::{ForgeError, ForgeResult};
use crate::i18n::t;

pub const HEADER_SIZE: usize = 208;
pub const BLOB_ENTRY_SIZE: usize = 50;
pub const MAGIC: [u8; 8] = *b"MSWIM\0\0\0";
/// Pipable WIMs (wimlib extension): another layout, never on Windows media.
pub const PWM_MAGIC: [u8; 8] = *b"WLPWM\0\0\0";
/// `WIM_VERSION_DEFAULT`: classic WIM.
pub const VERSION_DEFAULT: u32 = 0x10d00;
/// `WIM_VERSION_SOLID`: WIMs that may contain solid resources (ESD).
pub const VERSION_SOLID: u32 = 0xe00;

/// `Dism /Split-Image /FileSize:3800` (MB as DISM counts them: MiB).
pub const DEFAULT_PART_LIMIT: u64 = 3800 << 20;

/// Bounds for what we load into memory from an untrusted WIM.
const MAX_XML: u64 = 32 << 20;
const MAX_BLOB_TABLE: u64 = 512 << 20;
const MAX_IMAGES: usize = 4096;
const MAX_TEXT: usize = 1024;

/// Header flags (`WIM_HDR_FLAG_*`).
pub mod hdr_flags {
    pub const RESERVED: u32 = 0x0000_0001;
    pub const COMPRESSION: u32 = 0x0000_0002;
    pub const READONLY: u32 = 0x0000_0004;
    pub const SPANNED: u32 = 0x0000_0008;
    pub const RESOURCE_ONLY: u32 = 0x0000_0010;
    pub const METADATA_ONLY: u32 = 0x0000_0020;
    pub const WRITE_IN_PROGRESS: u32 = 0x0000_0040;
    pub const RP_FIX: u32 = 0x0000_0080;
    pub const COMPRESS_RESERVED: u32 = 0x0001_0000;
    pub const COMPRESS_XPRESS: u32 = 0x0002_0000;
    pub const COMPRESS_LZX: u32 = 0x0004_0000;
    pub const COMPRESS_LZMS: u32 = 0x0008_0000;
    pub const COMPRESS_XPRESS_2: u32 = 0x0020_0000;
}

/// Resource header flags (`WIM_RESHDR_FLAG_*`).
pub mod res_flags {
    pub const FREE: u8 = 0x01;
    pub const METADATA: u8 = 0x02;
    pub const COMPRESSED: u8 = 0x04;
    pub const SPANNED: u8 = 0x08;
    pub const SOLID: u8 = 0x10;
}

fn bad(msg: impl AsRef<str>) -> ForgeError {
    ForgeError::Invalid(format!("WIM: {}", msg.as_ref()))
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn le64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// `struct wim_reshdr_disk`: 7-byte size in the WIM, 1 flag byte, offset, original size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResHdr {
    pub size_in_wim: u64,
    pub flags: u8,
    pub offset: u64,
    pub uncompressed_size: u64,
}

impl ResHdr {
    pub fn parse(b: &[u8]) -> ResHdr {
        let mut s = [0u8; 8];
        s[..7].copy_from_slice(&b[..7]);
        ResHdr { size_in_wim: u64::from_le_bytes(s), flags: b[7], offset: le64(b, 8), uncompressed_size: le64(b, 16) }
    }

    pub fn write(&self, b: &mut [u8]) {
        b[..7].copy_from_slice(&self.size_in_wim.to_le_bytes()[..7]);
        b[7] = self.flags;
        b[8..16].copy_from_slice(&self.offset.to_le_bytes());
        b[16..24].copy_from_slice(&self.uncompressed_size.to_le_bytes());
    }

    pub fn is_empty(&self) -> bool {
        self.size_in_wim == 0 && self.offset == 0
    }

    /// Byte range in the file, checked against its size.
    fn range(&self, file_size: u64, what: &str) -> ForgeResult<(u64, u64)> {
        let end = self.offset.checked_add(self.size_in_wim).ok_or_else(|| bad(format!("{what}: offset overflow")))?;
        if end > file_size {
            return Err(bad(format!("{what} lies beyond the end of the file ({end} > {file_size})")));
        }
        Ok((self.offset, self.size_in_wim))
    }
}

/// `struct wim_header_disk` (208 bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WimHeader {
    /// The bytes as read: unknown / reserved fields are carried over unchanged.
    pub raw: [u8; HEADER_SIZE],
    pub magic: [u8; 8],
    pub version: u32,
    pub flags: u32,
    pub chunk_size: u32,
    pub guid: [u8; 16],
    pub part_number: u16,
    pub total_parts: u16,
    pub image_count: u32,
    pub blob_table: ResHdr,
    pub xml: ResHdr,
    pub boot_metadata: ResHdr,
    pub boot_index: u32,
    pub integrity: ResHdr,
}

impl WimHeader {
    pub fn parse(b: &[u8]) -> ForgeResult<WimHeader> {
        if b.len() < HEADER_SIZE {
            return Err(bad("file too short for a header"));
        }
        let mut magic = [0u8; 8];
        magic.copy_from_slice(&b[..8]);
        if magic != MAGIC && magic != PWM_MAGIC {
            return Err(bad("not a WIM file (bad magic)"));
        }
        let hdr_size = le32(b, 8);
        if hdr_size as usize != HEADER_SIZE {
            return Err(bad(format!("unexpected header size {hdr_size}")));
        }
        let version = le32(b, 12);
        if version != VERSION_DEFAULT && version != VERSION_SOLID {
            return Err(ForgeError::Unsupported(format!("WIM: unknown version {version:#x}")));
        }
        let mut raw = [0u8; HEADER_SIZE];
        raw.copy_from_slice(&b[..HEADER_SIZE]);
        let mut guid = [0u8; 16];
        guid.copy_from_slice(&b[24..40]);
        let h = WimHeader {
            raw,
            magic,
            version,
            flags: le32(b, 16),
            chunk_size: le32(b, 20),
            guid,
            part_number: le16(b, 40),
            total_parts: le16(b, 42),
            image_count: le32(b, 44),
            blob_table: ResHdr::parse(&b[48..72]),
            xml: ResHdr::parse(&b[72..96]),
            boot_metadata: ResHdr::parse(&b[96..120]),
            boot_index: le32(b, 120),
            integrity: ResHdr::parse(&b[124..148]),
        };
        if h.part_number == 0 || h.total_parts == 0 || h.part_number > h.total_parts {
            return Err(bad(format!("bad part number {}/{}", h.part_number, h.total_parts)));
        }
        if h.image_count as usize > MAX_IMAGES {
            return Err(bad(format!("implausible image count {}", h.image_count)));
        }
        if h.boot_index > h.image_count {
            return Err(bad(format!("boot index {} > image count {}", h.boot_index, h.image_count)));
        }
        Ok(h)
    }

    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut b = self.raw;
        b[..8].copy_from_slice(&self.magic);
        b[8..12].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        b[12..16].copy_from_slice(&self.version.to_le_bytes());
        b[16..20].copy_from_slice(&self.flags.to_le_bytes());
        b[20..24].copy_from_slice(&self.chunk_size.to_le_bytes());
        b[24..40].copy_from_slice(&self.guid);
        b[40..42].copy_from_slice(&self.part_number.to_le_bytes());
        b[42..44].copy_from_slice(&self.total_parts.to_le_bytes());
        b[44..48].copy_from_slice(&self.image_count.to_le_bytes());
        self.blob_table.write(&mut b[48..72]);
        self.xml.write(&mut b[72..96]);
        self.boot_metadata.write(&mut b[96..120]);
        b[120..124].copy_from_slice(&self.boot_index.to_le_bytes());
        self.integrity.write(&mut b[124..148]);
        b
    }

    pub fn is_pipable(&self) -> bool {
        self.magic == PWM_MAGIC
    }

    /// "none", "xpress", "lzx", "lzms" (from the header flags).
    pub fn compression(&self) -> &'static str {
        use hdr_flags::*;
        if self.flags & COMPRESSION == 0 {
            "none"
        } else if self.flags & COMPRESS_LZMS != 0 {
            "lzms"
        } else if self.flags & COMPRESS_LZX != 0 {
            "lzx"
        } else if self.flags & (COMPRESS_XPRESS | COMPRESS_XPRESS_2) != 0 {
            "xpress"
        } else {
            "unknown"
        }
    }
}

/// `struct blob_descriptor_disk` (50 bytes): where a blob (file contents, or an image's
/// metadata resource) lives, in which part, how many references, and its SHA-1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobEntry {
    pub res: ResHdr,
    pub part_number: u16,
    pub refcnt: u32,
    pub hash: [u8; 20],
}

impl BlobEntry {
    pub fn parse(b: &[u8]) -> BlobEntry {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&b[30..50]);
        BlobEntry { res: ResHdr::parse(&b[..24]), part_number: le16(b, 24), refcnt: le32(b, 26), hash }
    }

    pub fn write(&self, b: &mut [u8]) {
        self.res.write(&mut b[..24]);
        b[24..26].copy_from_slice(&self.part_number.to_le_bytes());
        b[26..30].copy_from_slice(&self.refcnt.to_le_bytes());
        b[30..50].copy_from_slice(&self.hash);
    }

    pub fn is_metadata(&self) -> bool {
        self.res.flags & res_flags::METADATA != 0
    }

    pub fn is_solid(&self) -> bool {
        self.res.flags & res_flags::SOLID != 0
    }
}

/// Read exactly `buf.len()` bytes of `path` at `off`.
pub fn read_exact(tree: &mut dyn SourceTree, path: &str, mut off: u64, buf: &mut [u8]) -> ForgeResult<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = tree.read(path, off, &mut buf[done..])?;
        if n == 0 {
            return Err(ForgeError::Io(format!("{path}: unexpected end of file at {off}")));
        }
        done += n;
        off += n as u64;
    }
    Ok(())
}

/// One image of a WIM, from its XML data.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct WimImage {
    pub index: u32,
    pub name: String,
    pub description: String,
    pub display_name: String,
    /// `WINDOWS/EDITIONID`: "Professional", "Core", "ServerStandard"…
    pub edition_id: String,
    /// `WINDOWS/INSTALLATIONTYPE`: "Client", "Server", "Server Core", "WindowsPE".
    pub installation_type: String,
    pub product_name: String,
    /// `WINDOWS/ARCH`: 0 x86, 5 ARM, 6 IA-64, 9 x64, 12 ARM64 (PROCESSOR_ARCHITECTURE_*).
    pub arch: Option<u32>,
    pub major: Option<u32>,
    pub minor: Option<u32>,
    pub build: Option<u32>,
    pub sp_build: Option<u32>,
    pub languages: Vec<String>,
    pub default_language: Option<String>,
    pub total_bytes: Option<u64>,
}

/// Decode the XML resource: UTF-16LE, normally with a byte-order mark.
pub fn decode_xml(raw: &[u8]) -> ForgeResult<String> {
    let b = raw.strip_prefix(&[0xFF, 0xFE]).unwrap_or(raw);
    if b.len() % 2 != 0 {
        return Err(bad("XML data has an odd length"));
    }
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let s = String::from_utf16_lossy(&units);
    Ok(s.trim_end_matches('\0').to_string())
}

fn clip(s: &str) -> String {
    let s = s.trim();
    match s.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// Integers in the XML are decimal, except the times' "0x…" parts.
fn num(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

/// The images described by a WIM's XML data (`<WIM><IMAGE INDEX="n">…`), in index order.
pub fn parse_xml(xml: &str) -> ForgeResult<Vec<WimImage>> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<String> = vec![];
    let mut images: Vec<WimImage> = vec![];
    let mut cur: Option<WimImage> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if stack.len() >= 64 {
                    return Err(bad("XML nested too deeply"));
                }
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_uppercase();
                if name == "IMAGE" && stack.len() == 1 {
                    if images.len() >= MAX_IMAGES {
                        return Err(bad("too many images in the XML"));
                    }
                    let mut img = WimImage::default();
                    for a in e.attributes().flatten() {
                        if a.key.local_name().as_ref().eq_ignore_ascii_case(b"INDEX") {
                            img.index = a.unescape_value().ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                        }
                    }
                    cur = Some(img);
                }
                stack.push(name);
            }
            Ok(Event::End(_)) => {
                if stack.pop().as_deref() == Some("IMAGE") && stack.len() == 1 {
                    if let Some(img) = cur.take() {
                        images.push(img);
                    }
                }
            }
            Ok(Event::Text(txt)) => {
                let Some(img) = cur.as_mut() else { continue };
                let v = txt.unescape().map(|c| c.into_owned()).unwrap_or_default();
                // Path below <WIM><IMAGE>.
                let rel: Vec<&str> = stack.iter().skip(2).map(String::as_str).collect();
                match rel.as_slice() {
                    ["NAME"] => img.name = clip(&v),
                    ["DESCRIPTION"] => img.description = clip(&v),
                    ["DISPLAYNAME"] => img.display_name = clip(&v),
                    ["TOTALBYTES"] => img.total_bytes = num(&v),
                    ["WINDOWS", "ARCH"] => img.arch = num(&v).and_then(|n| u32::try_from(n).ok()),
                    ["WINDOWS", "EDITIONID"] => img.edition_id = clip(&v),
                    ["WINDOWS", "INSTALLATIONTYPE"] => img.installation_type = clip(&v),
                    ["WINDOWS", "PRODUCTNAME"] => img.product_name = clip(&v),
                    ["WINDOWS", "VERSION", "MAJOR"] => img.major = num(&v).and_then(|n| u32::try_from(n).ok()),
                    ["WINDOWS", "VERSION", "MINOR"] => img.minor = num(&v).and_then(|n| u32::try_from(n).ok()),
                    ["WINDOWS", "VERSION", "BUILD"] => img.build = num(&v).and_then(|n| u32::try_from(n).ok()),
                    ["WINDOWS", "VERSION", "SPBUILD"] => img.sp_build = num(&v).and_then(|n| u32::try_from(n).ok()),
                    ["WINDOWS", "LANGUAGES", "LANGUAGE"] => {
                        if img.languages.len() < 256 {
                            img.languages.push(clip(&v));
                        }
                    }
                    ["WINDOWS", "LANGUAGES", "DEFAULT"] => img.default_language = Some(clip(&v)),
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(bad(format!("malformed XML data: {e}"))),
        }
    }
    images.sort_by_key(|i| i.index);
    Ok(images)
}

/// Everything we read from an install image without touching its resources.
#[derive(Clone, Debug)]
pub struct WimFile {
    pub path: String,
    pub size: u64,
    pub header: WimHeader,
    /// The XML resource as stored (UTF-16LE, BOM included), copied verbatim into each part.
    pub xml_raw: Vec<u8>,
    pub images: Vec<WimImage>,
}

impl WimFile {
    /// Read the header and the XML data of `path` (`size` bytes) from `tree`.
    pub fn open(tree: &mut dyn SourceTree, path: &str, size: u64) -> ForgeResult<WimFile> {
        let mut hb = [0u8; HEADER_SIZE];
        if size < HEADER_SIZE as u64 {
            return Err(bad("file too short for a header"));
        }
        read_exact(tree, path, 0, &mut hb)?;
        let header = WimHeader::parse(&hb)?;
        let mut xml_raw = vec![];
        let mut images = vec![];
        if !header.xml.is_empty() {
            let (off, len) = header.xml.range(size, "XML data")?;
            if header.xml.flags & res_flags::COMPRESSED != 0 {
                return Err(ForgeError::Unsupported("WIM: compressed XML data".into()));
            }
            if len > MAX_XML {
                return Err(bad(format!("XML data too large ({len} bytes)")));
            }
            xml_raw = vec![0u8; len as usize];
            read_exact(tree, path, off, &mut xml_raw)?;
            images = parse_xml(&decode_xml(&xml_raw)?)?;
        }
        Ok(WimFile { path: path.to_string(), size, header, xml_raw, images })
    }

    /// The blob table, validated against the file size.
    pub fn blob_table(&self, tree: &mut dyn SourceTree) -> ForgeResult<Vec<BlobEntry>> {
        let h = &self.header;
        if h.blob_table.is_empty() {
            return Ok(vec![]);
        }
        let (off, len) = h.blob_table.range(self.size, "blob table")?;
        if h.blob_table.flags & res_flags::COMPRESSED != 0 {
            return Err(ForgeError::Unsupported("WIM: compressed blob table".into()));
        }
        if len > MAX_BLOB_TABLE {
            return Err(bad(format!("blob table too large ({len} bytes)")));
        }
        if len % BLOB_ENTRY_SIZE as u64 != 0 {
            return Err(bad("blob table size is not a multiple of 50"));
        }
        let mut raw = vec![0u8; len as usize];
        read_exact(tree, &self.path, off, &mut raw)?;
        let mut out = Vec::with_capacity(raw.len() / BLOB_ENTRY_SIZE);
        for c in raw.chunks_exact(BLOB_ENTRY_SIZE) {
            let e = BlobEntry::parse(c);
            // Entries inside a solid resource address its uncompressed data, not the file.
            if !e.is_solid() || e.res.uncompressed_size == SOLID_RESOURCE_MAGIC_SIZE {
                e.res.range(self.size, "blob")?;
            }
            out.push(e);
        }
        Ok(out)
    }

    /// A solid (ESD-style) WIM: the version says so, or a blob table entry is solid.
    pub fn is_solid(&self, table: Option<&[BlobEntry]>) -> bool {
        self.header.version == VERSION_SOLID || table.is_some_and(|t| t.iter().any(BlobEntry::is_solid))
    }
}

/// `SOLID_RESOURCE_MAGIC_SIZE` (wimlib `resource.h`): the "uncompressed size" of the entry
/// that describes a solid resource itself.
pub const SOLID_RESOURCE_MAGIC_SIZE: u64 = 0x1_0000_0000;

/// One `.swm` part to write: which blobs, and its exact file size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedPart {
    /// Indices into the blob table: metadata first (in table order, which is image order),
    /// then the other blobs in source offset order.
    pub blobs: Vec<usize>,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct SplitPlan {
    pub parts: Vec<PlannedPart>,
    /// Shared by every part (a new one, as wimlib does).
    pub guid: [u8; 16],
}

impl SplitPlan {
    pub fn data_bytes(&self, table: &[BlobEntry]) -> u64 {
        self.parts.iter().flat_map(|p| &p.blobs).map(|&i| table[i].res.size_in_wim).sum()
    }
}

/// Why a WIM can't be split into parts of at most `limit` bytes (None: it can).
pub fn unsplittable_reason(wim: &WimFile, table: &[BlobEntry]) -> Option<String> {
    let h = &wim.header;
    if h.is_pipable() {
        return Some("pipable WIM".into());
    }
    if wim.is_solid(Some(table)) {
        return Some(t(
            "immagine ESD (compressione solida LZMS): non si può dividere",
            "ESD image (solid LZMS compression): it can't be split",
        ));
    }
    if h.total_parts != 1 {
        return Some(t("il WIM è già diviso in parti", "the WIM is already split into parts"));
    }
    if table.iter().any(|e| e.part_number != 1) {
        return Some(bad("blob table refers to other parts").to_string());
    }
    None
}

/// Plan the parts like wimlib's `split.c`: metadata resources in part 1, then the other
/// blobs in file order, starting a new part whenever the next one would push the current
/// part's file over `limit` (header, data, blob table and XML all counted).
pub fn plan_split(wim: &WimFile, table: &[BlobEntry], limit: u64) -> ForgeResult<SplitPlan> {
    if let Some(r) = unsplittable_reason(wim, table) {
        return Err(ForgeError::Unsupported(r));
    }
    let too_big = |what: &str| {
        ForgeError::Unsupported(t(
            &format!("{what}: più grande della dimensione massima di una parte ({limit} byte)"),
            &format!("{what}: larger than the maximum part size ({limit} bytes)"),
        ))
    };
    let fixed = (HEADER_SIZE as u64).checked_add(wim.xml_raw.len() as u64).ok_or_else(|| bad("size overflow"))?;
    let cost = |e: &BlobEntry| e.res.size_in_wim.checked_add(BLOB_ENTRY_SIZE as u64);
    let meta: Vec<usize> = (0..table.len()).filter(|&i| table[i].is_metadata()).collect();
    if (meta.len() as u64) < wim.header.image_count as u64 {
        return Err(bad(format!("{} images but {} metadata resources", wim.header.image_count, meta.len())));
    }
    let mut size = fixed;
    for &i in &meta {
        size = cost(&table[i]).and_then(|c| size.checked_add(c)).ok_or_else(|| bad("size overflow"))?;
    }
    if size > limit {
        return Err(too_big(&t("i metadati delle immagini", "the image metadata")));
    }
    let mut others: Vec<usize> = (0..table.len()).filter(|&i| !table[i].is_metadata()).collect();
    others.sort_by_key(|&i| (table[i].res.offset, i));
    let mut parts = vec![PlannedPart { blobs: meta, size }];
    for i in others {
        let c = cost(&table[i]).ok_or_else(|| bad("size overflow"))?;
        let cur = parts.last_mut().expect("at least one part");
        match cur.size.checked_add(c) {
            Some(s) if s <= limit => {
                cur.blobs.push(i);
                cur.size = s;
            }
            _ => {
                let s = fixed.checked_add(c).filter(|&s| s <= limit).ok_or_else(|| too_big(&format!("blob {}", hex(&table[i].hash))))?;
                if parts.len() >= u16::MAX as usize {
                    return Err(ForgeError::Unsupported("WIM: more than 65535 parts".into()));
                }
                parts.push(PlannedPart { blobs: vec![i], size: s });
            }
        }
    }
    Ok(SplitPlan { parts, guid: *uuid::Uuid::new_v4().as_bytes() })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `install.swm`, `install2.swm`, `install3.swm`… (DISM / wimlib naming).
pub fn part_name(base: &str, part: u16) -> String {
    if part <= 1 {
        format!("{base}.swm")
    } else {
        format!("{base}{part}.swm")
    }
}

/// Where the parts go. Each part is written sequentially from its first byte to its last.
pub trait PartSink {
    fn begin(&mut self, part: u16, name: &str, size: u64) -> ForgeResult<()>;
    fn append(&mut self, data: &[u8]) -> ForgeResult<()>;
    fn end(&mut self) -> ForgeResult<()>;
}

/// What was written: the name, size and SHA-256 of each part.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WrittenPart {
    pub name: String,
    pub size: u64,
    #[serde(skip)]
    pub sha256: [u8; 32],
}

/// Write every part of `plan` to `sink`, streaming the resources from `tree`. Progress is
/// reported in bytes of resource data; cancellation is honoured between chunks.
pub fn write_split(
    tree: &mut dyn SourceTree,
    wim: &WimFile,
    table: &[BlobEntry],
    plan: &SplitPlan,
    base: &str,
    sink: &mut dyn PartSink,
    ctx: &JobCtx,
) -> ForgeResult<Vec<WrittenPart>> {
    const CHUNK: usize = 4 << 20;
    let total = plan.parts.len() as u16;
    let mut buf = vec![0u8; CHUNK];
    let mut out = vec![];
    // Which metadata entry is the boot image's (the n-th metadata entry is image n).
    let boot_meta = (wim.header.boot_index > 0)
        .then(|| (0..table.len()).filter(|&i| table[i].is_metadata()).nth(wim.header.boot_index as usize - 1))
        .flatten();
    for (pi, part) in plan.parts.iter().enumerate() {
        ctx.check()?;
        let part_no = pi as u16 + 1;
        // Layout: header, data in source order, blob table, XML.
        let mut order = part.blobs.clone();
        order.sort_by_key(|&i| (table[i].res.offset, i));
        let mut new_off = std::collections::HashMap::with_capacity(order.len());
        let mut pos = HEADER_SIZE as u64;
        for &i in &order {
            new_off.insert(i, pos);
            pos += table[i].res.size_in_wim;
        }
        let table_off = pos;
        let table_len = (part.blobs.len() * BLOB_ENTRY_SIZE) as u64;
        let xml_off = table_off + table_len;
        let file_size = xml_off + wim.xml_raw.len() as u64;
        debug_assert_eq!(file_size, part.size);

        let mut h = wim.header.clone();
        h.flags = (h.flags | hdr_flags::SPANNED) & !hdr_flags::WRITE_IN_PROGRESS;
        h.guid = plan.guid;
        h.part_number = part_no;
        h.total_parts = total;
        h.blob_table = ResHdr { size_in_wim: table_len, flags: wim.header.blob_table.flags, offset: table_off, uncompressed_size: table_len };
        h.xml = if wim.xml_raw.is_empty() {
            ResHdr::default()
        } else {
            let n = wim.xml_raw.len() as u64;
            ResHdr { size_in_wim: n, flags: wim.header.xml.flags, offset: xml_off, uncompressed_size: n }
        };
        h.integrity = ResHdr::default();
        match boot_meta.filter(|i| part_no == 1 && new_off.contains_key(i)) {
            Some(i) => h.boot_metadata = ResHdr { offset: new_off[&i], ..table[i].res },
            None => {
                h.boot_metadata = ResHdr::default();
                if part_no != 1 {
                    h.boot_index = 0;
                }
            }
        }

        let name = part_name(base, part_no);
        sink.begin(part_no, &name, file_size)?;
        let mut sha = sha2::Sha256::new();
        let hb = h.to_bytes();
        sha.update(hb);
        sink.append(&hb)?;
        for &i in &order {
            let r = table[i].res;
            let mut done = 0u64;
            while done < r.size_in_wim {
                let n = (r.size_in_wim - done).min(CHUNK as u64) as usize;
                read_exact(tree, &wim.path, r.offset + done, &mut buf[..n])?;
                sha.update(&buf[..n]);
                sink.append(&buf[..n])?;
                done += n as u64;
                ctx.advance(n as u64)?;
            }
        }
        let mut tb = vec![0u8; table_len as usize];
        for (k, &i) in part.blobs.iter().enumerate() {
            let mut e = table[i];
            e.res.offset = new_off[&i];
            e.part_number = part_no;
            e.write(&mut tb[k * BLOB_ENTRY_SIZE..(k + 1) * BLOB_ENTRY_SIZE]);
        }
        sha.update(&tb);
        sink.append(&tb)?;
        sha.update(&wim.xml_raw);
        sink.append(&wim.xml_raw)?;
        sink.end()?;
        out.push(WrittenPart { name, size: file_size, sha256: sha.finalize().into() });
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::forge::tree::TreeEntry;

    /// An in-memory tree with a few files (tests).
    pub(crate) struct MemTree(pub Vec<(String, Vec<u8>)>);

    impl SourceTree for MemTree {
        fn entries(&mut self) -> ForgeResult<Vec<TreeEntry>> {
            let mut out: Vec<TreeEntry> = vec![];
            let mut dirs = std::collections::BTreeSet::new();
            for (p, _) in &self.0 {
                let mut acc = String::new();
                let comps: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
                for c in &comps[..comps.len() - 1] {
                    acc.push('/');
                    acc.push_str(c);
                    dirs.insert(acc.clone());
                }
            }
            for d in dirs {
                out.push(TreeEntry { path: d, is_dir: true, size: 0, mode: 0o755, mtime: 0, symlink: None });
            }
            for (p, d) in &self.0 {
                out.push(TreeEntry { path: p.clone(), is_dir: false, size: d.len() as u64, mode: 0o644, mtime: 1_700_000_000, symlink: None });
            }
            out.sort_by(|a, b| a.path.split('/').count().cmp(&b.path.split('/').count()).then(a.path.cmp(&b.path)));
            Ok(out)
        }
        fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> ForgeResult<usize> {
            let d = &self.0.iter().find(|(p, _)| p.eq_ignore_ascii_case(path)).ok_or_else(|| ForgeError::Io(format!("{path}: not found")))?.1;
            if offset >= d.len() as u64 {
                return Ok(0);
            }
            let n = buf.len().min(d.len() - offset as usize);
            buf[..n].copy_from_slice(&d[offset as usize..offset as usize + n]);
            Ok(n)
        }
        fn label(&self) -> String {
            "MEM".into()
        }
    }

    pub(crate) fn utf16_xml(s: &str) -> Vec<u8> {
        let mut v = vec![0xFF, 0xFE];
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    /// A structurally valid uncompressed WIM: `blobs` file blobs and one opaque metadata
    /// resource per image (enough for everything but extraction).
    pub(crate) fn synth_wim(blobs: &[usize], images: usize, xml: &str, boot_index: u32) -> Vec<u8> {
        let mut data = vec![0u8; HEADER_SIZE];
        let mut entries = vec![];
        let mut seed = 0x1234_5678u32;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed & 0xFF) as u8
        };
        for (k, &len) in blobs.iter().enumerate() {
            let off = data.len() as u64;
            for _ in 0..len {
                data.push(rnd());
            }
            let mut hash = [0u8; 20];
            hash[..8].copy_from_slice(&(k as u64 + 1).to_le_bytes());
            entries.push(BlobEntry { res: ResHdr { size_in_wim: len as u64, flags: 0, offset: off, uncompressed_size: len as u64 }, part_number: 1, refcnt: 1, hash });
        }
        let mut meta = vec![];
        for i in 0..images {
            let off = data.len() as u64;
            let len = 300 + i * 17;
            data.extend(std::iter::repeat(0xA0 + i as u8).take(len));
            let mut hash = [0xEEu8; 20];
            hash[0] = i as u8;
            meta.push(BlobEntry { res: ResHdr { size_in_wim: len as u64, flags: res_flags::METADATA, offset: off, uncompressed_size: len as u64 }, part_number: 1, refcnt: 1, hash });
        }
        // wimlib puts the metadata entries first in the table.
        let all: Vec<BlobEntry> = meta.iter().chain(entries.iter()).copied().collect();
        let table_off = data.len() as u64;
        for e in &all {
            let mut b = [0u8; BLOB_ENTRY_SIZE];
            e.write(&mut b);
            data.extend_from_slice(&b);
        }
        let xml_raw = utf16_xml(xml);
        let xml_off = data.len() as u64;
        data.extend_from_slice(&xml_raw);
        let mut raw = [0u8; HEADER_SIZE];
        raw[..8].copy_from_slice(&MAGIC);
        let h = WimHeader {
            raw,
            magic: MAGIC,
            version: VERSION_DEFAULT,
            flags: hdr_flags::RP_FIX,
            chunk_size: 0,
            guid: [7u8; 16],
            part_number: 1,
            total_parts: 1,
            image_count: images as u32,
            blob_table: ResHdr { size_in_wim: (all.len() * BLOB_ENTRY_SIZE) as u64, flags: res_flags::METADATA, offset: table_off, uncompressed_size: (all.len() * BLOB_ENTRY_SIZE) as u64 },
            xml: ResHdr { size_in_wim: xml_raw.len() as u64, flags: res_flags::METADATA, offset: xml_off, uncompressed_size: xml_raw.len() as u64 },
            boot_metadata: if boot_index > 0 { meta[boot_index as usize - 1].res } else { ResHdr::default() },
            boot_index,
            integrity: ResHdr::default(),
        };
        data[..HEADER_SIZE].copy_from_slice(&h.to_bytes());
        data
    }

    pub(crate) const XML: &str = r#"<WIM><TOTALBYTES>1</TOTALBYTES><IMAGE INDEX="2"><NAME>Windows 11 Pro</NAME><DESCRIPTION>Windows 11 Pro</DESCRIPTION><WINDOWS><ARCH>9</ARCH><PRODUCTNAME>Microsoft&#xAE; Windows&#xAE; Operating System</PRODUCTNAME><EDITIONID>Professional</EDITIONID><INSTALLATIONTYPE>Client</INSTALLATIONTYPE><LANGUAGES><LANGUAGE>it-IT</LANGUAGE><DEFAULT>it-IT</DEFAULT></LANGUAGES><VERSION><MAJOR>10</MAJOR><MINOR>0</MINOR><BUILD>26100</BUILD><SPBUILD>1742</SPBUILD></VERSION></WINDOWS><DISPLAYNAME>Windows 11 Pro</DISPLAYNAME></IMAGE><IMAGE INDEX="1"><NAME>Windows 11 Home</NAME><WINDOWS><ARCH>9</ARCH><EDITIONID>Core</EDITIONID><VERSION><BUILD>26100</BUILD></VERSION></WINDOWS></IMAGE></WIM>"#;

    struct MemSink(Vec<(String, Vec<u8>)>);
    impl PartSink for MemSink {
        fn begin(&mut self, _p: u16, name: &str, size: u64) -> ForgeResult<()> {
            self.0.push((name.to_string(), Vec::with_capacity(size as usize)));
            Ok(())
        }
        fn append(&mut self, d: &[u8]) -> ForgeResult<()> {
            self.0.last_mut().unwrap().1.extend_from_slice(d);
            Ok(())
        }
        fn end(&mut self) -> ForgeResult<()> {
            Ok(())
        }
    }

    #[test]
    fn header_round_trips_and_garbage_is_rejected() {
        let w = synth_wim(&[10, 20], 2, XML, 1);
        let h = WimHeader::parse(&w).unwrap();
        assert_eq!(h.to_bytes(), h.raw);
        assert_eq!((h.part_number, h.total_parts, h.image_count, h.boot_index), (1, 1, 2, 1));
        assert_eq!(h.compression(), "none");
        let mut bad = w.clone();
        bad[0] = b'X';
        assert!(WimHeader::parse(&bad).is_err());
        let mut bad = w.clone();
        bad[12..16].copy_from_slice(&0x1234u32.to_le_bytes());
        assert!(WimHeader::parse(&bad).is_err());
        let mut bad = w.clone();
        bad[40..42].copy_from_slice(&3u16.to_le_bytes()); // part 3 of 1
        assert!(WimHeader::parse(&bad).is_err());
        assert!(WimHeader::parse(&w[..100]).is_err());
    }

    #[test]
    fn xml_editions_are_parsed_in_index_order() {
        let imgs = parse_xml(XML).unwrap();
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].index, 1);
        assert_eq!(imgs[0].name, "Windows 11 Home");
        assert_eq!(imgs[0].edition_id, "Core");
        assert_eq!(imgs[1].edition_id, "Professional");
        assert_eq!(imgs[1].arch, Some(9));
        assert_eq!(imgs[1].build, Some(26100));
        assert_eq!(imgs[1].languages, vec!["it-IT"]);
        assert_eq!(imgs[1].product_name, "Microsoft® Windows® Operating System");
        assert!(parse_xml("<WIM><IMAGE INDEX=\"1\"><NAME>x</NAME></WIM>").is_err(), "mismatched tags");
        assert_eq!(decode_xml(&utf16_xml("<WIM/>\0")).unwrap(), "<WIM/>");
        assert!(decode_xml(&[0xFF, 0xFE, 0x41]).is_err());
    }

    #[test]
    fn a_wim_is_split_into_parts_that_hold_every_blob_once() {
        let w = synth_wim(&[5000, 7000, 3000, 9000, 100, 0, 4000], 3, XML, 2);
        let mut tree = MemTree(vec![("/sources/install.wim".into(), w.clone())]);
        let wim = WimFile::open(&mut tree, "/sources/install.wim", w.len() as u64).unwrap();
        assert_eq!(wim.images.len(), 2);
        let table = wim.blob_table(&mut tree).unwrap();
        assert_eq!(table.len(), 10);
        assert!(unsplittable_reason(&wim, &table).is_none());
        let limit = 12_000;
        let plan = plan_split(&wim, &table, limit).unwrap();
        assert!(plan.parts.len() >= 3, "{plan:?}");
        let mut sink = MemSink(vec![]);
        let ctx = JobCtx::detached(false);
        let written = write_split(&mut tree, &wim, &table, &plan, "install", &mut sink, &ctx).unwrap();
        assert_eq!(ctx.done(), plan.data_bytes(&table));
        assert_eq!(written[0].name, "install.swm");
        assert_eq!(written[1].name, "install2.swm");
        let mut seen = std::collections::HashMap::new();
        for (k, (name, data)) in sink.0.iter().enumerate() {
            assert!(data.len() as u64 <= limit, "{name} is {} bytes", data.len());
            assert_eq!(data.len() as u64, written[k].size);
            assert_eq!(written[k].sha256, <[u8; 32]>::from(sha2::Sha256::digest(data)));
            let h = WimHeader::parse(data).unwrap();
            assert_eq!((h.part_number, h.total_parts), (k as u16 + 1, sink.0.len() as u16));
            assert_eq!(h.guid, plan.guid);
            assert_eq!(h.image_count, 3);
            assert!(h.flags & hdr_flags::SPANNED != 0 && h.flags & hdr_flags::RP_FIX != 0);
            let mut t2 = MemTree(vec![("/p".into(), data.clone())]);
            let part = WimFile::open(&mut t2, "/p", data.len() as u64).unwrap();
            assert_eq!(part.xml_raw, wim.xml_raw, "XML copied verbatim");
            let pt = part.blob_table(&mut t2).unwrap();
            let metas = pt.iter().filter(|e| e.is_metadata()).count();
            assert_eq!(metas, if k == 0 { 3 } else { 0 }, "metadata only in part 1");
            if k == 0 {
                assert_eq!(h.boot_index, 2);
                assert_eq!(h.boot_metadata, pt[1].res, "boot metadata points at image 2's");
                let order: Vec<u8> = pt.iter().filter(|e| e.is_metadata()).map(|e| e.hash[0]).collect();
                assert_eq!(order, vec![0, 1, 2], "image order kept");
            } else {
                assert_eq!((h.boot_index, h.boot_metadata), (0, ResHdr::default()));
            }
            for e in pt {
                assert_eq!(e.part_number, k as u16 + 1);
                let orig = table.iter().find(|o| o.hash == e.hash).unwrap();
                let a = &data[e.res.offset as usize..(e.res.offset + e.res.size_in_wim) as usize];
                let b = &w[orig.res.offset as usize..(orig.res.offset + orig.res.size_in_wim) as usize];
                assert_eq!(a, b);
                assert!(seen.insert(e.hash, k).is_none(), "blob in two parts");
            }
        }
        assert_eq!(seen.len(), table.len());
    }

    #[test]
    fn blobs_bigger_than_a_part_and_solid_wims_are_not_split() {
        let w = synth_wim(&[5000, 20_000], 1, XML, 0);
        let mut tree = MemTree(vec![("/i.wim".into(), w.clone())]);
        let wim = WimFile::open(&mut tree, "/i.wim", w.len() as u64).unwrap();
        let table = wim.blob_table(&mut tree).unwrap();
        assert!(plan_split(&wim, &table, 12_000).is_err());
        assert!(plan_split(&wim, &table, 30_000).is_ok());
        let mut solid = w.clone();
        solid[12..16].copy_from_slice(&VERSION_SOLID.to_le_bytes());
        let mut tree = MemTree(vec![("/i.esd".into(), solid.clone())]);
        let wim = WimFile::open(&mut tree, "/i.esd", solid.len() as u64).unwrap();
        let table = wim.blob_table(&mut tree).unwrap();
        assert!(wim.is_solid(Some(&table)));
        assert!(unsplittable_reason(&wim, &table).unwrap().contains("ESD"));
    }

    #[test]
    fn corrupt_tables_are_errors_not_panics() {
        let w = synth_wim(&[100, 200], 1, XML, 0);
        let h = WimHeader::parse(&w).unwrap();
        // Blob table pointing past the end, absurd sizes, odd lengths.
        for patch in [
            ResHdr { offset: w.len() as u64, ..h.blob_table },
            ResHdr { size_in_wim: (1 << 56) - 1, ..h.blob_table },
            ResHdr { size_in_wim: h.blob_table.size_in_wim - 1, ..h.blob_table },
            ResHdr { offset: u64::MAX - 10, ..h.blob_table },
        ] {
            let mut c = w.clone();
            patch.write(&mut c[48..72]);
            let mut tree = MemTree(vec![("/i.wim".into(), c.clone())]);
            let wim = WimFile::open(&mut tree, "/i.wim", c.len() as u64).unwrap();
            assert!(wim.blob_table(&mut tree).is_err(), "{patch:?}");
        }
        // An entry pointing past the end.
        let mut c = w.clone();
        let o = h.blob_table.offset as usize + BLOB_ENTRY_SIZE + 8;
        c[o..o + 8].copy_from_slice(&(w.len() as u64).to_le_bytes());
        let mut tree = MemTree(vec![("/i.wim".into(), c.clone())]);
        let wim = WimFile::open(&mut tree, "/i.wim", c.len() as u64).unwrap();
        assert!(wim.blob_table(&mut tree).is_err());
        // XML beyond the end / too big.
        let mut c = w.clone();
        ResHdr { size_in_wim: 1 << 40, ..h.xml }.write(&mut c[72..96]);
        let mut tree = MemTree(vec![("/i.wim".into(), c.clone())]);
        assert!(WimFile::open(&mut tree, "/i.wim", c.len() as u64).is_err());
        // Random garbage never panics.
        let mut seed = 99u64;
        for _ in 0..300 {
            let mut c = w.clone();
            for _ in 0..8 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let pos = (seed >> 33) as usize % 200;
                c[pos] = (seed >> 24) as u8;
            }
            let mut tree = MemTree(vec![("/i.wim".into(), c.clone())]);
            if let Ok(wim) = WimFile::open(&mut tree, "/i.wim", c.len() as u64) {
                if let Ok(table) = wim.blob_table(&mut tree) {
                    let _ = plan_split(&wim, &table, 4000);
                }
            }
        }
    }
}
