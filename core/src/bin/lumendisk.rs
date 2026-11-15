//! `lumendisk` — command-line front end / test runner for lumendisk-core.
//!
//!   lumendisk inspect <image|/dev/diskN>
//!   lumendisk mkfs <ext4|ext3|ext2|ntfs|fat32|fat16|fat12|exfat> <image> <size-MiB> [label]
//!   lumendisk mount <image|/dev/diskNsM> [--partition N] [--mode readWrite|shadow|readOnly]
//!                   [--no-ghost] [--root DIR] [--name NAME] [--control SOCKET]
//!   lumendisk ctl <SOCKET> <status|changes|commit|rollback|ghost|unmount>
//!   lumendisk repair <image> [--partition N]
//!   lumendisk locks <mountpoint>
//!   lumendisk shadow-sessions                 saved Shadow sessions (pending changes per volume)
//!   lumendisk shadow-discard <KEY>            delete a saved Shadow session
//!
//! State lives in ~/Library/Application Support/LumenDisk, or in $LUMENDISK_SUPPORT_DIR.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use lumendisk_core::lock_inspector;
use lumendisk_core::volume::{runtime, MountMode, MountRequest};
use serde_json::json;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  lumendisk inspect <path>\n  lumendisk mkfs <fs> <path> <MiB> [label]\n  lumendisk mount <path> [--partition N] [--mode readWrite|shadow|readOnly] [--no-ghost] [--root DIR] [--name NAME] [--control SOCK]\n  lumendisk ctl <SOCK> <status|changes|commit|rollback|ghost|unmount>\n  lumendisk repair <path> [--partition N]\n  lumendisk locks <mountpoint>\n  lumendisk shadow-sessions\n  lumendisk shadow-discard <KEY>"
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
                media_uuid: None,
                expect_session: None,
            };
            // Bind the control socket first so a bad path can't leave an orphaned mount.
            let listener = opt(&args, "--control").map(|sock| {
                let _ = std::fs::remove_file(&sock);
                UnixListener::bind(&sock).unwrap_or_else(|e| fail(format!("{sock}: {e}")))
            });
            let st = rt.mount(req).unwrap_or_else(|e| fail(e));
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
            if let Some(sock) = opt(&args, "--control") {
                let _ = std::fs::remove_file(sock);
            }
            eprintln!("lumendisk: unmounted cleanly");
        }
        _ => usage(),
    }
}
