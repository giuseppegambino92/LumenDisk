//! Forge format / doctor end to end: FormatRequests run as jobs against image files and
//! against disk images attached with `hdiutil`, then checked by independent tools: sgdisk,
//! parted, sfdisk and blkid for the tables, each filesystem's own fsck (and the Linux kernel's
//! ntfs3 for NTFS) in the `lumendisk-toolbox` container, `diskutil` on macOS. The doctor
//! suite corrupts tables and checks repairs and conversions the same way.
//!
//! Run: `cargo test --release -p lumendisk-core --test format_e2e -- --ignored --test-threads=1`
//! (needs Docker with the `lumendisk-toolbox` image).
//!
//! Safety: every test sets `LUMENDISK_FORGE_VIRTUAL_ONLY=1` first, so `forge::target` accepts
//! only attached disk images as `Disk` targets, and only images the test created are attached
//! (and always detached, see [`Attached`]).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use lumendisk_core::block::{BlockDev, FileDev};
use lumendisk_core::forge::job::{self, JobCtx, State};
use lumendisk_core::forge::plan::{DoctorAction, DoctorRequest, FormatRequest, PartSpec, Request, Scheme};
use lumendisk_core::forge::target::{self, TargetSpec};
use lumendisk_core::forge::{format, MkfsOptions, TargetFs};
use serde_json::Value;

const MIB: u64 = 1 << 20;

fn virtual_only() {
    std::env::set_var("LUMENDISK_FORGE_VIRTUAL_ONLY", "1");
}

/// Run a request as a job (like the app does) and return its result.
fn run_job(req: Request) -> Result<Value, String> {
    let id = lumendisk_core::forge::plan::start(req);
    for _ in 0..12000 {
        let s = job::status(id, 0).unwrap();
        if s.state != State::Running {
            job::forget(id);
            return match s.state {
                State::Succeeded => Ok(s.result.unwrap()),
                _ => Err(format!("{:?}: {:?}\n{}", s.state, s.error, s.logs.iter().map(|l| l.msg.clone()).collect::<Vec<_>>().join("\n"))),
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("job {id} did not finish in 10 minutes");
}

/// An image attached with `hdiutil attach -nomount`; detached on drop, also when the test
/// fails.
struct Attached(String);

impl Attached {
    fn new(img: &Path) -> Attached {
        Attached(target::attach_image_for_tests(img).expect("hdiutil attach"))
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        if let Err(e) = target::detach(&self.0) {
            eprintln!("detach {}: {e}", self.0);
        }
    }
}

fn diskutil(args: &[&str]) -> String {
    let o = Command::new("/usr/sbin/diskutil").args(args).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
}

fn part(fs: TargetFs, size: Option<u64>, label: &str, cluster: Option<u32>) -> PartSpec {
    PartSpec { fs, mkfs: MkfsOptions { label: label.into(), cluster_size: cluster, uuid: None }, size, name: None }
}

fn sparse(path: &Path, size: u64) {
    let f = std::fs::File::create(path).unwrap();
    f.set_len(size).unwrap();
}

/// What the toolbox has to find in an image.
struct Expect {
    /// File name inside the work dir.
    img: String,
    scheme: Scheme,
    sector: u64,
    parts: Vec<ExpectPart>,
}

struct ExpectPart {
    start: u64,
    size: u64,
    fs: TargetFs,
    label: String,
    cluster: Option<u64>,
}

fn expect_from(img: &str, scheme: Scheme, result: &Value) -> Expect {
    let parts = result["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| ExpectPart {
            start: p["start"].as_u64().unwrap(),
            size: p["size"].as_u64().unwrap(),
            fs: serde_json::from_value(p["fs"].clone()).unwrap(),
            label: p["label"].as_str().unwrap().to_string(),
            cluster: p["cluster_size"].as_u64(),
        })
        .collect();
    Expect { img: img.into(), scheme, sector: result["sector_size"].as_u64().unwrap(), parts }
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// One shell script checking every image; prints `OK …` / `FAIL …` lines.
fn verify_script(all: &[Expect]) -> String {
    let mut s = String::from(
        "set +e\nok() { echo \"OK $*\"; }\nfail() { echo \"FAIL $*\"; }\nmkdir -p /mnt/x\n\
         check() { name=\"$1\"; shift; if \"$@\" >/tmp/out 2>&1; then ok \"$name\"; else fail \"$name\"; sed 's/^/    /' /tmp/out | head -40; fi; }\n",
    );
    for e in all {
        let img = &e.img;
        let ss = e.sector;
        s += &format!("echo \"== {img}\"\nW=$(losetup -b {ss} -f --show {img})\n");
        let pt = match e.scheme {
            Scheme::Gpt => "gpt",
            Scheme::Mbr => "dos",
            Scheme::Apm => "mac",
            Scheme::None => "",
        };
        if !pt.is_empty() {
            s += &format!("check '{img} blkid PTTYPE={pt}' sh -c '[ \"$(blkid -p -o value -s PTTYPE $0)\" = {pt} ]' $W\n");
            let parted = match e.scheme {
                Scheme::Gpt => "gpt",
                Scheme::Mbr => "msdos",
                _ => "mac",
            };
            s += &format!("check '{img} parted label {parted}' sh -c 'parted -s -m $0 unit B print | grep -q \":{parted}:\"' $W\n");
        }
        if e.scheme == Scheme::Gpt {
            s += &format!("check '{img} sgdisk -v' sh -c 'sgdisk -v $0 | tee /dev/stderr | grep -q \"No problems found\"' $W\n");
        }
        for (i, p) in e.parts.iter().enumerate() {
            let n = i + 1;
            let (start, size, end) = (p.start, p.size, p.start + p.size - 1);
            if e.scheme != Scheme::None {
                s += &format!("check '{img} p{n} parted {start}+{size}' sh -c 'parted -s -m $0 unit B print | grep -q \":{start}B:{end}B:{size}B:\"' $W\n");
            }
            if matches!(e.scheme, Scheme::Gpt | Scheme::Mbr) {
                let (ls, lc) = (start / ss, size / ss);
                s += &format!("check '{img} p{n} sfdisk' sh -c 'sfdisk -d $0 | grep -Eq \"start= *{ls}, size= *{lc},\"' $W\n");
                s += &format!("check '{img} p{n} fdisk -l' sh -c 'fdisk -l $0 | grep -Eq \" {ls} +{} +{lc} \"' $W\n", ls + lc - 1);
            }
            s += &format!("L=$(losetup -b {ss} -f --show -o {start} --sizelimit {size} {img})\n");
            let fsck = match p.fs {
                TargetFs::Fat16 | TargetFs::Fat32 => "fsck.vfat -n -v $L".to_string(),
                TargetFs::Exfat => "fsck.exfat -n $L".to_string(),
                TargetFs::Ntfs => "ntfsfix -n $L".to_string(),
                _ => "e2fsck -fn $L".to_string(),
            };
            s += &format!("check '{img} p{n} {} fsck' sh -c 'L=$0; {fsck}' $L\n", p.fs.display_name());
            if let Some(c) = p.cluster {
                let probe = match p.fs {
                    TargetFs::Fat16 | TargetFs::Fat32 => format!("fsck.vfat -n -v $L | grep -Eq \"^ *{c} bytes per cluster\""),
                    TargetFs::Exfat => format!("dump.exfat $L | grep -Eq \"^Cluster size: *{c}$\""),
                    TargetFs::Ntfs => format!("ntfsinfo -m $L | grep -Eq \"Cluster Size: {c}$\""),
                    _ => format!("dumpe2fs -h $L 2>/dev/null | grep -Eq \"^Block size: *{c}$\""),
                };
                s += &format!("check '{img} p{n} cluster {c}' sh -c 'L=$0; {probe}' $L\n");
            }
            if p.fs == TargetFs::Fat16 {
                s += &format!("check '{img} p{n} FAT16 type' sh -c '[ \"$(blkid -p -o value -s VERSION $0)\" = FAT16 ]' $L\n");
            }
            let label = sh_quote(&p.label);
            s += &format!("check '{img} p{n} label' sh -c '[ \"$(blkid -p -o value -s LABEL $0)\" = \"$1\" ]' $L {label}\n");
            let kmod = match p.fs {
                TargetFs::Fat16 | TargetFs::Fat32 => "vfat",
                TargetFs::Exfat => "exfat",
                TargetFs::Ntfs => "ntfs3",
                TargetFs::Ext2 => "ext2",
                TargetFs::Ext3 => "ext3",
                _ => "ext4",
            };
            s += &format!("check '{img} p{n} kernel mount {kmod}' sh -c 'mount -t {kmod} -o ro $0 /mnt/x && ls -la /mnt/x && umount /mnt/x' $L\n");
            s += "losetup -d $L\n";
        }
        s += "losetup -d $W\n";
    }
    s
}

fn verify_in_toolbox(dir: &Path, all: &[Expect]) {
    let script = verify_script(all);
    std::fs::write(dir.join("verify.sh"), &script).unwrap();
    let (code, out) = common::docker_priv(dir, "sh /w/verify.sh");
    let oks = out.lines().filter(|l| l.starts_with("OK ")).count();
    let fails: Vec<&str> = out.lines().filter(|l| l.starts_with("FAIL ")).collect();
    eprintln!("toolbox: {oks} checks passed, {} failed (exit {code})", fails.len());
    if !fails.is_empty() || oks == 0 {
        eprintln!("{out}");
        panic!("toolbox verification failed: {fails:?}");
    }
}

/// The format matrix: every scheme × the 7 filesystems LumenDisk mounts, with labels, plus
/// cluster sizes across each filesystem's range; run as jobs on image files.
#[test]
#[ignore]
fn format_matrix_on_image_files_checked_by_linux_tools() {
    virtual_only();
    let dir = common::workdir("forge_format_matrix");
    let all = [TargetFs::Fat16, TargetFs::Fat32, TargetFs::Exfat, TargetFs::Ntfs, TargetFs::Ext2, TargetFs::Ext3, TargetFs::Ext4];
    let label_for = |fs: TargetFs| match fs {
        TargetFs::Fat16 | TargetFs::Fat32 => "Dati USB",
        TargetFs::Exfat => "Città 2024",
        TargetFs::Ntfs => "Archivio è più",
        _ => "lumen-éxt",
    };
    let mut cases: Vec<(String, Scheme, u64, Vec<PartSpec>)> = vec![];
    for scheme in [Scheme::Gpt, Scheme::Mbr, Scheme::Apm, Scheme::None] {
        for fs in all {
            let tag = format!("{scheme:?}-{}", fs.display_name()).to_lowercase();
            cases.push((tag, scheme, 96 * MIB, vec![part(fs, None, label_for(fs), None)]));
        }
    }
    let clusters: Vec<(TargetFs, u32, u64)> = vec![
        (TargetFs::Fat16, 2048, 96 * MIB),
        (TargetFs::Fat16, 65536, 400 * MIB),
        (TargetFs::Fat32, 512, 64 * MIB),
        (TargetFs::Fat32, 4096, 600 * MIB),
        (TargetFs::Fat32, 32768, 2100 * MIB),
        (TargetFs::Fat32, 65536, 4200 * MIB),
        (TargetFs::Exfat, 512, 64 * MIB),
        (TargetFs::Exfat, 131072, 600 * MIB),
        (TargetFs::Exfat, 32 << 20, 1024 * MIB),
        (TargetFs::Ntfs, 512, 96 * MIB),
        (TargetFs::Ntfs, 2048, 96 * MIB),
        (TargetFs::Ntfs, 16384, 200 * MIB),
        (TargetFs::Ntfs, 65536, 300 * MIB),
        (TargetFs::Ntfs, 2 << 20, 600 * MIB),
        (TargetFs::Ext2, 1024, 96 * MIB),
        (TargetFs::Ext3, 2048, 96 * MIB),
        (TargetFs::Ext4, 1024, 300 * MIB),
        (TargetFs::Ext4, 2048, 300 * MIB),
        (TargetFs::Ext4, 4096, 96 * MIB),
    ];
    for (fs, c, size) in clusters {
        let tag = format!("cluster-{}-{c}", fs.display_name()).to_lowercase();
        cases.push((tag, Scheme::Gpt, size, vec![part(fs, None, label_for(fs), Some(c))]));
    }
    // FAT32 well above 32 GB (Windows refuses to create it; it is valid), sparse.
    cases.push(("fat32-64g".into(), Scheme::Gpt, 64 << 30, vec![part(TargetFs::Fat32, None, "BIG", None)]));
    // Several partitions per scheme.
    let multi = |last: Option<u64>| {
        vec![part(TargetFs::Fat32, Some(64 * MIB), "EFI", None), part(TargetFs::Ntfs, Some(80 * MIB), "Windows", Some(8192)), part(TargetFs::Exfat, Some(40 * MIB), "Scambio", None), part(TargetFs::Ext4, last, "linux", None)]
    };
    cases.push(("multi-gpt".into(), Scheme::Gpt, 256 * MIB, multi(None)));
    cases.push(("multi-mbr".into(), Scheme::Mbr, 256 * MIB, multi(None)));
    cases.push(("multi-apm".into(), Scheme::Apm, 256 * MIB, multi(Some(48 * MIB))));

    let mut expects = vec![];
    for (tag, scheme, size, parts) in cases {
        let name = format!("{tag}.img");
        let path = dir.join(&name);
        let req = FormatRequest { target: TargetSpec::File { path: path.clone(), size: Some(size) }, scheme, partitions: parts, clear_signatures: true };
        let res = run_job(Request::Format(req)).unwrap_or_else(|e| panic!("{tag}: {e}"));
        assert_eq!(res["partitions"].as_array().unwrap().len(), res["partitions"].as_array().unwrap().len());
        expects.push(expect_from(&name, scheme, &res));
    }
    eprintln!("{} images formatted", expects.len());
    verify_in_toolbox(&dir, &expects);
}

/// A 4K-native disk (the tables in 4096-byte LBAs, FAT / exFAT with 4096-byte sectors),
/// checked through Linux loop devices with 4096-byte logical blocks.
#[test]
#[ignore]
fn format_on_4kn_disks_checked_by_linux_tools() {
    virtual_only();
    struct Sector4k(FileDev);
    impl BlockDev for Sector4k {
        fn read_at(&self, o: u64, b: &mut [u8]) -> std::io::Result<()> {
            self.0.read_at(o, b)
        }
        fn write_at(&self, o: u64, b: &[u8]) -> std::io::Result<()> {
            self.0.write_at(o, b)
        }
        fn size(&self) -> u64 {
            self.0.size()
        }
        fn flush(&self) -> std::io::Result<()> {
            self.0.flush()
        }
        fn read_only(&self) -> bool {
            false
        }
        fn logical_sector_size(&self) -> Option<u32> {
            Some(4096)
        }
    }
    let dir = common::workdir("forge_format_4kn");
    let mut expects = vec![];
    let cases: Vec<(&str, Scheme, Vec<PartSpec>)> = vec![
        ("4kn-gpt", Scheme::Gpt, vec![part(TargetFs::Fat32, Some(300 * MIB), "EFI4K", None), part(TargetFs::Exfat, Some(200 * MIB), "Dati", Some(65536)), part(TargetFs::Ext4, None, "linux", None)]),
        ("4kn-mbr", Scheme::Mbr, vec![part(TargetFs::Exfat, Some(300 * MIB), "Dati", None), part(TargetFs::Fat32, None, "F", Some(4096))]),
        ("4kn-none", Scheme::None, vec![part(TargetFs::Exfat, None, "Floppy", None)]),
    ];
    for (tag, scheme, parts) in cases {
        let name = format!("{tag}.img");
        let path = dir.join(&name);
        sparse(&path, 800 * MIB);
        let dev: Arc<dyn BlockDev> = Arc::new(Sector4k(FileDev::open(&path, false).unwrap()));
        let req = FormatRequest { target: TargetSpec::File { path: path.clone(), size: None }, scheme, partitions: parts, clear_signatures: true };
        let res = format::format_device(dev, &name, &req, &JobCtx::detached(true)).unwrap();
        assert_eq!(res["sector_size"], 4096);
        expects.push(expect_from(&name, scheme, &res));
    }
    verify_in_toolbox(&dir, &expects);
}

/// What `diskutil list` shows for an attached disk: (line, identifier) per data partition
/// (the APM map and free-space entries left out).
fn diskutil_partitions(bsd: &str) -> Vec<(String, String)> {
    let out = diskutil(&["list", bsd]);
    out.lines()
        .filter_map(|l| {
            let l = l.trim();
            let (num, rest) = l.split_once(':')?;
            if num.trim().parse::<u32>().ok()? == 0 || rest.contains("Apple_partition_map") || rest.contains("Apple_Free") {
                return None;
            }
            let ident = rest.split_whitespace().last()?.to_string();
            Some((rest.trim().to_string(), ident))
        })
        .collect()
}

fn diskutil_info(dev: &str) -> String {
    diskutil(&["info", dev])
}

/// FormatRequests on `Disk` targets (attached images), then what macOS sees, then the
/// toolbox on the detached image.
#[test]
#[ignore]
fn format_attached_disk_images_seen_by_macos() {
    virtual_only();
    let dir = common::workdir("forge_format_disk");
    let cases: Vec<(&str, Scheme, Vec<PartSpec>, Vec<&str>)> = vec![
        (
            "disk-gpt",
            Scheme::Gpt,
            vec![part(TargetFs::Fat32, Some(64 * MIB), "EFIDATA", None), part(TargetFs::Exfat, Some(64 * MIB), "Scambio", Some(65536)), part(TargetFs::Ntfs, None, "Windows", None)],
            vec!["Microsoft Basic Data", "Microsoft Basic Data", "Microsoft Basic Data"],
        ),
        ("disk-mbr", Scheme::Mbr, vec![part(TargetFs::Fat32, Some(64 * MIB), "MBRFAT", None), part(TargetFs::Ext4, None, "linux", None)], vec!["Windows_FAT_32", "Linux"]),
        ("disk-apm", Scheme::Apm, vec![part(TargetFs::Fat32, Some(64 * MIB), "APMFAT", None), part(TargetFs::Exfat, None, "APMEX", None)], vec!["DOS_FAT_32", "Windows_NTFS"]),
        ("disk-fat16-mbr", Scheme::Mbr, vec![part(TargetFs::Fat16, None, "VECCHIO", None)], vec!["FAT_16"]),
    ];
    let mut expects = vec![];
    for (tag, scheme, parts, want_types) in cases {
        let name = format!("{tag}.img");
        let path = dir.join(&name);
        sparse(&path, 200 * MIB);
        let res = {
            let disk = Attached::new(&path);
            let info = target::disk_info(&disk.0).unwrap();
            assert!(info.allowed, "{:?}", info.refusal);
            let req = FormatRequest { target: TargetSpec::Disk { bsd: disk.0.clone() }, scheme, partitions: parts.clone(), clear_signatures: true };
            let res = run_job(Request::Format(req)).unwrap_or_else(|e| panic!("{tag}: {e}"));
            // macOS re-reads the table once the job closed the disk.
            let mut got = vec![];
            for _ in 0..50 {
                got = diskutil_partitions(&disk.0);
                if got.len() == want_types.len() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            eprintln!("{tag}:\n{}", diskutil(&["list", &disk.0]));
            assert_eq!(got.len(), want_types.len(), "{tag}: diskutil list: {got:?}");
            for (k, ((line, ident), want)) in got.iter().zip(&want_types).enumerate() {
                assert!(line.contains(want), "{tag} p{}: {line:?} should be {want}", k + 1);
                let info = diskutil_info(ident);
                let p = &res["partitions"][k];
                let off = format!("Partition Offset:          {} Bytes", p["start"]);
                assert!(info.contains(&off), "{tag} {ident}: offset\n{info}");
                let fs = p["fs"].as_str().unwrap();
                let personality = match fs {
                    "fat32" => Some("MS-DOS FAT32"),
                    "fat16" => Some("MS-DOS FAT16"),
                    "exfat" => Some("ExFAT"),
                    "ntfs" => Some("NTFS"),
                    _ => None,
                };
                if let Some(per) = personality {
                    assert!(info.contains(per), "{tag} {ident}: macOS should recognise {per}\n{info}");
                    let label = p["label"].as_str().unwrap();
                    assert!(info.contains(label), "{tag} {ident}: label {label}\n{info}");
                }
            }
            res
        };
        expects.push(expect_from(&name, scheme, &res));
    }
    verify_in_toolbox(&dir, &expects);
}

/// APM, GPT and MBR images written to files (no `diskutil` involved in writing), then
/// attached: macOS must read each table natively.
#[test]
#[ignore]
fn macos_reads_tables_written_to_files() {
    virtual_only();
    let dir = common::workdir("forge_format_macos_read");
    for (scheme, want) in [(Scheme::Apm, "Apple_partition_scheme"), (Scheme::Gpt, "GUID_partition_scheme"), (Scheme::Mbr, "FDisk_partition_scheme")] {
        let path = dir.join(format!("{scheme:?}.img"));
        let parts = vec![part(TargetFs::Fat32, Some(48 * MIB), "UNO", None), part(TargetFs::Exfat, Some(32 * MIB), "Due", None), part(TargetFs::Ext4, None, "tre", None)];
        let req = FormatRequest { target: TargetSpec::File { path: path.clone(), size: Some(160 * MIB) }, scheme, partitions: parts, clear_signatures: true };
        run_job(Request::Format(req)).unwrap();
        let disk = Attached::new(&path);
        let list = diskutil(&["list", &disk.0]);
        eprintln!("{list}");
        assert!(list.contains(want), "{scheme:?}: {list}");
        let got = diskutil_partitions(&disk.0);
        assert_eq!(got.len(), 3, "{scheme:?}: {got:?}");
        assert!(diskutil_info(&got[0].1).contains("MS-DOS FAT32"));
        assert!(diskutil_info(&got[1].1).contains("ExFAT"));
    }
}

/// Unsupported filesystems and impossible requests fail as jobs with a clear message and
/// leave the target as it was.
#[test]
#[ignore]
fn failing_requests_leave_the_target_alone() {
    virtual_only();
    let dir = common::workdir("forge_format_fail");
    let path = dir.join("keep.img");
    sparse(&path, 64 * MIB);
    std::fs::write(&path, vec![0x42u8; 4096]).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(64 * MIB).unwrap();
    for parts in [vec![part(TargetFs::Fat16, None, "", Some(65536))], vec![part(TargetFs::Fat32, None, "", Some(65536))], vec![part(TargetFs::Ext4, Some(80 * MIB), "", None)]] {
        let req = FormatRequest { target: TargetSpec::File { path: path.clone(), size: None }, scheme: Scheme::Gpt, partitions: parts, clear_signatures: true };
        let e = run_job(Request::Format(req)).unwrap_err();
        eprintln!("{e}");
        assert!(e.contains("Failed"));
        assert_eq!(std::fs::read(&path).unwrap()[..4096], [0x42; 4096]);
    }
    // A physical disk is refused in virtual-only mode, whatever it is.
    let req = FormatRequest { target: TargetSpec::Disk { bsd: "disk0".into() }, scheme: Scheme::Gpt, partitions: vec![part(TargetFs::Exfat, None, "", None)], clear_signatures: true };
    assert!(run_job(Request::Format(req)).is_err());
}

fn doctor(target: TargetSpec, action: DoctorAction) -> Result<Value, String> {
    run_job(Request::Doctor(DoctorRequest { target, action }))
}

fn file_target(p: &Path) -> TargetSpec {
    TargetSpec::File { path: PathBuf::from(p), size: None }
}

fn finding_codes(report: &Value) -> Vec<String> {
    report["findings"].as_array().unwrap().iter().map(|f| f["code"].as_str().unwrap().to_string()).collect()
}

/// Patch bytes of an image file in place.
fn patch(path: &Path, off: u64, data: &[u8]) {
    use std::os::unix::fs::FileExt;
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.write_all_at(data, off).unwrap();
}

fn read_at(path: &Path, off: u64, len: usize) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).unwrap();
    let mut b = vec![0u8; len];
    f.read_exact_at(&mut b, off).unwrap();
    b
}

fn set_len(path: &Path, len: u64) {
    std::fs::OpenOptions::new().write(true).open(path).unwrap().set_len(len).unwrap();
}

/// Four real filesystems on a GPT image, the table damaged in every way the doctor repairs;
/// after each repair sgdisk finds no problem and every filesystem passes its fsck.
#[test]
#[ignore]
fn doctor_repairs_damaged_gpt_images() {
    virtual_only();
    let dir = common::workdir("forge_doctor_repair");
    let size = 256 * MIB;
    let master = dir.join("master.img");
    let parts = vec![part(TargetFs::Fat32, Some(64 * MIB), "EFI", None), part(TargetFs::Ntfs, Some(64 * MIB), "Windows", None), part(TargetFs::Exfat, Some(40 * MIB), "Dati", None), part(TargetFs::Ext4, Some(64 * MIB), "linux", None)];
    let req = FormatRequest { target: TargetSpec::File { path: master.clone(), size: Some(size) }, scheme: Scheme::Gpt, partitions: parts, clear_signatures: true };
    let res = run_job(Request::Format(req)).unwrap();
    let lbas = size / 512;
    type Damage = Box<dyn Fn(&Path)>;
    let cases: Vec<(&str, &str, Damage)> = vec![
        ("primary-header", "gpt_primary_missing", Box::new(|p: &Path| patch(p, 512, &[0u8; 512]))),
        ("primary-entries", "gpt_primary_entries_crc", Box::new(|p: &Path| patch(p, 1024 + 130, &[0x42]))),
        ("primary-crc", "gpt_primary_header_crc", Box::new(|p: &Path| patch(p, 512 + 16, &[1, 2, 3, 4]))),
        ("backup-header", "gpt_backup_missing", Box::new(move |p: &Path| patch(p, (lbas - 1) * 512, &[0u8; 512]))),
        ("backup-entries", "gpt_backup_entries_crc", Box::new(move |p: &Path| patch(p, (lbas - 33) * 512 + 7, &[0x42]))),
        ("pmbr", "pmbr_missing", Box::new(|p: &Path| patch(p, 0, &[0u8; 512]))),
        // dd of the image onto a disk 128 MiB larger.
        ("grown", "gpt_backup_not_at_end", Box::new(move |p: &Path| set_len(p, size + 128 * MIB))),
        // …and onto one 8 MiB smaller (the partitions leave 23 MiB free at the end).
        ("shrunk", "gpt_beyond_end", Box::new(move |p: &Path| set_len(p, size - 8 * MIB))),
    ];
    let mut expects = vec![];
    for (tag, code, damage) in cases {
        let name = format!("repair-{tag}.img");
        let path = dir.join(&name);
        std::fs::copy(&master, &path).unwrap();
        damage(&path);
        let check = doctor(file_target(&path), DoctorAction::Check).unwrap();
        let codes = finding_codes(&check);
        assert!(codes.iter().any(|c| c == code), "{tag}: {codes:?}");
        assert_eq!(check["healthy"], false);
        assert_eq!(check["repairable"], true, "{tag}: {check}");
        let r = doctor(file_target(&path), DoctorAction::Repair).unwrap_or_else(|e| panic!("{tag}: {e}"));
        eprintln!("{tag}: {}", r["actions"]);
        assert_eq!(r["after"]["healthy"], true, "{tag}: {r}");
        assert_eq!(r["after"]["partitions"].as_array().unwrap().len(), 4);
        expects.push(expect_from(&name, Scheme::Gpt, &res));
    }
    // Ambiguous: both copies valid but different → refused, file unchanged.
    let path = dir.join("ambiguous.img");
    std::fs::copy(&master, &path).unwrap();
    let mut arr = read_at(&path, (lbas - 33) * 512, 16384);
    arr[56] = b'Z';
    let crc = lumendisk_core::forge::layout::crc32(&arr);
    patch(&path, (lbas - 33) * 512, &arr);
    let mut hdr = read_at(&path, (lbas - 1) * 512, 512);
    hdr[88..92].copy_from_slice(&crc.to_le_bytes());
    hdr[16..20].fill(0);
    let hcrc = lumendisk_core::forge::layout::crc32(&hdr[..92]);
    hdr[16..20].copy_from_slice(&hcrc.to_le_bytes());
    patch(&path, (lbas - 1) * 512, &hdr);
    let before = std::fs::read(&path).unwrap();
    assert!(finding_codes(&doctor(file_target(&path), DoctorAction::Check).unwrap()).contains(&"gpt_mismatch".to_string()));
    let e = doctor(file_target(&path), DoctorAction::Repair).unwrap_err();
    eprintln!("ambiguous: {e}");
    assert!(std::fs::read(&path).unwrap() == before, "refused repair wrote nothing");
    verify_in_toolbox(&dir, &expects);
}

/// MBR ⇄ GPT in place on images with real filesystems (ours and ones made by the Linux tools,
/// with logical partitions): extents identical, every filesystem still passes fsck.
#[test]
#[ignore]
fn doctor_converts_mbr_and_gpt_in_place() {
    virtual_only();
    let dir = common::workdir("forge_doctor_convert");
    let size = 256 * MIB;
    // Ours: 4 primaries, the last one leaving room at the end for the backup GPT.
    let path = dir.join("ours.img");
    let parts = vec![part(TargetFs::Fat32, Some(64 * MIB), "UNO", None), part(TargetFs::Ntfs, Some(48 * MIB), "Due", None), part(TargetFs::Exfat, Some(40 * MIB), "Tre", None), part(TargetFs::Ext4, Some(64 * MIB), "quattro", None)];
    let req = FormatRequest { target: TargetSpec::File { path: path.clone(), size: Some(size) }, scheme: Scheme::Mbr, partitions: parts, clear_signatures: true };
    let res = run_job(Request::Format(req)).unwrap();
    let r = doctor(file_target(&path), DoctorAction::ConvertToGpt).unwrap();
    assert_eq!(r["after"]["scheme"], "gpt", "{r}");
    std::fs::copy(&path, dir.join("ours-gpt.img")).unwrap();
    let r = doctor(file_target(&path), DoctorAction::ConvertToMbr).unwrap();
    assert_eq!(r["after"]["scheme"], "mbr", "{r}");
    let types: Vec<&str> = r["after"]["partitions"].as_array().unwrap().iter().map(|p| p["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["0x0C", "0x07", "0x07", "0x83"], "types from the filesystems found");
    std::fs::copy(&path, dir.join("ours-mbr.img")).unwrap();
    // A rest-of-disk MBR partition leaves no room for the backup GPT: refused, untouched.
    let full = dir.join("full.img");
    let req = FormatRequest { target: TargetSpec::File { path: full.clone(), size: Some(64 * MIB) }, scheme: Scheme::Mbr, partitions: vec![part(TargetFs::Exfat, None, "", None)], clear_signatures: true };
    run_job(Request::Format(req)).unwrap();
    let before = std::fs::read(&full).unwrap();
    let e = doctor(file_target(&full), DoctorAction::ConvertToGpt).unwrap_err();
    eprintln!("full: {e}");
    assert!(e.contains("33"));
    assert!(std::fs::read(&full).unwrap() == before);

    // Linux's: sfdisk with an extended partition and two logical ones, mkfs by the Linux tools.
    let script = "set -e; truncate -s 256M linux.img; \
        printf 'label: dos\\nstart=2048, size=131072, type=c, bootable\\nstart=133120, type=5\\nstart=135168, size=65536, type=83\\nstart=202752, size=65536, type=7\\n' | sfdisk -q linux.img; \
        sfdisk -d linux.img; \
        L=$(losetup -f --show -o $((2048*512)) --sizelimit $((131072*512)) linux.img); mkfs.vfat -F 32 -n LINUXFAT $L >/dev/null; losetup -d $L; \
        L=$(losetup -f --show -o $((135168*512)) --sizelimit $((65536*512)) linux.img); mkfs.ext4 -q -L logico $L; losetup -d $L; \
        L=$(losetup -f --show -o $((202752*512)) --sizelimit $((65536*512)) linux.img); mkfs.exfat -L logico2 $L >/dev/null; losetup -d $L";
    let (code, out) = common::docker_priv(&dir, script);
    assert_eq!(code, 0, "{out}");
    let linux = dir.join("linux.img");
    let check = doctor(file_target(&linux), DoctorAction::Check).unwrap();
    assert_eq!(check["scheme"], "mbr");
    assert_eq!(check["partitions"].as_array().unwrap().len(), 3, "{check}");
    assert_eq!(check["healthy"], true, "{check}");
    let r = doctor(file_target(&linux), DoctorAction::ConvertToGpt).unwrap();
    assert_eq!(r["after"]["healthy"], true, "{r}");

    let mut expects = vec![];
    let mut gpt = expect_from("ours-gpt.img", Scheme::Gpt, &res);
    let mut mbr = expect_from("ours-mbr.img", Scheme::Mbr, &res);
    for e in gpt.parts.iter_mut().chain(mbr.parts.iter_mut()) {
        e.cluster = None;
    }
    expects.push(gpt);
    expects.push(mbr);
    let lp = |start: u64, count: u64, fs: TargetFs, label: &str| ExpectPart { start: start * 512, size: count * 512, fs, label: label.into(), cluster: None };
    expects.push(Expect { img: "linux.img".into(), scheme: Scheme::Gpt, sector: 512, parts: vec![lp(2048, 131072, TargetFs::Fat32, "LINUXFAT"), lp(135168, 65536, TargetFs::Ext4, "logico"), lp(202752, 65536, TargetFs::Exfat, "logico2")] });
    verify_in_toolbox(&dir, &expects);
}

/// The doctor on a `Disk` target (an attached image): check, repair a damaged backup GPT,
/// macOS sees the partitions afterwards.
#[test]
#[ignore]
fn doctor_on_an_attached_disk_image() {
    virtual_only();
    let dir = common::workdir("forge_doctor_disk");
    let path = dir.join("disk.img");
    let size = 160 * MIB;
    let parts = vec![part(TargetFs::Fat32, Some(64 * MIB), "DOCTOR", None), part(TargetFs::Exfat, None, "Paziente", None)];
    let req = FormatRequest { target: TargetSpec::File { path: path.clone(), size: Some(size) }, scheme: Scheme::Gpt, partitions: parts, clear_signatures: true };
    let res = run_job(Request::Format(req)).unwrap();
    patch(&path, size - 512, &[0u8; 512]);
    {
        let disk = Attached::new(&path);
        let t = TargetSpec::Disk { bsd: disk.0.clone() };
        let check = doctor(t.clone(), DoctorAction::Check).unwrap();
        assert!(finding_codes(&check).contains(&"gpt_backup_missing".to_string()), "{check}");
        let r = doctor(t.clone(), DoctorAction::Repair).unwrap();
        assert_eq!(r["after"]["healthy"], true, "{r}");
        let mut got = vec![];
        for _ in 0..50 {
            got = diskutil_partitions(&disk.0);
            if got.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        eprintln!("{}", diskutil(&["list", &disk.0]));
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(diskutil_info(&got[1].1).contains("ExFAT"));
    }
    verify_in_toolbox(&dir, &[expect_from("disk.img", Scheme::Gpt, &res)]);
}
