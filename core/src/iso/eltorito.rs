//! El Torito boot catalog ("El Torito" Bootable CD-ROM Format Specification 1.0, 1995).
//!
//! A Boot Record Volume Descriptor (ECMA-119 8.2, El Torito 2.0) with the identifier
//! "EL TORITO SPECIFICATION" points at the Boot Catalog: a Validation Entry (2.1), the
//! Initial/Default Entry (2.2), then Section Headers (2.3) each followed by Section Entries
//! (2.4) and their Extension Entries (2.5). UEFI images use platform id 0xEF (UEFI 2.x,
//! 13.3.2.1); their sector count is customarily 0 or 1 when the image is large, so the size is
//! also read from the image's own FAT boot sector.

use super::iso9660::{self, SECTOR};
use crate::block::BlockDev;
use crate::forge::{ForgeError, ForgeResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BootPlatform {
    Bios,
    Efi,
    PowerPc,
    Mac,
    Other(u8),
}

impl BootPlatform {
    fn from_id(id: u8) -> Self {
        match id {
            0 => BootPlatform::Bios,
            1 => BootPlatform::PowerPc,
            2 => BootPlatform::Mac,
            0xEF => BootPlatform::Efi,
            x => BootPlatform::Other(x),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BootMedia {
    NoEmulation,
    Floppy12,
    Floppy144,
    Floppy288,
    HardDisk,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct BootEntry {
    pub platform: BootPlatform,
    pub bootable: bool,
    pub media: BootMedia,
    /// Location of the image in 2048-byte sectors.
    pub lba: u32,
    /// Bytes of the image (from the sector count, or the image's own FAT header when the count
    /// is the conventional 0 / 1 of large EFI images).
    pub size: u64,
    /// Sector count field as recorded (512-byte virtual sectors).
    pub sectors: u16,
    pub load_segment: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct BootCatalog {
    /// Location of the catalog in 2048-byte sectors.
    pub catalog_lba: u32,
    pub entries: Vec<BootEntry>,
}

impl BootCatalog {
    /// The first bootable entry for `platform`.
    pub fn find(&self, platform: BootPlatform) -> Option<&BootEntry> {
        self.entries.iter().find(|e| e.platform == platform && e.bootable)
    }

    /// Read the bytes of a boot image (bounded by `max`).
    pub fn read_image(&self, dev: &dyn BlockDev, e: &BootEntry, max: u64) -> ForgeResult<Vec<u8>> {
        if e.size > max {
            return Err(ForgeError::Invalid(format!("El Torito: boot image too large ({} bytes)", e.size)));
        }
        let pos = e.lba as u64 * SECTOR;
        dev.read_vec(pos, e.size as usize).map_err(|err| ForgeError::Io(format!("El Torito: boot image: {err}")))
    }
}

/// Catalog sectors read at most (each holds 64 entries).
const MAX_CATALOG_SECTORS: u64 = 4;

/// Size of a FAT file system image from its boot sector (Microsoft FAT spec, BPB fields).
fn fat_image_size(b: &[u8]) -> Option<u64> {
    if b.len() < 512 || b[510] != 0x55 || b[511] != 0xAA || !(b[0] == 0xEB || b[0] == 0xE9) {
        return None;
    }
    let bps = super::le16(b, 11) as u64;
    if !matches!(bps, 512 | 1024 | 2048 | 4096) {
        return None;
    }
    let small = super::le16(b, 19) as u64;
    let total = if small != 0 { small } else { super::le32(b, 32) as u64 };
    (total > 0).then(|| total * bps)
}

/// Parse the boot catalog. Ok(None): no El Torito boot record.
pub(crate) fn read(dev: &dyn BlockDev) -> ForgeResult<Option<BootCatalog>> {
    if !iso9660::has_iso9660(dev) {
        return Ok(None);
    }
    let vds = iso9660::volume_descriptors(dev)?;
    // Boot Record Volume Descriptor (El Torito 2.0): identifier at 7, catalog pointer at 0x47.
    let Some((_, brvd)) = vds.iter().find(|(t, v)| *t == 0 && v[7..30] == *b"EL TORITO SPECIFICATION") else { return Ok(None) };
    let catalog_lba = super::le32(brvd, 0x47);
    let pos = catalog_lba as u64 * SECTOR;
    let len = (MAX_CATALOG_SECTORS * SECTOR).min(dev.size().saturating_sub(pos));
    if len < 64 {
        return Err(ForgeError::Invalid("El Torito: boot catalog beyond the end of the image".into()));
    }
    let cat = dev.read_vec(pos, len as usize).map_err(|e| ForgeError::Io(format!("El Torito: {e}")))?;
    // Validation Entry (2.1): header id 1, key bytes 55 AA, 16-bit words summing to zero.
    let v = &cat[0..32];
    let sum = (0..16).fold(0u16, |s, i| s.wrapping_add(super::le16(v, i * 2)));
    if v[0] != 1 || v[30] != 0x55 || v[31] != 0xAA || sum != 0 {
        return Err(ForgeError::Invalid("El Torito: invalid validation entry".into()));
    }
    let mut out = BootCatalog { catalog_lba, entries: vec![] };
    let mut platform = BootPlatform::from_id(v[1]);
    // Initial / Default Entry (2.2).
    if let Some(e) = entry(dev, &cat[32..64], platform) {
        out.entries.push(e);
    }
    let mut p = 64usize;
    while p + 32 <= cat.len() {
        let h = &cat[p..p + 32];
        match h[0] {
            // Section Header (2.3): 0x90 = more headers follow, 0x91 = final.
            0x90 | 0x91 => {
                platform = BootPlatform::from_id(h[1]);
                let n = super::le16(h, 2) as usize;
                p += 32;
                let mut k = 0usize;
                while k < n && p + 32 <= cat.len() {
                    let e = &cat[p..p + 32];
                    p += 32;
                    if e[0] == 0x44 {
                        continue; // Extension Entry (2.5) of the previous entry
                    }
                    if let Some(b) = entry(dev, e, platform) {
                        out.entries.push(b);
                    }
                    k += 1;
                    // Section Entry Extensions follow while bit 5 of the media byte says so.
                    while p + 32 <= cat.len() && cat[p] == 0x44 {
                        p += 32;
                    }
                }
                if h[0] == 0x91 {
                    break;
                }
            }
            0x44 => p += 32,
            _ => break,
        }
    }
    Ok(Some(out))
}

/// A Default Entry (2.2) or Section Entry (2.4).
fn entry(dev: &dyn BlockDev, e: &[u8], platform: BootPlatform) -> Option<BootEntry> {
    let bootable = match e[0] {
        0x88 => true,
        0x00 => false,
        _ => return None,
    };
    let media = match e[1] & 0x0F {
        0 => BootMedia::NoEmulation,
        1 => BootMedia::Floppy12,
        2 => BootMedia::Floppy144,
        3 => BootMedia::Floppy288,
        4 => BootMedia::HardDisk,
        _ => return None,
    };
    let lba = super::le32(e, 8);
    let sectors = super::le16(e, 6);
    if lba == 0 && sectors == 0 {
        return None;
    }
    let pos = lba as u64 * SECTOR;
    let avail = dev.size().saturating_sub(pos);
    let head = dev.read_vec(pos, 512.min(avail as usize)).ok();
    let mut size = match media {
        BootMedia::Floppy12 => 1_228_800,
        BootMedia::Floppy144 => 1_474_560,
        BootMedia::Floppy288 => 2_949_120,
        BootMedia::NoEmulation => sectors as u64 * 512,
        BootMedia::HardDisk => {
            // The image is a disk with an MBR: its size is the end of its single partition.
            head.as_deref()
                .filter(|h| h.len() >= 512 && h[510] == 0x55 && h[511] == 0xAA)
                .map(|h| (super::le32(h, 446 + 8) as u64 + super::le32(h, 446 + 12) as u64) * 512)
                .unwrap_or(sectors as u64 * 512)
        }
    };
    if media == BootMedia::NoEmulation && platform == BootPlatform::Efi {
        if let Some(fat) = head.as_deref().and_then(fat_image_size) {
            size = size.max(fat);
        }
    }
    Some(BootEntry { platform, bootable, media, lba, size: size.min(avail), sectors, load_segment: super::le16(e, 2) })
}
