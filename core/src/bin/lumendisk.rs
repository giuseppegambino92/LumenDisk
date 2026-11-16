//! `lumendisk` — command-line front end / test runner for lumendisk-core.
//!
//!   lumendisk inspect <image|/dev/diskN>
//!   lumendisk mkfs <ext4|ext3|ext2|ntfs|fat32|fat16|fat12|exfat> <image> <size-MiB> [label]
//!   lumendisk mount <image|/dev/diskNsM> [--partition N] [--mode readWrite|shadow|readOnly]
//!                   [--no-ghost] [--root DIR] [--name NAME] [--control SOCKET]
//!   lumendisk mount-remote <REQUEST.json> [--control SOCKET]
//!                   SFTP / WebDAV / LAN share; the file holds a RemoteMountRequest (kind, url,
//!                   mode, credentials…), so passwords never appear in the process list
//!   lumendisk ctl <SOCKET> <status|changes|commit|rollback|ghost|unmount>
//!   lumendisk repair <image> [--partition N]
//!   lumendisk locks <mountpoint>
//!   lumendisk shadow-sessions                 saved Shadow sessions (pending changes per volume)
//!   lumendisk shadow-discard <KEY>            delete a saved Shadow session
//!   lumendisk forge <REQUEST.json|->         run a forge request (flash, hash, format…) in the
//!                   foreground, printing phase / progress / speed / ETA, then the result
//!   lumendisk disks                           candidate disks and whether LumenDisk may write them
//!   lumendisk inspect-image <path> [--deep]   format, size, partitions, ISO / OS, write mode
//!   lumendisk hash <path|diskN> [--algo sha256,sha1,md5] [--decompressed] [--offset N] [--length N]
//!
//! State lives in ~/Library/Application Support/LumenDisk, or in $LUMENDISK_SUPPORT_DIR.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use lumendisk_core::forge::{self, plan::Request};
use lumendisk_core::lock_inspector;
use lumendisk_core::volume::{runtime, MountMode, MountRequest, RemoteMountRequest, VolumeStatus};
use serde_json::json;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  lumendisk inspect <path>\n  lumendisk mkfs <fs> <path> <MiB> [label]\n  lumendisk mount <path> [--partition N] [--mode readWrite|shadow|readOnly] [--no-ghost] [--root DIR] [--name NAME] [--control SOCK] [--media-uuid UUID] [--expect-session KEY]\n  lumendisk mount-remote <request.json> [--control SOCK]\n  lumendisk ctl <SOCK> <status|changes|commit|rollback|ghost|unmount>\n  lumendisk repair <path> [--partition N]\n  lumendisk locks <mountpoint>\n  lumendisk shadow-sessions\n  lumendisk shadow-discard <KEY>\n  lumendisk forge <request.json|->\n  lumendisk disks\n  lumendisk inspect-image <path> [--deep]\n  lumendisk hash <path|diskN> [--algo sha256,sha1,md5] [--decompressed] [--offset N] [--length N]"
    );
    std::process::exit(2)
}

fn print(v: serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
}

fn fail(e: impl std::fmt::Display) -> ! {
    eprintln!("lumendisk: {e}");
    std::process::exit(1)
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn handle_ctl(cmd: &str, id: &str) -> serde_json::Value {
    let rt = runtime();
    let r: Result<serde_json::Value, String> = match cmd {
        "status" => rt.get(id).map(|v| json!(rt.status(&v))),
        "changes" => rt.shadow_changes(id).map(|c| json!(c)),
        "commit" => rt.shadow_commit(id).map(|n| json!({"applied": n})),
        "rollback" => rt.shadow_rollback(id).map(|n| json!({"discarded": n})),
        "ghost" => rt.ghost_entries(id).map(|e| json!(e)),
        "unmount" => {
            STOP.store(true, Ordering::SeqCst);
            Ok(json!({"unmounting": true}))
        }
        o => Err(format!("unknown command {o}")),
    };
    match r {
        Ok(v) => json!({"ok": true, "result": v}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(|s| s.as_str()) else { usage() };
    if std::env::var_os("LUMENDISK_NFS_DEBUG").is_some() {
        let _ = tracing_subscriber::fmt().with_ansi(false).with_env_filter("nfsserve=debug").with_writer(std::io::stderr).try_init();
    }
    let rt = runtime();
    let support = std::env::var_os("LUMENDISK_SUPPORT_DIR")
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(lumendisk_core::volume::default_support_dir);
    rt.set_support_dir(&support);
    match cmd {
        "inspect" => {
            let p = args.get(1).unwrap_or_else(|| usage());
            match rt.inspect_source(p, None) {
                Ok(r) => print(json!(r)),
                Err(e) => fail(e),
            }
        }
        "mkfs" => {
            let (Some(fs), Some(p), Some(mb)) = (args.get(1), args.get(2), args.get(3)) else { usage() };
            let label = args.get(4).cloned().unwrap_or_else(|| "LUMENDISK".into());
            match rt.create_image(p, fs, mb.parse().unwrap_or_else(|_| usage()), &label) {
                Ok(p) => print(json!({"created": p})),
                Err(e) => fail(e),
            }
        }
        "repair" => {
            let p = args.get(1).unwrap_or_else(|| usage());
            let part = opt(&args, "--partition").and_then(|s| s.parse().ok()).unwrap_or(0);
            match rt.repair_source(p, part, None) {
                Ok(r) => print(json!({"report": r})),
                Err(e) => fail(e),
            }
        }
        "shadow-sessions" => print(json!(rt.shadow_sessions())),
        "shadow-discard" => {
            let k = args.get(1).unwrap_or_else(|| usage());
            match rt.shadow_discard_session(k) {
                Ok(n) => print(json!({"discarded": n})),
                Err(e) => fail(e),
            }
        }
        "forge" => {
            let p = args.get(1).unwrap_or_else(|| usage());
            let raw = if p == "-" {
                let mut v = vec![];
                std::io::Read::read_to_end(&mut std::io::stdin(), &mut v).unwrap_or_else(|e| fail(e));
                v
            } else {
                std::fs::read(p).unwrap_or_else(|e| fail(format!("{p}: {e}")))
            };
            let req: Request = serde_json::from_slice(&raw).unwrap_or_else(|e| fail(format!("{p}: {e}")));
            run_forge(req);
        }
        "disks" => match forge::target::list_disks() {
            Ok(disks) => {
                for d in disks {
                    let verdict = if d.allowed { "allowed".to_string() } else { format!("refused: {}", d.refusal.unwrap_or_default()) };
                    let mounted = if d.mounted.is_empty() { String::new() } else { format!("  [{}]", d.mounted.join(", ")) };
                    println!("{:<8} {:>10}  {:<14} {:<28} {verdict}{mounted}", d.bsd, forge::flash::human(d.size), d.protocol, d.media_name);
                }
            }
            Err(e) => fail(e),
        },
        "inspect-image" => {
            let p = args.get(1).unwrap_or_else(|| usage()).clone();
            if args.iter().any(|a| a == "--deep") {
                run_forge(Request::Inspect { source: p });
            } else {
                match forge::image::inspect_quick(&p) {
                    Ok(v) => print(v),
                    Err(e) => fail(e),
                }
            }
        }
        "hash" => {
            let p = args.get(1).unwrap_or_else(|| usage()).clone();
            let algorithms = opt(&args, "--algo").map(|a| a.split(',').map(str::to_string).collect()).unwrap_or_default();
            let num = |name: &str| opt(&args, name).map(|v| v.parse::<u64>().unwrap_or_else(|_| fail(format!("{name}: not a number"))));
            run_forge(Request::Hash(forge::plan::HashRequest {
                source: p,
                algorithms,
                decompressed: args.iter().any(|a| a == "--decompressed"),
                offset: num("--offset"),
                length: num("--length"),
            }));
        }
        "locks" => {
            let p = args.get(1).unwrap_or_else(|| usage());
            print(json!(lock_inspector::inspect(p)));
        }
        "ctl" => {
            let (Some(sock), Some(c)) = (args.get(1), args.get(2)) else { usage() };
            let mut s = UnixStream::connect(sock).unwrap_or_else(|e| fail(format!("{sock}: {e}")));
            writeln!(s, "{c}").unwrap();
            let mut line = String::new();
            BufReader::new(s).read_line(&mut line).unwrap();
            let v: serde_json::Value = serde_json::from_str(&line).unwrap_or(json!({"ok": false, "error": line}));
            let ok = v["ok"].as_bool().unwrap_or(false);
            print(v);
            if !ok {
                std::process::exit(1);
            }
        }
        "mount" => {
            let p = args.get(1).unwrap_or_else(|| usage()).clone();
            let mode = match opt(&args, "--mode").as_deref() {
                None | Some("readWrite") | Some("rw") => MountMode::ReadWrite,
                Some("shadow") => MountMode::Shadow,
                Some("readOnly") | Some("ro") => MountMode::ReadOnly,
                Some(o) => fail(format!("unknown mode {o}")),
            };
            let req = MountRequest {
                path: p,
                partition: opt(&args, "--partition").and_then(|s| s.parse().ok()).unwrap_or(0),
                mode,
                ghost_shield: !args.iter().any(|a| a == "--no-ghost"),
                ghost_rules: None,
                uid_map: None,
                mount_root: opt(&args, "--root"),
                name: opt(&args, "--name"),
                fd: None,
                volume_icon: None,
                // What the app passes from DiskArbitration (the partition's media UUID) and
                // when resuming a saved Shadow session (refused if the disk doesn't hold it).
                media_uuid: opt(&args, "--media-uuid"),
                expect_session: opt(&args, "--expect-session"),
            };
            let listener = bind_control(&args);
            let st = rt.mount(req).unwrap_or_else(|e| fail(e));
            serve(st, listener, &args);
        }
        "mount-remote" => {
            let p = args.get(1).unwrap_or_else(|| usage());
            let raw = std::fs::read(p).unwrap_or_else(|e| fail(format!("{p}: {e}")));
            let req: RemoteMountRequest = serde_json::from_slice(&raw).unwrap_or_else(|e| fail(format!("{p}: {e}")));
            let listener = bind_control(&args);
            let st = rt.mount_remote(req).unwrap_or_else(|e| fail(e));
            serve(st, listener, &args);
        }
        _ => usage(),
    }
}

/// Bind the control socket before mounting, so a bad path can't leave an orphaned mount.
fn bind_control(args: &[String]) -> Option<UnixListener> {
    opt(args, "--control").map(|sock| {
        let _ = std::fs::remove_file(&sock);
        UnixListener::bind(&sock).unwrap_or_else(|e| fail(format!("{sock}: {e}")))
    })
}

/// Serve a mounted volume until Ctrl-C / SIGTERM / `ctl unmount`, then unmount it.
fn serve(st: VolumeStatus, listener: Option<UnixListener>, args: &[String]) {
    let rt = runtime();
    let id = st.id.clone();
    print(json!({"mounted": st}));
    // SAFETY: installing a trivial async-signal-safe handler.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
    }
    if let Some(listener) = listener {
        let id = Arc::new(id.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let id = id.clone();
                std::thread::spawn(move || {
                    let mut r = BufReader::new(conn.try_clone().unwrap());
                    let mut line = String::new();
                    if r.read_line(&mut line).is_ok() {
                        let v = handle_ctl(line.trim(), &id);
                        let mut w = conn;
                        let _ = writeln!(w, "{}", serde_json::to_string(&v).unwrap());
                    }
                });
            }
        });
    }
    eprintln!("lumendisk: mounted at {} (Ctrl-C to unmount)", st.mountpoint);
    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if !lumendisk_core::nfs_loopback::is_mountpoint(std::path::Path::new(&st.mountpoint)) {
            eprintln!("lumendisk: mount disappeared");
            break;
        }
    }
    let mut tries = 0;
    loop {
        match rt.unmount(&id, tries >= 10) {
            Ok(()) => break,
            Err(e) if tries < 10 => {
                eprintln!("lumendisk: unmount retry: {e}");
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => fail(e),
        }
    }
    if let Some(sock) = opt(args, "--control") {
        let _ = std::fs::remove_file(sock);
    }
    eprintln!("lumendisk: unmounted cleanly");
}

/// Run a forge request as a job in the foreground: phase / progress / speed / ETA lines on
/// stderr, the result as JSON on stdout. Ctrl-C cancels the job (it stops at its next check).
fn run_forge(req: Request) -> ! {
    // SAFETY: installing a trivial async-signal-safe handler.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
    }
    let id = forge::plan::start(req);
    let mut seen = 0u64;
    let mut cancelled = false;
    let mut last_line = std::time::Instant::now() - std::time::Duration::from_secs(10);
    let mut last_phase = None;
    loop {
        if STOP.load(Ordering::SeqCst) && !cancelled {
            eprintln!("lumendisk: cancelling…");
            forge::job::cancel(id);
            cancelled = true;
        }
        let Some(s) = forge::job::status(id, seen) else { fail("job vanished") };
        for l in &s.logs {
            eprintln!("  [{:>7.1}s] {:?}: {}", l.ms as f64 / 1000.0, l.level, l.msg);
            seen = l.seq;
        }
        let phase_changed = last_phase != Some(s.phase);
        if s.state == forge::job::State::Running && (phase_changed || last_line.elapsed().as_secs_f64() >= 1.0) {
            last_phase = Some(s.phase);
            last_line = std::time::Instant::now();
            let pct = if s.total > 0 { format!("{:5.1}%", s.done as f64 * 100.0 / s.total as f64) } else { "   …  ".into() };
            let (done, total, rate) = if s.unit == "bytes" {
                (forge::flash::human(s.done), if s.total > 0 { forge::flash::human(s.total) } else { "?".into() }, format!("{:.1} MB/s", s.rate / 1e6))
            } else {
                (s.done.to_string(), s.total.to_string(), format!("{:.1} {}/s", s.rate, s.unit))
            };
            let eta = s.eta_secs.map(|e| format!("ETA {}:{:02}", e / 60, e % 60)).unwrap_or_default();
            eprintln!("{:<12} {pct}  {done} / {total}  {rate}  {eta}", format!("[{:?}]", s.phase).to_lowercase());
        }
        match s.state {
            forge::job::State::Running => std::thread::sleep(std::time::Duration::from_millis(200)),
            forge::job::State::Succeeded => {
                print(s.result.unwrap_or(json!(null)));
                std::process::exit(0)
            }
            forge::job::State::Cancelled => {
                eprintln!("lumendisk: cancelled");
                std::process::exit(130)
            }
            forge::job::State::Failed => fail(s.error.unwrap_or_default()),
        }
    }
}
