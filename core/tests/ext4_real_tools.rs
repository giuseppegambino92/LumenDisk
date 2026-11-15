//! ext2/3/4 driver vs. the real e2fsprogs (run with `cargo test --test ext4_real_tools -- --ignored`).
mod common;
use common::*;
use lumendisk_core::block::{BlockDev, FileDev};
use lumendisk_core::vfs::ext4_fs::{self, Ext4Fs};
use lumendisk_core::vfs::{testutil, DirtyState, FileSystem, FsType};
use std::sync::Arc;

fn open(img: &std::path::Path) -> Ext4Fs {
    let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(img, false).unwrap());
    Ext4Fs::open(dev, false).unwrap()
}

fn fsck_clean(dir: &std::path::Path, img: &str) {
    let (code, out) = docker(dir, &format!("e2fsck -fn {img}"));
    assert_eq!(code, 0, "e2fsck reported problems:\n{out}");
}

/// Populate a source tree for `mke2fs -d`: a large htree directory, nested dirs, symlinks, big file.
fn populate_script() -> &'static str {
    "mkdir -p src/many src/etc/ssh src/home/pi && \
     for i in $(seq 1 1500); do echo \"entry $i\" > src/many/file_$i.txt; done && \
     echo 'dtparam=audio=on' > src/etc/config.txt && \
     head -c 5000000 /dev/urandom > src/home/pi/blob.bin && sha256sum src/home/pi/blob.bin | cut -d' ' -f1 > blob.sha && \
     ln -s ../etc/config.txt src/home/pi/cfg && \
     ln -s /very/long/target/path/that/is/definitely/longer/than/sixty/characters/for/slow/symlink src/home/pi/slow && \
     chown -R 1000:1000 src/home/pi"
}

fn real_image(name: &str, mkfs: &str, mb: u64) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = workdir(name);
    let img = dir.join("disk.img");
    make_sparse(&img, mb);
    let (code, out) = docker(&dir, &format!("{} && {mkfs} -q -F -L REALVOL -d src disk.img && e2fsck -fn disk.img", populate_script()));
    assert_eq!(code, 0, "{out}");
    (dir, img)
}

fn exercise_existing(fs: &mut Ext4Fs) {
    let root = fs.root_ino();
    let many = fs.lookup(root, "many").unwrap();
    assert_eq!(fs.readdir(many).unwrap().len(), 1500);
    let f = fs.lookup(many, "file_777.txt").unwrap();
    assert_eq!(fs.read(f, 0, 100).unwrap(), b"entry 777\n");
    // modify the (htree) directory: delete some, add some
    for i in 1..=300 {
        fs.unlink(many, &format!("file_{i}.txt")).unwrap();
    }
    for i in 0..200 {
        let n = fs.create(many, &format!("nuovo_{i}.dat"), 0o644, 1000, 1000).unwrap();
        fs.write(n, 0, format!("new {i}").as_bytes()).unwrap();
    }
    assert_eq!(fs.readdir(many).unwrap().len(), 1400);
    let f = fs.lookup(many, "file_1200.txt").unwrap();
    assert_eq!(fs.read(f, 0, 100).unwrap(), b"entry 1200\n");
    let home = fs.lookup(root, "home").unwrap();
    let pi = fs.lookup(home, "pi").unwrap();
    let a = fs.getattr(pi).unwrap();
    assert_eq!((a.uid, a.gid), (1000, 1000));
    let cfg = fs.lookup(pi, "cfg").unwrap();
    assert_eq!(fs.readlink(cfg).unwrap(), "../etc/config.txt");
    let slow = fs.lookup(pi, "slow").unwrap();
    assert!(fs.readlink(slow).unwrap().ends_with("slow/symlink"));
    let etc = fs.lookup(root, "etc").unwrap();
    let c = fs.lookup(etc, "config.txt").unwrap();
    fs.write(c, 17, b"\ngpu_mem=128\n").unwrap();
}

fn verify_after(dir: &std::path::Path) {
    fsck_clean(dir, "disk.img");
    let (code, out) = docker(dir, "debugfs -R 'cat /keep/sub/hello.txt' disk.img 2>/dev/null; echo; debugfs -R 'cat /etc/config.txt' disk.img 2>/dev/null; \
        debugfs -R 'dump /home/pi/blob.bin /tmp/b' disk.img 2>/dev/null; sha256sum /tmp/b | cut -d' ' -f1; cat blob.sha; \
        debugfs -R 'ls -l /many' disk.img 2>/dev/null | grep -c nuovo_");
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(out.contains("hello lumendisk"), "{out}");
    assert!(out.contains("gpu_mem=128"), "{out}");
    let shas: Vec<&&str> = lines.iter().filter(|l| l.len() == 64).collect();
    assert_eq!(shas.len(), 2, "{out}");
    assert_eq!(shas[0], shas[1], "blob.bin changed!");
    assert_eq!(lines.last().unwrap().trim(), "200", "{out}");
}

#[test]
#[ignore]
fn real_ext4_modern_defaults() {
    let (dir, img) = real_image("ext4_real", "mkfs.ext4", 256);
    let mut fs = open(&img);
    assert_eq!(fs.volume_info().fs_type, FsType::Ext4);
    assert!(fs.forced_ro_reason().is_none(), "{:?}", fs.forced_ro_reason());
    assert_eq!(fs.dirty_state(), DirtyState::Clean);
    exercise_existing(&mut fs);
    testutil::conformance(&mut fs, true);
    fs.sync().unwrap();
    drop(fs);
    let mut fs = open(&img);
    testutil::check_persisted(&mut fs);
    drop(fs);
    verify_after(&dir);
}

#[test]
#[ignore]
fn real_ext4_small_1k_blocks() {
    let (dir, img) = real_image("ext4_real_1k", "mkfs.ext4 -b 1024", 64);
    let mut fs = open(&img);
    exercise_existing(&mut fs);
    testutil::conformance(&mut fs, true);
    drop(fs);
    verify_after(&dir);
}

#[test]
#[ignore]
fn real_ext3_and_ext2() {
    for (name, mkfs, t) in [("ext3_real", "mkfs.ext3", FsType::Ext3), ("ext2_real", "mkfs.ext2", FsType::Ext2)] {
        let (dir, img) = real_image(name, mkfs, 128);
        let mut fs = open(&img);
        assert_eq!(fs.volume_info().fs_type, t);
        exercise_existing(&mut fs);
        testutil::conformance(&mut fs, true);
        drop(fs);
        verify_after(&dir);
    }
}

#[test]
#[ignore]
fn own_mkfs_passes_e2fsck() {
    for (t, mb) in [(FsType::Ext4, 64u64), (FsType::Ext4, 512), (FsType::Ext3, 64), (FsType::Ext2, 32)] {
        let dir = workdir(&format!("own_{:?}_{mb}", t));
        let img = dir.join("disk.img");
        make_sparse(&img, mb);
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
        ext4_fs::format(dev, t, "LUMEN").unwrap();
        fsck_clean(&dir, "disk.img");
        let (_, info) = docker(&dir, "dumpe2fs -h disk.img 2>/dev/null | grep -E 'features|volume name'");
        println!("{t:?} {mb}MiB: {info}");
        let mut fs = open(&img);
        testutil::conformance(&mut fs, true);
        drop(fs);
        fsck_clean(&dir, "disk.img");
        let (code, out) = docker(&dir, "debugfs -R 'cat /keep/sub/hello.txt' disk.img");
        assert_eq!(code, 0);
        assert!(out.contains("hello lumendisk"), "{out}");
    }
}

#[test]
#[ignore]
fn journal_doctor_replays_real_jbd2_transaction() {
    let dir = workdir("ext4_journal");
    let img = dir.join("disk.img");
    make_sparse(&img, 128);
    // Create a file, then use debugfs to log a new version of its data block in the journal
    // WITHOUT checkpointing (exactly what a crash after commit looks like).
    let script = "mkdir -p src && printf 'OLD-CONTENT-%.0s' $(seq 1 300) > src/victim.txt && \
        mkfs.ext4 -q -F -d src disk.img && \
        BLK=$(debugfs -R 'bmap /victim.txt 0' disk.img 2>/dev/null) && \
        printf 'NEW-CONTENT-%.0s' $(seq 1 341) | head -c 4096 > newblock && \
        printf 'jo\\njw -b %s newblock\\njc\\n' $BLK > cmds && debugfs -w -f cmds disk.img && \
        dumpe2fs -h disk.img 2>/dev/null | grep -i 'features'";
    let (code, out) = docker(&dir, script);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("needs_recovery"), "debugfs did not leave a dirty journal: {out}");
    let mut fs = open(&img);
    match fs.dirty_state() {
        DirtyState::Dirty(r) => assert!(r.contains("needs_recovery"), "{r}"),
        DirtyState::Clean => panic!("expected dirty"),
    }
    let report = fs.repair().unwrap();
    println!("{report}");
    assert!(report.contains("Journal replayed: 1 transactions"), "{report}");
    assert_eq!(fs.dirty_state(), DirtyState::Clean);
    let root = fs.root_ino();
    let v = fs.lookup(root, "victim.txt").unwrap();
    let data = fs.read(v, 0, 12).unwrap();
    assert_eq!(data, b"NEW-CONTENT-");
    drop(fs);
    fsck_clean(&dir, "disk.img");
}
