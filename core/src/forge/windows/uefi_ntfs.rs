//! UEFI:NTFS: the small FAT partition that lets UEFI firmware boot an NTFS partition
//! (Rufus's "NTFS + UEFI:NTFS" layout). The firmware starts `\EFI\Boot\boot<arch>.efi` from
//! it — UEFI:NTFS, which loads the NTFS driver from `\EFI\Rufus\ntfs_<arch>.efi` and then
//! chain-loads `\efi\boot\boot<arch>.efi` from the NTFS partition.
//!
//! The partition image is embedded verbatim from Rufus 4.15 (`res/uefi/uefi-ntfs.img`, 1 MiB,
//! FAT12, label `RUFUS_BOOT`), whose `res/uefi/readme.txt` describes it:
//! - UEFI:NTFS 2.8 boot loaders (GPL-2.0-or-later, https://github.com/pbatard/uefi-ntfs);
//!   x86_32, x64 and ARM64 binaries signed with "Microsoft Corporation UEFI CA 2011", so they
//!   start under Secure Boot when the firmware trusts Microsoft's third-party CA;
//! - the ntfs-3g based NTFS UEFI drivers 1.9 (read-only, GPL-2.0-or-later,
//!   https://github.com/pbatard/ntfs-3g, `ntfs_x64.efi` reports "ntfs-3g a2226ecd"), also signed
//!   with the Microsoft UEFI CA 2011;
//! - the exFAT and ARM/RISC-V NTFS drivers of EfiFs 1.12 (GPL-3.0-or-later,
//!   https://github.com/pbatard/efifs), not Secure Boot signed.
//!
//! Corresponding source (GPL): https://github.com/pbatard/uefi-ntfs (tag v2.8, commit
//! 42de94dce5a0be68aab4e410494bb9d986dbcac4), https://github.com/pbatard/ntfs-3g (UEFI driver
//! 1.9, commit a2226ecd8344a427a7ad79fbb878f3244c7af393), https://github.com/pbatard/efifs
//! (tag v1.12, commit bfc6ae29ac15516bddb97928eaf1eda187a6d242); the image itself comes from
//! https://github.com/pbatard/rufus/blob/v4.15/res/uefi/uefi-ntfs.img (tag v4.15, commit
//! 6d8fbf98305ff37eb531c45cbd6ff44563c53917).

use sha2::Digest;

use super::super::{ForgeError, ForgeResult};
use crate::block::BlockDev;

/// The partition image, byte for byte as in Rufus 4.15.
pub static IMAGE: &[u8] = include_bytes!("res/uefi-ntfs.img");
/// sha256 of [`IMAGE`].
pub const SHA256: &str = "72683fa1250eeea772d3399277b434d4e55ba8dd0dc926e52d817e701fc2eb9e";
/// What the result JSON / logs say about it.
pub const VERSION: &str = "UEFI:NTFS 2.8, ntfs-3g UEFI driver 1.9 (from Rufus 4.15)";
/// Size of the partition Rufus creates for it (the image size).
pub const PARTITION_SIZE: u64 = 1 << 20;
/// GPT partition name Rufus uses (and looks for).
pub const GPT_NAME: &str = "UEFI:NTFS";
/// MBR type: an EFI System Partition ("UEFI:NTFS as an MBR ESP somehow seems to be okay",
/// Rufus `drive.c`).
pub const MBR_TYPE: u8 = 0xEF;
/// GPT: Microsoft basic data, not an ESP (Windows Setup can't cope with two ESPs, Rufus
/// `drive.c`), with `GPT_BASIC_DATA_ATTRIBUTE_NO_DRIVE_LETTER`.
pub const GPT_TYPE: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
pub const GPT_ATTRIBUTES: u64 = 0x8000_0000_0000_0000;

/// Check the embedded image against its recorded hash.
pub fn embedded_ok() -> bool {
    let d: [u8; 32] = sha2::Sha256::digest(IMAGE).into();
    d.iter().map(|b| format!("{b:02x}")).collect::<String>() == SHA256
}

/// Write the image at the start of `part` (the UEFI:NTFS partition).
pub fn write(part: &dyn BlockDev) -> ForgeResult<()> {
    if part.size() < IMAGE.len() as u64 {
        return Err(ForgeError::Invalid(format!("UEFI:NTFS partition too small ({} bytes)", part.size())));
    }
    part.write_at(0, IMAGE)?;
    part.flush()?;
    Ok(())
}

/// The partition holds the image.
pub fn present(part: &dyn BlockDev) -> bool {
    part.read_vec(0, IMAGE.len()).is_ok_and(|d| d == IMAGE)
}

/// OR `attrs` into the attributes of GPT partition `index` (1-based), in the primary and the
/// backup table, and fix both CRCs. (`layout::write_table` only sets the legacy-BIOS-bootable
/// bit; this is the one-off for UEFI:NTFS's "no drive letter" bit.)
pub fn gpt_add_attributes(disk: &dyn BlockDev, index: u32, attrs: u64) -> ForgeResult<()> {
    use super::super::layout::crc32;
    let bad = |m: &str| ForgeError::Invalid(format!("GPT: {m}"));
    let primary = disk.read_vec(512, 512)?;
    if &primary[..8] != b"EFI PART" {
        return Err(bad("no primary header"));
    }
    let alt_lba = u64::from_le_bytes(primary[32..40].try_into().expect("8"));
    for lba in [1u64, alt_lba] {
        let off = lba.checked_mul(512).ok_or_else(|| bad("bad header location"))?;
        let mut h = disk.read_vec(off, 512)?;
        if &h[..8] != b"EFI PART" {
            return Err(bad("missing header"));
        }
        let hsize = u32::from_le_bytes(h[12..16].try_into().expect("4")) as usize;
        let entries_lba = u64::from_le_bytes(h[72..80].try_into().expect("8"));
        let n = u32::from_le_bytes(h[80..84].try_into().expect("4")) as usize;
        let esz = u32::from_le_bytes(h[84..88].try_into().expect("4")) as usize;
        if !(92..=512).contains(&hsize) || esz < 128 || n == 0 || n.saturating_mul(esz) > (1 << 20) || index as usize > n || index == 0 {
            return Err(bad("unexpected header"));
        }
        let eoff = entries_lba.checked_mul(512).ok_or_else(|| bad("bad entries location"))?;
        let mut entries = disk.read_vec(eoff, n * esz)?;
        let e = (index as usize - 1) * esz;
        let cur = u64::from_le_bytes(entries[e + 48..e + 56].try_into().expect("8"));
        entries[e + 48..e + 56].copy_from_slice(&(cur | attrs).to_le_bytes());
        h[88..92].copy_from_slice(&crc32(&entries).to_le_bytes());
        h[16..20].copy_from_slice(&[0; 4]);
        let c = crc32(&h[..hsize]);
        h[16..20].copy_from_slice(&c.to_le_bytes());
        disk.write_at(eoff, &entries)?;
        disk.write_at(off, &h)?;
    }
    disk.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{parse_partitions, MemDev};
    use crate::forge::layout::{self, guid, NewPart, PartType, TableOptions};
    use crate::forge::plan::Scheme;
    use crate::forge::TargetFs;

    #[test]
    fn the_embedded_image_is_rufus_4_15s_and_is_a_fat_volume() {
        assert!(embedded_ok());
        assert_eq!(IMAGE.len() as u64, PARTITION_SIZE);
        assert_eq!(&IMAGE[0x2B..0x36], b"RUFUS_BOOT ");
        assert_eq!(&IMAGE[0x36..0x3E], b"FAT12   ");
    }

    #[test]
    fn gpt_attributes_are_added_and_the_tables_stay_valid() {
        let dev = MemDev::new(64 << 20);
        let parts = [
            NewPart { size: Some(32 << 20), kind: PartType::Fs(TargetFs::Ntfs), name: "ESD-USB".into(), bootable: false },
            NewPart { size: Some(PARTITION_SIZE), kind: PartType::Raw { mbr: MBR_TYPE, gpt: guid(GPT_TYPE) }, name: GPT_NAME.into(), bootable: false },
        ];
        layout::write_table(&dev, Scheme::Gpt, &parts, &TableOptions::default()).unwrap();
        gpt_add_attributes(&dev, 2, GPT_ATTRIBUTES).unwrap();
        let got = parse_partitions(&dev).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].name, "UEFI:NTFS");
        let snap = dev.snapshot();
        // entry 2 attributes, primary entries at LBA 2
        let e = 2 * 512 + 128;
        assert_eq!(u64::from_le_bytes(snap[e + 48..e + 56].try_into().unwrap()), GPT_ATTRIBUTES);
        // CRCs: header and entries, both copies
        for hoff in [512usize, snap.len() - 512] {
            let h = &snap[hoff..hoff + 512];
            let eoff = u64::from_le_bytes(h[72..80].try_into().unwrap()) as usize * 512;
            assert_eq!(u32::from_le_bytes(h[88..92].try_into().unwrap()), layout::crc32(&snap[eoff..eoff + 128 * 128]));
            let mut hh = h[..92].to_vec();
            hh[16..20].copy_from_slice(&[0; 4]);
            assert_eq!(u32::from_le_bytes(h[16..20].try_into().unwrap()), layout::crc32(&hh));
        }
    }
}
