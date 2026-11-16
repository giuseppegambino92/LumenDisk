//! SYSLINUX 6.03 on FAT32: the official release binaries (see `syslinux-6.03/SOURCE`) and a
//! Rust port of the parts of SYSLINUX's `libinstaller` that make a FAT volume boot:
//!
//! - `syslxmod.c` `syslinux_patch()`: find the patch area in `ldlinux.sys` (LDLINUX_MAGIC),
//!   record the sector map of the file as extents, the ADV sectors, the install directory, and
//!   recompute the negative checksum;
//! - `fs.c` `syslinux_make_bootsect()`: put the `ldlinux.bss` boot code into the volume's boot
//!   sector around the existing BPB (bytes 0–10 and 90–509 for FAT32);
//! - `setadv.c` `syslinux_reset_adv()`: an empty Auxiliary Data Vector (two 512-byte copies);
//! - the `mtools` installer's sequence: write `ldlinux.sys` (+ ADV) as a file, map its sectors
//!   on the device, write the patched first sectors back, then the boot sector.
//!
//! SYSLINUX is © H. Peter Anvin et al., GPL-2.0-or-later; LumenDisk is GPL-3.0-or-later.

use super::super::{ForgeError, ForgeResult};
use crate::block::BlockDev;

/// The embedded release.
pub const VERSION: &str = "6.03";
pub const VERSION_NUM: (u32, u32) = (6, 3);

pub static LDLINUX_SYS: &[u8] = include_bytes!("syslinux-6.03/ldlinux.sys");
pub static LDLINUX_BSS: &[u8] = include_bytes!("syslinux-6.03/ldlinux.bss");
/// MBR boot code (first 440 bytes): boots the active partition.
pub static MBR_BIN: &[u8] = include_bytes!("syslinux-6.03/mbr.bin");

/// COM32 modules of the release (BIOS build), by file name.
pub static MODULES: &[(&str, &[u8])] = &[
    ("cat.c32", include_bytes!("syslinux-6.03/cat.c32")),
    ("chain.c32", include_bytes!("syslinux-6.03/chain.c32")),
    ("cmd.c32", include_bytes!("syslinux-6.03/cmd.c32")),
    ("config.c32", include_bytes!("syslinux-6.03/config.c32")),
    ("cptime.c32", include_bytes!("syslinux-6.03/cptime.c32")),
    ("cpuid.c32", include_bytes!("syslinux-6.03/cpuid.c32")),
    ("cpuidtest.c32", include_bytes!("syslinux-6.03/cpuidtest.c32")),
    ("debug.c32", include_bytes!("syslinux-6.03/debug.c32")),
    ("disk.c32", include_bytes!("syslinux-6.03/disk.c32")),
    ("dmitest.c32", include_bytes!("syslinux-6.03/dmitest.c32")),
    ("elf.c32", include_bytes!("syslinux-6.03/elf.c32")),
    ("ethersel.c32", include_bytes!("syslinux-6.03/ethersel.c32")),
    ("gfxboot.c32", include_bytes!("syslinux-6.03/gfxboot.c32")),
    ("gpxecmd.c32", include_bytes!("syslinux-6.03/gpxecmd.c32")),
    ("hdt.c32", include_bytes!("syslinux-6.03/hdt.c32")),
    ("hexdump.c32", include_bytes!("syslinux-6.03/hexdump.c32")),
    ("host.c32", include_bytes!("syslinux-6.03/host.c32")),
    ("ifcpu.c32", include_bytes!("syslinux-6.03/ifcpu.c32")),
    ("ifcpu64.c32", include_bytes!("syslinux-6.03/ifcpu64.c32")),
    ("ifmemdsk.c32", include_bytes!("syslinux-6.03/ifmemdsk.c32")),
    ("ifplop.c32", include_bytes!("syslinux-6.03/ifplop.c32")),
    ("kbdmap.c32", include_bytes!("syslinux-6.03/kbdmap.c32")),
    ("kontron_wdt.c32", include_bytes!("syslinux-6.03/kontron_wdt.c32")),
    ("ldlinux.c32", include_bytes!("syslinux-6.03/ldlinux.c32")),
    ("libcom32.c32", include_bytes!("syslinux-6.03/libcom32.c32")),
    ("libgpl.c32", include_bytes!("syslinux-6.03/libgpl.c32")),
    ("libmenu.c32", include_bytes!("syslinux-6.03/libmenu.c32")),
    ("libutil.c32", include_bytes!("syslinux-6.03/libutil.c32")),
    ("linux.c32", include_bytes!("syslinux-6.03/linux.c32")),
    ("ls.c32", include_bytes!("syslinux-6.03/ls.c32")),
    ("mboot.c32", include_bytes!("syslinux-6.03/mboot.c32")),
    ("meminfo.c32", include_bytes!("syslinux-6.03/meminfo.c32")),
    ("menu.c32", include_bytes!("syslinux-6.03/menu.c32")),
    ("pcitest.c32", include_bytes!("syslinux-6.03/pcitest.c32")),
    ("pmload.c32", include_bytes!("syslinux-6.03/pmload.c32")),
    ("poweroff.c32", include_bytes!("syslinux-6.03/poweroff.c32")),
    ("prdhcp.c32", include_bytes!("syslinux-6.03/prdhcp.c32")),
    ("pwd.c32", include_bytes!("syslinux-6.03/pwd.c32")),
    ("pxechn.c32", include_bytes!("syslinux-6.03/pxechn.c32")),
    ("reboot.c32", include_bytes!("syslinux-6.03/reboot.c32")),
    ("sanboot.c32", include_bytes!("syslinux-6.03/sanboot.c32")),
    ("sdi.c32", include_bytes!("syslinux-6.03/sdi.c32")),
    ("vesainfo.c32", include_bytes!("syslinux-6.03/vesainfo.c32")),
    ("vesamenu.c32", include_bytes!("syslinux-6.03/vesamenu.c32")),
    ("vpdtest.c32", include_bytes!("syslinux-6.03/vpdtest.c32")),
    ("whichsys.c32", include_bytes!("syslinux-6.03/whichsys.c32")),
    ("zzjson.c32", include_bytes!("syslinux-6.03/zzjson.c32")),
];

/// The embedded build of module `name` ("vesamenu.c32"), ignoring case.
pub fn module(name: &str) -> Option<&'static [u8]> {
    MODULES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, b)| *b)
}

/// "ISOLINUX 6.04 6.04-pre1 …" in an isolinux.bin / ldlinux.sys / ldlinux.c32: (major, minor,
/// the version text as written).
pub fn detect_version(bin: &[u8]) -> Option<(u32, u32, String)> {
    for tag in [&b"ISOLINUX "[..], b"SYSLINUX ", b"EXTLINUX ", b"PXELINUX "] {
        let mut from = 0usize;
        while let Some(p) = find(&bin[from..], tag) {
            let at = from + p + tag.len();
            let tail = &bin[at..bin.len().min(at + 40)];
            let text: String = tail.iter().take_while(|&&c| (0x20..0x7F).contains(&c)).map(|&c| c as char).collect();
            let ver = text.split_whitespace().next().unwrap_or("");
            let mut it = ver.splitn(2, '.');
            if let (Some(maj), Some(min)) = (it.next(), it.next()) {
                let min: String = min.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let (Ok(a), Ok(b)) = (maj.parse::<u32>(), min.parse::<u32>()) {
                    if (1..=20).contains(&a) {
                        return Some((a, b, text.trim().to_string()));
                    }
                }
            }
            from = at;
        }
    }
    None
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

const SECTOR: usize = 512;
/// `syslxint.h`
const LDLINUX_MAGIC: u32 = 0x3eb2_02fe;
/// `setadv.h` / `setadv.c`
const ADV_SIZE: usize = 512;
const ADV_MAGIC1: u32 = 0x5a2d_2fa5;
const ADV_MAGIC2: u32 = 0xa304_1767;
const ADV_MAGIC3: u32 = 0xdd28_bf64;

fn rd16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn wr16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn wr32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn wr64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// `ldlinux.sys` as the installer embeds it (bin2c pads it to a multiple of 512 bytes).
fn boot_image() -> Vec<u8> {
    let mut v = LDLINUX_SYS.to_vec();
    v.resize(LDLINUX_SYS.len().div_ceil(SECTOR) * SECTOR, 0);
    v
}

/// `syslinux_reset_adv()` + `cleanup_adv()`: an empty ADV, primary and backup copy.
fn empty_adv() -> Vec<u8> {
    let mut a = vec![0u8; 2 * ADV_SIZE];
    wr32(&mut a, 0, ADV_MAGIC1);
    let mut csum = ADV_MAGIC2;
    for i in (8..ADV_SIZE - 4).step_by(4) {
        csum = csum.wrapping_sub(rd32(&a, i));
    }
    wr32(&mut a, 4, csum);
    wr32(&mut a, ADV_SIZE - 4, ADV_MAGIC3);
    let (first, second) = a.split_at_mut(ADV_SIZE);
    second.copy_from_slice(first);
    a
}

/// The content of `ldlinux.sys` to write as a file before patching: boot image + 2 ADV sectors.
pub fn ldlinux_file() -> Vec<u8> {
    let mut v = boot_image();
    v.extend_from_slice(&empty_adv());
    v
}

/// Port of `generate_extents()` (syslxmod.c): sector runs that ldlinux.sys's loader reads at
/// once; an extent never grows past 64 KiB nor crosses a 64 KiB boundary of its load address
/// (which starts at 0x8000). Each extent is { u64 lba; u16 len } packed (10 bytes).
fn generate_extents(area: &mut [u8], nptrs: usize, sectors: &[u64]) -> ForgeResult<()> {
    let need = nptrs.checked_mul(10).filter(|&n| n <= area.len()).ok_or_else(|| ForgeError::Invalid("syslinux: bad extent table".into()))?;
    area[..need].fill(0);
    let mut addr: u32 = 0x8000;
    let mut base = addr;
    let (mut lba, mut len) = (0u64, 0u32);
    let mut idx = 0usize;
    let mut put = |idx: &mut usize, lba: u64, len: u32| -> ForgeResult<()> {
        if *idx >= nptrs {
            return Err(ForgeError::Unsupported("syslinux: ldlinux.sys is too fragmented on the volume".into()));
        }
        wr64(area, *idx * 10, lba);
        wr16(area, *idx * 10 + 8, len as u16);
        *idx += 1;
        Ok(())
    };
    for &sect in sectors {
        if len > 0 {
            let xbytes = (len + 1) * SECTOR as u32;
            if sect == lba + len as u64 && xbytes < 65536 && ((addr ^ (base.wrapping_add(xbytes - 1))) & 0xffff_0000) == 0 {
                len += 1;
                addr = addr.wrapping_add(SECTOR as u32);
                continue;
            }
            put(&mut idx, lba, len)?;
        }
        base = addr;
        lba = sect;
        len = 1;
        addr = addr.wrapping_add(SECTOR as u32);
    }
    if len > 0 {
        put(&mut idx, lba, len)?;
    }
    Ok(())
}

/// Port of `syslinux_patch()` (syslxmod.c) for a FAT install: `sectors` are the volume-relative
/// sectors of ldlinux.sys (boot image + 2 ADV sectors), `subdir` the install directory
/// ("/boot/syslinux/"). Returns the patched boot image and the patched boot sector template
/// (ldlinux.bss with the first sector's address).
pub fn patch(sectors: &[u64], subdir: &str) -> ForgeResult<(Vec<u8>, Vec<u8>)> {
    let mut img = boot_image();
    let mut bss = LDLINUX_BSS.to_vec();
    if bss.len() != SECTOR {
        return Err(ForgeError::Invalid("syslinux: bad ldlinux.bss".into()));
    }
    let nsect = img.len() / SECTOR + 2;
    if sectors.len() < nsect {
        return Err(ForgeError::Invalid("syslinux: ldlinux.sys is smaller than expected".into()));
    }
    // Patch area: the dword-aligned LDLINUX_MAGIC.
    let pa = (0..img.len() / 4).map(|i| i * 4).find(|&o| rd32(&img, o) == LDLINUX_MAGIC).ok_or_else(|| ForgeError::Invalid("syslinux: no patch area in ldlinux.sys".into()))?;
    // struct patch_area { magic, instance, data_sectors u16, adv_sectors u16, dwords, checksum,
    //                     maxtransfer u16, epaoffset u16 }
    let epa = rd16(&img, pa + 22) as usize;
    let field = |o: usize| -> ForgeResult<usize> { img.get(epa + o..epa + o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).ok_or_else(|| ForgeError::Invalid("syslinux: bad extended patch area".into())) };
    // struct ext_patch_area { advptroffset, diroffset, dirlen, subvoloffset, subvollen,
    //                         secptroffset, secptrcnt, sect1ptr0, sect1ptr1, raidpatch }
    let (advptr, diroff, dirlen, secptr, secptrcnt, s1p0, s1p1) = (field(0)?, field(2)?, field(4)?, field(10)?, field(12)?, field(14)?, field(16)?);
    // First sector: pointer in the boot sector (LSW / MSW dwords).
    if s1p0 + 4 > SECTOR - 2 || s1p1 + 4 > SECTOR - 2 {
        return Err(ForgeError::Invalid("syslinux: bad boot sector pointer offsets".into()));
    }
    wr32(&mut bss, s1p0, sectors[0] as u32);
    wr32(&mut bss, s1p1, (sectors[0] >> 32) as u32);
    let rest = &sectors[1..nsect];
    let dw = img.len() / 4;
    wr16(&mut img, pa + 8, (nsect - 2) as u16); // data_sectors (not including the ADVs)
    wr16(&mut img, pa + 10, 2); // adv_sectors
    wr32(&mut img, pa + 12, dw as u32); // dwords
    // Sector extents: -1 for the boot sector's pointer, -2 for the ADVs.
    let n_ex = nsect - 1 - 2;
    let area_end = secptr.checked_add(secptrcnt * 10).filter(|&e| e <= img.len()).ok_or_else(|| ForgeError::Invalid("syslinux: bad extent table".into()))?;
    generate_extents(&mut img[secptr..area_end], secptrcnt, &rest[..n_ex])?;
    // ADV pointers (two u64).
    if advptr + 16 > img.len() {
        return Err(ForgeError::Invalid("syslinux: bad ADV pointer offset".into()));
    }
    wr64(&mut img, advptr, rest[n_ex]);
    wr64(&mut img, advptr + 8, rest[n_ex + 1]);
    // Install directory ("CurrentDirName"), NUL-terminated.
    let sub = subdir.as_bytes();
    if sub.len() + 1 > dirlen || diroff + dirlen > img.len() {
        return Err(ForgeError::Unsupported("syslinux: install directory path too long".into()));
    }
    img[diroff..diroff + sub.len()].copy_from_slice(sub);
    img[diroff + sub.len()] = 0;
    // Negative checksum over the boot image's complete dwords.
    wr32(&mut img, pa + 16, 0);
    let mut csum = LDLINUX_MAGIC;
    for i in 0..dw {
        csum = csum.wrapping_sub(rd32(&img, i * 4));
    }
    wr32(&mut img, pa + 16, csum);
    Ok((img, bss))
}

/// Port of `syslinux_make_bootsect()` for FAT: the boot code around the volume's BPB.
pub fn make_bootsect(volume_bs: &mut [u8], template: &[u8]) {
    // FAT_bsHead = jump + OEM name (offset 0, up to bsBytesPerSec); FAT_bsCode = bs32.Code
    // (offset 90 up to the 0x55AA signature at 510).
    volume_bs[0..11].copy_from_slice(&template[0..11]);
    volume_bs[90..510].copy_from_slice(&template[90..510]);
}

// ───────────────────────── a minimal FAT32 reader (sector map) ─────────────────────────

/// FAT32 geometry from the BPB (Microsoft FAT32 File System Specification, BPB / FAT32 EBPB).
struct Fat {
    spc: u64,
    reserved: u64,
    fat_size: u64,
    nfats: u64,
    root_cluster: u32,
    clusters: u32,
}

impl Fat {
    fn parse(bs: &[u8]) -> ForgeResult<Fat> {
        let bad = |m: &str| Err(ForgeError::Invalid(format!("syslinux: {m}")));
        // syslinux_check_bootsect(): media byte, 512-byte sectors, FAT32 signature.
        if bs[21] != 0xF0 && bs[21] < 0xF8 {
            return bad("not a FAT volume (media byte)");
        }
        if rd16(bs, 11) != 512 {
            return bad("only 512-byte sectors are supported");
        }
        if &bs[82..90] != b"FAT32   " || bs[66] != 0x29 {
            return bad("missing FAT32 signature");
        }
        let spc = bs[13] as u64;
        if spc == 0 || !spc.is_power_of_two() {
            return bad("impossible cluster size");
        }
        let reserved = rd16(bs, 14) as u64;
        let nfats = bs[16] as u64;
        let fat_size = rd32(bs, 36) as u64;
        let total = rd32(bs, 32) as u64;
        let data = reserved + nfats * fat_size;
        if reserved == 0 || nfats == 0 || fat_size == 0 || data >= total {
            return bad("bad FAT32 geometry");
        }
        let clusters = ((total - data) / spc).min(0x0FFF_FFF5) as u32;
        Ok(Fat { spc, reserved, fat_size, nfats, root_cluster: rd32(bs, 44), clusters })
    }

    fn data_start(&self) -> u64 {
        self.reserved + self.nfats * self.fat_size
    }

    fn cluster_sector(&self, c: u32) -> u64 {
        self.data_start() + (c as u64 - 2) * self.spc
    }

    fn next(&self, dev: &dyn BlockDev, c: u32) -> ForgeResult<Option<u32>> {
        let off = self.reserved * 512 + c as u64 * 4;
        let mut b = [0u8; 4];
        dev.read_at(off, &mut b)?;
        let v = u32::from_le_bytes(b) & 0x0FFF_FFFF;
        Ok((2..self.clusters + 2).contains(&v).then_some(v))
    }

    /// The cluster chain from `first` (bounded, loop-safe).
    fn chain(&self, dev: &dyn BlockDev, first: u32, max: usize) -> ForgeResult<Vec<u32>> {
        let mut out = vec![];
        let mut c = Some(first).filter(|c| (2..self.clusters + 2).contains(c));
        while let Some(cur) = c {
            if out.len() >= max {
                break;
            }
            out.push(cur);
            c = self.next(dev, cur)?;
        }
        Ok(out)
    }
}

/// A directory entry found by [`lookup`]: first cluster, size, byte offset of the entry.
struct Found {
    cluster: u32,
    size: u32,
    entry_off: u64,
}

/// The 8.3 or long name of the entries of directory `cluster`, matched case-insensitively.
fn lookup(dev: &dyn BlockDev, fat: &Fat, dir_cluster: u32, name: &str) -> ForgeResult<Option<Found>> {
    let clusters = fat.chain(dev, dir_cluster, 1 << 16)?;
    let mut lfn: Vec<(u8, [u16; 13])> = vec![];
    for c in clusters {
        let base = fat.cluster_sector(c) * 512;
        let data = dev.read_vec(base, (fat.spc * 512) as usize)?;
        for (k, e) in data.chunks_exact(32).enumerate() {
            if e[0] == 0 {
                return Ok(None);
            }
            if e[0] == 0xE5 {
                lfn.clear();
                continue;
            }
            if e[11] == 0x0F {
                let mut u = [0u16; 13];
                for (i, o) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].iter().enumerate() {
                    u[i] = rd16(e, *o);
                }
                lfn.push((e[0] & 0x1F, u));
                continue;
            }
            let short = {
                let base: String = e[0..8].iter().map(|&c| c as char).collect::<String>().trim_end().to_string();
                let ext: String = e[8..11].iter().map(|&c| c as char).collect::<String>().trim_end().to_string();
                if ext.is_empty() {
                    base
                } else {
                    format!("{base}.{ext}")
                }
            };
            let long = if lfn.is_empty() {
                None
            } else {
                lfn.sort_by_key(|(o, _)| *o);
                let units: Vec<u16> = lfn.iter().flat_map(|(_, u)| u.iter().copied()).take_while(|&c| c != 0 && c != 0xFFFF).collect();
                Some(String::from_utf16_lossy(&units))
            };
            lfn.clear();
            if e[11] & 0x08 != 0 {
                continue; // volume label
            }
            if short.eq_ignore_ascii_case(name) || long.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(name)) {
                let cluster = ((rd16(e, 20) as u32) << 16) | rd16(e, 26) as u32;
                return Ok(Some(Found { cluster, size: rd32(e, 28), entry_off: base + k as u64 * 32 }));
            }
        }
    }
    Ok(None)
}

/// Install SYSLINUX on the FAT32 volume `vol` (a partition view) whose `dir`/ldlinux.sys was
/// just written with [`ldlinux_file`] (and the file system synced and closed). `part_lba` is
/// the partition's first sector on the disk (recorded as the BPB's hidden sectors).
pub fn install(vol: &dyn BlockDev, part_lba: u64, dir: &str) -> ForgeResult<()> {
    let mut bs = vol.read_vec(0, 512)?;
    let fat = Fat::parse(&bs)?;
    let mut cluster = fat.root_cluster;
    let comps: Vec<&str> = dir.split('/').filter(|c| !c.is_empty()).collect();
    for c in &comps {
        cluster = lookup(vol, &fat, cluster, c)?.ok_or_else(|| ForgeError::Invalid(format!("syslinux: {dir} not found on the volume")))?.cluster;
    }
    let f = lookup(vol, &fat, cluster, "ldlinux.sys")?.ok_or_else(|| ForgeError::Invalid("syslinux: ldlinux.sys not found on the volume".into()))?;
    let expect = ldlinux_file();
    if f.size as usize != expect.len() {
        return Err(ForgeError::Invalid(format!("syslinux: ldlinux.sys has {} bytes, expected {}", f.size, expect.len())));
    }
    // Sector map of the file (libfat_clustertosector / libfat_nextsector).
    let nsect = expect.len() / SECTOR;
    let chain = fat.chain(vol, f.cluster, nsect.div_ceil(fat.spc as usize) + 1)?;
    let sectors: Vec<u64> = chain.iter().flat_map(|&c| (0..fat.spc).map(move |k| (c, k))).map(|(c, k)| fat.cluster_sector(c) + k).take(nsect).collect();
    if sectors.len() < nsect {
        return Err(ForgeError::Invalid("syslinux: ldlinux.sys cluster chain is too short".into()));
    }
    let subdir = if comps.is_empty() { "/".to_string() } else { format!("/{}/", comps.join("/")) };
    let (img, template) = patch(&sectors, &subdir)?;
    // Write the patched sectors of the boot image (the ADV sectors are already right).
    for (i, s) in sectors.iter().enumerate().take(img.len() / SECTOR) {
        vol.write_at(s * 512, &img[i * SECTOR..(i + 1) * SECTOR])?;
    }
    // ldlinux.sys read-only + hidden + system, as the installers do (keeps it from moving).
    let mut attr = [0u8; 1];
    vol.read_at(f.entry_off + 11, &mut attr)?;
    vol.write_at(f.entry_off + 11, &[attr[0] | 0x07])?;
    // Boot sector: SYSLINUX code around the BPB; hidden sectors = the partition's offset.
    make_bootsect(&mut bs, &template);
    wr32(&mut bs, 28, part_lba.min(u32::MAX as u64) as u32);
    vol.write_at(0, &bs)?;
    // The backup boot sector (FAT32 BPB_BkBootSec) keeps a consistent BPB.
    let bk = rd16(&bs, 50) as u64;
    if bk > 0 && bk < fat.reserved {
        let mut b2 = vol.read_vec(bk * 512, 512)?;
        wr32(&mut b2, 28, part_lba.min(u32::MAX as u64) as u32);
        vol.write_at(bk * 512, &b2)?;
    }
    vol.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{MemDev, PartitionDev};
    use crate::forge::tree::{read_file, write_file};
    use crate::vfs::{self, FsType, MountDefaults};
    use std::sync::Arc;

    #[test]
    fn versions_are_read_from_isolinux_binaries() {
        let mut b = vec![0u8; 100];
        b.extend_from_slice(b"ISOLINUX 6.04 6.04-pre1  ETCD Copyright");
        assert_eq!(detect_version(&b).map(|v| (v.0, v.1)), Some((6, 4)));
        assert_eq!(detect_version(b"xxISOLINUX 4.07 2013-07-25 ETCD").map(|v| (v.0, v.1)), Some((4, 7)));
        assert_eq!(detect_version(LDLINUX_SYS).map(|v| (v.0, v.1)), Some((6, 3)), "the embedded release is 6.03");
        assert!(detect_version(b"nothing here").is_none());
        assert!(module("VESAMENU.C32").is_some() && module("ldlinux.c32").is_some());
    }

    #[test]
    fn empty_adv_is_consistent() {
        let a = empty_adv();
        // adv_consistent(): magic words and the checksum over bytes 4..ADV_SIZE-4.
        for copy in a.chunks(ADV_SIZE) {
            assert_eq!(rd32(copy, 0), ADV_MAGIC1);
            assert_eq!(rd32(copy, ADV_SIZE - 4), ADV_MAGIC3);
            let mut csum = 0u32;
            for i in (4..ADV_SIZE - 4).step_by(4) {
                csum = csum.wrapping_add(rd32(copy, i));
            }
            assert_eq!(csum, ADV_MAGIC2);
        }
    }

    #[test]
    fn extents_follow_the_installer_rules() {
        let mut area = vec![0u8; 10 * 8];
        // 0..130 contiguous: split at 64 KiB of load address (0x8000 + 64 sectors = 0x10000).
        let s: Vec<u64> = (1000..1130).collect();
        generate_extents(&mut area, 8, &s).unwrap();
        let ex: Vec<(u64, u16)> = (0..8).map(|i| (u64::from_le_bytes(area[i * 10..i * 10 + 8].try_into().unwrap()), rd16(&area, i * 10 + 8))).filter(|e| e.1 > 0).collect();
        assert!(ex.len() >= 2, "{ex:?}");
        let mut next = 1000u64;
        for &(lba, len) in &ex {
            assert_eq!(lba, next, "runs in file order, contiguous: {ex:?}");
            assert!((len as u32) * 512 < 65536);
            next += len as u64;
        }
        assert_eq!(next, 1130);
        // Fragmented beyond the table: refused, not overflowed.
        let frag: Vec<u64> = (0..20).map(|i| i * 10).collect();
        assert!(generate_extents(&mut area, 8, &frag).is_err());
    }

    /// Install on a freshly formatted FAT32 partition and check everything the BIOS loader
    /// relies on: boot code around an intact BPB, the first-sector pointer, the extent map,
    /// the ADV pointers, the install directory and the checksum.
    #[test]
    fn install_patches_boot_sector_and_ldlinux_sys() {
        let disk: Arc<dyn BlockDev> = Arc::new(MemDev::new(80 << 20));
        let part_lba = 2048u64;
        let part: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(disk.clone(), part_lba * 512, (80 << 20) - part_lba * 512).unwrap());
        vfs::format(part.clone(), FsType::Fat32, "LIVE").unwrap();
        let before = part.read_vec(0, 512).unwrap();
        {
            let mut fs = vfs::open_fs(part.clone(), false, MountDefaults::default()).unwrap();
            write_file(fs.as_mut(), "/boot/filler.bin", &vec![1u8; 300_000]).unwrap();
            write_file(fs.as_mut(), "/boot/syslinux/ldlinux.sys", &ldlinux_file()).unwrap();
            fs.sync().unwrap();
        }
        install(part.as_ref(), part_lba, "/boot/syslinux").unwrap();
        let bs = part.read_vec(0, 512).unwrap();
        assert_eq!(&bs[11..28], &before[11..28], "BPB preserved");
        assert_eq!(&bs[32..90], &before[32..90], "BPB preserved");
        let differ = (90..510).filter(|&i| bs[i] != LDLINUX_BSS[i]).count();
        assert!(differ <= 8, "boot code = ldlinux.bss except the first-sector pointer ({differ} bytes differ)");
        assert_eq!(&bs[3..11], &LDLINUX_BSS[3..11], "OEM name from ldlinux.bss");
        assert_eq!(rd32(&bs, 28), part_lba as u32, "hidden sectors");
        assert_eq!((bs[510], bs[511]), (0x55, 0xAA));
        // The file as the FS sees it is the patched image: check the patch area.
        let mut fs = vfs::open_fs(part.clone(), true, MountDefaults::default()).unwrap();
        let file = read_file(fs.as_mut(), "/boot/syslinux/ldlinux.sys").unwrap().unwrap();
        let img_len = boot_image().len();
        let pa = (0..img_len / 4).map(|i| i * 4).find(|&o| rd32(&file, o) == LDLINUX_MAGIC).unwrap();
        let sum = (0..img_len / 4).fold(0u32, |s, i| s.wrapping_add(rd32(&file, i * 4)));
        assert_eq!(sum, LDLINUX_MAGIC, "negative checksum makes the dword sum equal the magic");
        assert_eq!(rd16(&file, pa + 8) as usize, img_len / 512, "data sectors");
        assert_eq!(rd16(&file, pa + 10), 2, "ADV sectors");
        let epa = rd16(&file, pa + 22) as usize;
        let diroff = rd16(&file, epa + 2) as usize;
        assert_eq!(&file[diroff..diroff + 16], b"/boot/syslinux/\0");
        // First sector pointer in the boot sector, then extents covering the rest in order.
        let s1 = rd32(&bs, rd16(&file, epa + 14) as usize) as u64;
        let secptr = rd16(&file, epa + 10) as usize;
        let mut lbas = vec![s1];
        for i in 0..rd16(&file, epa + 12) as usize {
            let lba = u64::from_le_bytes(file[secptr + i * 10..secptr + i * 10 + 8].try_into().unwrap());
            let len = rd16(&file, secptr + i * 10 + 8) as u64;
            lbas.extend(lba..lba + len);
        }
        assert_eq!(lbas.len(), img_len / 512);
        // Those sectors hold the file's bytes, in order.
        for (i, s) in lbas.iter().enumerate() {
            assert_eq!(part.read_vec(s * 512, 512).unwrap(), file[i * 512..(i + 1) * 512].to_vec(), "sector {i}");
        }
        let advp = rd16(&file, epa) as usize;
        let adv0 = u64::from_le_bytes(file[advp..advp + 8].try_into().unwrap());
        assert_eq!(part.read_vec(adv0 * 512, 4).unwrap(), ADV_MAGIC1.to_le_bytes().to_vec());
        // Still a valid FAT32 volume with the file hidden/system/read-only.
        assert!(read_file(fs.as_mut(), "/boot/filler.bin").unwrap().is_some());
    }
}
