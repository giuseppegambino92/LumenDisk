//! End-to-end flashing: a real partitioned image (GPT + FAT32 + ext4, made by LumenDisk),
//! compressed / wrapped by the real tools (gzip, xz, bzip2, zstd, zip, qemu-img in the
//! `lumendisk-toolbox` container; hdiutil on the Mac), written by `forge::flash` to files and
//! to disk images attached with hdiutil, and compared byte for byte (SHA-256).
//!
//!   LUMENDISK_FORGE_VIRTUAL_ONLY=1 cargo test --release -p lumendisk-core --test flash_e2e \
//!       -- --ignored --test-threads=1 --nocapture
//!
//! Only files and disk images created here are ever written; attached images are detached by
//! a guard even when a test fails.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use lumendisk_core::block::{BlockDev, FileDev, PartitionDev};
use lumendisk_core::forge::job::{self, JobCtx, Phase, State};
use lumendisk_core::forge::layout::{self, NewPart, PartType, TableOptions};
use lumendisk_core::forge::plan::{self, FlashRequest, Request, Scheme};
use lumendisk_core::forge::target::{self, TargetSpec};
use lumendisk_core::forge::{format, image, ForgeError, MkfsOptions, TargetFs};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const RAW_MB: u64 = 192;

fn virtual_only() {
    // Every write below must be refused unless it targets a disk image (forge::target policy).
    std::env::set_var("LUMENDISK_FORGE_VIRTUAL_ONLY", "1");
    assert!(lumendisk_core::forge::virtual_only());
}

fn sha_file(p: &Path, len: Option<u64>) -> String {
    let dev = FileDev::open(p, true).unwrap();
    sha_dev(&dev, len.unwrap_or(dev.size()))
}

fn sha_dev(dev: &dyn BlockDev, len: u64) -> String {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 4 << 20];
    let mut pos = 0u64;
    while pos < len {
        let n = (len - pos).min(buf.len() as u64) as usize;
        dev.read_at(pos, &mut buf[..n]).unwrap();
        h.update(&buf[..n]);
        pos += n as u64;
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

struct Fixture {
    dir: PathBuf,
    raw: PathBuf,
    raw_len: u64,
    raw_sha: String,
    /// (label, path) of every encoding of the raw image.
    sources: Vec<(String, PathBuf)>,
    /// SHA-256 of each DMG as hdiutil itself expands it (see `fixture`).
    dmg_sha: Vec<(String, String)>,
}

impl Fixture {
    /// The SHA-256 the disk must have after writing source `label`.
    fn expected(&self, label: &str) -> &str {
        self.dmg_sha.iter().find(|(l, _)| l == label).map(|(_, s)| s.as_str()).unwrap_or(&self.raw_sha)
    }
}

fn rand_fill(buf: &mut [u8], seed: &mut u64) {
    for b in buf.iter_mut() {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *b = (*seed >> 24) as u8;
    }
}

/// A 192 MiB raw disk: GPT, a FAT32 and an ext4 partition made by LumenDisk, files of random
/// data in both, random data in the free space between.
fn make_raw(path: &Path) {
    let f = std::fs::File::create(path).unwrap();
    f.set_len(RAW_MB << 20).unwrap();
    drop(f);
    let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(path, false).unwrap());
    let mut seed = 0x1234_5678_9ABC_DEF1u64;
    // Random "old data" everywhere first (part of it survives in unallocated space).
    let mut buf = vec![0u8; 1 << 20];
    for mb in (0..RAW_MB).step_by(3) {
        rand_fill(&mut buf, &mut seed);
        dev.write_at(mb << 20, &buf).unwrap();
    }
    let parts = [
        NewPart { size: Some(64 << 20), kind: PartType::Fs(TargetFs::Fat32), name: "EFI-DATA".into(), bootable: false },
        NewPart { size: None, kind: PartType::Fs(TargetFs::Ext4), name: "linux".into(), bootable: false },
    ];
    let placed = layout::write_table(dev.as_ref(), Scheme::Gpt, &parts, &TableOptions::default()).unwrap();
    for (p, fs) in placed.iter().zip([TargetFs::Fat32, TargetFs::Ext4]) {
        let pdev: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(dev.clone(), p.start, p.size).unwrap());
        format::mkfs(pdev.clone(), fs, &MkfsOptions { label: "FLASHTEST".into(), ..Default::default() }).unwrap();
        let mut vol = lumendisk_core::vfs::open_fs(pdev, false, lumendisk_core::vfs::MountDefaults::default()).unwrap();
        for i in 0..3 {
            let mut data = vec![0u8; (3 << 20) + i * 12345];
            rand_fill(&mut data, &mut seed);
            lumendisk_core::forge::tree::write_file(vol.as_mut(), &format!("/data/file{i}.bin"), &data).unwrap();
        }
        lumendisk_core::forge::tree::write_file(vol.as_mut(), "/readme.txt", b"LumenDisk flash test\n").unwrap();
        vol.sync().unwrap();
    }
    dev.flush().unwrap();
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let dir = common::workdir("flash_e2e");
        let raw = dir.join("disk.img");
        make_raw(&raw);
        let raw_len = RAW_MB << 20;
        let raw_sha = sha_file(&raw, None);
        let script = "set -e
            gzip -k -6 disk.img
            xz -k -T4 -6 disk.img && mv disk.img.xz disk.img.mt.xz
            xz -k -T1 -6 disk.img && mv disk.img.xz disk.img.st.xz
            bzip2 -k -9 disk.img
            zstd -q -k -T0 -10 disk.img
            echo 'checksums not included' > README.txt
            zip -q -6 disk.img.zip disk.img README.txt
            qemu-img convert -f raw -O vpc -o subformat=fixed,force_size=on disk.img disk.fixed.vhd
            qemu-img convert -f raw -O vpc -o subformat=dynamic,force_size=on disk.img disk.dynamic.vhd
            chmod a+rw *";
        let (code, out) = common::docker(&dir, script);
        assert_eq!(code, 0, "toolbox: {out}");
        let mut sources = vec![
            ("raw".to_string(), raw.clone()),
            ("gzip".into(), dir.join("disk.img.gz")),
            ("xz-multithreaded".into(), dir.join("disk.img.mt.xz")),
            ("xz-single-block".into(), dir.join("disk.img.st.xz")),
            ("bzip2".into(), dir.join("disk.img.bz2")),
            ("zstd".into(), dir.join("disk.img.zst")),
            ("zip".into(), dir.join("disk.img.zip")),
            ("vhd-fixed".into(), dir.join("disk.fixed.vhd")),
            ("vhd-dynamic".into(), dir.join("disk.dynamic.vhd")),
        ];
        let mut dmg_sha = vec![];
        for fmt in ["UDRO", "UDZO", "UDBZ", "ULFO", "ULMO"] {
            let out = dir.join(format!("disk-{fmt}.dmg"));
            let o = Command::new("/usr/bin/hdiutil").args(["convert", "-quiet", "-format", fmt, "-o"]).arg(&out).arg(&raw).output().unwrap();
            assert!(o.status.success(), "hdiutil convert {fmt}: {}", String::from_utf8_lossy(&o.stderr));
            // hdiutil knows the filesystems: free space (Apple_Free, unused FAT clusters) is
            // stored as "ignore" runs, which read back as zeros, so a DMG is not the raw image
            // byte for byte. The reference is what Apple's own decoder expands it to.
            let reference = dir.join(format!("ref-{fmt}"));
            let o = Command::new("/usr/bin/hdiutil").args(["convert", "-quiet", "-format", "UDTO", "-o"]).arg(&reference).arg(&out).output().unwrap();
            assert!(o.status.success(), "hdiutil convert {fmt} → UDTO: {}", String::from_utf8_lossy(&o.stderr));
            let reference = reference.with_extension("cdr");
            assert_eq!(std::fs::metadata(&reference).unwrap().len(), raw_len);
            dmg_sha.push((format!("dmg-{fmt}"), sha_file(&reference, None)));
            std::fs::remove_file(&reference).unwrap();
            sources.push((format!("dmg-{fmt}"), out));
        }
        for (label, p) in &sources {
            eprintln!("source {label:<18} {:>11} bytes", std::fs::metadata(p).unwrap().len());
        }
        Fixture { dir, raw, raw_len, raw_sha, sources, dmg_sha }
    })
}

fn flash(source: &Path, target: TargetSpec, extra: Value) -> Result<Value, ForgeError> {
    let mut j = json!({"source": source, "target": target, "eject": false});
    for (k, v) in extra.as_object().unwrap() {
        j[k] = v.clone();
    }
    let r: FlashRequest = serde_json::from_value(j).unwrap();
    lumendisk_core::forge::flash::run(&r, &JobCtx::detached(false))
}

/// A disk image attached with hdiutil, detached on drop.
struct Attached {
    bsd: String,
    backing: PathBuf,
}

impl Attached {
    fn new(dir: &Path, name: &str, mb: u64, fill: u8) -> Attached {
        let backing = dir.join(name);
        std::fs::write(&backing, vec![fill; (mb << 20) as usize]).unwrap();
        let bsd = target::attach_image_for_tests(&backing).unwrap();
        let info = target::disk_info(&bsd).unwrap();
        assert!(info.disk_image && info.allowed, "{bsd} must be an attached image the policy accepts: {info:?}");
        Attached { bsd, backing }
    }
    fn read_sha(&self, len: u64) -> String {
        let dev = lumendisk_core::block::open_source(&format!("/dev/r{}", self.bsd), true, None).unwrap();
        sha_dev(dev.as_ref(), len)
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        if let Err(e) = target::detach(&self.bsd) {
            eprintln!("detach {}: {e}", self.bsd);
        }
        let _ = std::fs::remove_file(&self.backing);
    }
}

#[test]
#[ignore]
fn every_format_is_detected_and_written_identically_to_files() {
    virtual_only();
    let fx = fixture();
    let ctx = JobCtx::detached(false);
    for (label, src) in &fx.sources {
        let img = image::Image::open(src).unwrap();
        let ins = image::inspect_image(&img, &ctx, false).unwrap();
        assert_eq!(ins.partition_scheme.as_deref(), Some("GPT"), "{label}: partitions seen inside");
        assert_eq!(ins.partitions.len(), 2, "{label}");
        let out = fx.dir.join(format!("out-{label}.img"));
        let v = flash(src, TargetSpec::File { path: out.clone(), size: None }, json!({})).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(std::fs::metadata(&out).unwrap().len(), fx.raw_len, "{label}");
        assert_eq!(sha_file(&out, None), fx.expected(label), "{label}: written bytes");
        assert_eq!(v["hashes"]["image_sha256"], fx.expected(label), "{label}");
        assert_eq!(v["verification"]["ok"], true, "{label}");
        eprintln!(
            "file  {label:<18} format={:<6} write {:>7.1} MB/s  verify {:>7.1} MB/s",
            v["format"].as_str().unwrap(),
            v["write_mb_s"].as_f64().unwrap(),
            v["verify_mb_s"].as_f64().unwrap()
        );
        std::fs::remove_file(&out).unwrap();
    }
}

#[test]
#[ignore]
fn every_format_is_written_identically_to_an_attached_disk_image() {
    virtual_only();
    let fx = fixture();
    let disk = Attached::new(&fx.dir, "target-disk.img", RAW_MB + 64, 0xA5);
    for (label, src) in &fx.sources {
        let v = flash(src, TargetSpec::Disk { bsd: disk.bsd.clone() }, json!({})).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(v["target"], format!("/dev/r{}", disk.bsd), "{label}");
        assert_eq!(v["verification"]["ok"], true, "{label}");
        assert_eq!(disk.read_sha(fx.raw_len), fx.expected(label), "{label}: bytes on the disk");
        eprintln!(
            "disk  {label:<18} write {:>7.1} MB/s  verify {:>7.1} MB/s  total {:.1} s",
            v["write_mb_s"].as_f64().unwrap(),
            v["verify_mb_s"].as_f64().unwrap(),
            v["durations"]["total"].as_f64().unwrap()
        );
    }
}

#[test]
#[ignore]
fn cancelling_mid_write_stops_the_job() {
    virtual_only();
    let fx = fixture();
    let disk = Attached::new(&fx.dir, "cancel-disk.img", RAW_MB + 64, 0);
    let src = fx.dir.join("disk.img.st.xz"); // single-threaded XZ: slow enough to catch mid-way
    for tgt in [TargetSpec::File { path: fx.dir.join("cancel.img"), size: None }, TargetSpec::Disk { bsd: disk.bsd.clone() }] {
        let req: FlashRequest = serde_json::from_value(json!({"source": src, "target": tgt, "eject": false})).unwrap();
        let id = plan::start(Request::Flash(req));
        let mut cancelled_at = None;
        for _ in 0..3000 {
            let s = job::status(id, 0).unwrap();
            if s.phase == Phase::Writing && s.done > (16 << 20) && cancelled_at.is_none() {
                assert!(job::cancel(id));
                cancelled_at = Some(s.done);
            }
            if s.state != State::Running {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let s = job::status(id, 0).unwrap();
        assert_eq!(s.state, State::Cancelled, "{tgt:?}: {s:?}");
        let at = cancelled_at.expect("cancelled while writing");
        assert!(s.done < fx.raw_len, "stopped before the end ({} of {})", s.done, fx.raw_len);
        eprintln!("cancel {tgt:?}: requested at {at} bytes, stopped at {}", s.done);
        assert!(job::forget(id));
        // The disk is free again (the claim was released).
        assert!(target::claim(&disk.bsd).is_ok());
    }
}

#[test]
#[ignore]
fn too_large_images_and_wrong_checksums_are_refused_before_writing() {
    virtual_only();
    let fx = fixture();
    // A 64 MiB disk for a 192 MiB image.
    let small = Attached::new(&fx.dir, "small-disk.img", 64, 0x5A);
    let before = small.read_sha(64 << 20);
    for (label, src) in fx.sources.iter().filter(|(l, _)| ["raw", "xz-multithreaded", "dmg-UDZO", "vhd-dynamic"].contains(&l.as_str())) {
        let e = flash(src, TargetSpec::Disk { bsd: small.bsd.clone() }, json!({})).unwrap_err();
        assert!(e.to_string().contains("larger") || e.to_string().contains("più grande"), "{label}: {e}");
    }
    // gzip: the size is only known while decompressing, so the write stops at the end of the
    // disk (another disk: this one is partly written by then).
    {
        let small2 = Attached::new(&fx.dir, "small-disk-2.img", 64, 0);
        let e = flash(&fx.dir.join("disk.img.gz"), TargetSpec::Disk { bsd: small2.bsd.clone() }, json!({})).unwrap_err();
        assert!(e.to_string().contains("larger") || e.to_string().contains("più grande"), "gzip: {e}");
    }
    let e = flash(&fx.raw, TargetSpec::File { path: fx.dir.join("small.img"), size: Some(100 << 20) }, json!({})).unwrap_err();
    assert!(matches!(e, ForgeError::Invalid(_)), "{e}");

    let disk = Attached::new(&fx.dir, "hash-disk.img", RAW_MB + 64, 0x5A);
    let before_big = disk.read_sha(RAW_MB << 20);
    let wrong = format!("sha256:{}", "ab".repeat(32));
    for src in [fx.raw.clone(), fx.dir.join("disk.img.mt.xz")] {
        let e = flash(&src, TargetSpec::Disk { bsd: disk.bsd.clone() }, json!({"expect_hash": wrong})).unwrap_err();
        assert!(matches!(e, ForgeError::Mismatch(_)), "{e}");
    }
    assert_eq!(disk.read_sha(RAW_MB << 20), before_big, "nothing was written");
    // The right checksum of the decompressed image is accepted for a compressed one.
    let v = flash(&fx.dir.join("disk.img.mt.xz"), TargetSpec::Disk { bsd: disk.bsd.clone() }, json!({"expect_hash": fx.raw_sha})).unwrap();
    assert_eq!(v["hashes"]["expected"]["matched"], "decompressed");
    assert_eq!(disk.read_sha(fx.raw_len), fx.raw_sha);
    drop(disk);
    assert_eq!(small.read_sha(64 << 20), before, "the small disk was not touched");
}

#[test]
#[ignore]
fn hybrid_isos_are_recognised_and_written_in_dd_mode() {
    virtual_only();
    let dir = common::workdir("flash_e2e_iso");
    let script = "set -e
        MBR=/usr/lib/syslinux/mbr/isohdpfx.bin; [ -f $MBR ] || MBR=/usr/lib/ISOLINUX/isohdpfx.bin
        mkdir -p root/isolinux root/EFI/BOOT root/live
        cp /usr/lib/ISOLINUX/isolinux.bin root/isolinux/
        cp /usr/lib/syslinux/modules/bios/ldlinux.c32 root/isolinux/
        printf 'DEFAULT live\\nLABEL live\\n  KERNEL /live/vmlinuz\\n' > root/isolinux/isolinux.cfg
        head -c 3000000 /dev/urandom > root/live/filesystem.squashfs
        head -c 100000 /dev/urandom > root/live/vmlinuz
        dd if=/dev/zero of=root/EFI/efiboot.img bs=1k count=2880 2>/dev/null
        mkfs.vfat root/EFI/efiboot.img >/dev/null
        xorriso -as mkisofs -quiet -V LUMEN_LIVE -J -R \
          -isohybrid-mbr $MBR \
          -b isolinux/isolinux.bin -c isolinux/boot.cat -no-emul-boot -boot-load-size 4 -boot-info-table \
          -eltorito-alt-boot -e EFI/efiboot.img -no-emul-boot -isohybrid-gpt-basdat \
          -o hybrid.iso root
        xorriso -as mkisofs -quiet -V PLAIN_ISO -J -R -b isolinux/isolinux.bin -c isolinux/boot.cat \
          -no-emul-boot -boot-load-size 4 -o plain.iso root
        xz -k -T2 hybrid.iso
        chmod a+rw *";
    let (code, out) = common::docker(&dir, script);
    assert_eq!(code, 0, "toolbox: {out}");
    let hybrid = dir.join("hybrid.iso");
    let v = image::inspect_quick(&hybrid.to_string_lossy()).unwrap();
    eprintln!("hybrid: {}", serde_json::to_string(&v["iso"]).unwrap());
    assert_eq!(v["iso"]["label"], "LUMEN_LIVE");
    assert_eq!(v["iso"]["el_torito"], true);
    assert_eq!(v["iso"]["efi_boot"], true);
    assert_eq!(v["iso"]["hybrid"], true);
    assert_eq!(v["recommended_mode"], "dd");
    assert!(!v["partitions"].as_array().unwrap().is_empty(), "the hybrid MBR / GPT partitions are listed");
    // Compressed: the same, through the on-demand temporary decompression.
    let vx = image::inspect_quick(&dir.join("hybrid.iso.xz").to_string_lossy()).unwrap();
    assert_eq!(vx["format"], "xz");
    assert_eq!(vx["iso"], v["iso"]);
    assert_eq!(vx["recommended_mode"], "dd");
    let plain = image::inspect_quick(&dir.join("plain.iso").to_string_lossy()).unwrap();
    assert_eq!(plain["iso"]["label"], "PLAIN_ISO");
    assert_eq!(plain["iso"]["hybrid"], false);
    assert_eq!(plain["iso"]["el_torito"], true);
    assert_eq!(plain["recommended_mode"], "iso");
    // Auto mode → DD for the hybrid ISO, byte for byte, to an attached disk.
    let disk = Attached::new(&dir, "iso-disk.img", 64, 0);
    let len = std::fs::metadata(&hybrid).unwrap().len();
    for src in [hybrid.clone(), dir.join("hybrid.iso.xz")] {
        let v = flash(&src, TargetSpec::Disk { bsd: disk.bsd.clone() }, json!({"mode": "auto"})).unwrap();
        assert_eq!(v["mode"], "dd");
        assert_eq!(disk.read_sha(len), sha_file(&hybrid, None));
    }
    // A non-hybrid ISO resolves to ISO mode (implemented by the ISO / Linux workstream).
    let e = flash(&dir.join("plain.iso"), TargetSpec::Disk { bsd: disk.bsd.clone() }, json!({"mode": "auto"}));
    eprintln!("plain.iso in auto mode: {:?}", e.as_ref().map(|v| v["mode"].clone()).map_err(|e| e.to_string()));
}
