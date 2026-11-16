//! El Torito boot catalog ("El Torito" Bootable CD-ROM Format Specification 1.0).

use crate::block::BlockDev;
use crate::forge::ForgeResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BootPlatform {
    Bios,
    Efi,
    PowerPc,
    Mac,
    Other(u8),
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

pub(crate) fn read(_dev: &dyn BlockDev) -> ForgeResult<Option<BootCatalog>> {
    Ok(None)
}
