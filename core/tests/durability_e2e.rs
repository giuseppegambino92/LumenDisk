//! Zero loss window for acknowledged operations, end to end: files and directories are changed
//! through the real macOS NFS client, and the `lumendisk` server is SIGKILLed the instant the
//! last call returns (no sleep). After a restart every acknowledged change must be there, with
//! nothing extra: metadata operations (mkdir / create / rename / unlink / rmdir) are durable when
//! they return, file data when close() or fsync() returns (the client COMMITs, or writes
//! FILE_SYNC). Shadow mode (journal + stage) and direct read/write mode (WAL + driver), several
//! kill / restart cycles each, then the result is committed (Shadow) and checked with e2fsck.
//!
//!   cargo test -p lumendisk-core --release --test durability_e2e -- --ignored --test-threads=1 --nocapture
//!
//! `DURABILITY_E2E_BIN=<path>` runs the same checks against another build of the CLI (e.g. one
//! from before this guarantee existed, to see the test catch the loss window).
mod common;
use common::*;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumendisk_core::block::{BlockDev, FileDev};
use lumendisk_core::vfs::{open_fs, testutil, FileKind, FileSystem, MountDefaults};

fn bin() -> String {
    std::env::var("DURABILITY_E2E_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_lumendisk").to_string())
}

fn cli(dir: &Path) -> Command {
    let mut c = Command::new(bin());
    c.current_dir(dir).env("LUMENDISK_SUPPORT_DIR", dir.join("support")).env("HOME", dir);
    c
}

fn is_mounted(mp: &Path) -> bool {
    // Never stat the mountpoint itself: after a SIGKILL its NFS server is gone.
    let out = Command::new("/sbin/mount").output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().any(|l| l.contains(&format!(" on {} (", mp.display())))
}

fn force_unmount(mp: &Path) {
    for _ in 0..50 {
        if !is_mounted(mp) {
            return;
        }
        let _ = Command::new("/sbin/umount").arg("-f").arg(mp).output();
        if is_mounted(mp) {
            let _ = Command::new("/usr/sbin/diskutil").args(["unmount", "force"]).arg(mp).output();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("could not unmount {}", mp.display());
}

/// Kills the CLI and force-unmounts on drop (also when a test panics).
struct Mounted {
    child: Option<Child>,
    mp: PathBuf,
}

impl Mounted {
    fn kill9(&mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill().unwrap(); // SIGKILL
            c.wait().unwrap();
        }
        force_unmount(&self.mp);
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        force_unmount(&self.mp);
    }
}

fn mount(dir: &Path, img: &str, mode: &str) -> Mounted {
    let mp = dir.join("mnt").join("Vol");
    let n = std::fs::read_dir(dir).unwrap().count();
    let log = std::fs::File::create(dir.join(format!("server-{n:03}.log"))).unwrap();
    let mut child = cli(dir)
        .args(["mount", img, "--mode", mode, "--no-ghost", "--root"])
        .arg(dir.join("mnt"))
        .args(["--name", "Vol", "--control", "c.sock"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            panic!("the server exited ({st}) before mounting: see {}", dir.display());
        }
        if is_mounted(&mp) && dir.join("c.sock").exists() {
            return Mounted { child: Some(child), mp };
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "mount timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn ctl(dir: &Path, cmd: &str) -> serde_json::Value {
    let out = cli(dir).args(["ctl", "c.sock", cmd]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("ctl {cmd}: {}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(v["ok"], true, "ctl {cmd}: {v}");
    v["result"].clone()
}

fn unmount(dir: &Path, mut m: Mounted) {
    ctl(dir, "unmount");
    let mut c = m.child.take().unwrap();
    let t0 = Instant::now();
    while c.try_wait().unwrap().is_none() {
        assert!(t0.elapsed() < Duration::from_secs(60), "unmount timed out");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!is_mounted(&m.mp));
}

/// What the volume must contain: every acknowledged change so far.
#[derive(Default, Clone)]
struct Model {
    dirs: BTreeSet<String>,
    files: BTreeMap<String, Vec<u8>>,
}

impl Model {
    /// Compare with the mounted tree: same directories, same files with the same bytes, nothing
    /// else.
    fn check(&self, mp: &Path, when: &str) {
        let mut dirs = BTreeSet::new();
        let mut files = BTreeMap::new();
        fn walk(root: &Path, rel: &str, dirs: &mut BTreeSet<String>, files: &mut BTreeMap<String, Vec<u8>>) {
            let mut es: Vec<_> = std::fs::read_dir(root.join(rel)).unwrap().flatten().collect();
            es.sort_by_key(|e| e.file_name());
            for e in es {
                let name = e.file_name().to_string_lossy().into_owned();
                if name == "lost+found" && rel.is_empty() {
                    continue;
                }
                let p = if rel.is_empty() { name } else { format!("{rel}/{name}") };
                if e.file_type().unwrap().is_dir() {
                    dirs.insert(p.clone());
                    walk(root, &p, dirs, files);
                } else {
                    files.insert(p.clone(), std::fs::read(root.join(&p)).unwrap());
                }
            }
        }
        walk(mp, "", &mut dirs, &mut files);
        assert_eq!(dirs, self.dirs, "{when}: directories differ");
        let got: BTreeSet<_> = files.keys().collect();
        let want: BTreeSet<_> = self.files.keys().collect();
        assert_eq!(got, want, "{when}: file names differ");
        for (p, data) in &self.files {
            assert!(files[p] == *data, "{when}: {p}: content differs ({} bytes on the volume, {} acknowledged)", files[p].len(), data.len());
        }
    }

    /// Same check on the image itself, without LumenDisk's NFS layer (after a clean unmount).
    fn check_image(&self, img: &Path) {
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(img, true).unwrap());
        let mut fs = open_fs(dev, true, MountDefaults::default()).unwrap();
        let mut dirs = BTreeSet::new();
        let mut files = BTreeMap::new();
        fn walk(fs: &mut dyn FileSystem, ino: u64, rel: &str, dirs: &mut BTreeSet<String>, files: &mut BTreeMap<String, Vec<u8>>) {
            for e in fs.readdir(ino).unwrap() {
                if e.name == "lost+found" && rel.is_empty() {
                    continue;
                }
                let p = if rel.is_empty() { e.name.clone() } else { format!("{rel}/{}", e.name) };
                if e.kind == FileKind::Dir {
                    dirs.insert(p.clone());
                    walk(fs, e.ino, &p, dirs, files);
                } else {
                    let size = fs.getattr(e.ino).unwrap().size;
                    let mut data = vec![];
                    while (data.len() as u64) < size {
                        let c = fs.read(e.ino, data.len() as u64, 1 << 20).unwrap();
                        assert!(!c.is_empty());
                        data.extend_from_slice(&c);
                    }
                    files.insert(p, data);
                }
            }
        }
        let root = fs.root_ino();
        walk(fs.as_mut(), root, "", &mut dirs, &mut files);
        assert_eq!(dirs, self.dirs, "image: directories differ");
        assert_eq!(files.keys().collect::<Vec<_>>(), self.files.keys().collect::<Vec<_>>(), "image: file names differ");
        for (p, data) in &self.files {
            assert!(files[p] == *data, "image: {p}: content differs");
        }
    }
}

/// The operation an iteration ends with: the server dies the instant it returns.
#[derive(Clone, Copy, Debug)]
enum Last {
    Mkdir,
    SmallFile,
    RenameOver,
    Unlink,
    Rmdir,
    BigFile,
    Fsync,
}

const LASTS: [Last; 7] = [Last::Mkdir, Last::SmallFile, Last::RenameOver, Last::Unlink, Last::Rmdir, Last::BigFile, Last::Fsync];

fn write_file(mp: &Path, m: &mut Model, p: &str, data: Vec<u8>) {
    std::fs::write(mp.join(p), &data).unwrap(); // open + write + close
    m.files.insert(p.to_string(), data);
}

/// One iteration's worth of acknowledged changes, ending with `last`. Returns the file left
/// open after an fsync (closed only once the server is dead).
fn changes(mp: &Path, m: &mut Model, it: usize, last: Last) -> Option<std::fs::File> {
    let d = format!("it{it}");
    std::fs::create_dir(mp.join(&d)).unwrap();
    m.dirs.insert(d.clone());
    for k in 0..12 {
        let len = 200 + (it * 977 + k * 131) % 9000;
        write_file(mp, m, &format!("{d}/small{k:02}.txt"), testutil::pattern(len, (it * 100 + k) as u32));
    }
    // rename over an existing file (safe-save pattern) and a plain move into a new directory
    write_file(mp, m, &format!("{d}/.doc.tmp"), testutil::pattern(4321, it as u32 + 7));
    write_file(mp, m, &format!("{d}/doc.txt"), b"old".to_vec());
    std::fs::rename(mp.join(format!("{d}/.doc.tmp")), mp.join(format!("{d}/doc.txt"))).unwrap();
    let v = m.files.remove(&format!("{d}/.doc.tmp")).unwrap();
    m.files.insert(format!("{d}/doc.txt"), v);
    std::fs::create_dir(mp.join(format!("{d}/sub"))).unwrap();
    m.dirs.insert(format!("{d}/sub"));
    std::fs::rename(mp.join(format!("{d}/small00.txt")), mp.join(format!("{d}/sub/moved.txt"))).unwrap();
    let v = m.files.remove(&format!("{d}/small00.txt")).unwrap();
    m.files.insert(format!("{d}/sub/moved.txt"), v);
    // delete something from the previous iteration, and an emptied directory
    std::fs::remove_file(mp.join(format!("{d}/small01.txt"))).unwrap();
    m.files.remove(&format!("{d}/small01.txt"));
    if it > 0 {
        let prev = format!("it{}", it - 1);
        std::fs::remove_file(mp.join(format!("{prev}/small02.txt"))).unwrap();
        m.files.remove(&format!("{prev}/small02.txt"));
    }
    std::fs::create_dir(mp.join(format!("{d}/empty"))).unwrap();
    std::fs::remove_dir(mp.join(format!("{d}/empty"))).unwrap();
    // overwrite the middle of an existing file in place, then close
    {
        let p = format!("{d}/small03.txt");
        let mut f = std::fs::OpenOptions::new().write(true).open(mp.join(&p)).unwrap();
        f.seek(SeekFrom::Start(100)).unwrap();
        f.write_all(b"PATCHED IN PLACE").unwrap();
        drop(f);
        m.files.get_mut(&p).unwrap()[100..116].copy_from_slice(b"PATCHED IN PLACE");
    }
    // the operation the server dies after
    match last {
        Last::Mkdir => {
            std::fs::create_dir(mp.join(format!("{d}/last-dir"))).unwrap();
            m.dirs.insert(format!("{d}/last-dir"));
        }
        Last::SmallFile => write_file(mp, m, &format!("{d}/last.txt"), testutil::pattern(777, it as u32)),
        Last::RenameOver => {
            std::fs::rename(mp.join(format!("{d}/small04.txt")), mp.join(format!("{d}/small05.txt"))).unwrap();
            let v = m.files.remove(&format!("{d}/small04.txt")).unwrap();
            m.files.insert(format!("{d}/small05.txt"), v);
        }
        Last::Unlink => {
            std::fs::remove_file(mp.join(format!("{d}/small06.txt"))).unwrap();
            m.files.remove(&format!("{d}/small06.txt"));
        }
        Last::Rmdir => {
            std::fs::remove_file(mp.join(format!("{d}/sub/moved.txt"))).unwrap();
            m.files.remove(&format!("{d}/sub/moved.txt"));
            std::fs::remove_dir(mp.join(format!("{d}/sub"))).unwrap();
            m.dirs.remove(&format!("{d}/sub"));
        }
        Last::BigFile => write_file(mp, m, &format!("{d}/big.bin"), testutil::pattern((24 << 20) + 12345, it as u32)),
        Last::Fsync => {
            let p = format!("{d}/fsynced.bin");
            let data = testutil::pattern((3 << 20) + 999, it as u32 + 1);
            let mut f = std::fs::File::create(mp.join(&p)).unwrap();
            f.write_all(&data).unwrap();
            f.sync_all().unwrap(); // fsync: the client COMMITs; the file stays open
            m.files.insert(p, data);
            return Some(f);
        }
    }
    None
}

fn run(name: &str, mode: &str, fs: &str) {
    let dir = workdir(name);
    std::fs::create_dir_all(dir.join("mnt")).unwrap();
    let out = cli(&dir).args(["mkfs", fs, "vol.img", "256", "DURABLE"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut model = Model::default();
    for (it, last) in LASTS.iter().enumerate() {
        let mut m = mount(&dir, "vol.img", mode);
        let mp = m.mp.clone();
        let st = ctl(&dir, "status");
        assert!(st["forced_read_only_reason"].is_null(), "{mode} {fs} after restart #{it}: {st}");
        model.check(&mp, &format!("{mode}: after restart #{it}"));
        let t0 = Instant::now();
        let open = changes(&mp, &mut model, it, *last);
        m.kill9(); // the instant the last call returned
        drop(open);
        println!("    {mode} #{it}: {} files, {} dirs acknowledged in {:.2} s, then SIGKILL right after {last:?}", model.files.len(), model.dirs.len(), t0.elapsed().as_secs_f64());
        if mode == "shadow" {
            let s: Vec<serde_json::Value> = serde_json::from_slice(&cli(&dir).arg("shadow-sessions").output().unwrap().stdout).unwrap();
            assert_eq!(s.len(), 1, "{s:?}");
            assert_eq!(s[0]["state"], "pending");
        }
    }
    let m = mount(&dir, "vol.img", mode);
    model.check(&m.mp, &format!("{mode}: after the last restart"));
    if mode == "shadow" {
        let st = ctl(&dir, "status");
        assert_eq!(st["shadow"]["restored"], true, "{st}");
        ctl(&dir, "commit");
        model.check(&m.mp, "shadow: after the commit");
    }
    unmount(&dir, m);
    let fsck = if fs == "ext4" { "e2fsck -fn /w/vol.img" } else { "fsck.fat -n /w/vol.img" };
    let (code, out) = docker(&dir, fsck);
    assert_eq!(code, 0, "{fsck}: {out}");
    println!("    {mode} {fs}: {fsck} clean: {}", out.lines().find(|l| l.contains(" files")).unwrap_or(""));
    model.check_image(&dir.join("vol.img"));
}

#[test]
#[ignore]
fn e2e_shadow_acknowledged_changes_survive_sigkill() {
    run("durability_shadow", "shadow", "ext4");
}

#[test]
#[ignore]
fn e2e_shadow_fat32_acknowledged_changes_survive_sigkill() {
    run("durability_shadow_fat", "shadow", "fat32");
}

#[test]
#[ignore]
fn e2e_direct_acknowledged_changes_survive_sigkill() {
    run("durability_direct", "readWrite", "ext4");
}

/// FAT marks itself "not unmounted cleanly" while written: after the SIGKILL the next mount
/// replays the WAL and must clear that mark (it is LumenDisk's own) instead of going read-only.
#[test]
#[ignore]
fn e2e_direct_fat32_acknowledged_changes_survive_sigkill() {
    run("durability_direct_fat", "readWrite", "fat32");
}
