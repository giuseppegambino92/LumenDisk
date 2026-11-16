//! Unit tests of the whole Windows pipeline on synthetic media (no ISO, no Docker): trees in
//! memory and in folders, written to image files, checked with LumenDisk's own drivers.

use super::wim::tests::{synth_wim, MemTree, XML};
use super::*;
use crate::block::parse_partitions;
use crate::forge::plan::LocalAccount;
use crate::forge::target::{open_for_write, TargetSpec};
use crate::forge::tree::DirTree;

fn media(install: Vec<u8>, name: &str) -> MemTree {
    MemTree(vec![
        ("/bootmgr".into(), vec![0xB0; 5000]),
        ("/efi/boot/bootx64.efi".into(), vec![0xEF; 3000]),
        ("/efi/microsoft/boot/bcd".into(), vec![1; 700]),
        ("/sources/boot.wim".into(), synth_wim(&[1000], 1, XML, 1)),
        ("/sources/idwbinfo.txt".into(), b"[BUILDINFO]\r\nBuildArch=amd64\r\nBuildFull=26100.1.amd64fre.ge_release.240331-1435\r\n".to_vec()),
        (format!("/sources/{name}"), install),
        ("/setup.exe".into(), vec![0x4D; 1234]),
    ])
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ld-win-{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn request(windows: WindowsOptions) -> FlashRequest {
    FlashRequest {
        source: "/x.iso".into(),
        target: TargetSpec::File { path: "/unused".into(), size: None },
        mode: crate::forge::plan::WriteMode::Windows,
        verify: true,
        expect_hash: None,
        windows: Some(windows),
        linux: None,
        rpi: None,
        eject: false,
    }
}

fn small() -> Tuning {
    Tuning { max_file: 40_000, part_limit: 25_000 }
}

fn open_part(path: &std::path::Path, n: usize) -> (Arc<dyn BlockDev>, Box<dyn FileSystem>, u64) {
    let dev = crate::block::open_source(&path.to_string_lossy(), true, None).unwrap();
    let p = &parse_partitions(dev.as_ref()).unwrap()[n];
    let pd: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(dev.clone(), p.start, p.size).unwrap());
    let fs = vfs::open_fs(pd.clone(), true, MountDefaults::default()).unwrap();
    (pd, fs, p.start)
}

#[test]
fn version_names() {
    assert_eq!(version_name(Some(26100), false, &[]), "Windows 11");
    assert_eq!(version_name(Some(19045), false, &[]), "Windows 10");
    assert_eq!(version_name(Some(9600), false, &[]), "Windows 8.1");
    assert_eq!(version_name(Some(7601), false, &[]), "Windows 7");
    assert_eq!(version_name(Some(20348), true, &[]), "Windows Server 2022");
    assert_eq!(version_name(Some(26100), true, &[]), "Windows Server 2025");
    assert_eq!(version_name(None, false, &["Windows 10 Pro".into()]), "Windows 10");
    assert_eq!(version_name(Some(9600), true, &["Windows Server 2012 R2 SERVERSTANDARD".into()]), "Windows Server 2012 R2");
    assert_eq!(version_name(None, false, &[]), "Windows");
}

#[test]
fn detect_reads_version_arch_editions_and_recommends_a_layout() {
    let wim = synth_wim(&[15_000, 14_000, 12_000], 2, XML, 0);
    let mut t = media(wim.clone(), "install.wim");
    let i = detect(&mut t).unwrap().unwrap();
    assert_eq!(i.version, "Windows 11");
    assert_eq!(i.arch, "x64");
    assert_eq!(i.build, Some(26100));
    assert_eq!(i.install_image, "/sources/install.wim");
    assert_eq!(i.install_image_size, wim.len() as u64);
    assert_eq!(i.editions, vec!["Windows 11 Home", "Windows 11 Pro"]);
    assert_eq!(i.editions_detail[1].edition_id, "Professional");
    assert_eq!(i.languages, vec!["it-IT"]);
    assert!(i.bios_boot);
    assert_eq!(i.uefi_boot, vec!["x64"]);
    assert!(!i.needs_split);
    assert_eq!(i.recommended_layout, Some(WindowsLayout::Fat32Split));

    let entries = t.entries().unwrap();
    let m = analyse(&mut t, &entries, &small()).unwrap().unwrap();
    assert!(m.info.needs_split && m.info.split_blocker.is_none());
    assert_eq!(m.info.recommended_layout, Some(WindowsLayout::Fat32Split));

    let mut esd = wim.clone();
    esd[12..16].copy_from_slice(&wim::VERSION_SOLID.to_le_bytes());
    let mut t = media(esd, "install.esd");
    let entries = t.entries().unwrap();
    let m = analyse(&mut t, &entries, &small()).unwrap().unwrap();
    assert!(m.info.split_blocker.is_some());
    assert_eq!(m.info.recommended_layout, Some(WindowsLayout::NtfsUefi));

    // Not Windows.
    let mut t = MemTree(vec![("/casper/vmlinuz".into(), vec![1; 10]), ("/efi/boot/bootx64.efi".into(), vec![2; 10])]);
    assert!(detect(&mut t).unwrap().is_none());
}

#[test]
fn fat32_layout_with_a_split_wim_boots_both_ways_on_paper() {
    let dir = tmp("fat32");
    let img = dir.join("disk.img");
    let wim = synth_wim(&[12_000, 15_000, 9_000, 11_000], 2, XML, 1);
    let mut t = media(wim.clone(), "install.wim");
    let ctx = JobCtx::detached(false);
    let target = open_for_write(&TargetSpec::File { path: img.clone(), size: Some(96 << 20) }, &ctx).unwrap();
    let opts = WindowsOptions { bypass_hardware_checks: true, local_account: Some(LocalAccount { name: "mario".into(), password: "pw".into() }), ..Default::default() };
    let r = write_tree_tuned(&request(opts), &mut t, &target, &ctx, &small()).unwrap();
    drop(target);
    assert_eq!(r["layout"], "fat32_split");
    assert_eq!(r["scheme"], "mbr");
    assert_eq!(r["label"], "ESD-USB");
    let parts = r["split"]["parts"].as_array().unwrap();
    assert!(parts.len() >= 2, "{r}");
    assert_eq!(parts[0]["name"], "install.swm");
    assert_eq!(parts[1]["name"], "install2.swm");
    let snap = ctx.snapshot(0);
    assert_eq!(snap.phases, vec![Phase::Preparing, Phase::Partitioning, Phase::Formatting, Phase::Extracting, Phase::Writing, Phase::Customizing, Phase::Verifying]);

    let dev = crate::block::open_source(&img.to_string_lossy(), true, None).unwrap();
    assert!(bootrec::has_win7_mbr(dev.as_ref()));
    let pt = parse_partitions(dev.as_ref()).unwrap();
    assert_eq!(pt.len(), 1);
    let mbr = dev.read_vec(0, 512).unwrap();
    assert_eq!((mbr[446], mbr[450]), (0x80, 0x0C), "active FAT32 LBA");
    let (pd, mut fs, start) = open_part(&img, 0);
    assert!(bootrec::has_fat32(pd.as_ref(), start / 512));
    assert_eq!(tree::read_file(fs.as_mut(), "/bootmgr").unwrap().unwrap(), vec![0xB0; 5000]);
    assert_eq!(tree::read_file(fs.as_mut(), "/EFI/BOOT/BOOTX64.EFI").unwrap().unwrap(), vec![0xEF; 3000]);
    assert!(tree::read_file(fs.as_mut(), "/sources/install.wim").unwrap().is_none(), "replaced by the parts");
    let xml = String::from_utf8(tree::read_file(fs.as_mut(), "/autounattend.xml").unwrap().unwrap()).unwrap();
    assert!(xml.contains("BypassTPMCheck") && xml.contains("<Name>mario</Name>") && xml.contains("processorArchitecture=\"amd64\""));
    // The parts reassemble into the original blobs.
    let mut total_blobs = 0;
    for (k, p) in parts.iter().enumerate() {
        let data = tree::read_file(fs.as_mut(), &format!("/sources/{}", p["name"].as_str().unwrap())).unwrap().unwrap();
        let h = wim::WimHeader::parse(&data).unwrap();
        assert_eq!((h.part_number as usize, h.total_parts as usize), (k + 1, parts.len()));
        assert!(data.len() as u64 <= small().part_limit);
        let mut mt = MemTree(vec![("/p".into(), data.clone())]);
        let w = wim::WimFile::open(&mut mt, "/p", data.len() as u64).unwrap();
        total_blobs += w.blob_table(&mut mt).unwrap().len();
    }
    assert_eq!(total_blobs, 6);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ntfs_layout_has_uefi_ntfs_at_the_end_and_keeps_big_files_whole() {
    for boot in [BootTarget::Both, BootTarget::Uefi] {
        let dir = tmp("ntfs");
        let img = dir.join("disk.img");
        let wim = synth_wim(&[30_000, 25_000], 2, XML, 0);
        let mut t = media(wim.clone(), "install.wim");
        let ctx = JobCtx::detached(false);
        let size = 120u64 << 20;
        let target = open_for_write(&TargetSpec::File { path: img.clone(), size: Some(size) }, &ctx).unwrap();
        let opts = WindowsOptions { layout: Some(WindowsLayout::NtfsUefi), boot: Some(boot), label: Some("Win11 Setup".into()), ..Default::default() };
        let r = write_tree_tuned(&request(opts), &mut t, &target, &ctx, &small()).unwrap();
        drop(target);
        assert_eq!(r["layout"], "ntfs_uefi");
        assert_eq!(r["label"], "Win11 Setup");
        assert!(r["split"].is_null() && r["autounattend"].is_null());
        let dev = crate::block::open_source(&img.to_string_lossy(), true, None).unwrap();
        let pt = parse_partitions(dev.as_ref()).unwrap();
        assert_eq!(pt.len(), 2);
        assert_eq!(pt[1].size, uefi_ntfs::PARTITION_SIZE);
        let tail = if boot == BootTarget::Uefi { 16384 + 512 } else { 0 };
        assert!(size - tail - (pt[1].start + pt[1].size) < (1 << 20), "UEFI:NTFS at the end of the disk");
        let p2 = PartitionDev::new(dev.clone(), pt[1].start, pt[1].size).unwrap();
        assert!(uefi_ntfs::present(&p2));
        let (pd, mut fs, start) = open_part(&img, 0);
        assert_eq!(tree::read_file(fs.as_mut(), "/sources/install.wim").unwrap().unwrap(), wim, "copied whole");
        let mbr = dev.read_vec(0, 512).unwrap();
        if boot == BootTarget::Both {
            assert!(bootrec::has_win7_mbr(dev.as_ref()));
            assert!(bootrec::has_ntfs(pd.as_ref(), start / 512));
            assert_eq!((mbr[446], mbr[450], mbr[462], mbr[466]), (0x80, 0x07, 0x00, 0xEF));
        } else {
            assert_eq!(mbr[450], 0xEE, "protective MBR");
            assert_eq!(pt[1].name, "UEFI:NTFS");
            assert!(!bootrec::has_ntfs(pd.as_ref(), start / 512), "no BIOS code on a UEFI-only disk");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn an_esd_too_big_for_fat32_goes_to_ntfs_or_is_refused_untouched() {
    let wim = synth_wim(&[30_000, 25_000], 1, XML, 0);
    let mut esd = wim.clone();
    esd[12..16].copy_from_slice(&wim::VERSION_SOLID.to_le_bytes());
    let dir = tmp("esd");
    let img = dir.join("disk.img");
    let ctx = JobCtx::detached(false);
    // Forced FAT32: refused before anything is written.
    let target = open_for_write(&TargetSpec::File { path: img.clone(), size: Some(96 << 20) }, &ctx).unwrap();
    let opts = WindowsOptions { layout: Some(WindowsLayout::Fat32Split), ..Default::default() };
    let e = write_tree_tuned(&request(opts), &mut media(esd.clone(), "install.esd"), &target, &ctx, &small()).unwrap_err();
    assert!(e.to_string().contains("install.esd") && e.to_string().contains("NTFS"), "{e}");
    assert!(std::fs::read(&img).unwrap().iter().all(|&b| b == 0), "disk untouched");
    // Default: NTFS + UEFI:NTFS.
    let r = write_tree_tuned(&request(WindowsOptions::default()), &mut media(esd, "install.esd"), &target, &ctx, &small()).unwrap();
    assert_eq!(r["layout"], "ntfs_uefi");
    // Bad options are refused before anything is written, too.
    drop(target);
    std::fs::remove_file(&img).unwrap();
    let target = open_for_write(&TargetSpec::File { path: img.clone(), size: Some(96 << 20) }, &ctx).unwrap();
    let opts = WindowsOptions { local_account: Some(LocalAccount { name: "Administrator".into(), password: String::new() }), ..Default::default() };
    assert!(write_tree_tuned(&request(opts), &mut media(wim.clone(), "install.wim"), &target, &ctx, &small()).is_err());
    let opts = WindowsOptions { custom_answer_file: Some("<unattend>".into()), ..Default::default() };
    assert!(write_tree_tuned(&request(opts), &mut media(wim, "install.wim"), &target, &ctx, &small()).is_err());
    assert!(std::fs::read(&img).unwrap().iter().all(|&b| b == 0), "disk untouched");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_folder_tree_works_and_cancel_stops_the_job() {
    let src = tmp("dirtree-src");
    for (p, d) in media(synth_wim(&[5000], 1, XML, 0), "install.wim").0 {
        let host = src.join(p.trim_start_matches('/'));
        std::fs::create_dir_all(host.parent().unwrap()).unwrap();
        std::fs::write(host, d).unwrap();
    }
    let dir = tmp("dirtree");
    let img = dir.join("disk.img");
    let ctx = JobCtx::detached(false);
    let target = open_for_write(&TargetSpec::File { path: img.clone(), size: Some(64 << 20) }, &ctx).unwrap();
    let opts = WindowsOptions { skip_privacy_questions: true, locale: Some("it-IT".into()), time_zone: Some("Europe/Rome".into()), ..Default::default() };
    let r = write_tree(&request(opts), &mut DirTree::new(&src), &target, &ctx).unwrap();
    assert_eq!(r["layout"], "fat32_split");
    assert!(r["split"].is_null());
    assert_eq!(r["windows"]["version"], "Windows 11");
    let ctx2 = JobCtx::detached(false);
    ctx2.cancel();
    assert!(matches!(write_tree(&request(WindowsOptions::default()), &mut DirTree::new(&src), &target, &ctx2), Err(ForgeError::Cancelled)));
    drop(target);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&src).unwrap();
}

#[test]
fn the_iso_hookup_reports_clearly_until_the_reader_lands() {
    let dir = tmp("hook");
    let ctx = JobCtx::detached(false);
    let target = open_for_write(&TargetSpec::File { path: dir.join("d.img"), size: Some(1 << 20) }, &ctx).unwrap();
    let src: Arc<dyn BlockDev> = Arc::new(crate::block::MemDev::new(4096));
    let e = write(&request(WindowsOptions::default()), src, &target, &ctx).unwrap_err();
    assert!(matches!(e, ForgeError::Unsupported(_)));
    drop(target);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_medias_own_answer_file_is_replaced_and_still_verifies() {
    let mut t = media(synth_wim(&[5000], 1, XML, 0), "install.wim");
    t.0.push(("/autounattend.xml".into(), b"<unattend>theirs</unattend>".to_vec()));
    let dir = tmp("own-answer");
    let ctx = JobCtx::detached(false);
    let target = open_for_write(&TargetSpec::File { path: dir.join("d.img"), size: Some(64 << 20) }, &ctx).unwrap();
    let opts = WindowsOptions { local_account: Some(LocalAccount { name: "anna".into(), password: String::new() }), ..Default::default() };
    let r = write_tree(&request(opts), &mut t, &target, &ctx).unwrap();
    assert_eq!(r["verified"], true);
    drop(target);
    let (_, mut fs, _) = open_part(&dir.join("d.img"), 0);
    let xml = String::from_utf8(tree::read_file(fs.as_mut(), "/autounattend.xml").unwrap().unwrap()).unwrap();
    assert!(xml.contains("<Name>anna</Name>"));
    std::fs::remove_dir_all(&dir).unwrap();
}
