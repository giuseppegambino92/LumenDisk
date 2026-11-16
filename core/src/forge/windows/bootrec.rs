//! BIOS boot records for Windows media, ported from ms-sys as Rufus uses it
//! (`src/ms-sys/br.c`, `fat32.c`, `ntfs.c`, `partition_info.c`; Rufus `format.c`
//! `WriteMBR` / `WritePBR`): the Windows 7 MBR, and the NT6 partition boot records for FAT32
//! and NTFS that load `\BOOTMGR` from the partition. The bytes are in [`super::ms_sys`].
//!
//! Like ms-sys, only the code areas are written: the jump + OEM name (0x00–0x0A), then the
//! code after the BIOS Parameter Block. The BPB, the label and the other filesystem fields are
//! kept. Two fields that a Windows format sets and the boot code reads are then set the way
//! ms-sys's `partition_info.c` helpers set them: the "hidden sectors" (partition start LBA,
//! 0x1C) and the BIOS drive number (0x80, at 0x40 on FAT32 and 0x24 on NTFS).
//!
//! One deliberate difference from `write_fat_32_pe_br`: its 0x52 array also spans the FSInfo
//! sector (0x200–0x3EF) with a free-cluster count of 0. Those bytes are filesystem fields, not
//! code, so they are preserved here (the FSInfo signatures, which that array also contains,
//! are checked instead): the free count of the volume stays right.

use super::super::{ForgeError, ForgeResult};
use super::ms_sys::*;
use crate::block::BlockDev;

const SECTOR: u64 = 512;

/// The Windows 7 MBR boot code (440 bytes, for `layout::TableOptions::mbr_boot_code`).
pub fn win7_mbr() -> Vec<u8> {
    MBR_WIN7.to_vec()
}

fn read(dev: &dyn BlockDev, off: u64, len: usize) -> ForgeResult<Vec<u8>> {
    Ok(dev.read_vec(off, len)?)
}

fn hidden_sectors(start_lba: u64) -> ForgeResult<[u8; 4]> {
    u32::try_from(start_lba)
        .map(u32::to_le_bytes)
        .map_err(|_| ForgeError::Unsupported("partition starts beyond 2 TiB: BIOS boot records can't address it".into()))
}

/// ms-sys `is_fat_32_fs` + the layout the NT6 code needs (sectors 0–2 and 12, and their
/// backups 6–8 and 18, in the reserved area).
fn check_fat32(bs: &[u8]) -> ForgeResult<()> {
    let not = |why: &str| Err(ForgeError::Invalid(format!("FAT32 boot record: {why}")));
    if &bs[0x52..0x5A] != b"FAT32   " || bs[0x1FE] != 0x55 || bs[0x1FF] != 0xAA {
        return not("the volume has no FAT32 boot sector");
    }
    if u16::from_le_bytes([bs[0x0B], bs[0x0C]]) != 512 {
        return not("only 512-byte sectors are supported");
    }
    let reserved = u16::from_le_bytes([bs[0x0E], bs[0x0F]]);
    let backup = u16::from_le_bytes([bs[0x32], bs[0x33]]);
    if reserved < 19 || backup != 6 {
        return not("unexpected reserved area (needs ≥ 19 sectors, backup boot sector at 6)");
    }
    Ok(())
}

/// Install the NT6 FAT32 boot record (ms-sys `write_fat_32_pe_br(fp, bKeepLabel = 1)` on the
/// boot sector and on its backup at sector 6, then `write_partition_physical_disk_drive_id_fat32`
/// and `write_partition_start_sector_number`). `part` is the partition, `start_lba` where it
/// starts on the disk.
pub fn install_fat32(part: &dyn BlockDev, start_lba: u64) -> ForgeResult<()> {
    let hidden = hidden_sectors(start_lba)?;
    let bs = read(part, 0, 512)?;
    check_fat32(&bs)?;
    // Rufus: `for (i = 0; i < 2; i++) { … fake_fd._offset += 6 * SectorSize; }`
    for base in [0, 6 * SECTOR] {
        let mut s0 = read(part, base, 512)?;
        let fsinfo = read(part, base + SECTOR, 512)?;
        if fsinfo[0..4] != *b"RRaA" || fsinfo[0x1E4..0x1E8] != *b"rrAa" {
            return Err(ForgeError::Invalid("FAT32 boot record: no FSInfo sector".into()));
        }
        s0[..BR_FAT32_0X0.len()].copy_from_slice(&BR_FAT32_0X0);
        // 0x52..0x200 of the 0x52 array is the code; past 0x200 it is FSInfo (kept, see above).
        let code = &BR_FAT32PE_0X52[..0x200 - 0x52];
        s0[0x52..0x200].copy_from_slice(code);
        s0[0x1C..0x20].copy_from_slice(&hidden);
        s0[0x40] = 0x80;
        part.write_at(base, &s0)?;
        part.write_at(base + 0x3F0, &BR_FAT32PE_0X3F0)?;
        part.write_at(base + 0x1800, &BR_FAT32PE_0X1800)?;
    }
    part.flush()?;
    Ok(())
}

/// ms-sys `entire_fat_32_pe_br_matches` (plus our hidden-sectors field).
pub fn has_fat32(part: &dyn BlockDev, start_lba: u64) -> bool {
    let Ok(s) = read(part, 0, 0x1A00) else { return false };
    s[..11] == BR_FAT32_0X0
        && s[0x52..0x200] == BR_FAT32PE_0X52[..0x200 - 0x52]
        && s[0x3F0..0x600] == BR_FAT32PE_0X3F0
        && s[0x1800..0x1A00] == BR_FAT32PE_0X1800
        && hidden_sectors(start_lba).is_ok_and(|h| s[0x1C..0x20] == h)
}

/// Install the NT6 NTFS boot record (ms-sys `write_ntfs_br`: 0x00 and 0x54…0x1027, i.e. the
/// first 8 KiB of `$Boot` minus the BPB). The backup boot sector (last sector of the volume)
/// gets the same first sector, as a Windows format leaves it.
pub fn install_ntfs(part: &dyn BlockDev, start_lba: u64) -> ForgeResult<()> {
    let hidden = hidden_sectors(start_lba)?;
    let mut s0 = read(part, 0, 512)?;
    if &s0[3..11] != b"NTFS    " || s0[0x1FE] != 0x55 || s0[0x1FF] != 0xAA {
        return Err(ForgeError::Invalid("NTFS boot record: the volume has no NTFS boot sector".into()));
    }
    if u16::from_le_bytes([s0[0x0B], s0[0x0C]]) != 512 {
        return Err(ForgeError::Invalid("NTFS boot record: only 512-byte sectors are supported".into()));
    }
    // $Boot is at least 8 KiB (16 sectors); the code ends at 0x1028.
    let end = 0x54 + BR_NTFS_0X54.len();
    debug_assert!(end <= 8192);
    let mut area = read(part, 0, end)?;
    area[..BR_NTFS_0X0.len()].copy_from_slice(&BR_NTFS_0X0);
    area[0x54..end].copy_from_slice(&BR_NTFS_0X54);
    area[0x1C..0x20].copy_from_slice(&hidden);
    area[0x24] = 0x80;
    part.write_at(0, &area)?;
    s0.copy_from_slice(&area[..512]);
    let total = u64::from_le_bytes(s0[0x28..0x30].try_into().expect("8 bytes"));
    if let Some(off) = total.checked_mul(SECTOR).filter(|&o| o.checked_add(SECTOR).is_some_and(|e| e <= part.size())) {
        let backup = read(part, off, 512)?;
        if &backup[3..11] == b"NTFS    " {
            part.write_at(off, &s0)?;
        }
    }
    part.flush()?;
    Ok(())
}

/// ms-sys `entire_ntfs_br_matches` (plus our hidden-sectors field).
pub fn has_ntfs(part: &dyn BlockDev, start_lba: u64) -> bool {
    let end = 0x54 + BR_NTFS_0X54.len();
    let Ok(s) = read(part, 0, end) else { return false };
    s[..11] == BR_NTFS_0X0 && s[0x54..end] == BR_NTFS_0X54 && hidden_sectors(start_lba).is_ok_and(|h| s[0x1C..0x20] == h)
}

/// The MBR carries the Windows 7 boot code (ms-sys `is_win7_mbr`: the language-independent
/// first 0x163 bytes).
pub fn has_win7_mbr(disk: &dyn BlockDev) -> bool {
    read(disk, 0, 512).is_ok_and(|m| m[..0x163] == MBR_WIN7[..0x163] && m[510] == 0x55 && m[511] == 0xAA)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemDev;
    use crate::vfs::{self, FsType, MountDefaults};
    use std::sync::Arc;

    #[test]
    fn arrays_have_the_sizes_ms_sys_writes() {
        assert_eq!(MBR_WIN7.len(), 440);
        assert_eq!(&MBR_WIN7[..4], &[0x33, 0xC0, 0x8E, 0xD0]);
        assert_eq!(BR_FAT32_0X0, *b"\xEB\x58\x90MSWIN4.1");
        assert_eq!(&BR_FAT32PE_0X52[..8], b"FAT32   ");
        assert_eq!(&BR_FAT32PE_0X52[0x1FE - 0x52..0x200 - 0x52], &[0x55, 0xAA]);
        assert_eq!(BR_FAT32PE_0X3F0.len(), 0x600 - 0x3F0);
        assert_eq!(BR_FAT32PE_0X1800.len(), 512);
        assert_eq!(BR_NTFS_0X0, *b"\xEB\x52\x90NTFS    ");
        assert_eq!(&BR_NTFS_0X54[0x1FE - 0x54..0x200 - 0x54], &[0x55, 0xAA]);
        // Every sector of the NTFS code past the first ends like ms-sys's array does.
        assert_eq!(0x54 + BR_NTFS_0X54.len(), 0x1028);
    }

    fn fs_files_ok(dev: Arc<dyn BlockDev>) {
        let mut fs = vfs::open_fs(dev, false, MountDefaults::default()).unwrap();
        crate::forge::tree::write_file(fs.as_mut(), "/bootmgr", &vec![0x5Au8; 9000]).unwrap();
        fs.sync().unwrap();
        assert_eq!(crate::forge::tree::read_file(fs.as_mut(), "/BOOTMGR").unwrap().unwrap(), vec![0x5Au8; 9000]);
    }

    #[test]
    fn fat32_record_keeps_the_bpb_label_and_free_count() {
        let mem = Arc::new(MemDev::new(64 << 20));
        let dev: Arc<dyn BlockDev> = mem.clone();
        vfs::format(dev.clone(), FsType::Fat32, "ESD-USB").unwrap();
        fs_files_ok(dev.clone());
        let before = mem.snapshot();
        install_fat32(dev.as_ref(), 2048).unwrap();
        let after = mem.snapshot();
        assert!(has_fat32(dev.as_ref(), 2048));
        assert!(!has_fat32(dev.as_ref(), 4096));
        // BPB (0x0B..0x52 minus hidden sectors / drive id) untouched, label kept.
        assert_eq!(after[0x0B..0x1C], before[0x0B..0x1C]);
        assert_eq!(after[0x20..0x40], before[0x20..0x40]);
        assert_eq!(after[0x41..0x52], before[0x41..0x52]);
        assert_eq!(&after[0x47..0x52], b"ESD-USB    ");
        assert_eq!(u32::from_le_bytes(after[0x1C..0x20].try_into().unwrap()), 2048);
        // FSInfo (free count, next free) untouched.
        assert_eq!(after[0x200..0x3F0], before[0x200..0x3F0]);
        // Backup boot sector got the same treatment.
        assert_eq!(after[0xC00..0xE00], after[0..0x200]);
        assert_eq!(after[0x2400..0x2600], BR_FAT32PE_0X1800, "sector 6 + 12");
        // Nothing beyond the reserved area changed, and the volume still mounts.
        assert_eq!(after[32 * 512..], before[32 * 512..]);
        fs_files_ok(dev);
    }

    #[test]
    fn ntfs_record_keeps_the_bpb_and_the_volume_mounts() {
        let mem = Arc::new(MemDev::new(64 << 20));
        let dev: Arc<dyn BlockDev> = mem.clone();
        vfs::format(dev.clone(), FsType::Ntfs, "ESD-USB").unwrap();
        fs_files_ok(dev.clone());
        let before = mem.snapshot();
        install_ntfs(dev.as_ref(), 2048).unwrap();
        let after = mem.snapshot();
        assert!(has_ntfs(dev.as_ref(), 2048));
        assert_eq!(after[0x0B..0x1C], before[0x0B..0x1C]);
        assert_eq!(after[0x20..0x24], before[0x20..0x24]);
        assert_eq!(after[0x25..0x54], before[0x25..0x54], "BPB incl. serial and MFT location");
        let n = after.len();
        assert_eq!(after[0x1028..n - 512], before[0x1028..n - 512], "only $Boot and the backup sector change");
        assert_eq!(after[n - 512..], after[..512], "backup boot sector updated");
        fs_files_ok(dev);
    }

    #[test]
    fn wrong_filesystems_are_refused() {
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(64 << 20));
        assert!(install_fat32(dev.as_ref(), 2048).is_err());
        assert!(install_ntfs(dev.as_ref(), 2048).is_err());
        vfs::format(dev.clone(), FsType::Ntfs, "X").unwrap();
        assert!(install_fat32(dev.as_ref(), 2048).is_err());
        assert!(install_ntfs(dev.as_ref(), 1 << 33).is_err(), "beyond 2 TiB");
    }
}
