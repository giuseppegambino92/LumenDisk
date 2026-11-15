//! Phase 2 end-to-end tests (real mount_nfs + macOS kernel NFS client, upstream tools as oracles).
//!
//!   cargo test -p lumendisk-core --release --test phase2_e2e -- --ignored --test-threads=1
mod common;
use common::*;

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumendisk_core::block::{BlockDev, FileDev};
use lumendisk_core::vfs::{open_fs, FileSystem, MountDefaults};
use lumendisk_core::volume::{runtime, ImageUrlRequest, MountMode, MountRequest, RemoteMountRequest, VolumeStatus};
use lumendisk_core::wal::{Fault, Journaled, WalDev};

fn setup(name: &str) -> PathBuf {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("{info}");
        for v in runtime().list() {
            let _ = runtime().unmount(&v.id, true);
        }
    }));
    let dir = workdir(name);
    runtime().set_support_dir(&dir.join("support"));
    fs::create_dir_all(dir.join("mnt")).unwrap();
    dir
}

fn req(dir: &Path, img: &str, mode: MountMode) -> MountRequest {
    MountRequest {
        path: dir.join(img).to_string_lossy().into_owned(),
        partition: 0,
        mode,
        ghost_shield: true,
        ghost_rules: None,
        uid_map: None,
        mount_root: Some(dir.join("mnt").to_string_lossy().into_owned()),
        name: None,
        fd: None,
        volume_icon: None,
    }
}

fn status(id: &str) -> VolumeStatus {
    runtime().list().into_iter().find(|v| v.id == id).expect("volume vanished")
}

fn fnv(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ───────────────────────── 1. NTFS symlinks through Finder's NFS path ─────────────────────────

#[test]
#[ignore]
fn e2e_ntfs_symlinks_cp_r() {
    let dir = setup("p2_ntfs_symlinks");
    make_sparse(&dir.join("disk.img"), 128);
    let (c, o) = docker(&dir, "mkntfs -F -Q -L WINLINKS disk.img");
    assert_eq!(c, 0, "{o}");
    // A source tree with relative, absolute and directory symlinks.
    let src = dir.join("src");
    fs::create_dir_all(src.join("sub/deep")).unwrap();
    fs::write(src.join("sub/file.txt"), "contenuto reale\n").unwrap();
    fs::write(src.join("sub/deep/x.bin"), vec![7u8; 100_000]).unwrap();
    std::os::unix::fs::symlink("sub/file.txt", src.join("rel_link")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", src.join("abs_link")).unwrap();
    std::os::unix::fs::symlink("sub", src.join("dir_link")).unwrap();
    std::os::unix::fs::symlink("../file.txt", src.join("sub/deep/up_link")).unwrap();
    std::os::unix::fs::symlink("does/not/exist", src.join("dangling")).unwrap();

    let st = runtime().mount(req(&dir, "disk.img", MountMode::ReadWrite)).unwrap();
    let mp = PathBuf::from(&st.mountpoint);
    let out = std::process::Command::new("/bin/cp").arg("-R").arg(&src).arg(&mp).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && !err.contains("Input/output error"), "cp -R failed: {err}");
    let m = mp.join("src");
    let expect = [("rel_link", "sub/file.txt"), ("abs_link", "/etc/hosts"), ("dir_link", "sub"), ("sub/deep/up_link", "../file.txt"), ("dangling", "does/not/exist")];
    for (l, t) in expect {
        assert_eq!(fs::read_link(m.join(l)).unwrap().to_string_lossy(), t, "{l} via NFS");
    }
    assert_eq!(fs::read_to_string(m.join("rel_link")).unwrap(), "contenuto reale\n");
    assert_eq!(fs::read(m.join("dir_link/deep/x.bin")).unwrap().len(), 100_000);
    runtime().unmount(&st.id, false).unwrap();

    // Byte-for-byte targets after a fresh open of the raw image.
    let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&dir.join("disk.img"), true).unwrap());
    let mut ntfs = open_fs(dev, true, MountDefaults::default()).unwrap();
    let root = ntfs.root_ino();
    let s = ntfs.lookup(root, "src").unwrap();
    for (l, t) in expect {
        let mut cur = s;
        let parts: Vec<&str> = l.split('/').collect();
        for p in &parts[..parts.len() - 1] {
            cur = ntfs.lookup(cur, p).unwrap();
        }
        let ino = ntfs.lookup(cur, parts[parts.len() - 1]).unwrap();
        assert_eq!(ntfs.readlink(ino).unwrap().as_bytes(), t.as_bytes(), "{l} on disk");
    }
    drop(ntfs);
    // Upstream: ntfsfix is clean, ntfs-3g resolves the relative links.
    let (c, o) = docker_priv(
        &dir,
        "ntfsfix -n disk.img && mkdir -p /m && ntfs-3g -o ro disk.img /m && readlink /m/src/rel_link && readlink /m/src/dir_link && cat /m/src/rel_link && ls /m/src/dir_link/deep",
    );
    assert_eq!(c, 0, "{o}");
    assert!(o.contains("sub/file.txt") && o.contains("contenuto reale") && o.contains("x.bin"), "{o}");
}

// ───────────────────────── 2. WAL crash recovery ─────────────────────────

fn crash_and_recover(fs_name: &str, mkfs: &str, fsck: &str) {
    let dir = setup(&format!("p2_wal_{fs_name}"));
    let img = dir.join("disk.img");
    make_sparse(&img, 128);
    let (c, o) = docker(&dir, &format!("{mkfs} disk.img"));
    assert_eq!(c, 0, "{o}");
    let path = img.to_string_lossy().into_owned();
    // Exactly what a mount does: WAL keyed by volume identity in the support dir.
    let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
    let key = lumendisk_core::volume::Runtime::wal_key(&dev, &path, 0);
    let wp = lumendisk_core::wal::wal_path(&runtime().support(), &key);
    {
        let wal = WalDev::open(dev.clone(), &wp, &key).unwrap();
        let drv = open_fs(wal.clone(), false, MountDefaults::default()).unwrap();
        let mut j = Journaled { inner: drv, wal: wal.clone() };
        let root = j.root_ino();
        let d = j.mkdir(root, "Lavoro", 0o755, 0, 0).unwrap();
        for i in 0..300 {
            let f = j.create(d, &format!("documento numero {i}.txt"), 0o644, 0, 0).unwrap();
            j.write(f, 0, format!("riga {i}\n").repeat(50).as_bytes()).unwrap();
        }
        // Power loss half-way through writing the metadata in place.
        wal.inject_fault(Fault::CrashAfterWal { apply_blocks: 4 });
        let _ = j.sync();
        std::mem::forget(j);
        std::mem::forget(wal);
    }
    drop(dev);
    // Without LumenDisk's replay the image is damaged:
    fs::copy(&img, dir.join("crashed.img")).unwrap();
    let (c_crashed, o_crashed) = docker(&dir, &fsck.replace("disk.img", "crashed.img"));
    println!("{fs_name} fsck BEFORE replay: exit {c_crashed}\n{}", o_crashed.lines().take(12).collect::<Vec<_>>().join("\n"));
    assert_ne!(c_crashed, 0, "the simulated crash should leave an inconsistent image");
    // Normal mount → automatic replay → consistent.
    assert!(wp.exists(), "WAL must survive the crash");
    let st = runtime().mount(req(&dir, "disk.img", MountMode::ReadWrite)).unwrap();
    assert!(st.journal.replayed_at_mount >= 1, "{:?}", st.journal);
    let n = fs::read_dir(PathBuf::from(&st.mountpoint).join("Lavoro")).unwrap().count();
    assert_eq!(n, 300);
    assert_eq!(fs::read_to_string(PathBuf::from(&st.mountpoint).join("Lavoro/documento numero 299.txt")).unwrap(), "riga 299\n".repeat(50));
    runtime().unmount(&st.id, false).unwrap();
    assert!(!wp.exists(), "clean unmount removes the WAL");
    let (c, o) = docker(&dir, fsck);
    assert_eq!(c, 0, "fsck after replay:\n{o}");
}

#[test]
#[ignore]
fn e2e_wal_crash_recovery_fat32() {
    crash_and_recover("fat32", "mkfs.fat -F 32 -n WALTEST", "fsck.fat -n disk.img");
}

#[test]
#[ignore]
fn e2e_wal_crash_recovery_ext4() {
    crash_and_recover("ext4", "mkfs.ext4 -q -F -L waltest", "e2fsck -fn disk.img");
}

// ───────────────────────── 3. Loopback security ─────────────────────────

fn xdr_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_be_bytes());
}
fn xdr_opaque(v: &mut Vec<u8>, b: &[u8]) {
    xdr_u32(v, b.len() as u32);
    v.extend_from_slice(b);
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

fn auth_unix(uid: u32) -> Vec<u8> {
    let mut b = vec![];
    xdr_u32(&mut b, 0);
    xdr_opaque(&mut b, b"attacker");
    xdr_u32(&mut b, uid);
    xdr_u32(&mut b, uid);
    xdr_u32(&mut b, 0);
    b
}

/// Send one RPC call; return the reply body (after the record mark).
fn rpc(sock: &mut TcpStream, xid: u32, prog: u32, proc_: u32, cred: Option<Vec<u8>>, args: &[u8]) -> Vec<u8> {
    let mut m = vec![];
    xdr_u32(&mut m, xid);
    xdr_u32(&mut m, 0); // CALL
    xdr_u32(&mut m, 2);
    xdr_u32(&mut m, prog);
    xdr_u32(&mut m, 3);
    xdr_u32(&mut m, proc_);
    match cred {
        Some(c) => {
            xdr_u32(&mut m, 1); // AUTH_UNIX
            xdr_opaque(&mut m, &c);
        }
        None => {
            xdr_u32(&mut m, 0);
            xdr_u32(&mut m, 0);
        }
    }
    xdr_u32(&mut m, 0);
    xdr_u32(&mut m, 0);
    m.extend_from_slice(args);
    let mut rec = (0x8000_0000u32 | m.len() as u32).to_be_bytes().to_vec();
    rec.extend_from_slice(&m);
    sock.write_all(&rec).unwrap();
    let mut hdr = [0u8; 4];
    sock.read_exact(&mut hdr).unwrap();
    let len = (u32::from_be_bytes(hdr) & 0x7fff_ffff) as usize;
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).unwrap();
    body
}

fn be(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())
}

#[test]
#[ignore]
fn e2e_loopback_rejects_foreign_uid_and_second_mount() {
    let dir = setup("p2_security");
    let img = dir.join("disk.img");
    runtime().create_image(img.to_str().unwrap(), "EXT4", 32, "SECURE").unwrap();
    let st = runtime().mount(req(&dir, "disk.img", MountMode::ReadWrite)).unwrap();
    let me = unsafe { libc::geteuid() };
    let mut s = TcpStream::connect(("127.0.0.1", st.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // NULL ping is always allowed.
    let r = rpc(&mut s, 1, 100003, 0, None, &[]);
    assert_eq!((be(&r, 1), be(&r, 2)), (1, 0), "NULL must be MSG_ACCEPTED");
    // GETATTR with a foreign AUTH_SYS uid → MSG_DENIED / AUTH_ERROR / AUTH_TOOWEAK.
    let mut fh = vec![];
    xdr_opaque(&mut fh, &[0u8; 24]);
    let r = rpc(&mut s, 2, 100003, 1, Some(auth_unix(4242)), &fh);
    assert_eq!((be(&r, 1), be(&r, 2), be(&r, 3), be(&r, 4)), (1, 1, 1, 5), "foreign uid must be rejected with AUTH_TOOWEAK: {r:?}");
    // AUTH_NULL for a real procedure → rejected too.
    let r = rpc(&mut s, 3, 100003, 1, None, &fh);
    assert_eq!((be(&r, 2), be(&r, 4)), (1, 5));
    // Our uid but a forged handle (no secret) → NFS3ERR_STALE (70).
    let r = rpc(&mut s, 4, 100003, 1, Some(auth_unix(me)), &fh);
    assert_eq!((be(&r, 2), be(&r, 5), be(&r, 6)), (0, 0, 70), "forged handle must be stale: {r:?}");
    // A second MOUNT (anyone, even us) is refused: the export is locked to mount_nfs's handshake.
    let mut path = vec![];
    xdr_opaque(&mut path, b"/");
    let r = rpc(&mut s, 5, 100005, 1, Some(auth_unix(me)), &path);
    assert_eq!((be(&r, 2), be(&r, 4)), (1, 5), "second MNT must be refused");
    // The real mount keeps working.
    fs::write(PathBuf::from(&st.mountpoint).join("ok.txt"), "still fine").unwrap();
    assert_eq!(fs::read_to_string(PathBuf::from(&st.mountpoint).join("ok.txt")).unwrap(), "still fine");
    runtime().unmount(&st.id, false).unwrap();
}

// ───────────────────────── 4. Remote .img over HTTP Range ─────────────────────────

/// Minimal HTTP/1.1 range server that counts the bytes it serves.
fn range_server(file: PathBuf, served: Arc<AtomicU64>, requests: Arc<AtomicU64>) -> u16 {
    let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = srv.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for rq in srv.incoming_requests() {
            requests.fetch_add(1, Ordering::SeqCst);
            let mut f = fs::File::open(&file).unwrap();
            let total = f.metadata().unwrap().len();
            let range = rq.headers().iter().find(|h| h.field.equiv("Range")).map(|h| h.value.as_str().to_string());
            match range.and_then(|r| r.strip_prefix("bytes=").map(|s| s.to_string())) {
                Some(r) => {
                    let (a, b) = r.split_once('-').unwrap();
                    let a: u64 = a.parse().unwrap();
                    let b: u64 = if b.is_empty() { total - 1 } else { b.parse::<u64>().unwrap().min(total - 1) };
                    use std::io::Seek;
                    let mut body = vec![0u8; (b - a + 1) as usize];
                    f.seek(std::io::SeekFrom::Start(a)).unwrap();
                    f.read_exact(&mut body).unwrap();
                    served.fetch_add(body.len() as u64, Ordering::SeqCst);
                    let resp = tiny_http::Response::from_data(body)
                        .with_status_code(206)
                        .with_header(format!("Content-Range: bytes {a}-{b}/{total}").parse::<tiny_http::Header>().unwrap())
                        .with_header("Accept-Ranges: bytes".parse::<tiny_http::Header>().unwrap());
                    let _ = rq.respond(resp);
                }
                None => {
                    let _ = rq.respond(tiny_http::Response::empty(400));
                }
            }
        }
    });
    port
}

fn make_rpi_image(dir: &Path) {
    make_sparse(&dir.join("rpi.img"), 600);
    let script = "printf 'label: dos\\nstart=8192, size=131072, type=c\\nstart=139264, type=83\\n' | sfdisk -q rpi.img && \
        dd if=/dev/zero of=boot.part bs=1M count=64 status=none && mkfs.fat -F 32 -n bootfs boot.part >/dev/null && \
        echo 'console=serial0,115200 root=PARTUUID=1234-02 rootwait' > cmdline.txt && mcopy -i boot.part cmdline.txt ::/ && \
        dd if=boot.part of=rpi.img bs=512 seek=8192 conv=notrunc status=none && \
        mkdir -p root/etc root/home/pi && echo raspberrypi > root/etc/hostname && head -c 3000000 /dev/urandom > root/home/pi/foto.jpg && chown -R 1000:1000 root/home/pi && \
        dd if=/dev/zero of=root.part bs=1M count=500 status=none && mkfs.ext4 -q -F -L rootfs -d root root.part && \
        dd if=root.part of=rpi.img bs=512 seek=139264 conv=notrunc status=none && rm -f root.part boot.part";
    let (c, o) = docker(dir, script);
    assert_eq!(c, 0, "{o}");
}

#[test]
#[ignore]
fn e2e_remote_image_http_range() {
    let dir = setup("p2_http_range");
    make_rpi_image(&dir);
    let img = dir.join("rpi.img");
    let before = fnv(&fs::read(&img).unwrap());
    let size = fs::metadata(&img).unwrap().len();
    let served = Arc::new(AtomicU64::new(0));
    let requests = Arc::new(AtomicU64::new(0));
    let port = range_server(img.clone(), served.clone(), requests.clone());
    let url = format!("http://127.0.0.1:{port}/raspios.img");
    let ireq = |partition: u32| ImageUrlRequest {
        url: url.clone(),
        username: None,
        password: None,
        identity_file: None,
        partition,
        mode: MountMode::Shadow,
        ghost_shield: true,
        name: None,
        mount_root: Some(dir.join("mnt").to_string_lossy().into_owned()),
        volume_icon: None,
    };
    let t0 = Instant::now();
    let (rep, _) = runtime().inspect_image_url(&ireq(0)).unwrap();
    let kinds: Vec<(u32, Option<String>)> = rep.partitions.iter().map(|p| (p.part.index, p.fs_type.clone())).collect();
    assert_eq!(kinds, vec![(1, Some("FAT32".into())), (2, Some("EXT4".into()))]);
    let boot = runtime().mount_image_url(ireq(1)).unwrap();
    let rootfs = runtime().mount_image_url(ireq(2)).unwrap();
    let mount_time = t0.elapsed();
    println!("inspect + mount of both partitions over HTTP Range: {mount_time:?}");
    let (bmp, rmp) = (PathBuf::from(&boot.mountpoint), PathBuf::from(&rootfs.mountpoint));
    assert!(fs::read_to_string(bmp.join("cmdline.txt")).unwrap().contains("rootwait"));
    assert_eq!(fs::read_to_string(rmp.join("etc/hostname")).unwrap(), "raspberrypi\n");
    assert_eq!(fs::read(rmp.join("home/pi/foto.jpg")).unwrap().len(), 3_000_000);
    // Full read/write in Finder — staged in Shadow, never sent to the server.
    fs::write(bmp.join("ssh"), b"").unwrap();
    fs::write(rmp.join("etc/hostname"), "lumenpi\n").unwrap();
    fs::create_dir_all(rmp.join("home/pi/nuovi")).unwrap();
    fs::write(rmp.join("home/pi/nuovi/nota.txt"), "scritto sul Mac").unwrap();
    assert_eq!(fs::read_to_string(rmp.join("etc/hostname")).unwrap(), "lumenpi\n");
    let sh = status(&rootfs.id).shadow.unwrap();
    assert!(sh.added >= 2 && sh.modified >= 1, "{sh:?}");
    let fetched = served.load(Ordering::SeqCst);
    let stat_fetched = status(&rootfs.id).remote.unwrap().fetched_bytes + status(&boot.id).remote.unwrap().fetched_bytes;
    println!("image {size} bytes, served {fetched} bytes ({:.1}%) in {} requests; status reports {stat_fetched}", fetched as f64 * 100.0 / size as f64, requests.load(Ordering::SeqCst));
    assert!(fetched < size / 10, "should stream sparsely: fetched {fetched} of {size}");
    assert!(mount_time < Duration::from_secs(3), "mount took {mount_time:?}");
    runtime().shadow_rollback(&boot.id).unwrap();
    runtime().shadow_rollback(&rootfs.id).unwrap();
    runtime().unmount(&boot.id, false).unwrap();
    runtime().unmount(&rootfs.id, false).unwrap();
    assert_eq!(fnv(&fs::read(&img).unwrap()), before, "the remote image must never be modified");
}

// ───────────────────────── 5. LAN share + zero-beachball disconnect ─────────────────────────

/// TCP proxy with a "freeze" switch: frozen = connections stay open but nothing is forwarded
/// (exactly what a dropped Wi-Fi link looks like).
fn freezable_proxy(target: u16, frozen: Arc<AtomicBool>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let frozen = frozen.clone();
            std::thread::spawn(move || {
                let Ok(up) = TcpStream::connect(("127.0.0.1", target)) else { return };
                let pump = |mut a: TcpStream, mut b: TcpStream, frozen: Arc<AtomicBool>| {
                    let mut buf = [0u8; 65536];
                    loop {
                        let n = match a.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        while frozen.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        if b.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = b.shutdown(std::net::Shutdown::Both);
                };
                let (a2, u2) = (c.try_clone().unwrap(), up.try_clone().unwrap());
                let f2 = frozen.clone();
                std::thread::spawn(move || pump(u2, a2, f2));
                pump(c, up, frozen);
            });
        }
    });
    port
}

fn wait_for(what: &str, max: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < max, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore]
fn e2e_lan_share_zero_beachball_disconnect() {
    let dir = setup("p2_lan_offline");
    let img = dir.join("host.img");
    runtime().create_image(img.to_str().unwrap(), "EXT4", 64, "DISCO-HOST").unwrap();
    let host = runtime().mount(req(&dir, "host.img", MountMode::ReadWrite)).unwrap();
    let hmp = PathBuf::from(&host.mountpoint);
    fs::write(hmp.join("leggimi.txt"), "file dal Mac che condivide").unwrap();
    fs::create_dir_all(hmp.join("Foto")).unwrap();
    let share = runtime().share_start(&host.id).unwrap();
    assert_eq!(share.pairing_code.len(), 9);
    let frozen = Arc::new(AtomicBool::new(false));
    let pport = freezable_proxy(share.port, frozen.clone());
    let rreq = RemoteMountRequest {
        kind: "lan".into(),
        url: format!("http://127.0.0.1:{pport}"),
        password: None,
        identity_file: None,
        username: None,
        pairing_code: Some(share.pairing_code.to_lowercase().replace('-', " ")), // forgiving input
        ssh_config_file: None,
        known_hosts_file: None,
        extra_ssh_options: vec![],
        fingerprint: Some(share.fingerprint.clone()),
        mode: MountMode::ReadWrite,
        ghost_shield: true,
        name: Some("Disco di Rete".into()),
        mount_root: Some(dir.join("mnt").to_string_lossy().into_owned()),
        volume_icon: None,
    };
    // Wrong pairing code is refused.
    let bad = RemoteMountRequest { pairing_code: Some("AAAA-BBBB".into()), ..rreq.clone() };
    assert!(runtime().mount_remote(bad).is_err());
    let client = runtime().mount_remote(rreq).unwrap();
    assert_eq!(format!("{:?}", client.kind), "Lan");
    let cmp = PathBuf::from(&client.mountpoint);
    assert_eq!(fs::read_to_string(cmp.join("leggimi.txt")).unwrap(), "file dal Mac che condivide");
    assert!(fs::read_dir(&cmp).unwrap().count() >= 2);
    fs::write(cmp.join("Foto/dal client.txt"), "ciao host").unwrap();
    fs::write(cmp.join(".DS_Store"), "junk").unwrap();
    wait_for("upload", Duration::from_secs(15), || status(&client.id).remote.map(|r| r.pending_ops == 0).unwrap_or(false));
    wait_for("host sees file", Duration::from_secs(5), || fs::read_to_string(hmp.join("Foto/dal client.txt")).map(|s| s == "ciao host").unwrap_or(false));
    let r = status(&client.id).remote.unwrap();
    assert!(r.online && r.latency_ms.is_some(), "{r:?}");

    // ── Network drops ──
    frozen.store(true, Ordering::SeqCst);
    wait_for("offline detection", Duration::from_secs(6), || !status(&client.id).remote.unwrap().online);
    std::thread::sleep(Duration::from_millis(1200)); // let the kernel's attribute cache expire (actimeo=1)
    let mut worst = Duration::ZERO;
    for _ in 0..20 {
        let t0 = Instant::now();
        let _ = fs::metadata(cmp.join("leggimi.txt")).unwrap();
        let n = fs::read_dir(&cmp).unwrap().count();
        assert!(n >= 2);
        let _ = fs::read_to_string(cmp.join("leggimi.txt")).unwrap(); // cached content
        worst = worst.max(t0.elapsed());
        std::thread::sleep(Duration::from_millis(60));
    }
    println!("offline: slowest stat + ls + cached read = {worst:?}");
    assert!(worst < Duration::from_millis(50), "Finder would beachball: {worst:?}");
    let t0 = Instant::now();
    fs::write(cmp.join("scritto offline.txt"), "in coda").unwrap();
    fs::create_dir(cmp.join("Cartella offline")).unwrap();
    let wt = t0.elapsed();
    println!("offline write + mkdir took {wt:?}");
    assert!(wt < Duration::from_millis(100), "offline write blocked: {wt:?}");
    let pending = status(&client.id).remote.unwrap().pending_ops;
    assert!(pending >= 2, "pending {pending}");
    assert!(!hmp.join("scritto offline.txt").exists());

    // ── Network back ──
    frozen.store(false, Ordering::SeqCst);
    wait_for("resync", Duration::from_secs(20), || status(&client.id).remote.map(|r| r.online && r.pending_ops == 0).unwrap_or(false));
    wait_for("host sees offline writes", Duration::from_secs(5), || {
        fs::read_to_string(hmp.join("scritto offline.txt")).map(|s| s == "in coda").unwrap_or(false) && hmp.join("Cartella offline").is_dir()
    });
    // Ghost Shield on the client: the junk never crossed the network.
    assert!(!hmp.join(".DS_Store").exists());
    runtime().unmount(&client.id, false).unwrap();
    runtime().share_stop(&host.id).unwrap();
    runtime().unmount(&host.id, false).unwrap();
}

// ───────────────────────── 6. SFTP mount (real OpenSSH) ─────────────────────────

#[test]
#[ignore]
fn e2e_sftp_mount_roundtrip() {
    let dir = setup("p2_sftp");
    let key = dir.join("id_ed25519");
    assert!(std::process::Command::new("ssh-keygen").args(["-t", "ed25519", "-N", "", "-q", "-f"]).arg(&key).status().unwrap().success());
    let pubkey = fs::read_to_string(dir.join("id_ed25519.pub")).unwrap();
    let name = format!("lumendisk-p2-sftp-{}", std::process::id());
    let boot = "mkdir -p /run/sshd && useradd -m -s /bin/sh pi && mkdir -p /home/pi/.ssh && echo \"$PUBKEY\" > /home/pi/.ssh/authorized_keys && \
        chown -R pi:pi /home/pi && chmod 700 /home/pi/.ssh && chmod 600 /home/pi/.ssh/authorized_keys && \
        mkdir -p /home/pi/progetti && echo 'dal server' > /home/pi/progetti/server.txt && chown -R pi:pi /home/pi/progetti && /usr/sbin/sshd -e && exec sleep infinity";
    let _ = std::process::Command::new("docker").args(["rm", "-f", &name]).output();
    let out = std::process::Command::new("docker")
        .args(["run", "-d", "--rm", "--name", &name, "-p", "127.0.0.1::22", "-e"])
        .arg(format!("PUBKEY={}", pubkey.trim()))
        .args(["lumendisk-toolbox", "sh", "-c", boot])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    struct Kill(String);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker").args(["rm", "-f", &self.0]).output();
        }
    }
    let _k = Kill(name.clone());
    let o = std::process::Command::new("docker").args(["port", &name, "22/tcp"]).output().unwrap();
    let port: u16 = String::from_utf8_lossy(&o.stdout).lines().next().and_then(|l| l.rsplit(':').next()).unwrap().trim().parse().unwrap();
    let rreq = RemoteMountRequest {
        kind: "sftp".into(),
        url: format!("sftp://pi@127.0.0.1:{port}/~/progetti"),
        password: None,
        identity_file: Some(key.to_string_lossy().into()),
        username: None,
        pairing_code: None,
        ssh_config_file: Some("/dev/null".into()),
        known_hosts_file: Some(dir.join("known_hosts").to_string_lossy().into()),
        extra_ssh_options: vec!["-oIdentityAgent=none".into()],
        fingerprint: None,
        mode: MountMode::ReadWrite,
        ghost_shield: true,
        name: Some("Raspberry".into()),
        mount_root: Some(dir.join("mnt").to_string_lossy().into_owned()),
        volume_icon: None,
    };
    let mut st = None;
    for _ in 0..40 {
        match runtime().mount_remote(rreq.clone()) {
            Ok(s) => {
                st = Some(s);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
    let st = st.expect("sftp mount");
    let mp = PathBuf::from(&st.mountpoint);
    assert_eq!(fs::read_to_string(mp.join("server.txt")).unwrap(), "dal server\n");
    let big: Vec<u8> = (0..2_000_000u32).map(|i| (i * 31 % 251) as u8).collect();
    fs::write(mp.join("dal mac.bin"), &big).unwrap();
    fs::create_dir(mp.join("sotto")).unwrap();
    std::os::unix::fs::symlink("../server.txt", mp.join("sotto/link")).unwrap();
    fs::write(mp.join("._dal mac.bin"), "appledouble").unwrap();
    wait_for("sftp upload", Duration::from_secs(20), || status(&st.id).remote.map(|r| r.pending_ops == 0).unwrap_or(false));
    runtime().unmount(&st.id, false).unwrap();
    let o = std::process::Command::new("docker")
        .args(["exec", &name, "sh", "-c", "cd /home/pi/progetti && sha256sum 'dal mac.bin' && readlink sotto/link && ls -a"])
        .output()
        .unwrap();
    let o = String::from_utf8_lossy(&o.stdout).to_string();
    use sha2::Digest;
    let want: String = sha2::Sha256::digest(&big).iter().map(|b| format!("{b:02x}")).collect();
    assert!(o.contains(&want), "{o}");
    assert!(o.contains("../server.txt"), "{o}");
    assert!(!o.contains("._dal"), "Ghost Shield must keep AppleDouble files off the server: {o}");
}

// ───────────────────────── 7. Wi-Fi portal + 8. Finder volume icon ─────────────────────────

#[test]
#[ignore]
fn e2e_portal_and_volume_icon() {
    let dir = setup("p2_portal_icon");
    let img = dir.join("disk.img");
    runtime().create_image(img.to_str().unwrap(), "EXT4", 32, "ICONA").unwrap();
    let icon = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../macos/Resources/AppIcon.icns");
    let mut r = req(&dir, "disk.img", MountMode::ReadWrite);
    r.volume_icon = Some(icon.to_string_lossy().into());
    let st = runtime().mount(r).unwrap();
    let mp = PathBuf::from(&st.mountpoint);
    // Finder sees a custom volume icon…
    assert_eq!(fs::read(mp.join(".VolumeIcon.icns")).unwrap(), fs::read(&icon).unwrap());
    // …and the portal answers with a PIN page.
    let p = runtime().portal_start(&st.id).unwrap();
    assert_eq!(p.pin.len(), 6);
    assert!(p.url.starts_with("https://") && p.http_url.is_none(), "HTTPS only by default: {p:?}");
    // The unencrypted address only exists after an explicit opt-in, and goes away again.
    let p2 = runtime().portal_set_plain_http(&st.id, true).unwrap();
    assert!(p2.http_url.as_deref().is_some_and(|u| u.starts_with("http://")), "{p2:?}");
    assert!(runtime().portal_set_plain_http(&st.id, false).unwrap().http_url.is_none());
    let local = format!("https://127.0.0.1:{}/", p.port);
    let page = lumendisk_core::lan::pinned_agent(&p.fingerprint).get(&local).call().unwrap().into_string().unwrap();
    assert!(page.to_lowercase().contains("pin"), "{page}");
    assert!(status(&st.id).portal.is_some());
    runtime().portal_stop(&st.id).unwrap();
    runtime().unmount(&st.id, false).unwrap();
    // …but nothing was written to the disk.
    let (c, o) = docker(&dir, "debugfs -R 'ls -a /' disk.img 2>/dev/null");
    assert_eq!(c, 0);
    assert!(!o.contains("VolumeIcon"), "{o}");
}
