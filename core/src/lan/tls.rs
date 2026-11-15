//! TLS for the LAN server, and the certificate-pinning client used to talk to it.
//!
//! # Server side
//! Every [`super::LanServer::start`] generates a fresh ECDSA P-256 key and a self-signed
//! certificate (CN "LumenDisk", SANs = [`super::lan_addresses`] + 127.0.0.1 + "localhost",
//! serverAuth EKU, ~13 months validity so Apple's TLS stack accepts it after a click-through).
//! Nothing is written to disk; the identity of a running server is the SHA-256 of its
//! certificate DER ([`super::LanServer::fingerprint`]), which must reach the client out of band
//! (shown in the app next to the pairing code).
//!
//! tiny_http only speaks rustls 0.20 and cannot be restricted to TLS 1.3, so TLS is terminated
//! here instead: the public port is a plain `TcpListener` whose connections are each handled by
//! their own thread (handshake included, so a slow handshake never blocks anybody else). After
//! the handshake the thread opens a loopback connection to an internal tiny_http listener on
//! 127.0.0.1 and relays plaintext both ways. The internal connection's local port is recorded in
//! `Ctx::peers` so request handlers still see the real client address; requests arriving on the
//! internal listener from any other local socket are refused.
//!
//! # Client side
//! [`pinned_agent`] / [`pinned_agent_builder`] accept exactly one server certificate: the one
//! whose leaf DER hashes to the given fingerprint. CA chain, validity dates and host name are
//! ignored (the certificate is self-signed and the address is whatever DHCP handed out), but the
//! handshake signature is still verified with the provider's algorithms, so a peer must hold the
//! private key of the pinned certificate.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, ServerConnection, SignatureScheme};
use sha2::{Digest, Sha256};

use super::{ct_eq, hex, random_bytes, Ctx};

/// A TLS handshake must finish within this time or the connection is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Upper bound on concurrently open TLS connections (one thread each).
const MAX_CONNS: usize = 256;
/// Plaintext buffered towards the internal listener before we stop reading from the client.
const UP_LIMIT: usize = 256 * 1024;
/// Chunk read from the internal listener and encrypted in one go.
const DOWN_CHUNK: usize = 64 * 1024;
/// How often blocked relay / accept loops re-check `Ctx::stopped`.
const TICK_MS: i32 = 100;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("TLS: {e}"))
}

// ---------------------------------------------------------------------------------------------
// Certificate

pub(crate) struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    /// Lowercase hex SHA-256 of `cert`.
    pub fingerprint: String,
}

/// Fresh ECDSA P-256 key + self-signed certificate for this server run.
pub(crate) fn generate_identity() -> io::Result<Identity> {
    let mut sans = super::lan_addresses();
    for extra in ["127.0.0.1", "localhost"] {
        if !sans.iter().any(|s| s == extra) {
            sans.push(extra.to_string());
        }
    }
    let mut params = rcgen::CertificateParams::new(sans).map_err(tls_err)?;
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "LumenDisk");
    params.distinguished_name = dn;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let epoch = rcgen::date_time_ymd(1970, 1, 1);
    // Backdated a day for clock skew; 397 days is within every browser's leaf-validity cap.
    params.not_before = epoch + Duration::from_secs(now.saturating_sub(86_400));
    params.not_after = epoch + Duration::from_secs(now + 397 * 86_400);
    let mut serial = [0u8; 16];
    random_bytes(&mut serial);
    serial[0] &= 0x7f; // positive INTEGER
    serial[0] |= 0x01; // no leading zero byte
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];

    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(tls_err)?;
    let cert = params.self_signed(&key).map_err(tls_err)?;
    let cert = cert.der().clone();
    let fingerprint = hex(&Sha256::digest(cert.as_ref()));
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    Ok(Identity { cert, key, fingerprint })
}

/// Server config: TLS 1.3 only (`tls13_only`, Share) or TLS 1.2 + 1.3 (Portal, older phones).
pub(crate) fn server_config(id: &Identity, tls13_only: bool) -> io::Result<Arc<rustls::ServerConfig>> {
    let versions: &[&'static rustls::SupportedProtocolVersion] =
        if tls13_only { &[&rustls::version::TLS13] } else { &[&rustls::version::TLS13, &rustls::version::TLS12] };
    let cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)
        .map_err(tls_err)?
        .with_no_client_auth()
        .with_single_cert(vec![id.cert.clone()], id.key.clone_key())
        .map_err(tls_err)?;
    Ok(Arc::new(cfg))
}

// ---------------------------------------------------------------------------------------------
// Pinning client

/// Accepts exactly one leaf certificate (by SHA-256), ignoring CA chain, dates and host name.
#[derive(Debug)]
struct PinnedVerifier {
    /// `None` when the fingerprint given was malformed: then every server is rejected.
    pin: Option<[u8; 32]>,
    provider: Arc<CryptoProvider>,
}

fn parse_fingerprint(s: &str) -> Option<[u8; 32]> {
    let h: Vec<u8> = s.bytes().filter(|b| !matches!(b, b':' | b' ' | b'\t' | b'\n' | b'\r')).collect();
    if h.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in h.chunks(2).enumerate() {
        let s = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = Sha256::digest(end_entity.as_ref());
        match &self.pin {
            Some(pin) if ct_eq(got.as_slice(), pin) => Ok(ServerCertVerified::assertion()),
            _ => Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// rustls client config that trusts only the certificate with this SHA-256 fingerprint
/// (64 hex digits, case-insensitive, ':' separators allowed). TLS 1.3 only, like the share.
/// A malformed fingerprint yields a config that rejects every server.
pub fn pinned_client_config(fingerprint_hex: &str) -> Arc<rustls::ClientConfig> {
    let provider = provider();
    let verifier = Arc::new(PinnedVerifier { pin: parse_fingerprint(fingerprint_hex), provider: provider.clone() });
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("ring provider supports TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Arc::new(cfg)
}

/// `ureq` agent builder pinned to one LumenDisk server certificate; set timeouts etc. on it.
pub fn pinned_agent_builder(fingerprint_hex: &str) -> ureq::AgentBuilder {
    ureq::AgentBuilder::new().tls_config(pinned_client_config(fingerprint_hex))
}

/// `ureq` agent that connects only to the server whose leaf certificate hashes to
/// `fingerprint_hex` (see [`pinned_client_config`]); everything else is a transport error.
pub fn pinned_agent(fingerprint_hex: &str) -> ureq::Agent {
    pinned_agent_builder(fingerprint_hex).build()
}

// ---------------------------------------------------------------------------------------------
// TLS front: accept, handshake, relay to the internal tiny_http listener.

/// Wait for `events` on up to two fds. A negative fd is ignored. Returns revents per fd.
fn poll(fds: &[(i32, i16)], timeout_ms: i32) -> [i16; 2] {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&(fd, events)| libc::pollfd { fd, events, revents: 0 }).collect();
    // SAFETY: valid array of pollfd of the given length.
    let r = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout_ms) };
    let mut out = [0i16; 2];
    if r > 0 {
        for (o, x) in out.iter_mut().zip(&p) {
            *o = x.revents;
        }
    }
    out
}

fn no_sigpipe(s: &TcpStream) {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let one: libc::c_int = 1;
        // SAFETY: valid socket fd and option buffer.
        unsafe {
            libc::setsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let _ = s;
}

/// Accept loop for the public TLS port. Exits (dropping the listener) within ~TICK_MS of stop.
pub(crate) fn front_loop(listener: TcpListener, cfg: Arc<rustls::ServerConfig>, backend: SocketAddr, ctx: Arc<Ctx>) {
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    let fd = listener.as_raw_fd();
    while !ctx.is_stopped() {
        if poll(&[(fd, libc::POLLIN)], TICK_MS)[0] == 0 {
            continue;
        }
        loop {
            match listener.accept() {
                Ok((sock, peer)) => {
                    if ctx.is_stopped() {
                        break;
                    }
                    if ctx.conns.fetch_add(1, Ordering::SeqCst) >= MAX_CONNS {
                        ctx.conns.fetch_sub(1, Ordering::SeqCst);
                        continue; // drops (closes) the socket
                    }
                    let (c, cfg) = (ctx.clone(), cfg.clone());
                    let spawned = std::thread::Builder::new().name("lan-tls".into()).spawn(move || {
                        serve_conn(sock, peer, cfg, backend, &c);
                        c.conns.fetch_sub(1, Ordering::SeqCst);
                    });
                    if spawned.is_err() {
                        ctx.conns.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(20));
                    break;
                }
            }
        }
    }
}

/// Removes the loopback-port → client mapping when the relay ends.
struct PeerGuard<'a> {
    ctx: &'a Ctx,
    port: u16,
}

impl Drop for PeerGuard<'_> {
    fn drop(&mut self) {
        self.ctx.peers.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.port);
    }
}

fn is_would_block(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
}

/// Write pending TLS records; Ok(true) if anything was written. Err = client gone.
fn flush_tls(conn: &mut ServerConnection, tcp: &mut TcpStream) -> io::Result<bool> {
    let mut any = false;
    while conn.wants_write() {
        match conn.write_tls(tcp) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(_) => any = true,
            Err(e) if is_would_block(&e) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(any)
}

fn serve_conn(mut tcp: TcpStream, peer: SocketAddr, cfg: Arc<rustls::ServerConfig>, backend_addr: SocketAddr, ctx: &Ctx) {
    no_sigpipe(&tcp);
    let _ = tcp.set_nodelay(true);
    if tcp.set_nonblocking(true).is_err() {
        return;
    }
    let Ok(mut conn) = ServerConnection::new(cfg) else { return };
    let tfd = tcp.as_raw_fd();

    // --- Handshake (this thread only; other connections are unaffected).
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while conn.is_handshaking() {
        if ctx.is_stopped() || Instant::now() >= deadline {
            return;
        }
        let ev = libc::POLLIN | if conn.wants_write() { libc::POLLOUT } else { 0 };
        poll(&[(tfd, ev)], TICK_MS);
        if flush_tls(&mut conn, &mut tcp).is_err() {
            return;
        }
        match conn.read_tls(&mut tcp) {
            Ok(0) => return,
            Ok(_) => {
                if conn.process_new_packets().is_err() {
                    // Best effort: send the alert (e.g. to a plain-HTTP client), then close.
                    let _ = flush_tls(&mut conn, &mut tcp);
                    return;
                }
            }
            Err(e) if is_would_block(&e) => {}
            Err(_) => return,
        }
    }

    // --- Connect to the internal plaintext listener and register the real peer.
    let Ok(mut backend) = TcpStream::connect_timeout(&backend_addr, Duration::from_secs(3)) else { return };
    no_sigpipe(&backend);
    let _ = backend.set_nodelay(true);
    let Ok(local) = backend.local_addr() else { return };
    ctx.peers.lock().unwrap_or_else(|e| e.into_inner()).insert(local.port(), peer);
    // Declared after `backend`: dropped (mapping removed) before the socket closes.
    let _guard = PeerGuard { ctx, port: local.port() };
    if backend.set_nonblocking(true).is_err() {
        return;
    }
    let bfd = backend.as_raw_fd();
    conn.set_buffer_limit(Some(4 * DOWN_CHUNK));

    // --- Relay.
    let mut up: Vec<u8> = Vec::new(); // client plaintext not yet written to the backend
    let mut up_off = 0usize;
    let mut buf = vec![0u8; DOWN_CHUNK];
    let mut client_eof = false;
    let mut backend_eof = false;
    let mut backend_wr_shut = false;
    loop {
        if ctx.is_stopped() {
            return;
        }
        let mut progress = false;

        // 1. Decrypted client data → `up`.
        loop {
            match conn.reader().read(&mut buf) {
                Ok(0) => {
                    client_eof = true; // close_notify
                    break;
                }
                Ok(n) => {
                    up.extend_from_slice(&buf[..n]);
                    progress = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    client_eof = true; // TCP closed without close_notify
                    break;
                }
            }
        }

        // 2. `up` → backend.
        if up_off < up.len() {
            match backend.write(&up[up_off..]) {
                Ok(n) => {
                    up_off += n;
                    progress |= n > 0;
                }
                Err(e) if is_would_block(&e) => {}
                Err(_) => return,
            }
            if up_off == up.len() {
                up.clear();
                up_off = 0;
            }
        }
        if client_eof && up.is_empty() && !backend_wr_shut {
            let _ = backend.shutdown(Shutdown::Write);
            backend_wr_shut = true;
        }

        // 3. Backend → TLS (only once previous records went out: backpressure).
        if !backend_eof && !conn.wants_write() {
            match backend.read(&mut buf) {
                Ok(0) => {
                    backend_eof = true;
                    conn.send_close_notify();
                    progress = true;
                }
                Ok(n) => {
                    if conn.writer().write_all(&buf[..n]).is_err() {
                        return;
                    }
                    progress = true;
                }
                Err(e) if is_would_block(&e) => {}
                Err(_) => {
                    backend_eof = true;
                    conn.send_close_notify();
                    progress = true;
                }
            }
        }

        // 4. TLS records → client.
        match flush_tls(&mut conn, &mut tcp) {
            Ok(w) => progress |= w,
            Err(_) => return,
        }
        if backend_eof && !conn.wants_write() {
            return; // response finished and flushed
        }

        // 5. Client → TLS.
        let want_client = !client_eof && up.len() < UP_LIMIT;
        if want_client {
            match conn.read_tls(&mut tcp) {
                Ok(0) => {
                    client_eof = true;
                    progress = true;
                }
                Ok(_) => {
                    if conn.process_new_packets().is_err() {
                        let _ = flush_tls(&mut conn, &mut tcp);
                        return;
                    }
                    progress = true;
                }
                Err(e) if is_would_block(&e) => {}
                Err(_) => return,
            }
        }

        if progress {
            continue;
        }
        // 6. Nothing to do right now: sleep until a socket is ready (or the tick elapses).
        let tev = (if want_client { libc::POLLIN } else { 0 }) | (if conn.wants_write() { libc::POLLOUT } else { 0 });
        let bev = (if !backend_eof && !conn.wants_write() { libc::POLLIN } else { 0 })
            | (if !up.is_empty() { libc::POLLOUT } else { 0 });
        poll(&[(if tev != 0 { tfd } else { -1 }, tev), (if bev != 0 { bfd } else { -1 }, bev)], TICK_MS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_parsing() {
        let h = "AB".repeat(32);
        assert_eq!(parse_fingerprint(&h), Some([0xab; 32]));
        let colons = vec!["ab"; 32].join(":");
        assert_eq!(parse_fingerprint(&colons), Some([0xab; 32]));
        assert_eq!(parse_fingerprint("abcd"), None);
        assert_eq!(parse_fingerprint(&"zz".repeat(32)), None);
        assert_eq!(parse_fingerprint(""), None);
    }

    #[test]
    fn identity_shape() {
        let a = generate_identity().unwrap();
        let b = generate_identity().unwrap();
        assert_eq!(a.fingerprint.len(), 64);
        assert!(a.fingerprint.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
        assert_ne!(a.fingerprint, b.fingerprint);
        assert!(server_config(&a, true).is_ok());
        assert!(server_config(&a, false).is_ok());
    }
}

/// Trust-on-first-use helper: accepts any certificate but records its SHA-256. Used only to
/// *learn* a share's fingerprint when it wasn't provided (address typed by hand); the client
/// then pins it and requires the pairing proof (HMAC over that fingerprint) before sending
/// any credential, so an impostor still fails.
#[derive(Debug)]
struct CaptureVerifier {
    seen: std::sync::Mutex<Option<String>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for CaptureVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap() = Some(hex(&Sha256::digest(end_entity.as_ref())));
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Learn the certificate fingerprint of `https://host:port` (see [`CaptureVerifier`]).
pub fn learn_fingerprint(origin: &str, timeout: std::time::Duration) -> Result<String, String> {
    let provider = provider();
    let v = Arc::new(CaptureVerifier { seen: std::sync::Mutex::new(None), provider: provider.clone() });
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(v.clone())
        .with_no_client_auth();
    let agent = ureq::AgentBuilder::new().tls_config(Arc::new(cfg)).timeout(timeout).build();
    let _ = agent.get(&format!("{origin}/.lumendisk/proof?nonce=00000000000000000000000000000000")).call();
    let fp = v.seen.lock().unwrap().clone();
    fp.ok_or_else(|| format!("{origin}: nessuna risposta TLS"))
}
