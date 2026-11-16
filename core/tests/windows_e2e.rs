//! Windows media end to end, on synthetic Windows-like media (no Windows ISO is used):
//! install.wim built by wimlib with real-looking metadata, a stand-in BOOTMGR and a stand-in
//! `\efi\boot\bootx64.efi` that print markers. LumenDisk writes the media to image files and
//! to an attached disk image; the result is checked with the Linux tools (fsck.vfat,
//! ntfsfix, the kernel's vfat / ntfs3, wimlib-imagex verify / apply) and booted in QEMU:
//! UEFI (OVMF) from FAT32 and through UEFI:NTFS, UEFI:NTFS under Secure Boot with
//! Microsoft's keys, and BIOS (SeaBIOS) through the Windows 7 MBR and the NT6 boot records.
//!
//! Needs Docker with the `lumendisk-toolbox` image (scripts/docker/Dockerfile); the x86 boot
//! payloads are built once in `lumendisk-wintest-amd64` (scripts/docker/wintest-amd64, built
//! here if missing). Run:
//!   cargo test --release -p lumendisk-core --test windows_e2e -- --ignored --test-threads=1
//! Destructive steps only ever target files and disk images these tests create
//! (`LUMENDISK_FORGE_VIRTUAL_ONLY=1` is set first thing).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use lumendisk_core::forge::job::JobCtx;
use lumendisk_core::forge::plan::{FlashRequest, Request};
use lumendisk_core::forge::target::{self, TargetSpec};
use lumendisk_core::forge::tree::DirTree;
use lumendisk_core::forge::windows::{self, wim, Tuning};
use serde_json::{json, Value};

const MIB: u64 = 1 << 20;

fn virtual_only() {
    std::env::set_var("LUMENDISK_FORGE_VIRTUAL_ONLY", "1");
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/windows_fixtures")
}

fn run(cmd: &mut Command) -> (i32, String) {
    let out = cmd.output().expect("spawn");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

/// `docker run` of the toolbox with the fixtures at /fx and `dir` at /w.
fn toolbox(dir: &Path, privileged: bool, script: &str) -> (i32, String) {
    let mut c = Command::new("docker");
    c.args(["run", "--rm"]);
    if privileged {
        c.arg("--privileged");
    }
    c.arg("-v").arg(format!("{}:/fx:ro", fixtures().display()));
    c.arg("-v").arg(format!("{}:/w", dir.display()));
    c.args(["-w", "/w", "lumendisk-toolbox", "sh", "-c", script]);
    run(&mut c)
}

/// The boot payloads (stand-in BOOTMGR and bootx64.efi), built once per run.
fn payloads() -> &'static PathBuf {
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(|| {
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("it").join("windows-payloads");
        std::fs::create_dir_all(&out).unwrap();
        if out.join("bootmgr").exists() && out.join("bootx64.efi").exists() {
            return out;
        }
        let (code, _) = run(Command::new("docker").args(["image", "inspect", "lumendisk-wintest-amd64"]));
        if code != 0 {
            let ctx = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scripts/docker/wintest-amd64");
            let (code, log) = run(Command::new("docker").args(["build", "--platform", "linux/amd64", "-t", "lumendisk-wintest-amd64"]).arg(&ctx));
            assert_eq!(code, 0, "building lumendisk-wintest-amd64:\n{log}");
        }
        let (code, log) = run(Command::new("docker")
            .args(["run", "--rm", "--platform", "linux/amd64", "-v"])
            .arg(format!("{}:/src:ro", fixtures().display()))
            .arg("-v")
            .arg(format!("{}:/out", out.display()))
            .args(["lumendisk-wintest-amd64", "sh", "/src/build_payloads.sh"]));
        assert_eq!(code, 0, "building the boot payloads:\n{log}");
        out
    })
}

/// A Windows-11-like ISO tree in `dir/media`: install.wim of about `wim_mb` MiB (three
/// editions, wimlib), boot.wim, the stand-in BOOTMGR and bootx64.efi, and the usual files.
fn make_media(dir: &Path, wim_mb: u64, compression: &str, file_mb: u64) -> PathBuf {
    let media = dir.join("media");
    let _ = std::fs::remove_dir_all(&media);
    for d in ["efi/boot", "efi/microsoft/boot", "boot", "sources", "support/logging"] {
        std::fs::create_dir_all(media.join(d)).unwrap();
    }
    let p = payloads();
    std::fs::copy(p.join("bootmgr"), media.join("bootmgr")).unwrap();
    std::fs::copy(p.join("bootx64.efi"), media.join("efi/boot/bootx64.efi")).unwrap();
    let blob = |n: usize, seed: u8| -> Vec<u8> { (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect() };
    std::fs::write(media.join("efi/microsoft/boot/bcd"), blob(16384, 1)).unwrap();
    std::fs::write(media.join("boot/bcd"), blob(16384, 2)).unwrap();
    std::fs::write(media.join("bootmgr.efi"), blob(40000, 3)).unwrap();
    std::fs::write(media.join("setup.exe"), blob(100_000, 4)).unwrap();
    std::fs::write(media.join("autorun.inf"), "[AutoRun.Amd64]\r\nopen=setup.exe\r\nicon=setup.exe,0\r\n").unwrap();
    std::fs::write(media.join("support/logging/Ünïcödé näme.txt"), "ok").unwrap();
    std::fs::write(media.join("sources/idwbinfo.txt"), "[BUILDINFO]\r\nBuildType=fre\r\nBuildArch=amd64\r\nBuildBranch=ge_release\r\nBuildFull=26100.1.amd64fre.ge_release.240331-1435\r\nBuildFlavor=fre\r\n").unwrap();
    let script = format!(
        "set -e; sh /fx/make_wim.sh /w/media/sources/install.wim {wim_mb} {compression} {file_mb} && \
         mkdir -p /tmp/pe/Windows/System32 && head -c 300000 /dev/urandom > /tmp/pe/Windows/System32/winpeshl.exe && \
         wimlib-imagex capture /tmp/pe /w/media/sources/boot.wim 'Microsoft Windows Setup (amd64)' --boot \
           --image-property WINDOWS/ARCH=9 --image-property WINDOWS/INSTALLATIONTYPE=WindowsPE \
           --image-property WINDOWS/VERSION/BUILD=26100 >/dev/null && ls -l /w/media/sources"
    );
    let (code, log) = toolbox(dir, false, &script);
    assert_eq!(code, 0, "making the media:\n{log}");
    media
}

fn flash_request(target: &Path, size: u64, windows: Value) -> FlashRequest {
    // As the app sends it: the JSON contract.
    let r: Request = serde_json::from_value(json!({
        "op": "flash", "source": "/synthetic/Win11_24H2_x64.iso",
        "target": {"kind": "file", "path": target, "size": size},
        "mode": "windows", "windows": windows,
    }))
    .unwrap();
    match r {
        Request::Flash(f) => f,
        _ => unreachable!(),
    }
}

fn write_media(r: &FlashRequest, media: &Path, tune: &Tuning) -> Value {
    let ctx = JobCtx::detached(true);
    let t = target::open_for_write(&r.target, &ctx).unwrap();
    let v = windows::write_tree_tuned(r, &mut DirTree::new(media), &t, &ctx, tune).unwrap();
    eprintln!("result: {v}");
    v
}

fn partition(v: &Value, i: usize) -> (u64, u64) {
    let p = &v["partitions"][i];
    (p["start"].as_u64().unwrap(), p["size"].as_u64().unwrap())
}

/// fsck + hashes + (split) wimlib verify / apply of partition `i`.
fn check_volume(dir: &Path, disk: &str, v: &Value, i: usize, fs: &str, orig_wim: Option<&str>) -> String {
    let (off, size) = partition(v, i);
    let script = format!("sh /fx/check_volume.sh /w/{disk} {off} {size} {fs} /w/media {}", orig_wim.unwrap_or(""));
    let (code, log) = toolbox(dir, true, &script);
    eprintln!("{log}");
    assert_eq!(code, 0, "volume check failed:\n{log}");
    for k in ["FSCK-OK", "HASHES-OK", "ALL-OK"] {
        assert!(log.contains(k), "{k} missing:\n{log}");
    }
    log
}

/// Boot `disk` in QEMU and wait for `marker` on the serial console.
fn boot(dir: &Path, disk: &str, mode: &str, marker: &str, timeout: u32, linger: u32) -> String {
    let log = format!("/w/qemu-{mode}-{}.log", disk.replace(['/', '.'], "_"));
    let script = format!("sh /fx/qemu_boot.sh {mode} /w/{disk} {log} '{marker}' {timeout} {linger}");
    let (code, out) = toolbox(dir, false, &script);
    eprintln!("== QEMU {mode} {disk}\n{out}");
    assert_eq!(code, 0, "QEMU {mode} boot of {disk} did not reach '{marker}':\n{out}");
    out
}

const UEFI_MARKER: &str = "LUMENDISK-UEFI-BOOTX64-REACHED";
const BIOS_MARKER: &str = "LUMENDISK-BIOS-BOOTMGR-REACHED";

#[test]
#[ignore]
fn wim_split_sets_pass_wimlib_verify_and_apply_identically() {
    virtual_only();
    let dir = common::workdir("win-split");
    let wimp = dir.join("orig.wim");
    let (code, log) = toolbox(&dir, false, "sh /fx/make_wim.sh /w/orig.wim 30 LZX 1 && wimlib-imagex info /w/orig.wim 2 --boot >/dev/null");
    assert_eq!(code, 0, "{log}");
    // Our split, straight into files (no volume in between).
    struct Files(PathBuf, Option<std::fs::File>);
    impl wim::PartSink for Files {
        fn begin(&mut self, _p: u16, name: &str, _s: u64) -> lumendisk_core::forge::ForgeResult<()> {
            self.1 = Some(std::fs::File::create(self.0.join(name))?);
            Ok(())
        }
        fn append(&mut self, d: &[u8]) -> lumendisk_core::forge::ForgeResult<()> {
            use std::io::Write;
            Ok(self.1.as_mut().unwrap().write_all(d)?)
        }
        fn end(&mut self) -> lumendisk_core::forge::ForgeResult<()> {
            self.1 = None;
            Ok(())
        }
    }
    std::fs::create_dir_all(dir.join("parts")).unwrap();
    let size = std::fs::metadata(&wimp).unwrap().len();
    let mut tree = DirTree::new(&dir);
    let w = wim::WimFile::open(&mut tree, "/orig.wim", size).unwrap();
    assert_eq!(w.images.len(), 3);
    assert_eq!(w.images[1].name, "Windows 11 Pro");
    assert_eq!(w.images[1].edition_id, "Professional");
    assert_eq!((w.images[1].build, w.images[1].arch), (Some(26100), Some(9)));
    assert_eq!(w.header.boot_index, 2);
    assert_eq!(w.header.compression(), "lzx");
    let table = w.blob_table(&mut tree).unwrap();
    let limit = 12 * MIB;
    let plan = wim::plan_split(&w, &table, limit).unwrap();
    assert!((3..=4).contains(&plan.parts.len()), "{} parts", plan.parts.len());
    let parts = wim::write_split(&mut tree, &w, &table, &plan, "install", &mut Files(dir.join("parts"), None), &JobCtx::detached(false)).unwrap();
    for p in &parts {
        let len = std::fs::metadata(dir.join("parts").join(&p.name)).unwrap().len();
        assert!(len <= limit && len == p.size, "{} is {len} bytes", p.name);
    }
    let script = "set -e; cd /w/parts; ls -l; \
        for p in install*.swm; do wimlib-imagex info $p --header | grep -E '^(GUID|Part Number|Total Parts|Image Count|Boot Index|Flags)'; done; \
        wimlib-imagex info install.swm | grep -E 'Part Number|Image Count|Boot Index'; \
        wimlib-imagex verify install.swm --ref='install*.swm' && echo SWM-VERIFY-OK; \
        for i in 1 2 3; do wimlib-imagex apply /w/orig.wim $i /tmp/a$i >/dev/null; \
          wimlib-imagex apply install.swm $i /tmp/b$i --ref='install*.swm' >/dev/null; diff -r /tmp/a$i /tmp/b$i; done; echo APPLY-OK; \
        wimlib-imagex split /w/orig.wim /tmp/ref.swm 12 >/dev/null && ls -l /tmp/ref*.swm; \
        for k in '' 2 3 4; do [ -e install$k.swm ] || continue; \
          if cmp -s -i 208 install$k.swm /tmp/ref$k.swm; then echo SAME-AS-WIMLIB-SPLIT; else echo DIFFERS-FROM-WIMLIB-SPLIT; fi; done";
    let (code, log) = toolbox(&dir, false, script);
    eprintln!("{log}");
    assert_eq!(code, 0, "{log}");
    assert!(log.contains("SWM-VERIFY-OK") && log.contains("APPLY-OK"), "{log}");
    assert_eq!(log.matches("Total Parts").count(), parts.len());
    // wimlib's own split of the same WIM at the same size: same parts, byte for byte after the
    // header (whose GUID is random, and whose boot index wimlib leaves at 0).
    assert_eq!(log.matches("SAME-AS-WIMLIB-SPLIT").count(), parts.len(), "{log}");
    let guids: std::collections::HashSet<&str> = log.lines().filter(|l| l.starts_with("GUID")).collect();
    assert_eq!(guids.len(), 1, "one GUID shared by every part: {guids:?}");
}

#[test]
#[ignore]
fn fat32_split_media_verifies_and_boots_uefi_and_bios() {
    virtual_only();
    let dir = common::workdir("win-fat32");
    let media = make_media(&dir, 24, "LZX", 1);
    let disk = dir.join("fat32.img");
    let r = flash_request(&disk, 160 * MIB, json!({
        "bypass_hardware_checks": true,
        "local_account": {"name": "mario", "password": "Pa$$w0rd<&>"},
        "skip_privacy_questions": true, "locale": "it-IT", "time_zone": "Europe/Rome"
    }));
    let v = write_media(&r, &media, &Tuning { max_file: 8 * MIB, part_limit: 10 * MIB });
    assert_eq!(v["layout"], "fat32_split");
    assert_eq!(v["scheme"], "mbr");
    assert_eq!(v["windows"]["version"], "Windows 11");
    assert_eq!(v["windows"]["editions"], json!(["Windows 11 Home", "Windows 11 Pro", "Windows 11 Education"]));
    let n = v["split"]["parts"].as_array().unwrap().len();
    assert!((3..=4).contains(&n), "{n} parts");
    let (code, log) = toolbox(&dir, false, "cp media/sources/install.wim orig.wim");
    assert_eq!(code, 0, "{log}");
    let log = check_volume(&dir, "fat32.img", &v, 0, "fat32", Some("/w/orig.wim"));
    assert!(log.contains("SWM-VERIFY-OK") && log.contains("APPLY-OK 3 images") && log.contains("AUTOUNATTEND-PRESENT"), "{log}");
    // The answer file, read by Python's XML parser.
    let (code, js) = toolbox(&dir, false, "python3 -I /fx/check_unattend.py /w/autounattend.out.xml");
    assert_eq!(code, 0, "{js}");
    let u: Value = serde_json::from_str(js.trim()).unwrap();
    assert_eq!(u["labconfig"], json!(["BypassTPMCheck", "BypassSecureBootCheck", "BypassRAMCheck", "BypassCPUCheck", "BypassStorageCheck"]));
    assert_eq!(u["passes"]["windowsPE"], json!([["Microsoft-Windows-Setup", "amd64"]]));
    assert!(u["specialize"].as_array().unwrap().iter().any(|c| c.as_str().unwrap().contains("BypassNRO")));
    assert_eq!(u["accounts"][0], json!({"name": "mario", "group": "Administrators;Power Users", "plain_text": "false", "password": "Pa$$w0rd<&>"}));
    assert_eq!(u["oobe"]["ProtectYourPC"], "3");
    assert_eq!(u["oobe"]["HideOnlineAccountScreens"], "true");
    assert_eq!(u["international"]["UILanguage"], "en-US", "the image only has en-US");
    assert_eq!(u["international"]["UserLocale"], "it-IT");
    assert_eq!(u["time_zone"], "W. Europe Standard Time");
    // Boot it.
    boot(&dir, "fat32.img", "uefi", UEFI_MARKER, 240, 1);
    boot(&dir, "fat32.img", "bios", BIOS_MARKER, 240, 1);
}

#[test]
#[ignore]
fn ntfs_uefi_media_verifies_and_boots_through_uefi_ntfs_and_bios() {
    virtual_only();
    let dir = common::workdir("win-ntfs");
    let media = make_media(&dir, 12, "none", 1);
    // A >4 GiB file can't be faked cheaply on the source side; the layout is asked for.
    let disk = dir.join("ntfs.img");
    let r = flash_request(&disk, 128 * MIB, json!({"layout": "ntfs_uefi"}));
    let v = write_media(&r, &media, &Tuning::default());
    assert_eq!(v["layout"], "ntfs_uefi");
    assert_eq!(v["uefi_ntfs"], windows::uefi_ntfs::VERSION);
    assert!(v["split"].is_null() && v["autounattend"].is_null());
    check_volume(&dir, "ntfs.img", &v, 0, "ntfs", None);
    // Partition 2 is the UEFI:NTFS image, and a valid FAT volume.
    let (off, size) = partition(&v, 1);
    assert_eq!(size, MIB);
    let (code, log) = toolbox(&dir, false, &format!("dd if=/w/ntfs.img of=/tmp/p2 bs=1M skip={} count=1 2>/dev/null && sha256sum /tmp/p2 && fsck.vfat -n /tmp/p2 && mdir -i /tmp/p2 ::/efi/boot", off / MIB));
    assert_eq!(code, 0, "{log}");
    assert!(log.contains(windows::uefi_ntfs::SHA256), "{log}");
    let uefi = boot(&dir, "ntfs.img", "uefi", UEFI_MARKER, 300, 1);
    assert!(uefi.contains("UEFI:NTFS"), "UEFI:NTFS chain-loaded bootx64.efi from NTFS:\n{uefi}");
    boot(&dir, "ntfs.img", "bios", BIOS_MARKER, 300, 1);
    // Secure Boot (Microsoft keys): UEFI:NTFS is signed and starts; the unsigned test
    // bootx64.efi may then be refused.
    let sb = boot(&dir, "ntfs.img", "uefi-sb", "UEFI:NTFS", 300, 25);
    assert!(sb.contains("Secure Boot status: Enabled"), "{sb}");
    assert!(sb.contains("NTFS Driver 1.9"), "the signed NTFS driver loads under Secure Boot:\n{sb}");
    assert!(sb.contains("Security Violation") && !sb.contains(UEFI_MARKER), "the unsigned test loader is refused:\n{sb}");

    // UEFI only: GPT, same chain.
    let disk = dir.join("ntfs-gpt.img");
    let r = flash_request(&disk, 128 * MIB, json!({"layout": "ntfs_uefi", "boot": "uefi"}));
    let v = write_media(&r, &media, &Tuning::default());
    assert_eq!(v["scheme"], "gpt");
    check_volume(&dir, "ntfs-gpt.img", &v, 0, "ntfs", None);
    boot(&dir, "ntfs-gpt.img", "uefi", UEFI_MARKER, 300, 1);
}

#[test]
#[ignore]
fn an_esd_too_big_for_fat32_selects_ntfs_and_forced_fat32_is_refused() {
    virtual_only();
    let dir = common::workdir("win-esd");
    let media = make_media(&dir, 12, "none", 1);
    let (code, log) = toolbox(&dir, false, "set -e; wimlib-imagex export media/sources/install.wim all media/sources/install.esd --solid >/dev/null; \
        rm media/sources/install.wim; wimlib-imagex info media/sources/install.esd --header | grep -E '^(Version|Flags)'");
    assert_eq!(code, 0, "{log}");
    assert!(log.contains("0xe00"), "solid WIM version: {log}");
    let tune = Tuning { max_file: 4 * MIB, part_limit: 3 * MIB };
    let info = {
        let mut t = DirTree::new(&media);
        windows::detect(&mut t).unwrap().unwrap()
    };
    assert_eq!(info.image_format, "esd");
    assert_eq!(info.compression, "lzms");
    let disk = dir.join("esd.img");
    let r = flash_request(&disk, 96 * MIB, json!({"layout": "fat32_split"}));
    let ctx = JobCtx::detached(true);
    let t = target::open_for_write(&r.target, &ctx).unwrap();
    let e = windows::write_tree_tuned(&r, &mut DirTree::new(&media), &t, &ctx, &tune).unwrap_err().to_string();
    assert!(e.contains("install.esd") && e.contains("NTFS"), "{e}");
    drop(t);
    let r = flash_request(&disk, 96 * MIB, json!({}));
    let v = write_media(&r, &media, &tune);
    assert_eq!(v["layout"], "ntfs_uefi");
    check_volume(&dir, "esd.img", &v, 0, "ntfs", None);
    let (code, log) = toolbox(&dir, true, &format!(
        "set -e; L=$(losetup -f --show -o {} --sizelimit {} /w/esd.img); mkdir /m; mount -t ntfs3 -o ro $L /m; \
         wimlib-imagex verify /m/sources/install.esd && echo ESD-VERIFY-OK; umount /m; losetup -d $L",
        partition(&v, 0).0, partition(&v, 0).1));
    assert!(code == 0 && log.contains("ESD-VERIFY-OK"), "{log}");
}

/// Detaches the test's disk image even if the test fails.
struct Attached(String);
impl Drop for Attached {
    fn drop(&mut self) {
        let _ = target::detach(&self.0);
    }
}

#[test]
#[ignore]
fn attached_disk_image_is_written_through_the_disk_path() {
    virtual_only();
    let dir = common::workdir("win-attached");
    let media = make_media(&dir, 20, "LZX", 1);
    let img = dir.join("stick.img");
    common::make_sparse(&img, 192);
    let bsd = target::attach_image_for_tests(&img).unwrap();
    let guard = Attached(bsd.clone());
    let info = target::disk_info(&bsd).unwrap();
    assert!(info.disk_image && info.allowed, "{info:?}");
    let mut r = flash_request(&img, 0, json!({"local_account": {"name": "ospite"}}));
    r.target = TargetSpec::Disk { bsd: bsd.clone() };
    let v = write_media(&r, &media, &Tuning { max_file: 8 * MIB, part_limit: 9 * MIB });
    assert_eq!(v["layout"], "fat32_split");
    drop(guard);
    let (code, log) = toolbox(&dir, false, "cp media/sources/install.wim orig.wim");
    assert_eq!(code, 0, "{log}");
    check_volume(&dir, "stick.img", &v, 0, "fat32", Some("/w/orig.wim"));
    boot(&dir, "stick.img", "bios", BIOS_MARKER, 240, 1);
}

/// The real thing: an install.wim over 4 GiB, split with the real 3800 MiB limit onto FAT32.
/// Slow (about 5 GB of data is generated, written, read back and verified).
#[test]
#[ignore]
fn big_wim_over_4gib_is_split_with_the_real_limit() {
    virtual_only();
    let dir = common::workdir("win-big");
    let media = make_media(&dir, 4400, "none", 16);
    let size = std::fs::metadata(media.join("sources/install.wim")).unwrap().len();
    assert!(size > u32::MAX as u64, "install.wim is {size} bytes");
    let info = windows::detect(&mut DirTree::new(&media)).unwrap().unwrap();
    assert!(info.needs_split && info.split_blocker.is_none());
    assert_eq!(info.recommended_layout, Some(lumendisk_core::forge::plan::WindowsLayout::Fat32Split));
    let disk = dir.join("big.img");
    let r = flash_request(&disk, 6 * 1024 * MIB, json!({}));
    let v = write_media(&r, &media, &Tuning::default());
    let parts = v["split"]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert!(parts.iter().all(|p| p["size"].as_u64().unwrap() <= 3800 * MIB));
    let (code, log) = toolbox(&dir, false, "mv media/sources/install.wim orig.wim");
    assert_eq!(code, 0, "{log}");
    let log = check_volume(&dir, "big.img", &v, 0, "fat32", Some("/w/orig.wim"));
    assert!(log.contains("SWM-VERIFY-OK") && log.contains("APPLY-OK 3 images"), "{log}");
}
