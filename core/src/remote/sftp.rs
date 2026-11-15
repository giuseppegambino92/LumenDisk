//! SFTP backend: speaks SFTP v3 (draft-ietf-secsh-filexfer-02) over the stdin/stdout of the
//! system `/usr/bin/ssh -s <host> sftp`, so the user's ~/.ssh/config, keys, ssh-agent and
//! known_hosts all apply and no SSH library is linked.
//!
//! * Key / agent auth: ssh runs with `BatchMode=yes` (never prompts).
//! * Password auth: an SSH_ASKPASS helper in a private 0700 temp dir prints the password,
//!   which is passed only through the ssh child's environment (`SSH_ASKPASS_REQUIRE=force`).
//! * Requests carry ids; a reader thread routes replies to per-request channels, so every
//!   call waits with `recv_timeout` and returns `Timeout` instead of hanging. Reads and writes
//!   are pipelined (32 KiB requests, up to 64 in flight).
//! * When ssh dies or the pipe breaks, in-flight calls get `Disconnected` and the next call
//!   spawns a new ssh. A single timeout keeps the session; three timeouts in a row with no
//!   byte from the server drop it (ssh's ServerAlive probes also end a dead link in ~15 s).
//!
//! Paths given to [`RemoteBackend`] methods are relative to the export root, which is
//! resolved once (SSH_FXP_REALPATH) at connect time; ".." components are refused.

mod proto;
mod session;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use self::proto::*;
use self::session::{expect, expect_ok, Conn, Session};
use super::*;
use crate::vfs::{FileKind, Timestamp};

/// Connection settings. Build with [`SftpConfig::from_url`] or
/// `SftpConfig { host, ..Default::default() }`.
#[derive(Clone)]
pub struct SftpConfig {
    pub host: String,
    pub port: u16,
    /// Login name; `None` lets ssh decide (ssh config `User`, else the local user name).
    pub user: Option<String>,
    /// Export root (for [`SftpRangeSource`]: the remote file). Absolute paths start with "/";
    /// anything else is relative to the login (home) directory, "." being the home itself.
    pub root: String,
    /// Password for password / keyboard-interactive auth (via SSH_ASKPASS). `None` = keys /
    /// agent only, ssh runs in BatchMode and never prompts.
    pub password: Option<String>,
    /// Private key (`ssh -i`, with IdentitiesOnly=yes).
    pub identity_file: Option<String>,
    /// `ssh -F <file>`; `None` = the user's normal ~/.ssh/config. Tests use "/dev/null".
    pub ssh_config_file: Option<String>,
    /// `-oUserKnownHostsFile=<file>`; `None` = the user's normal ~/.ssh/known_hosts.
    pub known_hosts_file: Option<String>,
    /// `-oStrictHostKeyChecking=` value, default "accept-new" (trust on first use, refuse
    /// changed keys).
    pub strict_host_key_checking: String,
    /// ssh binary, default "/usr/bin/ssh".
    pub ssh_program: String,
    /// Extra raw ssh arguments inserted before the destination (e.g. "-oIdentityAgent=none").
    pub extra_ssh_options: Vec<String>,
    /// `-oConnectTimeout=` in seconds (default 5).
    pub connect_timeout_secs: u32,
    /// Initial per-request timeout (default 10 s); change later with `set_timeout`.
    pub timeout: Duration,
    /// Use OpenSSH extensions (posix-rename, statvfs) when the server advertises them
    /// (default true; false behaves like a plain SFTP v3 server).
    pub use_extensions: bool,
}

impl Default for SftpConfig {
    fn default() -> Self {
        SftpConfig {
            host: String::new(),
            port: 22,
            user: None,
            root: ".".into(),
            password: None,
            identity_file: None,
            ssh_config_file: None,
            known_hosts_file: None,
            strict_host_key_checking: "accept-new".into(),
            ssh_program: "/usr/bin/ssh".into(),
            extra_ssh_options: Vec::new(),
            connect_timeout_secs: 5,
            timeout: Duration::from_secs(10),
            use_extensions: true,
        }
    }
}

impl std::fmt::Debug for SftpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("root", &self.root)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("identity_file", &self.identity_file)
            .finish_non_exhaustive()
    }
}

fn pct_decode(s: &str) -> RResult<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok());
            match hex {
                Some(v) => {
                    out.push(v);
                    i += 3;
                    continue;
                }
                None => return Err(RemoteError::Other(format!("bad percent escape in URL: {s}"))),
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).map_err(|_| RemoteError::Other("URL is not valid UTF-8".into()))
}

impl SftpConfig {
    /// Parse `sftp://[user[:password]@]host[:port][/path]` (percent-escapes allowed; IPv6
    /// hosts in brackets). Port defaults to 22.
    ///
    /// Root: no path (or "/~") = the login user's **home directory**; "/~/sub" = relative to
    /// home; any other path is absolute ("sftp://pi@nas/" exports the whole filesystem).
    pub fn from_url(url: &str) -> RResult<Self> {
        let bad = |m: &str| RemoteError::Other(format!("invalid sftp URL '{url}': {m}"));
        let (scheme, rest) = url.split_once("://").ok_or_else(|| bad("missing scheme"))?;
        if !scheme.eq_ignore_ascii_case("sftp") {
            return Err(bad("scheme must be sftp"));
        }
        let (auth, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (userinfo, hostport) = match auth.rfind('@') {
            Some(i) => (Some(&auth[..i]), &auth[i + 1..]),
            None => (None, auth),
        };
        let (user, password) = match userinfo {
            None => (None, None),
            Some(ui) => match ui.split_once(':') {
                Some((u, p)) => (Some(pct_decode(u)?), Some(pct_decode(p)?)),
                None => (Some(pct_decode(ui)?), None),
            },
        };
        let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
            let (h, after) = h.split_once(']').ok_or_else(|| bad("unterminated IPv6 literal"))?;
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or_else(|| bad("junk after host"))?),
            };
            (h.to_string(), port)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (pct_decode(h)?, Some(p)),
                None => (pct_decode(hostport)?, None),
            }
        };
        let port = match port {
            None | Some("") => 22,
            Some(p) => p.parse::<u16>().ok().filter(|&p| p != 0).ok_or_else(|| bad("bad port"))?,
        };
        let path = pct_decode(path)?;
        let root = if path.is_empty() || path == "/~" || path == "/~/" {
            ".".to_string()
        } else if let Some(rel) = path.strip_prefix("/~/") {
            normalize_root(rel, false)
        } else {
            normalize_root(&path, true)
        };
        let cfg = SftpConfig {
            host,
            port,
            user: user.filter(|u| !u.is_empty()),
            root,
            password,
            ..Default::default()
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> RResult<()> {
        let bad = |m: String| Err(RemoteError::Other(m));
        if self.host.is_empty() {
            return bad("sftp: empty host name".into());
        }
        let unsafe_arg = |s: &str| s.starts_with('-') || s.chars().any(|c| c.is_whitespace() || c.is_control());
        if unsafe_arg(&self.host) {
            return bad(format!("sftp: invalid host name '{}'", self.host));
        }
        if let Some(u) = &self.user {
            if unsafe_arg(u) || u.contains('@') {
                return bad(format!("sftp: invalid user name '{u}'"));
            }
        }
        if self.root.contains('\0') {
            return bad("sftp: invalid root path".into());
        }
        Ok(())
    }

    fn display(&self, path: &str) -> String {
        let user = self.user.as_ref().map(|u| format!("{u}@")).unwrap_or_default();
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let path = if path.starts_with('/') { path.to_string() } else { format!("/~/{path}") };
        format!("sftp://{user}{host}:{}{path}", self.port)
    }
}

/// Collapse "//" and "." components; keep ".." (refused later only for trait paths).
fn normalize_root(p: &str, absolute: bool) -> String {
    let parts: Vec<&str> = p.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    match (absolute, parts.is_empty()) {
        (true, true) => "/".into(),
        (true, false) => format!("/{}", parts.join("/")),
        (false, true) => ".".into(),
        (false, false) => parts.join("/"),
    }
}

/// Map a trait path ("/a/b") onto the resolved remote root; refuses ".." and NUL.
fn map_path(root: &str, path: &str) -> RResult<String> {
    let mut out = root.trim_end_matches('/').to_string();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => return Err(RemoteError::Perm),
            c if c.contains('\0') => return Err(RemoteError::Other("NUL in path".into())),
            c => {
                out.push('/');
                out.push_str(c);
            }
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    Ok(out)
}

fn is_root_path(path: &str) -> bool {
    path.split('/').all(|c| c.is_empty() || c == ".")
}

fn last_name(path: &str) -> String {
    path.rsplit('/').find(|c| !c.is_empty() && *c != ".").unwrap_or("").to_string()
}

fn entry_from(name: String, a: &Attrs) -> RemoteEntry {
    let mode = a.perm.unwrap_or(0);
    let kind = match mode & S_IFMT {
        S_IFDIR => FileKind::Dir,
        S_IFLNK => FileKind::Symlink,
        _ => FileKind::File,
    };
    let perm = match (a.perm, &kind) {
        (Some(p), _) => (p & 0o7777) as u16,
        (None, FileKind::Dir) => 0o755,
        (None, _) => 0o644,
    };
    RemoteEntry {
        name,
        kind,
        size: a.size.unwrap_or(0),
        mtime: Timestamp { secs: a.times.map(|t| t.1 as i64).unwrap_or(0), nsecs: 0 },
        perm,
        symlink_target: None,
    }
}

fn to_u32_secs(t: i64) -> u32 {
    t.clamp(0, u32::MAX as i64) as u32
}

/// Shared by the backend and the range source: session + resolved root.
struct Client {
    cfg: SftpConfig,
    sess: Session,
}

impl Client {
    fn connect(cfg: SftpConfig) -> RResult<(Client, String)> {
        cfg.validate()?;
        let sess = Session::new(&cfg)?;
        // The first connection gets a generous budget (key exchange + auth on a slow Pi).
        let wait = cfg.timeout.max(Duration::from_secs(cfg.connect_timeout_secs as u64 + 10));
        let c = sess.conn_wait(wait)?;
        if let Some(i) = c.info() {
            if i.version < 3 {
                return Err(RemoteError::Unsupported(format!("SFTP protocol version {} (need 3)", i.version)));
            }
        }
        let t = c.send(FXP_REALPATH, &Buf::new().str(&cfg.root).0)?;
        let r = sess.wait(&c, &t, wait)?;
        let mut rd = expect(&r, FXP_NAME)?;
        if rd.u32()? < 1 {
            return Err(RemoteError::Other("sftp: empty REALPATH reply".into()));
        }
        let abs = rd.string()?;
        Ok((Client { cfg, sess }, abs))
    }

    fn conn(&self) -> RResult<Arc<Conn>> {
        self.sess.conn()
    }

    fn call(&self, typ: u8, body: Buf) -> RResult<session::Reply> {
        self.sess.call(typ, &body.0)
    }

    fn call_ok(&self, typ: u8, body: Buf) -> RResult<()> {
        expect_ok(&self.call(typ, body)?)
    }

    fn lstat_raw(&self, rp: &str) -> RResult<Attrs> {
        expect(&self.call(FXP_LSTAT, Buf::new().str(rp))?, FXP_ATTRS)?.attrs()
    }

    fn stat_raw(&self, rp: &str) -> RResult<Attrs> {
        expect(&self.call(FXP_STAT, Buf::new().str(rp))?, FXP_ATTRS)?.attrs()
    }

    fn ext(&self, c: &Conn, name: &str) -> bool {
        self.cfg.use_extensions && c.info().map(|i| i.has(name)).unwrap_or(false)
    }

    /// File kind at `rp` (lstat), None if absent or unknown.
    fn kind_of(&self, rp: &str) -> Option<u32> {
        self.lstat_raw(rp).ok().map(|a| a.perm.unwrap_or(0) & S_IFMT)
    }
}

/// [`RemoteBackend`] over SFTP v3.
pub struct SftpBackend {
    cl: Client,
    root: String,
    desc: String,
}

impl SftpBackend {
    /// Spawn ssh, authenticate, and resolve the export root. Fails with a readable error on
    /// authentication / host key / network problems, `NotFound` / `NotDir` for a bad root.
    pub fn connect(cfg: SftpConfig) -> RResult<Self> {
        let (cl, root) = Client::connect(cfg)?;
        let a = cl.stat_raw(&root)?;
        if a.perm.map(|p| p & S_IFMT != S_IFDIR).unwrap_or(false) {
            return Err(RemoteError::NotDir);
        }
        let desc = cl.cfg.display(&root);
        Ok(SftpBackend { cl, root, desc })
    }

    /// Absolute remote path of the export root.
    pub fn remote_root(&self) -> &str {
        &self.root
    }

    /// Kill the current ssh session (the next call reconnects).
    pub fn disconnect(&self) {
        self.cl.sess.disconnect();
    }

    fn rp(&self, path: &str) -> RResult<String> {
        map_path(&self.root, path)
    }

    /// For errors from operations whose target should be a regular file.
    fn refine_file_op(&self, rp: &str, e: RemoteError) -> RemoteError {
        if (is_failure(&e) || e == RemoteError::Perm) && self.cl.kind_of(rp) == Some(S_IFDIR) {
            return RemoteError::IsDir;
        }
        e
    }

    /// NotFound from ENOTDIR: an existing non-directory where a directory was expected.
    fn refine_not_dir(&self, rp: &str, e: RemoteError) -> RemoteError {
        if e == RemoteError::NotFound {
            if let Some(k) = self.cl.kind_of(rp) {
                if k != S_IFDIR {
                    return RemoteError::NotDir;
                }
            }
        }
        e
    }

    fn with_write_handle<T>(
        &self,
        rp: &str,
        pflags: u32,
        attrs: &Attrs,
        f: impl FnOnce(&Conn, &[u8]) -> RResult<T>,
    ) -> RResult<T> {
        let c = self.cl.conn()?;
        let h = self.cl.sess.open(&c, rp, pflags, attrs).map_err(|e| self.refine_file_op(rp, e))?;
        let res = f(&c, &h);
        match res {
            Ok(v) => {
                self.cl.sess.close(&c, &h)?;
                Ok(v)
            }
            Err(e) => {
                // After a timeout the session may live on: release the server-side handle.
                if !matches!(e, RemoteError::Disconnected(_)) {
                    self.cl.sess.close_nowait(&c, &h);
                }
                Err(e)
            }
        }
    }
}

impl RemoteBackend for SftpBackend {
    fn describe(&self) -> String {
        self.desc.clone()
    }

    fn set_timeout(&self, t: Duration) {
        self.cl.sess.set_timeout(t);
    }

    fn ping(&self) -> RResult<Duration> {
        let c = self.cl.conn()?;
        let t0 = Instant::now();
        let r = self.cl.sess.call_on(&c, FXP_REALPATH, &Buf::new().str(".").0)?;
        expect(&r, FXP_NAME)?;
        Ok(t0.elapsed())
    }

    fn stat(&self, path: &str) -> RResult<RemoteEntry> {
        let rp = self.rp(path)?;
        let a = self.cl.lstat_raw(&rp)?;
        Ok(entry_from(last_name(path), &a))
    }

    fn list(&self, path: &str) -> RResult<Vec<RemoteEntry>> {
        let rp = self.rp(path)?;
        let c = self.cl.conn()?;
        let r = self.cl.sess.call_on(&c, FXP_OPENDIR, &Buf::new().str(&rp).0)?;
        let h = match expect(&r, FXP_HANDLE).and_then(|mut rd| Ok(rd.bytes()?.to_vec())) {
            Ok(h) => h,
            Err(e) => return Err(self.refine_not_dir(&rp, e)),
        };
        let res = self.cl.sess.readdir_all(&c, &h);
        if !matches!(res, Err(RemoteError::Disconnected(_))) {
            self.cl.sess.close_nowait(&c, &h);
        }
        Ok(res?.into_iter().map(|(n, a)| entry_from(n, &a)).collect())
    }

    fn read(&self, path: &str, offset: u64, len: u32) -> RResult<Vec<u8>> {
        let rp = self.rp(path)?;
        let c = self.cl.conn()?;
        let h = self.cl.sess.open(&c, &rp, FXF_READ, &Attrs::default()).map_err(|e| self.refine_file_op(&rp, e))?;
        let res = self.cl.sess.read_at(&c, &h, offset, len as u64);
        if !matches!(res, Err(RemoteError::Disconnected(_))) {
            self.cl.sess.close_nowait(&c, &h);
        }
        res.map_err(|e| self.refine_file_op(&rp, e))
    }

    fn supports_partial_write(&self) -> bool {
        true
    }

    fn write(&self, path: &str, offset: u64, data: &[u8]) -> RResult<()> {
        let rp = self.rp(path)?;
        self.with_write_handle(&rp, FXF_WRITE, &Attrs::default(), |c, h| {
            self.cl.sess.write_at(c, h, offset, data)
        })
    }

    fn put_all(&self, path: &str, data: &[u8]) -> RResult<()> {
        let rp = self.rp(path)?;
        self.with_write_handle(&rp, FXF_WRITE | FXF_CREAT | FXF_TRUNC, &Attrs::default(), |c, h| {
            self.cl.sess.write_at(c, h, 0, data)
        })
    }

    fn create(&self, path: &str, perm: u16) -> RResult<()> {
        let rp = self.rp(path)?;
        let attrs = Attrs { perm: Some(perm as u32 & 0o7777), ..Default::default() };
        let c = self.cl.conn()?;
        let h = match self.cl.sess.open(&c, &rp, FXF_WRITE | FXF_CREAT | FXF_EXCL, &attrs) {
            Ok(h) => h,
            Err(e) if is_failure(&e) && self.cl.kind_of(&rp).is_some() => return Err(RemoteError::Exists),
            Err(e) => return Err(e),
        };
        // Apply the exact mode (the server's umask applied to the open), then close.
        let t1 = c.send(FXP_FSETSTAT, &Buf::new().bytes(&h).attrs(&attrs).0)?;
        let t2 = c.send(FXP_CLOSE, &Buf::new().bytes(&h).0)?;
        let r1 = self.cl.sess.wait(&c, &t1, self.cl.sess.timeout()).and_then(|r| expect_ok(&r));
        let r2 = self.cl.sess.wait(&c, &t2, self.cl.sess.timeout()).and_then(|r| expect_ok(&r));
        r1.and(r2)
    }

    fn truncate(&self, path: &str, size: u64) -> RResult<()> {
        let rp = self.rp(path)?;
        let attrs = Attrs { size: Some(size), ..Default::default() };
        self.cl
            .call_ok(FXP_SETSTAT, Buf::new().str(&rp).attrs(&attrs))
            .map_err(|e| self.refine_file_op(&rp, e))
    }

    fn mkdir(&self, path: &str, perm: u16) -> RResult<()> {
        let rp = self.rp(path)?;
        let attrs = Attrs { perm: Some(perm as u32 & 0o7777), ..Default::default() };
        match self.cl.call_ok(FXP_MKDIR, Buf::new().str(&rp).attrs(&attrs)) {
            Ok(()) => {}
            Err(e) if (is_failure(&e) || e == RemoteError::Perm) && self.cl.kind_of(&rp).is_some() => {
                return Err(RemoteError::Exists)
            }
            Err(e) => return Err(e),
        }
        // mkdir(2) applied the server's umask; set the requested mode exactly.
        self.cl.call_ok(FXP_SETSTAT, Buf::new().str(&rp).attrs(&attrs))
    }

    fn remove(&self, path: &str) -> RResult<()> {
        if is_root_path(path) {
            return Err(RemoteError::IsDir);
        }
        let rp = self.rp(path)?;
        self.cl.call_ok(FXP_REMOVE, Buf::new().str(&rp)).map_err(|e| self.refine_file_op(&rp, e))
    }

    fn rmdir(&self, path: &str) -> RResult<()> {
        if is_root_path(path) {
            return Err(RemoteError::Perm);
        }
        let rp = self.rp(path)?;
        match self.cl.call_ok(FXP_RMDIR, Buf::new().str(&rp)) {
            Ok(()) => Ok(()),
            Err(e) if is_failure(&e) || e == RemoteError::NotFound => match self.cl.kind_of(&rp) {
                Some(S_IFDIR) if e != RemoteError::NotFound => Err(RemoteError::NotEmpty),
                Some(k) if k != S_IFDIR => Err(RemoteError::NotDir),
                _ => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    fn rename(&self, from: &str, to: &str) -> RResult<()> {
        if is_root_path(from) || is_root_path(to) {
            return Err(RemoteError::Perm);
        }
        let (rf, rt) = (self.rp(from)?, self.rp(to)?);
        if rf == rt {
            return self.cl.lstat_raw(&rf).map(|_| ());
        }
        let c = self.cl.conn()?;
        let refine = |e: RemoteError| -> RemoteError {
            if !(is_failure(&e) || e == RemoteError::NotFound) {
                return e;
            }
            let (kf, kt) = (self.cl.kind_of(&rf), self.cl.kind_of(&rt));
            match (kf, kt) {
                (None, _) => RemoteError::NotFound,
                (Some(S_IFDIR), Some(S_IFDIR)) => RemoteError::NotEmpty,
                (Some(S_IFDIR), Some(_)) => RemoteError::NotDir,
                (Some(_), Some(S_IFDIR)) => RemoteError::IsDir,
                _ => e,
            }
        };
        if self.cl.ext(&c, "posix-rename@openssh.com") {
            let body = Buf::new().str("posix-rename@openssh.com").str(&rf).str(&rt);
            return expect_ok(&self.cl.sess.call_on(&c, FXP_EXTENDED, &body.0)?).map_err(refine);
        }
        // Plain v3: RENAME refuses an existing target; remove it first (not atomic).
        match expect_ok(&self.cl.sess.call_on(&c, FXP_RENAME, &Buf::new().str(&rf).str(&rt).0)?) {
            Ok(()) => return Ok(()),
            Err(e) if !is_failure(&e) => return Err(refine(e)),
            Err(e) => {
                let (kf, kt) = (self.cl.kind_of(&rf), self.cl.kind_of(&rt));
                match (kf, kt) {
                    (None, _) => return Err(RemoteError::NotFound),
                    (_, None) => return Err(e),
                    (Some(S_IFDIR), Some(S_IFDIR)) => self.rmdir(to)?,
                    (Some(S_IFDIR), Some(_)) => return Err(RemoteError::NotDir),
                    (Some(_), Some(S_IFDIR)) => return Err(RemoteError::IsDir),
                    _ => self.remove(to)?,
                }
            }
        }
        self.cl.call_ok(FXP_RENAME, Buf::new().str(&rf).str(&rt)).map_err(refine)
    }

    fn symlink(&self, path: &str, target: &str) -> RResult<()> {
        let rp = self.rp(path)?;
        // OpenSSH's sftp-server takes (target, linkpath) - swapped relative to the draft.
        match self.cl.call_ok(FXP_SYMLINK, Buf::new().str(target).str(&rp)) {
            Err(e) if is_failure(&e) && self.cl.kind_of(&rp).is_some() => Err(RemoteError::Exists),
            r => r,
        }
    }

    fn readlink(&self, path: &str) -> RResult<String> {
        let rp = self.rp(path)?;
        let r = self.cl.call(FXP_READLINK, Buf::new().str(&rp))?;
        match expect(&r, FXP_NAME) {
            Ok(mut rd) => {
                if rd.u32()? < 1 {
                    return Err(RemoteError::Other("sftp: empty READLINK reply".into()));
                }
                rd.string()
            }
            Err(e) if is_failure(&e) => match self.cl.kind_of(&rp) {
                Some(S_IFLNK) | None => Err(e),
                Some(_) => Err(RemoteError::Other("not a symbolic link".into())),
            },
            Err(e) => Err(e),
        }
    }

    fn set_perm(&self, path: &str, perm: u16) -> RResult<()> {
        let rp = self.rp(path)?;
        let attrs = Attrs { perm: Some(perm as u32 & 0o7777), ..Default::default() };
        self.cl.call_ok(FXP_SETSTAT, Buf::new().str(&rp).attrs(&attrs))
    }

    fn set_mtime(&self, path: &str, mtime: Timestamp) -> RResult<()> {
        let rp = self.rp(path)?;
        // v3 sets atime and mtime together: keep the current atime.
        let cur = self.cl.stat_raw(&rp)?;
        let m = to_u32_secs(mtime.secs);
        let atime = cur.times.map(|t| t.0).unwrap_or(m);
        let attrs = Attrs { times: Some((atime, m)), ..Default::default() };
        self.cl.call_ok(FXP_SETSTAT, Buf::new().str(&rp).attrs(&attrs))
    }

    fn statfs(&self) -> RResult<Option<(u64, u64)>> {
        let c = self.cl.conn()?;
        if !self.cl.ext(&c, "statvfs@openssh.com") {
            return Ok(None);
        }
        let body = Buf::new().str("statvfs@openssh.com").str(&self.root);
        let r = self.cl.sess.call_on(&c, FXP_EXTENDED, &body.0)?;
        let mut rd = expect(&r, FXP_EXTENDED_REPLY)?;
        let _bsize = rd.u64()?;
        let frsize = rd.u64()?;
        let blocks = rd.u64()?;
        let _bfree = rd.u64()?;
        let bavail = rd.u64()?;
        Ok(Some((blocks.saturating_mul(frsize), bavail.saturating_mul(frsize))))
    }
}

// ---------------------------------------------------------------------------------------------

/// Random-access reader for a remote disk image (`cfg.root` is the file path). Keeps one
/// read handle open per ssh connection and reopens it after a reconnect.
pub struct SftpRangeSource {
    cl: Client,
    path: String,
    size: u64,
    desc: String,
    handle: Mutex<Option<(u64, Arc<Vec<u8>>)>>,
}

fn io_err(e: RemoteError) -> std::io::Error {
    use std::io::ErrorKind as K;
    let kind = match &e {
        RemoteError::NotFound => K::NotFound,
        RemoteError::Perm => K::PermissionDenied,
        RemoteError::Timeout => K::TimedOut,
        RemoteError::Disconnected(_) => K::ConnectionAborted,
        RemoteError::Unsupported(_) => K::Unsupported,
        _ => K::Other,
    };
    std::io::Error::new(kind, format!("sftp: {e}"))
}

impl SftpRangeSource {
    pub fn open(cfg: SftpConfig) -> RResult<Self> {
        let (cl, path) = Client::connect(cfg)?;
        let a = cl.stat_raw(&path)?;
        match a.perm.map(|p| p & S_IFMT) {
            Some(S_IFDIR) => return Err(RemoteError::IsDir),
            Some(S_IFREG) | None => {}
            Some(_) => return Err(RemoteError::Other(format!("{path} is not a regular file"))),
        }
        let size = a.size.ok_or_else(|| RemoteError::Other("server did not report the file size".into()))?;
        let desc = cl.cfg.display(&path);
        let s = SftpRangeSource { cl, path, size, desc, handle: Mutex::new(None) };
        let c = s.cl.conn()?;
        s.handle_for(&c)?;
        Ok(s)
    }

    pub fn set_timeout(&self, t: Duration) {
        self.cl.sess.set_timeout(t);
    }

    fn handle_for(&self, c: &Conn) -> RResult<Arc<Vec<u8>>> {
        if let Some((g, h)) = &*self.handle.lock().unwrap() {
            if *g == c.gen {
                return Ok(h.clone());
            }
        }
        let h = Arc::new(self.cl.sess.open(c, &self.path, FXF_READ, &Attrs::default())?);
        let mut g = self.handle.lock().unwrap();
        if let Some((og, oh)) = g.as_ref() {
            if *og == c.gen {
                // Another thread won the race; keep its handle.
                self.cl.sess.close_nowait(c, &h);
                return Ok(oh.clone());
            }
        }
        *g = Some((c.gen, h.clone()));
        Ok(h)
    }

    fn fetch_once(&self, offset: u64, len: usize) -> RResult<Vec<u8>> {
        let c = self.cl.conn()?;
        let h = self.handle_for(&c)?;
        self.cl.sess.read_at(&c, &h, offset, len as u64)
    }
}

impl crate::block::RangeSource for SftpRangeSource {
    fn describe(&self) -> String {
        self.desc.clone()
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn fetch(&self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let mut res = self.fetch_once(offset, len);
        if let Err(RemoteError::Disconnected(_)) = res {
            res = self.fetch_once(offset, len); // one transparent reconnect
        }
        let d = res.map_err(io_err)?;
        if d.len() != len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("sftp: short read at {offset}: {} of {len} bytes", d.len()),
            ));
        }
        Ok(d)
    }
}

impl Drop for SftpRangeSource {
    fn drop(&mut self) {
        if let Some((g, h)) = self.handle.lock().unwrap().take() {
            if let Ok(c) = self.cl.conn_if_live() {
                if c.gen == g {
                    self.cl.sess.close_nowait(&c, &h);
                }
            }
        }
    }
}

impl Client {
    /// The current connection if it is up (never spawns ssh).
    fn conn_if_live(&self) -> RResult<Arc<Conn>> {
        self.sess.current().ok_or(RemoteError::Disconnected("not connected".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let c = SftpConfig::from_url("sftp://pi@raspberrypi.local:2222/srv/data/").unwrap();
        assert_eq!((c.host.as_str(), c.port, c.user.as_deref(), c.root.as_str()), ("raspberrypi.local", 2222, Some("pi"), "/srv/data"));
        assert!(c.password.is_none());
        let c = SftpConfig::from_url("sftp://nas").unwrap();
        assert_eq!((c.port, c.user.clone(), c.root.as_str()), (22, None, "."));
        let c = SftpConfig::from_url("sftp://u:p%40ss@[::1]:23/~/Music/").unwrap();
        assert_eq!((c.host.as_str(), c.port, c.password.as_deref(), c.root.as_str()), ("::1", 23, Some("p@ss"), "Music"));
        let c = SftpConfig::from_url("SFTP://a%20b@h/").unwrap_err();
        assert!(matches!(c, RemoteError::Other(_)));
        assert_eq!(SftpConfig::from_url("sftp://h/~").unwrap().root, ".");
        assert_eq!(SftpConfig::from_url("sftp://h/").unwrap().root, "/");
        assert_eq!(SftpConfig::from_url("sftp://h/a//b/./c%20d").unwrap().root, "/a/b/c d");
        assert!(SftpConfig::from_url("sftp://h:0/").is_err());
        assert!(SftpConfig::from_url("sftp://h:x/").is_err());
        assert!(SftpConfig::from_url("http://h/").is_err());
        assert!(SftpConfig::from_url("sftp://-oProxyCommand=x/").is_err());
        let d = SftpConfig::from_url("sftp://pi@h/srv").unwrap().display("/srv");
        assert_eq!(d, "sftp://pi@h:22/srv");
        assert!(!format!("{:?}", SftpConfig::from_url("sftp://u:secret@h").unwrap()).contains("secret"));
    }

    #[test]
    fn path_mapping() {
        assert_eq!(map_path("/home/pi", "/").unwrap(), "/home/pi");
        assert_eq!(map_path("/home/pi", "/a/b.txt").unwrap(), "/home/pi/a/b.txt");
        assert_eq!(map_path("/", "/a//./b").unwrap(), "/a/b");
        assert_eq!(map_path("/", "/").unwrap(), "/");
        assert_eq!(map_path("/", "").unwrap(), "/");
        assert_eq!(map_path("/r", "/a/../../etc").unwrap_err(), RemoteError::Perm);
        assert_eq!(map_path("/r", "..").unwrap_err(), RemoteError::Perm);
        assert!(is_root_path("/") && is_root_path("/./") && !is_root_path("/a"));
        assert_eq!(last_name("/a/b/"), "b");
        assert_eq!(last_name("/"), "");
    }
}
