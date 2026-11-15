//! Portal mode: PIN-protected "Drop-to-Disk" web page for phones on the same network.
//!
//! Routes (all except `/`, `/login` and `/favicon.ico` need a session, else 303 → `/`):
//! * `GET /`, `GET /b/<dir>/` — PIN page without a session, folder page with one.
//! * `POST /login` (form `pin`) — sets the session cookie (1 h); 5 failures/min/IP → 429.
//! * `POST /logout`
//! * `GET /dl/<path>` — download (attachment, byte ranges supported).
//! * `PUT /up/<dir>/<name>` — raw-body upload; never overwrites ("name (2).ext"); → 201 JSON
//!   `{"name": final name, "size": n}`.
//! * `POST /mkdir/<path>` — new folder → 201.
//!
//! The pages are self-contained (inline CSS/JS, inline SVG, no external URLs).
//!
//! # Schemes and sessions
//! The portal is served over HTTPS (`port()`) and, only when the user explicitly enables the
//! insecure compatibility mode for phones that refuse the self-signed certificate, over plain
//! HTTP (`http_port()`). The PIN and its rate limit are shared; sessions are
//! not:
//! * HTTPS login sets `__Host-ld_session=…; Secure; HttpOnly; SameSite=Strict; Path=/`. The
//!   `__Host-` prefix makes browsers refuse to set or overwrite it from plain HTTP (no cookie
//!   tossing / session fixation from the unencrypted port), and `Secure` keeps it off HTTP.
//! * HTTP login sets `ld_session=…; HttpOnly; SameSite=Strict; Path=/` (Secure is impossible).
//! * Each session remembers the scheme it was created on and is only accepted on that scheme,
//!   and each scheme only reads its own cookie name. So an HTTPS session id never becomes valid
//!   over HTTP, and an id that travelled in clear never grants access over HTTPS (where it would
//!   masquerade as an encrypted session). Logging in again on the other scheme is required.
//! * Browsers do not isolate cookies by port, so the HTTP cookie is also sent to the HTTPS
//!   port; it is ignored there.
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tiny_http::{Request, ResponseBox};

use super::http::*;
use super::{ct_eq, random_hex, Ctx};
use crate::vfs::{ghost, FileKind, FileSystem, FsError, FsResult, Ino};

const SESSION_TTL: Duration = Duration::from_secs(3600);
const FAIL_WINDOW: Duration = Duration::from_secs(60);
const MAX_FAILS: usize = 5;
/// Session cookie name on HTTPS (`__Host-` requires Secure, Path=/ and no Domain).
const COOKIE_TLS: &str = "__Host-ld_session";
/// Session cookie name on the plain-HTTP compatibility port.
const COOKIE_PLAIN: &str = "ld_session";

fn cookie_name(secure: bool) -> &'static str {
    if secure {
        COOKIE_TLS
    } else {
        COOKIE_PLAIN
    }
}

fn cookie_attrs(secure: bool) -> &'static str {
    if secure {
        "Secure; HttpOnly; SameSite=Strict; Path=/"
    } else {
        "HttpOnly; SameSite=Strict; Path=/"
    }
}

/// `ip`: the real client address; `secure`: request arrived over HTTPS.
pub fn handle(ctx: &Arc<Ctx>, pin: &str, rq: &mut Request, ip: Option<IpAddr>, secure: bool) -> ResponseBox {
    let method = rq.method().as_str().to_ascii_uppercase();
    let url = rq.url().to_string();
    let path_only = url.split('?').next().unwrap_or("").to_string();
    let lang = Lang::from_header(get_header(rq, "Accept-Language").as_deref());

    if path_only == "/favicon.ico" {
        return text(404, "not found");
    }
    if path_only == "/login" && method == "POST" {
        return login(ctx, pin, rq, ip, lang, secure);
    }

    let session = session_of(ctx, rq, secure);
    if session.is_none() {
        if path_only == "/" && (method == "GET" || method == "HEAD") {
            return login_page(ctx, lang, None, 200, secure);
        }
        return redirect("/");
    }

    if path_only == "/logout" && method == "POST" {
        if let Some(s) = session {
            ctx.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&s);
        }
        let mut r = redirect("/");
        r.add_header(hdr("Set-Cookie", &format!("{}=; {}; Max-Age=0", cookie_name(secure), cookie_attrs(secure))));
        return r;
    }

    let (route, rest) = match path_only.strip_prefix('/').and_then(|p| p.split_once('/')) {
        Some((r, rest)) => (r.to_string(), format!("/{rest}")),
        None => (path_only.trim_start_matches('/').to_string(), "/".to_string()),
    };
    let Some(comps) = parse_path(&rest) else {
        return text(400, "bad path");
    };

    match (method.as_str(), route.as_str()) {
        ("GET" | "HEAD", "") | ("GET" | "HEAD", "login") if comps.is_empty() => {
            if route == "login" {
                redirect("/")
            } else {
                folder_page(ctx, &[], lang, secure)
            }
        }
        ("GET" | "HEAD", "b") => folder_page(ctx, &comps, lang, secure),
        ("GET" | "HEAD", "dl") => download(ctx, rq, &comps, method == "HEAD"),
        ("PUT", "up") => upload(ctx, rq, &comps),
        ("POST", "mkdir") => mkdir(ctx, &comps),
        _ => text(404, "not found"),
    }
}

fn redirect(to: &str) -> ResponseBox {
    let mut r = empty(303);
    r.add_header(hdr("Location", to));
    r
}

/// The live session named by this scheme's cookie, if it was created on the same scheme.
fn session_of(ctx: &Ctx, rq: &Request, secure: bool) -> Option<String> {
    let name = cookie_name(secure);
    let cookies = get_header(rq, "Cookie")?;
    let sid = cookies.split(';').filter_map(|kv| kv.trim().split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v.trim().to_string())?;
    let mut m = ctx.sessions.lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, (t, _)| t.elapsed() < SESSION_TTL);
    // Compare against every live session in constant time per entry.
    let found = m.iter().find(|(k, (_, s))| ct_eq(k.as_bytes(), sid.as_bytes()) && *s == secure).map(|(k, _)| k.clone());
    found
}

fn login(ctx: &Ctx, pin: &str, rq: &mut Request, ip: Option<IpAddr>, lang: Lang, secure: bool) -> ResponseBox {
    let ipk = ip.unwrap_or(IpAddr::from([0, 0, 0, 0]));
    {
        let mut f = ctx.failures.lock().unwrap_or_else(|e| e.into_inner());
        f.retain(|_, v| {
            v.retain(|t| t.elapsed() < FAIL_WINDOW);
            !v.is_empty()
        });
        if f.get(&ipk).map(|v| v.len()).unwrap_or(0) >= MAX_FAILS {
            return login_page(ctx, lang, Some(lang.s().too_many), 429, secure);
        }
    }
    let body = read_small_body(rq, 4096).unwrap_or_default();
    let given = query_params(&body).into_iter().find(|(k, _)| k == "pin").map(|(_, v)| v).unwrap_or_default();
    let given: String = given.chars().filter(|c| !c.is_whitespace()).collect();
    if !ct_eq(given.as_bytes(), pin.as_bytes()) {
        ctx.failures.lock().unwrap_or_else(|e| e.into_inner()).entry(ipk).or_default().push(Instant::now());
        return login_page(ctx, lang, Some(lang.s().wrong_pin), 401, secure);
    }
    let sid = random_hex(16);
    ctx.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(sid.clone(), (Instant::now(), secure));
    let mut r = redirect("/");
    r.add_header(hdr(
        "Set-Cookie",
        &format!("{}={sid}; {}; Max-Age={}", cookie_name(secure), cookie_attrs(secure), SESSION_TTL.as_secs()),
    ));
    r
}

// ---------------------------------------------------------------------------------------------
// Data routes

fn is_junk(name: &str) -> bool {
    ghost::classify(name).is_some()
}

fn download(ctx: &Arc<Ctx>, rq: &Request, comps: &[String], is_head: bool) -> ResponseBox {
    if comps.iter().any(|c| is_junk(c)) {
        return text(404, "not found");
    }
    let r = ctx.fs(|fs| -> FsResult<_> {
        let ino = resolve(fs, comps)?;
        Ok((ino, fs.getattr(ino)?))
    });
    match r {
        Err(e) => fs_err(&e),
        Ok((_, a)) if a.kind != FileKind::File => text(405, "not a file"),
        Ok((ino, a)) => {
            let name = comps.last().map(|s| s.as_str()).unwrap_or("file");
            let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
            let extra = vec![
                hdr("Content-Disposition", &format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", pct_encode(name))),
                hdr("Content-Type", mime_for(name)),
                hdr("X-Content-Type-Options", "nosniff"),
                hdr("Cache-Control", "no-store"),
            ];
            file_response(ctx, rq, ino, &a, is_head, extra)
        }
    }
}

fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "webp" => "image/webp",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" | "csv" => "text/plain; charset=utf-8",
        "zip" => "application/zip",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

/// "photo.jpg" → "photo (2).jpg"; ".bashrc" → ".bashrc (2)".
fn numbered(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} ({n}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({n})"),
    }
}

fn create_unique(fs: &mut dyn FileSystem, parent: Ino, name: &str) -> FsResult<(Ino, String)> {
    let pa = fs.getattr(parent)?;
    for n in 1..10_000u32 {
        let cand = if n == 1 { name.to_string() } else { numbered(name, n) };
        if fs.lookup(parent, &cand).is_ok() {
            continue;
        }
        match fs.create(parent, &cand, 0o644, pa.uid, pa.gid) {
            Ok(ino) => return Ok((ino, cand)),
            Err(FsError::Exists) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(FsError::Exists)
}

fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 255 && !name.chars().any(|c| c.is_control()) && !is_junk(name)
}

fn upload(ctx: &Ctx, rq: &mut Request, comps: &[String]) -> ResponseBox {
    if ctx.read_only {
        return text(403, "read-only volume");
    }
    let Some(name) = comps.last() else {
        return text(400, "missing file name");
    };
    if !valid_name(name) || comps.iter().any(|c| is_junk(c)) {
        return text(400, "file name not allowed");
    }
    let made = ctx.fs(|fs| -> FsResult<(Ino, Ino, String)> {
        let parent = resolve_parent(fs, comps)?;
        let (ino, final_name) = create_unique(fs, parent, name)?;
        Ok((parent, ino, final_name))
    });
    let (parent, ino, final_name) = match made {
        Ok(v) => v,
        Err(FsError::NotFound) => return text(409, "folder not found"),
        Err(e) => return fs_err(&e),
    };
    match write_body(ctx, rq.as_reader(), ino, 0) {
        Ok(n) => {
            ctx.uploads.fetch_add(1, Ordering::Relaxed);
            let body = serde_json::json!({ "name": final_name, "size": n });
            bytes(201, "application/json", serde_json::to_vec(&body).unwrap_or_default())
        }
        Err(e) => {
            // Never leave a truncated file behind.
            let _ = ctx.fs(|fs| fs.unlink(parent, &final_name));
            upload_err(e)
        }
    }
}

fn mkdir(ctx: &Ctx, comps: &[String]) -> ResponseBox {
    if ctx.read_only {
        return text(403, "read-only volume");
    }
    let Some(name) = comps.last() else {
        return text(400, "missing folder name");
    };
    if !valid_name(name) {
        return text(400, "folder name not allowed");
    }
    let r = ctx.fs(|fs| -> FsResult<()> {
        let parent = resolve_parent(fs, comps)?;
        if fs.lookup(parent, name).is_ok() {
            return Err(FsError::Exists);
        }
        let pa = fs.getattr(parent)?;
        fs.mkdir(parent, name, 0o755, pa.uid, pa.gid)?;
        Ok(())
    });
    match r {
        Ok(()) => empty(201),
        Err(FsError::Exists) => text(409, "already exists"),
        Err(e) => fs_err(&e),
    }
}

// ---------------------------------------------------------------------------------------------
// Localisation

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    It,
    En,
}

struct Strings {
    html_lang: &'static str,
    pin_title: &'static str,
    pin_prompt: &'static str,
    pin_button: &'static str,
    wrong_pin: &'static str,
    too_many: &'static str,
    free: &'static str,
    of: &'static str,
    upload: &'static str,
    new_folder: &'static str,
    folder_name: &'static str,
    create: &'static str,
    cancel: &'static str,
    empty: &'static str,
    drop_here: &'static str,
    read_only: &'static str,
    uploading: &'static str,
    done: &'static str,
    failed: &'static str,
    renamed: &'static str,
    logout: &'static str,
    network_error: &'static str,
    exists: &'static str,
    hint: &'static str,
    folder: &'static str,
    plain_http: &'static str,
    encrypted: &'static str,
}

const IT: Strings = Strings {
    html_lang: "it",
    pin_title: "Drop-to-Disk",
    pin_prompt: "Inserisci il PIN a 6 cifre mostrato su LumenDisk.",
    pin_button: "Accedi",
    wrong_pin: "PIN errato. Riprova.",
    too_many: "Troppi tentativi. Riprova tra un minuto.",
    free: "liberi",
    of: "di",
    upload: "Carica file",
    new_folder: "Nuova cartella",
    folder_name: "Nome della cartella",
    create: "Crea",
    cancel: "Annulla",
    empty: "Questa cartella è vuota.",
    drop_here: "Rilascia qui per caricare",
    read_only: "Sola lettura",
    uploading: "Caricamento",
    done: "Completato",
    failed: "Errore",
    renamed: "salvato come",
    logout: "Esci",
    network_error: "Errore di rete",
    exists: "Esiste già un elemento con questo nome.",
    hint: "Trascina qui i file o usa “Carica file”. Vanno direttamente sul disco esterno.",
    folder: "Cartella",
    plain_http: "Connessione locale non cifrata: usala solo su reti di cui ti fidi.",
    encrypted: "Connessione cifrata",
};

const EN: Strings = Strings {
    html_lang: "en",
    pin_title: "Drop-to-Disk",
    pin_prompt: "Enter the 6-digit PIN shown in LumenDisk.",
    pin_button: "Sign in",
    wrong_pin: "Wrong PIN. Try again.",
    too_many: "Too many attempts. Try again in a minute.",
    free: "free",
    of: "of",
    upload: "Upload files",
    new_folder: "New folder",
    folder_name: "Folder name",
    create: "Create",
    cancel: "Cancel",
    empty: "This folder is empty.",
    drop_here: "Drop here to upload",
    read_only: "Read-only",
    uploading: "Uploading",
    done: "Done",
    failed: "Failed",
    renamed: "saved as",
    logout: "Sign out",
    network_error: "Network error",
    exists: "An item with this name already exists.",
    hint: "Drag files here or use “Upload files”. They go straight to the external drive.",
    folder: "Folder",
    plain_http: "Unencrypted local connection: use it only on networks you trust.",
    encrypted: "Encrypted connection",
};

impl Lang {
    /// First of it/en in `Accept-Language` preference order; Italian by default.
    fn from_header(h: Option<&str>) -> Lang {
        let Some(h) = h else { return Lang::It };
        let mut tags: Vec<(f32, usize, String)> = h
            .split(',')
            .enumerate()
            .filter_map(|(i, part)| {
                let mut it = part.split(';');
                let tag = it.next()?.trim().to_ascii_lowercase();
                let q = it.find_map(|p| p.trim().strip_prefix("q=").and_then(|v| v.parse::<f32>().ok())).unwrap_or(1.0);
                Some((q, i, tag))
            })
            .collect();
        tags.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
        for (q, _, t) in tags {
            if q <= 0.0 {
                continue;
            }
            if t == "it" || t.starts_with("it-") {
                return Lang::It;
            }
            if t == "en" || t.starts_with("en-") {
                return Lang::En;
            }
        }
        Lang::It
    }

    fn s(self) -> &'static Strings {
        match self {
            Lang::It => &IT,
            Lang::En => &EN,
        }
    }
}

/// Decimal units, like Finder.
fn human(n: u64, lang: Lang) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    let s = if v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") };
    let s = if lang == Lang::It { s.replace('.', ",") } else { s };
    format!("{s} {}", U[i])
}

fn esc(s: &str) -> String {
    xml_escape(s)
}

/// JSON for embedding inside a <script> element.
fn js(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into()).replace("</", "<\\/").replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029")
}

fn enc_path(comps: &[String]) -> String {
    comps.iter().map(|c| pct_encode(c)).collect::<Vec<_>>().join("/")
}

// ---------------------------------------------------------------------------------------------
// HTML

const CSS: &str = r#"
:root{--bg:#f2f2f7;--card:#fff;--text:#1d1d1f;--muted:#6e6e73;--line:rgba(60,60,67,.12);--accent:#5856d6;--accent-2:#7a78ff;--soft:rgba(88,86,214,.10);--danger:#ff3b30;--ok:#34c759;--shadow:0 1px 2px rgba(0,0,0,.04),0 8px 24px rgba(0,0,0,.06);color-scheme:light dark}
@media (prefers-color-scheme:dark){:root{--bg:#000;--card:#1c1c1e;--text:#f5f5f7;--muted:#98989f;--line:rgba(84,84,88,.45);--accent:#7d7aff;--accent-2:#9b99ff;--soft:rgba(125,122,255,.16);--shadow:none}}
*{box-sizing:border-box;-webkit-tap-highlight-color:transparent}
html,body{margin:0;background:var(--bg);color:var(--text)}
body{font:16px/1.4 -apple-system,BlinkMacSystemFont,"SF Pro Text","Helvetica Neue","Segoe UI",Roboto,system-ui,sans-serif;-webkit-font-smoothing:antialiased;min-height:100vh}
.wrap{max-width:720px;margin:0 auto;padding:max(16px,env(safe-area-inset-top)) 16px calc(32px + env(safe-area-inset-bottom))}
.card{background:var(--card);border-radius:16px;box-shadow:var(--shadow);overflow:hidden}
header.vol{display:flex;gap:14px;align-items:center;padding:16px;margin-bottom:14px}
.disk{flex:none;width:44px;height:44px;border-radius:12px;background:linear-gradient(145deg,var(--accent-2),var(--accent));display:grid;place-items:center;color:#fff}
.vol h1{font-size:19px;font-weight:650;margin:0;letter-spacing:-.01em;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.vol .sub{color:var(--muted);font-size:13px;margin-top:2px}
.vol .info{flex:1;min-width:0}
.bar{height:6px;border-radius:3px;background:var(--line);margin-top:8px;overflow:hidden}
.bar>i{display:block;height:100%;background:var(--accent);border-radius:3px}
.badge{display:inline-block;font-size:11px;font-weight:600;color:var(--accent);background:var(--soft);border-radius:6px;padding:2px 7px;margin-left:6px;vertical-align:1px}
.out{background:none;border:0;color:var(--muted);font:inherit;font-size:13px;padding:6px;cursor:pointer}
nav.crumbs{display:flex;flex-wrap:wrap;gap:6px;align-items:center;margin:4px 2px 12px;font-size:14px}
nav.crumbs a{color:var(--accent);text-decoration:none;padding:4px 10px;border-radius:999px;background:var(--soft);max-width:200px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
nav.crumbs span.sep{color:var(--muted)}
nav.crumbs b{font-weight:600;padding:4px 2px;max-width:220px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.actions{display:flex;gap:10px;margin-bottom:14px}
.btn{flex:1;display:inline-flex;align-items:center;justify-content:center;gap:8px;height:46px;border-radius:12px;border:0;font:inherit;font-weight:600;font-size:15px;cursor:pointer;text-decoration:none}
.btn.primary{background:var(--accent);color:#fff}
.btn.secondary{background:var(--card);color:var(--accent);box-shadow:var(--shadow)}
.btn:active{transform:scale(.98)}
.hint{color:var(--muted);font-size:13px;margin:-4px 4px 14px}
.row{display:flex;align-items:center;gap:12px;padding:12px 16px;color:inherit;text-decoration:none;min-height:56px}
.row+.row{border-top:1px solid var(--line)}
a.row:active{background:var(--soft)}
.ic{flex:none;width:34px;height:34px;border-radius:9px;display:grid;place-items:center}
.ic.f{background:var(--soft);color:var(--accent)}
.ic.d{background:rgba(10,132,255,.12);color:#0a84ff}
.nm{flex:1;min-width:0}
.nm div{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.nm small{display:block;color:var(--muted);font-size:12.5px;margin-top:1px}
.chev,.get{flex:none;color:var(--muted)}
.get{color:var(--accent)}
.empty{padding:36px 16px;text-align:center;color:var(--muted)}
#queue{margin-bottom:14px}
#queue:empty{display:none}
.up{padding:12px 16px}
.up+.up{border-top:1px solid var(--line)}
.up .t{display:flex;justify-content:space-between;gap:10px;font-size:14px}
.up .t span:first-child{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.up .t span:last-child{color:var(--muted);flex:none;font-variant-numeric:tabular-nums}
.up .p{height:4px;border-radius:2px;background:var(--line);margin-top:8px;overflow:hidden}
.up .p i{display:block;height:100%;width:0;background:var(--accent);transition:width .15s}
.up.ok .p i{background:var(--ok)}
.up.err .p i{background:var(--danger);width:100%}
.up.err .t span:last-child{color:var(--danger)}
#drop{position:fixed;inset:0;display:none;align-items:center;justify-content:center;background:rgba(88,86,214,.18);backdrop-filter:blur(6px);-webkit-backdrop-filter:blur(6px);z-index:10;padding:24px}
#drop div{border:2px dashed var(--accent);border-radius:20px;padding:40px 28px;background:var(--card);color:var(--accent);font-weight:600;font-size:18px;text-align:center}
body.dragging #drop{display:flex}
dialog{border:0;border-radius:16px;padding:20px;width:min(92vw,360px);background:var(--card);color:var(--text);box-shadow:0 20px 60px rgba(0,0,0,.3)}
dialog::backdrop{background:rgba(0,0,0,.35)}
dialog h2{font-size:17px;margin:0 0 12px}
input[type=text],input[type=password],input[inputmode]{width:100%;font:inherit;font-size:17px;padding:12px 14px;border-radius:12px;border:1px solid var(--line);background:var(--bg);color:var(--text);outline:none}
input:focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--soft)}
.drow{display:flex;gap:10px;margin-top:14px}
.err{color:var(--danger);font-size:14px;margin-top:10px;min-height:1em}
.login{min-height:100vh;display:grid;place-items:center;padding:24px 16px}
.login .card{width:100%;max-width:380px;padding:28px 22px;text-align:center}
.login .disk{margin:0 auto 14px;width:56px;height:56px;border-radius:16px}
.login h1{font-size:22px;margin:0 0 4px;letter-spacing:-.01em}
.login p{color:var(--muted);margin:0 0 18px;font-size:15px}
.login input{text-align:center;letter-spacing:.35em;font-size:24px;font-variant-numeric:tabular-nums}
.login .btn{width:100%;margin-top:14px}
.foot{color:var(--muted);font-size:12px;text-align:center;margin-top:18px}
.tls.ok{color:var(--muted)}.tls.warn{color:var(--danger)}
"#;

const ICON_DISK: &str = r#"<svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="13" width="18" height="7" rx="2"/><path d="M5 13l2.2-7.2A2 2 0 0 1 9.1 4.4h5.8a2 2 0 0 1 1.9 1.4L19 13"/><circle cx="17" cy="16.5" r=".9" fill="currentColor"/></svg>"#;
const ICON_FOLDER: &str = r#"<svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M3 7.5A2.5 2.5 0 0 1 5.5 5h3.6c.6 0 1.2.25 1.6.7l1.2 1.3h6.6A2.5 2.5 0 0 1 21 9.5v8A2.5 2.5 0 0 1 18.5 20h-13A2.5 2.5 0 0 1 3 17.5z"/></svg>"#;
const ICON_FILE: &str = r#"<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round" aria-hidden="true"><path d="M14 3H7.5A2.5 2.5 0 0 0 5 5.5v13A2.5 2.5 0 0 0 7.5 21h9a2.5 2.5 0 0 0 2.5-2.5V8z"/><path d="M14 3v5h5"/></svg>"#;
const ICON_DOWN: &str = r#"<svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 4v11"/><path d="M7 10.5l5 5 5-5"/><path d="M5 20h14"/></svg>"#;
const ICON_UP: &str = r#"<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 20V8"/><path d="M7 12.5l5-5 5 5"/><path d="M5 4h14"/></svg>"#;
const ICON_PLUS: &str = r#"<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"><path d="M12 5v14M5 12h14"/></svg>"#;
const ICON_CHEV: &str = r#"<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 6l6 6-6 6"/></svg>"#;

const JS: &str = r#"
(function(){
var CUR=window.LD.cur,T=window.LD.t,RO=window.LD.ro;
function enc(p){return p.split('/').filter(Boolean).map(encodeURIComponent).join('/')}
function el(tag,cls,txt){var e=document.createElement(tag);if(cls)e.className=cls;if(txt!=null)e.textContent=txt;return e}
function junk(n){return n==='.DS_Store'||n.indexOf('._')===0}
function human(n){var u=['B','KB','MB','GB','TB'],i=0;while(n>=1000&&i<u.length-1){n/=1000;i++}var s=i?(n>=100?n.toFixed(0):n.toFixed(1)):String(n);if(T.lang==='it')s=s.replace('.',',');return s+' '+u[i]}
async function refresh(){try{var r=await fetch(location.pathname,{credentials:'same-origin',cache:'no-store'});var d=new DOMParser().parseFromString(await r.text(),'text/html');['list','space'].forEach(function(id){var n=d.getElementById(id),o=document.getElementById(id);if(n&&o)o.replaceWith(n)})}catch(e){}}
var q=[],active=0,queue=document.getElementById('queue');
function add(files){if(RO)return;for(var i=0;i<files.length;i++){var f=files[i];if(junk(f.name))continue;var row=el('div','up'),t=el('div','t'),a=el('span',null,f.name),b=el('span',null,human(f.size)),p=el('div','p');p.appendChild(el('i'));t.appendChild(a);t.appendChild(b);row.appendChild(t);row.appendChild(p);queue.appendChild(row);q.push({f:f,row:row,a:a,b:b,bar:p.firstChild})}pump()}
function pump(){while(active<2&&q.length){active++;send(q.shift(),function(){active--;if(q.length)pump();else if(!active)refresh()})}}
function send(it,next){var x=new XMLHttpRequest();x.open('PUT','/up/'+enc(CUR+'/'+it.f.name));
x.upload.onprogress=function(e){if(e.lengthComputable){var r=e.loaded/e.total;it.bar.style.width=(r*100).toFixed(1)+'%';it.b.textContent=Math.floor(r*100)+'%'}};
x.onload=function(){var j=null;try{j=JSON.parse(x.responseText)}catch(e){}
if(x.status>=200&&x.status<300&&j&&j.name){it.row.classList.add('ok');it.bar.style.width='100%';it.b.textContent=T.done;if(j.name!==it.f.name)it.a.textContent=it.f.name+' \u2192 '+T.renamed+' '+j.name}
else if(x.status>=200&&x.status<400&&!j){location.reload()}
else{it.row.classList.add('err');it.b.textContent=T.failed;it.a.title=x.responseText}next()};
x.onerror=function(){it.row.classList.add('err');it.b.textContent=T.network_error;next()};
it.b.textContent='0%';x.send(it.f)}
var pick=document.getElementById('pick');if(pick)pick.addEventListener('change',function(){add(pick.files);pick.value=''});
var depth=0;
function hasFiles(e){var t=e.dataTransfer&&e.dataTransfer.types;return t&&Array.prototype.indexOf.call(t,'Files')>=0}
if(!RO){
document.addEventListener('dragenter',function(e){if(!hasFiles(e))return;e.preventDefault();depth++;document.body.classList.add('dragging')});
document.addEventListener('dragover',function(e){if(hasFiles(e))e.preventDefault()});
document.addEventListener('dragleave',function(e){if(--depth<=0){depth=0;document.body.classList.remove('dragging')}});
document.addEventListener('drop',function(e){if(!hasFiles(e))return;e.preventDefault();depth=0;document.body.classList.remove('dragging');
var items=e.dataTransfer.items,files=[];if(items&&items.length&&items[0].webkitGetAsEntry){for(var i=0;i<items.length;i++){var en=items[i].webkitGetAsEntry&&items[i].webkitGetAsEntry();if(en&&en.isDirectory)continue;var f=items[i].getAsFile();if(f)files.push(f)}}else{files=e.dataTransfer.files}add(files)});
}
var dlg=document.getElementById('mk'),mkbtn=document.getElementById('mkbtn');
if(mkbtn&&dlg){mkbtn.addEventListener('click',function(){var inp=document.getElementById('mkname');inp.value='';document.getElementById('mkerr').textContent='';if(dlg.showModal)dlg.showModal();else dlg.setAttribute('open','');inp.focus()});
document.getElementById('mkcancel').addEventListener('click',function(){dlg.close?dlg.close():dlg.removeAttribute('open')});
document.getElementById('mkform').addEventListener('submit',async function(e){e.preventDefault();var n=document.getElementById('mkname').value.trim();if(!n)return;
try{var r=await fetch('/mkdir/'+enc(CUR+'/'+n),{method:'POST',credentials:'same-origin'});if(r.status===201){dlg.close?dlg.close():dlg.removeAttribute('open');refresh()}else{document.getElementById('mkerr').textContent=r.status===409?T.exists:(await r.text())}}catch(err){document.getElementById('mkerr').textContent=T.network_error}})}
})();
"#;

fn page(lang: Lang, title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"{}\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1,viewport-fit=cover\"><meta name=\"color-scheme\" content=\"light dark\"><meta name=\"referrer\" content=\"no-referrer\"><title>{}</title><style>{}</style></head><body>{}</body></html>",
        lang.s().html_lang,
        esc(title),
        CSS,
        body
    )
}

fn html_response(code: u16, html: String) -> ResponseBox {
    let mut r = bytes(code, "text/html; charset=utf-8", html.into_bytes());
    r.add_header(hdr("Cache-Control", "no-store"));
    r.add_header(hdr("X-Frame-Options", "DENY"));
    r.add_header(hdr("X-Content-Type-Options", "nosniff"));
    r.add_header(hdr("Referrer-Policy", "no-referrer"));
    r.add_header(hdr(
        "Content-Security-Policy",
        "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
    ));
    r
}

/// Footer note on the transport: reassuring on HTTPS, a gentle warning on plain HTTP.
fn transport_note(lang: Lang, secure: bool) -> String {
    let s = lang.s();
    if secure {
        format!("<span class=\"tls ok\">{}</span>", esc(s.encrypted))
    } else {
        format!("<span class=\"tls warn\">{}</span>", esc(s.plain_http))
    }
}

fn login_page(ctx: &Ctx, lang: Lang, error: Option<&str>, code: u16, secure: bool) -> ResponseBox {
    let s = lang.s();
    let body = format!(
        "<main class=\"login\"><form class=\"card\" method=\"post\" action=\"/login\" autocomplete=\"off\">\
<div class=\"disk\">{ICON_DISK}</div><h1>{name}</h1><p>{prompt}</p>\
<input name=\"pin\" inputmode=\"numeric\" pattern=\"[0-9]*\" maxlength=\"6\" autocomplete=\"one-time-code\" autofocus required aria-label=\"PIN\">\
<div class=\"err\" role=\"alert\">{err}</div>\
<button class=\"btn primary\" type=\"submit\">{btn}</button>\
<div class=\"foot\">{title} · {plain}</div></form></main>",
        name = esc(&ctx.name),
        prompt = esc(s.pin_prompt),
        err = esc(error.unwrap_or("")),
        btn = esc(s.pin_button),
        title = esc(s.pin_title),
        plain = transport_note(lang, secure),
    );
    html_response(code, page(lang, &format!("{} · {}", ctx.name, s.pin_title), &body))
}

struct Item {
    name: String,
    dir: bool,
    size: u64,
}

fn folder_page(ctx: &Ctx, comps: &[String], lang: Lang, secure: bool) -> ResponseBox {
    let s = lang.s();
    let listing = ctx.fs(|fs| -> FsResult<_> {
        let ino = resolve(fs, comps)?;
        if fs.getattr(ino)?.kind != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        Ok((fs.readdir(ino)?, fs.statfs().unwrap_or_default()))
    });
    let (entries, st) = match listing {
        Ok(v) => v,
        Err(FsError::NotFound) | Err(FsError::NotDir) => return redirect("/"),
        Err(e) => return fs_err(&e),
    };
    // Hide macOS junk, symlinks and the ext* `lost+found` system folder.
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|e| !is_junk(&e.name) && e.kind != FileKind::Symlink && !(comps.is_empty() && e.name == "lost+found"))
        .collect();
    let mut items: Vec<Item> = Vec::with_capacity(entries.len());
    for batch in entries.chunks(128) {
        let attrs: Vec<_> = ctx.fs(|fs| batch.iter().map(|e| fs.getattr(e.ino).ok()).collect());
        for (e, a) in batch.iter().zip(attrs) {
            let Some(a) = a else { continue };
            items.push(Item { name: e.name.clone(), dir: a.kind == FileKind::Dir, size: a.size });
        }
    }
    items.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then_with(|| a.name.cmp(&b.name)));

    // Header with free space.
    let used_pct = if st.total_bytes > 0 { 100.0 * (st.total_bytes.saturating_sub(st.free_bytes)) as f64 / st.total_bytes as f64 } else { 0.0 };
    let ro_badge = if ctx.read_only { format!("<span class=\"badge\">{}</span>", esc(s.read_only)) } else { String::new() };
    let mut h = String::new();
    h.push_str(&format!(
        "<div class=\"wrap\"><header class=\"vol card\"><div class=\"disk\">{ICON_DISK}</div><div class=\"info\"><h1>{}{}</h1>\
<div id=\"space\"><div class=\"sub\">{} {} · {} {}</div><div class=\"bar\"><i style=\"width:{:.1}%\"></i></div></div></div>\
<form method=\"post\" action=\"/logout\"><button class=\"out\" type=\"submit\">{}</button></form></header>",
        esc(&ctx.name),
        ro_badge,
        esc(&human(st.free_bytes, lang)),
        esc(s.free),
        esc(s.of),
        esc(&human(st.total_bytes, lang)),
        used_pct,
        esc(s.logout),
    ));

    // Breadcrumbs.
    h.push_str("<nav class=\"crumbs\" aria-label=\"breadcrumb\">");
    if comps.is_empty() {
        h.push_str(&format!("<b>{}</b>", esc(&ctx.name)));
    } else {
        h.push_str(&format!("<a href=\"/\">{}</a>", esc(&ctx.name)));
        for i in 0..comps.len() {
            h.push_str(&format!("<span class=\"sep\">{ICON_CHEV}</span>"));
            if i + 1 == comps.len() {
                h.push_str(&format!("<b>{}</b>", esc(&comps[i])));
            } else {
                h.push_str(&format!("<a href=\"/b/{}/\">{}</a>", enc_path(&comps[..=i]), esc(&comps[i])));
            }
        }
    }
    h.push_str("</nav>");

    if !ctx.read_only {
        h.push_str(&format!(
            "<div class=\"actions\"><label class=\"btn primary\" for=\"pick\">{ICON_UP}{}</label>\
<input id=\"pick\" type=\"file\" multiple hidden>\
<button class=\"btn secondary\" id=\"mkbtn\" type=\"button\">{ICON_PLUS}{}</button></div><p class=\"hint\">{}</p>",
            esc(s.upload),
            esc(s.new_folder),
            esc(s.hint)
        ));
    }
    h.push_str("<div id=\"queue\" class=\"card\"></div>");

    // Listing.
    h.push_str("<div id=\"list\" class=\"card\">");
    if items.is_empty() {
        h.push_str(&format!("<div class=\"empty\">{}</div>", esc(s.empty)));
    }
    let mut child = comps.to_vec();
    child.push(String::new());
    for it in &items {
        *child.last_mut().unwrap() = it.name.clone();
        let p = enc_path(&child);
        if it.dir {
            h.push_str(&format!(
                "<a class=\"row\" href=\"/b/{p}/\"><span class=\"ic d\">{ICON_FOLDER}</span><span class=\"nm\"><div>{}</div><small>{}</small></span><span class=\"chev\">{ICON_CHEV}</span></a>",
                esc(&it.name),
                esc(s.folder)
            ));
        } else {
            h.push_str(&format!(
                "<a class=\"row\" href=\"/dl/{p}\" download><span class=\"ic f\">{ICON_FILE}</span><span class=\"nm\"><div>{}</div><small>{}</small></span><span class=\"get\">{ICON_DOWN}</span></a>",
                esc(&it.name),
                esc(&human(it.size, lang))
            ));
        }
    }
    h.push_str("</div>");
    h.push_str(&format!("<p class=\"foot\">{} · {}</p></div>", esc(s.pin_title), transport_note(lang, secure)));

    // Drop overlay + new-folder dialog.
    h.push_str(&format!("<div id=\"drop\"><div>{}</div></div>", esc(s.drop_here)));
    h.push_str(&format!(
        "<dialog id=\"mk\"><form id=\"mkform\"><h2>{}</h2><input type=\"text\" id=\"mkname\" placeholder=\"{}\" autocomplete=\"off\" maxlength=\"255\">\
<div class=\"err\" id=\"mkerr\"></div><div class=\"drow\"><button class=\"btn secondary\" type=\"button\" id=\"mkcancel\">{}</button>\
<button class=\"btn primary\" type=\"submit\">{}</button></div></form></dialog>",
        esc(s.new_folder),
        esc(s.folder_name),
        esc(s.cancel),
        esc(s.create)
    ));

    let cur = format!("/{}", comps.join("/"));
    let data = serde_json::json!({
        "cur": cur,
        "ro": ctx.read_only,
        "t": {
            "lang": s.html_lang, "done": s.done, "failed": s.failed, "renamed": s.renamed,
            "network_error": s.network_error, "exists": s.exists, "uploading": s.uploading,
        }
    });
    h.push_str(&format!("<script>window.LD={};</script><script>{JS}</script>", js(&data)));

    let title = if comps.is_empty() { ctx.name.clone() } else { format!("{} · {}", comps.last().unwrap(), ctx.name) };
    html_response(200, page(lang, &title, &h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_pick() {
        assert!(Lang::from_header(None) == Lang::It);
        assert!(Lang::from_header(Some("en-US,en;q=0.9")) == Lang::En);
        assert!(Lang::from_header(Some("it-IT,it;q=0.9,en;q=0.8")) == Lang::It);
        assert!(Lang::from_header(Some("de-DE,en;q=0.5")) == Lang::En);
        assert!(Lang::from_header(Some("fr-FR")) == Lang::It);
        assert!(Lang::from_header(Some("en;q=0.2,it;q=0.8")) == Lang::It);
    }

    #[test]
    fn numbering() {
        assert_eq!(numbered("photo.jpg", 2), "photo (2).jpg");
        assert_eq!(numbered(".bashrc", 3), ".bashrc (3)");
        assert_eq!(numbered("README", 2), "README (2)");
    }

    #[test]
    fn human_units() {
        assert_eq!(human(999, Lang::It), "999 B");
        assert_eq!(human(1_500_000, Lang::It), "1,5 MB");
        assert_eq!(human(1_500_000, Lang::En), "1.5 MB");
        assert_eq!(human(250_000_000_000, Lang::En), "250 GB");
    }
}
