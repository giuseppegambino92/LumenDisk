//! WebDAV client backend (RFC 4918 subset) — works with NAS / Apache / nginx WebDAV servers
//! and with LumenDisk LAN shares (`urn:lumendisk` extensions: partial PUT via Content-Range,
//! POST ?op=… for truncate/chmod/mtime/symlink, /.lumendisk/info for statfs).

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use quick_xml::events::Event;

use super::*;

pub struct WebDavConfig {
    /// http(s)://host[:port][/base/path]
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// LAN shares: bearer token derived from the pairing code.
    pub bearer: Option<String>,
    /// LAN shares: TLS certificate SHA-256 to pin (from Bonjour). Learned on first contact
    /// when absent; either way the pairing proof must match before the token is sent.
    pub fingerprint: Option<String>,
}

pub struct WebDavBackend {
    origin: String,
    base: String,
    auth: Option<String>,
    agent: ureq::Agent,
    timeout: Mutex<Duration>,
    lumendisk: bool,
    /// Login name (never the password): part of the Shadow session identity.
    user: Option<String>,
    /// LAN shares: the pinned certificate fingerprint.
    fingerprint: Option<String>,
    /// LAN shares: (name, stable share id) from `/.lumendisk/info`.
    share: Option<(String, String)>,
}

fn pct_encode(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `host[:port]` → (lowercase host without a trailing dot, port). IPv6 literals keep brackets.
pub(crate) fn split_host_port(hostport: &str, default_port: u16) -> (String, u16) {
    let (h, p) = if let Some(rest) = hostport.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, after)) => (format!("[{h}]"), after.strip_prefix(':')),
            None => (hostport.to_string(), None),
        }
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (hostport.to_string(), None),
        }
    };
    let port = p.and_then(|p| p.parse().ok()).unwrap_or(default_port);
    (h.trim_end_matches('.').to_ascii_lowercase(), port)
}

/// Absolute path without empty or "." components and without a trailing slash ("/" for root).
pub(crate) fn canonical_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    format!("/{}", parts.join("/"))
}

/// (name, stable id) of a LumenDisk share from its `/.lumendisk/info` JSON.
fn share_identity(v: &serde_json::Value) -> (String, String) {
    use sha2::Digest;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let (name, fs, uuid) = (s("name"), s("fs_type"), s("uuid"));
    // Volumes without a UUID / serial: the share name is the best stable hint there is.
    let basis = if uuid.trim().is_empty() { format!("name|{fs}|{name}") } else { format!("uuid|{fs}|{uuid}") };
    let d = sha2::Sha256::digest(basis.as_bytes());
    (name, d[..12].iter().map(|b| format!("{b:02x}")).collect())
}

fn map_status(code: u16) -> RemoteError {
    match code {
        404 => RemoteError::NotFound,
        401 | 403 => RemoteError::Perm,
        405 | 412 => RemoteError::Exists,
        409 => RemoteError::NotFound,
        423 => RemoteError::Perm,
        507 => RemoteError::Other("spazio esaurito sul server".into()),
        c => RemoteError::Other(format!("HTTP {c}")),
    }
}

fn map_transport(e: &ureq::Transport) -> RemoteError {
    let msg = e.to_string();
    let low = msg.to_ascii_lowercase();
    if low.contains("timed out") || low.contains("timeout") || low.contains("would block") {
        RemoteError::Timeout
    } else {
        RemoteError::Disconnected(msg)
    }
}

impl WebDavBackend {
    pub fn connect(cfg: WebDavConfig) -> RResult<Self> {
        let url = cfg.url.trim_end_matches('/').to_string();
        let scheme_end = url.find("://").ok_or_else(|| RemoteError::Other("URL WebDAV non valido".into()))? + 3;
        let path_start = url[scheme_end..].find('/').map(|i| i + scheme_end).unwrap_or(url.len());
        let mut origin = url[..path_start].to_string();
        let base = pct_decode(&url[path_start..]);
        // user:pass@host in the URL
        let mut user = cfg.username.clone();
        if let Some(at) = origin[scheme_end..].find('@') {
            let creds = origin[scheme_end..scheme_end + at].to_string();
            if user.is_none() {
                user = Some(pct_decode(creds.split(':').next().unwrap_or("")));
            }
            origin = format!("{}{}", &origin[..scheme_end], &origin[scheme_end + at + 1..]);
        }
        let auth = if let Some(t) = &cfg.bearer {
            Some(format!("Bearer {t}"))
        } else {
            user.as_ref().map(|u| {
                let p = cfg.password.clone().unwrap_or_default();
                format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")))
            })
        };
        // LumenDisk LAN shares are HTTPS with a pinned self-signed certificate.
        if cfg.bearer.is_some() && origin.starts_with("http://") {
            origin = origin.replacen("http://", "https://", 1);
        }
        let mut pinned_fp = None;
        let builder = if cfg.bearer.is_some() {
            let fp = match cfg.fingerprint.clone().filter(|f| !f.trim().is_empty()) {
                Some(f) => f,
                None => crate::lan::learn_fingerprint(&origin, Duration::from_secs(5)).map_err(RemoteError::Disconnected)?,
            };
            // Before revealing the token, make the server prove it knows the pairing code
            // (HMAC over a fresh nonce and the very certificate we pinned).
            let pinned = crate::lan::pinned_agent_builder(&fp).timeout(Duration::from_secs(5)).build();
            let nonce: String = uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string()[..16];
            let token = cfg.bearer.clone().unwrap();
            let want = crate::lan::pairing_proof(&token, &nonce, &fp);
            match pinned.get(&format!("{origin}/.lumendisk/proof?nonce={nonce}")).call() {
                Ok(r) => {
                    let got = r.into_string().unwrap_or_default();
                    if !crate::lan::ct_eq_pub(got.trim().as_bytes(), want.as_bytes()) {
                        return Err(RemoteError::Perm);
                    }
                }
                Err(ureq::Error::Transport(t)) => return Err(map_transport(&t)),
                Err(ureq::Error::Status(c, _)) => return Err(map_status(c)),
            }
            let b = crate::lan::pinned_agent_builder(&fp);
            pinned_fp = Some(fp);
            b
        } else {
            ureq::AgentBuilder::new()
        };
        // Redirects are handled by hand: following them would drop the credentials.
        let agent = builder.timeout_connect(Duration::from_secs(3)).max_idle_connections_per_host(8).redirects(0).build();
        let user = if cfg.bearer.is_some() { None } else { user.filter(|u| !u.is_empty()) };
        let mut b = WebDavBackend {
            origin,
            base,
            auth,
            agent,
            timeout: Mutex::new(Duration::from_secs(5)),
            lumendisk: false,
            user,
            fingerprint: pinned_fp,
            share: None,
        };
        // Detect a LumenDisk share.
        if let Ok(r) = b.req("GET", "/.lumendisk/info", &[], None, 1) {
            if r.status() == 200 {
                b.lumendisk = true;
                let mut body = String::new();
                let _ = r.into_reader().take(64 << 10).read_to_string(&mut body);
                b.share = serde_json::from_str::<serde_json::Value>(&body).ok().map(|v| share_identity(&v));
            }
        }
        match b.stat("/") {
            Ok(_) => {}
            // Unreachable right now: still usable from the local cache / offline queue.
            Err(e) if e.is_network() => eprintln!("lumendisk: {} unreachable at connect ({e}); starting offline", b.describe()),
            Err(e) => return Err(e),
        }
        Ok(b)
    }

    pub fn is_lumendisk(&self) -> bool {
        self.lumendisk
    }

    /// Name of the shared volume, for LumenDisk LAN shares.
    pub fn share_name(&self) -> Option<String> {
        self.share.as_ref().map(|s| s.0.clone()).filter(|n| !n.trim().is_empty())
    }

    /// Canonical, credential-free identity of what this backend serves (Shadow session key).
    ///
    /// * WebDAV: `webdav+<scheme>://[user@]<host>:<port><base>` with the host lowercased, the
    ///   default port written out and the base path without duplicate or trailing slashes.
    /// * LumenDisk LAN share: `lan://<share id>`. The id is derived from the shared volume's
    ///   own identity (filesystem type + UUID / serial, see `/.lumendisk/info`), which stays
    ///   the same when the sharing Mac restarts the share, while its address, port, pairing
    ///   code and certificate are new every time. Older shares without one fall back to the
    ///   certificate fingerprint (stable only while that share keeps running).
    pub fn canonical_endpoint(&self) -> String {
        if self.lumendisk {
            if let Some((_, id)) = &self.share {
                return format!("lan://{id}");
            }
            if let Some(fp) = &self.fingerprint {
                return format!("lan-cert://{}", fp.to_ascii_lowercase());
            }
        }
        let (scheme, hostport) = self.origin.split_once("://").unwrap_or(("http", &self.origin));
        let scheme = scheme.to_ascii_lowercase();
        let default_port = if scheme == "https" { 443 } else { 80 };
        let (host, port) = split_host_port(hostport, default_port);
        let user = self.user.as_ref().map(|u| format!("{u}@")).unwrap_or_default();
        let base = canonical_path(&self.base);
        let kind = if self.lumendisk { "lan" } else { "webdav" };
        format!("{kind}+{scheme}://{user}{host}:{port}{base}")
    }

    fn url_for(&self, path: &str) -> String {
        let p = if path == "/" { "/".to_string() } else { path.to_string() };
        format!("{}{}{}", self.origin, pct_encode(self.base.trim_end_matches('/')), pct_encode(&p))
    }

    fn timeout(&self, mult: u32) -> Duration {
        *self.timeout.lock().unwrap() * mult
    }

    fn req(&self, method: &str, path: &str, headers: &[(&str, String)], body: Option<&[u8]>, mult: u32) -> RResult<ureq::Response> {
        let url = if path.starts_with("/.lumendisk/") { format!("{}{}", self.origin, path) } else { self.url_for(path) };
        let r = self.req_url(method, &url, headers, body, mult)?;
        // Collections without a trailing slash are redirected by most servers (Apache:
        // 301 → …/): retry once with the slash, keeping our credentials.
        if (300..400).contains(&r.status()) && !url.ends_with('/') {
            return self.req_url(method, &format!("{url}/"), headers, body, mult);
        }
        Ok(r)
    }

    fn req_url(&self, method: &str, url: &str, headers: &[(&str, String)], body: Option<&[u8]>, mult: u32) -> RResult<ureq::Response> {
        let mut r = self.agent.request(method, url).timeout(self.timeout(mult));
        if let Some(a) = &self.auth {
            r = r.set("Authorization", a);
        }
        for (k, v) in headers {
            r = r.set(k, v);
        }
        let res = match body {
            Some(b) => r.send_bytes(b),
            None => r.call(),
        };
        match res {
            Ok(resp) => Ok(resp),
            Err(ureq::Error::Status(code, _)) => Err(map_status(code)),
            Err(ureq::Error::Transport(t)) => Err(map_transport(&t)),
        }
    }

    fn propfind(&self, path: &str, depth: u32) -> RResult<Vec<(String, RemoteEntry)>> {
        let body = br#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>"#;
        let r = self.req(
            "PROPFIND",
            path,
            &[("Depth", depth.to_string()), ("Content-Type", "application/xml".into())],
            Some(body),
            1,
        )?;
        let mut xml = String::new();
        r.into_reader().take(64 << 20).read_to_string(&mut xml).map_err(|e| RemoteError::Disconnected(e.to_string()))?;
        Ok(self.parse_multistatus(&xml))
    }

    /// Parse a 207 multistatus into (absolute path relative to base, entry).
    fn parse_multistatus(&self, xml: &str) -> Vec<(String, RemoteEntry)> {
        let mut out = vec![];
        let mut reader = quick_xml::Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut cur: Option<(String, RemoteEntry, Option<i64>)> = None;
        let mut field = String::new();
        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    let n = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                    if n == "response" {
                        cur = Some((
                            String::new(),
                            RemoteEntry { name: String::new(), kind: FileKind::File, size: 0, mtime: Timestamp::default(), perm: 0, symlink_target: None, etag: None },
                            None,
                        ));
                    } else if n == "collection" {
                        if let Some(c) = cur.as_mut() {
                            c.1.kind = FileKind::Dir;
                        }
                    }
                    field = n;
                }
                Ok(Event::Empty(e)) => {
                    let n = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                    if n == "collection" {
                        if let Some(c) = cur.as_mut() {
                            c.1.kind = FileKind::Dir;
                        }
                    }
                }
                Ok(Event::Text(t)) => {
                    let v = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                    if let Some(c) = cur.as_mut() {
                        match field.as_str() {
                            "href" => c.0 = v,
                            "getcontentlength" => c.1.size = v.trim().parse().unwrap_or(0),
                            "getlastmodified" => {
                                if let Ok(t) = httpdate::parse_http_date(v.trim()) {
                                    let d = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                                    if c.1.mtime == Timestamp::default() {
                                        c.1.mtime = Timestamp { secs: d.as_secs() as i64, nsecs: 0 };
                                    }
                                }
                            }
                            "mtime_ns" => c.2 = v.trim().parse().ok(),
                            // The LAN server's ETag contains the sharing Mac's inode number, which
                            // isn't stable across its restarts; its mtime_ns is precise instead.
                            "getetag" if !self.lumendisk => {
                                let t = v.trim();
                                let t = t.strip_prefix("W/").unwrap_or(t).trim_matches('"');
                                c.1.etag = Some(t.to_string()).filter(|t| !t.is_empty());
                            }
                            "mode" => c.1.perm = u16::from_str_radix(v.trim(), 8).unwrap_or(0) & 0o7777,
                            "symlink" => {
                                c.1.kind = FileKind::Symlink;
                                c.1.symlink_target = Some(v);
                            }
                            _ => {}
                        }
                    }
                }
                Ok(Event::End(e)) => {
                    let n = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                    if n == "response" {
                        if let Some((href, mut ent, ns)) = cur.take() {
                            if let Some(ns) = ns {
                                ent.mtime = Timestamp { secs: ns.div_euclid(1_000_000_000), nsecs: ns.rem_euclid(1_000_000_000) as u32 };
                            }
                            let mut p = pct_decode(&href);
                            if let Some(i) = p.find("://") {
                                p = p[i + 3..].find('/').map(|j| p[i + 3 + j..].to_string()).unwrap_or_else(|| "/".into());
                            }
                            let p = p.strip_prefix(&self.base).map(|s| s.to_string()).unwrap_or(p);
                            let p = p.trim_end_matches('/');
                            let p = if p.is_empty() { "/".to_string() } else if p.starts_with('/') { p.to_string() } else { format!("/{p}") };
                            ent.name = p.rsplit('/').next().unwrap_or("").to_string();
                            if ent.perm == 0 {
                                ent.perm = if ent.kind == FileKind::Dir { 0o755 } else { 0o644 };
                            }
                            if ent.kind == FileKind::Symlink {
                                ent.size = ent.symlink_target.as_ref().map(|t| t.len() as u64).unwrap_or(0);
                            }
                            out.push((p, ent));
                        }
                    }
                    field.clear();
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
        out
    }

    fn post_op(&self, path: &str, query: &str) -> RResult<()> {
        let url = format!("{}?{}", self.url_for(path), query);
        let mut r = self.agent.post(&url).timeout(self.timeout(2));
        if let Some(a) = &self.auth {
            r = r.set("Authorization", a);
        }
        match r.send_bytes(&[]) {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, _)) => Err(map_status(code)),
            Err(ureq::Error::Transport(t)) => Err(map_transport(&t)),
        }
    }

    fn is_dir(&self, path: &str) -> RResult<bool> {
        Ok(self.stat(path)?.kind == FileKind::Dir)
    }
}

impl RemoteBackend for WebDavBackend {
    fn describe(&self) -> String {
        format!("{}{}", self.origin, self.base)
    }
    fn set_timeout(&self, t: Duration) {
        *self.timeout.lock().unwrap() = t;
    }
    fn ping(&self) -> RResult<Duration> {
        let t0 = Instant::now();
        if self.lumendisk {
            self.req("GET", "/.lumendisk/info", &[], None, 1)?;
        } else {
            self.propfind("/", 0)?;
        }
        Ok(t0.elapsed())
    }
    fn stat(&self, path: &str) -> RResult<RemoteEntry> {
        let mut v = self.propfind(path, 0)?;
        v.pop().map(|(_, e)| e).ok_or(RemoteError::NotFound)
    }
    fn list(&self, path: &str) -> RResult<Vec<RemoteEntry>> {
        let want = path.trim_end_matches('/');
        let want = if want.is_empty() { "/" } else { want };
        let dir = if path.ends_with('/') { path.to_string() } else { format!("{path}/") };
        Ok(self.propfind(&dir, 1)?.into_iter().filter(|(p, _)| p != want).map(|(_, e)| e).collect())
    }
    fn read(&self, path: &str, offset: u64, len: u32) -> RResult<Vec<u8>> {
        if len == 0 {
            return Ok(vec![]);
        }
        let r = match self.req("GET", path, &[("Range", format!("bytes={}-{}", offset, offset + len as u64 - 1))], None, 4) {
            Err(RemoteError::Other(m)) if m == "HTTP 416" => return Ok(vec![]),
            r => r?,
        };
        let status = r.status();
        let mut buf = Vec::with_capacity(len as usize);
        let mut rd = r.into_reader();
        if status == 200 {
            // Server ignored the range: skip to offset.
            std::io::copy(&mut (&mut rd).take(offset), &mut std::io::sink()).map_err(|e| RemoteError::Disconnected(e.to_string()))?;
        }
        rd.take(len as u64).read_to_end(&mut buf).map_err(|e| RemoteError::Disconnected(e.to_string()))?;
        Ok(buf)
    }
    fn supports_partial_write(&self) -> bool {
        self.lumendisk
    }
    fn write(&self, path: &str, offset: u64, data: &[u8]) -> RResult<()> {
        if data.is_empty() {
            return Ok(());
        }
        if !self.lumendisk {
            return Err(RemoteError::Unsupported("scrittura parziale WebDAV".into()));
        }
        self.req("PUT", path, &[("Content-Range", format!("bytes {}-{}/*", offset, offset + data.len() as u64 - 1))], Some(data), 8)?;
        Ok(())
    }
    fn put_all(&self, path: &str, data: &[u8]) -> RResult<()> {
        self.req("PUT", path, &[], Some(data), 8 + (data.len() as u32 >> 20))?;
        Ok(())
    }
    fn create(&self, path: &str, perm: u16) -> RResult<()> {
        match self.stat(path) {
            Ok(_) => return Err(RemoteError::Exists),
            Err(RemoteError::NotFound) => {}
            Err(e) => return Err(e),
        }
        self.req("PUT", path, &[], Some(&[]), 2)?;
        if self.lumendisk {
            let _ = self.set_perm(path, perm);
        }
        Ok(())
    }
    fn truncate(&self, path: &str, size: u64) -> RResult<()> {
        if self.lumendisk {
            return self.post_op(path, &format!("op=truncate&size={size}"));
        }
        let cur = self.stat(path)?.size;
        if cur == size {
            return Ok(());
        }
        let mut data = if size > 0 { self.read(path, 0, cur.min(size) as u32)? } else { vec![] };
        data.resize(size as usize, 0);
        self.put_all(path, &data)
    }
    fn mkdir(&self, path: &str, _perm: u16) -> RResult<()> {
        self.req("MKCOL", path, &[], None, 2).map(|_| ())
    }
    fn remove(&self, path: &str) -> RResult<()> {
        if self.is_dir(path)? {
            return Err(RemoteError::IsDir);
        }
        self.req("DELETE", path, &[], None, 2).map(|_| ())
    }
    fn rmdir(&self, path: &str) -> RResult<()> {
        // WebDAV DELETE on a collection is recursive: check emptiness first.
        if !self.list(path)?.is_empty() {
            return Err(RemoteError::NotEmpty);
        }
        self.req("DELETE", path, &[], None, 2).map(|_| ())
    }
    fn rename(&self, from: &str, to: &str) -> RResult<()> {
        let dest = self.url_for(to);
        self.req("MOVE", from, &[("Destination", dest), ("Overwrite", "T".into())], None, 4).map(|_| ())
    }
    fn symlink(&self, path: &str, target: &str) -> RResult<()> {
        if !self.lumendisk {
            return Err(RemoteError::Unsupported("symlink su WebDAV".into()));
        }
        self.post_op(path, &format!("op=symlink&target={}", pct_encode(target).replace('/', "%2F")))
    }
    fn readlink(&self, path: &str) -> RResult<String> {
        self.stat(path)?.symlink_target.ok_or(RemoteError::Other("not a symlink".into()))
    }
    fn set_perm(&self, path: &str, perm: u16) -> RResult<()> {
        if self.lumendisk {
            self.post_op(path, &format!("op=chmod&mode={:o}", perm & 0o7777))
        } else {
            Ok(())
        }
    }
    fn set_mtime(&self, path: &str, mtime: Timestamp) -> RResult<()> {
        if self.lumendisk {
            let ns = mtime.secs as i128 * 1_000_000_000 + mtime.nsecs as i128;
            self.post_op(path, &format!("op=mtime&ns={ns}"))
        } else {
            Ok(())
        }
    }
    fn statfs(&self) -> RResult<Option<(u64, u64)>> {
        if !self.lumendisk {
            return Ok(None);
        }
        let r = self.req("GET", "/.lumendisk/info", &[], None, 1)?;
        let v: serde_json::Value = serde_json::from_reader(r.into_reader()).map_err(|e| RemoteError::Other(e.to_string()))?;
        Ok(Some((v["total_bytes"].as_u64().unwrap_or(0), v["free_bytes"].as_u64().unwrap_or(0))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(origin: &str, base: &str, user: Option<&str>) -> WebDavBackend {
        WebDavBackend {
            origin: origin.into(),
            base: base.into(),
            auth: None,
            agent: ureq::AgentBuilder::new().build(),
            timeout: Mutex::new(Duration::from_secs(1)),
            lumendisk: false,
            user: user.map(String::from),
            fingerprint: None,
            share: None,
        }
    }

    #[test]
    fn etags_are_parsed_without_the_weak_marker() {
        let b = backend("http://nas", "/dav", None);
        let xml = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">
            <D:response><D:href>/dav/a.txt</D:href><D:propstat><D:prop><D:getcontentlength>5</D:getcontentlength>
            <D:getetag>W/"5-63f1a2b3c4d5e"</D:getetag></D:prop></D:propstat></D:response>
            <D:response><D:href>/dav/b.txt</D:href><D:propstat><D:prop><D:getetag>"9-1"</D:getetag></D:prop></D:propstat></D:response>
            <D:response><D:href>/dav/c.txt</D:href><D:propstat><D:prop><D:getcontentlength>1</D:getcontentlength></D:prop></D:propstat></D:response>
            </D:multistatus>"#;
        let v = b.parse_multistatus(xml);
        let tag = |p: &str| v.iter().find(|(x, _)| x == p).and_then(|(_, e)| e.etag.clone());
        // Apache marks an ETag weak for a second after a change: same version, same tag.
        assert_eq!(tag("/a.txt").as_deref(), Some("5-63f1a2b3c4d5e"));
        assert_eq!(tag("/b.txt").as_deref(), Some("9-1"));
        assert_eq!(tag("/c.txt"), None);
        // LumenDisk shares put the host's inode number in theirs: ignored (mtime_ns instead).
        let mut lan = backend("https://mac", "", None);
        lan.lumendisk = true;
        assert!(lan.parse_multistatus(xml).iter().all(|(_, e)| e.etag.is_none()));
    }

    #[test]
    fn canonical_endpoints() {
        assert_eq!(backend("https://NAS.Local.", "/dav/", Some("pi")).canonical_endpoint(), "webdav+https://pi@nas.local:443/dav");
        assert_eq!(backend("http://nas:8080", "//dav//x/", None).canonical_endpoint(), "webdav+http://nas:8080/dav/x");
        assert_eq!(backend("http://[fe80::1]", "", None).canonical_endpoint(), "webdav+http://[fe80::1]:80/");
        let mut lan = backend("https://192.168.1.20:50123", "", None);
        lan.lumendisk = true;
        lan.fingerprint = Some("AB12".into());
        assert_eq!(lan.canonical_endpoint(), "lan-cert://ab12");
        let info = serde_json::json!({"name": "Disco di Anna", "fs_type": "EXT4", "uuid": "1234-abcd"});
        lan.share = Some(share_identity(&info));
        let a = lan.canonical_endpoint();
        // Another address, port and certificate of the same shared volume: same identity.
        let mut again = backend("https://10.0.0.7:61000", "", None);
        again.lumendisk = true;
        again.fingerprint = Some("CD34".into());
        again.share = Some(share_identity(&info));
        assert_eq!(a, again.canonical_endpoint());
        assert!(a.starts_with("lan://"), "{a}");
        assert_eq!(lan.share_name().as_deref(), Some("Disco di Anna"));
    }
}
