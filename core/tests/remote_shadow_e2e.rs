//! Persistent Shadow sessions on remote servers, end to end: the real `lumendisk` CLI serves a
//! real `mount_nfs` mount of an SFTP server (OpenSSH in Docker), a WebDAV server (Apache
//! mod_dav in Docker) and another Mac's LAN share (a real `LanServer`, TLS + pairing code), is
//! killed with SIGKILL, and is mounted again.
//!
//! For each server:
//! 1. Shadow mount; create, modify, rename and delete through the NFS mount; SIGKILL. The
//!    session is listed and on disk, the server is untouched. Mount again: the changes are
//!    back; Commit; the server has them as soon as the commit returns.
//! 2. Change a file the session touched, SIGKILL, change that file on the server: the next
//!    Shadow mount is refused as drift (like a local disk modified elsewhere), the session is
//!    listed as discard-only, and Discard works.
//! 3. The same while mounted: the commit is refused and the session can only be discarded.
//!
//!   cargo test -p lumendisk-core --release --test remote_shadow_e2e -- --ignored --test-threads=1 --nocapture
mod common;
use common::*;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lumendisk_core::block::{BlockDev, MemDev};
use lumendisk_core::lan::{new_pairing_code, token_from_pairing_code, LanMode, LanServer};
use lumendisk_core::vfs::ext4_fs::{self, Ext4Fs};
use lumendisk_core::vfs::{FileSystem, FsType, Ino};
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_lumendisk");

// ───────────────────────── CLI harness (as in shadow_persist_e2e) ─────────────────────────

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

/// `lumendisk mount-remote <request.json>` in Shadow mode; the request (with its credentials)
/// goes through a file, never the command line.
fn mount(dir: &Path, req: &Value) -> Result<Mounted, String> {
    let mp = dir.join("mnt").join("Vol");
    let stamp = format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    let rfile = dir.join(format!("req-{stamp}.json"));
    let mut req = req.clone();
    req["mode"] = json!("shadow");
    req["name"] = json!("Vol");
    req["mount_root"] = json!(dir.join("mnt").to_string_lossy());
    std::fs::write(&rfile, serde_json::to_vec(&req).unwrap()).unwrap();
    let logp = dir.join(format!("mount-{stamp}.log"));
    let log = std::fs::File::create(&logp).unwrap();
    let _ = std::fs::remove_file(dir.join("c.sock"));
    let mut child = cli(dir)
        .arg("mount-remote")
        .arg(&rfile)
        .args(["--control", "c.sock"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            let _ = std::fs::remove_file(&rfile);
            return Err(std::fs::read_to_string(&logp).unwrap_or_default());
        }
        if is_mounted(&mp) && dir.join("c.sock").exists() {
            let _ = std::fs::remove_file(&rfile);
            return Ok(Mounted { child: Some(child), mp });
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "mount timed out: {}", std::fs::read_to_string(&logp).unwrap_or_default());
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn ctl_raw(dir: &Path, cmd: &str) -> Value {
    let out = cli(dir).args(["ctl", "c.sock", cmd]).output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| panic!("ctl {cmd}: {}", String::from_utf8_lossy(&out.stderr)))
}

fn ctl(dir: &Path, cmd: &str) -> Value {
    let v = ctl_raw(dir, cmd);
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

fn sessions(dir: &Path) -> Vec<Value> {
    let out = cli(dir).arg("shadow-sessions").output().unwrap();
    assert!(out.status.success());
    serde_json::from_slice::<Vec<Value>>(&out.stdout).unwrap()
}

fn discard(dir: &Path, key: &str) -> Value {
    let out = cli(dir).args(["shadow-discard", key]).output().unwrap();
    assert!(out.status.success(), "shadow-discard: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Let the NFS client push everything and the journal group-commit.
fn settle() {
    std::thread::sleep(Duration::from_millis(1500));
}

// ───────────────────────── servers ─────────────────────────

trait Server {
    /// RemoteMountRequest (without mode / name / mount root).
    fn request(&self) -> Value;
    /// Content of a file as the server holds it (path relative to the export).
    fn read(&self, path: &str) -> Option<Vec<u8>>;
    /// Change a file directly on the server, behind LumenDisk's back.
    fn write(&self, path: &str, data: &[u8]);
    /// The sharing side restarts (LAN: new port, pairing code and certificate).
    fn restart(&mut self) {}
    fn kind(&self) -> &'static str;
    /// The session name `shadow-sessions` must show.
    fn display(&self) -> String;
}

fn sh(cmd: &mut Command) -> (i32, String) {
    let out = cmd.output().expect("command failed to start");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

/// Container removed on drop, also when the test panics.
struct Container(String);
impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

fn start_container(name: &str, port: u16, env: &[String], boot: &str) -> (Container, u16) {
    let _ = Command::new("docker").args(["rm", "-f", name]).output();
    let mut c = Command::new("docker");
    c.args(["run", "-d", "--rm", "--name", name, "-p", &format!("127.0.0.1::{port}")]);
    for e in env {
        c.arg("-e").arg(e);
    }
    let (code, o) = sh(c.args(["lumendisk-toolbox", "sh", "-c", boot]));
    assert_eq!(code, 0, "docker run: {o}");
    let guard = Container(name.to_string());
    let (_, o) = sh(Command::new("docker").args(["port", name, &format!("{port}/tcp")]));
    let host_port = o.lines().next().and_then(|l| l.rsplit(':').next()).and_then(|p| p.trim().parse().ok()).expect(&o);
    (guard, host_port)
}

fn docker_read(name: &str, path: &str) -> Option<Vec<u8>> {
    let out = Command::new("docker").args(["exec", name, "cat", path]).output().unwrap();
    out.status.success().then_some(out.stdout)
}

fn docker_write(name: &str, path: &str, data: &[u8], owner: &str) {
    use std::io::Write;
    let mut c = Command::new("docker")
        .args(["exec", "-i", name, "sh", "-c", &format!("cat > '{path}' && chown {owner} '{path}'")])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(data).unwrap();
    assert!(c.wait().unwrap().success(), "docker write {path}");
}

const CONTENT: &str = "mkdir -p $ROOT/docs && printf 'alpha\\n' > $ROOT/docs/a.txt && printf 'bravo\\n' > $ROOT/docs/b.txt && \
    printf 'log\\n' > $ROOT/old.log && printf 'keep\\n' > $ROOT/keep.txt";

struct Sftp {
    name: String,
    port: u16,
    dir: PathBuf,
    _c: Container,
}

impl Sftp {
    fn start(dir: &Path) -> Sftp {
        let key = dir.join("id_ed25519");
        let (c, o) = sh(Command::new("ssh-keygen").args(["-t", "ed25519", "-N", "", "-q", "-f"]).arg(&key));
        assert_eq!(c, 0, "ssh-keygen: {o}");
        let pubkey = std::fs::read_to_string(dir.join("id_ed25519.pub")).unwrap();
        let boot = format!(
            "mkdir -p /run/sshd && useradd -m -s /bin/sh pi && mkdir -p /home/pi/.ssh && echo \"$PUBKEY\" > /home/pi/.ssh/authorized_keys && \
             chmod 700 /home/pi/.ssh && chmod 600 /home/pi/.ssh/authorized_keys && ROOT=/home/pi/export && {CONTENT} && \
             chown -R pi:pi /home/pi && /usr/sbin/sshd -e && exec sleep infinity"
        );
        let name = format!("lumendisk-rshadow-sftp-{}", std::process::id());
        let (guard, port) = start_container(&name, 22, &[format!("PUBKEY={}", pubkey.trim())], &boot);
        let s = Sftp { name, port, dir: dir.to_path_buf(), _c: guard };
        let banner = || -> bool {
            use std::io::Read;
            let Ok(mut c) = std::net::TcpStream::connect(("127.0.0.1", s.port)) else { return false };
            let _ = c.set_read_timeout(Some(Duration::from_secs(1)));
            let mut b = [0u8; 4];
            c.read_exact(&mut b).is_ok() && &b == b"SSH-"
        };
        let t0 = Instant::now();
        while !banner() {
            assert!(t0.elapsed() < Duration::from_secs(30), "sshd did not come up");
            std::thread::sleep(Duration::from_millis(200));
        }
        s
    }
}

impl Server for Sftp {
    fn request(&self) -> Value {
        json!({
            "kind": "sftp",
            "url": format!("sftp://pi@127.0.0.1:{}/~/export", self.port),
            "identity_file": self.dir.join("id_ed25519"),
            "ssh_config_file": "/dev/null",
            "known_hosts_file": self.dir.join("known_hosts"),
            "extra_ssh_options": ["-oIdentityAgent=none"],
        })
    }
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        docker_read(&self.name, &format!("/home/pi/export/{path}"))
    }
    fn write(&self, path: &str, data: &[u8]) {
        docker_write(&self.name, &format!("/home/pi/export/{path}"), data, "pi:pi")
    }
    fn kind(&self) -> &'static str {
        "sftp"
    }
    fn display(&self) -> String {
        format!("pi@127.0.0.1:/home/pi/export (port {})", self.port)
    }
}

const APACHE_BOOT: &str = r#"set -e
a2enmod -q dav dav_fs dav_lock auth_basic >/dev/null
ROOT=/srv/dav/export
CONTENT
mkdir -p /var/lib/dav && chown -R www-data:www-data /srv/dav /var/lib/dav
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

struct Dav {
    name: String,
    port: u16,
    _c: Container,
}

impl Dav {
    fn start() -> Dav {
        let name = format!("lumendisk-rshadow-dav-{}", std::process::id());
        let (guard, port) = start_container(&name, 80, &[], &APACHE_BOOT.replace("CONTENT", CONTENT));
        let s = Dav { name, port, _c: guard };
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(5)).build();
        let t0 = Instant::now();
        loop {
            use base64::Engine;
            let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("pi:raspberry"));
            let ok = agent
                .request("PROPFIND", &format!("http://127.0.0.1:{}/dav/export/", s.port))
                .set("Authorization", &auth)
                .set("Depth", "0")
                .call()
                .map(|r| r.status() == 207)
                .unwrap_or(false);
            if ok {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(30), "apache did not come up");
            std::thread::sleep(Duration::from_millis(200));
        }
        s
    }
}

impl Server for Dav {
    fn request(&self) -> Value {
        json!({ "kind": "webdav", "url": format!("http://127.0.0.1:{}/dav/export", self.port), "username": "pi", "password": "raspberry" })
    }
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        docker_read(&self.name, &format!("/srv/dav/export/{path}"))
    }
    fn write(&self, path: &str, data: &[u8]) {
        docker_write(&self.name, &format!("/srv/dav/export/{path}"), data, "www-data:www-data")
    }
    fn kind(&self) -> &'static str {
        "webdav"
    }
    fn display(&self) -> String {
        format!("http://127.0.0.1:{}/dav/export", self.port)
    }
}

type Vol = Arc<Mutex<Ext4Fs>>;

/// "Another Mac": a LumenDisk LAN share of an ext4 volume, served from this process.
struct Lan {
    vol: Vol,
    srv: Option<LanServer>,
    code: String,
}

const SHARE_NAME: &str = "Disco di Anna";

impl Lan {
    fn start() -> Lan {
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(64 << 20));
        ext4_fs::format(dev.clone(), FsType::Ext4, "ANNA").unwrap();
        let vol: Vol = Arc::new(Mutex::new(Ext4Fs::open(dev, false).unwrap()));
        {
            let mut fs = vol.lock().unwrap();
            let r = fs.root_ino();
            let docs = fs.mkdir(r, "docs", 0o755, 501, 20).unwrap();
            for (p, n, d) in [(docs, "a.txt", "alpha\n"), (docs, "b.txt", "bravo\n"), (r, "old.log", "log\n"), (r, "keep.txt", "keep\n")] {
                let i = fs.create(p, n, 0o644, 501, 20).unwrap();
                fs.write(i, 0, d.as_bytes()).unwrap();
            }
        }
        let mut l = Lan { vol, srv: None, code: String::new() };
        l.restart();
        l
    }

    fn lookup(&self, path: &str) -> Option<Ino> {
        let mut fs = self.vol.lock().unwrap();
        let mut cur = fs.root_ino();
        for c in path.split('/').filter(|c| !c.is_empty()) {
            cur = fs.lookup(cur, c).ok()?;
        }
        Some(cur)
    }
}

impl Server for Lan {
    fn request(&self) -> Value {
        let s = self.srv.as_ref().unwrap();
        json!({ "kind": "lan", "url": format!("https://127.0.0.1:{}", s.port()), "pairing_code": self.code, "fingerprint": s.fingerprint() })
    }
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        let i = self.lookup(path)?;
        let mut fs = self.vol.lock().unwrap();
        let size = fs.getattr(i).ok()?.size;
        fs.read(i, 0, size as u32).ok()
    }
    fn write(&self, path: &str, data: &[u8]) {
        let i = self.lookup(path).unwrap();
        let mut fs = self.vol.lock().unwrap();
        fs.setattr(i, &lumendisk_core::vfs::SetAttr { size: Some(0), ..Default::default() }).unwrap();
        fs.write(i, 0, data).unwrap();
    }
    /// A new share of the same volume: new port, pairing code and certificate.
    fn restart(&mut self) {
        if let Some(s) = self.srv.take() {
            s.stop();
        }
        self.code = new_pairing_code();
        self.srv = Some(LanServer::start(self.vol.clone(), LanMode::Share { token: token_from_pairing_code(&self.code) }, SHARE_NAME, false).unwrap());
    }
    fn kind(&self) -> &'static str {
        "lan"
    }
    fn display(&self) -> String {
        SHARE_NAME.into()
    }
}

// ───────────────────────── the scenario ─────────────────────────

fn workdir_for(tag: &str) -> PathBuf {
    let dir = workdir(&format!("remote_shadow_{tag}"));
    std::fs::create_dir_all(dir.join("mnt")).unwrap();
    dir
}

fn text(v: Option<Vec<u8>>) -> String {
    v.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|| "<missing>".into())
}

fn scenario(dir: &Path, srv: &mut dyn Server) {
    let kind = srv.kind();
    println!("── {kind}: changes survive kill -9, then commit");
    let mut m = mount(dir, &srv.request()).unwrap();
    let mp = m.mp.clone();
    assert_eq!(std::fs::read_to_string(mp.join("docs/a.txt")).unwrap(), "alpha\n");
    std::fs::create_dir(mp.join("new")).unwrap();
    std::fs::write(mp.join("new/hello.txt"), "ciao dal Mac").unwrap();
    std::fs::write(mp.join("docs/a.txt"), "alpha, edited in Shadow").unwrap();
    std::fs::remove_file(mp.join("old.log")).unwrap();
    std::fs::rename(mp.join("docs"), mp.join("documents")).unwrap();
    let st = ctl(dir, "status");
    assert_eq!(st["shadow"]["persistent"], true, "{st}");
    let key = st["shadow"]["session_key"].as_str().unwrap().to_string();
    assert!(key.starts_with(&format!("{kind}-")), "{key}");
    settle();
    m.kill9();

    let s = sessions(dir);
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0]["key"], key.as_str());
    assert_eq!(s[0]["kind"], kind);
    assert_eq!(s[0]["state"], "pending");
    assert_eq!(s[0]["name"], srv.display().as_str(), "{s:?}");
    assert_eq!(s[0]["connected"], false, "a remote session is not 'connected' until it is mounted again");
    assert!(s[0]["fingerprint_ok"].is_null(), "unknown until mounted: {s:?}");
    assert!(s[0]["changes"].as_u64().unwrap() >= 4, "{s:?}");
    let sdir = dir.join("support/Shadow").join(&key);
    assert!(sdir.join("manifest.json").is_file() && sdir.join("journal.ldj").metadata().unwrap().len() > 0, "session on disk");
    if kind == "webdav" {
        let j = std::fs::read(sdir.join("journal.ldj")).unwrap();
        assert!(j.windows(6).any(|w| w == b"\"tag\":"), "WebDAV bases carry the server's ETag");
    }
    let manifest = std::fs::read_to_string(sdir.join("manifest.json")).unwrap();
    assert!(!manifest.contains("raspberry") && !manifest.contains(&srv.request()["pairing_code"].as_str().unwrap_or("§none§").to_string()), "no credentials on disk: {manifest}");
    println!("    session {key} kept {} changes, endpoint {}", s[0]["changes"], s[0]["endpoint"]);
    assert_eq!(text(srv.read("docs/a.txt")), "alpha\n", "Shadow must not touch the server");
    assert_eq!(text(srv.read("old.log")), "log\n");
    assert!(srv.read("new/hello.txt").is_none() && srv.read("documents/a.txt").is_none());

    srv.restart();
    let m = mount(dir, &srv.request()).unwrap();
    let mp = m.mp.clone();
    let st = ctl(dir, "status");
    assert_eq!(st["shadow"]["restored"], true, "{st}");
    assert_eq!(st["shadow"]["session_key"], key.as_str());
    assert_eq!(std::fs::read_to_string(mp.join("new/hello.txt")).unwrap(), "ciao dal Mac");
    assert_eq!(std::fs::read_to_string(mp.join("documents/a.txt")).unwrap(), "alpha, edited in Shadow");
    assert_eq!(std::fs::read_to_string(mp.join("documents/b.txt")).unwrap(), "bravo\n");
    assert!(!mp.join("old.log").exists() && !mp.join("docs").exists());
    let applied = ctl(dir, "commit");
    assert!(applied["applied"].as_u64().unwrap() >= 4, "{applied}");
    // The commit returned: the server already has everything (nothing is only queued).
    assert_eq!(text(srv.read("new/hello.txt")), "ciao dal Mac");
    assert_eq!(text(srv.read("documents/a.txt")), "alpha, edited in Shadow");
    assert_eq!(text(srv.read("documents/b.txt")), "bravo\n");
    assert!(srv.read("old.log").is_none() && srv.read("docs/a.txt").is_none());
    assert_eq!(text(srv.read("keep.txt")), "keep\n");
    unmount(dir, m);
    assert!(sessions(dir).is_empty(), "the session is gone after the commit");

    println!("── {kind}: a file changed on the server while unmounted → drift, discard only");
    let mut m = mount(dir, &srv.request()).unwrap();
    std::fs::write(m.mp.join("documents/a.txt"), "second round").unwrap();
    settle();
    m.kill9();
    srv.write("documents/a.txt", b"changed on the server\n");
    srv.restart();
    let err = mount(dir, &srv.request()).err().expect("the Shadow mount must be refused");
    assert!(err.contains("server") && err.contains("a.txt"), "{err}");
    let s = sessions(dir);
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0]["fingerprint_ok"], false, "{s:?}");
    assert!(s[0]["fingerprint_problem"].as_str().unwrap().contains("a.txt"), "{s:?}");
    println!("    drift: {}", s[0]["fingerprint_problem"]);
    assert!(mount(dir, &srv.request()).is_err(), "still refused");
    let d = discard(dir, s[0]["key"].as_str().unwrap());
    assert!(d["discarded"].as_u64().unwrap() >= 1, "{d}");
    assert!(sessions(dir).is_empty());
    assert_eq!(text(srv.read("documents/a.txt")), "changed on the server\n", "the server's version stays");

    println!("── {kind}: a file changed on the server while mounted → the commit is refused");
    let m = mount(dir, &srv.request()).unwrap();
    std::fs::write(m.mp.join("documents/b.txt"), "edited in Shadow, round three").unwrap();
    std::fs::write(m.mp.join("round3.txt"), "new").unwrap();
    settle();
    srv.write("documents/b.txt", b"changed on the server meanwhile\n");
    let r = ctl_raw(dir, "commit");
    assert_eq!(r["ok"], false, "{r}");
    let e = r["error"].as_str().unwrap();
    assert!(e.contains("server") && e.contains("b.txt"), "{e}");
    let s = sessions(dir);
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0]["fingerprint_ok"], false, "{s:?}");
    assert_eq!(ctl(dir, "status")["shadow"]["drift"].is_string(), true);
    assert_eq!(ctl_raw(dir, "commit")["ok"], false, "still refused");
    assert!(srv.read("round3.txt").is_none());
    let d = ctl(dir, "rollback");
    assert!(d["discarded"].as_u64().unwrap() >= 2, "{d}");
    unmount(dir, m);
    assert!(sessions(dir).is_empty());
    assert_eq!(text(srv.read("documents/b.txt")), "changed on the server meanwhile\n");
    assert!(srv.read("round3.txt").is_none());
}

#[test]
#[ignore]
fn e2e_sftp_shadow_session_survives_kill9_and_detects_drift() {
    let dir = workdir_for("sftp");
    let mut srv = Sftp::start(&dir);
    scenario(&dir, &mut srv);
}

#[test]
#[ignore]
fn e2e_webdav_shadow_session_survives_kill9_and_detects_drift() {
    let dir = workdir_for("webdav");
    let mut srv = Dav::start();
    scenario(&dir, &mut srv);
}

#[test]
#[ignore]
fn e2e_lan_shadow_session_survives_kill9_and_share_restart() {
    let dir = workdir_for("lan");
    let mut srv = Lan::start();
    scenario(&dir, &mut srv);
}
