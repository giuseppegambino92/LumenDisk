//! SFTP backend vs. a real OpenSSH server (sshd in the `lumendisk-toolbox` image).
//! Run with `cargo test -p lumendisk-core --test sftp_real -- --ignored --nocapture`.
//! Each test starts its own container on a random loopback port and removes it on drop,
//! also when the test panics. Nothing touches ~/.ssh: `-F /dev/null`, a throwaway key pair
//! and a private known_hosts file in the test's work dir.
mod common;
use common::*;
use lumendisk_core::block::{BlockDev, CachedRangeDev, RangeSource};
use lumendisk_core::remote::sftp::{SftpBackend, SftpConfig, SftpRangeSource};
use lumendisk_core::remote::{RemoteBackend, RemoteError};
use lumendisk_core::vfs::{FileKind, Timestamp};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const BOOT: &str = "mkdir -p /run/sshd && useradd -m -s /bin/bash pi && echo pi:raspberry | chpasswd && \
    install -d -m 700 -o pi -g pi /home/pi/.ssh && printf '%s\\n' \"$PUBKEY\" > /home/pi/.ssh/authorized_keys && \
    chown pi:pi /home/pi/.ssh/authorized_keys && chmod 600 /home/pi/.ssh/authorized_keys && \
    /usr/sbin/sshd -e && exec sleep infinity";

struct Server {
    name: String,
    port: u16,
    dir: PathBuf,
}

fn sh(cmd: &mut Command) -> (i32, String) {
    let out = cmd.output().expect("command failed to start");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

impl Server {
    fn start(tag: &str) -> Server {
        let dir = workdir(&format!("sftp_{tag}"));
        let key = dir.join("id_ed25519");
        let (c, o) = sh(Command::new("ssh-keygen").args(["-t", "ed25519", "-N", "", "-q", "-f"]).arg(&key));
        assert_eq!(c, 0, "ssh-keygen: {o}");
        let pubkey = std::fs::read_to_string(dir.join("id_ed25519.pub")).unwrap();
        let name = format!("lumendisk-sftp-{tag}-{}", std::process::id());
        let _ = sh(Command::new("docker").args(["rm", "-f", &name]));
        let (c, o) = sh(Command::new("docker")
            .args(["run", "-d", "--rm", "--name", &name, "-p", "127.0.0.1::22", "-e"])
            .arg(format!("PUBKEY={}", pubkey.trim()))
            .args(["lumendisk-toolbox", "sh", "-c", BOOT]));
        assert_eq!(c, 0, "docker run: {o}");
        let mut srv = Server { name, port: 0, dir };
        let (_, o) = sh(Command::new("docker").args(["port", &srv.name, "22/tcp"]));
        srv.port = o.lines().next().and_then(|l| l.rsplit(':').next()).and_then(|p| p.trim().parse().ok()).expect(&o);
        // Wait for sshd.
        let t0 = Instant::now();
        loop {
            match SftpBackend::connect(srv.cfg(".")) {
                Ok(_) => break,
                Err(e) if t0.elapsed() < Duration::from_secs(20) => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) => panic!("sshd did not come up: {e}"),
            }
        }
        srv
    }

    fn base(&self, root: &str) -> SftpConfig {
        SftpConfig {
            host: "127.0.0.1".into(),
            port: self.port,
            user: Some("pi".into()),
            root: root.into(),
            ssh_config_file: Some("/dev/null".into()),
            known_hosts_file: Some(self.dir.join("known_hosts").to_string_lossy().into_owned()),
            ..Default::default()
        }
    }

    /// Key auth with the throwaway key only (no agent).
    fn cfg(&self, root: &str) -> SftpConfig {
        SftpConfig {
            identity_file: Some(self.dir.join("id_ed25519").to_string_lossy().into_owned()),
            extra_ssh_options: vec!["-oIdentityAgent=none".into()],
            ..self.base(root)
        }
    }

    /// Password auth only.
    fn pw_cfg(&self, root: &str, pw: &str) -> SftpConfig {
        SftpConfig {
            password: Some(pw.into()),
            extra_ssh_options: vec!["-oIdentityAgent=none".into(), "-oPubkeyAuthentication=no".into()],
            ..self.base(root)
        }
    }

    fn exec(&self, script: &str) -> String {
        let (c, o) = sh(Command::new("docker").args(["exec", &self.name, "sh", "-c", script]));
        assert_eq!(c, 0, "docker exec {script}: {o}");
        o
    }

    fn docker(&self, verb: &str) {
        let (c, o) = sh(Command::new("docker").args([verb, &self.name]));
        assert_eq!(c, 0, "docker {verb}: {o}");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = sh(Command::new("docker").args(["unpause", &self.name]));
        let _ = sh(Command::new("docker").args(["rm", "-f", &self.name]));
    }
}

/// Deterministic pseudo-random bytes (xorshift).
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn hex(d: &[u8]) -> String {
    Sha256::digest(d).iter().map(|b| format!("{b:02x}")).collect()
}

fn names(v: &[lumendisk_core::remote::RemoteEntry]) -> Vec<String> {
    let mut n: Vec<String> = v.iter().map(|e| e.name.clone()).collect();
    n.sort();
    n
}

#[test]
#[ignore]
fn key_auth_file_operations() {
    let srv = Server::start("ops");
    let b = SftpBackend::connect(srv.cfg(".")).expect("key auth connect");
    assert_eq!(b.remote_root(), "/home/pi");
    assert_eq!(b.describe(), format!("sftp://pi@127.0.0.1:{}/home/pi", srv.port));
    println!("ping: {:?}", b.ping().unwrap());

    let root = b.stat("/").unwrap();
    assert_eq!((root.kind, root.name.as_str()), (FileKind::Dir, ""));
    assert_eq!(b.stat("/nope").unwrap_err(), RemoteError::NotFound);
    assert_eq!(b.stat("/../etc/passwd").unwrap_err(), RemoteError::Perm, "'..' must be refused");

    // mkdir / create
    b.mkdir("/d", 0o750).unwrap();
    let d = b.stat("/d").unwrap();
    assert_eq!((d.kind, d.perm, d.name.as_str()), (FileKind::Dir, 0o750, "d"));
    assert_eq!(b.mkdir("/d", 0o755).unwrap_err(), RemoteError::Exists);
    b.create("/d/f.txt", 0o664).unwrap();
    let f = b.stat("/d/f.txt").unwrap();
    assert_eq!((f.kind, f.perm, f.size), (FileKind::File, 0o664, 0));
    assert_eq!(b.create("/d/f.txt", 0o644).unwrap_err(), RemoteError::Exists);
    assert_eq!(b.create("/missing/f", 0o644).unwrap_err(), RemoteError::NotFound);

    // put_all / read / partial write / truncate
    b.put_all("/d/a.txt", b"hello world").unwrap();
    assert_eq!(b.read("/d/a.txt", 0, 100).unwrap(), b"hello world");
    assert_eq!(b.read("/d/a.txt", 6, 5).unwrap(), b"world");
    assert_eq!(b.read("/d/a.txt", 50, 5).unwrap(), b"");
    b.write("/d/a.txt", 6, b"WORLD!").unwrap();
    assert_eq!(b.read("/d/a.txt", 0, 100).unwrap(), b"hello WORLD!");
    b.write("/d/a.txt", 20, b"x").unwrap();
    assert_eq!(b.stat("/d/a.txt").unwrap().size, 21);
    assert_eq!(b.read("/d/a.txt", 12, 9).unwrap(), b"\0\0\0\0\0\0\0\0x");
    assert_eq!(srv.exec("od -An -c /home/pi/d/a.txt | head -1").split_whitespace().take(5).collect::<String>(), "hello");
    assert_eq!(b.write("/d/none.txt", 0, b"x").unwrap_err(), RemoteError::NotFound);
    b.truncate("/d/a.txt", 5).unwrap();
    assert_eq!(b.read("/d/a.txt", 0, 100).unwrap(), b"hello");
    b.truncate("/d/a.txt", 10).unwrap();
    assert_eq!(b.read("/d/a.txt", 0, 100).unwrap(), b"hello\0\0\0\0\0");
    b.put_all("/d/a.txt", b"short").unwrap();
    assert_eq!(b.read("/d/a.txt", 0, 100).unwrap(), b"short", "put_all truncates");
    assert!(b.supports_partial_write());

    // listing & error mapping
    assert_eq!(names(&b.list("/d").unwrap()), vec!["a.txt", "f.txt"]);
    assert_eq!(b.list("/d/a.txt").unwrap_err(), RemoteError::NotDir);
    assert_eq!(b.list("/zz").unwrap_err(), RemoteError::NotFound);
    assert_eq!(b.read("/d", 0, 10).unwrap_err(), RemoteError::IsDir);
    assert_eq!(b.rmdir("/d").unwrap_err(), RemoteError::NotEmpty);
    assert_eq!(b.rmdir("/d/a.txt").unwrap_err(), RemoteError::NotDir);
    assert_eq!(b.remove("/d").unwrap_err(), RemoteError::IsDir);
    assert_eq!(b.remove("/d/zz").unwrap_err(), RemoteError::NotFound);

    // rename (incl. replacing an existing file), with posix-rename and with the v3 fallback
    b.rename("/d/a.txt", "/d/b.txt").unwrap();
    assert_eq!(b.stat("/d/a.txt").unwrap_err(), RemoteError::NotFound);
    b.put_all("/d/c.txt", b"new content").unwrap();
    b.rename("/d/c.txt", "/d/b.txt").unwrap();
    assert_eq!(b.read("/d/b.txt", 0, 100).unwrap(), b"new content");
    assert_eq!(srv.exec("cat /home/pi/d/b.txt"), "new content");
    let plain = SftpBackend::connect(SftpConfig { use_extensions: false, ..srv.cfg(".") }).unwrap();
    plain.put_all("/d/c.txt", b"via plain v3").unwrap();
    plain.rename("/d/c.txt", "/d/b.txt").unwrap();
    assert_eq!(b.read("/d/b.txt", 0, 100).unwrap(), b"via plain v3");
    assert_eq!(plain.statfs().unwrap(), None);
    b.mkdir("/d/sub", 0o755).unwrap();
    assert_eq!(b.rename("/d/b.txt", "/d/sub").unwrap_err(), RemoteError::IsDir);
    assert_eq!(b.rename("/d/nope", "/d/x").unwrap_err(), RemoteError::NotFound);

    // symlinks
    b.symlink("/d/link", "b.txt").unwrap();
    let l = b.stat("/d/link").unwrap();
    assert_eq!(l.kind, FileKind::Symlink);
    assert_eq!(b.readlink("/d/link").unwrap(), "b.txt");
    assert_eq!(srv.exec("readlink /home/pi/d/link").trim(), "b.txt");
    assert_eq!(b.read("/d/link", 0, 100).unwrap(), b"via plain v3", "reads follow symlinks");
    assert_eq!(b.symlink("/d/link", "x").unwrap_err(), RemoteError::Exists);
    assert!(b.readlink("/d/b.txt").is_err());

    // attributes
    b.set_perm("/d/b.txt", 0o600).unwrap();
    assert_eq!(b.stat("/d/b.txt").unwrap().perm, 0o600);
    assert_eq!(srv.exec("stat -c %a /home/pi/d/b.txt").trim(), "600");
    b.set_mtime("/d/b.txt", Timestamp { secs: 1_000_000_000, nsecs: 0 }).unwrap();
    assert_eq!(b.stat("/d/b.txt").unwrap().mtime.secs, 1_000_000_000);
    assert_eq!(srv.exec("stat -c %Y /home/pi/d/b.txt").trim(), "1000000000");
    let (total, free) = b.statfs().unwrap().expect("statvfs@openssh.com");
    assert!(total > 0 && free <= total, "{total} {free}");

    // unicode names
    let ud = "/Ünïcødé 📁";
    b.mkdir(ud, 0o755).unwrap();
    b.put_all(&format!("{ud}/日本語 ファイル.txt"), "ciao 🌍".as_bytes()).unwrap();
    assert_eq!(names(&b.list(ud).unwrap()), vec!["日本語 ファイル.txt"]);
    assert!(names(&b.list("/").unwrap()).contains(&"Ünïcødé 📁".to_string()));
    assert_eq!(srv.exec("cat '/home/pi/Ünïcødé 📁/日本語 ファイル.txt'"), "ciao 🌍");

    // 5 MB file with hash check (pipelined transfers)
    let big = noise(5 * 1024 * 1024 + 123, 42);
    let t0 = Instant::now();
    b.put_all("/big.bin", &big).unwrap();
    let up = t0.elapsed();
    let t0 = Instant::now();
    let mut back = Vec::new();
    let mut off = 0u64;
    loop {
        let chunk = b.read("/big.bin", off, 1 << 20).unwrap();
        if chunk.is_empty() {
            break;
        }
        off += chunk.len() as u64;
        back.extend_from_slice(&chunk);
    }
    let down = t0.elapsed();
    assert_eq!(back.len(), big.len());
    assert_eq!(hex(&back), hex(&big));
    assert_eq!(srv.exec("sha256sum /home/pi/big.bin").split_whitespace().next().unwrap(), hex(&big));
    let t0 = Instant::now();
    assert_eq!(b.read("/big.bin", 1 << 20, 1 << 20).unwrap(), &big[1 << 20..2 << 20]);
    println!("5 MB: upload {up:?}, download (1 MiB reads) {down:?}; one 1 MiB read {:?}", t0.elapsed());
    // partial write in the middle of the big file
    b.write("/big.bin", 3_000_001, b"PATCH").unwrap();
    assert_eq!(b.read("/big.bin", 3_000_000, 7).unwrap(), [&big[3_000_000..3_000_001], b"PATCH", &big[3_000_006..3_000_007]].concat());

    // cleanup through the backend
    b.remove("/d/link").unwrap();
    b.remove("/d/b.txt").unwrap();
    b.remove("/d/f.txt").unwrap();
    b.rmdir("/d/sub").unwrap();
    b.rmdir("/d").unwrap();
    assert_eq!(b.stat("/d").unwrap_err(), RemoteError::NotFound);

    // an export root below home
    b.mkdir("/exp", 0o755).unwrap();
    b.put_all("/exp/inside.txt", b"1").unwrap();
    let sub = SftpBackend::connect(srv.cfg("exp")).unwrap();
    assert_eq!(sub.remote_root(), "/home/pi/exp");
    assert_eq!(names(&sub.list("/").unwrap()), vec!["inside.txt"]);
    assert_eq!(sub.stat("/../big.bin").unwrap_err(), RemoteError::Perm);
    assert_eq!(SftpBackend::connect(srv.cfg("/nonexistent")).err().unwrap(), RemoteError::NotFound);
    assert_eq!(SftpBackend::connect(srv.cfg("big.bin")).err().unwrap(), RemoteError::NotDir);
}

#[test]
#[ignore]
fn password_auth_and_auth_errors() {
    let srv = Server::start("auth");
    let b = SftpBackend::connect(srv.pw_cfg(".", "raspberry")).expect("password auth");
    b.put_all("/pw.txt", b"password works").unwrap();
    assert_eq!(b.read("/pw.txt", 0, 64).unwrap(), b"password works");
    assert_eq!(srv.exec("cat /home/pi/pw.txt"), "password works");

    let url = format!("sftp://pi:raspberry@127.0.0.1:{}/home/pi", srv.port);
    let cfg = SftpConfig { known_hosts_file: srv.pw_cfg(".", "").known_hosts_file, ssh_config_file: Some("/dev/null".into()), extra_ssh_options: srv.pw_cfg(".", "").extra_ssh_options, ..SftpConfig::from_url(&url).unwrap() };
    let u = SftpBackend::connect(cfg).expect("password from URL");
    assert_eq!(names(&u.list("/").unwrap()).contains(&"pw.txt".to_string()), true);

    let t0 = Instant::now();
    let e = SftpBackend::connect(srv.pw_cfg(".", "wrong")).err().expect("wrong password must fail");
    println!("wrong password ({:?}): {e}", t0.elapsed());
    assert!(matches!(&e, RemoteError::Other(m) if m.contains("authentication failed")), "{e}");

    // key not authorized
    let other = srv.dir.join("other_key");
    sh(Command::new("ssh-keygen").args(["-t", "ed25519", "-N", "", "-q", "-f"]).arg(&other));
    let e = SftpBackend::connect(SftpConfig { identity_file: Some(other.to_string_lossy().into()), ..srv.cfg(".") }).err().unwrap();
    println!("unauthorized key: {e}");
    assert!(matches!(&e, RemoteError::Other(m) if m.contains("authentication failed")), "{e}");

    // changed host key
    let kh = srv.dir.join("bad_known_hosts");
    let fake = std::fs::read_to_string(other.with_extension("pub")).unwrap();
    std::fs::write(&kh, format!("[127.0.0.1]:{} {}\n", srv.port, fake.split_whitespace().take(2).collect::<Vec<_>>().join(" "))).unwrap();
    let mut cfg = srv.cfg(".");
    cfg.known_hosts_file = Some(kh.to_string_lossy().into());
    cfg.extra_ssh_options.push("-oHostKeyAlgorithms=ssh-ed25519".into());
    let e = SftpBackend::connect(cfg).err().unwrap();
    println!("changed host key: {e}");
    assert!(matches!(&e, RemoteError::Other(m) if m.contains("host key verification failed")), "{e}");

    // nobody listening
    let e = SftpBackend::connect(SftpConfig { port: 1, ..srv.cfg(".") }).err().unwrap();
    println!("closed port: {e}");
    assert!(matches!(&e, RemoteError::Disconnected(m) if m.contains("refused")), "{e}");

    // the password never shows up on the ssh command line
    let (_, ps) = sh(Command::new("ps").args(["-axo", "args"]));
    assert!(!ps.contains("raspberry"));
}

fn retry_until_ok(b: &SftpBackend, max: Duration) -> (usize, Duration) {
    let t0 = Instant::now();
    let mut n = 0;
    loop {
        n += 1;
        match b.stat("/") {
            Ok(_) => return (n, t0.elapsed()),
            Err(e) => {
                assert!(e.is_network(), "unexpected {e}");
                assert!(t0.elapsed() < max, "no recovery after {n} attempts: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

#[test]
#[ignore]
fn timeout_on_pause_and_reconnect() {
    let srv = Server::start("net");
    let b = SftpBackend::connect(srv.cfg(".")).unwrap();
    b.put_all("/file.bin", &noise(3 << 20, 7)).unwrap();
    let tmo = Duration::from_millis(400);
    b.set_timeout(tmo);
    b.stat("/").unwrap();

    // 1. Frozen server: calls return Timeout after ~400 ms and the session survives.
    srv.docker("pause");
    for (what, f) in [
        ("stat", Box::new(|| b.stat("/").map(|_| ())) as Box<dyn Fn() -> Result<(), RemoteError>>),
        ("read 1 MiB", Box::new(|| b.read("/file.bin", 0, 1 << 20).map(|_| ()))),
    ] {
        let t0 = Instant::now();
        let r = f();
        let el = t0.elapsed();
        println!("paused {what}: {r:?} after {el:?}");
        assert_eq!(r, Err(RemoteError::Timeout));
        assert!(el >= Duration::from_millis(390) && el < Duration::from_millis(1000), "{what} took {el:?}");
    }
    srv.docker("unpause");
    let t0 = Instant::now();
    let (n, el) = retry_until_ok(&b, Duration::from_secs(5));
    println!("after unpause: ok on attempt {n} after {el:?}");
    assert_eq!(n, 1, "the session must survive a single timeout");
    assert!(t0.elapsed() < tmo);
    assert_eq!(b.read("/file.bin", 0, 1 << 20).unwrap(), &noise(3 << 20, 7)[..1 << 20]);

    // 2. Long freeze: after 3 silent timeouts the session is dropped; every call still
    //    returns within ~timeout, and the backend reconnects once the server thaws.
    srv.docker("pause");
    for i in 0..6 {
        let t0 = Instant::now();
        let r = b.stat("/");
        let el = t0.elapsed();
        println!("long pause call {i}: {r:?} after {el:?}");
        assert!(matches!(r, Err(ref e) if e.is_network()), "{r:?}");
        assert!(el < Duration::from_millis(1000), "call {i} took {el:?}");
    }
    srv.docker("unpause");
    let (n, el) = retry_until_ok(&b, Duration::from_secs(20));
    println!("after long pause: ok on attempt {n} after {el:?}");

    // 3. sshd killed: Disconnected, then automatic reconnect once sshd is back.
    srv.exec("pkill sshd || true");
    std::thread::sleep(Duration::from_millis(300));
    let t0 = Instant::now();
    let r = b.stat("/");
    println!("sshd killed: {r:?} after {:?}", t0.elapsed());
    assert!(matches!(r, Err(RemoteError::Disconnected(_))), "{r:?}");
    let r = b.stat("/");
    println!("sshd still down: {r:?}");
    assert!(matches!(r, Err(RemoteError::Disconnected(_))), "{r:?}");
    srv.exec("/usr/sbin/sshd -e");
    let (n, el) = retry_until_ok(&b, Duration::from_secs(10));
    println!("sshd restarted: ok on attempt {n} after {el:?}");
    assert_eq!(b.read("/file.bin", 5, 5).unwrap(), &noise(3 << 20, 7)[5..10]);
    println!("ping after reconnect: {:?}", b.ping().unwrap());

    // 4. Only the session dies (sshd-session killed): the in-flight / next call reports
    //    Disconnected and the one after that transparently reconnects.
    srv.exec("pkill sshd-session || true");
    std::thread::sleep(Duration::from_millis(300));
    let r = b.stat("/");
    println!("session killed: {r:?}");
    assert!(r.is_ok() || matches!(r, Err(RemoteError::Disconnected(_))), "{r:?}");
    b.stat("/").expect("reconnected");
}

#[test]
#[ignore]
fn range_source_matches_local_copy() {
    let srv = Server::start("range");
    let size = 20 * 1024 * 1024usize;
    srv.exec(&format!("head -c {size} /dev/urandom > /home/pi/disk.img && chown pi /home/pi/disk.img"));
    let local = srv.dir.join("disk.img");
    let (c, o) = sh(Command::new("docker").arg("cp").arg(format!("{}:/home/pi/disk.img", srv.name)).arg(&local));
    assert_eq!(c, 0, "{o}");
    let want = std::fs::read(&local).unwrap();
    assert_eq!(want.len(), size);

    let src = SftpRangeSource::open(srv.cfg("disk.img")).expect("open range source");
    assert_eq!(src.size(), size as u64);
    assert_eq!(src.describe(), format!("sftp://pi@127.0.0.1:{}/home/pi/disk.img", srv.port));
    let abs = SftpRangeSource::open(srv.cfg("/home/pi/disk.img")).unwrap();
    assert_eq!(abs.size(), size as u64);

    let mut ranges: Vec<(u64, usize)> = vec![
        (0, 512),
        (0, 4096),
        (12345, 100_000),
        (size as u64 - 1000, 1000),
        (size as u64 - 1, 1),
        (5 << 20, 3 << 20),
        (1 << 20, 32 * 1024),
        (7, 32 * 1024 + 1),
    ];
    let rnd = noise(128, 99);
    for i in 0..16 {
        let off = u64::from_le_bytes(rnd[i * 4..i * 4 + 8].try_into().unwrap_or([0; 8])) % (size as u64 - 300_000);
        ranges.push((off, 1 + (rnd[i] as usize * 1171) % 300_000));
    }
    let t0 = Instant::now();
    for &(off, len) in &ranges {
        let got = src.fetch(off, len).unwrap();
        assert!(got == want[off as usize..off as usize + len], "mismatch at {off}+{len}");
    }
    println!("{} random ranges in {:?}", ranges.len(), t0.elapsed());
    let t0 = Instant::now();
    let whole = src.fetch(0, size).unwrap();
    println!("whole 20 MB in one fetch: {:?}", t0.elapsed());
    assert_eq!(hex(&whole), hex(&want));

    // Through the block cache, concurrently.
    let dev = std::sync::Arc::new(CachedRangeDev::new(Box::new(abs), 16));
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let dev = dev.clone();
            let want = want.clone();
            std::thread::spawn(move || {
                for i in 0..20u64 {
                    let off = (t * 5_000_003 + i * 97_001) % (size as u64 - 70_000);
                    let mut buf = vec![0u8; 65_000];
                    dev.read_at(off, &mut buf).unwrap();
                    assert!(buf == want[off as usize..off as usize + 65_000]);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }

    // The source survives a dropped session (one transparent reconnect inside fetch).
    srv.exec("pkill sshd-session || true");
    std::thread::sleep(Duration::from_millis(300));
    let got = src.fetch(9_999_999, 4321).expect("fetch after session loss");
    assert!(got == want[9_999_999..9_999_999 + 4321]);
    assert!(SftpRangeSource::open(srv.cfg("missing.img")).is_err());
}
