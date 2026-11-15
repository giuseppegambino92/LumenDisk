//! Persistent Shadow sessions, end to end: the real `lumendisk` CLI serving a real `mount_nfs`
//! mount is killed with SIGKILL, then mounted again.
//!
//!   cargo test -p lumendisk-core --release --test shadow_persist_e2e -- --ignored --test-threads=1 --nocapture
mod common;
use common::*;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumendisk_core::block::{BlockDev, FileDev};
use lumendisk_core::vfs::{open_fs, testutil, FileSystem, MountDefaults};

const BIN: &str = env!("CARGO_BIN_EXE_lumendisk");

fn cli(dir: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.current_dir(dir).env("LUMENDISK_SUPPORT_DIR", dir.join("support")).env("HOME", dir);
    c
}

fn is_mounted(mp: &Path) -> bool {
    // Never stat the mountpoint itself: after a SIGKILL its NFS server is gone.
    let out = Command::new("/sbin/mount").output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().any(|l| l.contains(&format!(" on {} (", mp.display())))
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

fn mount(dir: &Path, img: &str, mode: &str) -> Result<Mounted, String> {
    mount_env(dir, img, mode, &[])
}

fn mount_env(dir: &Path, img: &str, mode: &str, env: &[(&str, &str)]) -> Result<Mounted, String> {
    let mp = dir.join("mnt").join("Vol");
    let log = std::fs::File::create(dir.join(format!("mount-{}.log", Instant::now().elapsed().as_nanos()))).unwrap();
    let mut child = cli(dir)
        .envs(env.iter().copied())
        .args(["mount", img, "--mode", mode, "--root"])
        .arg(dir.join("mnt"))
        .args(["--name", "Vol", "--control", "c.sock"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(log.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            let _ = st;
            let logs: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("mount-")).map(|e| std::fs::read_to_string(e.path()).unwrap_or_default()).collect();
            return Err(logs.join("\n"));
        }
        if is_mounted(&mp) && dir.join("c.sock").exists() {
            return Ok(Mounted { child: Some(child), mp });
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "mount timed out");
        std::thread::sleep(Duration::from_millis(100));
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

fn sessions(dir: &Path) -> Vec<serde_json::Value> {
    let out = cli(dir).arg("shadow-sessions").output().unwrap();
    assert!(out.status.success());
    serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout).unwrap()
}

fn sha(path: &Path) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(std::fs::read(path).unwrap());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn mkfs(dir: &Path, name: &str, mb: u32) -> String {
    let out = cli(dir).args(["mkfs", "ext4", name, &mb.to_string(), "PERSIST"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    name.to_string()
}

/// Read a file of the image without LumenDisk's NFS layer.
fn image_fs(img: &Path) -> Box<dyn FileSystem> {
    let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(img, true).unwrap());
    open_fs(dev, true, MountDefaults::default()).unwrap()
}

fn read_path(fs: &mut dyn FileSystem, path: &str) -> Option<Vec<u8>> {
    let mut cur = fs.root_ino();
    for c in path.split('/').filter(|c| !c.is_empty()) {
        cur = fs.lookup(cur, c).ok()?;
    }
    let size = fs.getattr(cur).ok()?.size;
    let mut out = vec![];
    while (out.len() as u64) < size {
        let chunk = fs.read(cur, out.len() as u64, 1 << 20).unwrap();
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    Some(out)
}

fn fsck(dir: &Path, img: &str) {
    let (code, out) = docker(dir, &format!("e2fsck -fn /w/{img}"));
    assert_eq!(code, 0, "e2fsck: {out}");
    println!("    e2fsck -fn clean: {}", out.lines().find(|l| l.contains(" files ")).unwrap_or(""));
}

#[test]
#[ignore]
fn e2e_shadow_changes_survive_kill9_then_commit_is_fsck_clean() {
    let dir = workdir("shadow_kill9");
    std::fs::create_dir_all(dir.join("mnt")).unwrap();
    let img = mkfs(&dir, "persist.img", 128);
    // Base content written directly (not through Shadow).
    {
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&dir.join(&img), false).unwrap());
        let mut fs = open_fs(dev, false, MountDefaults::default()).unwrap();
        let r = fs.root_ino();
        let docs = fs.mkdir(r, "docs", 0o755, 1000, 1000).unwrap();
        let a = fs.create(docs, "a.txt", 0o644, 1000, 1000).unwrap();
        fs.write(a, 0, b"alpha").unwrap();
        let b = fs.create(docs, "b.txt", 0o644, 1000, 1000).unwrap();
        fs.write(b, 0, b"bravo").unwrap();
        fs.create(r, "old.log", 0o644, 1000, 1000).unwrap();
        let big = fs.create(r, "base.bin", 0o644, 1000, 1000).unwrap();
        fs.write(big, 0, &testutil::pattern(3 << 20, 1)).unwrap();
        fs.sync().unwrap();
    }
    let before = sha(&dir.join(&img));

    let mut m = mount(&dir, &img, "shadow").unwrap();
    let mp = m.mp.clone();
    std::fs::create_dir(mp.join("new")).unwrap();
    std::fs::write(mp.join("new/hello.txt"), b"ciao dal Mac").unwrap();
    let big = testutil::pattern(20 << 20, 7);
    std::fs::write(mp.join("new/big.bin"), &big).unwrap();
    std::fs::write(mp.join("docs/a.txt"), b"alpha, edited in Shadow").unwrap();
    std::fs::remove_file(mp.join("old.log")).unwrap();
    std::fs::rename(mp.join("docs"), mp.join("documents")).unwrap();
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(mp.join("base.bin")).unwrap();
        f.seek(SeekFrom::Start(1_000_000)).unwrap();
        f.write_all(b"PATCHED IN SHADOW").unwrap();
    }
    let st = ctl(&dir, "status");
    assert_eq!(st["shadow"]["persistent"], true);
    let key = st["shadow"]["session_key"].as_str().unwrap().to_string();
    // Let the NFS client push everything and the journal group-commit (≤ 100 ms).
    std::thread::sleep(Duration::from_millis(1500));
    m.kill9();

    assert_eq!(sha(&dir.join(&img)), before, "Shadow Mode must not touch the disk");
    let s = sessions(&dir);
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0]["key"], key.as_str());
    assert_eq!(s[0]["state"], "pending");
    assert_eq!(s[0]["connected"], true);
    assert_eq!(s[0]["fingerprint_ok"], true);
    assert!(s[0]["changes"].as_u64().unwrap() >= 4, "{s:?}");
    println!("    after kill -9: {} changes kept in session {key}", s[0]["changes"]);

    // read/write is refused while the session is pending
    let err = mount(&dir, &img, "readWrite").err().expect("read/write mount must be refused");
    assert!(err.contains("Shadow"), "{err}");

    // Shadow mount restores the changes
    let m = mount(&dir, &img, "shadow").unwrap();
    let mp = m.mp.clone();
    let st = ctl(&dir, "status");
    assert_eq!(st["shadow"]["restored"], true, "{st}");
    assert_eq!(std::fs::read(mp.join("new/hello.txt")).unwrap(), b"ciao dal Mac");
    assert!(std::fs::read(mp.join("new/big.bin")).unwrap() == big);
    assert_eq!(std::fs::read(mp.join("documents/a.txt")).unwrap(), b"alpha, edited in Shadow");
    assert!(!mp.join("old.log").exists() && !mp.join("docs").exists());
    let mut want_base = testutil::pattern(3 << 20, 1);
    want_base[1_000_000..1_000_017].copy_from_slice(b"PATCHED IN SHADOW");
    assert!(std::fs::read(mp.join("base.bin")).unwrap() == want_base);
    let applied = ctl(&dir, "commit");
    assert!(applied["applied"].as_u64().unwrap() >= 4, "{applied}");
    unmount(&dir, m);
    assert!(sessions(&dir).is_empty(), "the session is gone after the commit");

    fsck(&dir, &img);
    let mut fs = image_fs(&dir.join(&img));
    assert_eq!(read_path(fs.as_mut(), "/new/hello.txt").unwrap(), b"ciao dal Mac");
    assert!(read_path(fs.as_mut(), "/new/big.bin").unwrap() == big);
    assert_eq!(read_path(fs.as_mut(), "/documents/a.txt").unwrap(), b"alpha, edited in Shadow");
    assert_eq!(read_path(fs.as_mut(), "/documents/b.txt").unwrap(), b"bravo");
    assert!(read_path(fs.as_mut(), "/base.bin").unwrap() == want_base);
    assert!(read_path(fs.as_mut(), "/old.log").is_none());
    assert!(read_path(fs.as_mut(), "/docs/a.txt").is_none());
}

#[test]
#[ignore]
fn e2e_kill9_during_commit_then_resume_is_fsck_clean() {
    let dir = workdir("shadow_kill9_commit");
    std::fs::create_dir_all(dir.join("mnt")).unwrap();
    let img = mkfs(&dir, "commit.img", 512);
    // The server parks the commit after its 8th durable checkpoint (inside the content phase),
    // so the SIGKILL below always lands half-way through, however loaded the machine is.
    let m = mount_env(&dir, &img, "shadow", &[("LUMENDISK_TEST_STALL_COMMIT_AFTER", "8")]).unwrap();
    let mp = m.mp.clone();
    let mut want: Vec<(String, Vec<u8>)> = vec![];
    for d in 0..30 {
        std::fs::create_dir(mp.join(format!("dir{d:02}"))).unwrap();
        for f in 0..50 {
            let p = format!("dir{d:02}/file{f:03}.txt");
            let data = format!("{p} {}\n", "x".repeat((d * 50 + f) % 300)).into_bytes();
            std::fs::write(mp.join(&p), &data).unwrap();
            want.push((p, data));
        }
    }
    let big = testutil::pattern(150 << 20, 11);
    std::fs::write(mp.join("big.bin"), &big).unwrap();
    want.push(("big.bin".into(), big));
    let key = ctl(&dir, "status")["shadow"]["session_key"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(1500));

    // Commit in the background and SIGKILL the server half-way through it.
    let progress = dir.join("support/Shadow").join(&key).join("progress.ldj");
    let d2 = dir.clone();
    let committer = std::thread::spawn(move || cli(&d2).args(["ctl", "c.sock", "commit"]).output().unwrap());
    let t0 = Instant::now();
    let mut m = m;
    loop {
        // 22-byte frames: kill once a few checkpoints exist (inside the content phase).
        let len = std::fs::metadata(&progress).map(|x| x.len()).unwrap_or(0);
        if len >= 8 * 22 {
            m.kill9();
            break;
        }
        assert!(!committer.is_finished(), "the commit finished before it could be interrupted");
        assert!(t0.elapsed() < Duration::from_secs(120), "commit did not start");
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = committer.join();
    let s = sessions(&dir);
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0]["state"], "committing");
    let (applied, total) = (s[0]["commit_applied"].as_u64().unwrap(), s[0]["commit_total"].as_u64().unwrap());
    assert!(applied < total, "{s:?}");
    println!("    killed during the commit: {applied} of {total} steps checkpointed");
    assert!(mount(&dir, &img, "readWrite").is_err(), "read/write must be refused while a commit is interrupted");

    // Mounting in Shadow mode finishes the commit.
    let m = mount(&dir, &img, "shadow").unwrap();
    let st = ctl(&dir, "status");
    assert_eq!(st["shadow"]["state"], "pending", "{st}");
    assert_eq!(st["shadow"]["added"], 0, "{st}");
    let mp = m.mp.clone();
    assert_eq!(std::fs::read(mp.join("dir07/file013.txt")).unwrap(), want.iter().find(|w| w.0 == "dir07/file013.txt").unwrap().1);
    unmount(&dir, m);
    assert!(sessions(&dir).is_empty());

    fsck(&dir, &img);
    let mut fs = image_fs(&dir.join(&img));
    for (p, data) in &want {
        let got = read_path(fs.as_mut(), p).unwrap_or_else(|| panic!("{p} missing"));
        assert!(got == *data, "{p}: content differs");
    }
}
