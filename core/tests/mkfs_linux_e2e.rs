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
        // xfs_logprint must find our unmount record where the superblock says the log is:
        // a log the kernel can't find gets overwritten by file data, and the unmount then
        // hangs forever in the kernel (seen while developing).
        Fs::Xfs => "step xfs_db xfs_db -r -c 'sb 0' -c print -c 'agf 0' -c print -c 'agi 0' -c print /w/disk.img; \
                    step logprint sh -c 'xfs_logprint -t /w/disk.img 2>&1; xfs_logprint -t /w/disk.img 2>&1 | grep -q \"state: <CLEAN>\"';",
    };
    let losetup = if c.sector == 512 { "losetup -f --show /w/disk.img".to_string() } else { format!("losetup -b {} -f --show /w/disk.img", c.sector) };
    // Big file: 100 MB, or a quarter of the filesystem on the smallest images.
    let big = (100 * 1000 * 1000).min(c.size / 4);
    format!(
        r##"
step() {{ name=$1; shift; echo ">>> $name: $*"; "$@" 2>&1; rc=$?; echo "@@ $name rc=$rc"; [ $rc -eq 0 ] || FAILED=1; }}
# Run a command that may block in the kernel (sync, umount) for at most 120 s; a stuck one
# can't be killed, so report it and stop instead of waiting forever.
bounded() {{ "$@" & p=$!; i=0; while kill -0 $p 2>/dev/null; do i=$((i+1)); if [ $i -gt 1200 ]; then echo "HUNG: $* (kernel task stuck)"; exit 3; fi; sleep 0.1; done; wait $p; }}
cd /w
{check}
{dump}
step blkid blkid -p -o export /w/disk.img
# Never hand the kernel a filesystem the tools already rejected.
[ -z "$FAILED" ] || exit 0
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
step sync bounded sync -f /mnt/t
step umount bounded umount /mnt/t
losetup -d $LOOP 2>/dev/null
{check}
LOOP=$({losetup})
step mount_ro mount -o ro $LOOP /mnt/t
step verify sh -c 'test "$(sha256sum /mnt/t/big.bin | cut -d" " -f1)" = "$(cat /w/big.sha)" && test $(ls /mnt/t/dir | wc -l) -eq 401 && test $(ls /mnt/t/dir2 | wc -l) -eq 290 && test "$(cat /mnt/t/link)" = "file number 1"'
step umount_ro bounded umount /mnt/t
losetup -d $LOOP 2>/dev/null
# Kernel messages about our loop device since the first mount (checked by the caller).
dmesg | awk -v t=$T0 '{{ ts = substr($0, 2, index($0, "]") - 2) + 0; if (ts >= t) print }}' | grep -E "[(: ]$DEV[):, ]" > /tmp/kmsg.txt
step kmsg cat /tmp/kmsg.txt
rm -f /w/big.sha
"##
    )
}

/// Like `common::docker_priv`, but gives up after `secs`: a filesystem the kernel chokes on
/// can leave a task in uninterruptible sleep, whose container then never exits. The test
/// fails (naming the container) instead of hanging the whole suite.
fn docker_priv_bounded(dir: &Path, script: &str, secs: u64) -> (i32, String) {
    use std::process::{Command, Stdio};
    let name = format!("lumendisk-mkfs-e2e-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let out_path = dir.with_extension("out");
    let out = std::fs::File::create(&out_path).unwrap();
    let mut child = Command::new("docker")
        .args(["run", "--rm", "--privileged", "--name", &name, "-v"])
        .arg(format!("{}:/w", dir.display()))
        .args(["-w", "/w", "lumendisk-toolbox", "sh", "-c", script])
        .stdin(Stdio::null())
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .expect("docker not available");
    let t0 = std::time::Instant::now();
    let code = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st.code().unwrap_or(-1);
        }
        if t0.elapsed() > std::time::Duration::from_secs(secs) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = Command::new("docker").args(["kill", &name]).output();
            let text = std::fs::read_to_string(&out_path).unwrap_or_default();
            let _ = std::fs::remove_file(&out_path);
            return (-2, format!("{text}\nTIMEOUT after {secs} s: container {name} may be stuck in the kernel"));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let text = std::fs::read_to_string(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    (code, text)
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
    let (code, out) = docker_priv_bounded(&dir, &script(&c), 900);
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
        Fs::Xfs => &["repair_n", "repair_nf", "xfs_db", "logprint", "blkid", "mount", "populate", "bigfile", "copy", "rm", "df", "sync", "umount", "repair_n", "repair_nf", "mount_ro", "verify", "umount_ro", "kmsg"],
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
            // xfs_repair on an image file always notes it can't ask the host for the sector
            // size; that is not about the filesystem.
            let host_note = ["Cannot get host filesystem geometry.", "Repair may fail if there is a sector size mismatch between", "the image and the host filesystem."];
            let bad = o.lines().filter(|l| !host_note.contains(&l.trim())).any(|l| {
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

#[test]
#[ignore]
fn xfs_32k_blocks_minimum_size() {
    // The smallest filesystem mkfs.xfs makes with 32 KiB blocks: the log is at the kernel
    // minimum for this AG size (2213 blocks), not the 64 MiB default.
    run(Case { name: "xfs-32k-min", fs: Fs::Xfs, size: lumendisk_core::mkfs::xfs::MIN_DEV_SIZE, cluster: Some(32768), label: "thirtytwo", sector: 512 });
}

#[test]
#[ignore]
fn xfs_16k_blocks_4k_sectors() {
    run(Case { name: "xfs-16k-4kn", fs: Fs::Xfs, size: 5 * GIB + 16384 * 7, cluster: Some(16384), label: "16k4kn", sector: 4096 });
}

#[test]
#[ignore]
fn btrfs_sparse_300g_third_mirror() {
    // Large enough for the third superblock copy at 256 GiB.
    run(Case { name: "btrfs-300g", fs: Fs::Btrfs, size: 300 * GIB, cluster: Some(32768), label: "Three mirrors", sector: 512 });
}

/// Same size, block size, sector, label and UUID formatted by mkfs.xfs 6.13: every AG's
/// superblock, AGF, AGI and AGFL (as printed by xfs_db) and the root / rt inodes must be
/// identical, except time stamps, generation numbers and checksums covering them.
fn xfs_same_as_mkfs(name: &str, size: u64, bs: u32, sector: u32) {
    let dir = common::workdir(&format!("mkfs-cmp-{name}"));
    let uuid = *uuid::Uuid::new_v4().as_bytes();
    let c = Case { name: "cmp", fs: Fs::Xfs, size, cluster: Some(bs), label: "cmp", sector };
    lumen_format(&dir.join("disk.img"), &c, uuid);
    let dump = |img: &str| {
        format!(
            "n=$(xfs_db -r -c 'sb 0' -c 'print agcount' {img} | cut -d' ' -f3); a=0; \
             while [ $a -lt $n ]; do xfs_db -r -c \"sb $a\" -c print -c \"agf $a\" -c print -c \"agi $a\" -c print -c \"agfl $a\" -c print {img} | sed \"s/^/ag$a /\"; a=$((a+1)); done; \
             r=$(xfs_db -r -c 'sb 0' -c 'print rootino' {img} | cut -d' ' -f3); \
             for i in 0 1 2; do xfs_db -r -c \"inode $((r + i))\" -c print {img} | sed \"s/^/ino$i /\"; done"
        )
    };
    let filter = "grep -vE 'crc = |time|\\.gen = |change_count|nsec'";
    let script = format!(
        "set -e; truncate -s {size} ref.img; mkfs.xfs -q -f -b size={bs} -s size={sector} -L cmp -m uuid={u} ref.img; \
         ({mine}) > mine.txt 2>&1; ({theirs}) > ref.txt 2>&1; \
         {filter} mine.txt > a.txt; {filter} ref.txt > b.txt; \
         echo \"lines: $(wc -l < a.txt), AGs: $(grep -c 'magicnum = 0x58414746' a.txt), fields: $(grep -cE '^ag[0-9]+ (dblocks|logstart|rootino|fdblocks|bnoroot|freeblks|longest|flcount|count|freecount|newino|bno\\[)' a.txt)\"; \
         grep -qE '^ino0 u3.sfdir3.hdr.parent' a.txt; diff a.txt b.txt; \
         logrec() {{ d=$(xfs_logprint -t $1 2>&1 | sed -n 's/.*daddr: \\([0-9]*\\).*/\\1/p'); dd if=$1 bs=512 skip=$d count=512 2>/dev/null; }}; \
         logrec disk.img > a.log; logrec ref.img > b.log; test -s a.log; cmp a.log b.log; \
         xfs_logprint -t disk.img 2>&1 | grep 'state: <CLEAN>'; echo SAME",
        u = uuid::Uuid::from_bytes(uuid).hyphenated(),
        mine = dump("disk.img"),
        theirs = dump("ref.img"),
    );
    let (code, out) = common::docker(&dir, &script);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(code == 0 && out.contains("SAME"), "[{name}] differs from mkfs.xfs:\n{out}");
    eprintln!(
        "SUMMARY xfs-vs-mkfs {name:<8} size={size} block={bs} sector={sector}: every AG's sb/agf/agi/agfl + root/rt inodes + first log record identical to mkfs.xfs 6.13 ({})",
        out.lines().next().unwrap_or("").trim()
    );
}

#[test]
#[ignore]
fn xfs_headers_match_mkfs_xfs() {
    xfs_same_as_mkfs("1g-4k", GIB, 4096, 512);
    xfs_same_as_mkfs("min-4k", 300 * MIB, 4096, 512);
    xfs_same_as_mkfs("odd-1k", GIB + 12345, 1024, 512);
    xfs_same_as_mkfs("2k", 700 * MIB + 2048 * 5, 2048, 512);
    xfs_same_as_mkfs("64k", 2 * GIB + 65536 * 3 + 100, 65536, 512);
    xfs_same_as_mkfs("32k", GIB, 32768, 512);
    xfs_same_as_mkfs("4kn", 3 * GIB / 2, 4096, 4096);
    xfs_same_as_mkfs("16k-4kn", 5 * GIB + 16384 * 7, 16384, 4096);
    xfs_same_as_mkfs("64k-4kn", 5 * GIB, 65536, 4096);
    xfs_same_as_mkfs("50g", 50 * GIB, 4096, 512);
}
