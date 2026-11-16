//! Writing partition tables: GPT (with protective MBR and backup), MBR, APM.
//!
//! [`write_table`] places partitions 1 MiB-aligned (APM / tiny disks excepted), writes the
//! table(s), and returns where each partition landed. It never touches partition contents
//! beyond the table areas; [`super::format`] formats them afterwards.

use serde::{Deserialize, Serialize};

use super::plan::Scheme;
use super::{ForgeError, ForgeResult, TargetFs};
use crate::block::BlockDev;

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
}

/// Microsoft basic data, Linux filesystem, EFI system… as mixed-endian GUID bytes.
pub fn gpt_type(kind: PartType) -> [u8; 16] {
    match kind {
        PartType::EfiSystem => guid("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
        PartType::Raw { gpt, .. } => gpt,
        PartType::Fs(fs) => match fs {
            TargetFs::Fat16 | TargetFs::Fat32 | TargetFs::Exfat | TargetFs::Ntfs => guid("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"),
            TargetFs::Ufs2 => guid("516E7CB6-6ECF-11D6-8FF8-00022D09712B"),
            _ => guid("0FC63DAF-8483-4772-8E79-3D69D8477DE4"),
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

/// Parse "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" into GPT's mixed-endian byte order.
pub fn guid(s: &str) -> [u8; 16] {
    let hex: Vec<u8> = s.split('-').flat_map(|p| (0..p.len()).step_by(2).map(move |i| u8::from_str_radix(&p[i..i + 2], 16).expect("guid"))).collect();
    let mut g = [0u8; 16];
    g[0..4].copy_from_slice(&[hex[3], hex[2], hex[1], hex[0]]);
    g[4..6].copy_from_slice(&[hex[5], hex[4]]);
    g[6..8].copy_from_slice(&[hex[7], hex[6]]);
    g[8..16].copy_from_slice(&hex[8..16]);
    g
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

fn random16() -> [u8; 16] {
    *uuid::Uuid::new_v4().as_bytes()
}

/// Place `parts` on a disk of `size` bytes with `sector`-byte sectors. Returns (start, size)
/// for each, or an error if they don't fit.
pub fn place(size: u64, sector: u64, scheme: Scheme, parts: &[NewPart], align: u64) -> ForgeResult<Vec<Placed>> {
    let (first, last_end) = match scheme {
        // primary GPT: MBR + header + 128 entries (16 KiB); backup at the end.
        Scheme::Gpt => (sector * 2 + 16384, size.saturating_sub(16384 + sector)),
        Scheme::Mbr => (sector, size),
        Scheme::Apm => (sector * 64, size),
        Scheme::None => (0, size),
    };
    let align = if scheme == Scheme::None { 1 } else { align.max(sector) };
    let mut out = vec![];
    let mut pos = first;
    for (i, p) in parts.iter().enumerate() {
        let start = pos.div_ceil(align) * align;
        let size_p = match p.size {
            Some(s) => s / sector * sector,
            None if i == parts.len() - 1 => last_end.saturating_sub(start) / sector * sector,
            None => return Err(ForgeError::Invalid("only the last partition can take the rest of the disk".into())),
        };
        if size_p == 0 || start + size_p > last_end {
            return Err(ForgeError::Invalid(crate::i18n::t("le partizioni non entrano nel disco", "the partitions do not fit on the disk")));
        }
        out.push(Placed { index: i as u32 + 1, start, size: size_p });
        pos = start + size_p;
    }
    if scheme == Scheme::Mbr && out.len() > 4 {
        return Err(ForgeError::Unsupported("MBR: at most 4 partitions".into()));
    }
    if scheme == Scheme::None && out.len() != 1 {
        return Err(ForgeError::Invalid("no partition table: exactly one filesystem".into()));
    }
    if scheme == Scheme::Mbr && out.iter().any(|p| (p.start + p.size) / sector > u32::MAX as u64) {
        return Err(ForgeError::Unsupported(crate::i18n::t("MBR non supporta dischi oltre 2 TiB: usa GPT", "MBR can't address disks over 2 TiB: use GPT")));
    }
    Ok(out)
}

/// Write the partition table. Clears the areas a previous table could have left (both GPT
/// headers) so no stale table survives a scheme change.
pub fn write_table(dev: &dyn BlockDev, scheme: Scheme, parts: &[NewPart], opts: &TableOptions) -> ForgeResult<Vec<Placed>> {
    let sector = 512u64.max(sector_size(dev));
    let size = dev.size() / sector * sector;
    let placed = place(size, sector, scheme, parts, opts.align.unwrap_or(1 << 20))?;
    // Stale GPT headers / APM blocks at both ends.
    let zero = vec![0u8; 64 * 1024];
    dev.write_at(0, &zero)?;
    dev.write_at(size - zero.len() as u64, &zero)?;
    match scheme {
        Scheme::None => {}
        Scheme::Mbr => write_mbr(dev, sector, size, parts, &placed, opts, false)?,
        Scheme::Gpt => {
            write_mbr(dev, sector, size, parts, &placed, opts, true)?;
            write_gpt(dev, sector, size, parts, &placed, opts)?;
        }
        Scheme::Apm => return Err(ForgeError::Unsupported("APM: not implemented yet".into())),
    }
    dev.flush()?;
    Ok(placed)
}

/// Logical sector size of a device (512 unless it says otherwise).
pub fn sector_size(_dev: &dyn BlockDev) -> u64 {
    512
}

fn chs(lba: u64) -> [u8; 3] {
    // Beyond CHS range for anything but tiny disks: the conventional "max" value.
    if lba >= 1024 * 255 * 63 {
        return [0xFE, 0xFF, 0xFF];
    }
    let c = lba / (255 * 63);
    let h = (lba / 63) % 255;
    let s = lba % 63 + 1;
    [h as u8, (((c >> 2) & 0xC0) as u8) | s as u8, c as u8]
}

fn write_mbr(dev: &dyn BlockDev, sector: u64, size: u64, parts: &[NewPart], placed: &[Placed], opts: &TableOptions, protective: bool) -> ForgeResult<()> {
    let mut mbr = vec![0u8; 512];
    if let Some(code) = &opts.mbr_boot_code {
        let n = code.len().min(440);
        mbr[..n].copy_from_slice(&code[..n]);
    }
    let id = opts.disk_id.unwrap_or_else(random16);
    mbr[440..444].copy_from_slice(&id[..4]);
    let mut entry = |slot: usize, boot: bool, typ: u8, start_lba: u64, n: u64| {
        let o = 446 + slot * 16;
        mbr[o] = if boot { 0x80 } else { 0 };
        mbr[o + 1..o + 4].copy_from_slice(&chs(start_lba));
        mbr[o + 4] = typ;
        mbr[o + 5..o + 8].copy_from_slice(&chs(start_lba + n - 1));
        mbr[o + 8..o + 12].copy_from_slice(&(start_lba.min(u32::MAX as u64) as u32).to_le_bytes());
        mbr[o + 12..o + 16].copy_from_slice(&(n.min(u32::MAX as u64) as u32).to_le_bytes());
    };
    if protective {
        entry(0, false, 0xEE, 1, (size / sector - 1).min(u32::MAX as u64));
    } else {
        for (i, (p, pl)) in parts.iter().zip(placed).enumerate() {
            entry(i, p.bootable, mbr_type(p.kind), pl.start / sector, pl.size / sector);
        }
    }
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    dev.write_at(0, &mbr)?;
    Ok(())
}

fn write_gpt(dev: &dyn BlockDev, sector: u64, size: u64, parts: &[NewPart], placed: &[Placed], opts: &TableOptions) -> ForgeResult<()> {
    const N: usize = 128;
    const ESZ: usize = 128;
    let mut entries = vec![0u8; N * ESZ];
    for (i, (p, pl)) in parts.iter().zip(placed).enumerate() {
        let e = &mut entries[i * ESZ..(i + 1) * ESZ];
        e[0..16].copy_from_slice(&gpt_type(p.kind));
        e[16..32].copy_from_slice(&random16());
        e[32..40].copy_from_slice(&(pl.start / sector).to_le_bytes());
        e[40..48].copy_from_slice(&((pl.start + pl.size) / sector - 1).to_le_bytes());
        let attrs: u64 = if p.bootable { 1 << 2 } else { 0 }; // legacy BIOS bootable
        e[48..56].copy_from_slice(&attrs.to_le_bytes());
        for (k, u) in p.name.encode_utf16().take(36).enumerate() {
            e[56 + k * 2..58 + k * 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    let entries_crc = crc32(&entries);
    let lbas = size / sector;
    let entry_sectors = (N * ESZ) as u64 / sector;
    let first_usable = 2 + entry_sectors;
    let last_usable = lbas - 2 - entry_sectors;
    let disk_guid = opts.disk_id.unwrap_or_else(random16);
    let header = |my_lba: u64, alt_lba: u64, entries_lba: u64| {
        let mut h = vec![0u8; sector as usize];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
        h[40..48].copy_from_slice(&first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&disk_guid);
        h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&(N as u32).to_le_bytes());
        h[84..88].copy_from_slice(&(ESZ as u32).to_le_bytes());
        h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let c = crc32(&h[..92]);
        h[16..20].copy_from_slice(&c.to_le_bytes());
        h
    };
    let backup_entries_lba = lbas - 1 - entry_sectors;
    dev.write_at(2 * sector, &entries)?;
    dev.write_at(backup_entries_lba * sector, &entries)?;
    dev.write_at(sector, &header(1, lbas - 1, 2))?;
    dev.write_at((lbas - 1) * sector, &header(lbas - 1, 1, backup_entries_lba))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{parse_partitions, MemDev};

    fn p(size: Option<u64>, fs: TargetFs) -> NewPart {
        NewPart { size, kind: PartType::Fs(fs), name: "data".into(), bootable: false }
    }

    #[test]
    fn known_crc_and_guid_encoding() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let g = guid("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(&g[..4], &[0x28, 0x73, 0x2A, 0xC1]);
        assert_eq!(&g[8..], &[0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
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
        }
    }

    #[test]
    fn impossible_layouts_are_refused() {
        assert!(place(10 << 20, 512, Scheme::Mbr, &[p(Some(20 << 20), TargetFs::Fat32)], 1 << 20).is_err());
        assert!(place(64 << 20, 512, Scheme::Gpt, &[p(None, TargetFs::Fat32), p(None, TargetFs::Ext4)], 1 << 20).is_err());
        let five: Vec<_> = (0..5).map(|_| p(Some(1 << 20), TargetFs::Fat32)).collect();
        assert!(place(64 << 20, 512, Scheme::Mbr, &five, 1 << 20).is_err());
    }
}
