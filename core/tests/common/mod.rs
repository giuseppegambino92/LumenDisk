//! Helpers for integration tests that use the real upstream Linux tools in Docker
//! (image `lumendisk-toolbox`, see scripts/docker/Dockerfile).
#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn workdir(name: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("it").join(name);
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

// ───────────── in-process suites (one shared `runtime()`, tests in parallel) ─────────────

/// Where a test's volumes are mounted: next to its working dir, not inside it. The working dir
/// is bind-mounted into Docker containers (mkfs, fsck), and Docker Desktop's VM then keeps
/// files of an NFS mount inside it open, so unmounting it fails with EBUSY.
pub fn mount_root(dir: &Path) -> PathBuf {
    dir.with_file_name(format!("{}-mnt", dir.file_name().unwrap().to_string_lossy()))
}

thread_local! {
    static TEST_MOUNT_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Set up a test of a suite that drives the process-wide `runtime()` directly: its working
/// dir, its mount root, the suite's support dir and the panic cleanup.
///
/// The runtime has one support dir for the whole process (WAL, offline queues, remote caches,
/// Shadow sessions). Setting it per test made tests running in parallel keep their volumes'
/// state in whichever test's directory was set last, which that test then wiped or inspected:
/// it is set once per process, to `<suite>-support`. A panic unmounts the volumes of the
/// panicking test only (those under its mount root), never those of the tests beside it.
pub fn setup_in_process(suite: &str, name: &str) -> PathBuf {
    use lumendisk_core::volume::runtime;
    static SUPPORT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    SUPPORT.get_or_init(|| {
        let s = workdir(&format!("{suite}-support"));
        runtime().set_support_dir(&s);
        std::panic::set_hook(Box::new(|info| {
            eprintln!("{info}");
            unmount_this_tests_volumes();
        }));
        s
    });
    let dir = workdir(name);
    let mnt = mount_root(&dir);
    let _ = std::fs::remove_dir_all(&mnt);
    std::fs::create_dir_all(&mnt).unwrap();
    TEST_MOUNT_ROOT.with(|r| *r.borrow_mut() = Some(mnt));
    dir
}

/// Force-unmount the volumes of the current test (those under its mount root), and only them:
/// the tests running beside it keep theirs.
pub fn unmount_this_tests_volumes() {
    use lumendisk_core::volume::runtime;
    if let Some(root) = TEST_MOUNT_ROOT.with(|r| r.borrow().clone()) {
        for v in runtime().list() {
            if Path::new(&v.mountpoint).starts_with(&root) {
                let _ = runtime().unmount(&v.id, true);
            }
        }
    }
}

/// `runtime().unmount(id, false)`, waiting (bounded) while macOS reports the mount busy, and
/// naming who holds it. Seen under parallel load: Docker Desktop's VM
/// (com.apple.Virtualization.VirtualMachine) holding files of a mount open, and, with no holder
/// at all, the NFS client still settling for a moment after a 64 MB write.
pub fn unmount_settled(id: &str) {
    use lumendisk_core::volume::runtime;
    let mp = runtime().get(id).map(|v| v.mountpoint.clone()).unwrap_or_default();
    let t0 = std::time::Instant::now();
    let mut reported = false;
    loop {
        match runtime().unmount(id, false) {
            Ok(()) => return,
            Err(e) if e.contains("Resource busy") && t0.elapsed() < std::time::Duration::from_secs(60) => {
                if !reported {
                    let holders: Vec<String> = lumendisk_core::lock_inspector::inspect(&mp.to_string_lossy()).into_iter().map(|h| format!("{} ({})", h.name, h.pid)).collect();
                    eprintln!("unmount {}: busy, held by {holders:?}; waiting", mp.display());
                    reported = true;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => {
                let holders = lumendisk_core::lock_inspector::inspect(&mp.to_string_lossy());
                panic!("unmount {}: {e} (after {:?}; held by {holders:?})", mp.display(), t0.elapsed());
            }
        }
    }
}

/// Run `sh -c <script>` inside the toolbox with `dir` mounted at /w. Returns (exit code, output).
pub fn docker(dir: &Path, script: &str) -> (i32, String) {
    let out = Command::new("docker")
        .args(["run", "--rm", "-v"])
        .arg(format!("{}:/w", dir.display()))
        .args(["-w", "/w", "lumendisk-toolbox", "sh", "-c", script])
        .output()
        .expect("docker not available");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

pub fn make_sparse(path: &Path, mb: u64) {
    let f = std::fs::File::create(path).unwrap();
    f.set_len(mb * 1024 * 1024).unwrap();
}

/// Like [`docker`] but privileged (loop mounts of the Linux kernel's own drivers).
pub fn docker_priv(dir: &Path, script: &str) -> (i32, String) {
    let out = Command::new("docker")
        .args(["run", "--rm", "--privileged", "-v"])
        .arg(format!("{}:/w", dir.display()))
        .args(["-w", "/w", "lumendisk-toolbox", "sh", "-c", script])
        .output()
        .expect("docker not available");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}
