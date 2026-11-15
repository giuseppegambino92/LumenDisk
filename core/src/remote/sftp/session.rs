//! The ssh child process, the request/reply demultiplexer and reconnect logic.
//!
//! One [`Conn`] = one `ssh -s host sftp` child. Three threads per connection:
//! * writer: drains an unbounded queue of encoded packets into the child's stdin, so a
//!   caller never blocks on a full pipe (e.g. while the server is frozen);
//! * reader: parses packets from stdout and routes each reply to the per-request channel
//!   registered under its id (callers wait with `recv_timeout`, so they never hang);
//! * stderr: keeps the tail of ssh's diagnostics to turn into readable errors.
//!
//! [`Session`] owns the current connection and replaces it on the next call once it is dead.

use std::collections::{HashMap, VecDeque};
use std::io::{BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::proto::*;
use super::SftpConfig;
use crate::remote::{RResult, RemoteError};

/// Bytes per READ / WRITE request.
pub const CHUNK: u32 = 32 * 1024;
/// Requests kept in flight by pipelined transfers (64 x 32 KiB = 2 MiB, like OpenSSH's sftp).
pub const WINDOW: usize = 64;
/// Upper bound for an incoming packet; anything bigger is a protocol error.
const MAX_PACKET: usize = 4 << 20;
/// Timeouts in a row without a single byte from the server before the session is dropped.
const SILENT_TIMEOUTS_BEFORE_KILL: u32 = 3;
/// A handshake that has not completed after this long is abandoned (ssh's own
/// ConnectTimeout normally fires much earlier).
const HANDSHAKE_GIVE_UP: Duration = Duration::from_secs(30);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------------------------
// SSH_ASKPASS helper

/// Private temp dir (0700) with an askpass script (0700) that prints `$LUMENDISK_SSH_PASS`.
/// The password itself only ever lives in the ssh child's environment. Removed on drop.
struct Askpass {
    dir: PathBuf,
    script: PathBuf,
}

impl Askpass {
    fn create() -> RResult<Askpass> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let io = |e: std::io::Error| RemoteError::Other(format!("cannot create askpass helper: {e}"));
        let dir = std::env::temp_dir().join(format!("lumendisk-askpass-{}", uuid::Uuid::new_v4().simple()));
        // `create` (not create_all) fails if the path already exists: no symlink games.
        std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(io)?;
        let script = dir.join("askpass");
        let res = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&script)
            .and_then(|mut f| f.write_all(b"#!/bin/sh\nprintf '%s\\n' \"$LUMENDISK_SSH_PASS\"\n"));
        if let Err(e) = res {
            let _ = std::fs::remove_file(&script);
            let _ = std::fs::remove_dir(&dir);
            return Err(io(e));
        }
        Ok(Askpass { dir, script })
    }
}

impl Drop for Askpass {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.script);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

// ---------------------------------------------------------------------------------------------
// ssh command line

struct SshSpec {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    target: String,
}

/// Quote a value for `-oKey=value` (ssh parses it like a config-file line).
fn opt_value(v: &str) -> String {
    if v.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'') {
        format!("\"{}\"", v.replace('"', ""))
    } else {
        v.to_string()
    }
}

fn build_spec(cfg: &SftpConfig, askpass: Option<&Askpass>) -> SshSpec {
    let mut a: Vec<String> = Vec::new();
    if let Some(f) = &cfg.ssh_config_file {
        a.push("-F".into());
        a.push(f.clone());
    }
    a.extend(["-T", "-x", "-a"].map(String::from));
    a.push(format!("-oBatchMode={}", if askpass.is_some() { "no" } else { "yes" }));
    a.push(format!("-oConnectTimeout={}", cfg.connect_timeout_secs.max(1)));
    a.push("-oServerAliveInterval=5".into());
    a.push("-oServerAliveCountMax=2".into());
    a.push(format!("-oStrictHostKeyChecking={}", cfg.strict_host_key_checking));
    if askpass.is_some() {
        a.push("-oNumberOfPasswordPrompts=1".into());
    }
    if let Some(k) = &cfg.known_hosts_file {
        a.push(format!("-oUserKnownHostsFile={}", opt_value(k)));
    }
    if let Some(i) = &cfg.identity_file {
        a.push("-i".into());
        a.push(i.clone());
        a.push("-oIdentitiesOnly=yes".into());
    }
    a.push("-p".into());
    a.push(cfg.port.to_string());
    if let Some(u) = &cfg.user {
        a.push("-l".into());
        a.push(u.clone());
    }
    a.extend(cfg.extra_ssh_options.iter().cloned());
    a.push("-s".into());
    a.push("--".into());
    a.push(cfg.host.clone());
    a.push("sftp".into());

    let mut env = Vec::new();
    if let (Some(ap), Some(pw)) = (askpass, &cfg.password) {
        env.push(("SSH_ASKPASS".into(), ap.script.to_string_lossy().into_owned()));
        env.push(("SSH_ASKPASS_REQUIRE".into(), "force".into()));
        env.push(("LUMENDISK_SSH_PASS".into(), pw.clone()));
        if std::env::var_os("DISPLAY").is_none() {
            env.push(("DISPLAY".into(), "lumendisk:0".into()));
        }
    }
    let target = match &cfg.user {
        Some(u) => format!("{u}@{}", cfg.host),
        None => cfg.host.clone(),
    };
    SshSpec { program: cfg.ssh_program.clone(), args: a, env, target }
}

/// Turn ssh's stderr into a readable error.
fn classify_exit(stderr: &str, target: &str) -> RemoteError {
    let text: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("Warning: Permanently added"))
        .collect();
    let text = text.join(" | ");
    let has = |s: &str| text.contains(s);
    if has("Permission denied") || has("Too many authentication failures") {
        RemoteError::Other(format!("SSH authentication failed for {target}: {text}"))
    } else if has("Host key verification failed") || has("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        // Keep the useful lines of ssh's big warning banner.
        let key_lines: Vec<&str> = stderr
            .lines()
            .map(str::trim)
            .filter(|l| {
                l.starts_with("Host key for")
                    || l.starts_with("Offending")
                    || l.contains("SHA256:")
                    || l.starts_with("No ")
                    || l.starts_with("Host key verification failed")
            })
            .collect();
        RemoteError::Other(format!(
            "SSH host key verification failed for {target} (the server's key changed or is unknown): {}",
            key_lines.join(" ")
        ))
    } else if has("subsystem request failed") {
        RemoteError::Unsupported(format!("{target} does not offer the SFTP subsystem: {text}"))
    } else if text.is_empty() {
        RemoteError::Disconnected(format!("ssh session to {target} closed"))
    } else {
        RemoteError::Disconnected(format!("ssh {target}: {text}"))
    }
}

// ---------------------------------------------------------------------------------------------
// Connection

pub struct Reply {
    pub typ: u8,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct ServerInfo {
    pub version: u32,
    pub ext: HashMap<String, Vec<u8>>,
}

impl ServerInfo {
    pub fn has(&self, name: &str) -> bool {
        self.ext.contains_key(name)
    }
}

enum Hs {
    Pending,
    Ready(Arc<ServerInfo>),
    Failed(RemoteError),
}

struct Pending {
    map: HashMap<u32, Sender<Reply>>,
    dead: Option<RemoteError>,
}

struct Shared {
    pending: Mutex<Pending>,
    hs: Mutex<Hs>,
    hs_cv: Condvar,
    started: Instant,
    /// Milliseconds after `started` at which the last packet arrived (0 = never).
    last_rx_ms: AtomicU64,
    stderr: Mutex<String>,
    target: String,
}

impl Shared {
    fn mark_dead(&self, err: RemoteError) {
        {
            let mut p = lock(&self.pending);
            if p.dead.is_none() {
                p.dead = Some(err.clone());
            }
            // Dropping the senders wakes every waiter with RecvError → dead_error().
            p.map.clear();
        }
        let mut hs = lock(&self.hs);
        if matches!(*hs, Hs::Pending) {
            *hs = Hs::Failed(err);
        }
        self.hs_cv.notify_all();
    }

    fn dead_error(&self) -> RemoteError {
        lock(&self.pending)
            .dead
            .clone()
            .unwrap_or_else(|| RemoteError::Disconnected("sftp connection closed".into()))
    }
}

pub struct Ticket {
    id: u32,
    rx: Receiver<Reply>,
    shared: Arc<Shared>,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        lock(&self.shared.pending).map.remove(&self.id);
    }
}

pub struct Conn {
    pub gen: u64,
    child: Mutex<Option<Child>>,
    tx: Mutex<Sender<Vec<u8>>>,
    next_id: AtomicU32,
    shared: Arc<Shared>,
}

fn no_sigpipe_on(fd: std::os::fd::RawFd) {
    #[cfg(target_vendor = "apple")]
    unsafe {
        libc::fcntl(fd, libc::F_SETNOSIGPIPE, 1);
    }
    #[cfg(not(target_vendor = "apple"))]
    let _ = fd;
}

/// Block SIGPIPE for the current thread: a write to a dead ssh then fails with EPIPE instead
/// of killing the host app (a staticlib does not get Rust's SIG_IGN default).
fn block_sigpipe_in_thread() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

impl Conn {
    fn spawn(spec: &SshSpec, gen: u64) -> RResult<Conn> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        // New session: no controlling terminal, so ssh can never block on a /dev/tty prompt.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| RemoteError::Other(format!("cannot run {}: {e}", spec.program)))?;
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        no_sigpipe_on(stdin.as_raw_fd());

        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending { map: HashMap::new(), dead: None }),
            hs: Mutex::new(Hs::Pending),
            hs_cv: Condvar::new(),
            started: Instant::now(),
            last_rx_ms: AtomicU64::new(0),
            stderr: Mutex::new(String::new()),
            target: spec.target.clone(),
        });

        // stderr collector
        let (err_done_tx, err_done_rx) = mpsc::channel::<()>();
        {
            let sh = shared.clone();
            std::thread::Builder::new()
                .name("sftp-stderr".into())
                .spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stderr.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let mut s = lock(&sh.stderr);
                        s.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if s.len() > 8192 {
                            let cut = s.len() - 4096;
                            let cut = (cut..s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len());
                            s.drain(..cut);
                        }
                    }
                    let _ = err_done_tx.send(());
                })
                .map_err(|e| RemoteError::Other(format!("thread: {e}")))?;
        }

        // writer
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        {
            let sh = shared.clone();
            std::thread::Builder::new()
                .name("sftp-writer".into())
                .spawn(move || {
                    block_sigpipe_in_thread();
                    let mut w = BufWriter::with_capacity(256 * 1024, stdin);
                    let res: std::io::Result<()> = (|| {
                        while let Ok(p) = rx.recv() {
                            w.write_all(&p)?;
                            while let Ok(p) = rx.try_recv() {
                                w.write_all(&p)?;
                            }
                            w.flush()?;
                        }
                        Ok(())
                    })();
                    if let Err(e) = res {
                        // Let the reader report ssh's own reason first, if it has one.
                        std::thread::sleep(Duration::from_millis(300));
                        sh.mark_dead(RemoteError::Disconnected(format!("write to ssh failed: {e}")));
                    }
                })
                .map_err(|e| RemoteError::Other(format!("thread: {e}")))?;
        }

        // reader / demultiplexer
        {
            let sh = shared.clone();
            std::thread::Builder::new()
                .name("sftp-reader".into())
                .spawn(move || {
                    let err = reader_loop(&mut stdout, &sh);
                    let err = match err {
                        Some(e) => e,
                        None => {
                            // EOF: ssh is exiting; give stderr a moment to drain.
                            let _ = err_done_rx.recv_timeout(Duration::from_millis(1000));
                            let text = lock(&sh.stderr).clone();
                            classify_exit(&text, &sh.target)
                        }
                    };
                    sh.mark_dead(err);
                })
                .map_err(|e| RemoteError::Other(format!("thread: {e}")))?;
        }

        // SSH_FXP_INIT (version 3) carries no request id.
        let _ = tx.send(vec![0, 0, 0, 5, FXP_INIT, 0, 0, 0, 3]);
        Ok(Conn {
            gen,
            child: Mutex::new(Some(child)),
            tx: Mutex::new(tx),
            next_id: AtomicU32::new(1),
            shared,
        })
    }

    pub fn is_dead(&self) -> bool {
        lock(&self.shared.pending).dead.is_some()
    }

    pub fn age(&self) -> Duration {
        self.shared.started.elapsed()
    }

    fn last_rx(&self) -> Option<Instant> {
        match self.shared.last_rx_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(self.shared.started + Duration::from_millis(ms)),
        }
    }

    pub fn info(&self) -> Option<Arc<ServerInfo>> {
        match &*lock(&self.shared.hs) {
            Hs::Ready(i) => Some(i.clone()),
            _ => None,
        }
    }

    fn wait_ready(&self, wait: Duration) -> RResult<Arc<ServerInfo>> {
        let deadline = Instant::now() + wait;
        let mut hs = lock(&self.shared.hs);
        loop {
            match &*hs {
                Hs::Ready(i) => return Ok(i.clone()),
                Hs::Failed(e) => return Err(e.clone()),
                Hs::Pending => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RemoteError::Timeout);
            }
            hs = self.shared.hs_cv.wait_timeout(hs, deadline - now).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    fn packet(typ: u8, id: Option<u32>, body: &[u8]) -> Vec<u8> {
        let len = 1 + if id.is_some() { 4 } else { 0 } + body.len();
        let mut p = Vec::with_capacity(4 + len);
        p.extend_from_slice(&(len as u32).to_be_bytes());
        p.push(typ);
        if let Some(id) = id {
            p.extend_from_slice(&id.to_be_bytes());
        }
        p.extend_from_slice(body);
        p
    }

    /// Queue a request; the reply is collected with [`Session::wait`].
    pub fn send(&self, typ: u8, body: &[u8]) -> RResult<Ticket> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (stx, srx) = mpsc::channel();
        {
            let mut p = lock(&self.shared.pending);
            if let Some(e) = &p.dead {
                return Err(e.clone());
            }
            p.map.insert(id, stx);
        }
        let t = Ticket { id, rx: srx, shared: self.shared.clone() };
        if lock(&self.tx).send(Self::packet(typ, Some(id), body)).is_err() {
            return Err(self.shared.dead_error());
        }
        Ok(t)
    }

    /// Queue a request whose reply is ignored (e.g. CLOSE of a read handle).
    pub fn send_nowait(&self, typ: u8, body: &[u8]) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = lock(&self.tx).send(Self::packet(typ, Some(id), body));
    }

    pub fn kill(&self, err: RemoteError) {
        self.shared.mark_dead(err);
        if let Some(c) = lock(&self.child).as_mut() {
            let _ = c.kill();
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(mut c) = lock(&self.child).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Returns Some(error) for protocol errors, None for EOF / read errors (ssh exited).
fn reader_loop(r: &mut impl Read, sh: &Shared) -> Option<RemoteError> {
    let mut first = true;
    loop {
        let mut lb = [0u8; 4];
        if r.read_exact(&mut lb).is_err() {
            return None;
        }
        let len = u32::from_be_bytes(lb) as usize;
        if len == 0 || len > MAX_PACKET || (first && len > 64 * 1024) {
            let hint = if first {
                " (is a shell startup file printing text on non-interactive logins?)"
            } else {
                ""
            };
            return Some(RemoteError::Other(format!("sftp protocol error: bad packet length {len}{hint}")));
        }
        let mut body = vec![0u8; len];
        if r.read_exact(&mut body).is_err() {
            return None;
        }
        let ms = sh.started.elapsed().as_millis() as u64;
        sh.last_rx_ms.store(ms.max(1), Ordering::Relaxed);
        let typ = body[0];
        if first {
            first = false;
            if typ != FXP_VERSION {
                return Some(RemoteError::Other(format!("sftp protocol error: expected VERSION, got type {typ}")));
            }
            let mut rd = Rd::new(&body[1..]);
            let version = match rd.u32() {
                Ok(v) => v,
                Err(e) => return Some(e),
            };
            let mut ext = HashMap::new();
            while rd.remaining() > 0 {
                match (rd.string(), rd.bytes()) {
                    (Ok(n), Ok(d)) => {
                        ext.insert(n, d.to_vec());
                    }
                    _ => break,
                }
            }
            let mut hs = lock(&sh.hs);
            *hs = Hs::Ready(Arc::new(ServerInfo { version, ext }));
            sh.hs_cv.notify_all();
            continue;
        }
        if len < 5 {
            continue;
        }
        let id = u32::from_be_bytes(body[1..5].try_into().unwrap());
        let tx = lock(&sh.pending).map.remove(&id);
        if let Some(tx) = tx {
            body.drain(..5);
            let _ = tx.send(Reply { typ, data: body });
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Reply helpers

fn status_of(data: &[u8]) -> (u32, String) {
    let mut rd = Rd::new(data);
    let code = rd.u32().unwrap_or(FX_FAILURE);
    let msg = rd.string().unwrap_or_default();
    (code, msg)
}

/// Body reader for a reply of type `want`; STATUS replies become errors.
pub fn expect(r: &Reply, want: u8) -> RResult<Rd<'_>> {
    if r.typ == want {
        return Ok(Rd::new(&r.data));
    }
    if r.typ == FXP_STATUS {
        let (code, msg) = status_of(&r.data);
        return Err(if code == FX_OK {
            RemoteError::Other(format!("sftp: unexpected OK status (wanted packet type {want})"))
        } else {
            status_error(code, &msg)
        });
    }
    Err(RemoteError::Other(format!("sftp: unexpected packet type {} (wanted {want})", r.typ)))
}

pub fn expect_ok(r: &Reply) -> RResult<()> {
    if r.typ == FXP_STATUS {
        let (code, msg) = status_of(&r.data);
        if code == FX_OK {
            return Ok(());
        }
        return Err(status_error(code, &msg));
    }
    Err(RemoteError::Other(format!("sftp: unexpected packet type {} (wanted STATUS)", r.typ)))
}

fn is_eof(r: &Reply) -> bool {
    r.typ == FXP_STATUS && status_of(&r.data).0 == FX_EOF
}

// ---------------------------------------------------------------------------------------------
// Session

pub struct Session {
    spec: SshSpec,
    _askpass: Option<Askpass>,
    cur: Mutex<Option<Arc<Conn>>>,
    gen: AtomicU64,
    timeout_ms: AtomicU64,
    silent_timeouts: AtomicU32,
}

impl Session {
    pub fn new(cfg: &SftpConfig) -> RResult<Session> {
        let askpass = if cfg.password.is_some() { Some(Askpass::create()?) } else { None };
        let spec = build_spec(cfg, askpass.as_ref());
        Ok(Session {
            spec,
            _askpass: askpass,
            cur: Mutex::new(None),
            gen: AtomicU64::new(0),
            timeout_ms: AtomicU64::new(cfg.timeout.as_millis().max(1) as u64),
            silent_timeouts: AtomicU32::new(0),
        })
    }

    pub fn set_timeout(&self, t: Duration) {
        self.timeout_ms.store(t.as_millis().max(1) as u64, Ordering::Relaxed);
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.load(Ordering::Relaxed))
    }

    /// Current live connection, (re)spawning ssh if needed and waiting at most the current
    /// timeout for the handshake.
    pub fn conn(&self) -> RResult<Arc<Conn>> {
        self.conn_wait(self.timeout())
    }

    pub fn conn_wait(&self, wait: Duration) -> RResult<Arc<Conn>> {
        let c = {
            let mut g = lock(&self.cur);
            match g.as_ref() {
                Some(c) if !c.is_dead() => c.clone(),
                _ => {
                    // Drop the dead one outside of any reply wait; Conn::drop reaps the child.
                    *g = None;
                    let gen = self.gen.fetch_add(1, Ordering::Relaxed) + 1;
                    let c = Arc::new(Conn::spawn(&self.spec, gen)?);
                    *g = Some(c.clone());
                    c
                }
            }
        };
        match c.wait_ready(wait) {
            Ok(_) => Ok(c),
            Err(RemoteError::Timeout) => {
                // Keep a slow handshake alive for the next call, unless it is hopeless.
                if c.age() > HANDSHAKE_GIVE_UP {
                    c.kill(RemoteError::Disconnected(format!("ssh handshake with {} timed out", self.spec.target)));
                }
                Err(RemoteError::Timeout)
            }
            Err(e) => Err(e),
        }
    }

    /// The current connection if it is alive and ready (never spawns ssh).
    pub fn current(&self) -> Option<Arc<Conn>> {
        lock(&self.cur).as_ref().filter(|c| !c.is_dead() && c.info().is_some()).cloned()
    }

    /// Drop the current connection (next call reconnects).
    pub fn disconnect(&self) {
        if let Some(c) = lock(&self.cur).take() {
            c.kill(RemoteError::Disconnected("disconnected".into()));
        }
    }

    /// Wait for a reply for at most `d`.
    pub fn wait(&self, c: &Conn, t: &Ticket, d: Duration) -> RResult<Reply> {
        let since = Instant::now();
        match t.rx.recv_timeout(d) {
            Ok(r) => {
                self.silent_timeouts.store(0, Ordering::Relaxed);
                Ok(r)
            }
            Err(RecvTimeoutError::Timeout) => {
                let silent = c.last_rx().map(|t| t < since).unwrap_or(true);
                if silent {
                    let n = self.silent_timeouts.fetch_add(1, Ordering::Relaxed) + 1;
                    if n >= SILENT_TIMEOUTS_BEFORE_KILL {
                        self.silent_timeouts.store(0, Ordering::Relaxed);
                        c.kill(RemoteError::Disconnected(format!(
                            "no response from {} after {n} timeouts",
                            self.spec.target
                        )));
                    }
                }
                Err(RemoteError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(c.shared.dead_error()),
        }
    }

    /// One request/reply round trip on `c` with the current timeout.
    pub fn call_on(&self, c: &Conn, typ: u8, body: &[u8]) -> RResult<Reply> {
        let t = c.send(typ, body)?;
        self.wait(c, &t, self.timeout())
    }

    pub fn call(&self, typ: u8, body: &[u8]) -> RResult<Reply> {
        let c = self.conn()?;
        self.call_on(&c, typ, body)
    }

    pub fn open(&self, c: &Conn, path: &str, pflags: u32, attrs: &Attrs) -> RResult<Vec<u8>> {
        let r = self.call_on(c, FXP_OPEN, &Buf::new().str(path).u32(pflags).attrs(attrs).0)?;
        Ok(expect(&r, FXP_HANDLE)?.bytes()?.to_vec())
    }

    pub fn close_nowait(&self, c: &Conn, h: &[u8]) {
        c.send_nowait(FXP_CLOSE, &Buf::new().bytes(h).0);
    }

    pub fn close(&self, c: &Conn, h: &[u8]) -> RResult<()> {
        expect_ok(&self.call_on(c, FXP_CLOSE, &Buf::new().bytes(h).0)?)
    }

    fn send_read(c: &Conn, h: &[u8], off: u64, n: u32) -> RResult<Ticket> {
        c.send(FXP_READ, &Buf::new().bytes(h).u64(off).u32(n).0)
    }

    /// Pipelined read of up to `len` bytes at `offset`; short only at EOF. Each reply must
    /// arrive within the current timeout of the previous one (progress-based deadline).
    pub fn read_at(&self, c: &Conn, h: &[u8], offset: u64, len: u64) -> RResult<Vec<u8>> {
        let end = offset.saturating_add(len);
        let mut out = Vec::with_capacity(len.min(64 << 20) as usize);
        let mut pos = offset;
        'outer: while pos < end {
            let mut q: VecDeque<(u32, Ticket)> = VecDeque::new();
            let mut next = pos;
            while q.len() < WINDOW && next < end {
                let n = (end - next).min(CHUNK as u64) as u32;
                q.push_back((n, Self::send_read(c, h, next, n)?));
                next += n as u64;
            }
            while let Some((n, t)) = q.pop_front() {
                let r = self.wait(c, &t, self.timeout())?;
                if is_eof(&r) {
                    break 'outer;
                }
                let d = expect(&r, FXP_DATA)?.bytes()?;
                let got = d.len().min(n as usize);
                out.extend_from_slice(&d[..got]);
                pos += got as u64;
                if got == 0 {
                    break 'outer;
                }
                if got < n as usize {
                    // Short read: EOF if the following request says so, otherwise the server
                    // just returned less; re-issue from the new position.
                    if let Some((_, t2)) = q.pop_front() {
                        if is_eof(&self.wait(c, &t2, self.timeout())?) {
                            break 'outer;
                        }
                    }
                    continue 'outer;
                }
                if next < end {
                    let n = (end - next).min(CHUNK as u64) as u32;
                    q.push_back((n, Self::send_read(c, h, next, n)?));
                    next += n as u64;
                }
            }
        }
        Ok(out)
    }

    /// Pipelined write of `data` at `offset` (progress-based deadline like `read_at`).
    pub fn write_at(&self, c: &Conn, h: &[u8], offset: u64, data: &[u8]) -> RResult<()> {
        let mut q: VecDeque<Ticket> = VecDeque::new();
        let mut next = 0usize;
        loop {
            while q.len() < WINDOW && next < data.len() {
                let n = (data.len() - next).min(CHUNK as usize);
                let mut b = Vec::with_capacity(h.len() + n + 16);
                b.extend_from_slice(&(h.len() as u32).to_be_bytes());
                b.extend_from_slice(h);
                b.extend_from_slice(&(offset + next as u64).to_be_bytes());
                b.extend_from_slice(&(n as u32).to_be_bytes());
                b.extend_from_slice(&data[next..next + n]);
                q.push_back(c.send(FXP_WRITE, &b)?);
                next += n;
            }
            let Some(t) = q.pop_front() else { return Ok(()) };
            expect_ok(&self.wait(c, &t, self.timeout())?)?;
        }
    }

    /// READDIR loop with a few requests in flight.
    pub fn readdir_all(&self, c: &Conn, h: &[u8]) -> RResult<Vec<(String, Attrs)>> {
        const DIR_WINDOW: usize = 4;
        let mut out = Vec::new();
        let mut q: VecDeque<Ticket> = VecDeque::new();
        for _ in 0..DIR_WINDOW {
            q.push_back(c.send(FXP_READDIR, &Buf::new().bytes(h).0)?);
        }
        while let Some(t) = q.pop_front() {
            let r = self.wait(c, &t, self.timeout())?;
            if is_eof(&r) {
                break;
            }
            let mut rd = expect(&r, FXP_NAME)?;
            let n = rd.u32()?;
            for _ in 0..n {
                let name = rd.string()?;
                let _long = rd.bytes()?;
                let a = rd.attrs()?;
                if name != "." && name != ".." {
                    out.push((name, a));
                }
            }
            q.push_back(c.send(FXP_READDIR, &Buf::new().bytes(h).0)?);
        }
        Ok(out)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify() {
        assert!(matches!(
            classify_exit("pi@h: Permission denied (publickey,password).\n", "pi@h"),
            RemoteError::Other(m) if m.contains("authentication failed")
        ));
        assert!(matches!(
            classify_exit("Host key verification failed.\n", "h"),
            RemoteError::Other(m) if m.contains("host key")
        ));
        assert!(matches!(
            classify_exit("ssh: connect to host h port 22: Connection refused\n", "h"),
            RemoteError::Disconnected(m) if m.contains("Connection refused")
        ));
    }

    #[test]
    fn askpass_lifecycle() {
        use std::os::unix::fs::PermissionsExt;
        let a = Askpass::create().unwrap();
        let (d, s) = (a.dir.clone(), a.script.clone());
        assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&s).unwrap().permissions().mode() & 0o777, 0o700);
        let out = Command::new(&s).env("LUMENDISK_SSH_PASS", "s3cr et").output().unwrap();
        assert_eq!(out.stdout, b"s3cr et\n");
        drop(a);
        assert!(!d.exists());
    }
}
