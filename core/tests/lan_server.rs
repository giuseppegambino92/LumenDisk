//! LAN server tests: a real `LanServer` on an ext4 volume in RAM, driven over HTTPS (pinned to
//! the server's fingerprint) and, for the portal's compatibility port, plain HTTP, with `ureq`.
//!
//!   cargo test -p lumendisk-core --test lan_server

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lumendisk_core::block::{BlockDev, MemDev};
use lumendisk_core::lan::{
    lan_addresses, new_pairing_code, new_pin, pinned_agent, pinned_agent_builder, pinned_client_config, token_from_pairing_code, LanMode,
    LanServer,
};
use lumendisk_core::vfs::ext4_fs::{self, Ext4Fs};
use lumendisk_core::vfs::testutil::pattern;
use lumendisk_core::vfs::{FileKind, FileSystem, FsType, Ino, SetAttr, Timestamp};

type Vol = Arc<Mutex<Ext4Fs>>;

const TOKEN: &str = "s3cret-token-0123456789abcdef";

fn volume(mb: usize) -> Vol {
    let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(mb * 1024 * 1024));
    ext4_fs::format(dev.clone(), FsType::Ext4, "LANTEST").unwrap();
    Arc::new(Mutex::new(Ext4Fs::open(dev, false).unwrap()))
}

thread_local! {
    /// Fingerprint of the server the current test talks to (each test runs on its own thread and
    /// starts its own server). `agent()` pins to it.
    static FP: RefCell<String> = const { RefCell::new(String::new()) };
}

fn pin_to(s: &LanServer) {
    FP.with(|f| *f.borrow_mut() = s.fingerprint());
}

fn share(vol: &Vol, ro: bool) -> (LanServer, String) {
    let s = LanServer::start(vol.clone(), LanMode::Share { token: TOKEN.into() }, "Test Disk", ro).unwrap();
    let base = format!("https://127.0.0.1:{}", s.port());
    pin_to(&s);
    (s, base)
}

/// Agent pinned to the current server's certificate (also fine for plain-HTTP URLs).
fn agent() -> ureq::Agent {
    let fp = FP.with(|f| f.borrow().clone());
    pinned_agent_builder(&fp).redirects(0).timeout(Duration::from_secs(30)).build()
}

struct Resp {
    status: u16,
    headers: HashMap<String, Vec<String>>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, n: &str) -> Option<&str> {
        self.headers.get(&n.to_ascii_lowercase()).and_then(|v| v.first()).map(|s| s.as_str())
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn send(req: ureq::Request, body: Option<&[u8]>) -> Resp {
    let r = match body {
        Some(b) => req.send_bytes(b),
        None => req.call(),
    };
    let resp = match r {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("transport error: {e}"),
    };
    let status = resp.status();
    let mut headers: HashMap<String, Vec<String>> = HashMap::new();
    for n in resp.headers_names() {
        let v = resp.all(&n).into_iter().map(|s| s.to_string()).collect();
        headers.insert(n.to_ascii_lowercase(), v);
    }
    let mut body = Vec::new();
    resp.into_reader().read_to_end(&mut body).unwrap();
    Resp { status, headers, body }
}

/// Authenticated share-mode request.
fn dav(base: &str, method: &str, path: &str) -> ureq::Request {
    agent().request(method, &format!("{base}{path}")).set("Authorization", &format!("Bearer {TOKEN}"))
}

// --- direct filesystem helpers -----------------------------------------------------------------

fn lookup_path(vol: &Vol, path: &str) -> Option<Ino> {
    let mut fs = vol.lock().unwrap();
    let mut ino = fs.root_ino();
    for c in path.split('/').filter(|c| !c.is_empty()) {
        ino = fs.lookup(ino, c).ok()?;
    }
    Some(ino)
}

fn read_all(vol: &Vol, path: &str) -> Vec<u8> {
    let ino = lookup_path(vol, path).unwrap_or_else(|| panic!("{path} missing"));
    let mut fs = vol.lock().unwrap();
    let size = fs.getattr(ino).unwrap().size;
    let mut out = Vec::new();
    while (out.len() as u64) < size {
        let d = fs.read(ino, out.len() as u64, 1 << 20).unwrap();
        assert!(!d.is_empty());
        out.extend(d);
    }
    out
}

fn mkfile(vol: &Vol, dir: &str, name: &str, data: &[u8]) -> Ino {
    let parent = lookup_path(vol, dir).unwrap();
    let mut fs = vol.lock().unwrap();
    let ino = fs.create(parent, name, 0o644, 1000, 1000).unwrap();
    fs.write(ino, 0, data).unwrap();
    ino
}

fn mkdir(vol: &Vol, dir: &str, name: &str) -> Ino {
    let parent = lookup_path(vol, dir).unwrap();
    vol.lock().unwrap().mkdir(parent, name, 0o755, 1000, 1000).unwrap()
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn unpct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

// --- PROPFIND parsing ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Prop {
    href: String,
    collection: bool,
    length: Option<u64>,
    lastmod: String,
    mode: String,
    mtime_ns: String,
    symlink: Option<String>,
}

fn parse_multistatus(xml: &str) -> Vec<Prop> {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur: Option<Prop> = None;
    let mut path: Vec<String> = Vec::new();
    let mut saw_ns = (false, false);
    loop {
        match r.read_event().unwrap() {
            Event::Start(e) => {
                let n = String::from_utf8(e.name().as_ref().to_vec()).unwrap();
                if n == "D:multistatus" {
                    for a in e.attributes() {
                        let a = a.unwrap();
                        let (k, v) = (String::from_utf8_lossy(a.key.as_ref()).into_owned(), a.unescape_value().unwrap().into_owned());
                        if k == "xmlns:D" && v == "DAV:" {
                            saw_ns.0 = true;
                        }
                        if k == "xmlns:L" && v == "urn:lumendisk" {
                            saw_ns.1 = true;
                        }
                    }
                }
                if n == "D:response" {
                    cur = Some(Prop::default());
                }
                path.push(n);
            }
            Event::Empty(e) => {
                if e.name().as_ref() == b"D:collection" {
                    if let Some(c) = cur.as_mut() {
                        c.collection = true;
                    }
                }
            }
            Event::Text(t) => {
                let v = t.unescape().unwrap().into_owned();
                if let (Some(c), Some(tag)) = (cur.as_mut(), path.last()) {
                    match tag.as_str() {
                        "D:href" => c.href = v,
                        "D:getcontentlength" => c.length = Some(v.parse().unwrap()),
                        "D:getlastmodified" => c.lastmod = v,
                        "L:mode" => c.mode = v,
                        "L:mtime_ns" => c.mtime_ns = v,
                        "L:symlink" => c.symlink = Some(v),
                        _ => {}
                    }
                }
            }
            Event::End(e) => {
                if e.name().as_ref() == b"D:response" {
                    out.push(cur.take().unwrap());
                }
                path.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    assert!(saw_ns.0 && saw_ns.1, "namespaces missing");
    out
}

fn propfind(base: &str, path: &str, depth: &str) -> (u16, Vec<Prop>) {
    let r = send(dav(base, "PROPFIND", path).set("Depth", depth), Some(b""));
    if r.status != 207 {
        return (r.status, vec![]);
    }
    (r.status, parse_multistatus(&r.text()))
}

/// Pinned TLS stream to `https://host:port` (raw bytes, no HTTP client in between).
fn tls_stream(base: &str) -> rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream> {
    let addr = base.trim_start_matches("https://");
    let fp = FP.with(|f| f.borrow().clone());
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
    let conn = rustls::ClientConnection::new(pinned_client_config(&fp), name).unwrap();
    let tcp = std::net::TcpStream::connect(addr).unwrap();
    rustls::StreamOwned::new(conn, tcp)
}

/// Send a request verbatim (over pinned TLS) and return the status line.
fn raw_status(base: &str, method: &str, target: &str) -> String {
    let addr = base.trim_start_matches("https://");
    let mut s = tls_stream(base);
    write!(s, "{method} {target} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\nDepth: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = Vec::new();
    // The server may close without close_notify after `Connection: close`; keep what we got.
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).lines().next().unwrap_or("").to_string()
}

// ================================================================================================
// Pure helpers

#[test]
fn pairing_token_normalisation() {
    use sha2::Digest;
    let expect = {
        let mut h = sha2::Sha256::new();
        h.update(b"lumendisk-pair-v1:ABCDEFGH");
        h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    assert_eq!(token_from_pairing_code("ABCD-EFGH"), expect);
    assert_eq!(token_from_pairing_code("abcd-efgh"), expect);
    assert_eq!(token_from_pairing_code(" abcd efgh "), expect);
    assert_eq!(token_from_pairing_code("ABCDEFGH"), expect);
    assert_eq!(token_from_pairing_code("AB-CD-EF-GH"), expect);
    assert_ne!(token_from_pairing_code("ABCD-EFGJ"), expect);
    assert_eq!(expect.len(), 64);

    let c = new_pairing_code();
    assert_eq!(c.len(), 9);
    assert!(c.chars().all(|ch| ch == '-' || "ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(ch)));
    assert_eq!(token_from_pairing_code(&c), token_from_pairing_code(&c.to_lowercase().replace('-', " ")));
    let p = new_pin();
    assert!(p.len() == 6 && p.bytes().all(|b| b.is_ascii_digit()));
}

#[test]
fn lan_addresses_are_real() {
    let a = lan_addresses();
    assert!(!a.is_empty(), "no active LAN interface found");
    for ip in &a {
        assert!(!ip.starts_with("127."), "{ip}");
        assert!(ip.parse::<std::net::Ipv4Addr>().is_ok(), "{ip}");
    }
}

// ================================================================================================
// Share mode

#[test]
fn share_auth_and_info() {
    let vol = volume(32);
    let (srv, base) = share(&vol, false);

    let r = send(agent().get(&format!("{base}/.lumendisk/info")), None);
    assert_eq!(r.status, 401);
    let r = send(agent().get(&format!("{base}/.lumendisk/info")).set("Authorization", "Bearer nope"), None);
    assert_eq!(r.status, 401);
    let r = send(agent().request("PROPFIND", &format!("{base}/")).set("Authorization", &format!("Basic {TOKEN}")), None);
    assert_eq!(r.status, 401);

    let r = send(dav(&base, "GET", "/.lumendisk/info"), None);
    assert_eq!(r.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["version"], 1);
    assert_eq!(v["name"], "Test Disk");
    assert_eq!(v["fs_type"], "EXT4");
    assert_eq!(v["read_only"], false);
    let total = v["total_bytes"].as_u64().unwrap();
    let free = v["free_bytes"].as_u64().unwrap();
    assert!(total > 30 * 1024 * 1024 && free > 0 && free < total);

    assert!(srv.stats().clients >= 1);
}

#[test]
fn share_propfind() {
    let vol = volume(32);
    let body = pattern(1234, 1);
    mkfile(&vol, "/", "a b.txt", &body);
    mkdir(&vol, "/", "dir");
    let uni = "ünï cødé ☃ & <x>.bin";
    mkfile(&vol, "/dir", uni, b"hello");
    mkdir(&vol, "/dir", "sub dir");
    {
        let mut fs = vol.lock().unwrap();
        let root = fs.root_ino();
        fs.symlink(root, "lnk", "dir/target with space & <x>", 1000, 1000).unwrap();
        let f = fs.lookup(root, "a b.txt").unwrap();
        fs.setattr(f, &SetAttr { mtime: Some(Timestamp { secs: 1_700_000_000, nsecs: 123_456_789 }), perm: Some(0o640), ..Default::default() })
            .unwrap();
    }
    let (_srv, base) = share(&vol, false);

    // Depth 0 on the root: just the root.
    let (st, p) = propfind(&base, "/", "0");
    assert_eq!(st, 207);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].href, "/");
    assert!(p[0].collection);

    // Depth 1 on the root.
    let (st, p) = propfind(&base, "/", "1");
    assert_eq!(st, 207);
    let by: HashMap<String, Prop> = p.iter().map(|x| (x.href.clone(), x.clone())).collect();
    assert!(by.contains_key("/"));
    let a = &by["/a%20b.txt"];
    assert!(!a.collection);
    assert_eq!(a.length, Some(1234));
    assert_eq!(a.mode, "640");
    assert_eq!(a.mtime_ns, "1700000000123456789");
    assert_eq!(a.lastmod, "Tue, 14 Nov 2023 22:13:20 GMT");
    assert!(a.symlink.is_none());
    let d = &by["/dir/"];
    assert!(d.collection);
    assert_eq!(d.mode, "755");
    let l = &by["/lnk"];
    assert!(!l.collection);
    assert_eq!(l.symlink.as_deref(), Some("dir/target with space & <x>"));
    assert!(by.contains_key("/lost%2Bfound/"));

    // Depth 1 on a subdirectory, addressed with percent-encoding.
    let (st, p) = propfind(&base, "/dir/", "1");
    assert_eq!(st, 207);
    assert_eq!(p.len(), 3);
    assert_eq!(p[0].href, "/dir/");
    let hrefs: Vec<String> = p.iter().map(|x| unpct(&x.href)).collect();
    assert!(hrefs.contains(&format!("/dir/{uni}")), "{hrefs:?}");
    assert!(hrefs.contains(&"/dir/sub dir/".to_string()));
    let u = p.iter().find(|x| unpct(&x.href) == format!("/dir/{uni}")).unwrap();
    assert_eq!(u.length, Some(5));
    assert_eq!(u.href, format!("/dir/{}", pct(uni)));

    // Depth 0 on a unicode file.
    let (st, p) = propfind(&base, &format!("/dir/{}", pct(uni)), "0");
    assert_eq!(st, 207);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].length, Some(5));

    // Missing.
    assert_eq!(propfind(&base, "/nope", "0").0, 404);
    assert_eq!(propfind(&base, "/dir/nope/deeper", "1").0, 404);
    // ".." rejected (raw socket: ureq would normalise the URL).
    for target in ["/dir/../", "/dir/%2E%2E/", "/dir/%2e%2e/a%20b.txt"] {
        assert!(raw_status(&base, "PROPFIND", target).starts_with("HTTP/1.1 400"), "{target}");
    }
}

#[test]
fn share_get_and_put() {
    let vol = volume(32);
    let data = pattern(700_000, 3);
    mkfile(&vol, "/", "big.bin", &data);
    mkdir(&vol, "/", "d");
    let (srv, base) = share(&vol, false);

    // Full GET.
    let r = send(dav(&base, "GET", "/big.bin"), None);
    assert_eq!(r.status, 200);
    assert_eq!(r.body, data);
    assert_eq!(r.header("Content-Length"), Some("700000"));
    assert_eq!(r.header("Accept-Ranges"), Some("bytes"));

    // Ranges.
    let r = send(dav(&base, "GET", "/big.bin").set("Range", "bytes=10-19"), None);
    assert_eq!(r.status, 206);
    assert_eq!(r.body, &data[10..20]);
    assert_eq!(r.header("Content-Range"), Some("bytes 10-19/700000"));
    let r = send(dav(&base, "GET", "/big.bin").set("Range", "bytes=699990-"), None);
    assert_eq!(r.status, 206);
    assert_eq!(r.body, &data[699_990..]);
    let r = send(dav(&base, "GET", "/big.bin").set("Range", "bytes=300000-599999"), None);
    assert_eq!(r.status, 206);
    assert_eq!(r.body, &data[300_000..600_000]);
    let r = send(dav(&base, "GET", "/big.bin").set("Range", "bytes=800000-"), None);
    assert_eq!(r.status, 416);

    // HEAD.
    let r = send(dav(&base, "HEAD", "/big.bin"), None);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("Content-Length"), Some("700000"));
    assert!(r.body.is_empty());

    // Directories / missing.
    assert_eq!(send(dav(&base, "GET", "/d"), None).status, 405);
    assert_eq!(send(dav(&base, "GET", "/missing"), None).status, 404);

    // PUT new file → 201.
    let new = pattern(300_000, 9);
    let r = send(dav(&base, "PUT", "/d/new%20file.bin"), Some(&new));
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(read_all(&vol, "/d/new file.bin"), new);

    // PUT replacing a larger file with a smaller one → 204, truncated.
    let r = send(dav(&base, "PUT", "/big.bin"), Some(b"short"));
    assert_eq!(r.status, 204);
    assert_eq!(read_all(&vol, "/big.bin"), b"short");

    // Partial PUT: write at an offset without truncating.
    let r = send(dav(&base, "PUT", "/d/new%20file.bin").set("Content-Range", "bytes 100-104/*"), Some(b"HELLO"));
    assert_eq!(r.status, 204);
    let mut expect = new.clone();
    expect[100..105].copy_from_slice(b"HELLO");
    assert_eq!(read_all(&vol, "/d/new file.bin"), expect);
    // ...and past EOF with an explicit total.
    let r = send(dav(&base, "PUT", "/d/new%20file.bin").set("Content-Range", "bytes 300000-300002/300003"), Some(b"END"));
    assert_eq!(r.status, 204);
    expect.extend_from_slice(b"END");
    assert_eq!(read_all(&vol, "/d/new file.bin"), expect);

    // Empty PUT creates an empty file.
    assert_eq!(send(dav(&base, "PUT", "/empty"), Some(b"")).status, 201);
    assert_eq!(read_all(&vol, "/empty"), b"");

    // Missing parent → 409; PUT onto a directory → 405.
    assert_eq!(send(dav(&base, "PUT", "/nope/x.bin"), Some(b"x")).status, 409);
    assert_eq!(send(dav(&base, "PUT", "/d"), Some(b"x")).status, 405);

    let st = srv.stats();
    assert!(st.bytes_received >= 300_000 + 5 + 5 + 3);
    assert!(st.bytes_sent >= 700_000);
}

#[test]
fn share_mkcol_delete_move() {
    let vol = volume(32);
    let (_srv, base) = share(&vol, false);

    assert_eq!(send(dav(&base, "MKCOL", "/x"), None).status, 201);
    assert_eq!(send(dav(&base, "MKCOL", "/x"), None).status, 405);
    assert_eq!(send(dav(&base, "MKCOL", "/nope/y"), None).status, 409);
    assert_eq!(send(dav(&base, "MKCOL", "/x/%C3%A0%20b"), None).status, 201);
    assert!(lookup_path(&vol, "/x/à b").is_some());

    // DELETE: non-empty dir → 409, file → 204, empty dir → 204, missing → 404.
    assert_eq!(send(dav(&base, "PUT", "/x/f.txt"), Some(b"f")).status, 201);
    assert_eq!(send(dav(&base, "DELETE", "/x"), None).status, 409);
    assert_eq!(send(dav(&base, "DELETE", "/x/f.txt"), None).status, 204);
    assert_eq!(send(dav(&base, "DELETE", "/x/%C3%A0%20b"), None).status, 204);
    assert_eq!(send(dav(&base, "DELETE", "/x"), None).status, 204);
    assert_eq!(send(dav(&base, "DELETE", "/x"), None).status, 404);

    // MOVE to a new name (relative path form) → 201.
    send(dav(&base, "PUT", "/a.txt"), Some(b"AAA"));
    send(dav(&base, "PUT", "/b.txt"), Some(b"BBB"));
    let r = send(dav(&base, "MOVE", "/a.txt").set("Destination", "/c%20c.txt"), None);
    assert_eq!(r.status, 201);
    assert!(lookup_path(&vol, "/a.txt").is_none());
    assert_eq!(read_all(&vol, "/c c.txt"), b"AAA");

    // Overwrite: F → 412, nothing changes.
    let r = send(dav(&base, "MOVE", "/c%20c.txt").set("Destination", &format!("{base}/b.txt")).set("Overwrite", "F"), None);
    assert_eq!(r.status, 412);
    assert_eq!(read_all(&vol, "/b.txt"), b"BBB");
    // Overwrite T (default) with an absolute URL → 204, replaced.
    let r = send(dav(&base, "MOVE", "/c%20c.txt").set("Destination", &format!("{base}/b.txt")), None);
    assert_eq!(r.status, 204);
    assert_eq!(read_all(&vol, "/b.txt"), b"AAA");
    assert!(lookup_path(&vol, "/c c.txt").is_none());

    // Move a directory into another directory.
    send(dav(&base, "MKCOL", "/m"), None);
    send(dav(&base, "MKCOL", "/n"), None);
    send(dav(&base, "PUT", "/m/inner.txt"), Some(b"in"));
    assert_eq!(send(dav(&base, "MOVE", "/m/").set("Destination", "/n/m2/"), None).status, 201);
    assert_eq!(read_all(&vol, "/n/m2/inner.txt"), b"in");
    // Missing destination parent → 409; missing source → 404; into itself → 409.
    assert_eq!(send(dav(&base, "MOVE", "/b.txt").set("Destination", "/zz/b.txt"), None).status, 409);
    assert_eq!(send(dav(&base, "MOVE", "/nope").set("Destination", "/b2.txt"), None).status, 404);
    assert_eq!(send(dav(&base, "MOVE", "/n").set("Destination", "/n/m2/n"), None).status, 409);
    // Onto a non-empty directory with Overwrite T → 409.
    send(dav(&base, "MKCOL", "/full"), None);
    send(dav(&base, "PUT", "/full/x"), Some(b"x"));
    send(dav(&base, "MKCOL", "/e"), None);
    assert_eq!(send(dav(&base, "MOVE", "/e").set("Destination", "/full"), None).status, 409);
}

#[test]
fn share_post_ops() {
    let vol = volume(32);
    mkfile(&vol, "/", "f.bin", &pattern(5000, 4));
    let (_srv, base) = share(&vol, false);
    let attr = |p: &str| {
        let ino = lookup_path(&vol, p).unwrap();
        vol.lock().unwrap().getattr(ino).unwrap()
    };

    assert_eq!(send(dav(&base, "POST", "/f.bin?op=truncate&size=100"), Some(b"")).status, 204);
    assert_eq!(attr("/f.bin").size, 100);
    assert_eq!(read_all(&vol, "/f.bin"), &pattern(5000, 4)[..100]);
    assert_eq!(send(dav(&base, "POST", "/f.bin?op=truncate&size=200"), Some(b"")).status, 204);
    assert_eq!(attr("/f.bin").size, 200);

    assert_eq!(send(dav(&base, "POST", "/f.bin?op=chmod&mode=600"), Some(b"")).status, 204);
    assert_eq!(attr("/f.bin").perm, 0o600);
    assert_eq!(send(dav(&base, "POST", "/f.bin?op=chmod&mode=4755"), Some(b"")).status, 204);
    assert_eq!(attr("/f.bin").perm, 0o4755);
    assert_eq!(send(dav(&base, "POST", "/f.bin?op=chmod&mode=9z"), Some(b"")).status, 400);

    assert_eq!(send(dav(&base, "POST", "/f.bin?op=mtime&ns=1600000000987654321"), Some(b"")).status, 204);
    assert_eq!(attr("/f.bin").mtime, Timestamp { secs: 1_600_000_000, nsecs: 987_654_321 });

    let target = "../some dir/tärget+plus&amp";
    let enc = pct(target);
    assert_eq!(send(dav(&base, "POST", &format!("/link%20one?op=symlink&target={enc}")), Some(b"")).status, 204);
    let ino = lookup_path(&vol, "/link one").unwrap();
    {
        let mut fs = vol.lock().unwrap();
        assert_eq!(fs.getattr(ino).unwrap().kind, FileKind::Symlink);
        assert_eq!(fs.readlink(ino).unwrap(), target);
    }
    let (_, p) = propfind(&base, "/link%20one", "0");
    assert_eq!(p[0].symlink.as_deref(), Some(target));
    // Exists → 405, missing parent → 409.
    assert_eq!(send(dav(&base, "POST", &format!("/link%20one?op=symlink&target={enc}")), Some(b"")).status, 405);
    assert_eq!(send(dav(&base, "POST", "/no/l?op=symlink&target=x"), Some(b"")).status, 409);

    assert_eq!(send(dav(&base, "POST", "/missing?op=chmod&mode=644"), Some(b"")).status, 404);
    assert_eq!(send(dav(&base, "POST", "/f.bin?op=explode"), Some(b"")).status, 400);
}

#[test]
fn share_read_only() {
    let vol = volume(32);
    mkfile(&vol, "/", "r.txt", b"read me");
    let (_srv, base) = share(&vol, true);

    let v: serde_json::Value = serde_json::from_slice(&send(dav(&base, "GET", "/.lumendisk/info"), None).body).unwrap();
    assert_eq!(v["read_only"], true);
    assert_eq!(send(dav(&base, "GET", "/r.txt"), None).body, b"read me");
    assert_eq!(propfind(&base, "/", "1").0, 207);

    assert_eq!(send(dav(&base, "PUT", "/r.txt"), Some(b"x")).status, 403);
    assert_eq!(send(dav(&base, "PUT", "/new.txt"), Some(b"x")).status, 403);
    assert_eq!(send(dav(&base, "MKCOL", "/d"), None).status, 403);
    assert_eq!(send(dav(&base, "DELETE", "/r.txt"), None).status, 403);
    assert_eq!(send(dav(&base, "MOVE", "/r.txt").set("Destination", "/s.txt"), None).status, 403);
    assert_eq!(send(dav(&base, "POST", "/r.txt?op=truncate&size=0"), Some(b"")).status, 403);
    assert_eq!(read_all(&vol, "/r.txt"), b"read me");
    assert!(lookup_path(&vol, "/new.txt").is_none());
}

// ================================================================================================
// Portal mode

const PIN: &str = "482913";

fn portal(vol: &Vol) -> (LanServer, String) {
    let s = LanServer::start(vol.clone(), LanMode::Portal { pin: PIN.into() }, "Portal Disk", false).unwrap();
    let base = format!("https://127.0.0.1:{}", s.port());
    pin_to(&s);
    (s, base)
}

fn http_base(s: &LanServer) -> String {
    format!("http://127.0.0.1:{}", s.http_port().expect("portal has an HTTP port"))
}

fn login(base: &str, pin: &str) -> Resp {
    send(agent().post(&format!("{base}/login")).set("Content-Type", "application/x-www-form-urlencoded"), Some(format!("pin={pin}").as_bytes()))
}

/// `name=value` of the session cookie set by `r` (either scheme).
fn session_cookie(r: &Resp) -> String {
    let c = r
        .headers
        .get("set-cookie")
        .and_then(|v| v.iter().find(|c| c.starts_with("__Host-ld_session=") || c.starts_with("ld_session=")))
        .expect("no session cookie");
    c.split(';').next().unwrap().to_string()
}

fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(5) {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn portal_flow_https() {
    portal_flow(true);
}

#[test]
fn portal_flow_http_fallback() {
    portal_flow(false);
}

fn portal_flow(secure: bool) {
    let vol = volume(64);
    mkdir(&vol, "/", "Foto");
    mkfile(&vol, "/", "notes.txt", b"hi");
    let (srv, https) = portal(&vol);
    let base = if secure { https } else { http_base(&srv) };

    // No session: PIN page at /, redirects elsewhere, no data access.
    let r = send(agent().get(&format!("{base}/")), None);
    assert_eq!(r.status, 200);
    let html = r.text();
    assert!(html.contains("name=\"pin\""));
    assert!(html.contains("Portal Disk"));
    assert!(html.contains("Inserisci il PIN"), "Italian by default");
    assert!(!html.contains("notes.txt"));
    if secure {
        assert!(html.contains("Connessione cifrata") && !html.contains("non cifrata"));
    } else {
        assert!(html.contains("Connessione locale non cifrata") && !html.contains("Connessione cifrata"));
    }
    for p in ["/b/Foto/", "/dl/notes.txt"] {
        let r = send(agent().get(&format!("{base}{p}")), None);
        assert_eq!(r.status, 303, "{p}");
        assert_eq!(r.header("Location"), Some("/"));
    }
    let r = send(agent().put(&format!("{base}/up/evil.txt")), Some(b"x"));
    assert_eq!(r.status, 303);
    assert!(lookup_path(&vol, "/evil.txt").is_none());
    // A forged cookie does not help.
    for forged in ["ld_session=00112233445566778899aabbccddeeff", "__Host-ld_session=00112233445566778899aabbccddeeff"] {
        let r = send(agent().get(&format!("{base}/dl/notes.txt")).set("Cookie", forged), None);
        assert_eq!(r.status, 303);
    }

    // English when asked for.
    let r = send(agent().get(&format!("{base}/")).set("Accept-Language", "en-GB,en;q=0.9"), None);
    let t = r.text();
    assert!(t.contains("Enter the 6-digit PIN"));
    assert!(t.contains(if secure { "Encrypted connection" } else { "Unencrypted local connection" }));

    // Wrong PIN.
    let r = login(&base, "000000");
    assert_eq!(r.status, 401);
    assert!(r.text().contains("PIN errato"));
    assert!(r.headers.get("set-cookie").is_none());

    // Right PIN → cookie.
    let r = login(&base, PIN);
    assert_eq!(r.status, 303);
    let name = if secure { "__Host-ld_session=" } else { "ld_session=" };
    let set = r.headers["set-cookie"].iter().find(|c| c.starts_with(name)).unwrap().clone();
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict") && set.contains("Path=/"), "{set}");
    assert_eq!(set.contains("Secure"), secure, "Secure flag iff HTTPS: {set}");
    assert!(!set.contains("Domain"), "{set}");
    let cookie = session_cookie(&r);
    let sid = cookie.trim_start_matches(name);
    assert_eq!(sid.len(), 32);
    assert!(sid.bytes().all(|b| b.is_ascii_hexdigit()));
    let get = |p: &str| send(agent().get(&format!("{base}{p}")).set("Cookie", &format!("other=1; {cookie}")), None);

    // Listing: folders first, sizes, free space, no external resources.
    let r = get("/");
    assert_eq!(r.status, 200);
    let html = r.text();
    assert!(html.contains("Foto") && html.contains("notes.txt"));
    assert!(html.find("Foto").unwrap() < html.find("notes.txt").unwrap(), "folders first");
    assert!(html.contains("2 B"));
    assert!(html.contains("liberi"));
    assert!(html.contains("Nuova cartella"));
    assert!(html.contains(if secure { "Connessione cifrata" } else { "non cifrata" }));
    assert!(r.header("Strict-Transport-Security").is_none(), "HSTS would break the HTTP fallback");
    assert!(!html.contains("lost+found"), "system folder hidden");
    let lower = html.to_lowercase();
    for bad in ["http://", "https://", "//cdn", "src=\"//", "href=\"//", "@import", "url("] {
        assert!(!lower.contains(bad), "external reference {bad:?} in portal page");
    }
    assert!(!lower.contains(" src="), "no external scripts/images");
    let csp = r.header("Content-Security-Policy").unwrap();
    assert!(csp.contains("default-src 'none'"));

    // Upload 5 MB by PUT, download back byte-identical.
    let big = pattern(5 * 1024 * 1024, 77);
    let r = send(agent().put(&format!("{base}/up/Foto/vacanze%20%C3%A8.bin")).set("Cookie", &cookie), Some(&big));
    assert_eq!(r.status, 201, "{}", r.text());
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["name"], "vacanze è.bin");
    assert_eq!(v["size"], big.len() as u64);
    assert_eq!(read_all(&vol, "/Foto/vacanze è.bin"), big);

    let r = get("/dl/Foto/vacanze%20%C3%A8.bin");
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), big.len());
    assert!(r.body == big, "download differs");
    let cd = r.header("Content-Disposition").unwrap();
    assert!(cd.starts_with("attachment") && cd.contains("filename*=UTF-8''vacanze%20%C3%A8.bin"), "{cd}");
    assert!(wait_for(|| srv.stats().downloads == 1));

    // Name collision: never overwrite.
    let r = send(agent().put(&format!("{base}/up/Foto/vacanze%20%C3%A8.bin")).set("Cookie", &cookie), Some(b"second"));
    assert_eq!(r.status, 201);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["name"], "vacanze è (2).bin");
    let r = send(agent().put(&format!("{base}/up/Foto/vacanze%20%C3%A8.bin")).set("Cookie", &cookie), Some(b"third"));
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["name"], "vacanze è (3).bin");
    assert_eq!(read_all(&vol, "/Foto/vacanze è.bin"), big);
    assert_eq!(read_all(&vol, "/Foto/vacanze è (2).bin"), b"second");
    assert_eq!(read_all(&vol, "/Foto/vacanze è (3).bin"), b"third");

    // macOS junk is never written.
    for junk in [".DS_Store", "._vacanze.bin"] {
        let r = send(agent().put(&format!("{base}/up/Foto/{junk}")).set("Cookie", &cookie), Some(b"junk"));
        assert_eq!(r.status, 400, "{junk}");
        assert!(lookup_path(&vol, &format!("/Foto/{junk}")).is_none());
    }
    // Missing folder.
    let r = send(agent().put(&format!("{base}/up/Nope/x.bin")).set("Cookie", &cookie), Some(b"x"));
    assert_eq!(r.status, 409);

    // New folder, then browse it.
    let r = send(agent().post(&format!("{base}/mkdir/Foto/Nuova%20cartella")).set("Cookie", &cookie), Some(b""));
    assert_eq!(r.status, 201);
    let r = send(agent().post(&format!("{base}/mkdir/Foto/Nuova%20cartella")).set("Cookie", &cookie), Some(b""));
    assert_eq!(r.status, 409);
    let r = get("/b/Foto/");
    assert_eq!(r.status, 200);
    let html = r.text();
    assert!(html.contains("vacanze è (2).bin"));
    assert!(html.find("Nuova cartella</div>").unwrap() < html.find("vacanze è.bin").unwrap(), "folders first");
    assert!(html.contains("href=\"/b/Foto/Nuova%20cartella/\""));
    assert!(html.contains("href=\"/dl/Foto/vacanze%20%C3%A8.bin\""));
    assert!(html.contains("5,2 MB"), "human size");
    assert!(html.contains("href=\"/\">Portal Disk</a>"), "breadcrumb");

    let st = srv.stats();
    assert_eq!(st.uploads, 3);
    assert!(st.bytes_received >= big.len() as u64);
    assert!(st.bytes_sent >= big.len() as u64);
    assert!(st.clients >= 1);

    // Logout kills the session.
    let r = send(agent().post(&format!("{base}/logout")).set("Cookie", &cookie), Some(b""));
    assert_eq!(r.status, 303);
    let clear = r.headers["set-cookie"].iter().find(|c| c.starts_with(name)).unwrap().clone();
    assert!(clear.contains("Max-Age=0") && clear.contains("Secure") == secure, "{clear}");
    assert_eq!(get("/b/Foto/").status, 303);
}

#[test]
fn portal_rate_limit() {
    let vol = volume(16);
    let (srv, base) = portal(&vol);
    for _ in 0..5 {
        assert_eq!(login(&base, "111111").status, 401);
    }
    // Sixth attempt is refused even with the right PIN.
    let r = login(&base, PIN);
    assert_eq!(r.status, 429);
    assert!(r.text().contains("Troppi tentativi"));
    assert!(r.headers.get("set-cookie").is_none());
    // The limit is per client, not per scheme: the HTTP port is locked out too.
    let r = login(&http_base(&srv), PIN);
    assert_eq!(r.status, 429);
    assert!(r.headers.get("set-cookie").is_none());
}

#[test]
fn portal_read_only_hides_upload() {
    let vol = volume(16);
    let s = LanServer::start(vol.clone(), LanMode::Portal { pin: PIN.into() }, "RO", true).unwrap();
    pin_to(&s);
    for base in [format!("https://127.0.0.1:{}", s.port()), http_base(&s)] {
        let cookie = session_cookie(&login(&base, PIN));
        let html = send(agent().get(&format!("{base}/")).set("Cookie", &cookie), None).text();
        assert!(!html.contains("id=\"pick\""));
        let r = send(agent().put(&format!("{base}/up/x.bin")).set("Cookie", &cookie), Some(b"x"));
        assert_eq!(r.status, 403);
    }
}

// ================================================================================================

#[test]
fn stop_frees_port() {
    let vol = volume(16);
    let (srv, base) = share(&vol, false);
    let port = srv.port();
    assert_eq!(srv.http_port(), None, "share mode is HTTPS only");
    assert_eq!(send(dav(&base, "GET", "/.lumendisk/info"), None).status, 200);
    let t = Instant::now();
    srv.stop();
    assert!(t.elapsed() < Duration::from_secs(2), "stop() took {:?}", t.elapsed());
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "still accepting after stop()");
    std::net::TcpListener::bind(("0.0.0.0", port)).expect("port not released");
    srv.stop(); // idempotent

    // Drop also stops.
    let (srv, _) = share(&vol, false);
    let port = srv.port();
    drop(srv);
    std::net::TcpListener::bind(("0.0.0.0", port)).expect("port not released on drop");

    // Portal: both ports, even with an idle keep-alive TLS connection and a stalled handshake open.
    let (srv, base) = portal(&vol);
    let (port, hport) = (srv.port(), srv.http_port().unwrap());
    let a = agent(); // keeps its connection pooled
    assert_eq!(send(a.get(&format!("{base}/")), None).status, 200);
    let _stalled = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let t = Instant::now();
    srv.stop();
    assert!(t.elapsed() < Duration::from_secs(2), "stop() took {:?}", t.elapsed());
    for p in [port, hport] {
        assert!(std::net::TcpStream::connect(("127.0.0.1", p)).is_err(), "{p} still accepting after stop()");
        std::net::TcpListener::bind(("0.0.0.0", p)).unwrap_or_else(|e| panic!("port {p} not released: {e}"));
    }
}

// ================================================================================================
// TLS

fn is_transport_err(r: Result<ureq::Response, ureq::Error>) -> bool {
    matches!(r, Err(ureq::Error::Transport(_)))
}

#[test]
fn share_wrong_fingerprint_rejected() {
    let vol = volume(16);
    let (srv, base) = share(&vol, false);
    let url = format!("{base}/.lumendisk/info");
    let auth = format!("Bearer {TOKEN}");
    let fp = srv.fingerprint();
    assert_eq!(fp.len(), 64);
    assert!(fp.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "lowercase hex: {fp}");

    // Right pin: OK, also via the plain `pinned_agent` and with an upper-case / colon form.
    assert_eq!(pinned_agent(&fp).get(&url).set("Authorization", &auth).call().unwrap().status(), 200);
    let colon: Vec<String> = fp.as_bytes().chunks(2).map(|c| String::from_utf8(c.to_vec()).unwrap().to_uppercase()).collect();
    assert_eq!(pinned_agent(&colon.join(":")).get(&url).set("Authorization", &auth).call().unwrap().status(), 200);

    // One flipped bit, all zeros, malformed, empty: transport errors.
    let mut wrong = fp.clone().into_bytes();
    wrong[63] = if wrong[63] == b'0' { b'1' } else { b'0' };
    let wrong = String::from_utf8(wrong).unwrap();
    for bad in [wrong.as_str(), &"0".repeat(64), "not-a-fingerprint", "", &fp[..62]] {
        let r = pinned_agent(bad).get(&url).set("Authorization", &auth).call();
        assert!(is_transport_err(r), "fingerprint {bad:?} accepted");
    }
    // The default (web PKI) agent does not trust the self-signed certificate either.
    assert!(is_transport_err(ureq::AgentBuilder::new().build().get(&url).call()));
    // The hostname is ignored by the pin: "localhost" works the same.
    let r = pinned_agent(&fp).get(&format!("https://localhost:{}/.lumendisk/info", srv.port())).set("Authorization", &auth).call();
    assert_eq!(r.unwrap().status(), 200);
}

#[test]
fn share_plain_http_fails() {
    let vol = volume(16);
    let (srv, _base) = share(&vol, false);
    let plain = format!("http://127.0.0.1:{}/.lumendisk/info", srv.port());
    assert!(is_transport_err(agent().get(&plain).set("Authorization", &format!("Bearer {TOKEN}")).call()));
    // Raw: the server answers plaintext with (at most) a TLS alert, never HTTP.
    let mut s = std::net::TcpStream::connect(("127.0.0.1", srv.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET /.lumendisk/info HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    assert!(!out.starts_with(b"HTTP/"), "plain HTTP answered: {:?}", String::from_utf8_lossy(&out));
    assert!(out.is_empty() || out[0] == 0x15, "expected a TLS alert or nothing, got {out:?}");
    // Server still fine afterwards.
    assert_eq!(send(dav(&_base, "GET", "/.lumendisk/info"), None).status, 200);
}

/// Accept-anything verifier, only to probe which protocol versions the server allows.
#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &rustls::pki_types::CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Handshake with only `version` offered; Ok(negotiated version) or Err.
fn handshake_with(port: u16, version: &'static rustls::SupportedProtocolVersion) -> Result<rustls::ProtocolVersion, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[version])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
        .with_no_client_auth();
    let mut conn = rustls::ClientConnection::new(Arc::new(cfg), "localhost".try_into().unwrap()).unwrap();
    let mut tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp).map_err(|e| e.to_string())?;
    }
    Ok(conn.protocol_version().unwrap())
}

fn openssl_s_client(port: u16, flag: &str) -> Option<(bool, String)> {
    let out = std::process::Command::new("/usr/bin/openssl")
        .args(["s_client", flag, "-showcerts", "-connect", &format!("127.0.0.1:{port}")])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if text.contains("unknown option") || text.contains("usage:") {
        return None; // this openssl lacks the flag
    }
    Some((text.contains("BEGIN CERTIFICATE"), text))
}

#[test]
fn share_requires_tls13() {
    let vol = volume(16);
    let (srv, base) = share(&vol, false);

    // rustls side: TLS 1.3 negotiated, TLS 1.2 refused.
    assert_eq!(handshake_with(srv.port(), &rustls::version::TLS13), Ok(rustls::ProtocolVersion::TLSv1_3));
    assert!(handshake_with(srv.port(), &rustls::version::TLS12).is_err(), "share accepted TLS 1.2");
    let mut s = tls_stream(&base);
    write!(s, "GET /.lumendisk/info HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    assert!(out.starts_with(b"HTTP/1.1 200"), "{}", String::from_utf8_lossy(&out));
    assert_eq!(s.conn.protocol_version(), Some(rustls::ProtocolVersion::TLSv1_3));

    // openssl side (LibreSSL on macOS), when available.
    if let Some((ok, text)) = openssl_s_client(srv.port(), "-tls1_3") {
        assert!(ok && text.contains("TLSv1.3"), "openssl -tls1_3:\n{text}");
        if let Some((ok12, text12)) = openssl_s_client(srv.port(), "-tls1_2") {
            assert!(!ok12, "openssl -tls1_2 succeeded:\n{text12}");
        }
    } else {
        eprintln!("openssl without -tls1_3: relied on the rustls-side check");
    }
}

#[test]
fn portal_allows_tls12_and_tls13() {
    let vol = volume(16);
    let (srv, _) = portal(&vol);
    assert_eq!(handshake_with(srv.port(), &rustls::version::TLS13), Ok(rustls::ProtocolVersion::TLSv1_3));
    assert_eq!(handshake_with(srv.port(), &rustls::version::TLS12), Ok(rustls::ProtocolVersion::TLSv1_2));
}

/// Extract the first PEM certificate from `text`, run it through `openssl x509`.
fn openssl_x509(text: &str, args: &[&str]) -> String {
    let start = text.find("-----BEGIN CERTIFICATE-----").unwrap();
    let end = text.find("-----END CERTIFICATE-----").unwrap() + "-----END CERTIFICATE-----".len();
    let mut child = std::process::Command::new("/usr/bin/openssl")
        .arg("x509")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(text[start..end].as_bytes()).unwrap();
    String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
}

#[test]
fn certificate_contents() {
    let vol = volume(16);
    let (srv, _) = share(&vol, false);
    let Some((true, text)) = openssl_s_client(srv.port(), "-tls1_3") else {
        eprintln!("openssl unavailable: skipped");
        return;
    };
    // The fingerprint is the SHA-256 of the certificate actually served.
    let fp = openssl_x509(&text, &["-noout", "-fingerprint", "-sha256"]);
    let fp: String = fp.split('=').nth(1).unwrap().trim().replace(':', "").to_lowercase();
    assert_eq!(fp, srv.fingerprint());
    let info = openssl_x509(&text, &["-noout", "-text"]);
    assert!(info.contains("CN=LumenDisk") || info.contains("CN = LumenDisk"), "{info}");
    assert!(info.contains("prime256v1") || info.contains("P-256"), "{info}");
    assert!(info.contains("ecdsa-with-SHA256"), "{info}");
    assert!(info.contains("DNS:localhost") && info.contains("IP Address:127.0.0.1"), "{info}");
    for ip in lan_addresses() {
        assert!(info.contains(&format!("IP Address:{ip}")), "SAN {ip} missing:\n{info}");
    }
    assert!(info.contains("TLS Web Server Authentication"), "{info}");
}

#[test]
fn fingerprint_stable_then_rotates() {
    let vol = volume(16);
    let (srv, base) = share(&vol, false);
    let fp1 = srv.fingerprint();
    for _ in 0..3 {
        assert_eq!(send(dav(&base, "GET", "/.lumendisk/info"), None).status, 200);
        assert_eq!(srv.fingerprint(), fp1);
    }
    drop(srv);

    let (srv2, base2) = share(&vol, false);
    let fp2 = srv2.fingerprint();
    assert_ne!(fp1, fp2, "certificate reused across restarts");
    assert_eq!(send(dav(&base2, "GET", "/.lumendisk/info"), None).status, 200);
    // The old pin no longer opens the new server.
    let r = pinned_agent(&fp1).get(&format!("{base2}/.lumendisk/info")).set("Authorization", &format!("Bearer {TOKEN}")).call();
    assert!(is_transport_err(r));
}

#[test]
fn portal_sessions_bound_to_scheme() {
    let vol = volume(16);
    mkfile(&vol, "/", "notes.txt", b"hi");
    let (srv, https) = portal(&vol);
    let http = http_base(&srv);
    let dl = |base: &str, cookie: &str| send(agent().get(&format!("{base}/dl/notes.txt")).set("Cookie", cookie), None).status;

    let tls_cookie = session_cookie(&login(&https, PIN));
    assert!(tls_cookie.starts_with("__Host-ld_session="));
    let tls_sid = tls_cookie.split('=').nth(1).unwrap().to_string();
    let plain_cookie = session_cookie(&login(&http, PIN));
    assert!(plain_cookie.starts_with("ld_session="));
    let plain_sid = plain_cookie.split('=').nth(1).unwrap().to_string();

    assert_eq!(dl(&https, &tls_cookie), 200);
    assert_eq!(dl(&http, &plain_cookie), 200);
    // An HTTPS session is never valid over HTTP, whatever the cookie is called...
    assert_eq!(dl(&http, &tls_cookie), 303);
    assert_eq!(dl(&http, &format!("ld_session={tls_sid}")), 303);
    // ...and an id that travelled in clear never opens the HTTPS side.
    assert_eq!(dl(&https, &plain_cookie), 303, "browsers send non-Secure cookies to every port");
    assert_eq!(dl(&https, &format!("__Host-ld_session={plain_sid}")), 303);
    // Both cookies present (what a browser sends to the HTTPS port): the right one is used.
    assert_eq!(dl(&https, &format!("{plain_cookie}; {tls_cookie}")), 200);
    assert_eq!(dl(&http, &format!("{tls_cookie}; {plain_cookie}")), 200);
}

#[test]
fn stalled_handshake_does_not_block_others() {
    let vol = volume(16);
    let (srv, base) = share(&vol, false);
    // Clients that connect and never finish (or never start) a handshake.
    let mut stalled = Vec::new();
    for i in 0..8 {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", srv.port())).unwrap();
        if i % 2 == 0 {
            s.write_all(&[0x16, 0x03, 0x01, 0x02, 0x00, 0x01]).unwrap(); // truncated ClientHello
        }
        stalled.push(s);
    }
    let t = Instant::now();
    for _ in 0..5 {
        assert_eq!(send(dav(&base, "GET", "/.lumendisk/info"), None).status, 200);
    }
    assert!(t.elapsed() < Duration::from_secs(5), "requests delayed by stalled handshakes: {:?}", t.elapsed());
    // Parallel clients with large transfers work concurrently.
    let data = pattern(2_000_000, 5);
    mkfile(&vol, "/", "p.bin", &data);
    let fp = srv.fingerprint();
    let hs: Vec<_> = (0..4)
        .map(|_| {
            let (b, fp) = (base.clone(), fp.clone());
            std::thread::spawn(move || {
                let r = pinned_agent(&fp).get(&format!("{b}/p.bin")).set("Authorization", &format!("Bearer {TOKEN}")).call().unwrap();
                let mut v = Vec::new();
                r.into_reader().read_to_end(&mut v).unwrap();
                v
            })
        })
        .collect();
    for h in hs {
        assert!(h.join().unwrap() == data);
    }
    drop(stalled);
}
