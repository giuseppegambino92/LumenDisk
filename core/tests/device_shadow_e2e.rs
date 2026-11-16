//! Persistent Shadow sessions on a real block device node, end to end: a GPT disk image is
//! attached with `hdiutil` (no macOS mount), the real `lumendisk` CLI serves the *device node*
//! (`/dev/diskNs1`, or `/dev/rdiskN` + partition 1) through a real `mount_nfs` mount, and is
//! killed with SIGKILL, also in the middle of a commit. A third test drives the block layer
//! (`block.rs`) directly on all four kinds of node.
//!
//!   cargo test -p lumendisk-core --release --test device_shadow_e2e -- --ignored --test-threads=1 --nocapture
//!
//! hdiutil-attached nodes belong to the user who attached them, so no authorisation
//! (authopen) is involved: LumenDisk opens them like any file.
//!
//! Every operation acknowledged before the zero-delay SIGKILL must survive it (see
//! `survived_prefix`): metadata replies wait until the journal has the record, and close()
//! COMMITs the file's data.
mod common;
use common::*;

use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumendisk_core::block::{self, BlockDev, FileDev, PartitionDev};
use lumendisk_core::vfs::{self, open_fs, testutil, FileSystem, FsType, MountDefaults};

const BIN: &str = env!("CARGO_BIN_EXE_lumendisk");
const SECTOR: u64 = 512;
/// First LBA of the partition (1 MiB, like every partitioning tool).
const PART_START: u64 = 2048;
const LINUX_FS: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";

// ───────────────────────── a GPT disk image ─────────────────────────

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

/// GUID string → on-disk (mixed-endian) bytes.
fn guid_bytes(s: &str) -> [u8; 16] {
    let h: Vec<u8> = (0..32).map(|i| u8::from_str_radix(&s.replace('-', "")[i * 2..i * 2 + 2], 16).unwrap()).take(16).collect();
    let mut g = [0u8; 16];
    g[..4].copy_from_slice(&[h[3], h[2], h[1], h[0]]);
    g[4..6].copy_from_slice(&[h[5], h[4]]);
    g[6..8].copy_from_slice(&[h[7], h[6]]);
    g[8..].copy_from_slice(&h[8..]);
    g
}

/// A sparse GPT disk image with one "Linux filesystem" partition of `sectors` sectors at
/// LBA 2048 whose unique partition GUID is `part_guid`. Returns the partition's byte range.
fn gpt_image(path: &Path, sectors: u64, part_guid: &str) -> (u64, u64) {
    let total = PART_START + sectors + 33;
    let last = total - 1;
    let f = std::fs::File::create(path).unwrap();
    f.set_len(total * SECTOR).unwrap();
    let mut entries = vec![0u8; 128 * 128];
    entries[0..16].copy_from_slice(&guid_bytes(LINUX_FS));
    entries[16..32].copy_from_slice(&guid_bytes(part_guid));
    entries[32..40].copy_from_slice(&PART_START.to_le_bytes());
    entries[40..48].copy_from_slice(&(PART_START + sectors - 1).to_le_bytes());
    for (i, c) in "LumenDisk".encode_utf16().enumerate() {
        entries[56 + i * 2..58 + i * 2].copy_from_slice(&c.to_le_bytes());
    }
    let header = |cur: u64, backup: u64, table: u64| {
        let mut h = vec![0u8; 92];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&cur.to_le_bytes());
        h[32..40].copy_from_slice(&backup.to_le_bytes());
        h[40..48].copy_from_slice(&34u64.to_le_bytes());
        h[48..56].copy_from_slice(&(last - 33).to_le_bytes());
        h[56..72].copy_from_slice(&guid_bytes("6C1D2B9E-3F4A-4E5B-8C7D-0A1B2C3D4E5F"));
        h[72..80].copy_from_slice(&table.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&crc32(&entries).to_le_bytes());
        let c = crc32(&h);
        h[16..20].copy_from_slice(&c.to_le_bytes());
        h.resize(SECTOR as usize, 0);
        h
    };
    let mut mbr = vec![0u8; 512];
    mbr[446..462].copy_from_slice(&[0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF, 1, 0, 0, 0, 0, 0, 0, 0]);
    mbr[458..462].copy_from_slice(&((total - 1).min(u32::MAX as u64) as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    f.write_all_at(&mbr, 0).unwrap();
    f.write_all_at(&header(1, last, 2), SECTOR).unwrap();
    f.write_all_at(&entries, 2 * SECTOR).unwrap();
    f.write_all_at(&entries, (last - 32) * SECTOR).unwrap();
    f.write_all_at(&header(last, 1, last - 32), last * SECTOR).unwrap();
    f.sync_all().unwrap();
    (PART_START * SECTOR, sectors * SECTOR)
}

/// The partition of the image file, without going through the attached device.
fn image_part(img: &Path, ro: bool, (start, len): (u64, u64)) -> Arc<dyn BlockDev> {
    let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(img, ro).unwrap());
    Arc::new(PartitionDev::new(dev, start, len).unwrap())
}

fn image_fs(img: &Path, part: (u64, u64)) -> Box<dyn FileSystem> {
    open_fs(image_part(img, true, part), true, MountDefaults::default()).unwrap()
}

/// Format the partition as ext4 and lay down some base content (not through Shadow).
fn make_disk(img: &Path, sectors: u64, guid: &str) -> (u64, u64) {
    let part = gpt_image(img, sectors, guid);
    vfs::format(image_part(img, false, part), FsType::Ext4, "DEVSHADOW").unwrap();
    let mut fs = open_fs(image_part(img, false, part), false, MountDefaults::default()).unwrap();
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
    part
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

/// `e2fsck -fn` on the partition (cut out of the image) in the toolbox container; `cat`: a file
/// whose content e2fsprogs' `debugfs` prints too. Returns the output.
fn fsck_partition(dir: &Path, img: &str, (start, len): (u64, u64), cat: Option<&str>) -> String {
    let debugfs = cat.map(|f| format!(" && debugfs -R 'cat \"/{f}\"' /w/part.img 2>/dev/null")).unwrap_or_default();
    let script = format!(
        "dd if=/w/{img} of=/w/part.img bs=512 skip={} count={} status=none && e2fsck -fn /w/part.img{debugfs}; rc=$?; rm -f /w/part.img; exit $rc",
        start / SECTOR,
        len / SECTOR
    );
    let (code, out) = docker(dir, &script);
    assert_eq!(code, 0, "e2fsck: {out}");
    println!("    e2fsck -fn clean: {}", out.lines().find(|l| l.contains(" files ")).unwrap_or(""));
    out
}

fn sha_file(path: &Path) -> String {
    sha_reader(&std::fs::File::open(path).unwrap(), std::fs::metadata(path).unwrap().len())
}

/// SHA-256 of the first `len` bytes (a whole number of sectors) of a file or raw device node.
fn sha_reader(f: &std::fs::File, len: u64) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0;
    while off < len {
        let n = (len - off).min(buf.len() as u64) as usize;
        f.read_exact_at(&mut buf[..n], off).unwrap();
        h.update(&buf[..n]);
        off += n as u64;
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// ───────────────────────── hdiutil ─────────────────────────

/// A disk image attached by this test. Detached on drop (also when a test panics), and only
/// if `hdiutil info` still shows *our* image on that node.
struct Attached {
    img: PathBuf,
    /// `/dev/diskN` (empty once detached).
    disk: String,
}

impl Attached {
    fn new(img: &Path) -> Attached {
        let out = Command::new("/usr/bin/hdiutil").args(["attach", "-nomount", "-imagekey", "diskimage-class=CRawDiskImage"]).arg(img).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "hdiutil attach: {text}{}", String::from_utf8_lossy(&out.stderr));
        let disk = text.split_whitespace().find(|t| t.starts_with("/dev/disk")).unwrap().to_string();
        let att = Attached { img: img.canonicalize().unwrap(), disk };
        assert!(att.ours(), "hdiutil info doesn't list {} for {}", att.disk, img.display());
        assert!(text.contains(&format!("{}s1", att.disk)), "no partition node: {text}");
        att
    }
    fn slice(&self, n: u32) -> String {
        format!("{}s{n}", self.disk)
    }
    fn raw(node: &str) -> String {
        node.replacen("/dev/disk", "/dev/rdisk", 1)
    }
    /// Is our image still attached at `self.disk`?
    fn ours(&self) -> bool {
        if self.disk.is_empty() {
            return false;
        }
        let out = Command::new("/usr/bin/hdiutil").arg("info").output().unwrap();
        let info = String::from_utf8_lossy(&out.stdout);
        info.split("================================================").any(|sec| {
            sec.lines().any(|l| l.starts_with("image-path") && l.split_once(':').map(|(_, p)| Path::new(p.trim()) == self.img).unwrap_or(false))
                && sec.lines().any(|l| l.split_whitespace().next() == Some(self.disk.as_str()))
        })
    }
    fn detach(&mut self) {
        for attempt in 0..20 {
            if !self.ours() {
                break;
            }
            let mut c = Command::new("/usr/bin/hdiutil");
            c.arg("detach").arg(&self.disk);
            if attempt >= 10 {
                c.arg("-force");
            }
            if c.output().map(|o| o.status.success()).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        assert!(!self.ours(), "could not detach {}", self.disk);
        self.disk.clear();
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        if self.ours() {
            let _ = Command::new("/usr/bin/hdiutil").args(["detach", &self.disk, "-force"]).output();
        }
    }
}

/// DiskArbitration's media UUID of a node (what the app passes as `media_uuid`).
fn media_uuid(node: &str) -> Option<String> {
    let out = Command::new("/usr/sbin/diskutil").args(["info", "-plist", node]).output().unwrap();
    let plist = String::from_utf8_lossy(&out.stdout);
    let at = plist.find("<key>DiskUUID</key>")?;
    let rest = &plist[at..];
    let s = rest.find("<string>")? + 8;
    let e = rest[s..].find("</string>")?;
    Some(rest[s..s + e].to_string())
}

// ───────────────────────── the CLI ─────────────────────────

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

/// `lumendisk mount <node> --mode <mode> <args…>` in the background, until it is mounted.
fn mount(dir: &Path, node: &str, mode: &str, args: &[&str], env: &[(&str, &str)]) -> Result<Mounted, String> {
    let mp = dir.join("mnt").join("Vol");
    let log_path = dir.join(format!("mount-{}.log", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let log = std::fs::File::create(&log_path).unwrap();
    let mut child = cli(dir)
        .envs(env.iter().copied())
        .args(["mount", node, "--mode", mode, "--root"])
        .arg(dir.join("mnt"))
        .args(["--name", "Vol", "--control", "c.sock"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return Err(std::fs::read_to_string(&log_path).unwrap_or_default());
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

/// What a restarted server shows of one acknowledged operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Seen {
    Done,
    /// A file write cut short (the file exists, holding a prefix of the data).
    Partial,
    Lost,
}

/// A file that was written with `want`: whole, cut short, or not there.
fn written(got: &Option<Vec<u8>>, want: &[u8], what: &str) -> Seen {
    match got {
        None => Seen::Lost,
        Some(g) if g == want => Seen::Done,
        Some(g) if want.starts_with(g) => Seen::Partial,
        Some(g) => panic!("{what}: {} bytes that are neither the data written nor a prefix of it", g.len()),
    }
}

/// How many of the acknowledged operations (in order) survived the SIGKILL: all of them.
/// A metadata reply comes only once the journal has the record (written, so it survives the
/// process), and close() COMMITs the data (synced); what survives must anyway be a consistent
/// prefix, which is checked first for a clearer message.
fn survived_prefix(ops: &[(String, Seen)]) -> usize {
    let done = ops.iter().take_while(|o| o.1 == Seen::Done).count();
    let mut rest = &ops[done..];
    if rest.first().map(|o| o.1) == Some(Seen::Partial) {
        rest = &rest[1..];
    }
    if let Some(o) = rest.iter().find(|o| o.1 != Seen::Lost) {
        panic!("not a consistent prefix: {done} operations survived, then {:?} {:?} although an earlier one was lost", o.0, o.1);
    }
    assert_eq!(done, ops.len(), "acknowledged operation {:?} lost after kill -9", ops.get(done).map(|o| &o.0));
    done
}

/// Contents of `files` and existence of `dirs`, through a mount or on the image.
type View = (Vec<Option<Vec<u8>>>, Vec<bool>);

fn mount_view(mp: &Path, files: &[String], dirs: &[&str]) -> View {
    (files.iter().map(|f| std::fs::read(mp.join(f)).ok()).collect(), dirs.iter().map(|d| mp.join(d).is_dir()).collect())
}

fn disk_view(fs: &mut dyn FileSystem, files: &[String], dirs: &[&str]) -> View {
    let is_dir = |fs: &mut dyn FileSystem, p: &str| {
        let mut cur = fs.root_ino();
        for c in p.split('/') {
            match fs.lookup(cur, c) {
                Ok(i) => cur = i,
                Err(_) => return false,
            }
        }
        fs.getattr(cur).map(|a| a.kind == vfs::FileKind::Dir).unwrap_or(false)
    };
    (files.iter().map(|f| read_path(fs, f)).collect(), dirs.iter().map(|d| is_dir(fs, d)).collect())
}

// ───────────────────────── tests ─────────────────────────

/// 192 MiB + 3 sectors: the partition node's size isn't a whole number of 4 KiB blocks.
const PART_SECTORS: u64 = 192 * 2048 + 3;
const PART_GUID: &str = "A3C1E5D2-7B4F-4C8A-9E21-5D6F7A8B9C01";

#[test]
#[ignore]
fn e2e_device_node_shadow_survives_kill9_then_commit_is_fsck_clean() {
    let dir = workdir("dev_shadow_kill9");
    std::fs::create_dir_all(dir.join("mnt")).unwrap();
    let img = dir.join("disk.img");
    let part = make_disk(&img, PART_SECTORS, PART_GUID);
    let (uuid, fs_bytes) = {
        let mut fs = image_fs(&img, part);
        (fs.volume_info().uuid, fs.statfs().unwrap().total_bytes)
    };
    let before = sha_file(&img);
    let img_len = std::fs::metadata(&img).unwrap().len();

    let mut att = Attached::new(&img);
    let node = att.slice(1);
    // DiskArbitration identifies the partition by its GPT unique partition GUID.
    let media = media_uuid(&node).unwrap_or_else(|| panic!("no media UUID for {node}"));
    assert_eq!(media.to_ascii_uppercase(), PART_GUID, "media UUID of {node}");

    let mut m = mount(&dir, &node, "shadow", &["--media-uuid", &media], &[]).unwrap();
    let mp = m.mp.clone();
    let st = ctl(&dir, "status");
    assert_eq!(st["device"], node.trim_start_matches("/dev/"), "{st}");
    assert_eq!(st["shadow"]["persistent"], true, "{st}");
    let key = st["shadow"]["session_key"].as_str().unwrap().to_string();

    // Acknowledged operations, in order; the SIGKILL follows the last one with no delay.
    let hello = b"ciao dal dispositivo".to_vec();
    let big = testutil::pattern(24 << 20, 7);
    let edited = b"alpha, edited in Shadow on a device".to_vec();
    let base = testutil::pattern(3 << 20, 1);
    let mut patched = base.clone();
    patched[1_000_000..1_000_017].copy_from_slice(b"PATCHED IN SHADOW");
    let burst: Vec<Vec<u8>> = (0..200).map(|i| format!("burst file {i:03} {}\n", "y".repeat(i % 97)).into_bytes()).collect();
    let t0 = Instant::now();
    std::fs::create_dir(mp.join("new")).unwrap();
    std::fs::write(mp.join("new/hello.txt"), &hello).unwrap();
    std::fs::write(mp.join("new/big.bin"), &big).unwrap();
    std::fs::write(mp.join("docs/a.txt"), &edited).unwrap();
    std::fs::remove_file(mp.join("old.log")).unwrap();
    std::fs::rename(mp.join("docs"), mp.join("documents")).unwrap();
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(mp.join("base.bin")).unwrap();
        f.seek(SeekFrom::Start(1_000_000)).unwrap();
        f.write_all(b"PATCHED IN SHADOW").unwrap();
    }
    std::fs::create_dir(mp.join("burst")).unwrap();
    let t_burst = t0.elapsed();
    for (i, data) in burst.iter().enumerate() {
        std::fs::write(mp.join(format!("burst/f{i:03}.txt")), data).unwrap();
    }
    let t_all = t0.elapsed();
    m.kill9();

    // The device is untouched: every byte of the attached disk, read through its raw node.
    let raw = std::fs::File::open(Attached::raw(&att.disk)).unwrap();
    assert_eq!(sha_reader(&raw, img_len), before, "Shadow Mode must not touch the device");
    drop(raw);
    let s = sessions(&dir);
    assert_eq!(s.len(), 1, "no saved session after kill -9: {s:?}");
    let s0 = &s[0];
    assert_eq!(s0["key"], key.as_str());
    assert_eq!(s0["state"], "pending");
    assert_eq!(s0["kind"], "local");
    assert_eq!(s0["fs_type"], "EXT4");
    assert_eq!(s0["source_path"], node.as_str(), "the session belongs to the device node");
    assert_eq!(s0["partition"], 0);
    assert_eq!(s0["uuid"], uuid.as_str(), "keyed by the filesystem UUID on the device");
    assert_eq!(s0["size"], fs_bytes);
    assert_eq!(s0["media_uuid"], media.as_str());
    assert_eq!(s0["connected"], true);
    assert_eq!(s0["fingerprint_ok"], true, "{s0}");
    println!("    after kill -9: session {key} for {node} (media {media}) pending");

    // Read/write is refused while the session is pending, and so is resuming a session the
    // disk doesn't hold.
    let err = mount(&dir, &node, "readWrite", &[], &[]).err().expect("read/write mount must be refused");
    assert!(err.contains("Shadow"), "{err}");
    let err = mount(&dir, &node, "shadow", &["--expect-session", "ext4-0123456789abcdef"], &[]).err().expect("a foreign session must be refused");
    assert!(err.contains("doesn't match") || err.contains("non corrisponde"), "{err}");

    // Shadow mount of the same node resumes the session, as the app does it.
    let m = mount(&dir, &node, "shadow", &["--media-uuid", &media, "--expect-session", &key], &[]).unwrap();
    let mp = m.mp.clone();
    let st = ctl(&dir, "status");
    assert_eq!(st["shadow"]["restored"], true, "{st}");
    assert_eq!(st["shadow"]["session_key"], key.as_str(), "{st}");
    let mut files: Vec<String> = ["new/hello.txt", "new/big.bin", "docs/a.txt", "docs/b.txt", "documents/a.txt", "documents/b.txt", "old.log", "base.bin"].map(String::from).to_vec();
    files.extend((0..burst.len()).map(|i| format!("burst/f{i:03}.txt")));
    let dirs = ["new", "docs", "documents", "burst"];
    let view = mount_view(&mp, &files, &dirs);
    let (f, d) = (&view.0, &view.1);
    let a_txt = if d[2] { &f[4] } else { &f[2] };
    let mut ops: Vec<(String, Seen)> = vec![
        ("mkdir new".into(), if d[0] { Seen::Done } else { Seen::Lost }),
        ("write new/hello.txt".into(), written(&f[0], &hello, "new/hello.txt")),
        ("write new/big.bin".into(), written(&f[1], &big, "new/big.bin")),
        ("rewrite docs/a.txt".into(), if a_txt.as_deref() == Some(&b"alpha"[..]) { Seen::Lost } else { written(a_txt, &edited, "a.txt") }),
        ("rm old.log".into(), if f[6].is_none() { Seen::Done } else { Seen::Lost }),
        ("mv docs documents".into(), match (d[1], d[2]) {
            (false, true) => Seen::Done,
            (true, false) => Seen::Lost,
            other => panic!("docs/documents: {other:?}"),
        }),
        ("patch base.bin".into(), match &f[7] {
            Some(b) if *b == patched => Seen::Done,
            Some(b) if *b == base => Seen::Lost,
            _ => panic!("base.bin is neither the original nor the patched file"),
        }),
        ("mkdir burst".into(), if d[3] { Seen::Done } else { Seen::Lost }),
    ];
    for (i, data) in burst.iter().enumerate() {
        ops.push((format!("write burst/f{i:03}.txt"), written(&f[8 + i], data, "burst file")));
    }
    assert_eq!((f[3].as_deref(), f[5].as_deref()), if d[2] { (None, Some(&b"bravo"[..])) } else { (Some(&b"bravo"[..]), None) });
    let n = survived_prefix(&ops);
    println!(
        "    kill -9 with zero delay: {n} of {} acknowledged operations restored (writes took {:?}, the 200-file burst the last {:?})",
        ops.len(),
        t_all,
        t_all - t_burst,
    );
    let applied = ctl(&dir, "commit");
    println!("    committed {} change(s) to {node}", applied["applied"]);
    unmount(&dir, m);
    assert!(sessions(&dir).is_empty(), "the session is gone after the commit");
    att.detach();

    // The disk now holds exactly what the restored session showed.
    let out = fsck_partition(&dir, "disk.img", part, f[0].is_some().then_some("new/hello.txt"));
    if f[0].as_ref() == Some(&hello) {
        assert!(out.contains("ciao dal dispositivo"), "debugfs: {out}");
    }
    let mut fs = image_fs(&img, part);
    assert!(disk_view(fs.as_mut(), &files, &dirs) == view, "the committed disk differs from the restored session");
}

#[test]
#[ignore]
fn e2e_raw_device_kill9_during_commit_then_resume_is_fsck_clean() {
    let dir = workdir("dev_shadow_kill9_commit");
    std::fs::create_dir_all(dir.join("mnt")).unwrap();
    let img = dir.join("disk.img");
    let part = make_disk(&img, 384 * 2048 + 5, "5E8B2C14-90AF-4D3E-B6C7-1F2E3D4C5B6A");
    let mut att = Attached::new(&img);
    // The whole disk's raw node + partition 1 (PartitionDev over the sector-aligned raw node).
    let node = Attached::raw(&att.disk);
    // Park the commit after its 8th durable checkpoint, so the SIGKILL lands half-way through
    // it however loaded the machine is.
    let m = mount(&dir, &node, "shadow", &["--partition", "1"], &[("LUMENDISK_TEST_STALL_COMMIT_AFTER", "8")]).unwrap();
    let mp = m.mp.clone();
    let mut want: Vec<(String, Vec<u8>)> = vec![];
    for d in 0..24 {
        std::fs::create_dir(mp.join(format!("dir{d:02}"))).unwrap();
        for f in 0..40 {
            let p = format!("dir{d:02}/file{f:03}.txt");
            let data = format!("{p} {}\n", "x".repeat((d * 40 + f) % 300)).into_bytes();
            std::fs::write(mp.join(&p), &data).unwrap();
            want.push((p, data));
        }
    }
    let big = testutil::pattern(80 << 20, 11);
    std::fs::write(mp.join("big.bin"), &big).unwrap();
    want.push(("big.bin".into(), big));
    let key = ctl(&dir, "status")["shadow"]["session_key"].as_str().unwrap().to_string();

    // Commit in the background; SIGKILL the server once it is parked inside the commit.
    let progress = dir.join("support/Shadow").join(&key).join("progress.ldj");
    let d2 = dir.clone();
    let committer = std::thread::spawn(move || cli(&d2).args(["ctl", "c.sock", "commit"]).output().unwrap());
    let t0 = Instant::now();
    let mut m = m;
    loop {
        // 22-byte frames: the 8th checkpoint is durable.
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
    assert_eq!(s[0]["source_path"], node.as_str());
    assert_eq!(s[0]["partition"], 1);
    assert_eq!(s[0]["connected"], true);
    let (applied, total) = (s[0]["commit_applied"].as_u64().unwrap(), s[0]["commit_total"].as_u64().unwrap());
    assert!(applied < total, "{s:?}");
    println!("    killed during the commit on {node}: {applied} of {total} steps checkpointed");
    assert!(mount(&dir, &node, "readWrite", &["--partition", "1"], &[]).is_err(), "read/write must be refused while a commit is interrupted");

    // Mounting the same node in Shadow mode finishes the commit.
    let m = mount(&dir, &node, "shadow", &["--partition", "1", "--expect-session", &key], &[]).unwrap();
    let st = ctl(&dir, "status");
    assert_eq!(st["shadow"]["state"], "pending", "{st}");
    assert_eq!(st["shadow"]["added"], 0, "{st}");
    let mp = m.mp.clone();
    assert_eq!(std::fs::read(mp.join("dir07/file013.txt")).unwrap(), want.iter().find(|w| w.0 == "dir07/file013.txt").unwrap().1);
    unmount(&dir, m);
    assert!(sessions(&dir).is_empty());
    att.detach();

    let out = fsck_partition(&dir, "disk.img", part, Some("dir07/file013.txt"));
    assert!(out.contains("dir07/file013.txt x"), "debugfs: {out}");
    let mut fs = image_fs(&img, part);
    for (p, data) in &want {
        let got = read_path(fs.as_mut(), p).unwrap_or_else(|| panic!("{p} missing"));
        assert!(got == *data, "{p}: content differs");
    }
}

/// The block layer on the four kinds of node of an attached disk: size and sector size from
/// the ioctls (a partition node reports the partition), byte-granular I/O on raw nodes through
/// read-modify-write, the odd-sized tail of the partition, the GPT seen through the whole disk,
/// flush (F_FULLFSYNC is refused by device nodes). Every write is checked on the image file
/// after the detach.
#[test]
#[ignore]
fn e2e_block_layer_on_disk_partition_and_raw_nodes() {
    let dir = workdir("dev_block_nodes");
    let img = dir.join("disk.img");
    let sectors = 24 * 2048 + 3;
    let (start, len) = gpt_image(&img, sectors, "0B9D3E7A-1C2F-4A5B-8D6E-7F8091A2B3C4");
    let total = std::fs::metadata(&img).unwrap().len();
    let mut att = Attached::new(&img);
    let mut reference = vec![0u8; len as usize];
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = move |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    // Block node first, raw nodes after: no buffered read may follow a raw write.
    let slice = att.slice(1);
    let whole_raw = Attached::raw(&att.disk);
    for (node, partition) in [(slice.clone(), 0u32), (Attached::raw(&slice), 0), (whole_raw.clone(), 1)] {
        let f = FileDev::open(Path::new(&node), true).unwrap();
        assert_eq!(f.sector_size(), Some(512), "{node}");
        assert_eq!(f.size(), if partition == 0 { len } else { total }, "{node}: size from DKIOCGETBLOCKCOUNT");
        drop(f);
        if partition == 1 {
            let whole = block::open_source(&node, true, None).unwrap();
            let parts = block::parse_partitions(whole.as_ref()).unwrap();
            assert_eq!(parts.len(), 1, "{parts:?}");
            assert_eq!((parts[0].start, parts[0].size, parts[0].scheme.as_str()), (start, len, "GPT"));
        }
        let (dev, _) = lumendisk_core::volume::open_partition(&node, partition, false, None).unwrap();
        assert_eq!(dev.size(), len, "{node}");
        for i in 0..400u64 {
            // bias towards both ends of the partition (the tail isn't 4 KiB-aligned)
            let off = match i % 4 {
                0 => rnd(4096),
                1 => len - 1 - rnd(6000),
                _ => rnd(len),
            };
            let n = 1 + rnd((len - off).min(20_000)) as usize;
            if i % 2 == 0 {
                let data: Vec<u8> = (0..n as u64).map(|k| (k * 7 + i + off) as u8).collect();
                dev.write_at(off, &data).unwrap_or_else(|e| panic!("{node}: write {off}+{n}: {e}"));
                reference[off as usize..off as usize + n].copy_from_slice(&data);
            } else {
                let mut got = vec![0u8; n];
                dev.read_at(off, &mut got).unwrap_or_else(|e| panic!("{node}: read {off}+{n}: {e}"));
                assert!(got[..] == reference[off as usize..off as usize + n], "{node}: read {off}+{n} differs");
            }
        }
        let mut last = [0u8; 3];
        dev.write_at(len - 3, b"END").unwrap();
        dev.read_at(len - 3, &mut last).unwrap();
        assert_eq!(&last, b"END", "{node}");
        reference[len as usize - 3..].copy_from_slice(b"END");
        assert!(dev.read_at(len - 2, &mut last).is_err(), "{node}: past the end");
        dev.flush().unwrap_or_else(|e| panic!("{node}: flush: {e}"));
        println!("    {node}{}: {len} bytes, 400 byte-granular ops OK", if partition > 0 { " partition 1" } else { "" });
    }
    att.detach();
    let mut on_disk = vec![0u8; len as usize];
    std::fs::File::open(&img).unwrap().read_exact_at(&mut on_disk, start).unwrap();
    assert!(on_disk == reference, "the image file doesn't hold what was written through the nodes");
}
