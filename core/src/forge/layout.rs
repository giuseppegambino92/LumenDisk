//! Writing partition tables: GPT (with protective or hybrid MBR and backup), MBR, APM.
//!
//! [`write_table`] places partitions 1 MiB-aligned, writes the table(s), and returns where each
//! partition landed. It never touches partition contents beyond the table areas;
//! [`super::format`] formats them afterwards.
//!
//! Units: every table is written in the device's *logical* sectors ([`sector_size`]): 512
//! bytes, or 4096 on 4K-native ("4Kn") disks. A GPT LBA is one logical block (UEFI 2.10
//! §5.3.1), and so is an MBR LBA; on a 4Kn disk the 128 GPT entries fill 4 sectors instead of
//! 32. APM uses its own block size, recorded in the driver descriptor (`sbBlkSize`): we use the
//! logical sector size there too.
//!
//! References:
//! - UEFI Specification 2.10, §5.2 (legacy and protective MBR), §5.3 (GPT header, entry
//!   array, backup placement, CRC32).
//! - Inside Macintosh: Devices (1994), chapter 3 "SCSI Manager", "Data Organization on
//!   Devices": Driver Descriptor Record (block 0) and partition map entries (`'PM'`).

use serde::{Deserialize, Serialize};

use super::plan::Scheme;
use super::{ForgeError, ForgeResult, TargetFs};
use crate::block::BlockDev;
use crate::i18n::t;

/// What a partition will hold: decides the GPT type GUID / MBR type byte / APM type string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartType {
    Fs(TargetFs),
    /// EFI System Partition (FAT).
    EfiSystem,
    /// Explicit types for special cases (e.g. the UEFI:NTFS helper partition).
    Raw { mbr: u8, gpt: [u8; 16] },
}

#[derive(Clone, Debug)]
pub struct NewPart {
    /// Bytes; None = the rest of the disk (last partition only).
    pub size: Option<u64>,
    pub kind: PartType,
    /// GPT / APM name.
    pub name: String,
    /// MBR active flag (BIOS boot) / GPT legacy BIOS bootable attribute.
    pub bootable: bool,
}

/// A partition as placed on the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Placed {
    /// 1-based.
    pub index: u32,
    pub start: u64,
    pub size: u64,
}

#[derive(Clone, Debug, Default)]
pub struct TableOptions {
    /// x86 boot code for the first 440 bytes of the MBR (BIOS boot); None = zeros.
    pub mbr_boot_code: Option<Vec<u8>>,
    /// Disk GUID (GPT) / disk signature (low 4 bytes, MBR). None = random.
    pub disk_id: Option<[u8; 16]>,
    /// Alignment of partition starts in bytes (default 1 MiB).
    pub align: Option<u64>,
    /// GPT only: 1-based indices of partitions to mirror into the MBR as well ("hybrid MBR",
    /// at most 3, each below 2 TiB). Empty: a plain protective MBR (the default).
    pub hybrid: Vec<u32>,
}

/// GPT partition type GUIDs (UEFI 2.10 Table 5.7 and the de-facto registry).
pub mod guids {
    pub const EFI_SYSTEM: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
    pub const BIOS_BOOT: &str = "21686148-6449-6E6F-744E-656564454649";
    pub const MS_BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
    pub const MS_RESERVED: &str = "E3C9E316-0B5C-4DB8-817D-F92DF00215AE";
    pub const MS_RECOVERY: &str = "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC";
    pub const MS_LDM_DATA: &str = "AF9B60A0-1431-4F62-BC68-3311714A69AD";
    pub const LINUX_FS: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";
    pub const LINUX_SWAP: &str = "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F";
    pub const LINUX_LVM: &str = "E6D6D379-F507-44C2-A23C-238F2A3DF928";
    pub const LINUX_RAID: &str = "A19D880F-05FC-4D3B-A006-743F0F84911E";
    pub const FREEBSD_DATA: &str = "516E7CB4-6ECF-11D6-8FF8-00022D09712B";
    pub const FREEBSD_UFS: &str = "516E7CB6-6ECF-11D6-8FF8-00022D09712B";
    pub const APPLE_HFS: &str = "48465300-0000-11AA-AA11-00306543ECAC";
    pub const APPLE_APFS: &str = "7C3457EF-0000-11AA-AA11-00306543ECAC";
}

/// Microsoft basic data, Linux filesystem, EFI system… as mixed-endian GUID bytes.
pub fn gpt_type(kind: PartType) -> [u8; 16] {
    match kind {
        PartType::EfiSystem => guid(guids::EFI_SYSTEM),
        PartType::Raw { gpt, .. } => gpt,
        PartType::Fs(fs) => match fs {
            TargetFs::Fat16 | TargetFs::Fat32 | TargetFs::Exfat | TargetFs::Ntfs => guid(guids::MS_BASIC_DATA),
            TargetFs::Ufs2 => guid(guids::FREEBSD_UFS),
            _ => guid(guids::LINUX_FS),
        },
    }
}

pub fn mbr_type(kind: PartType) -> u8 {
    match kind {
        PartType::EfiSystem => 0xEF,
        PartType::Raw { mbr, .. } => mbr,
        PartType::Fs(fs) => match fs {
            TargetFs::Fat16 => 0x0E,
            TargetFs::Fat32 => 0x0C,
            TargetFs::Exfat | TargetFs::Ntfs => 0x07,
            TargetFs::Ufs2 => 0xA5,
            _ => 0x83,
        },
    }
}

/// APM partition type string (`pmParType`). Apple's own names where they exist (what Disk
/// Utility writes for MS-DOS / exFAT / NTFS partitions), the Linux and FreeBSD conventions
/// otherwise.
pub fn apm_type(kind: PartType) -> &'static str {
    let by_mbr = |t: u8| match t {
        0x01 | 0x04 | 0x06 | 0x0E => "DOS_FAT_16",
        0x0B | 0x0C => "DOS_FAT_32",
        0x07 => "Windows_NTFS",
        0xEF => "EFI",
        0xAF => "Apple_HFS",
        0xA5 => "FreeBSD-UFS",
        _ => "Linux",
    };
    match kind {
        PartType::EfiSystem => "EFI",
        PartType::Raw { mbr, .. } => by_mbr(mbr),
        PartType::Fs(fs) => match fs {
            TargetFs::Fat16 => "DOS_FAT_16",
            TargetFs::Fat32 => "DOS_FAT_32",
            TargetFs::Exfat | TargetFs::Ntfs => "Windows_NTFS",
            TargetFs::Ufs2 => "FreeBSD-UFS",
            _ => "Linux",
        },
    }
}

/// Parse "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" into GPT's mixed-endian byte order
/// (UEFI 2.10 Appendix A: the first three fields little-endian, the rest as bytes).
/// Panics on a malformed literal: only ever called with constants.
pub fn guid(s: &str) -> [u8; 16] {
    parse_guid(s).expect("guid literal")
}

/// Like [`guid`], for strings that may be malformed (None).
pub fn parse_guid(s: &str) -> Option<[u8; 16]> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 || parts.iter().map(|p| p.len()).collect::<Vec<_>>() != [8, 4, 4, 4, 12] {
        return None;
    }
    let hex: String = parts.concat();
    let mut raw = [0u8; 16];
    for (i, b) in raw.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    let mut g = [0u8; 16];
    g[0..4].copy_from_slice(&[raw[3], raw[2], raw[1], raw[0]]);
    g[4..6].copy_from_slice(&[raw[5], raw[4]]);
    g[6..8].copy_from_slice(&[raw[7], raw[6]]);
    g[8..16].copy_from_slice(&raw[8..16]);
    Some(g)
}

/// The canonical string of a mixed-endian GUID.
pub fn guid_string(g: &[u8; 16]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32::from_le_bytes([g[0], g[1], g[2], g[3]]),
        u16::from_le_bytes([g[4], g[5]]),
        u16::from_le_bytes([g[6], g[7]]),
        g[8],
        g[9],
        g[10],
        g[11],
        g[12],
        g[13],
        g[14],
        g[15]
    )
}

/// CRC-32 (IEEE 802.3, as used by GPT).
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: once_cell::sync::Lazy<[u32; 256]> = once_cell::sync::Lazy::new(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut c = !0u32;
    for &b in data {
        c = TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

pub(crate) fn random16() -> [u8; 16] {
    *uuid::Uuid::new_v4().as_bytes()
}

/// Logical sector size of a device: what it reports (`DKIOCGETBLOCKSIZE` for disks), 512 for
/// image files and anything implausible.
pub fn sector_size(dev: &dyn BlockDev) -> u64 {
    match dev.logical_sector_size() {
        Some(s) if s.is_power_of_two() && (512..=4096).contains(&s) => s as u64,
        _ => 512,
    }
}

// ───────────────────────────── GPT structures ─────────────────────────────

/// Number of entries and entry size we write (UEFI 2.10 §5.3.2: at least 16 KiB of entries).
pub const GPT_ENTRIES: u32 = 128;
pub const GPT_ENTRY_SIZE: u32 = 128;
/// Bytes of the entry array we write.
pub const GPT_ARRAY_BYTES: u64 = GPT_ENTRIES as u64 * GPT_ENTRY_SIZE as u64;
/// GPT header revision 1.0.
pub const GPT_REVISION: u32 = 0x0001_0000;

/// A GPT header (UEFI 2.10 Table 5.5). All fields are kept so a header read from a disk can be
/// compared, fixed and written back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GptHeader {
    pub revision: u32,
    pub header_size: u32,
    pub my_lba: u64,
    pub alt_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
    pub disk_guid: [u8; 16],
    pub entries_lba: u64,
    pub num_entries: u32,
    pub entry_size: u32,
    pub entries_crc: u32,
}

/// A header as found on disk.
#[derive(Clone, Debug)]
pub struct ParsedHeader {
    pub h: GptHeader,
    /// HeaderCRC32 matches.
    pub crc_ok: bool,
    /// The fields make sense on their own (sizes, ranges); see [`GptHeader::parse`].
    pub sane: bool,
}

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

impl GptHeader {
    /// Parse the header in `b` (one logical sector). None when there is no "EFI PART"
    /// signature or the header size can't even be read. `lbas`: the disk size in sectors, used
    /// for the sanity checks of UEFI 2.10 §5.3.2 ("the GPT Header must be checked for
    /// validity": signature, HeaderSize, CRC, MyLBA, ranges).
    pub fn parse(b: &[u8], lbas: u64) -> Option<ParsedHeader> {
        if b.len() < 92 || &b[0..8] != b"EFI PART" {
            return None;
        }
        let header_size = rd32(b, 12);
        let h = GptHeader {
            revision: rd32(b, 8),
            header_size,
            my_lba: rd64(b, 24),
            alt_lba: rd64(b, 32),
            first_usable: rd64(b, 40),
            last_usable: rd64(b, 48),
            disk_guid: b[56..72].try_into().ok()?,
            entries_lba: rd64(b, 72),
            num_entries: rd32(b, 80),
            entry_size: rd32(b, 84),
            entries_crc: rd32(b, 88),
        };
        let crc_ok = (92..=b.len() as u32).contains(&header_size) && {
            let mut c = b[..header_size as usize].to_vec();
            c[16..20].fill(0);
            crc32(&c) == rd32(b, 16)
        };
        let sane = (92..=b.len() as u32).contains(&header_size)
            && h.entry_size >= 128
            && h.entry_size.is_power_of_two()
            && h.entry_size <= 4096
            && h.num_entries >= 1
            && h.num_entries <= 4096
            && h.first_usable <= h.last_usable.saturating_add(1)
            && h.last_usable < lbas
            && h.my_lba < lbas
            && h.my_lba != 0
            && h.entries_lba != 0;
        Some(ParsedHeader { h, crc_ok, sane })
    }

    /// Bytes of the entry array.
    pub fn array_bytes(&self) -> u64 {
        self.num_entries as u64 * self.entry_size as u64
    }

    /// Sectors the entry array occupies.
    pub fn array_sectors(&self, sector: u64) -> u64 {
        self.array_bytes().div_ceil(sector)
    }

    /// One logical sector holding the header, HeaderCRC32 computed (the rest zero, as
    /// required: "Reserved; must be zero for the rest of the block").
    pub fn to_bytes(&self, sector: u64) -> Vec<u8> {
        let mut h = vec![0u8; sector as usize];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&self.revision.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&self.my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&self.alt_lba.to_le_bytes());
        h[40..48].copy_from_slice(&self.first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&self.last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&self.disk_guid);
        h[72..80].copy_from_slice(&self.entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&self.num_entries.to_le_bytes());
        h[84..88].copy_from_slice(&self.entry_size.to_le_bytes());
        h[88..92].copy_from_slice(&self.entries_crc.to_le_bytes());
        let c = crc32(&h[..92]);
        h[16..20].copy_from_slice(&c.to_le_bytes());
        h
    }

    /// The other copy of this header: same table, `my`/`alt` swapped, its own entry array.
    pub fn mirrored(&self, entries_lba: u64) -> GptHeader {
        GptHeader { my_lba: self.alt_lba, alt_lba: self.my_lba, entries_lba, header_size: 92, ..self.clone() }
    }
}

/// One used GPT entry (UEFI 2.10 Table 5.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GptEntry {
    /// 0-based slot in the array.
    pub slot: u32,
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub attrs: u64,
    pub name: String,
}

/// The used entries of an entry array (all-zero type GUID = unused).
pub fn gpt_entries(array: &[u8], entry_size: u32) -> Vec<GptEntry> {
    let esz = entry_size.max(128) as usize;
    array
        .chunks_exact(esz)
        .enumerate()
        .filter(|(_, e)| e[0..16].iter().any(|&b| b != 0))
        .map(|(i, e)| {
            let units: Vec<u16> = (0..36).map(|k| u16::from_le_bytes([e[56 + 2 * k], e[57 + 2 * k]])).take_while(|&c| c != 0).collect();
            GptEntry {
                slot: i as u32,
                type_guid: e[0..16].try_into().unwrap_or([0; 16]),
                unique_guid: e[16..32].try_into().unwrap_or([0; 16]),
                first_lba: rd64(e, 32),
                last_lba: rd64(e, 40),
                attrs: rd64(e, 48),
                name: String::from_utf16_lossy(&units),
            }
        })
        .collect()
}

/// Encode a GPT partition name: at most 36 UTF-16 code units, never splitting a surrogate pair.
pub fn encode_gpt_name(name: &str) -> Vec<u16> {
    let mut out = vec![];
    for c in name.chars().filter(|c| !c.is_control()) {
        let mut buf = [0u16; 2];
        let enc = c.encode_utf16(&mut buf);
        if out.len() + enc.len() > 36 {
            break;
        }
        out.extend_from_slice(enc);
    }
    out
}

/// Serialise entries into an array of `GPT_ENTRIES` × `GPT_ENTRY_SIZE` bytes.
pub fn gpt_array(entries: &[GptEntry]) -> ForgeResult<Vec<u8>> {
    let esz = GPT_ENTRY_SIZE as usize;
    let mut a = vec![0u8; GPT_ARRAY_BYTES as usize];
    for e in entries {
        let s = e.slot as usize;
        if s >= GPT_ENTRIES as usize {
            return Err(ForgeError::Unsupported(t("GPT: al massimo 128 partizioni", "GPT: at most 128 partitions")));
        }
        let o = &mut a[s * esz..(s + 1) * esz];
        o[0..16].copy_from_slice(&e.type_guid);
        o[16..32].copy_from_slice(&e.unique_guid);
        o[32..40].copy_from_slice(&e.first_lba.to_le_bytes());
        o[40..48].copy_from_slice(&e.last_lba.to_le_bytes());
        o[48..56].copy_from_slice(&e.attrs.to_le_bytes());
        for (k, u) in encode_gpt_name(&e.name).iter().enumerate() {
            o[56 + 2 * k..58 + 2 * k].copy_from_slice(&u.to_le_bytes());
        }
    }
    Ok(a)
}

/// Where a fresh GPT of `lbas` sectors puts things: (first usable, last usable, backup entry
/// array LBA). UEFI 2.10 §5.3.2: primary header at LBA 1, its entries from LBA 2; backup header
/// at the last LBA, its entries just before it.
pub fn gpt_geometry(lbas: u64, sector: u64) -> Option<(u64, u64, u64)> {
    let es = GPT_ARRAY_BYTES.div_ceil(sector);
    let first = 2 + es;
    let backup_entries = lbas.checked_sub(1 + es)?;
    let last = backup_entries.checked_sub(1)?;
    (last >= first).then_some((first, last, backup_entries))
}

/// Write both copies of a GPT: the backup first (array, then header), then the primary
/// (array, then header), so an interrupted write leaves at least one consistent copy.
pub fn write_gpt_raw(dev: &dyn BlockDev, sector: u64, primary: &GptHeader, array: &[u8]) -> ForgeResult<()> {
    let backup = primary.mirrored(primary.alt_lba.checked_sub(primary.array_sectors(sector)).ok_or_else(|| ForgeError::Invalid("GPT: bad backup location".into()))?);
    let at = |lba: u64| lba.checked_mul(sector).ok_or_else(|| ForgeError::Invalid("GPT: LBA overflow".into()));
    dev.write_at(at(backup.entries_lba)?, array)?;
    dev.write_at(at(backup.my_lba)?, &backup.to_bytes(sector))?;
    dev.write_at(at(primary.entries_lba)?, array)?;
    dev.write_at(at(primary.my_lba)?, &primary.to_bytes(sector))?;
    Ok(())
}

// ───────────────────────────── MBR structures ─────────────────────────────

/// One MBR partition record (UEFI 2.10 Table 5.2 / the IBM PC convention).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MbrEntry {
    pub boot: u8,
    pub kind: u8,
    pub start: u32,
    pub count: u32,
}

impl MbrEntry {
    pub fn is_empty(&self) -> bool {
        self.kind == 0 || self.count == 0
    }
}

/// Read the 4 primary records of a sector-0 image.
pub fn mbr_entries(mbr: &[u8]) -> [MbrEntry; 4] {
    let mut out = [MbrEntry::default(); 4];
    for (i, e) in out.iter_mut().enumerate() {
        let o = 446 + i * 16;
        *e = MbrEntry { boot: mbr[o], kind: mbr[o + 4], start: rd32(mbr, o + 8), count: rd32(mbr, o + 12) };
    }
    out
}

/// CHS address of an LBA for the legacy fields (255 heads × 63 sectors geometry). Beyond the
/// CHS range: the conventional maximum (1023/254/63), as the UEFI spec requires for 0xEE.
pub fn chs(lba: u64) -> [u8; 3] {
    if lba >= 1024 * 255 * 63 {
        return [0xFE, 0xFF, 0xFF];
    }
    let c = lba / (255 * 63);
    let h = (lba / 63) % 255;
    let s = lba % 63 + 1;
    [h as u8, (((c >> 2) & 0xC0) as u8) | s as u8, c as u8]
}

/// Build sector 0: `code` (first 440 bytes kept, e.g. boot code), disk signature, 4 records,
/// 0x55AA. Bytes 444..446 are zero.
pub fn build_mbr(code: &[u8], disk_sig: [u8; 4], entries: &[MbrEntry]) -> Vec<u8> {
    let mut mbr = vec![0u8; 512];
    let n = code.len().min(440);
    mbr[..n].copy_from_slice(&code[..n]);
    mbr[440..444].copy_from_slice(&disk_sig);
    for (i, e) in entries.iter().take(4).enumerate() {
        if e.is_empty() {
            continue;
        }
        let o = 446 + i * 16;
        let end = (e.start as u64 + e.count as u64).saturating_sub(1);
        mbr[o] = e.boot;
        mbr[o + 1..o + 4].copy_from_slice(&chs(e.start as u64));
        mbr[o + 4] = e.kind;
        mbr[o + 5..o + 8].copy_from_slice(&chs(end));
        mbr[o + 8..o + 12].copy_from_slice(&e.start.to_le_bytes());
        mbr[o + 12..o + 16].copy_from_slice(&e.count.to_le_bytes());
    }
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    mbr
}

/// The protective record of UEFI 2.10 Table 5.4: type 0xEE from LBA 1 over the whole disk
/// (0xFFFFFFFF when it is larger than that).
pub fn protective_entry(lbas: u64) -> MbrEntry {
    MbrEntry { boot: 0, kind: 0xEE, start: 1, count: lbas.saturating_sub(1).min(u32::MAX as u64) as u32 }
}

// ───────────────────────────── APM structures ─────────────────────────────

/// Blocks reserved for the partition map (Apple's tools use 63: blocks 1..=63), so user
/// partitions start at block 64 at the earliest.
pub const APM_MAP_BLOCKS: u64 = 63;

/// One APM partition map entry (Inside Macintosh: Devices, "Partition Map Entry").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApmEntry {
    pub start: u32,
    pub count: u32,
    pub name: String,
    pub kind: String,
    pub status: u32,
}

/// A parsed Apple Partition Map.
#[derive(Clone, Debug)]
pub struct Apm {
    /// sbBlkSize of the Driver Descriptor Record.
    pub block_size: u32,
    /// sbBlkCount.
    pub block_count: u32,
    /// Every entry, in map order (block 1, 2, …), including the map itself and free space.
    pub entries: Vec<ApmEntry>,
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    b[..end].iter().map(|&c| if c.is_ascii() && !c.is_ascii_control() { c as char } else { '?' }).collect()
}

/// Read an Apple Partition Map, if the device has one ("ER" driver descriptor in block 0 and
/// "PM" entries from block 1). Every count is validated: the map comes from an untrusted disk.
pub fn parse_apm(dev: &dyn BlockDev) -> std::io::Result<Option<Apm>> {
    if dev.size() < 1024 {
        return Ok(None);
    }
    let b0 = dev.read_vec(0, 512)?;
    if &b0[0..2] != b"ER" {
        return Ok(None);
    }
    let block_size = be16(&b0, 2) as u32;
    // Apple's partition maps are found at 512-byte blocks even when sbBlkSize is larger only
    // on CD-ROMs; we accept the declared size if it is a sane power of two.
    if !block_size.is_power_of_two() || !(512..=4096).contains(&block_size) {
        return Ok(None);
    }
    let bs = block_size as u64;
    let block_count = be32(&b0, 4);
    let first = dev.read_vec(bs, 512.min(dev.size().saturating_sub(bs)) as usize)?;
    if first.len() < 512 || &first[0..2] != b"PM" {
        return Ok(None);
    }
    let n = be32(&first, 4) as u64;
    if n == 0 || n > 256 || (n + 1).checked_mul(bs).map_or(true, |e| e > dev.size()) {
        return Ok(None);
    }
    let mut entries = vec![];
    for i in 1..=n {
        let e = dev.read_vec(i * bs, 512)?;
        if &e[0..2] != b"PM" {
            break;
        }
        entries.push(ApmEntry { start: be32(&e, 8), count: be32(&e, 12), name: cstr(&e[16..48]), kind: cstr(&e[48..80]), status: be32(&e, 88) });
    }
    Ok(Some(Apm { block_size, block_count, entries }))
}

fn apm_block(map_count: u32, e: &ApmEntry, block: usize) -> Vec<u8> {
    let mut b = vec![0u8; block];
    b[0..2].copy_from_slice(b"PM"); // pmSig
    b[4..8].copy_from_slice(&map_count.to_be_bytes()); // pmMapBlkCnt
    b[8..12].copy_from_slice(&e.start.to_be_bytes()); // pmPyPartStart
    b[12..16].copy_from_slice(&e.count.to_be_bytes()); // pmPartBlkCnt
    let name: Vec<u8> = e.name.bytes().filter(|c| c.is_ascii_graphic() || *c == b' ').take(31).collect();
    b[16..16 + name.len()].copy_from_slice(&name); // pmPartName
    let kind: Vec<u8> = e.kind.bytes().take(31).collect();
    b[48..48 + kind.len()].copy_from_slice(&kind); // pmParType
    // pmLgDataStart = 0, pmDataCnt = the whole partition (no Apple driver data area).
    let data = if e.kind == "Apple_Free" { 0 } else { e.count };
    b[84..88].copy_from_slice(&data.to_be_bytes());
    b[88..92].copy_from_slice(&e.status.to_be_bytes()); // pmPartStatus
    b
}

// ───────────────────────────── placement and writing ─────────────────────────────

/// Place `parts` on a disk of `size` bytes with `sector`-byte sectors. Returns (start, size)
/// for each, or an error if they don't fit.
pub fn place(size: u64, sector: u64, scheme: Scheme, parts: &[NewPart], align: u64) -> ForgeResult<Vec<Placed>> {
    let no_fit = || ForgeError::Invalid(t("le partizioni non entrano nel disco", "the partitions do not fit on the disk"));
    if parts.is_empty() {
        return Err(ForgeError::Invalid(t("nessuna partizione richiesta", "no partition requested")));
    }
    let size = size / sector * sector;
    let (first, last_end) = match scheme {
        // Primary GPT: MBR + header + entry array; backup array + header at the end.
        Scheme::Gpt => (sector * 2 + GPT_ARRAY_BYTES.div_ceil(sector) * sector, size.saturating_sub(GPT_ARRAY_BYTES.div_ceil(sector) * sector + sector)),
        Scheme::Mbr => (sector, size),
        Scheme::Apm => (sector * (APM_MAP_BLOCKS + 1), size),
        Scheme::None => (0, size),
    };
    let align = if scheme == Scheme::None { 1 } else { align.max(sector) };
    if scheme != Scheme::None && align % sector != 0 {
        return Err(ForgeError::Invalid("alignment must be a multiple of the sector size".into()));
    }
    let mut out = vec![];
    let mut pos = first;
    for (i, p) in parts.iter().enumerate() {
        let start = pos.div_ceil(align).checked_mul(align).ok_or_else(no_fit)?;
        let size_p = match p.size {
            Some(s) => s / sector * sector,
            None if i == parts.len() - 1 => last_end.saturating_sub(start) / sector * sector,
            None => return Err(ForgeError::Invalid(t("solo l'ultima partizione può occupare il resto del disco", "only the last partition can take the rest of the disk"))),
        };
        let end = start.checked_add(size_p).ok_or_else(no_fit)?;
        if size_p == 0 || end > last_end {
            return Err(no_fit());
        }
        out.push(Placed { index: i as u32 + 1, start, size: size_p });
        pos = end;
    }
    match scheme {
        Scheme::Mbr if out.len() > 4 => return Err(ForgeError::Unsupported(t("MBR: al massimo 4 partizioni (usa GPT per averne di più)", "MBR: at most 4 partitions (use GPT for more)"))),
        Scheme::Gpt if out.len() > GPT_ENTRIES as usize => return Err(ForgeError::Unsupported(t("GPT: al massimo 128 partizioni", "GPT: at most 128 partitions"))),
        // The map needs one entry for itself, one per partition and one per free gap.
        Scheme::Apm if 2 * out.len() + 2 > APM_MAP_BLOCKS as usize => return Err(ForgeError::Unsupported(t("APM: troppe partizioni", "APM: too many partitions"))),
        Scheme::None if out.len() != 1 => return Err(ForgeError::Invalid(t("senza tabella delle partizioni si può creare un solo filesystem", "without a partition table there can be exactly one filesystem"))),
        _ => {}
    }
    // MBR records hold 32-bit LBAs: nothing may end beyond 2^32 sectors (2 TiB with 512-byte
    // sectors, 16 TiB on 4Kn disks).
    if scheme == Scheme::Mbr && out.iter().any(|p| (p.start + p.size) / sector > u32::MAX as u64) {
        return Err(ForgeError::Unsupported(t(
            "MBR non può indirizzare oltre 2 TiB (con settori da 512 byte): usa GPT",
            "MBR can't address beyond 2 TiB (with 512-byte sectors): use GPT",
        )));
    }
    if scheme == Scheme::Apm && size / sector > u32::MAX as u64 {
        return Err(ForgeError::Unsupported(t("APM non può indirizzare dischi oltre 2 TiB: usa GPT", "APM can't address disks over 2 TiB: use GPT")));
    }
    Ok(out)
}

/// Write the partition table. Clears the areas a previous table could have left (both GPT
/// headers, an APM map) so no stale table survives a scheme change.
pub fn write_table(dev: &dyn BlockDev, scheme: Scheme, parts: &[NewPart], opts: &TableOptions) -> ForgeResult<Vec<Placed>> {
    let sector = sector_size(dev);
    let size = dev.size() / sector * sector;
    let placed = place(size, sector, scheme, parts, opts.align.unwrap_or(1 << 20))?;
    if !opts.hybrid.is_empty() && scheme != Scheme::Gpt {
        return Err(ForgeError::Invalid("hybrid MBR: GPT only".into()));
    }
    // Stale GPT headers / APM blocks at both ends (at least the whole APM map area).
    let clear = (64 * 1024u64).max(sector * (APM_MAP_BLOCKS + 1)).min(size);
    let zero = vec![0u8; clear as usize];
    dev.write_at(0, &zero)?;
    dev.write_at(size - clear, &zero)?;
    match scheme {
        Scheme::None => {}
        Scheme::Mbr => {
            let entries: Vec<MbrEntry> = parts
                .iter()
                .zip(&placed)
                .map(|(p, pl)| MbrEntry { boot: if p.bootable { 0x80 } else { 0 }, kind: mbr_type(p.kind), start: (pl.start / sector) as u32, count: (pl.size / sector) as u32 })
                .collect();
            write_mbr_sector(dev, opts, &entries)?;
        }
        Scheme::Gpt => {
            write_gpt(dev, sector, size, parts, &placed, opts)?;
            write_mbr_sector(dev, opts, &protective_or_hybrid(sector, size, parts, &placed, &opts.hybrid)?)?;
        }
        Scheme::Apm => write_apm(dev, sector, size, parts, &placed)?,
    }
    dev.flush()?;
    Ok(placed)
}

fn write_mbr_sector(dev: &dyn BlockDev, opts: &TableOptions, entries: &[MbrEntry]) -> ForgeResult<()> {
    let id = opts.disk_id.unwrap_or_else(random16);
    let code = opts.mbr_boot_code.clone().unwrap_or_default();
    dev.write_at(0, &build_mbr(&code, [id[0], id[1], id[2], id[3]], entries))?;
    Ok(())
}

/// The MBR of a GPT disk: protective (UEFI 2.10 §5.2.3), or hybrid — 0xEE over the GPT
/// structures up to the first mirrored partition, then up to 3 mirrored partitions. Hybrid
/// MBRs are outside the UEFI spec; they exist for BIOS-only machines (what `gdisk` writes).
fn protective_or_hybrid(sector: u64, size: u64, parts: &[NewPart], placed: &[Placed], hybrid: &[u32]) -> ForgeResult<Vec<MbrEntry>> {
    let lbas = size / sector;
    if hybrid.is_empty() {
        return Ok(vec![protective_entry(lbas)]);
    }
    if hybrid.len() > 3 {
        return Err(ForgeError::Invalid(t("MBR ibrido: al massimo 3 partizioni", "hybrid MBR: at most 3 partitions")));
    }
    let mut mirrored = vec![];
    for &i in hybrid {
        let k = (i as usize).checked_sub(1).filter(|&k| k < placed.len()).ok_or_else(|| ForgeError::Invalid(format!("hybrid MBR: no partition {i}")))?;
        let (p, pl) = (&parts[k], placed[k]);
        let (start, count) = (pl.start / sector, pl.size / sector);
        if start + count > u32::MAX as u64 {
            return Err(ForgeError::Unsupported(t("MBR ibrido: la partizione va oltre i 2 TiB", "hybrid MBR: the partition goes beyond 2 TiB")));
        }
        if mirrored.iter().any(|m: &MbrEntry| m.start as u64 == start) {
            return Err(ForgeError::Invalid(format!("hybrid MBR: partition {i} listed twice")));
        }
        mirrored.push(MbrEntry { boot: if p.bootable { 0x80 } else { 0 }, kind: mbr_type(p.kind), start: start as u32, count: count as u32 });
    }
    mirrored.sort_by_key(|m| m.start);
    let first = mirrored[0].start;
    let mut out = vec![MbrEntry { boot: 0, kind: 0xEE, start: 1, count: first - 1 }];
    out.extend(mirrored);
    Ok(out)
}

fn write_gpt(dev: &dyn BlockDev, sector: u64, size: u64, parts: &[NewPart], placed: &[Placed], opts: &TableOptions) -> ForgeResult<()> {
    let lbas = size / sector;
    let (first_usable, last_usable, backup_entries) = gpt_geometry(lbas, sector).ok_or_else(|| ForgeError::Invalid(t("disco troppo piccolo per GPT", "disk too small for GPT")))?;
    let entries: Vec<GptEntry> = parts
        .iter()
        .zip(placed)
        .enumerate()
        .map(|(i, (p, pl))| GptEntry {
            slot: i as u32,
            type_guid: gpt_type(p.kind),
            unique_guid: random16(),
            first_lba: pl.start / sector,
            last_lba: (pl.start + pl.size) / sector - 1,
            // Bit 2: legacy BIOS bootable (UEFI 2.10 Table 5.8).
            attrs: if p.bootable { 1 << 2 } else { 0 },
            name: p.name.clone(),
        })
        .collect();
    let array = gpt_array(&entries)?;
    let primary = GptHeader {
        revision: GPT_REVISION,
        header_size: 92,
        my_lba: 1,
        alt_lba: lbas - 1,
        first_usable,
        last_usable,
        disk_guid: opts.disk_id.unwrap_or_else(random16),
        entries_lba: 2,
        num_entries: GPT_ENTRIES,
        entry_size: GPT_ENTRY_SIZE,
        entries_crc: crc32(&array),
    };
    debug_assert_eq!(primary.alt_lba - primary.array_sectors(sector), backup_entries);
    write_gpt_raw(dev, sector, &primary, &array)
}

/// Apple Partition Map: block 0 the Driver Descriptor Record, blocks 1..=63 the map (the
/// map's own entry, the partitions and `Apple_Free` entries for every gap, in disk order).
fn write_apm(dev: &dyn BlockDev, block: u64, size: u64, parts: &[NewPart], placed: &[Placed]) -> ForgeResult<()> {
    let blocks = size / block;
    if blocks > u32::MAX as u64 {
        return Err(ForgeError::Unsupported(t("APM non può indirizzare dischi oltre 2 TiB: usa GPT", "APM can't address disks over 2 TiB: use GPT")));
    }
    // pmPartStatus bits (Inside Macintosh): 0 valid, 1 allocated, 2 in use, 4 readable,
    // 5 writable; 0x40000000 is what Apple's tools set on data partitions as well.
    let free = |start: u64, end: u64| ApmEntry { start: start as u32, count: (end - start) as u32, name: "Extra".into(), kind: "Apple_Free".into(), status: 0 };
    let mut map = vec![ApmEntry { start: 1, count: APM_MAP_BLOCKS as u32, name: "Apple".into(), kind: "Apple_partition_map".into(), status: 0x3 }];
    let mut pos = APM_MAP_BLOCKS + 1;
    for (p, pl) in parts.iter().zip(placed) {
        let (s, n) = (pl.start / block, pl.size / block);
        if s > pos {
            map.push(free(pos, s));
        }
        let name = if p.name.trim().is_empty() { "Untitled".to_string() } else { p.name.clone() };
        map.push(ApmEntry { start: s as u32, count: n as u32, name, kind: apm_type(p.kind).into(), status: 0x4000_0033 });
        pos = s + n;
    }
    if blocks > pos {
        map.push(free(pos, blocks));
    }
    if map.len() as u64 > APM_MAP_BLOCKS {
        return Err(ForgeError::Unsupported(t("APM: troppe partizioni", "APM: too many partitions")));
    }
    // Driver Descriptor Record: sbSig 'ER', sbBlkSize, sbBlkCount; no drivers (sbDrvrCount 0).
    let mut ddr = vec![0u8; block as usize];
    ddr[0..2].copy_from_slice(b"ER");
    ddr[2..4].copy_from_slice(&(block as u16).to_be_bytes());
    ddr[4..8].copy_from_slice(&(blocks as u32).to_be_bytes());
    let mut area = vec![0u8; ((APM_MAP_BLOCKS + 1) * block) as usize];
    area[..block as usize].copy_from_slice(&ddr);
    let n = map.len() as u32;
    for (i, e) in map.iter().enumerate() {
        let o = (i + 1) * block as usize;
        area[o..o + block as usize].copy_from_slice(&apm_block(n, e, block as usize));
    }
    dev.write_at(0, &area)?;
    Ok(())
}

// ───────────────────────────── reading back ─────────────────────────────

/// A partition as read back from any of the three schemes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TablePart {
    pub index: u32,
    pub start: u64,
    pub size: u64,
    /// GPT type GUID, "0xNN" MBR type or APM type string.
    pub kind: String,
    pub name: String,
}

/// Which table a disk carries and its partitions (GPT / MBR through
/// [`crate::block::parse_partitions`], APM through [`parse_apm`]).
pub fn read_table(dev: &dyn BlockDev) -> ForgeResult<(Scheme, Vec<TablePart>)> {
    if let Some(apm) = parse_apm(dev)? {
        let bs = apm.block_size as u64;
        let parts = apm
            .entries
            .iter()
            .filter(|e| e.kind != "Apple_partition_map" && e.kind != "Apple_Free")
            .enumerate()
            .map(|(i, e)| TablePart { index: i as u32 + 1, start: e.start as u64 * bs, size: e.count as u64 * bs, kind: e.kind.clone(), name: e.name.clone() })
            .collect();
        return Ok((Scheme::Apm, parts));
    }
    let parts = crate::block::parse_partitions(dev)?;
    let scheme = match parts.first().map(|p| p.scheme.as_str()) {
        Some("GPT") => Scheme::Gpt,
        Some(_) => Scheme::Mbr,
        None => {
            // An empty but valid GPT / MBR is still a table.
            let b = dev.read_vec(0, 512.min(dev.size()) as usize)?;
            if b.len() == 512 && b[510] == 0x55 && b[511] == 0xAA && mbr_entries(&b).iter().any(|e| e.kind == 0xEE) {
                Scheme::Gpt
            } else {
                Scheme::None
            }
        }
    };
    let parts = parts.into_iter().map(|p| TablePart { index: p.index, start: p.start, size: p.size, kind: p.type_id, name: p.name }).collect();
    Ok((scheme, parts))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::block::{parse_partitions, MemDev};
    use std::sync::Arc;

    /// A RAM disk that reports a logical sector size (4Kn tests).
    pub(crate) struct SectorDev {
        pub mem: MemDev,
        pub sector: u32,
    }
    impl SectorDev {
        pub(crate) fn new(size: usize, sector: u32) -> Self {
            SectorDev { mem: MemDev::new(size), sector }
        }
    }
    impl BlockDev for SectorDev {
        fn read_at(&self, o: u64, b: &mut [u8]) -> std::io::Result<()> {
            self.mem.read_at(o, b)
        }
        fn write_at(&self, o: u64, b: &[u8]) -> std::io::Result<()> {
            self.mem.write_at(o, b)
        }
        fn size(&self) -> u64 {
            self.mem.size()
        }
        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn read_only(&self) -> bool {
            false
        }
        fn logical_sector_size(&self) -> Option<u32> {
            Some(self.sector)
        }
    }

    /// Tiny deterministic PRNG (xorshift64*) for the property tests: reproducible from a seed.
    pub(crate) struct Rng(pub u64);
    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        pub(crate) fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    fn p(size: Option<u64>, fs: TargetFs) -> NewPart {
        NewPart { size, kind: PartType::Fs(fs), name: "data".into(), bootable: false }
    }

    #[test]
    fn known_crc_and_guid_encoding() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let g = guid("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(&g[..4], &[0x28, 0x73, 0x2A, 0xC1]);
        assert_eq!(&g[8..], &[0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
        assert_eq!(guid_string(&g), "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert!(parse_guid("C12A7328-F81F-11D2-BA4B").is_none());
        assert!(parse_guid("Z12A7328-F81F-11D2-BA4B-00A0C93EC93B").is_none());
        assert!(parse_guid("C12A7328-F81F-11D2-BA4B-00A0C93EC9é").is_none());
    }

    #[test]
    fn gpt_and_mbr_tables_are_read_back_by_the_partition_parser() {
        for scheme in [Scheme::Gpt, Scheme::Mbr] {
            let dev = MemDev::new(64 << 20);
            let parts = [NewPart { size: Some(8 << 20), kind: PartType::EfiSystem, name: "EFI".into(), bootable: true }, p(None, TargetFs::Ext4)];
            let placed = write_table(&dev, scheme, &parts, &TableOptions::default()).unwrap();
            assert_eq!(placed[0].start, 1 << 20, "1 MiB aligned");
            assert_eq!(placed[1].start, 9 << 20);
            let got = parse_partitions(&dev).unwrap();
            assert_eq!(got.len(), 2, "{scheme:?}");
            assert_eq!((got[0].start, got[0].size), (placed[0].start, placed[0].size));
            assert_eq!((got[1].start, got[1].size), (placed[1].start, placed[1].size));
            if scheme == Scheme::Gpt {
                assert_eq!(got[0].name, "EFI");
                assert!(placed[1].start + placed[1].size <= (64 << 20) - 16384 - 512, "backup GPT left intact");
            }
            assert_eq!(read_table(&dev).unwrap().0, scheme);
        }
    }

    #[test]
    fn impossible_layouts_are_refused() {
        assert!(place(10 << 20, 512, Scheme::Mbr, &[p(Some(20 << 20), TargetFs::Fat32)], 1 << 20).is_err());
        assert!(place(64 << 20, 512, Scheme::Gpt, &[p(None, TargetFs::Fat32), p(None, TargetFs::Ext4)], 1 << 20).is_err());
        let five: Vec<_> = (0..5).map(|_| p(Some(1 << 20), TargetFs::Fat32)).collect();
        assert!(place(64 << 20, 512, Scheme::Mbr, &five, 1 << 20).is_err());
        assert!(place(64 << 20, 512, Scheme::Gpt, &[], 1 << 20).is_err());
        assert!(place(64 << 20, 512, Scheme::None, &[p(Some(1 << 20), TargetFs::Fat32), p(None, TargetFs::Fat32)], 1 << 20).is_err());
        assert!(place(64 << 20, 512, Scheme::Gpt, &[p(Some(u64::MAX), TargetFs::Fat32)], 1 << 20).is_err(), "no overflow");
        assert!(place(64 << 20, 512, Scheme::Gpt, &[p(Some(100), TargetFs::Fat32)], 1 << 20).is_err(), "zero-sector partition");
        let many: Vec<_> = (0..40).map(|_| p(Some(1 << 20), TargetFs::Fat32)).collect();
        assert!(place(64 << 20, 512, Scheme::Apm, &many, 1 << 20).is_err());
        // MBR: 3 TiB disk, the rest-of-disk partition crosses 2 TiB; a 1 TiB one is fine.
        assert!(place(3 << 40, 512, Scheme::Mbr, &[p(None, TargetFs::Exfat)], 1 << 20).is_err());
        assert!(place(3 << 40, 512, Scheme::Mbr, &[p(Some(1 << 40), TargetFs::Exfat)], 1 << 20).is_ok());
        // …and on a 4Kn disk MBR reaches 16 TiB.
        assert!(place(3 << 40, 4096, Scheme::Mbr, &[p(None, TargetFs::Exfat)], 1 << 20).is_ok());
        assert!(place(3 << 40, 512, Scheme::Apm, &[p(None, TargetFs::Exfat)], 1 << 20).is_err());
    }

    #[test]
    fn superfloppy_is_one_filesystem_from_byte_zero() {
        let dev = MemDev::new(16 << 20);
        dev.write_at(0, &[0xAB; 4096]).unwrap();
        let placed = write_table(&dev, Scheme::None, &[p(None, TargetFs::Fat16)], &TableOptions::default()).unwrap();
        assert_eq!(placed, vec![Placed { index: 1, start: 0, size: 16 << 20 }]);
        assert!(dev.snapshot()[..65536].iter().all(|&b| b == 0), "old signatures cleared");
        assert_eq!(read_table(&dev).unwrap(), (Scheme::None, vec![]));
    }

    #[test]
    fn gpt_on_4kn_uses_4096_byte_lbas_and_4_entries_per_sector() {
        let dev = SectorDev::new(64 << 20, 4096);
        assert_eq!(sector_size(&dev), 4096);
        let parts = [p(Some(8 << 20), TargetFs::Fat32), p(None, TargetFs::Ext4)];
        let placed = write_table(&dev, Scheme::Gpt, &parts, &TableOptions::default()).unwrap();
        assert_eq!(placed[0].start, 1 << 20);
        let img = dev.mem.snapshot();
        assert_eq!(&img[4096..4104], b"EFI PART", "header at LBA 1 = byte 4096");
        let lbas = (64u64 << 20) / 4096;
        let h = GptHeader::parse(&img[4096..8192], lbas).unwrap();
        assert!(h.crc_ok && h.sane);
        assert_eq!((h.h.entries_lba, h.h.first_usable, h.h.alt_lba, h.h.last_usable), (2, 6, lbas - 1, lbas - 6));
        let b = GptHeader::parse(&img[((lbas - 1) * 4096) as usize..], lbas).unwrap();
        assert!(b.crc_ok);
        assert_eq!(b.h.entries_lba, lbas - 5);
        let mbr = mbr_entries(&img[..512]);
        assert_eq!((mbr[0].kind, mbr[0].start, mbr[0].count), (0xEE, 1, (lbas - 1) as u32));
        let got = parse_partitions(&dev).unwrap();
        assert_eq!(got.iter().map(|g| (g.start, g.size)).collect::<Vec<_>>(), placed.iter().map(|p| (p.start, p.size)).collect::<Vec<_>>());
        // MBR on 4Kn: LBAs in 4096-byte units, read back with the device's sector size.
        let dev = SectorDev::new(64 << 20, 4096);
        let placed = write_table(&dev, Scheme::Mbr, &parts, &TableOptions::default()).unwrap();
        let mbr = mbr_entries(&dev.mem.snapshot()[..512]);
        assert_eq!(mbr[0].start as u64 * 4096, placed[0].start);
        let got = parse_partitions(&dev).unwrap();
        assert_eq!((got[1].start, got[1].size), (placed[1].start, placed[1].size));
    }

    #[test]
    fn gpt_names_keep_36_utf16_units_without_splitting_surrogates() {
        assert_eq!(encode_gpt_name("Dati é").len(), 6);
        let long: String = "a".repeat(35) + "😀";
        let e = encode_gpt_name(&long);
        assert_eq!(e.len(), 35, "the emoji needs 2 units and only 1 is left");
        let dev = MemDev::new(32 << 20);
        let parts = [NewPart { size: None, kind: PartType::Fs(TargetFs::Exfat), name: "Archivio è😀".into(), bootable: false }];
        write_table(&dev, Scheme::Gpt, &parts, &TableOptions::default()).unwrap();
        assert_eq!(parse_partitions(&dev).unwrap()[0].name, "Archivio è😀");
    }

    #[test]
    fn apm_map_has_ddr_map_entry_partitions_and_free_space() {
        let dev = MemDev::new(64 << 20);
        let parts = [p(Some(16 << 20), TargetFs::Fat32), NewPart { size: None, kind: PartType::Fs(TargetFs::Exfat), name: "Dati".into(), bootable: false }];
        let placed = write_table(&dev, Scheme::Apm, &parts, &TableOptions::default()).unwrap();
        assert_eq!(placed[0].start, 1 << 20);
        let apm = parse_apm(&dev).unwrap().unwrap();
        assert_eq!((apm.block_size, apm.block_count), (512, (64 << 20) / 512));
        let kinds: Vec<&str> = apm.entries.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["Apple_partition_map", "Apple_Free", "DOS_FAT_32", "Windows_NTFS"]);
        assert_eq!((apm.entries[0].start, apm.entries[0].count), (1, 63));
        assert_eq!((apm.entries[1].start, apm.entries[1].count), (64, 2048 - 64));
        assert_eq!(apm.entries[3].name, "Dati");
        // Entries tile the disk from block 1 to the end, without overlaps.
        let mut pos = 1u64;
        for e in &apm.entries {
            assert_eq!(e.start as u64, pos);
            pos += e.count as u64;
        }
        assert_eq!(pos, (64 << 20) / 512);
        let (scheme, tp) = read_table(&dev).unwrap();
        assert_eq!(scheme, Scheme::Apm);
        assert_eq!(tp.iter().map(|t| (t.start, t.size)).collect::<Vec<_>>(), placed.iter().map(|p| (p.start, p.size)).collect::<Vec<_>>());
        // A gap between partitions and at the end become Apple_Free too.
        let dev = MemDev::new(64 << 20);
        let parts = [p(Some((16 << 20) + 4096), TargetFs::Fat32), p(Some(8 << 20), TargetFs::Ext4)];
        write_table(&dev, Scheme::Apm, &parts, &TableOptions::default()).unwrap();
        let apm = parse_apm(&dev).unwrap().unwrap();
        let kinds: Vec<&str> = apm.entries.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["Apple_partition_map", "Apple_Free", "DOS_FAT_32", "Apple_Free", "Linux", "Apple_Free"]);
        // A later GPT erases the APM completely.
        write_table(&dev, Scheme::Gpt, &parts, &TableOptions::default()).unwrap();
        assert!(parse_apm(&dev).unwrap().is_none());
        assert_eq!(read_table(&dev).unwrap().0, Scheme::Gpt);
    }

    #[test]
    fn apm_parser_survives_garbage() {
        let mut r = Rng(0x5eed);
        for _ in 0..300 {
            let dev = MemDev::new(256 << 10);
            let mut junk = vec![0u8; 8192];
            for b in junk.iter_mut() {
                *b = r.next() as u8;
            }
            junk[0..2].copy_from_slice(b"ER");
            junk[2..4].copy_from_slice(&[2, 0]);
            junk[512..514].copy_from_slice(b"PM");
            dev.write_at(0, &junk).unwrap();
            let _ = parse_apm(&dev);
            let _ = read_table(&dev);
        }
    }

    #[test]
    fn hybrid_mbr_mirrors_chosen_partitions_after_a_partial_0xee() {
        let dev = MemDev::new(64 << 20);
        let parts = [NewPart { size: Some(8 << 20), kind: PartType::EfiSystem, name: "EFI".into(), bootable: false }, NewPart { size: None, kind: PartType::Fs(TargetFs::Fat32), name: "D".into(), bootable: true }];
        let opts = TableOptions { hybrid: vec![2], ..Default::default() };
        let placed = write_table(&dev, Scheme::Gpt, &parts, &opts).unwrap();
        let m = mbr_entries(&dev.snapshot()[..512]);
        assert_eq!((m[0].kind, m[0].start, m[0].count), (0xEE, 1, (placed[1].start / 512 - 1) as u32));
        assert_eq!((m[1].kind, m[1].boot, m[1].start as u64 * 512), (0x0C, 0x80, placed[1].start));
        assert_eq!(parse_partitions(&dev).unwrap().len(), 2, "GPT still wins");
        for bad in [vec![3], vec![1, 1], vec![1, 2, 1, 2]] {
            let opts = TableOptions { hybrid: bad, ..Default::default() };
            assert!(write_table(&dev, Scheme::Gpt, &parts, &opts).is_err());
        }
        assert!(write_table(&dev, Scheme::Mbr, &parts, &TableOptions { hybrid: vec![1], ..Default::default() }).is_err());
    }

    /// Property: random layouts on random disk sizes / sector sizes round-trip through
    /// write_table → read back, are aligned, in order, non-overlapping and inside the usable area.
    #[test]
    fn random_layouts_round_trip() {
        let mut r = Rng(0xC0FFEE);
        let all = [TargetFs::Fat16, TargetFs::Fat32, TargetFs::Exfat, TargetFs::Ntfs, TargetFs::Ext4, TargetFs::Ufs2];
        for round in 0..200 {
            let sector = if r.below(4) == 0 { 4096 } else { 512 };
            let scheme = [Scheme::Gpt, Scheme::Mbr, Scheme::Apm][r.below(3) as usize];
            let mb = 8 + r.below(120) as usize;
            let n = 1 + r.below(if scheme == Scheme::Mbr { 4 } else { 7 }) as usize;
            let mut parts = vec![];
            for i in 0..n {
                let size = if i == n - 1 && r.below(2) == 0 { None } else { Some((1 + r.below(16)) << 20 | r.below(1 << 19)) };
                let kind = if r.below(5) == 0 { PartType::EfiSystem } else { PartType::Fs(all[r.below(all.len() as u64) as usize]) };
                parts.push(NewPart { size, kind, name: format!("p{i}"), bootable: r.below(3) == 0 });
            }
            let dev: Arc<dyn BlockDev> = Arc::new(SectorDev::new(mb << 20, sector));
            let placed = match write_table(dev.as_ref(), scheme, &parts, &TableOptions::default()) {
                Ok(p) => p,
                Err(_) => {
                    assert!(place(dev.size(), sector as u64, scheme, &parts, 1 << 20).is_err(), "round {round}");
                    continue;
                }
            };
            let (got_scheme, got) = read_table(dev.as_ref()).unwrap();
            assert_eq!(got_scheme, scheme, "round {round}");
            assert_eq!(got.iter().map(|g| (g.start, g.size)).collect::<Vec<_>>(), placed.iter().map(|p| (p.start, p.size)).collect::<Vec<_>>(), "round {round} {scheme:?} {sector}");
            for w in placed.windows(2) {
                assert!(w[0].start + w[0].size <= w[1].start);
            }
            for pl in &placed {
                assert_eq!(pl.start % (1 << 20), 0);
                assert_eq!(pl.size % sector as u64, 0);
            }
            if scheme == Scheme::Gpt {
                let img = dev.read_vec(0, dev.size() as usize).unwrap();
                let lbas = dev.size() / sector as u64;
                let h = GptHeader::parse(&img[sector as usize..2 * sector as usize], lbas).unwrap();
                assert!(h.crc_ok && h.sane);
                for e in gpt_entries(&img[(2 * sector) as usize..(2 * sector) as usize + GPT_ARRAY_BYTES as usize], 128) {
                    assert!(e.first_lba >= h.h.first_usable && e.last_lba <= h.h.last_usable);
                }
            }
            if scheme == Scheme::Mbr {
                let parsed = parse_partitions(dev.as_ref()).unwrap();
                let m = mbr_entries(&dev.read_vec(0, 512).unwrap());
                for (k, pt) in parts.iter().enumerate() {
                    assert_eq!(m[k].boot == 0x80, pt.bootable);
                    assert_eq!(parsed[k].type_id, format!("0x{:02X}", mbr_type(pt.kind)));
                }
            }
        }
    }
}
