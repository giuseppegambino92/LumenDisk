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
