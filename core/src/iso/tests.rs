//! Tests of the ISO 9660 / Joliet / Rock Ridge / El Torito readers and of view selection, on
//! images made by [`super::testimg::iso`] (no external tools needed). The kernel-comparison
//! suite (`tests/iso_linux_e2e.rs`) checks the reader against xorriso / genisoimage images and
//! the Linux kernel.

use super::testimg::iso::IsoBuilder;
use super::*;
use crate::block::{FileDev, MemDev};

fn dev(b: IsoBuilder) -> Arc<dyn BlockDev> {
    Arc::new(MemDev::from_vec(b.build().to_vec()))
}

fn read_file(t: &mut IsoTree, path: &str) -> Vec<u8> {
    let size = t.entry(path).unwrap_or_else(|| panic!("{path} missing")).size;
    let mut out = vec![0u8; size as usize];
    let mut off = 0usize;
    while off < out.len() {
        let n = t.read(path, off as u64, &mut out[off..]).unwrap();
        assert!(n > 0);
        off += n;
    }
    out
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u32).wrapping_mul(2654435761).rotate_left(seed as u32 % 31) as u8 ^ seed).collect()
}

fn sample() -> IsoBuilder {
    let long = "a-very-long-file-name-".repeat(10) + "end.txt"; // 227 chars: NM continues in a CE area
    IsoBuilder::new("SAMPLE_LABEL")
        .file("/readme.txt", b"hello iso\n".to_vec())
        .file("/boot/grub/grub.cfg", b"menuentry x {}\n".to_vec())
        .file("/Mixed Case/Caffè ☕.txt", pattern(5000, 3))
        .file(&format!("/long/{long}"), b"long".to_vec())
        .file("/empty.bin", vec![])
        .file("/big/multi.bin", pattern(20_000, 9))
        .symlink("/link-rel", "boot/grub/grub.cfg")
        .symlink("/link-abs", "/usr/lib/../share/x")
        .dir("/a/b/c/d/e/f/g/h/i/j")
        .file("/a/b/c/d/e/f/g/h/i/j/deep.txt", b"deep".to_vec())
        .max_extent(8192)
}

#[test]
fn rock_ridge_view_has_posix_names_modes_links_and_relocated_dirs() {
    let mut t = open(dev(sample())).unwrap();
    assert_eq!(t.view(), View::RockRidge);
    assert_eq!(t.label(), "SAMPLE_LABEL");
    assert_eq!(read_file(&mut t, "/readme.txt"), b"hello iso\n");
    assert_eq!(read_file(&mut t, "/Mixed Case/Caffè ☕.txt"), pattern(5000, 3));
    let long = "a-very-long-file-name-".repeat(10) + "end.txt";
    assert_eq!(read_file(&mut t, &format!("/long/{long}")), b"long", "NM continued through a CE area");
    // Multi-extent (8 KiB extents): 3 records, one file.
    assert_eq!(read_file(&mut t, "/big/multi.bin"), pattern(20_000, 9));
    let e = t.entry("/big/multi.bin").unwrap();
    assert_eq!(e.size, 20_000);
    assert_eq!(t.entry("/link-rel").unwrap().symlink.as_deref(), Some("boot/grub/grub.cfg"));
    assert_eq!(t.entry("/link-abs").unwrap().symlink.as_deref(), Some("/usr/lib/../share/x"));
    assert_eq!(t.entry("/empty.bin").unwrap().size, 0);
    // Relocated directory (depth 9) shows up at its real place, not in rr_moved.
    assert_eq!(read_file(&mut t, "/a/b/c/d/e/f/g/h/i/j/deep.txt"), b"deep");
    let all = t.entries().unwrap();
    assert!(all.iter().all(|e| !e.path.starts_with("/rr_moved/")), "{:?}", all.iter().map(|e| &e.path).collect::<Vec<_>>());
    // Modes from PX, mtimes from TF (modify stamp, not creation).
    let rd = t.entry("/readme.txt").unwrap();
    assert!(rd.mode & 0o644 == 0o644, "{:o}", rd.mode);
    assert!(rd.mtime > 1_704_000_000);
    assert!(t.entry("/boot/grub").unwrap().is_dir);
    // Parents before children.
    for (i, e) in all.iter().enumerate() {
        if e.parent() != "/" {
            assert!(all[..i].iter().any(|p| p.path == e.parent() && p.is_dir), "{} before its parent", e.path);
        }
    }
    // Case-insensitive lookups.
    assert_eq!(read_file(&mut t, "/README.TXT"), b"hello iso\n");
}

#[test]
fn joliet_and_plain_views() {
    let d = dev(sample());
    let mut j = open_view(d.clone(), View::Joliet).unwrap();
    assert_eq!(read_file(&mut j, "/Mixed Case/Caffè ☕.txt"), pattern(5000, 3));
    assert_eq!(read_file(&mut j, "/a/b/c/d/e/f/g/h/i/j/deep.txt"), b"deep");
    assert_eq!(read_file(&mut j, "/big/multi.bin"), pattern(20_000, 9));
    let mut p = open_view(d.clone(), View::Iso9660).unwrap();
    assert_eq!(read_file(&mut p, "/README.TXT"), b"hello iso\n");
    assert_eq!(read_file(&mut p, "/BIG/MULTI.BIN"), pattern(20_000, 9));
    assert!(p.entry("/EMPTY.BIN").is_some());
    // Without Rock Ridge, Joliet is the best view; without both, plain ISO 9660.
    let t = open(dev(sample().rock_ridge(false))).unwrap();
    assert_eq!(t.view(), View::Joliet);
    let mut t = open(dev(sample().rock_ridge(false).joliet(false))).unwrap();
    assert_eq!(t.view(), View::Iso9660);
    assert_eq!(read_file(&mut t, "/readme.txt"), b"hello iso\n", "case-insensitive");
    assert!(open_view(dev(sample().rock_ridge(false)), View::RockRidge).is_err());
    assert!(open_view(dev(sample().joliet(false)), View::Joliet).is_err());
}

#[test]
fn el_torito_entries_and_hybrid_detection() {
    let mut efi = vec![0u8; 64 * 1024];
    // A FAT boot sector: 128 sectors of 512 bytes = 64 KiB, recorded count 1 (as large EFI images).
    efi[0] = 0xEB;
    efi[11..13].copy_from_slice(&512u16.to_le_bytes());
    efi[19..21].copy_from_slice(&128u16.to_le_bytes());
    efi[510] = 0x55;
    efi[511] = 0xAA;
    let b = IsoBuilder::new("BOOT").file("/isolinux/isolinux.bin", pattern(2048, 1)).file("/boot/grub/efi.img", efi.clone()).bios_boot("/isolinux/isolinux.bin").efi_boot("/boot/grub/efi.img", Some(1));
    let d = dev(b);
    let cat = boot_info(d.as_ref()).unwrap().unwrap();
    assert_eq!(cat.entries.len(), 2);
    let bios = cat.find(BootPlatform::Bios).unwrap();
    assert_eq!((bios.media, bios.sectors, bios.size), (BootMedia::NoEmulation, 4, 2048));
    let e = cat.find(BootPlatform::Efi).unwrap();
    assert_eq!(e.size, 64 * 1024, "size from the image's FAT header, not the count of 1");
    assert_eq!(cat.read_image(d.as_ref(), e, 1 << 20).unwrap(), efi);
    assert!(!is_hybrid(d.as_ref()));
    let t = open(d.clone()).unwrap();
    assert_eq!(t.boot_catalog().unwrap().entries.len(), 2);
    let h = dev(IsoBuilder::new("H").file("/x", b"x".to_vec()).hybrid_mbr(true));
    assert!(is_hybrid(h.as_ref()));
    assert!(boot_info(h.as_ref()).unwrap().is_none());
    // Not an ISO at all.
    let m: Arc<dyn BlockDev> = Arc::new(MemDev::new(1 << 20));
    assert!(!is_hybrid(m.as_ref()));
    assert!(boot_info(m.as_ref()).unwrap().is_none());
    assert!(open(m).is_err());
}

#[test]
fn files_over_4_gib_span_several_extents() {
    // A 5 GiB file: two extents (4 GiB − 2 KiB, then the rest), written as a sparse image file.
    let size = 5u64 << 30;
    let tail_off = (4u64 << 30) + 12345;
    let b = IsoBuilder::new("BIG").file("/small.txt", b"s".to_vec()).sparse_file("/huge.bin", size, vec![(0, b"head".to_vec()), ((4u64 << 30) - 4096, pattern(8192, 5)), (tail_off, b"tail data".to_vec())]);
    let dir = std::env::temp_dir().join(format!("ld-iso-big-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.iso");
    b.build().write_sparse(&path).unwrap();
    let d: Arc<dyn BlockDev> = Arc::new(FileDev::open(&path, true).unwrap());
    for view in [View::RockRidge, View::Joliet, View::Iso9660] {
        let mut t = open_view(d.clone(), view).unwrap();
        let name = if view == View::Iso9660 { "/HUGE.BIN" } else { "/huge.bin" };
        assert_eq!(t.entry(name).unwrap().size, size, "{view:?}");
        let mut buf = vec![0u8; 9];
        t.read(name, tail_off, &mut buf).unwrap();
        assert_eq!(&buf, b"tail data", "{view:?}");
        // A read across the boundary between the two extents.
        let mut across = vec![0u8; 8192];
        let n = t.read(name, (4u64 << 30) - 4096, &mut across).unwrap();
        assert_eq!(n, 8192);
        assert_eq!(across, pattern(8192, 5), "{view:?}");
        let mut end = vec![0u8; 100];
        assert_eq!(t.read(name, size - 10, &mut end).unwrap(), 10);
        assert_eq!(t.read(name, size, &mut end).unwrap(), 0);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn copy_into_fat32_through_copy_tree() {
    use crate::forge::job::JobCtx;
    use crate::forge::tree::{copy_tree, read_file as fs_read, CopyAction};
    use crate::vfs::{self, FsType, MountDefaults};
    let mut t = open(dev(sample())).unwrap();
    let out: Arc<dyn BlockDev> = Arc::new(MemDev::new(64 << 20));
    vfs::format(out.clone(), FsType::Fat32, "T").unwrap();
    let mut fs = vfs::open_fs(out, false, MountDefaults::default()).unwrap();
    copy_tree(&mut t, fs.as_mut(), &JobCtx::detached(false), Some(u32::MAX as u64), |_| CopyAction::Copy).unwrap();
    assert_eq!(fs_read(fs.as_mut(), "/big/multi.bin").unwrap().unwrap(), pattern(20_000, 9));
    assert_eq!(fs_read(fs.as_mut(), "/a/b/c/d/e/f/g/h/i/j/deep.txt").unwrap().unwrap(), b"deep");
}

/// Hostile images: every mutation must give an error or a (partial) tree, never a panic, a
/// hang or a huge allocation.
#[test]
fn corrupted_images_never_panic() {
    let base = sample()
        .file("/isolinux/isolinux.bin", pattern(2048, 1))
        .file("/efi.img", pattern(4096, 2))
        .bios_boot("/isolinux/isolinux.bin")
        .efi_boot("/efi.img", None)
        .build()
        .to_vec();
    // Sectors that hold metadata (descriptors, catalog, directories, CE areas).
    let meta_end = 16 * 2048 + 64 * 2048;
    let mut rng = 0x1234_5678_9abc_def0u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let t0 = std::time::Instant::now();
    for round in 0..1500 {
        let mut img = base.clone();
        let flips = 1 + (next() % 24) as usize;
        for _ in 0..flips {
            let at = 16 * 2048 + (next() % (meta_end as u64 - 16 * 2048)) as usize;
            if at < img.len() {
                img[at] = match next() % 4 {
                    0 => 0xFF,
                    1 => 0,
                    _ => next() as u8,
                };
            }
        }
        if round % 10 == 0 {
            img.truncate((next() % img.len() as u64) as usize);
        }
        let d: Arc<dyn BlockDev> = Arc::new(MemDev::from_vec(img));
        let _ = boot_info(d.as_ref());
        let _ = is_hybrid(d.as_ref());
        for v in [None, Some(View::Joliet), Some(View::Iso9660)] {
            let r = match v {
                None => open(d.clone()),
                Some(v) => open_view(d.clone(), v),
            };
            if let Ok(mut t) = r {
                for e in t.entries().unwrap().into_iter().filter(|e| !e.is_dir).take(50) {
                    let mut buf = vec![0u8; 4096];
                    let _ = t.read(&e.path, 0, &mut buf);
                    let _ = t.read(&e.path, e.size.saturating_sub(10), &mut buf);
                }
            }
        }
    }
    assert!(t0.elapsed().as_secs() < 60, "fuzzing took {:?}", t0.elapsed());
}

#[test]
fn directory_loops_and_lying_lengths_are_contained() {
    let img = sample().build().to_vec();
    let d: Arc<dyn BlockDev> = Arc::new(MemDev::from_vec(img.clone()));
    let t = open_view(d, View::Iso9660).unwrap();
    let n = t.nodes.iter().find(|n| n.path == "/BOOT").unwrap().clone();
    let _ = n;
    // Make /BOOT's record point at the root directory: a loop.
    let pvd = &img[16 * 2048..17 * 2048];
    let root_lba = le32(pvd, 158) as usize;
    let root = &img[root_lba * 2048..root_lba * 2048 + 2048];
    let mut p = 0usize;
    let mut patched = img.clone();
    while p < 2048 && root[p] != 0 {
        let len = root[p] as usize;
        let name = &root[p + 33..p + 33 + root[p + 32] as usize];
        if name == b"BOOT" {
            let o = root_lba * 2048 + p;
            patched[o + 2..o + 6].copy_from_slice(&(root_lba as u32).to_le_bytes());
        }
        p += len;
    }
    let d: Arc<dyn BlockDev> = Arc::new(MemDev::from_vec(patched));
    let t = open_view(d.clone(), View::Iso9660).unwrap();
    assert!(t.entry("/BOOT").unwrap().is_dir);
    assert!(t.entry("/BOOT/README.TXT").is_none(), "the looping directory is not walked again");
    // A file whose size points far beyond the image is refused, not read past the end.
    let mut huge = img.clone();
    let mut p = 0usize;
    while p < 2048 && root[p] != 0 {
        let len = root[p] as usize;
        let name = &root[p + 33..p + 33 + root[p + 32] as usize];
        if name == b"README.TXT;1" {
            let o = root_lba * 2048 + p;
            huge[o + 10..o + 14].copy_from_slice(&u32::MAX.to_le_bytes());
        }
        p += len;
    }
    let d: Arc<dyn BlockDev> = Arc::new(MemDev::from_vec(huge));
    let mut t = open_view(d, View::Iso9660).unwrap();
    let mut buf = [0u8; 16];
    let e = t.read("/README.TXT", 0, &mut buf).unwrap_err();
    assert!(e.to_string().contains("beyond the end"), "{e}");
    assert!(t.read("/EMPTY.BIN", 0, &mut buf).is_ok(), "the rest of the tree is usable");
}
