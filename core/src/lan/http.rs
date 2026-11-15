//! HTTP plumbing shared by the WebDAV share and the portal: responses, header access,
//! percent-encoding, path resolution, byte ranges, and the streaming body reader / writer.

use std::io::{self, Read};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tiny_http::{Header, Request, Response, ResponseBox, StatusCode};

use super::{Ctx, CHUNK};
use crate::vfs::{Attr, FileKind, FileSystem, FsError, FsResult, Ino};

pub fn hdr(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .unwrap_or_else(|_| Header::from_bytes(name.as_bytes(), &b"-"[..]).expect("valid header name"))
}

pub fn get_header(rq: &Request, name: &str) -> Option<String> {
    rq.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str().to_string())
}

/// Response with a short text body.
pub fn text(code: u16, msg: &str) -> ResponseBox {
    let body = format!("{msg}\n").into_bytes();
    let len = body.len();
    Response::new(
        StatusCode(code),
        vec![hdr("Content-Type", "text/plain; charset=utf-8")],
        Box::new(io::Cursor::new(body)) as Box<dyn Read + Send>,
        Some(len),
        None,
    )
}

pub fn bytes(code: u16, content_type: &str, body: Vec<u8>) -> ResponseBox {
    let len = body.len();
    Response::new(
        StatusCode(code),
        vec![hdr("Content-Type", content_type)],
        Box::new(io::Cursor::new(body)) as Box<dyn Read + Send>,
        Some(len),
        None,
    )
    .with_chunked_threshold(usize::MAX)
}

pub fn empty(code: u16) -> ResponseBox {
    Response::new(StatusCode(code), vec![], Box::new(io::empty()) as Box<dyn Read + Send>, Some(0), None)
}

pub fn fs_err(e: &FsError) -> ResponseBox {
    let code = match e {
        FsError::NotFound | FsError::Stale => 404,
        FsError::Exists => 405,
        FsError::NotEmpty | FsError::NotDir | FsError::IsDir => 409,
        FsError::NoSpace => 507,
        FsError::ReadOnly | FsError::Perm => 403,
        FsError::InvalidName | FsError::NameTooLong | FsError::Invalid(_) => 400,
        _ => 500,
    };
    text(code, &e.to_string())
}

// ---------------------------------------------------------------------------------------------
// Percent-encoding

fn unhex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decode to UTF-8. `plus_space`: also map '+' to ' ' (form / query encoding).
pub fn pct_decode(s: &str, plus_space: bool) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = unhex(*b.get(i + 1)?)?;
                let l = unhex(*b.get(i + 2)?)?;
                out.push(h << 4 | l);
                i += 3;
            }
            b'+' if plus_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Percent-encode one path segment (everything except RFC 3986 unreserved characters).
pub fn pct_encode(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// Encode a component list as an absolute href ("/" for the root).
pub fn href(comps: &[String], dir: bool) -> String {
    let mut s = String::new();
    for c in comps {
        s.push('/');
        s.push_str(&pct_encode(c));
    }
    if s.is_empty() || dir {
        s.push('/');
    }
    s
}

/// Split a request target into (decoded path components, raw query).
/// Rejects ".." components, NULs and invalid encodings.
pub fn split_target(url: &str) -> Option<(Vec<String>, String)> {
    let (p, q) = match url.find('?') {
        Some(i) => (&url[..i], &url[i + 1..]),
        None => (url, ""),
    };
    let p = p.split('#').next().unwrap_or("");
    Some((parse_path(p)?, q.to_string()))
}

/// Decode an (encoded) absolute path into components.
pub fn parse_path(p: &str) -> Option<Vec<String>> {
    let mut comps = Vec::new();
    for seg in p.split('/') {
        let d = pct_decode(seg, false)?;
        if d.is_empty() || d == "." {
            continue;
        }
        if d == ".." || d.contains('\0') || d.contains('/') {
            return None;
        }
        comps.push(d);
    }
    Some(comps)
}

pub fn query_params(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            Some((pct_decode(k, true)?, pct_decode(v, true)?))
        })
        .collect()
}

pub fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------------------------------
// Path resolution

pub fn resolve(fs: &mut dyn FileSystem, comps: &[String]) -> FsResult<Ino> {
    let mut ino = fs.root_ino();
    for c in comps {
        ino = fs.lookup(ino, c).map_err(|e| if e == FsError::NotDir { FsError::NotFound } else { e })?;
    }
    Ok(ino)
}

/// Resolve the parent directory of `comps` (must be non-empty). Errors with `NotFound` when the
/// parent is missing or not a directory.
pub fn resolve_parent(fs: &mut dyn FileSystem, comps: &[String]) -> FsResult<Ino> {
    let p = resolve(fs, &comps[..comps.len() - 1])?;
    if fs.getattr(p)?.kind != FileKind::Dir {
        return Err(FsError::NotFound);
    }
    Ok(p)
}

pub fn mtime_ns(a: &Attr) -> i128 {
    a.mtime.secs as i128 * 1_000_000_000 + a.mtime.nsecs as i128
}

pub fn http_date(a: &Attr) -> String {
    let secs = a.mtime.secs.max(0) as u64;
    httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}

pub fn etag(a: &Attr) -> String {
    format!("\"{:x}-{:x}-{:x}\"", a.ino, a.size, mtime_ns(a))
}

// ---------------------------------------------------------------------------------------------
// Ranges

pub enum RangeReq {
    Full,
    Part(u64, u64),
    Unsatisfiable,
}

/// Parse a single `Range: bytes=a-b | a- | -n` against `size`. Multi-range → full body.
pub fn parse_range(h: Option<&str>, size: u64) -> RangeReq {
    let Some(h) = h else { return RangeReq::Full };
    let Some(spec) = h.trim().strip_prefix("bytes=") else { return RangeReq::Full };
    if spec.contains(',') {
        return RangeReq::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else { return RangeReq::Full };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let Ok(n) = b.parse::<u64>() else { return RangeReq::Full };
        if n == 0 || size == 0 {
            return RangeReq::Unsatisfiable;
        }
        let n = n.min(size);
        return RangeReq::Part(size - n, size - 1);
    }
    let Ok(start) = a.parse::<u64>() else { return RangeReq::Full };
    let end = if b.is_empty() {
        size.saturating_sub(1)
    } else {
        match b.parse::<u64>() {
            Ok(e) if e >= start => e.min(size.saturating_sub(1)),
            _ => return RangeReq::Full,
        }
    };
    if start >= size {
        return RangeReq::Unsatisfiable;
    }
    RangeReq::Part(start, end)
}

/// `Content-Range: bytes a-b/total` or `bytes a-b/*` (request side, for partial PUT).
pub fn parse_content_range(h: &str) -> Option<(u64, u64)> {
    let s = h.trim().strip_prefix("bytes")?.trim_start();
    let (r, _total) = s.split_once('/')?;
    let (a, b) = r.trim().split_once('-')?;
    let a: u64 = a.trim().parse().ok()?;
    let b: u64 = b.trim().parse().ok()?;
    (b >= a).then_some((a, b))
}

// ---------------------------------------------------------------------------------------------
// Streaming download

/// Reads `[pos, end)` of a file in CHUNK-sized pieces, locking the volume once per chunk.
pub struct FsReader {
    ctx: Arc<Ctx>,
    ino: Ino,
    pos: u64,
    end: u64,
    buf: Vec<u8>,
    off: usize,
    count_download: bool,
}

impl FsReader {
    pub fn new(ctx: Arc<Ctx>, ino: Ino, start: u64, end_excl: u64, count_download: bool) -> Self {
        FsReader { ctx, ino, pos: start, end: end_excl, buf: Vec::new(), off: 0, count_download }
    }
}

impl Read for FsReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.off >= self.buf.len() {
            if self.pos >= self.end {
                if self.count_download {
                    // Everything before this call has been written to the socket.
                    self.count_download = false;
                    self.ctx.downloads.fetch_add(1, Ordering::Relaxed);
                }
                return Ok(0);
            }
            if self.ctx.is_stopped() {
                return Err(io::Error::other("server stopped"));
            }
            let want = ((self.end - self.pos).min(CHUNK as u64)) as u32;
            let (ino, pos) = (self.ino, self.pos);
            let data = self.ctx.fs(|fs| fs.read(ino, pos, want)).map_err(|e| io::Error::other(e.to_string()))?;
            if data.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "file shrank during transfer"));
            }
            self.pos += data.len() as u64;
            self.buf = data;
            self.off = 0;
        }
        let n = out.len().min(self.buf.len() - self.off);
        out[..n].copy_from_slice(&self.buf[self.off..self.off + n]);
        self.off += n;
        self.ctx.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// Build a (possibly ranged) streaming response for a regular file.
pub fn file_response(ctx: &Arc<Ctx>, rq: &Request, ino: Ino, attr: &Attr, is_head: bool, extra: Vec<Header>) -> ResponseBox {
    let size = attr.size;
    let range = parse_range(get_header(rq, "Range").as_deref(), size);
    let mut headers = vec![
        hdr("Accept-Ranges", "bytes"),
        hdr("Last-Modified", &http_date(attr)),
        hdr("ETag", &etag(attr)),
    ];
    headers.extend(extra);
    let (code, start, end_excl) = match range {
        RangeReq::Full => (200, 0, size),
        RangeReq::Part(a, b) => {
            headers.push(hdr("Content-Range", &format!("bytes {a}-{b}/{size}")));
            (206, a, b + 1)
        }
        RangeReq::Unsatisfiable => {
            let mut r = text(416, "range not satisfiable");
            r.add_header(hdr("Content-Range", &format!("bytes */{size}")));
            return r;
        }
    };
    if !headers.iter().any(|h| h.field.equiv("Content-Type")) {
        headers.push(hdr("Content-Type", "application/octet-stream"));
    }
    let len = end_excl - start;
    let reader: Box<dyn Read + Send> = if is_head {
        Box::new(io::empty())
    } else {
        Box::new(FsReader::new(ctx.clone(), ino, start, end_excl, code == 200))
    };
    Response::new(StatusCode(code), headers, reader, Some(len as usize), None).with_chunked_threshold(usize::MAX)
}

// ---------------------------------------------------------------------------------------------
// Streaming upload

pub enum UploadError {
    /// Reading the request body failed (client went away).
    Body(io::Error),
    Fs(FsError),
}

/// Copy the request body into `ino` starting at `offset`, CHUNK bytes per lock acquisition.
/// Returns the number of bytes written.
pub fn write_body(ctx: &Ctx, body: &mut dyn Read, ino: Ino, offset: u64) -> Result<u64, UploadError> {
    let mut buf = vec![0u8; CHUNK];
    let mut off = offset;
    loop {
        if ctx.is_stopped() {
            return Err(UploadError::Fs(FsError::Io("server stopped".into())));
        }
        let mut n = 0;
        while n < buf.len() {
            match body.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(UploadError::Body(e)),
            }
        }
        if n == 0 {
            break;
        }
        ctx.bytes_received.fetch_add(n as u64, Ordering::Relaxed);
        let data = &buf[..n];
        ctx.fs(|fs| -> FsResult<()> {
            let mut w = 0usize;
            while w < data.len() {
                let k = fs.write(ino, off + w as u64, &data[w..])? as usize;
                if k == 0 {
                    return Err(FsError::Io("short write".into()));
                }
                w += k;
            }
            Ok(())
        })
        .map_err(UploadError::Fs)?;
        off += n as u64;
        if n < buf.len() {
            // Short fill means the body reader hit EOF.
            break;
        }
    }
    Ok(off - offset)
}

pub fn upload_err(e: UploadError) -> ResponseBox {
    match e {
        UploadError::Body(e) => text(400, &format!("upload interrupted: {e}")),
        UploadError::Fs(e) => fs_err(&e),
    }
}

// ---------------------------------------------------------------------------------------------

pub fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            // Not representable in XML 1.0.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => o.push('\u{FFFD}'),
            c => o.push(c),
        }
    }
    o
}

/// Read at most `limit` bytes of a small request body (forms).
pub fn read_small_body(rq: &mut Request, limit: usize) -> Option<String> {
    let mut v = Vec::new();
    Read::take(rq.as_reader(), limit as u64 + 1).read_to_end(&mut v).ok()?;
    if v.len() > limit {
        return None;
    }
    String::from_utf8(v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert!(matches!(parse_range(Some("bytes=0-9"), 100), RangeReq::Part(0, 9)));
        assert!(matches!(parse_range(Some("bytes=90-"), 100), RangeReq::Part(90, 99)));
        assert!(matches!(parse_range(Some("bytes=90-500"), 100), RangeReq::Part(90, 99)));
        assert!(matches!(parse_range(Some("bytes=-10"), 100), RangeReq::Part(90, 99)));
        assert!(matches!(parse_range(Some("bytes=100-"), 100), RangeReq::Unsatisfiable));
        assert!(matches!(parse_range(Some("bytes=0-1,5-6"), 100), RangeReq::Full));
        assert!(matches!(parse_range(None, 100), RangeReq::Full));
        assert_eq!(parse_content_range("bytes 10-19/*"), Some((10, 19)));
        assert_eq!(parse_content_range("bytes 10-19/100"), Some((10, 19)));
        assert_eq!(parse_content_range("bytes */100"), None);
    }

    #[test]
    fn paths() {
        let (c, q) = split_target("/a%20b/%C3%A8/./c?x=1").unwrap();
        assert_eq!(c, vec!["a b", "è", "c"]);
        assert_eq!(q, "x=1");
        assert!(split_target("/a/../b").is_none());
        assert!(split_target("/a/%2e%2e/b").is_none());
        assert!(split_target("/a/%2Fb").is_none());
        assert!(split_target("/a%zz").is_none());
        assert_eq!(href(&[], true), "/");
        assert_eq!(href(&["a b".into(), "è".into()], true), "/a%20b/%C3%A8/");
        assert_eq!(query_params("target=..%2Fx+y&size=3"), vec![("target".into(), "../x y".into()), ("size".into(), "3".into())]);
    }
}
