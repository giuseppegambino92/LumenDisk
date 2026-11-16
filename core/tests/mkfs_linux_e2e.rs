//! LumenDisk's own mkfs.btrfs / mkfs.xfs against the real Linux tools and kernel.
//!
//! Every case formats an image file with LumenDisk, then, in the `lumendisk-toolbox` container
//! (privileged: loop devices and the kernel's own btrfs / xfs drivers):
//! 1. runs the upstream checker (`btrfs check` in both modes / `xfs_repair -n`, with and
//!    without `-f`) and dumps the superblock (`btrfs inspect-internal dump-super`,
//!    `xfs_db -c "sb 0" -c print`);
//! 2. checks `blkid` TYPE / LABEL / UUID;
//! 3. mounts read-write, creates directories, a few hundred files and one large file,
//!    unmounts;
//! 4. runs the checker again, remounts read-only and verifies what was written.
//!
//! Run with: `cargo test --release --test mkfs_linux_e2e -- --ignored --test-threads=1`
mod common;

use std::path::Path;
use std::sync::Arc;

use lumendisk_core::block::{BlockDev, FileDev};
use lumendisk_core::forge::MkfsOptions;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Fs {
    Btrfs,
    Xfs,
}

struct Case {
    name: &'static str,
    fs: Fs,
    size: u64,
    cluster: Option<u32>,
    label: &'static str,
    /// XFS only: logical sector size of the loop device (and of the filesystem).
    sector: u32,
}

/// Format `img` with LumenDisk.
fn lumen_format(img: &Path, c: &Case, uuid: [u8; 16]) {
    let f = std::fs::File::create(img).unwrap();
    f.set_len(c.size).unwrap(); // sparse
    drop(f);
    let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(img, false).unwrap());
    let opts = MkfsOptions { label: c.label.into(), cluster_size: c.cluster, uuid: Some(uuid) };
    let t0 = std::time::Instant::now();
    match c.fs {
        Fs::Btrfs => lumendisk_core::mkfs::btrfs::format(dev, &opts).unwrap(),
        Fs::Xfs if c.sector == 512 => lumendisk_core::mkfs::xfs::format(dev, &opts).unwrap(),
        Fs::Xfs => lumendisk_core::mkfs::xfs::format_with_sector(dev, &opts, c.sector).unwrap(),
    }
    eprintln!("[{}] formatted in {:?}", c.name, t0.elapsed());
}

fn uuid_string(u: &[u8; 16]) -> String {
    uuid::Uuid::from_bytes(*u).hyphenated().to_string()
}

/// The shell script run in the container. Every step prints `@@ <step> rc=<code>`.
fn script(c: &Case) -> String {
    let check = match c.fs {
        Fs::Btrfs => "step check_ro btrfs check --readonly /w/disk.img; \
                      step check_lowmem btrfs check --mode=lowmem /w/disk.img;"
            .to_string(),
        Fs::Xfs => "step repair_n xfs_repair -n /w/disk.img; \
                    step repair_nf xfs_repair -n -f /w/disk.img;"
            .to_string(),
    };
    let dump = match c.fs {
        Fs::Btrfs => "step dump_super btrfs inspect-internal dump-super -f /w/disk.img;",
        Fs::Xfs => "step xfs_db xfs_db -r -c 'sb 0' -c print -c 'agf 0' -c print -c 'agi 0' -c print /w/disk.img;",
    };
    let losetup = if c.sector == 512 { "losetup -f --show /w/disk.img".to_string() } else { format!("losetup -b {} -f --show /w/disk.img", c.sector) };
    // Big file: 100 MB, or a quarter of the filesystem on the smallest images.
    let big = (100 * 1000 * 1000).min(c.size / 4);
    format!(
        r##"
step() {{ name=$1; shift; echo ">>> $name: $*"; "$@" 2>&1; rc=$?; echo "@@ $name rc=$rc"; }}
cd /w
{check}
{dump}
step blkid blkid -p -o export /w/disk.img
LOOP=$({losetup})
echo "loop: $LOOP"
DEV=$(basename $LOOP)
T0=$(cut -d' ' -f1 /proc/uptime)
mkdir -p /mnt/t
step mount mount $LOOP /mnt/t
step populate sh -c 'mkdir -p /mnt/t/dir/sub/deeper && i=0; while [ $i -lt 400 ]; do echo "file number $i" > /mnt/t/dir/f$i; i=$((i+1)); done; ln -s dir/f1 /mnt/t/link && echo hi > /mnt/t/dir/sub/deeper/x'
step bigfile sh -c 'head -c {big} /dev/urandom > /mnt/t/big.bin && sha256sum /mnt/t/big.bin | cut -d" " -f1 > /w/big.sha'
step copy cp -a /mnt/t/dir /mnt/t/dir2
step rm sh -c 'rm -f /mnt/t/dir2/f1*'
step df df -k /mnt/t
step sync sync
step umount umount /mnt/t
losetup -d $LOOP 2>/dev/null
{check}
LOOP=$({losetup})
step mount_ro mount -o ro $LOOP /mnt/t
step verify sh -c 'test "$(sha256sum /mnt/t/big.bin | cut -d" " -f1)" = "$(cat /w/big.sha)" && test $(ls /mnt/t/dir | wc -l) -eq 401 && test $(ls /mnt/t/dir2 | wc -l) -eq 290 && test "$(cat /mnt/t/link)" = "file number 1"'
step umount_ro umount /mnt/t
losetup -d $LOOP 2>/dev/null
# Kernel messages about our loop device since the first mount (checked by the caller).
dmesg | awk -v t=$T0 '{{ ts = substr($0, 2, index($0, "]") - 2) + 0; if (ts >= t) print }}' | grep -E "[(: ]$DEV[):, ]" > /tmp/kmsg.txt
step kmsg cat /tmp/kmsg.txt
rm -f /w/big.sha
"##
    )
}

/// Parse `@@ step rc=N` lines: (step, rc) in order.
fn steps(out: &str) -> Vec<(String, i32)> {
    out.lines()
        .filter_map(|l| l.strip_prefix("@@ "))
        .filter_map(|l| {
            let (name, rc) = l.split_once(" rc=")?;
            Some((name.to_string(), rc.trim().parse().ok()?))
        })
        .collect()
}

/// The output of one step (between its `>>>` header and its `@@` line), first occurrence
/// after `skip` previous ones.
fn step_output<'a>(out: &'a str, name: &str, nth: usize) -> &'a str {
    let head = format!(">>> {name}:");
    let mut rest = out;
    for _ in 0..nth {
        match rest.find(&head) {
            Some(i) => rest = &rest[i + head.len()..],
            None => return "",
        }
    }
    let start = match rest.find(&head) {
        Some(i) => i,
        None => return "",
    };
    let body = &rest[start..];
    let end = body.find(&format!("@@ {name} rc=")).unwrap_or(body.len());
    &body[..end]
}

fn run(c: Case) {
    let dir = common::workdir(&format!("mkfs-e2e-{}", c.name));
    let img = dir.join("disk.img");
    let uuid = *uuid::Uuid::new_v4().as_bytes();
    lumen_format(&img, &c, uuid);
    let t0 = std::time::Instant::now();
    let (code, out) = common::docker_priv(&dir, &script(&c));
    let st = steps(&out);
    if std::env::var("MKFS_E2E_VERBOSE").is_ok() {
        eprintln!("{out}");
    }
    let fail = |why: String| -> ! {
        let _ = std::fs::remove_file(&img);
        panic!("[{}] {why}\n----- container output -----\n{out}", c.name)
    };
    if code != 0 {
        fail(format!("container exited with {code}"));
    }
    let expected: &[&str] = match c.fs {
        Fs::Btrfs => &["check_ro", "check_lowmem", "dump_super", "blkid", "mount", "populate", "bigfile", "copy", "rm", "df", "sync", "umount", "check_ro", "check_lowmem", "mount_ro", "verify", "umount_ro", "kmsg"],
        Fs::Xfs => &["repair_n", "repair_nf", "xfs_db", "blkid", "mount", "populate", "bigfile", "copy", "rm", "df", "sync", "umount", "repair_n", "repair_nf", "mount_ro", "verify", "umount_ro", "kmsg"],
    };
    let names: Vec<&str> = st.iter().map(|s| s.0.as_str()).collect();
    if names != expected {
        fail(format!("unexpected steps {names:?}"));
    }
    for (name, rc) in &st {
        if *rc != 0 {
            fail(format!("step {name} failed with rc={rc}"));
        }
    }
    // Checker output: no errors or warnings at all, before and after the kernel wrote.
    for nth in 0..2 {
        let checks: &[&str] = match c.fs {
            Fs::Btrfs => &["check_ro", "check_lowmem"],
            Fs::Xfs => &["repair_n", "repair_nf"],
        };
        for name in checks {
            let o = step_output(&out, name, nth);
            let bad = o.lines().any(|l| {
                let low = l.to_ascii_lowercase();
                (low.contains("error") && !low.contains("no error found"))
                    || low.contains("warning")
                    || low.contains("would ")
                    || low.contains("corrupt")
                    || low.contains("bad ")
                    || low.contains("mismatch")
                    || low.contains("invalid")
            });
            if bad {
                fail(format!("{name} (pass {}) reported problems", nth + 1));
            }
            if c.fs == Fs::Btrfs && !o.contains("no error found") {
                fail(format!("{name} (pass {}): no 'no error found'", nth + 1));
            }
        }
    }
    // The kernel had nothing to complain about while mounting, writing and unmounting.
    let kmsg = step_output(&out, "kmsg", 0);
    if kmsg.lines().skip(1).any(|l| {
        let low = l.to_ascii_lowercase();
        ["warn", "error", "corrupt", "fail", "bad", "invalid", "mismatch", "repair"].iter().any(|w| low.contains(w))
    }) {
        fail("kernel log has warnings / errors for the device".into());
    }
    if !kmsg.lines().skip(1).any(|l| l.contains("BTRFS") || l.contains("XFS")) {
        fail("no kernel log lines for the device (log filtering broken?)".into());
    }
    // Superblock dump sanity.
    match c.fs {
        Fs::Btrfs => {
            let o = step_output(&out, "dump_super", 0);
            if !o.contains("[match]") || o.contains("[DON'T MATCH]") {
                fail("dump-super: checksum / magic not matching".into());
            }
        }
        Fs::Xfs => {
            let o = step_output(&out, "xfs_db", 0);
            let crcs = o.lines().filter(|l| l.starts_with("crc = ")).count();
            let good = o.lines().filter(|l| l.starts_with("crc = ") && l.ends_with("(correct)")).count();
            if crcs != 3 || good != 3 {
                fail("xfs_db: sb/agf/agi crc not all correct".into());
            }
        }
    }
    // blkid.
    let b = step_output(&out, "blkid", 0);
    let want_type = match c.fs {
        Fs::Btrfs => "TYPE=btrfs",
        Fs::Xfs => "TYPE=xfs",
    };
    if !b.lines().any(|l| l == want_type) {
        fail(format!("blkid: no {want_type}"));
    }
    if !b.lines().any(|l| l == format!("UUID={}", uuid_string(&uuid))) {
        fail("blkid: wrong UUID".into());
    }
    if !c.label.is_empty() && !b.lines().any(|l| l == format!("LABEL={}", c.label.replace(' ', "\\ "))) {
        fail("blkid: wrong LABEL".into());
    }
    let df = step_output(&out, "df", 0).lines().last().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    eprintln!(
        "SUMMARY {:<26} {:?} size={} cluster={:?} sector={}: checker clean x2 ({}), blkid ok, mount rw + 400 files + {} MB file + copy/rm ok, remount verify ok [{:?}] df: {}",
        c.name,
        c.fs,
        c.size,
        c.cluster,
        c.sector,
        match c.fs {
            Fs::Btrfs => "btrfs check readonly + lowmem",
            Fs::Xfs => "xfs_repair -n, -n -f",
        },
        (100 * 1000 * 1000).min(c.size / 4) / 1_000_000,
        t0.elapsed(),
        df
    );
    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────── Btrfs ─────────────

#[test]
#[ignore]
fn btrfs_minimum_size() {
    run(Case { name: "btrfs-min", fs: Fs::Btrfs, size: lumendisk_core::mkfs::btrfs::MIN_DEV_SIZE, cluster: None, label: "MinBtrfs", sector: 512 });
}

#[test]
#[ignore]
fn btrfs_1g_default() {
    run(Case { name: "btrfs-1g", fs: Fs::Btrfs, size: GIB, cluster: None, label: "LumenBtrfs", sector: 512 });
}

#[test]
#[ignore]
fn btrfs_odd_size_4k_nodes() {
    run(Case { name: "btrfs-odd-4k", fs: Fs::Btrfs, size: 777 * MIB + 12345, cluster: Some(4096), label: "odd size", sector: 512 });
}

#[test]
#[ignore]
fn btrfs_64k_nodes() {
    run(Case { name: "btrfs-64k", fs: Fs::Btrfs, size: 2 * GIB + 4096 * 3, cluster: Some(65536), label: "big nodes", sector: 512 });
}

#[test]
#[ignore]
fn btrfs_sparse_50g() {
    run(Case { name: "btrfs-50g", fs: Fs::Btrfs, size: 50 * GIB, cluster: None, label: "Fifty", sector: 512 });
}

// ───────────── XFS ─────────────

#[test]
#[ignore]
fn xfs_minimum_size() {
    run(Case { name: "xfs-min", fs: Fs::Xfs, size: lumendisk_core::mkfs::xfs::MIN_DEV_SIZE, cluster: None, label: "MinXfs", sector: 512 });
}

#[test]
#[ignore]
fn xfs_1g_default() {
    run(Case { name: "xfs-1g", fs: Fs::Xfs, size: GIB, cluster: None, label: "LumenXfs", sector: 512 });
}

#[test]
#[ignore]
fn xfs_odd_size_1k_blocks() {
    run(Case { name: "xfs-odd-1k", fs: Fs::Xfs, size: GIB + 12345, cluster: Some(1024), label: "odd1k", sector: 512 });
}

#[test]
#[ignore]
fn xfs_2k_blocks() {
    run(Case { name: "xfs-2k", fs: Fs::Xfs, size: 700 * MIB + 2048 * 5, cluster: Some(2048), label: "two k", sector: 512 });
}

#[test]
#[ignore]
fn xfs_64k_blocks() {
    run(Case { name: "xfs-64k", fs: Fs::Xfs, size: 2 * GIB + 65536 * 3 + 100, cluster: Some(65536), label: "sixtyfour", sector: 512 });
}

#[test]
#[ignore]
fn xfs_4k_sectors() {
    run(Case { name: "xfs-4kn", fs: Fs::Xfs, size: 3 * GIB / 2, cluster: None, label: "fourKn", sector: 4096 });
}

#[test]
#[ignore]
fn xfs_sparse_50g() {
    run(Case { name: "xfs-50g", fs: Fs::Xfs, size: 50 * GIB, cluster: None, label: "Fifty", sector: 512 });
}
