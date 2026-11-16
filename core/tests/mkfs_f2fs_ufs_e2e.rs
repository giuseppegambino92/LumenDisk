//! Native `mkfs` for F2FS and UFS2 checked by the real tools.
//!
//! F2FS: `fsck.f2fs`, `blkid` and the Linux kernel (loop mount) in the `lumendisk-toolbox`
//! image, plus a byte comparison with `mkfs.f2fs` 1.16. UFS2: FreeBSD's own `fsck_ffs`, `mount`,
//! `dumpfs` in a FreeBSD VM (scripts/freebsd_check.sh).
//!
//! Run with:
//! `cargo test --release -p lumendisk-core --test mkfs_f2fs_ufs_e2e -- --ignored --test-threads=1 --nocapture`
mod common;
use common::*;
use lumendisk_core::block::{BlockDev, FileDev};
use lumendisk_core::forge::MkfsOptions;
use lumendisk_core::mkfs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

const UUID: [u8; 16] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
const UUID_STR: &str = "01234567-89ab-cdef-0123-456789abcdef";

fn sized_file(path: &Path, bytes: u64) {
    let _ = std::fs::remove_file(path);
    let f = std::fs::File::create(path).unwrap();
    f.set_len(bytes).unwrap();
}

fn open_dev(path: &Path) -> Arc<dyn BlockDev> {
    Arc::new(FileDev::open(path, false).unwrap())
}

fn opts(label: &str, cluster: Option<u32>) -> MkfsOptions {
    MkfsOptions { label: label.into(), cluster_size: cluster, uuid: Some(UUID) }
}

// ─────────────────────────────────────────── F2FS ───────────────────────────────────────────

/// `fsck.f2fs` problems: any failed check, assertion, error or fix. Benign lines excluded:
/// "[Ok..]" results, "Force to fix corruption" (that is what `-f` means) and "Corrupted valid
/// nat_bits": `fsck.f2fs -f` prints it for every fresh `mkfs.f2fs` volume too (the root's NAT
/// entry sits in the checkpoint journal) and rewrites the bits.
fn f2fs_fsck_problems(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| {
            let u = l.to_ascii_lowercase();
            let suspicious = ["fail", "assert", "error", "corrupt", "[fix]", "invalid", "wrong", "mismatch", "bad "].iter().any(|w| u.contains(w));
            let benign = l.contains("[Ok..]") || l.contains("Info: Force to fix corruption") || l.contains("Corrupted valid nat_bits in checkpoint");
            suspicious && !benign
        })
        .map(str::to_string)
        .collect()
}

/// Format `bytes` with LumenDisk, then in the toolbox: fsck.f2fs, fsck.f2fs -f, blkid, kernel
/// mount + hundreds of files + a big file, umount, fsck again, remount read-only and verify.
fn f2fs_roundtrip(name: &str, bytes: u64, big_mb: u64) {
    let t0 = Instant::now();
    let dir = workdir(&format!("mkfs-f2fs-{name}"));
    let img = dir.join("f2fs.img");
    sized_file(&img, bytes);
    let label = format!("LD-{name}");
    mkfs::f2fs::format(open_dev(&img), &opts(&label, None)).unwrap();
    let t_mkfs = t0.elapsed();
    let script = format!(
        r#"set -u
img=/w/f2fs.img
step() {{ echo "=== $*"; }}
check() {{ "$@" > /w/out.txt 2>&1; rc=$?; cat /w/out.txt; [ $rc -eq 0 ] || {{ echo "FAILED($rc): $*"; exit 1; }}; }}
step fsck; check fsck.f2fs $img
step fsck-f; check fsck.f2fs -f $img
step blkid; blkid -p -o export $img | tee /w/blkid.txt
step mount
mkdir -p /mnt/t && mount -t f2fs -o loop $img /mnt/t || {{ echo "FAILED: mount"; dmesg | tail -20; exit 1; }}
for d in $(seq 1 10); do mkdir /mnt/t/dir$d; for f in $(seq 1 40); do echo "file $d/$f" > /mnt/t/dir$d/f$f.txt; done; done
mkdir -p /mnt/t/deep/a/b/c && echo deep > /mnt/t/deep/a/b/c/leaf
dd if=/dev/urandom of=/w/big.src bs=1M count={big_mb} status=none
cp /w/big.src /mnt/t/big.bin && sync
df -k /mnt/t | tail -1
umount /mnt/t || {{ echo "FAILED: umount"; exit 1; }}
step fsck-after; check fsck.f2fs -f $img
step verify
mount -t f2fs -o loop,ro $img /mnt/t || {{ echo "FAILED: remount"; exit 1; }}
n=$(find /mnt/t -type f | wc -l); echo "files=$n"
[ "$n" -eq 402 ] || {{ echo "FAILED: file count"; exit 1; }}
cmp /w/big.src /mnt/t/big.bin || {{ echo "FAILED: big file differs"; exit 1; }}
[ "$(cat /mnt/t/dir7/f33.txt)" = "file 7/33" ] || {{ echo "FAILED: content"; exit 1; }}
umount /mnt/t
rm -f /w/big.src
step done
"#
    );
    let (code, out) = docker_priv(&dir, &script);
    assert_eq!(code, 0, "{name}: toolbox checks failed:\n{out}");
    let problems = f2fs_fsck_problems(&out);
    assert!(problems.is_empty(), "{name}: fsck.f2fs reported problems: {problems:#?}\n{out}");
    let blkid = std::fs::read_to_string(dir.join("blkid.txt")).unwrap();
    assert!(blkid.contains("TYPE=f2fs"), "{blkid}");
    assert!(blkid.contains(&format!("LABEL={label}")), "{blkid}");
    assert!(blkid.contains(&format!("UUID={UUID_STR}")), "{blkid}");
    let df = out.lines().find(|l| l.contains("/mnt/t")).unwrap_or("").to_string();
    println!(
        "F2FS {name:>8}: {:>14} bytes  mkfs {:>6.2?}  fsck/fsck -f/blkid/mount/402 files + {big_mb} MiB/umount/fsck/verify OK  [{}]  total {:.1?}",
        bytes,
        t_mkfs,
        df.split_whitespace().collect::<Vec<_>>().join(" "),
        t0.elapsed()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn f2fs_minimum_size() {
    f2fs_roundtrip("min", mkfs::f2fs::MIN_BYTES, 4);
}

#[test]
#[ignore]
fn f2fs_1gib() {
    f2fs_roundtrip("1gib", 1 << 30, 100);
}

#[test]
#[ignore]
fn f2fs_odd_size() {
    // Not a multiple of the block, the segment or even the sector size.
    f2fs_roundtrip("odd", 300 * 1024 * 1024 + 12345, 100);
}

#[test]
#[ignore]
fn f2fs_sparse_50gib() {
    f2fs_roundtrip("50gib", 50 << 30, 100);
}

#[test]
#[ignore]
fn f2fs_refuses_too_small() {
    let dir = workdir("mkfs-f2fs-small");
    let img = dir.join("small.img");
    sized_file(&img, mkfs::f2fs::MIN_BYTES - 4096);
    let err = mkfs::f2fs::format(open_dev(&img), &opts("x", None)).unwrap_err();
    // mkfs.f2fs refuses the same size.
    let (code, out) = docker(&dir, "mkfs.f2fs -f small.img");
    assert_ne!(code, 0, "{out}");
    println!("F2FS too small: LumenDisk: {err}; mkfs.f2fs: {}", out.lines().find(|l| l.contains("Error")).unwrap_or("").trim());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Same size, label, UUID and time as `mkfs.f2fs -T`: every block must be identical except the
/// "kernel version" strings of the superblocks, the checkpoint flags (`mkfs.f2fs` discards the
/// file and sets CP_TRIMMED) with the checksums that follow from them.
#[test]
#[ignore]
fn f2fs_bytes_match_mkfs_f2fs() {
    let dir = workdir("mkfs-f2fs-compare");
    for bytes in [mkfs::f2fs::MIN_BYTES, 1_000_000_000u64, 1 << 30, (8u64 << 30) + 4096 * 7] {
        sized_file(&dir.join("ref.img"), bytes);
        let (code, out) = docker(&dir, &format!("mkfs.f2fs -q -f -l Cmp -T 1700000000 -U {UUID_STR} ref.img"));
        assert_eq!(code, 0, "{out}");
        let reference = std::fs::read(dir.join("ref.img")).unwrap();
        let blk = |img: &[u8], b: usize| img[b * 4096..(b + 1) * 4096].to_vec();
        let seg0 = 512usize;
        let cp_ver = u64::from_le_bytes(reference[seg0 * 4096..seg0 * 4096 + 8].try_into().unwrap());
        let ours_path = dir.join("ours.img");
        sized_file(&ours_path, bytes);
        mkfs::f2fs::format_reproducible(open_dev(&ours_path), &opts("Cmp", None), 1_700_000_000, cp_ver).unwrap();
        let ours = std::fs::read(&ours_path).unwrap();
        assert_eq!(ours.len(), reference.len());
        let nblocks = ours.len() / 4096;
        let mut differing = Vec::new();
        for b in 0..nblocks {
            if ours[b * 4096..(b + 1) * 4096] != reference[b * 4096..(b + 1) * 4096] {
                differing.push(b);
            }
        }
        // Allowed differences, byte ranges per block.
        let mut cp_pages = vec![seg0, seg0 + 5, seg0 + 512, seg0 + 517];
        let mut allowed: Vec<(usize, Vec<std::ops::Range<usize>>)> = vec![(0, vec![1024 + 1668..1024 + 2180]), (1, vec![1024 + 1668..1024 + 2180])];
        for &c in &cp_pages {
            allowed.push((c, vec![132..136, 4092..4096]));
        }
        // NAT bits block(s) at the end of segment 0: their tag embeds the checkpoint CRC.
        allowed.push((seg0 + 511, vec![4..8]));
        cp_pages.clear();
        for &b in &differing {
            let ranges = allowed.iter().find(|(blk_no, _)| *blk_no == b).map(|(_, r)| r.clone()).unwrap_or_else(|| panic!("{bytes}: block {b} differs from mkfs.f2fs"));
            let (o, r) = (blk(&ours, b), blk(&reference, b));
            for i in 0..4096 {
                if o[i] != r[i] {
                    assert!(ranges.iter().any(|rg| rg.contains(&i)), "{bytes}: block {b} byte {i} differs: ours {:#x} mkfs.f2fs {:#x}", o[i], r[i]);
                }
            }
        }
        // The flags differ only by CP_TRIMMED_FLAG.
        let flags = |img: &[u8]| u32::from_le_bytes(img[seg0 * 4096 + 132..seg0 * 4096 + 136].try_into().unwrap());
        assert_eq!(flags(&reference) & !0x100, flags(&ours));
        println!("F2FS vs mkfs.f2fs {bytes:>12} bytes: {nblocks} blocks, differing {differing:?} (version strings / CP_TRIMMED / CRCs only)");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ─────────────────────────────────────────── UFS2 ───────────────────────────────────────────

/// Run `script` as root in a FreeBSD VM with `disks` attached ($DISK1…): see
/// scripts/freebsd_check.sh. Returns (exit code, output).
fn freebsd(dir: &Path, script: &str, disks: &[PathBuf]) -> (i32, String) {
    let harness = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/freebsd_check.sh");
    let guest = dir.join("guest.sh");
    std::fs::write(&guest, script).unwrap();
    let out = Command::new("sh").arg(&harness).arg(&guest).args(disks).output().expect("sh");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

struct UfsCase {
    name: &'static str,
    bytes: u64,
    bsize: Option<u32>,
}

/// Normalise `dumpfs` output for comparison: times, `fs_id`, check-hashes and the random
/// parts differ between two runs of `newfs` too.
const DUMPFS_NORM: &str = r#"norm() {
  dumpfs "$1" | sed -E \
    -e 's/(time|mtime)[[:space:]]+[A-Z][a-z][a-z] .*$/\1 */' \
    -e 's/id[[:space:]]+\[ [0-9a-f]+ [0-9a-f]+ \]/id [ * ]/' \
    -e 's/ckhash[[:space:]]+[0-9a-f]+/ckhash */g'
}
"#;

/// Same size and options with FreeBSD's `newfs -b B -f B/8 -L label`: `dumpfs` of both must
/// be identical except times, `fs_id` and check-hashes.
#[test]
#[ignore]
fn ufs2_matches_freebsd_newfs() {
    let dir = workdir("mkfs-ufs-newfs");
    let cases = [
        UfsCase { name: "64m", bytes: 64 << 20, bsize: None },
        UfsCase { name: "1g-odd", bytes: (1 << 30) + 12345 * 512, bsize: Some(16384) },
        UfsCase { name: "300m-4k", bytes: 300 << 20, bsize: Some(4096) },
        UfsCase { name: "777m-8k", bytes: (777 << 20) + 4096 * 3, bsize: Some(8192) },
        UfsCase { name: "2g-64k", bytes: 2 << 30, bsize: Some(65536) },
        UfsCase { name: "min", bytes: mkfs::ufs::min_bytes(32768), bsize: None },
    ];
    let mut disks = Vec::new();
    let mut script = format!("set -u\n{DUMPFS_NORM}fail=0\n");
    for (i, c) in cases.iter().enumerate() {
        let ours = dir.join(format!("{}-ours.img", c.name));
        let reference = dir.join(format!("{}-newfs.img", c.name));
        sized_file(&ours, c.bytes);
        sized_file(&reference, c.bytes);
        mkfs::ufs::format(open_dev(&ours), &opts("cmpvol", c.bsize)).unwrap();
        disks.push(ours);
        disks.push(reference);
        let b = c.bsize.unwrap_or(32768);
        let (a, r, name, f) = (2 * i + 1, 2 * i + 2, c.name, b / 8);
        script.push_str(&format!(
            r#"echo "=== {name}"
if newfs -b {b} -f {f} -L cmpvol $DISK{r} > /tmp/newfs.out 2>&1; then head -3 /tmp/newfs.out; else cat /tmp/newfs.out; fail=1; fi
norm $DISK{a} > /tmp/ours.txt; norm $DISK{r} > /tmp/ref.txt
if [ "{name}" = min ]; then
  # Two 1-block clusters were the only free blocks; newfs takes both for / and .snap without
  # updating cg_clustersum (fsck_ffs recomputes it), LumenDisk keeps it exact.
  grep -v '^clusters 1-8' /tmp/ours.txt > /tmp/o; mv /tmp/o /tmp/ours.txt
  grep -v '^clusters 1-8' /tmp/ref.txt > /tmp/r; mv /tmp/r /tmp/ref.txt
fi
if diff -u /tmp/ref.txt /tmp/ours.txt > /tmp/diff.txt; then echo "{name}: dumpfs identical to newfs ($(wc -l < /tmp/ours.txt) lines)"; else echo "{name}: dumpfs DIFFERS from newfs"; head -80 /tmp/diff.txt; fail=1; fi
fsck_ffs -n $DISK{a} 2>&1 | tail -1
"#
        ));
    }
    script.push_str("exit $fail\n");
    let t0 = Instant::now();
    let (code, out) = freebsd(&dir, &script, &disks);
    println!("{out}");
    assert_eq!(code, 0, "dumpfs differs from newfs:\n{out}");
    let problems = fsck_ffs_problems(&out);
    assert!(problems.is_empty(), "fsck_ffs -n: {problems:#?}");
    println!("UFS2 vs FreeBSD newfs: {} cases identical (VM run {:.0?})", cases.len(), t0.elapsed());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `fsck_ffs` output problems: anything but the phase banners and the summary.
fn fsck_ffs_problems(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| {
            let u = l.to_ascii_uppercase();
            ["MODIFIED", "FIX?", "SALVAGE", "CLEAR?", "BAD ", "UNREF", "INCORRECT", "MISSING", "ADJUST", "RECONNECT", "UNEXPECTED", "CANNOT", "ERROR", "CHECK-HASH", "REBUILD"]
                .iter()
                .any(|w| u.contains(w))
        })
        .map(str::to_string)
        .collect()
}

/// The guest side of a UFS2 round trip on $1 (label $2, big file $3 MiB): FreeBSD's checker,
/// a read-write mount with soft updates, hundreds of files and a big file, unmount, the checker
/// again and a read-only remount comparing everything.
const UFS_ROUNDTRIP: &str = r#"rt() {
  dev=$1; label=$2; big=$3; name=$4
  echo "=== $name ($dev)"
  dumpfs -m $dev
  dumpfs $dev | grep -E '^(flags|volname)' | tr '\t' ' '
  [ -e /dev/ufs/$label ] && echo "glabel: /dev/ufs/$label" || { echo "FAILED: no /dev/ufs/$label"; return 1; }
  ls /dev/ufsid/ | head -20 | tr '\n' ' '; echo
  echo "--- fsck_ffs -n"; fsck_ffs -n $dev || { echo "FAILED: fsck_ffs -n"; return 1; }
  echo "--- fsck_ffs -f -y"; fsck_ffs -f -y $dev > /tmp/f.txt 2>&1; rc=$?; cat /tmp/f.txt
  [ $rc -eq 0 ] || { echo "FAILED: fsck_ffs -f -y ($rc)"; return 1; }
  grep -q MODIFIED /tmp/f.txt && { echo "FAILED: fsck modified the fresh filesystem"; return 1; }
  mkdir -p /mnt/u && mount $dev /mnt/u || { echo "FAILED: mount"; return 1; }
  mount | grep /mnt/u; mount | grep /mnt/u | grep -q soft-updates || { echo "FAILED: not mounted with soft updates"; return 1; }
  d=1; while [ $d -le 10 ]; do mkdir /mnt/u/dir$d; f=1; while [ $f -le 40 ]; do echo "file $d/$f" > /mnt/u/dir$d/f$f.txt; f=$((f+1)); done; d=$((d+1)); done
  mkdir -p /mnt/u/deep/a/b/c && echo deep > /mnt/u/deep/a/b/c/leaf
  dd if=/dev/random of=/mnt/u/big.bin bs=1m count=$big 2>/dev/null
  sum=$(sha256 -q /mnt/u/big.bin)
  df -k /mnt/u | tail -1
  umount /mnt/u || { echo "FAILED: umount"; return 1; }
  echo "--- fsck_ffs -f -y after writes"; fsck_ffs -f -y $dev > /tmp/f.txt 2>&1; rc=$?; cat /tmp/f.txt
  [ $rc -eq 0 ] || { echo "FAILED: fsck_ffs after writes ($rc)"; return 1; }
  grep -q MODIFIED /tmp/f.txt && { echo "FAILED: fsck modified the filesystem after writes"; return 1; }
  echo "--- fsck_ffs -n after writes"; fsck_ffs -n $dev | tail -1
  mount -o ro $dev /mnt/u || { echo "FAILED: remount"; return 1; }
  n=$(find /mnt/u -type f | wc -l | tr -d ' ')
  [ "$n" -eq 402 ] || { echo "FAILED: $n files"; umount /mnt/u; return 1; }
  [ "$(sha256 -q /mnt/u/big.bin)" = "$sum" ] || { echo "FAILED: big file differs"; umount /mnt/u; return 1; }
  [ "$(cat /mnt/u/dir7/f33.txt)" = "file 7/33" ] || { echo "FAILED: content"; umount /mnt/u; return 1; }
  umount /mnt/u
  echo "$name: OK files=$n big=${big}MiB sha256=$sum"
}
fail=0
"#;

/// Format with LumenDisk, then in FreeBSD: fsck_ffs -n, fsck_ffs -f -y, mount (soft updates),
/// 402 files + a big file, umount, fsck again, read-only verification. Also `blkid` on Linux.
#[test]
#[ignore]
fn ufs2_freebsd_roundtrip() {
    let dir = workdir("mkfs-ufs-roundtrip");
    // (case, label, big file MiB)
    let cases = [
        (UfsCase { name: "512m", bytes: 512 << 20, bsize: None }, "ld_512m", 100),
        (UfsCase { name: "small-8m", bytes: 8 << 20, bsize: None }, "ld_small", 2),
        (UfsCase { name: "odd-16k", bytes: (700 << 20) + 3 * 512, bsize: Some(16384) }, "ld_odd", 100),
        (UfsCase { name: "4k", bytes: 400 << 20, bsize: Some(4096) }, "ld_4k", 100),
        (UfsCase { name: "64k", bytes: 2 << 30, bsize: Some(65536) }, "ld_64k", 100),
        (UfsCase { name: "sparse-20g", bytes: 20 << 30, bsize: None }, "ld_20g", 100),
    ];
    let mut disks = Vec::new();
    let mut script = format!("set -u\n{UFS_ROUNDTRIP}");
    let mut mkfs_times = Vec::new();
    for (i, (c, label, big)) in cases.iter().enumerate() {
        let img = dir.join(format!("{}.img", c.name));
        sized_file(&img, c.bytes);
        let t = Instant::now();
        mkfs::ufs::format(open_dev(&img), &opts(label, c.bsize)).unwrap();
        mkfs_times.push(t.elapsed());
        disks.push(img);
        script.push_str(&format!("rt $DISK{} {label} {big} {} || fail=1\n", i + 1, c.name));
    }
    script.push_str(
        r#"echo "--- kernel messages"; dmesg | grep -iE "ufs|ffs|hash" | tail -20
if dmesg | grep -iE "check-hash|ffs.*(error|fail|corrupt)|not properly dismounted"; then echo "FAILED: kernel complained"; fail=1; fi
exit $fail
"#,
    );

    // Linux's blkid recognises the volumes too (TYPE, LABEL, UUID from fs_id).
    let (code, out) = docker(&dir, "for f in *.img; do echo \"$f: $(blkid -p -o export $f | tr '\\n' ' ')\"; done");
    assert_eq!(code, 0, "{out}");
    for (c, label, _) in &cases {
        let line = out.lines().find(|l| l.starts_with(&format!("{}.img:", c.name))).unwrap_or_else(|| panic!("{out}"));
        assert!(line.contains("TYPE=ufs") && line.contains(&format!("LABEL={label}")), "blkid: {line}");
        assert!(line.to_ascii_lowercase().contains("uuid=0123456789abcdef"), "blkid: {line}");
    }
    println!("blkid:\n{out}");

    let t0 = Instant::now();
    let (code, out) = freebsd(&dir, &script, &disks);
    println!("{out}");
    assert_eq!(code, 0, "FreeBSD checks failed:\n{out}");
    let problems = fsck_ffs_problems(&out);
    assert!(problems.is_empty(), "fsck_ffs: {problems:#?}\n{out}");
    for ((c, _, big), t) in cases.iter().zip(&mkfs_times) {
        assert!(out.contains(&format!("{}: OK files=402", c.name)), "{}", c.name);
        println!("UFS2 {:>10}: {:>12} bytes, block {:>5}  mkfs {:>8.2?}  fsck -n/-f -y, mount rw (SU), 402 files + {big} MiB, umount, fsck, ro verify OK", c.name, c.bytes, c.bsize.unwrap_or(32768), t);
    }
    println!("FreeBSD VM run: {:.0?}", t0.elapsed());
    let _ = std::fs::remove_dir_all(&dir);
}
