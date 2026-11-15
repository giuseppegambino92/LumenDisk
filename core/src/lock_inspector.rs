//! Feature 4 — Process Lock Inspector ("Chi blocca il disco?").
//!
//! Enumerates processes with libproc (`proc_listallpids`, `proc_pidinfo(PROC_PIDLISTFDS)`,
//! `proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO)`, `proc_pidinfo(PROC_PIDVNODEPATHINFO)` for the
//! working directory) and reports every process holding a file — or its current directory —
//! below a mountpoint. Only processes of the current user are visible without root, which is
//! exactly the set we're allowed to ask to quit.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::mem::{size_of, zeroed};

#[derive(Clone, Debug, serde::Serialize)]
pub struct LockHolder {
    pub pid: i32,
    pub name: String,
    pub executable: Option<String>,
    pub files: Vec<String>,
}

const PROC_PIDFDVNODEPATHINFO: i32 = 2;

/// `struct proc_fileinfo` from <sys/proc_info.h> (not exported by the libc crate).
#[repr(C)]
#[allow(dead_code)]
struct ProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: i64,
    fi_type: i32,
    fi_guardflags: u32,
}

/// `struct vnode_fdinfowithpath`.
#[repr(C)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    pvip: libc::vnode_info_path,
}

fn cpath<T>(buf: &T) -> String {
    // SAFETY: vip_path is a MAXPATHLEN char buffer, NUL terminated by the kernel.
    unsafe { CStr::from_ptr(buf as *const T as *const libc::c_char) }.to_string_lossy().into_owned()
}

fn under(path: &str, mp: &str) -> bool {
    path == mp || path.starts_with(&format!("{}/", mp.trim_end_matches('/')))
}

pub fn proc_name(pid: i32) -> String {
    let mut buf = [0u8; 256];
    // SAFETY: valid buffer.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut _, buf.len() as u32) };
    if n > 0 {
        String::from_utf8_lossy(&buf[..n as usize]).into_owned()
    } else {
        format!("pid {pid}")
    }
}

pub fn proc_path(pid: i32) -> Option<String> {
    let mut buf = [0u8; 4096];
    // SAFETY: valid buffer.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut _, buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// Processes holding files below `mountpoint`.
pub fn inspect(mountpoint: &str) -> Vec<LockHolder> {
    let mp = std::fs::canonicalize(mountpoint).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| mountpoint.to_string());
    let me = std::process::id() as i32;
    let mut holders: BTreeMap<i32, LockHolder> = BTreeMap::new();
    // SAFETY: libproc calls with correctly sized buffers.
    unsafe {
        let n = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if n <= 0 {
            return vec![];
        }
        let mut pids = vec![0i32; n as usize + 64];
        let n = libc::proc_listallpids(pids.as_mut_ptr() as *mut _, (pids.len() * size_of::<i32>()) as i32);
        pids.truncate(n.max(0) as usize);
        for pid in pids {
            if pid <= 0 || pid == me {
                continue;
            }
            let mut files = vec![];
            // cwd
            let mut vpi: libc::proc_vnodepathinfo = zeroed();
            let r = libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, &mut vpi as *mut _ as *mut _, size_of::<libc::proc_vnodepathinfo>() as i32);
            if r == size_of::<libc::proc_vnodepathinfo>() as i32 {
                let cwd = cpath(&vpi.pvi_cdir.vip_path);
                if under(&cwd, &mp) {
                    files.push(format!("{cwd} (cartella di lavoro)"));
                }
            }
            // fds
            let bytes = libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0);
            if bytes > 0 {
                let cnt = bytes as usize / size_of::<libc::proc_fdinfo>() + 16;
                let mut fds: Vec<libc::proc_fdinfo> = vec![zeroed(); cnt];
                let got = libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr() as *mut _, (cnt * size_of::<libc::proc_fdinfo>()) as i32);
                let got = got.max(0) as usize / size_of::<libc::proc_fdinfo>();
                for fd in &fds[..got] {
                    if fd.proc_fdtype as i32 != libc::PROX_FDTYPE_VNODE {
                        continue;
                    }
                    let mut vi: VnodeFdInfoWithPath = zeroed();
                    let r = libc::proc_pidfdinfo(pid, fd.proc_fd, PROC_PIDFDVNODEPATHINFO, &mut vi as *mut _ as *mut _, size_of::<VnodeFdInfoWithPath>() as i32);
                    if r == size_of::<VnodeFdInfoWithPath>() as i32 {
                        let p = cpath(&vi.pvip.vip_path);
                        if under(&p, &mp) && !files.contains(&p) {
                            files.push(p);
                        }
                    }
                }
            }
            if !files.is_empty() {
                holders.insert(pid, LockHolder { pid, name: proc_name(pid), executable: proc_path(pid), files });
            }
        }
    }
    holders.into_values().collect()
}

/// Ask holders to quit (SIGTERM), wait up to `grace_ms`, then SIGKILL the stubborn ones.
/// Returns the pids that are gone.
pub fn release(pids: &[i32], grace_ms: u64) -> Vec<i32> {
    // SAFETY: getuid is always safe.
    let me = std::process::id() as i32;
    let mut targets: Vec<i32> = pids.iter().copied().filter(|&p| p > 1 && p != me).collect();
    for &p in &targets {
        // SAFETY: plain kill(2).
        unsafe { libc::kill(p, libc::SIGTERM) };
    }
    // A process counts as alive if it exists and is not a zombie (exited, not yet reaped).
    let alive = |p: i32| unsafe {
        if libc::kill(p, 0) != 0 {
            return false;
        }
        let mut bi: libc::proc_bsdinfo = zeroed();
        let n = libc::proc_pidinfo(p, libc::PROC_PIDTBSDINFO, 0, &mut bi as *mut _ as *mut _, size_of::<libc::proc_bsdinfo>() as i32);
        // proc_pidinfo fails for zombies; we only ever target our own user's processes.
        n == size_of::<libc::proc_bsdinfo>() as i32 && bi.pbi_status != 5 /* SZOMB */
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(grace_ms);
    while std::time::Instant::now() < deadline && targets.iter().any(|&p| alive(p)) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    for &p in &targets {
        if alive(p) {
            // SAFETY: plain kill(2).
            unsafe { libc::kill(p, libc::SIGKILL) };
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    targets.retain(|&p| !alive(p));
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_process_holding_a_file() {
        let dir = std::env::temp_dir().join(format!("ld-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("held.txt");
        std::fs::write(&f, b"x").unwrap();
        let mut child = std::process::Command::new("/usr/bin/tail").arg("-f").arg(&f).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let h = inspect(dir.to_str().unwrap());
        assert!(h.iter().any(|x| x.pid == child.id() as i32 && x.files.iter().any(|p| p.ends_with("held.txt"))), "{h:?}");
        let gone = release(&[child.id() as i32], 2000);
        assert_eq!(gone, vec![child.id() as i32]);
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
