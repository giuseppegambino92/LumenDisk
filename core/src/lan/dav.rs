//! Share mode: the WebDAV dialect spoken between two LumenDisk installations.
//!
//! See the module docs in `mod.rs`; the wire contract is:
//! * `Authorization: Bearer <token>` on every request (401 otherwise).
//! * `GET /.lumendisk/info` → JSON volume description.
//! * `PROPFIND` (Depth 0/1) → 207 with `DAV:` props plus `L:mode`, `L:mtime_ns`, `L:symlink`
//!   in `urn:lumendisk`.
//! * `GET`/`HEAD` (single byte range), `PUT` (whole, or partial with `Content-Range`), `MKCOL`,
//!   `DELETE`, `MOVE` (`Destination`, `Overwrite`), and
//!   `POST ?op=truncate|chmod|mtime|symlink`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tiny_http::{Request, ResponseBox};

use super::http::*;
use super::{ct_eq, Ctx};
use crate::vfs::{Attr, FileKind, FsError, FsResult, SetAttr, Timestamp};

pub fn handle(ctx: &Arc<Ctx>, token: &str, rq: &mut Request) -> ResponseBox {
    // Unauthenticated pairing proof (see `pairing_proof`): lets a client verify it is talking
    // to the Mac that showed the pairing code before it reveals any credential.
    if rq.method().as_str().eq_ignore_ascii_case("GET") {
        if let Some(q) = rq.url().strip_prefix("/.lumendisk/proof?nonce=") {
            let nonce: String = q.chars().take_while(|c| c.is_ascii_hexdigit()).take(64).collect();
            if nonce.len() >= 32 {
                return text(200, &super::pairing_proof(token, &nonce, &ctx.fingerprint));
            }
            return text(400, "bad nonce");
        }
    }
    let authorized = get_header(rq, "Authorization")
        .and_then(|h| {
            let h = h.trim();
            let (scheme, rest) = h.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim().to_string())
        })
        .map(|t| ct_eq(t.as_bytes(), token.as_bytes()))
        .unwrap_or(false);
    if !authorized {
        let mut r = text(401, "unauthorized");
        r.add_header(hdr("WWW-Authenticate", "Bearer realm=\"LumenDisk\""));
        return r;
    }

    let method = rq.method().as_str().to_ascii_uppercase();
    let Some((comps, query)) = split_target(rq.url()) else {
        return text(400, "bad path");
    };

    if comps.first().map(|s| s.as_str()) == Some(".lumendisk") {
        return if comps.len() == 2 && comps[1] == "info" && (method == "GET" || method == "HEAD") {
            info(ctx)
        } else {
            text(404, "not found")
        };
    }

    let mutating = matches!(method.as_str(), "PUT" | "MKCOL" | "DELETE" | "MOVE" | "POST" | "COPY" | "PROPPATCH");
    if mutating && ctx.read_only {
        return text(403, "read-only volume");
    }

    match method.as_str() {
        "OPTIONS" => {
            let mut r = empty(200);
            r.add_header(hdr("DAV", "1"));
            r.add_header(hdr("Allow", "OPTIONS, GET, HEAD, PUT, DELETE, MKCOL, MOVE, POST, PROPFIND"));
            r
        }
        "PROPFIND" => propfind(ctx, rq, &comps),
        "GET" | "HEAD" => get(ctx, rq, &comps, method == "HEAD"),
        "PUT" => put(ctx, rq, &comps),
        "MKCOL" => mkcol(ctx, &comps),
        "DELETE" => delete(ctx, &comps),
        "MOVE" => mv(ctx, rq, &comps),
        "POST" => post(ctx, &comps, &query),
        _ => text(501, "method not implemented"),
    }
}

fn info(ctx: &Ctx) -> ResponseBox {
    let (vi, st) = ctx.fs(|fs| (fs.volume_info(), fs.statfs()));
    let st = st.unwrap_or_default();
    let body = serde_json::json!({
        "version": 1,
        "name": ctx.name,
        "fs_type": vi.fs_type.label(),
        "uuid": vi.uuid,
        "total_bytes": st.total_bytes,
        "free_bytes": st.free_bytes,
        "read_only": ctx.read_only,
    });
    bytes(200, "application/json", serde_json::to_vec(&body).unwrap_or_default())
}

// ---------------------------------------------------------------------------------------------
// PROPFIND

fn prop_xml(out: &mut String, href_s: &str, a: &Attr, link: Option<&str>) {
    out.push_str("<D:response><D:href>");
    out.push_str(&xml_escape(href_s));
    out.push_str("</D:href><D:propstat><D:prop>");
    match a.kind {
        FileKind::Dir => out.push_str("<D:resourcetype><D:collection/></D:resourcetype><D:getcontentlength>0</D:getcontentlength>"),
        _ => {
            out.push_str("<D:resourcetype/>");
            out.push_str(&format!("<D:getcontentlength>{}</D:getcontentlength>", a.size));
        }
    }
    out.push_str(&format!(
        "<D:getlastmodified>{}</D:getlastmodified><D:getetag>{}</D:getetag><L:mode>{:o}</L:mode><L:mtime_ns>{}</L:mtime_ns>",
        http_date(a),
        xml_escape(&etag(a)),
        a.perm & 0o7777,
        mtime_ns(a)
    ));
    if let Some(t) = link {
        out.push_str("<L:symlink>");
        out.push_str(&xml_escape(t));
        out.push_str("</L:symlink>");
    }
    out.push_str("</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n");
}

fn propfind(ctx: &Ctx, rq: &mut Request, comps: &[String]) -> ResponseBox {
    // The request body (prop selection) is ignored: we always answer with the full set.
    let _ = std::io::copy(&mut std::io::Read::take(rq.as_reader(), 1 << 20), &mut std::io::sink());
    let depth1 = get_header(rq, "Depth").map(|d| d.trim() != "0").unwrap_or(true);

    let me = ctx.fs(|fs| -> FsResult<_> {
        let ino = resolve(fs, comps)?;
        let a = fs.getattr(ino)?;
        let link = if a.kind == FileKind::Symlink { Some(fs.readlink(ino)?) } else { None };
        let entries = if depth1 && a.kind == FileKind::Dir { fs.readdir(ino)? } else { Vec::new() };
        Ok((a, link, entries))
    });
    let (attr, link, entries) = match me {
        Ok(v) => v,
        Err(e) => return fs_err(&e),
    };

    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\" xmlns:L=\"urn:lumendisk\">\n");
    prop_xml(&mut out, &href(comps, attr.kind == FileKind::Dir), &attr, link.as_deref());

    // Children: attributes fetched in batches so the volume lock is released regularly.
    let mut child = comps.to_vec();
    child.push(String::new());
    for batch in entries.chunks(128) {
        let attrs: Vec<_> = ctx.fs(|fs| {
            batch
                .iter()
                .map(|e| {
                    let a = fs.getattr(e.ino).ok()?;
                    let l = if a.kind == FileKind::Symlink { Some(fs.readlink(e.ino).unwrap_or_default()) } else { None };
                    Some((a, l))
                })
                .collect()
        });
        for (e, al) in batch.iter().zip(attrs) {
            let Some((a, l)) = al else { continue };
            *child.last_mut().unwrap() = e.name.clone();
            prop_xml(&mut out, &href(&child, a.kind == FileKind::Dir), &a, l.as_deref());
        }
    }
    out.push_str("</D:multistatus>\n");
    bytes(207, "application/xml; charset=utf-8", out.into_bytes())
}

// ---------------------------------------------------------------------------------------------

fn get(ctx: &Arc<Ctx>, rq: &Request, comps: &[String], is_head: bool) -> ResponseBox {
    let r = ctx.fs(|fs| -> FsResult<_> {
        let ino = resolve(fs, comps)?;
        Ok((ino, fs.getattr(ino)?))
    });
    match r {
        Err(e) => fs_err(&e),
        Ok((_, a)) if a.kind != FileKind::File => text(405, "not a regular file"),
        Ok((ino, a)) => file_response(ctx, rq, ino, &a, is_head, vec![]),
    }
}

fn put(ctx: &Ctx, rq: &mut Request, comps: &[String]) -> ResponseBox {
    if comps.is_empty() {
        return text(405, "cannot PUT the root");
    }
    let partial = match get_header(rq, "Content-Range") {
        Some(h) => match parse_content_range(&h) {
            Some(r) => Some(r),
            None => return text(400, "bad Content-Range"),
        },
        None => None,
    };
    let name = comps.last().unwrap().clone();
    let prep = ctx.fs(|fs| -> FsResult<(u64, bool)> {
        let parent = resolve_parent(fs, comps).map_err(|_| FsError::Invalid("parent".into()))?;
        match fs.lookup(parent, &name) {
            Ok(ino) => {
                let a = fs.getattr(ino)?;
                match a.kind {
                    FileKind::Dir => Err(FsError::IsDir),
                    FileKind::Symlink => {
                        fs.unlink(parent, &name)?;
                        let pa = fs.getattr(parent)?;
                        Ok((fs.create(parent, &name, 0o644, pa.uid, pa.gid)?, true))
                    }
                    FileKind::File => {
                        if partial.is_none() && a.size != 0 {
                            fs.setattr(ino, &SetAttr { size: Some(0), ..Default::default() })?;
                        }
                        Ok((ino, false))
                    }
                }
            }
            Err(FsError::NotFound) => {
                let pa = fs.getattr(parent)?;
                Ok((fs.create(parent, &name, 0o644, pa.uid, pa.gid)?, true))
            }
            Err(e) => Err(e),
        }
    });
    let (ino, created) = match prep {
        Ok(v) => v,
        Err(FsError::Invalid(s)) if s == "parent" => return text(409, "parent collection missing"),
        Err(FsError::IsDir) => return text(405, "is a collection"),
        Err(e) => return fs_err(&e),
    };
    let (offset, limit) = match partial {
        Some((a, b)) => (a, b - a + 1),
        None => (0, u64::MAX),
    };
    let mut body = std::io::Read::take(rq.as_reader(), limit);
    match write_body(ctx, &mut body, ino, offset) {
        Ok(_) => {
            if partial.is_none() {
                ctx.uploads.fetch_add(1, Ordering::Relaxed);
            }
            empty(if created { 201 } else { 204 })
        }
        Err(e) => upload_err(e),
    }
}

fn mkcol(ctx: &Ctx, comps: &[String]) -> ResponseBox {
    if comps.is_empty() {
        return text(405, "already exists");
    }
    let name = comps.last().unwrap();
    let r = ctx.fs(|fs| -> FsResult<Result<(), u16>> {
        let parent = match resolve_parent(fs, comps) {
            Ok(p) => p,
            Err(FsError::NotFound) => return Ok(Err(409)),
            Err(e) => return Err(e),
        };
        match fs.lookup(parent, name) {
            Ok(_) => return Ok(Err(405)),
            Err(FsError::NotFound) => {}
            Err(e) => return Err(e),
        }
        let pa = fs.getattr(parent)?;
        fs.mkdir(parent, name, 0o755, pa.uid, pa.gid)?;
        Ok(Ok(()))
    });
    match r {
        Ok(Ok(())) => empty(201),
        Ok(Err(409)) => text(409, "parent collection missing"),
        Ok(Err(c)) => text(c, "already exists"),
        Err(e) => fs_err(&e),
    }
}

fn delete(ctx: &Ctx, comps: &[String]) -> ResponseBox {
    if comps.is_empty() {
        return text(403, "cannot delete the root");
    }
    let name = comps.last().unwrap();
    let r = ctx.fs(|fs| -> FsResult<()> {
        let parent = resolve_parent(fs, comps)?;
        let ino = fs.lookup(parent, name)?;
        if fs.getattr(ino)?.kind == FileKind::Dir {
            fs.rmdir(parent, name)
        } else {
            fs.unlink(parent, name)
        }
    });
    match r {
        Ok(()) => empty(204),
        Err(FsError::NotEmpty) => text(409, "collection not empty"),
        Err(e) => fs_err(&e),
    }
}

/// Destination header → components (absolute URL or absolute path).
fn destination(h: &str) -> Option<Vec<String>> {
    let h = h.trim();
    let path = if let Some(i) = h.find("://") {
        let rest = &h[i + 3..];
        match rest.find('/') {
            Some(j) => &rest[j..],
            None => "/",
        }
    } else {
        h
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    parse_path(path)
}

fn mv(ctx: &Ctx, rq: &Request, comps: &[String]) -> ResponseBox {
    let Some(dest) = get_header(rq, "Destination") else {
        return text(400, "missing Destination");
    };
    let Some(dst) = destination(&dest) else {
        return text(400, "bad Destination");
    };
    if comps.is_empty() || dst.is_empty() {
        return text(403, "cannot move the root");
    }
    if dst == comps {
        return text(403, "source and destination are the same");
    }
    if dst.len() > comps.len() && dst[..comps.len()] == *comps {
        return text(409, "cannot move a collection into itself");
    }
    let overwrite = get_header(rq, "Overwrite").map(|v| !v.trim().eq_ignore_ascii_case("F")).unwrap_or(true);
    let (sname, dname) = (comps.last().unwrap(), dst.last().unwrap());
    let r = ctx.fs(|fs| -> FsResult<Result<bool, u16>> {
        let sp = resolve_parent(fs, comps)?;
        let sino = fs.lookup(sp, sname)?;
        let dp = match resolve_parent(fs, &dst) {
            Ok(p) => p,
            Err(FsError::NotFound) => return Ok(Err(409)),
            Err(e) => return Err(e),
        };
        let existed = match fs.lookup(dp, dname) {
            // Same inode under another spelling (case-insensitive FS): a case-only rename.
            Ok(dino) if dino == sino => false,
            Ok(_) => {
                if !overwrite {
                    return Ok(Err(412));
                }
                // `rename` replaces a file or an empty directory (POSIX); a non-empty
                // destination directory fails with NotEmpty → 409.
                true
            }
            Err(FsError::NotFound) => false,
            Err(e) => return Err(e),
        };
        fs.rename(sp, sname, dp, dname)?;
        Ok(Ok(existed))
    });
    match r {
        Ok(Ok(true)) => empty(204),
        Ok(Ok(false)) => empty(201),
        Ok(Err(412)) => text(412, "destination exists"),
        Ok(Err(c)) => text(c, "destination parent missing"),
        Err(FsError::NotEmpty) => text(409, "destination collection not empty"),
        Err(e) => fs_err(&e),
    }
}

fn post(ctx: &Ctx, comps: &[String], query: &str) -> ResponseBox {
    let params = query_params(query);
    let op = param(&params, "op").unwrap_or("");
    if op == "symlink" {
        let Some(target) = param(&params, "target") else {
            return text(400, "missing target");
        };
        if comps.is_empty() || target.is_empty() {
            return text(400, "bad symlink");
        }
        let name = comps.last().unwrap();
        let r = ctx.fs(|fs| -> FsResult<Result<(), u16>> {
            let parent = match resolve_parent(fs, comps) {
                Ok(p) => p,
                Err(FsError::NotFound) => return Ok(Err(409)),
                Err(e) => return Err(e),
            };
            if fs.lookup(parent, name).is_ok() {
                return Ok(Err(405));
            }
            let pa = fs.getattr(parent)?;
            fs.symlink(parent, name, target, pa.uid, pa.gid)?;
            Ok(Ok(()))
        });
        return match r {
            Ok(Ok(())) => empty(204),
            Ok(Err(409)) => text(409, "parent collection missing"),
            Ok(Err(c)) => text(c, "already exists"),
            Err(e) => fs_err(&e),
        };
    }

    let set = match op {
        "truncate" => match param(&params, "size").and_then(|s| s.parse::<u64>().ok()) {
            Some(n) => SetAttr { size: Some(n), ..Default::default() },
            None => return text(400, "bad size"),
        },
        "chmod" => match param(&params, "mode").and_then(|s| u16::from_str_radix(s.trim(), 8).ok()) {
            Some(m) if m <= 0o7777 => SetAttr { perm: Some(m), ..Default::default() },
            _ => return text(400, "bad mode"),
        },
        "mtime" => match param(&params, "ns").and_then(|s| s.trim().parse::<i128>().ok()) {
            Some(ns) => {
                let secs = ns.div_euclid(1_000_000_000);
                let nsecs = ns.rem_euclid(1_000_000_000) as u32;
                if secs < i64::MIN as i128 || secs > i64::MAX as i128 {
                    return text(400, "bad ns");
                }
                SetAttr { mtime: Some(Timestamp { secs: secs as i64, nsecs }), ..Default::default() }
            }
            None => return text(400, "bad ns"),
        },
        _ => return text(400, "unknown op"),
    };
    let r = ctx.fs(|fs| -> FsResult<()> {
        let ino = resolve(fs, comps)?;
        if set.size.is_some() && fs.getattr(ino)?.kind != FileKind::File {
            return Err(FsError::IsDir);
        }
        fs.setattr(ino, &set)?;
        Ok(())
    });
    match r {
        Ok(()) => empty(204),
        Err(e) => fs_err(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::destination;

    #[test]
    fn destinations() {
        assert_eq!(destination("http://10.0.0.2:8080/a%20b/c").unwrap(), vec!["a b", "c"]);
        assert_eq!(destination("/x/y/").unwrap(), vec!["x", "y"]);
        assert_eq!(destination("http://host").unwrap(), Vec::<String>::new());
        assert!(destination("/a/../b").is_none());
    }
}
