//! LAN server: exposes one mounted volume on the local network.
//!
//! * [`LanMode::Share`] — token-protected WebDAV (see `dav.rs`), consumed by another Mac
//!   running LumenDisk (`remote::webdav`).
//! * [`LanMode::Portal`] — PIN-protected "Drop-to-Disk" web page (see `portal.rs`) so a phone
//!   on the same Wi-Fi can upload to / download from the external drive directly.
//!
//! The volume is the same `Arc<Mutex<S>>` the NFS server uses: the mutex is only held for one
//! filesystem call (or one 256 KiB chunk) at a time, and bodies are streamed in both
//! directions — nothing is ever buffered whole in RAM.
//!
//! Transport (see `tls.rs`): every start generates an ephemeral self-signed ECDSA P-256
//! certificate; [`LanServer::fingerprint`] is its SHA-256, to be pinned by the client
//! ([`pinned_agent`]).
//! * Share: HTTPS only, TLS 1.3 only, on [`LanServer::port`].
//! * Portal: HTTPS (TLS 1.2+) on [`LanServer::port`]. A plain-HTTP compatibility listener
//!   for phones that refuse self-signed certificates exists only while explicitly enabled
//!   ([`LanServer::set_plain_http`], off by default: over HTTP the PIN and the files travel in
//!   clear). Same PIN, same rate limit; sessions are bound to the scheme they were created on
//!   (see `portal.rs`) and HTTP sessions end when HTTP is switched off. No HSTS is ever sent:
//!   it is host-wide and would break the opt-in HTTP fallback.
//!
//! Internally, TLS is terminated by `tls.rs` and relayed to a tiny_http listener on
//! 127.0.0.1; the opt-in plain-HTTP portal listener is a second tiny_http server on 0.0.0.0.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::vfs::FileSystem;

mod dav;
mod http;
mod portal;
mod tls;

pub use tls::{learn_fingerprint, pinned_agent, pinned_agent_builder, pinned_client_config};

/// How the volume is exposed.
pub enum LanMode {
    /// WebDAV for another LumenDisk; every request needs `Authorization: Bearer <token>`.
    Share { token: String },
    /// Browser "Drop-to-Disk" portal protected by a numeric PIN.
    Portal { pin: String },
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct LanStats {
    /// Distinct client IPs seen in the last 60 s.
    pub clients: u32,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// Completed whole-file uploads.
    pub uploads: u64,
    /// Completed whole-file downloads.
    pub downloads: u64,
}

/// Chunk size for reads/writes done under the volume lock.
pub(crate) const CHUNK: usize = 256 * 1024;
/// Upper bound on concurrently handled requests (each runs on its own thread).
const MAX_INFLIGHT: usize = 64;

// ---------------------------------------------------------------------------------------------
// Type-erased access to the shared volume.

pub(crate) trait Volume: Send + Sync {
    fn run(&self, f: &mut dyn FnMut(&mut dyn FileSystem));
}

impl<S: FileSystem + Send + 'static> Volume for Mutex<S> {
    fn run(&self, f: &mut dyn FnMut(&mut dyn FileSystem)) {
        let mut g = self.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut *g)
    }
}

pub(crate) enum Auth {
    Token(String),
    Pin(String),
}

pub(crate) struct Ctx {
    vol: Arc<dyn Volume>,
    /// SHA-256 of our TLS certificate (bound into the pairing proof).
    pub fingerprint: String,
    pub auth: Auth,
    pub name: String,
    pub read_only: bool,
    pub stopped: AtomicBool,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub uploads: AtomicU64,
    pub downloads: AtomicU64,
    inflight: AtomicUsize,
    seen: Mutex<HashMap<IpAddr, Instant>>,
    /// Portal sessions: id -> (creation time, created over HTTPS).
    pub sessions: Mutex<HashMap<String, (Instant, bool)>>,
    /// Portal login failures per IP.
    pub failures: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    /// TLS relay: local port of the loopback connection to the internal listener -> real client.
    peers: Mutex<HashMap<u16, SocketAddr>>,
    /// Open TLS connections (relay threads).
    conns: AtomicUsize,
}

impl Ctx {
    /// Run `f` with the volume locked. Keep `f` short (one call / one chunk).
    pub fn fs<R>(&self, f: impl FnOnce(&mut dyn FileSystem) -> R) -> R {
        let mut f = Some(f);
        let mut out = None;
        self.vol.run(&mut |fs| {
            if let Some(f) = f.take() {
                out = Some(f(fs));
            }
        });
        out.expect("volume callback not run")
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    fn touch_client(&self, ip: IpAddr) {
        let mut m = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        m.insert(ip, Instant::now());
        if m.len() > 256 {
            m.retain(|_, t| t.elapsed() < Duration::from_secs(60));
        }
    }

    fn stats(&self) -> LanStats {
        let clients = {
            let mut m = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            m.retain(|_, t| t.elapsed() < Duration::from_secs(60));
            m.len() as u32
        };
        LanStats {
            clients,
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            uploads: self.uploads.load(Ordering::Relaxed),
            downloads: self.downloads.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------------------------

pub struct LanServer {
    ctx: Arc<Ctx>,
    port: u16,
    /// The portal's unencrypted compatibility listener, only while explicitly enabled.
    plain: Mutex<Option<PlainListener>>,
    fingerprint: String,
    /// tiny_http servers (internal loopback one, plus the portal's plain-HTTP one) and their ports.
    servers: Mutex<Vec<(Arc<tiny_http::Server>, u16)>>,
    /// Accept loops (tiny_http dispatchers and the TLS front).
    threads: Mutex<Vec<JoinHandle<()>>>,
}

struct PlainListener {
    server: Arc<tiny_http::Server>,
    port: u16,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// Which listener a request came through.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Via {
    /// Internal loopback listener fed by the TLS relay.
    Tls,
    /// Public plain-HTTP listener (portal compatibility mode).
    Plain,
}

impl LanServer {
    /// Bind 0.0.0.0:0 (all interfaces, ephemeral port), serve on background threads.
    /// Both modes serve HTTPS (TLS 1.3) only. In Portal mode an unencrypted HTTP listener can
    /// be added later with [`LanServer::set_plain_http`], as an explicit insecure opt-in.
    pub fn start<S: FileSystem + Send + 'static>(
        fs: Arc<Mutex<S>>,
        mode: LanMode,
        volume_name: &str,
        read_only: bool,
    ) -> std::io::Result<LanServer> {
        let vol_ro = fs.lock().unwrap_or_else(|e| e.into_inner()).volume_info().read_only;
        let auth = match mode {
            LanMode::Share { token } => {
                if token.is_empty() {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty share token"));
                }
                Auth::Token(token)
            }
            LanMode::Portal { pin } => {
                if pin.is_empty() {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty portal PIN"));
                }
                Auth::Pin(pin)
            }
        };
        let is_share = matches!(auth, Auth::Token(_));
        let identity = tls::generate_identity()?;
        let tls_cfg = tls::server_config(&identity, is_share)?;

        let vol: Arc<dyn Volume> = fs;
        let ctx = Arc::new(Ctx {
            vol,
            fingerprint: identity.fingerprint.clone(),
            auth,
            name: volume_name.to_string(),
            read_only: read_only || vol_ro,
            stopped: AtomicBool::new(false),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            uploads: AtomicU64::new(0),
            downloads: AtomicU64::new(0),
            inflight: AtomicUsize::new(0),
            seen: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            peers: Mutex::new(HashMap::new()),
            conns: AtomicUsize::new(0),
        });
        // From here on, an early `?` drops `srv`, whose Drop stops whatever was started.
        let mut srv = LanServer {
            ctx: ctx.clone(),
            port: 0,
            plain: Mutex::new(None),
            fingerprint: identity.fingerprint.clone(),
            servers: Mutex::new(Vec::new()),
            threads: Mutex::new(Vec::new()),
        };

        // Internal plaintext listener, reachable only through the TLS relay.
        let internal = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let internal_addr = internal.local_addr()?;
        srv.spawn_http(internal, Via::Tls)?;

        // Public TLS port.
        let public = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        srv.port = public.local_addr()?.port();
        let c = ctx.clone();
        let front = std::thread::Builder::new()
            .name(format!("lan-tls-accept-{}", srv.port))
            .spawn(move || tls::front_loop(public, tls_cfg, internal_addr, c))?;
        srv.threads.lock().unwrap_or_else(|e| e.into_inner()).push(front);
        Ok(srv)
    }

    /// Portal only: open (`true`) or close (`false`) the unencrypted HTTP compatibility
    /// listener. Off by default. Over HTTP the PIN, the file list and every transferred file
    /// cross the network in clear, so this is an explicit, insecure opt-in for phones that
    /// can't get past the self-signed certificate. Closing it also ends every session that was
    /// created over HTTP. Returns the HTTP port while enabled.
    pub fn set_plain_http(&self, enabled: bool) -> std::io::Result<Option<u16>> {
        if !matches!(self.ctx.auth, Auth::Pin(_)) {
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "plain HTTP is only available for the portal"));
        }
        let mut g = self.plain.lock().unwrap_or_else(|e| e.into_inner());
        if enabled {
            if let Some(p) = g.as_ref() {
                return Ok(Some(p.port));
            }
            if self.ctx.is_stopped() {
                return Err(std::io::Error::other("server stopped"));
            }
            let listener = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
            let port = listener.local_addr()?.port();
            let server = Arc::new(tiny_http::Server::from_listener(listener, None).map_err(|e| std::io::Error::other(e.to_string()))?);
            let stop = Arc::new(AtomicBool::new(false));
            let (sv, c, st) = (server.clone(), self.ctx.clone(), stop.clone());
            let thread = std::thread::Builder::new()
                .name(format!("lan-plain-{port}"))
                .spawn(move || accept_loop(sv, c, Via::Plain, Some(st)))?;
            *g = Some(PlainListener { server, port, stop, thread });
            Ok(Some(port))
        } else {
            if let Some(p) = g.take() {
                close_listener(p);
            }
            // Sessions that travelled in clear die with the listener.
            self.ctx.sessions.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, (_, secure)| *secure);
            Ok(None)
        }
    }

    fn spawn_http(&self, listener: std::net::TcpListener, via: Via) -> std::io::Result<()> {
        let port = listener.local_addr()?.port();
        let server = tiny_http::Server::from_listener(listener, None).map_err(|e| std::io::Error::other(e.to_string()))?;
        let server = Arc::new(server);
        self.servers.lock().unwrap_or_else(|e| e.into_inner()).push((server.clone(), port));
        let c = self.ctx.clone();
        let h = std::thread::Builder::new().name(format!("lan-accept-{port}")).spawn(move || accept_loop(server, c, via, None))?;
        self.threads.lock().unwrap_or_else(|e| e.into_inner()).push(h);
        Ok(())
    }

    /// HTTPS port (both modes).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Plain-HTTP compatibility port: `Some` only while enabled with [`Self::set_plain_http`].
    pub fn http_port(&self) -> Option<u16> {
        self.plain.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|p| p.port)
    }

    /// Lowercase hex SHA-256 of this run's certificate (DER). Stable until the server stops;
    /// a new certificate is generated on every start.
    pub fn fingerprint(&self) -> String {
        self.fingerprint.clone()
    }

    pub fn stats(&self) -> LanStats {
        self.ctx.stats()
    }

    /// Stop serving: in-flight transfers are aborted at their next chunk and the listening
    /// sockets are closed before this returns.
    pub fn stop(&self) {
        self.ctx.stopped.store(true, Ordering::SeqCst);
        if let Some(p) = self.plain.lock().unwrap_or_else(|e| e.into_inner()).take() {
            close_listener(p);
        }
        let servers = std::mem::take(&mut *self.servers.lock().unwrap_or_else(|e| e.into_inner()));
        for (s, _) in &servers {
            s.unblock();
        }
        // Accept loops notice `stopped` within ~150 ms; the TLS front drops its listener on exit.
        let threads = std::mem::take(&mut *self.threads.lock().unwrap_or_else(|e| e.into_inner()));
        for h in threads {
            let _ = h.join();
        }
        // TLS relay threads exit within one poll tick.
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.ctx.conns.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        for (s, port) in servers {
            // Last reference: tiny_http's Drop wakes its accept thread, which drops the listener.
            drop(s);
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                match TcpStream::connect_timeout(&addr, Duration::from_millis(100)) {
                    Ok(s) => {
                        drop(s);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

impl Drop for LanServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Stop one listener's accept loop and make sure its port is closed before returning.
fn close_listener(p: PlainListener) {
    p.stop.store(true, Ordering::SeqCst);
    p.server.unblock();
    let _ = p.thread.join();
    drop(p.server);
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, p.port));
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(100)) {
            Ok(s) => {
                drop(s);
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

fn accept_loop(server: Arc<tiny_http::Server>, ctx: Arc<Ctx>, via: Via, stop: Option<Arc<AtomicBool>>) {
    let halted = || ctx.is_stopped() || stop.as_ref().is_some_and(|s| s.load(Ordering::SeqCst));
    while !halted() {
        let rq = match server.recv_timeout(Duration::from_millis(150)) {
            Ok(Some(rq)) => rq,
            Ok(None) => continue,
            Err(_) => {
                if halted() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        if halted() {
            let _ = rq.respond(http::text(503, "server stopped"));
            break;
        }
        if ctx.inflight.fetch_add(1, Ordering::SeqCst) >= MAX_INFLIGHT {
            ctx.inflight.fetch_sub(1, Ordering::SeqCst);
            let _ = rq.respond(http::text(503, "too many requests"));
            continue;
        }
        let c = ctx.clone();
        let spawned = std::thread::Builder::new().name("lan-req".into()).spawn(move || {
            handle(&c, rq, via);
            c.inflight.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            ctx.inflight.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn handle(ctx: &Arc<Ctx>, mut rq: tiny_http::Request, via: Via) {
    let peer = match via {
        // Only connections opened by our TLS relay are served on the internal listener; their
        // loopback source port maps to the real client.
        Via::Tls => {
            let mapped = rq
                .remote_addr()
                .filter(|a| a.ip().is_loopback())
                .and_then(|a| ctx.peers.lock().unwrap_or_else(|e| e.into_inner()).get(&a.port()).copied());
            match mapped {
                Some(p) => Some(p),
                None => {
                    let _ = rq.respond(http::text(403, "forbidden"));
                    return;
                }
            }
        }
        Via::Plain => rq.remote_addr().copied(),
    };
    if let Some(a) = peer {
        ctx.touch_client(a.ip());
    }
    let resp = match &ctx.auth {
        Auth::Token(t) => dav::handle(ctx, t, &mut rq),
        Auth::Pin(p) => portal::handle(ctx, p, &mut rq, peer.map(|a| a.ip()), via == Via::Tls),
    };
    let _ = rq.respond(resp);
}

// ---------------------------------------------------------------------------------------------
// Helpers exposed to the app.

/// Fill `buf` with cryptographically secure random bytes.
pub(crate) fn random_bytes(buf: &mut [u8]) {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
    {
        // SAFETY: valid, writable buffer of the given length.
        unsafe { libc::arc4random_buf(buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd")))]
    {
        for c in buf.chunks_mut(16) {
            let u = uuid::Uuid::new_v4();
            let b = u.as_bytes();
            c.copy_from_slice(&b[..c.len()]);
        }
    }
}

pub(crate) fn random_hex(nbytes: usize) -> String {
    let mut b = vec![0u8; nbytes];
    random_bytes(&mut b);
    hex(&b)
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Constant-time (for equal lengths) comparison of secrets.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// IPv4 addresses of active non-loopback interfaces (getifaddrs), Wi-Fi/Ethernet first.
pub fn lan_addresses() -> Vec<String> {
    let mut found: Vec<(u32, String, Ipv4Addr)> = Vec::new();
    // SAFETY: standard getifaddrs/freeifaddrs usage; every pointer is checked before use.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return Vec::new();
        }
        let mut p = head;
        while !p.is_null() {
            let ifa = &*p;
            p = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let flags = ifa.ifa_flags as i32;
            if flags & libc::IFF_UP == 0 || flags & libc::IFF_RUNNING == 0 || flags & libc::IFF_LOOPBACK != 0 {
                continue;
            }
            let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if ip.is_loopback() || ip.is_unspecified() {
                continue;
            }
            let name = if ifa.ifa_name.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned()
            };
            let mut rank = if name == "en0" {
                0
            } else if name.starts_with("en") {
                1
            } else if name.starts_with("bridge") {
                3
            } else if name.starts_with("utun") || name.starts_with("ipsec") || name.starts_with("ppp") || flags & libc::IFF_POINTOPOINT != 0 {
                4
            } else {
                2
            };
            if ip.is_link_local() {
                rank += 10;
            }
            found.push((rank, name, ip));
        }
        libc::freeifaddrs(head);
    }
    found.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let mut out: Vec<String> = Vec::new();
    for (_, _, ip) in found {
        let s = ip.to_string();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Unambiguous alphabet: no 0/O/1/I.
const PAIR_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// 8 chars from an unambiguous alphabet (no 0/O/1/I), formatted "ABCD-EFGH".
pub fn new_pairing_code() -> String {
    let mut b = [0u8; 8];
    random_bytes(&mut b);
    let mut s = String::with_capacity(9);
    for (i, x) in b.iter().enumerate() {
        if i == 4 {
            s.push('-');
        }
        // 256 is a multiple of 32: no modulo bias.
        s.push(PAIR_ALPHABET[(*x as usize) % 32] as char);
    }
    s
}

/// sha256("lumendisk-pair-v1:" + code uppercased with '-' and spaces removed), lowercase hex.
pub fn token_from_pairing_code(code: &str) -> String {
    let norm: String = code.chars().filter(|c| *c != '-' && !c.is_whitespace()).flat_map(|c| c.to_uppercase()).collect();
    let mut h = Sha256::new();
    h.update(b"lumendisk-pair-v1:");
    h.update(norm.as_bytes());
    hex(&h.finalize())
}

/// 6 random digits.
pub fn new_pin() -> String {
    let mut s = String::with_capacity(6);
    while s.len() < 6 {
        let mut b = [0u8; 16];
        random_bytes(&mut b);
        for x in b {
            // Rejection sampling: 250 = 25 * 10, so values below it are unbiased.
            if x < 250 && s.len() < 6 {
                s.push((b'0' + x % 10) as char);
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_shape() {
        for _ in 0..200 {
            let c = new_pairing_code();
            assert_eq!(c.len(), 9);
            assert_eq!(&c[4..5], "-");
            assert!(c.chars().filter(|c| *c != '-').all(|c| PAIR_ALPHABET.contains(&(c as u8))));
            assert!(!c.contains('0') && !c.contains('O') && !c.contains('1') && !c.contains('I'));
        }
    }

    #[test]
    fn pin_shape() {
        for _ in 0..200 {
            let p = new_pin();
            assert_eq!(p.len(), 6);
            assert!(p.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn ct_eq_works() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"", b"a"));
        assert!(ct_eq(b"", b""));
    }
}

/// HMAC-SHA256 (RFC 2104), lowercase hex.
pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = Sha256::new().chain_update(ipad).chain_update(msg).finalize();
    let out = Sha256::new().chain_update(opad).chain_update(inner).finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// Pairing proof: the share answers `GET /.lumendisk/proof?nonce=<hex>` with
/// HMAC(token, nonce ‖ fingerprint). A client that pinned `fingerprint` (from Bonjour) checks it
/// *before* sending its bearer token, so an impostor advertising its own certificate can't
/// harvest the token: it doesn't know the pairing code.
pub fn pairing_proof(token: &str, nonce: &str, fingerprint: &str) -> String {
    hmac_sha256_hex(token.as_bytes(), format!("{nonce}|{}", fingerprint.to_ascii_lowercase().replace(':', "")).as_bytes())
}

#[cfg(test)]
mod hmac_tests {
    #[test]
    fn rfc4231_case_2() {
        assert_eq!(
            super::hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}

/// Constant-time comparison (public for the WebDAV client).
pub fn ct_eq_pub(a: &[u8], b: &[u8]) -> bool {
    ct_eq(a, b)
}
