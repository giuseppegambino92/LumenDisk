//! ISO mode on synthetic ISOs (made with `iso::testimg`); real distributions are booted in QEMU
//! by `tests/iso_linux_e2e.rs`.

use super::*;
use crate::block::MemDev;
use crate::forge::plan::{LinuxOptions, WriteMode};
use crate::forge::target::{open_for_write, TargetSpec};
use crate::iso::testimg::iso::IsoBuilder;

fn isolinux_bin(ver: &str) -> Vec<u8> {
    let mut b = vec![0x90u8; 2048];
    let s = format!("ISOLINUX {ver} ETCD Copyright (C) 1994-2015 H. Peter Anvin et al");
    b[64..64 + s.len()].copy_from_slice(s.as_bytes());
    b
}

const LABEL: &str = "d-live 13.7.0 st amd64";

fn debian_like() -> IsoBuilder {
    IsoBuilder::new(LABEL)
        .file("/.disk/info", b"Official Debian GNU/Linux Live 13.7.0 standard\n".to_vec())
        .file("/isolinux/isolinux.bin", isolinux_bin("6.04 20250413"))
        .file("/isolinux/isolinux.cfg", b"include menu.cfg\ndefault vesamenu.c32\nprompt 0\ntimeout 0\n".to_vec())
        .file("/isolinux/live.cfg", b"label live-amd64\n\tlinux /live/vmlinuz\n\tinitrd /live/initrd.img\n\tappend boot=live components quiet splash\n".to_vec())
        .file("/isolinux/vesamenu.c32", b"old 6.04 module".to_vec())
        .file("/isolinux/ldlinux.c32", b"old 6.04 ldlinux".to_vec())
        .file("/isolinux/weird.c32", b"no such module in 6.03".to_vec())
        .file("/boot/grub/grub.cfg", format!("search --label \"{LABEL}\" --set root\nmenuentry \"Live\" {{\n\tlinux\t/live/vmlinuz boot=live components quiet\n\tinitrd\t/live/initrd.img\n}}\n").into_bytes())
        .file("/EFI/boot/bootx64.efi", vec![0x4D, 0x5A, 1, 2, 3])
        .file("/live/vmlinuz", vec![7u8; 100_000])
        .file("/live/initrd.img", vec![8u8; 50_000])
        .file("/live/filesystem.squashfs", vec![9u8; 300_000])
        .bios_boot("/isolinux/isolinux.bin")
}

fn src(b: IsoBuilder) -> Arc<dyn BlockDev> {
    Arc::new(MemDev::from_vec(b.build().to_vec()))
}

fn request(path: &std::path::Path, linux: LinuxOptions) -> FlashRequest {
    FlashRequest {
        source: "test.iso".into(),
        target: TargetSpec::File { path: path.into(), size: None },
        mode: WriteMode::Iso,
        verify: true,
        expect_hash: None,
        windows: None,
        linux: Some(linux),
        rpi: None,
        eject: false,
    }
}

struct Out {
    dir: std::path::PathBuf,
    img: std::path::PathBuf,
}

impl Out {
    fn new(size: u64) -> Out {
        let dir = std::env::temp_dir().join(format!("ld-isomode-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("usb.img");
        std::fs::File::create(&img).unwrap().set_len(size).unwrap();
        Out { dir, img }
    }
    fn run(&self, iso: Arc<dyn BlockDev>, linux: LinuxOptions) -> ForgeResult<Value> {
        let ctx = JobCtx::detached(false);
        let r = request(&self.img, linux);
        let t = open_for_write(&r.target, &ctx)?;
        write(&r, iso, &t, &ctx)
    }
    fn disk(&self) -> Arc<dyn BlockDev> {
        crate::block::open_source(&self.img.to_string_lossy(), true, None).unwrap()
    }
}

impl Drop for Out {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn part_fs(disk: &Arc<dyn BlockDev>, idx: usize) -> Box<dyn FileSystem> {
    let parts = crate::block::parse_partitions(disk.as_ref()).unwrap();
    let p = &parts[idx];
    let d: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(disk.clone(), p.start, p.size).unwrap());
    vfs::open_fs(d, true, MountDefaults::default()).unwrap()
}

fn text(fs: &mut dyn FileSystem, p: &str) -> String {
    String::from_utf8(tree::read_file(fs, p).unwrap().unwrap_or_else(|| panic!("{p} missing"))).unwrap()
}

#[test]
fn detection_of_distributions_and_boot_capabilities() {
    let mut t = crate::iso::open(src(debian_like())).unwrap();
    let i = detect(&mut t).unwrap().unwrap();
    assert!(i.distro.starts_with("Official Debian"), "{}", i.distro);
    assert_eq!(i.persistence.as_deref(), Some("live-boot"));
    assert_eq!(i.persistence_label.as_deref(), Some("persistence"));
    assert!(i.has_efi && i.has_isolinux && i.has_grub && i.bios_iso_mode);
    assert!(i.syslinux_version.as_deref().unwrap().starts_with("6.04 20250413"), "{:?}", i.syslinux_version);
    let ubuntu = IsoBuilder::new("Ubuntu 18.04.6 LTS amd64")
        .file("/.disk/info", b"Ubuntu 18.04.6 LTS \"Bionic Beaver\" - Release amd64 (20210915)".to_vec())
        .file("/casper/vmlinuz", vec![1])
        .file("/boot/grub/grub.cfg", b"linux /casper/vmlinuz boot=casper\n".to_vec());
    let i = detect(&mut crate::iso::open(src(ubuntu)).unwrap()).unwrap().unwrap();
    assert_eq!((i.persistence.as_deref(), i.persistence_label.as_deref()), (Some("casper"), Some("casper-rw")));
    assert!(!i.bios_iso_mode && i.bios_note.as_deref().unwrap().contains("DD"), "GRUB-only for BIOS: {:?}", i.bios_note);
    let noble = IsoBuilder::new("Ubuntu 24.04.1 LTS amd64").file("/.disk/info", b"Ubuntu 24.04.1 LTS \"Noble Numbat\"".to_vec()).file("/casper/vmlinuz", vec![1]);
    assert_eq!(detect(&mut crate::iso::open(src(noble)).unwrap()).unwrap().unwrap().persistence_label.as_deref(), Some("writable"));
    let win = IsoBuilder::new("CCCOMA_X64FRE_EN-US_DV9").file("/bootmgr", vec![1]).file("/sources/install.wim", vec![1]).file("/efi/boot/bootx64.efi", vec![1]);
    assert!(detect(&mut crate::iso::open(src(win)).unwrap()).unwrap().is_none());
    let old = IsoBuilder::new("OLD").file("/isolinux/isolinux.bin", isolinux_bin("4.07 2013-07-25")).file("/isolinux/isolinux.cfg", b"default linux\n".to_vec());
    let i = detect(&mut crate::iso::open(src(old)).unwrap()).unwrap().unwrap();
    assert!(!i.bios_iso_mode && i.bios_note.unwrap().contains("4.07"));
}

#[test]
fn iso_mode_bios_and_uefi_with_live_boot_persistence() {
    let out = Out::new(256 << 20);
    let res = out.run(src(debian_like()), LinuxOptions { persistence_size: Some(64 << 20), ..Default::default() }).unwrap();
    assert_eq!(res["scheme"], "mbr");
    assert_eq!(res["label"], "D-LIVE 1370", "dots are not valid in FAT labels");
    assert_eq!(res["persistence"]["label"], "persistence");
    assert!(res["bytes_verified"].as_u64().unwrap() > 400_000);
    let disk = out.disk();
    // MBR: SYSLINUX boot code, partition 1 active FAT32 (LBA), partition 2 Linux.
    let mbr = disk.read_vec(0, 512).unwrap();
    assert_eq!(&mbr[..440], &syslinux::MBR_BIN[..440]);
    assert_eq!((mbr[446], mbr[446 + 4]), (0x80, 0x0C));
    assert_eq!((mbr[462], mbr[462 + 4]), (0x00, 0x83));
    let parts = crate::block::parse_partitions(disk.as_ref()).unwrap();
    assert_eq!(parts.len(), 2);
    assert!(parts[1].size >= 63 << 20);
    let mut fs = part_fs(&disk, 0);
    assert_eq!(fs.volume_info().label, "D-LIVE 1370");
    assert_eq!(tree::read_file(fs.as_mut(), "/live/filesystem.squashfs").unwrap().unwrap(), vec![9u8; 300_000]);
    // Boot configurations: persistence added, label references rewritten.
    let live = text(fs.as_mut(), "/isolinux/live.cfg");
    assert!(live.contains("append boot=live persistence components"), "{live}");
    let grub = text(fs.as_mut(), "/boot/grub/grub.cfg");
    assert!(grub.contains("search --label \"D-LIVE 1370\""), "{grub}");
    assert!(grub.contains("boot=live persistence components"), "{grub}");
    assert_eq!(text(fs.as_mut(), "/isolinux/syslinux.cfg"), text(fs.as_mut(), "/isolinux/isolinux.cfg"));
    // Modules replaced with the 6.03 builds; unknown ones kept.
    assert_eq!(tree::read_file(fs.as_mut(), "/isolinux/vesamenu.c32").unwrap().unwrap(), syslinux::module("vesamenu.c32").unwrap());
    assert_eq!(tree::read_file(fs.as_mut(), "/isolinux/ldlinux.c32").unwrap().unwrap(), syslinux::module("ldlinux.c32").unwrap());
    assert_eq!(tree::read_file(fs.as_mut(), "/isolinux/weird.c32").unwrap().unwrap(), b"no such module in 6.03");
    assert!(res["warnings"].to_string().contains("weird.c32"));
    // SYSLINUX boot sector on the FAT volume, hidden sectors = partition start.
    let p0: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(disk.clone(), parts[0].start, parts[0].size).unwrap());
    let bs = p0.read_vec(0, 512).unwrap();
    assert_eq!(&bs[3..11], b"SYSLINUX");
    assert_eq!(u32::from_le_bytes(bs[28..32].try_into().unwrap()) as u64, parts[0].start / 512);
    let ld = tree::read_file(fs.as_mut(), "/isolinux/ldlinux.sys").unwrap().unwrap();
    assert_eq!(ld.len(), syslinux::ldlinux_file().len());
    assert_ne!(ld, syslinux::ldlinux_file(), "patched in place");
    // Persistence partition: ext4 labelled "persistence" with persistence.conf.
    let mut pfs = part_fs(&disk, 1);
    assert_eq!(pfs.volume_info().label, "persistence");
    assert_eq!(text(pfs.as_mut(), "/persistence.conf"), "/ union\n");
}

#[test]
fn uefi_only_is_gpt_without_bios_code_and_label_that_fits_is_kept() {
    let out = Out::new(128 << 20);
    // A label that fits FAT (11 upper-case characters): no rewriting needed.
    let b = IsoBuilder::new("ARCH_202411").file("/boot/grub/grub.cfg", b"linux /arch/vmlinuz archisolabel=ARCH_202411\n".to_vec()).file("/EFI/BOOT/BOOTX64.EFI", vec![1, 2, 3]);
    let res = out.run(src(b), LinuxOptions { boot: Some(BootTarget::Uefi), ..Default::default() }).unwrap();
    assert_eq!(res["scheme"], "gpt");
    assert_eq!(res["label"], "ARCH_202411");
    assert_eq!(res["patches"].as_array().unwrap().len(), 0, "{res}");
    let disk = out.disk();
    assert!(disk.read_vec(0, 440).unwrap().iter().all(|&b| b == 0), "no BIOS boot code");
    let parts = crate::block::parse_partitions(disk.as_ref()).unwrap();
    assert_eq!(parts[0].scheme, "GPT");
    let mut fs = part_fs(&disk, 0);
    assert!(tree::read_file(fs.as_mut(), "/EFI/BOOT/BOOTX64.EFI").unwrap().is_some());
    assert!(resolve(fs.as_mut(), "/ldlinux.sys").unwrap().is_none());
}

#[test]
fn efi_loader_is_extracted_from_the_el_torito_image() {
    // A FAT12 EFI image holding the loader; the ISO has no /EFI directory.
    let efi: Arc<dyn BlockDev> = Arc::new(MemDev::new(2 << 20));
    vfs::format(efi.clone(), crate::vfs::FsType::Fat12, "EFIBOOT").unwrap();
    {
        let mut fs = vfs::open_fs(efi.clone(), false, MountDefaults::default()).unwrap();
        tree::write_file(fs.as_mut(), "/EFI/BOOT/BOOTX64.EFI", b"MZ loader").unwrap();
        tree::write_file(fs.as_mut(), "/EFI/BOOT/grub.cfg", b"configfile /boot/grub/grub.cfg").unwrap();
        fs.sync().unwrap();
    }
    let img = efi.read_vec(0, 2 << 20).unwrap();
    let b = IsoBuilder::new("ALPINE").file("/boot/grub/efi.img", img).file("/boot/grub/grub.cfg", b"linux /boot/vmlinuz\n".to_vec()).efi_boot("/boot/grub/efi.img", Some(1));
    let out = Out::new(96 << 20);
    let res = out.run(src(b), LinuxOptions { boot: Some(BootTarget::Uefi), ..Default::default() }).unwrap();
    assert!(res["bootloaders"].to_string().contains("El Torito"), "{res}");
    let disk = out.disk();
    let mut fs = part_fs(&disk, 0);
    assert_eq!(tree::read_file(fs.as_mut(), "/EFI/BOOT/BOOTX64.EFI").unwrap().unwrap(), b"MZ loader");
}

#[test]
fn refusals_are_clear_and_happen_before_writing() {
    // GRUB-only ISO asked to boot on BIOS.
    let grub_only = IsoBuilder::new("X").file("/boot/grub/grub.cfg", b"linux /casper/vmlinuz\n".to_vec()).file("/casper/vmlinuz", vec![1]).file("/EFI/BOOT/BOOTX64.EFI", vec![1]);
    let out = Out::new(96 << 20);
    let e = out.run(src(grub_only.clone()), LinuxOptions { boot: Some(BootTarget::Bios), ..Default::default() }).unwrap_err();
    assert!(e.to_string().contains("DD"), "{e}");
    assert!(out.disk().read_vec(0, 1 << 20).unwrap().iter().all(|&b| b == 0), "nothing written");
    // Both: UEFI only, with a warning.
    let res = out.run(src(grub_only), LinuxOptions::default()).unwrap();
    assert!(res["warnings"].to_string().contains("DD"), "{res}");
    // Persistence on an ISO without support.
    let plain = IsoBuilder::new("P").file("/EFI/BOOT/BOOTX64.EFI", vec![1]).file("/boot/grub/grub.cfg", vec![]);
    let e = out.run(src(plain), LinuxOptions { persistence_size: Some(64 << 20), ..Default::default() }).unwrap_err();
    assert!(e.to_string().contains("persisten"), "{e}");
    // A file over 4 GiB: FAT32 can't hold it, DD mode is suggested.
    let dir = std::env::temp_dir().join(format!("ld-isomode-big-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.iso");
    IsoBuilder::new("BIG").file("/EFI/BOOT/BOOTX64.EFI", vec![1]).sparse_file("/live/filesystem.squashfs", 5 << 30, vec![]).build().write_sparse(&path).unwrap();
    let big = crate::block::open_source(&path.to_string_lossy(), true, None).unwrap();
    let e = out.run(big, LinuxOptions::default()).unwrap_err();
    assert!(e.to_string().contains("4 GB") && e.to_string().contains("DD"), "{e}");
    std::fs::remove_dir_all(&dir).unwrap();
    // Too small a disk.
    let tiny = Out::new(40 << 20);
    let e = tiny.run(src(debian_like().file("/big.bin", vec![0u8; 50 << 20])), LinuxOptions::default()).unwrap_err();
    assert!(e.to_string().contains("too small") || e.to_string().contains("troppo piccolo"), "{e}");
}
