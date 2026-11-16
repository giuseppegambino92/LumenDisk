//! Phase 3 end-to-end tests.
//!
//!   cargo test -p lumendisk-core --release --test phase3_e2e -- --ignored --test-threads=1 --nocapture
mod common;
use common::*;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumendisk_core::block::{BlockDev, FileDev};
use lumendisk_core::vfs::{open_fs, FileSystem, MountDefaults};
use lumendisk_core::volume::{runtime, MountMode, MountRequest, RemoteMountRequest, VolumeStatus};
use lumendisk_core::wal::{Fault, Journaled, WalDev};

fn setup(name: &str) -> PathBuf {
    setup_in_process("phase3", name)
}

fn req(dir: &Path, path: &str, mode: MountMode) -> MountRequest {
    MountRequest {
        path: path.into(),
        partition: 0,
        mode,
        ghost_shield: true,
        ghost_rules: None,
        uid_map: None,
        mount_root: Some(mount_root(&dir).to_string_lossy().into_owned()),
        name: None,
        fd: None,
        volume_icon: None,
        media_uuid: None,
        expect_session: None,
    }
}

fn status(id: &str) -> VolumeStatus {
    runtime().list().into_iter().find(|v| v.id == id).expect("volume vanished")
}

fn wait_for(what: &str, max: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < max, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn sh(cmd: &mut Command) -> (i32, String) {
    let o = cmd.output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    (o.status.code().unwrap_or(-1), s)
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

// ───────────────────────── Apache mod_dav (RFC 4918) ─────────────────────────

const APACHE_BOOT: &str = r#"set -e
a2enmod -q dav dav_fs dav_lock auth_basic >/dev/null
mkdir -p /srv/dav/Documenti /var/lib/dav && echo 'dal NAS' > /srv/dav/Documenti/nas.txt
chown -R www-data:www-data /srv/dav /var/lib/dav
htpasswd -bc /etc/apache2/dav.htpasswd pi raspberry >/dev/null 2>&1
cat > /etc/apache2/sites-available/000-default.conf <<'EOF'
<VirtualHost *:80>
  DavLockDB /var/lib/dav/lockdb
  Alias /dav /srv/dav
  <Directory /srv/dav>
    Dav On
    AuthType Basic
    AuthName "LumenDisk test NAS"
    AuthUserFile /etc/apache2/dav.htpasswd
    Require valid-user
    Options Indexes
  </Directory>
</VirtualHost>
EOF
exec apache2ctl -D FOREGROUND"#;

struct Container(String);
impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["unpause", &self.0]).output();
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

fn basic() -> String {
    use base64::Engine;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("pi:raspberry"))
}

#[test]
#[ignore]
fn e2e_webdav_apache_rfc4918_with_offline_queue() {
    let dir = setup("p3_webdav");
    let name = format!("lumendisk-dav-{}", std::process::id());
    let _ = Command::new("docker").args(["rm", "-f", &name]).output();
    let (c, o) = sh(Command::new("docker").args(["run", "-d", "--rm", "--name", &name, "-p", "127.0.0.1::80", "lumendisk-toolbox", "sh", "-c", APACHE_BOOT]));
    assert_eq!(c, 0, "{o}");
    let _guard = Container(name.clone());
    let (_, o) = sh(Command::new("docker").args(["port", &name, "80/tcp"]));
    let port: u16 = o.lines().next().and_then(|l| l.rsplit(':').next()).unwrap().trim().parse().unwrap();
    let base = format!("http://127.0.0.1:{port}/dav");
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(5)).build();
    wait_for("apache", Duration::from_secs(20), || {
        agent.request("PROPFIND", &format!("{base}/")).set("Authorization", &basic()).set("Depth", "0").call().map(|r| r.status() == 207).unwrap_or(false)
    });
    // The server speaks RFC 4918 class 2: DAV header, LOCK / UNLOCK.
    let r = agent.request("OPTIONS", &format!("{base}/")).set("Authorization", &basic()).call().unwrap();
    let dav = r.header("DAV").unwrap_or("").to_string();
    assert!(dav.contains('1') && dav.contains('2'), "DAV classes: {dav}");
    let lockbody = r#"<?xml version="1.0" encoding="utf-8"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner>LumenDisk</D:owner></D:lockinfo>"#;
    let r = agent.request("LOCK", &format!("{base}/Documenti/nas.txt")).set("Authorization", &basic()).set("Timeout", "Second-60").send_string(lockbody).unwrap();
    let token = r.header("Lock-Token").unwrap().to_string();
    assert!(token.contains("opaquelocktoken"), "{token}");
    let r = agent.request("UNLOCK", &format!("{base}/Documenti/nas.txt")).set("Authorization", &basic()).set("Lock-Token", &token).call().unwrap();
    assert_eq!(r.status(), 204);

    let rreq = RemoteMountRequest {
        kind: "webdav".into(),
        url: base.clone(),
        password: Some("raspberry".into()),
        identity_file: None,
        username: Some("pi".into()),
        pairing_code: None,
        ssh_config_file: None,
        known_hosts_file: None,
        extra_ssh_options: vec![],
        fingerprint: None,
        mode: MountMode::ReadWrite,
        ghost_shield: true,
        name: Some("NAS".into()),
        mount_root: Some(mount_root(&dir).to_string_lossy().into_owned()),
        volume_icon: None,
    };
    // Wrong password is refused.
    assert!(runtime().mount_remote(RemoteMountRequest { password: Some("nope".into()), ..rreq.clone() }).is_err());
    let st = runtime().mount_remote(rreq.clone()).unwrap();
    assert_eq!(format!("{:?}", st.kind), "Webdav");
    let mp = PathBuf::from(&st.mountpoint);
    assert_eq!(fs::read_to_string(mp.join("Documenti/nas.txt")).unwrap(), "dal NAS\n");
    let words = fs::read("/usr/share/dict/words").unwrap();
    fs::write(mp.join("Documenti/parole.txt"), &words).unwrap();
    fs::create_dir(mp.join("Città Ünicode")).unwrap();
    fs::write(mp.join("Città Ünicode/nota.md"), "# ciao").unwrap();
    fs::write(mp.join("da rinominare.txt"), "x").unwrap();
    fs::rename(mp.join("da rinominare.txt"), mp.join("Documenti/rinominato.txt")).unwrap();
    fs::write(mp.join("Documenti/.DS_Store"), "junk").unwrap();
    {
        // overwrite in the middle (generic WebDAV has no partial PUT: whole-file re-upload)
        use std::io::{Seek, SeekFrom, Write};
        let mut f = fs::OpenOptions::new().write(true).open(mp.join("Documenti/nas.txt")).unwrap();
        f.seek(SeekFrom::Start(4)).unwrap();
        f.write_all(b"MAC").unwrap();
    }
    wait_for("webdav upload", Duration::from_secs(30), || status(&st.id).remote.map(|r| r.pending_ops == 0).unwrap_or(false));
    let (_, o) = sh(Command::new("docker").args(["exec", &name, "sh", "-c", "cd /srv/dav && sha256sum Documenti/parole.txt && cat Documenti/nas.txt && cat 'Città Ünicode/nota.md' && ls -a Documenti"]));
    assert!(o.contains(&sha256_hex(&words)), "{o}");
    assert!(o.contains("dal MAC") && o.contains("# ciao") && o.contains("rinominato.txt"), "{o}");
    assert!(!o.contains(".DS_Store"), "Ghost Shield: junk must not reach the NAS\n{o}");

    // Browse the folders while online, as Finder does before anything is saved into them.
    // Offline, a new name can only be created in a folder whose listing is cached: otherwise
    // the name might exist on the server and the queued create would clobber it (EIO instead).
    // (This used to pass without it only when some other process — Spotlight, Docker Desktop's
    // VM — happened to list the mount while online.)
    assert!(fs::read_dir(&mp).unwrap().count() >= 2);
    assert!(fs::read_dir(mp.join("Documenti")).unwrap().count() >= 3);

    // ── offline: pause the server, keep working, quit, come back ──
    assert!(Command::new("docker").args(["pause", &name]).status().unwrap().success());
    wait_for("offline", Duration::from_secs(8), || !status(&st.id).remote.unwrap().online);
    std::thread::sleep(Duration::from_millis(1200));
    let t0 = Instant::now();
    let n = fs::read_dir(mp.join("Documenti")).unwrap().count();
    let _ = fs::metadata(mp.join("Documenti/parole.txt")).unwrap();
    let lt = t0.elapsed();
    assert!(n >= 3 && lt < Duration::from_millis(50), "offline ls/stat took {lt:?}");
    fs::write(mp.join("Documenti/offline 1.txt"), "scritto senza rete").unwrap();
    fs::create_dir(mp.join("Nuova offline")).unwrap();
    fs::write(mp.join("Nuova offline/dentro.bin"), vec![42u8; 300_000]).unwrap();
    assert!(status(&st.id).remote.unwrap().pending_ops >= 3);
    // Eject while still offline: nothing is lost, the queue is persisted.
    unmount_settled(&st.id);
    // (this volume's queue: other remote volumes of the suite share the support dir)
    let qdir = runtime().support().join("OfflineQueue");
    let ours = |p: &PathBuf| p.ends_with("queue.json") && fs::read_to_string(p).map(|q| q.contains(&base)).unwrap_or(false);
    let queued: Vec<_> = walk(&qdir).into_iter().filter(|p| ours(p)).collect();
    assert_eq!(queued.len(), 1, "offline queue must be on disk: {:?}", walk(&qdir));
    // Server comes back; the volume is mounted again and the queue drains by itself.
    assert!(Command::new("docker").args(["unpause", &name]).status().unwrap().success());
    let st2 = runtime().mount_remote(rreq).unwrap();
    wait_for("queued changes synced after restart", Duration::from_secs(30), || status(&st2.id).remote.map(|r| r.online && r.pending_ops == 0).unwrap_or(false));
    let (_, o) = sh(Command::new("docker").args(["exec", &name, "sh", "-c", "cat '/srv/dav/Documenti/offline 1.txt'; echo; stat -c %s '/srv/dav/Nuova offline/dentro.bin'"]));
    assert!(o.contains("scritto senza rete") && o.contains("300000"), "{o}");
    unmount_settled(&st2.id);
    assert!(!walk(&qdir).iter().any(ours), "queue must be cleared once synced");
}

fn walk(p: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                v.extend(walk(&path));
            } else {
                v.push(path);
            }
        }
    }
    v
}

// ───────────────────────── ordered WAL: 64 MB + power loss ─────────────────────────

fn ordered_crash(fs_name: &str, mkfs: &str, fsck: &str) {
    let dir = setup(&format!("p3_ordered_{fs_name}"));
    let img = dir.join("disk.img");
    make_sparse(&img, 256);
    let (c, o) = docker(&dir, &format!("{mkfs} disk.img"));
    assert_eq!(c, 0, "{o}");
    let path = img.to_string_lossy().into_owned();
    let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
    let key = lumendisk_core::volume::Runtime::wal_key(&dev, &path, 0);
    let wp = lumendisk_core::wal::wal_path(&runtime().support(), &key);
    let video: Vec<u8> = (0..64u32 << 20).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let stats;
    {
        let wal = WalDev::open(dev.clone(), &wp, &key).unwrap();
        let drv = open_fs(wal.clone(), false, MountDefaults::default()).unwrap();
        let mut j = Journaled { inner: drv, wal: wal.clone() };
        let root = j.root_ino();
        let f = j.create(root, "video.mov", 0o644, 0, 0).unwrap();
        for (k, chunk) in video.chunks(1 << 20).enumerate() {
            j.write(f, (k as u64) << 20, chunk).unwrap();
        }
        j.sync().unwrap(); // the 64 MiB file is durable: measure what it cost in the journal
        stats = wal.stats();
        let d = j.mkdir(root, "Progetto", 0o755, 0, 0).unwrap();
        for i in 0..100 {
            let g = j.create(d, &format!("clip {i}.txt"), 0o644, 0, 0).unwrap();
            j.write(g, 0, format!("clip {i}").as_bytes()).unwrap();
        }
        wal.inject_fault(Fault::CrashAfterWal { apply_blocks: 3 });
        let _ = j.sync();
        std::mem::forget(j);
        std::mem::forget(wal);
    }
    drop(dev);
    let overhead = stats.wal_bytes as f64 * 100.0 / video.len() as f64;
    println!("{fs_name}: 64 MiB file → {} MiB written in place, WAL {} KiB ({overhead:.2} % overhead)", stats.data_bytes >> 20, stats.wal_bytes >> 10);
    assert!(overhead < 5.0, "WAL overhead {overhead:.2} %");
    fs::copy(&img, dir.join("crashed.img")).unwrap();
    let (cc, _) = docker(&dir, &fsck.replace("disk.img", "crashed.img"));
    println!("{fs_name}: fsck right after the simulated power loss: exit {cc}");
    let st = runtime().mount(req(&dir, &path, MountMode::ReadWrite)).unwrap();
    assert!(st.journal.replayed_at_mount >= 1);
    assert_eq!(st.journal.mode, "ordered");
    let mp = PathBuf::from(&st.mountpoint);
    assert!(fs::read(mp.join("video.mov")).unwrap() == video, "video content after replay");
    assert_eq!(fs::read_dir(mp.join("Progetto")).unwrap().count(), 100);
    unmount_settled(&st.id);
    let (c, o) = docker(&dir, fsck);
    assert_eq!(c, 0, "fsck after replay:\n{o}");
}

#[test]
#[ignore]
fn e2e_ordered_wal_64mb_power_loss_ext4() {
    ordered_crash("ext4", "mkfs.ext4 -q -F -L ordered", "e2fsck -fn disk.img");
}

#[test]
#[ignore]
fn e2e_ordered_wal_64mb_power_loss_fat32() {
    ordered_crash("fat32", "mkfs.fat -F 32 -n ORDERED", "fsck.fat -n disk.img");
}

// ───────────────────────── a real attached block device ─────────────────────────

struct Attached(String);
impl Drop for Attached {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/hdiutil").args(["detach", &self.0, "-force"]).output();
    }
}

#[test]
#[ignore]
fn e2e_physical_device_mount_benchmark_health() {
    let dir = setup("p3_device");
    let img = dir.join("usb.img");
    runtime().create_image(img.to_str().unwrap(), "EXT4", 256, "CHIAVETTA").unwrap();
    let (c, o) = sh(Command::new("/usr/bin/hdiutil").args(["attach", "-nomount", "-imagekey", "diskimage-class=CRawDiskImage"]).arg(&img));
    assert_eq!(c, 0, "{o}");
    let devnode = o.split_whitespace().find(|t| t.starts_with("/dev/disk")).unwrap().to_string();
    let _att = Attached(devnode.clone());
    // macOS can't mount ext4: the device is there but unmounted.
    let (_, info) = sh(Command::new("/usr/sbin/diskutil").args(["info", &devnode]));
    assert!(info.contains("Mounted:") && info.contains("No"), "{info}");
    let st = runtime().mount(req(&dir, &devnode, MountMode::ReadWrite)).unwrap();
    assert_eq!(st.device.as_deref(), Some(devnode.trim_start_matches("/dev/")));
    assert_eq!(st.fs_type, "EXT4");
    let mp = PathBuf::from(&st.mountpoint);
    fs::write(mp.join("dalla chiavetta.txt"), "scritto sul dispositivo a blocchi").unwrap();
    let t0 = Instant::now();
    let b = lumendisk_core::health::benchmark(&mp).unwrap();
    println!("speed test on {devnode} via LumenDisk: read {:.0} MB/s, write {:.0} MB/s, {:.0} IOPS 4K, {:.2} ms ({:?})", b.read_mbps, b.write_mbps, b.iops_4k, b.latency_ms, t0.elapsed());
    assert!(b.read_mbps > 1.0 && b.write_mbps > 1.0 && b.iops_4k > 10.0);
    assert!(t0.elapsed() < Duration::from_secs(8));
    assert!(!fs::read_dir(&mp).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".lumendisk-speedtest")));
    let h = lumendisk_core::health::disk_health(&devnode).unwrap();
    println!("health: {h:?}");
    assert!(h.protocol.is_some());
    unmount_settled(&st.id);
    drop(_att);
    let (c, o) = docker(&dir, "e2fsck -fn usb.img && debugfs -R 'cat \"/dalla chiavetta.txt\"' usb.img");
    assert_eq!(c, 0, "{o}");
    assert!(o.contains("scritto sul dispositivo a blocchi"), "{o}");
}

// ───────────────────────── TLS LAN share: encryption, pinning, impostors ─────────────────────────

/// TCP relay that records every byte crossing it (what a Wi-Fi eavesdropper would see).
fn sniffing_proxy(target: u16, log: Arc<std::sync::Mutex<Vec<u8>>>) -> u16 {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let log = log.clone();
            std::thread::spawn(move || {
                let Ok(up) = TcpStream::connect(("127.0.0.1", target)) else { return };
                let pump = |mut a: TcpStream, mut b: TcpStream, log: Arc<std::sync::Mutex<Vec<u8>>>| {
                    let mut buf = [0u8; 65536];
                    while let Ok(n) = a.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        log.lock().unwrap().extend_from_slice(&buf[..n]);
                        if b.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = b.shutdown(std::net::Shutdown::Both);
                };
                let (a2, u2, l2) = (c.try_clone().unwrap(), up.try_clone().unwrap(), log.clone());
                std::thread::spawn(move || pump(u2, a2, l2));
                pump(c, up, log);
            });
        }
    });
    port
}

fn lan_req(dir: &Path, url: String, code: &str, fp: Option<String>) -> RemoteMountRequest {
    RemoteMountRequest {
        kind: "lan".into(),
        url,
        password: None,
        identity_file: None,
        username: None,
        pairing_code: Some(code.into()),
        ssh_config_file: None,
        known_hosts_file: None,
        extra_ssh_options: vec![],
        fingerprint: fp,
        mode: MountMode::ReadWrite,
        ghost_shield: true,
        name: Some("Mac di Anna".into()),
        mount_root: Some(mount_root(&dir).to_string_lossy().into_owned()),
        volume_icon: None,
    }
}

#[test]
#[ignore]
fn e2e_lan_share_tls_encrypted_pinned_and_impostor_proof() {
    let dir = setup("p3_tls");
    for n in ["host.img", "evil.img"] {
        runtime().create_image(dir.join(n).to_str().unwrap(), "EXT4", 48, n.trim_end_matches(".img")).unwrap();
    }
    let host = runtime().mount(req(&dir, dir.join("host.img").to_str().unwrap(), MountMode::ReadWrite)).unwrap();
    let evil = runtime().mount(req(&dir, dir.join("evil.img").to_str().unwrap(), MountMode::ReadWrite)).unwrap();
    let secret = "CONTENUTO-SEGRETO-0123456789 ".repeat(2000);
    fs::write(PathBuf::from(&host.mountpoint).join("segreto.txt"), &secret).unwrap();
    let share = runtime().share_start(&host.id).unwrap();
    let fake = runtime().share_start(&evil.id).unwrap();
    assert_eq!(share.scheme, "https");
    assert_eq!(share.fingerprint.len(), 64);

    // 1. Encrypted: an eavesdropper between the two Macs sees neither the data nor the token.
    let sniff = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pport = sniffing_proxy(share.port, sniff.clone());
    let client = runtime().mount_remote(lan_req(&dir, format!("https://127.0.0.1:{pport}"), &share.pairing_code, Some(share.fingerprint.clone()))).unwrap();
    let cmp = PathBuf::from(&client.mountpoint);
    assert_eq!(fs::read_to_string(cmp.join("segreto.txt")).unwrap(), secret);
    fs::write(cmp.join("dal client.txt"), "RISPOSTA-RISERVATA-4242").unwrap();
    wait_for("upload over TLS", Duration::from_secs(15), || status(&client.id).remote.map(|r| r.pending_ops == 0).unwrap_or(false));
    let wire = sniff.lock().unwrap().clone();
    let token = lumendisk_core::lan::token_from_pairing_code(&share.pairing_code);
    let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
    assert!(wire.len() > secret.len(), "traffic did flow through the proxy ({} bytes)", wire.len());
    assert!(!contains(&wire, b"CONTENUTO-SEGRETO"), "file content visible on the wire!");
    assert!(!contains(&wire, b"RISPOSTA-RISERVATA"), "uploaded content visible on the wire!");
    assert!(!contains(&wire, token.as_bytes()), "bearer token visible on the wire!");
    assert!(!contains(&wire, b"PROPFIND"), "WebDAV verbs visible on the wire!");
    println!("TLS: {} bytes relayed, no plaintext content/token/verbs observed", wire.len());
    unmount_settled(&client.id);

    // 2. Impostor: right certificate pin for the *impostor*, but it doesn't know our pairing
    //    code → the proof fails and the token is never sent.
    let imp = runtime().mount_remote(lan_req(&dir, format!("https://127.0.0.1:{}", fake.port), &share.pairing_code, Some(fake.fingerprint.clone())));
    assert!(imp.is_err(), "impostor must be refused");
    // 3. A pinned fingerprint that doesn't match the server is refused at the TLS layer.
    let wrong = runtime().mount_remote(lan_req(&dir, format!("https://127.0.0.1:{}", share.port), &share.pairing_code, Some(fake.fingerprint.clone())));
    assert!(wrong.is_err(), "wrong pin must be refused");
    // 4. Typed address without a fingerprint: learned on first contact + pairing proof.
    let typed = runtime().mount_remote(lan_req(&dir, format!("http://127.0.0.1:{}", share.port), &share.pairing_code, None)).unwrap();
    assert_eq!(fs::read_to_string(PathBuf::from(&typed.mountpoint).join("dal client.txt")).unwrap(), "RISPOSTA-RISERVATA-4242");
    unmount_settled(&typed.id);
    runtime().share_stop(&host.id).unwrap();
    runtime().share_stop(&evil.id).unwrap();
    unmount_settled(&host.id);
    unmount_settled(&evil.id);
}
