//! SFTP v3 wire format (draft-ietf-secsh-filexfer-02): packet types, encoding helpers,
//! file attributes and status-code mapping.

use crate::remote::{RResult, RemoteError};

pub const FXP_INIT: u8 = 1;
pub const FXP_VERSION: u8 = 2;
pub const FXP_OPEN: u8 = 3;
pub const FXP_CLOSE: u8 = 4;
pub const FXP_READ: u8 = 5;
pub const FXP_WRITE: u8 = 6;
pub const FXP_LSTAT: u8 = 7;
#[allow(dead_code)]
pub const FXP_FSTAT: u8 = 8;
pub const FXP_SETSTAT: u8 = 9;
pub const FXP_FSETSTAT: u8 = 10;
pub const FXP_OPENDIR: u8 = 11;
pub const FXP_READDIR: u8 = 12;
pub const FXP_REMOVE: u8 = 13;
pub const FXP_MKDIR: u8 = 14;
pub const FXP_RMDIR: u8 = 15;
pub const FXP_REALPATH: u8 = 16;
pub const FXP_STAT: u8 = 17;
pub const FXP_RENAME: u8 = 18;
pub const FXP_READLINK: u8 = 19;
pub const FXP_SYMLINK: u8 = 20;
pub const FXP_STATUS: u8 = 101;
pub const FXP_HANDLE: u8 = 102;
pub const FXP_DATA: u8 = 103;
pub const FXP_NAME: u8 = 104;
pub const FXP_ATTRS: u8 = 105;
pub const FXP_EXTENDED: u8 = 200;
pub const FXP_EXTENDED_REPLY: u8 = 201;

pub const FXF_READ: u32 = 0x01;
pub const FXF_WRITE: u32 = 0x02;
pub const FXF_CREAT: u32 = 0x08;
pub const FXF_TRUNC: u32 = 0x10;
pub const FXF_EXCL: u32 = 0x20;

const ATTR_SIZE: u32 = 0x01;
const ATTR_UIDGID: u32 = 0x02;
const ATTR_PERMISSIONS: u32 = 0x04;
const ATTR_ACMODTIME: u32 = 0x08;
const ATTR_EXTENDED: u32 = 0x8000_0000;

pub const FX_OK: u32 = 0;
pub const FX_EOF: u32 = 1;
pub const FX_NO_SUCH_FILE: u32 = 2;
pub const FX_PERMISSION_DENIED: u32 = 3;
pub const FX_FAILURE: u32 = 4;
pub const FX_BAD_MESSAGE: u32 = 5;
pub const FX_NO_CONNECTION: u32 = 6;
pub const FX_CONNECTION_LOST: u32 = 7;
pub const FX_OP_UNSUPPORTED: u32 = 8;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;

/// Request body builder (everything after the request id).
#[derive(Default)]
pub struct Buf(pub Vec<u8>);

impl Buf {
    pub fn new() -> Self {
        Buf(Vec::with_capacity(64))
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn bytes(mut self, s: &[u8]) -> Self {
        self.0.extend_from_slice(&(s.len() as u32).to_be_bytes());
        self.0.extend_from_slice(s);
        self
    }
    pub fn str(self, s: &str) -> Self {
        self.bytes(s.as_bytes())
    }
    pub fn attrs(mut self, a: &Attrs) -> Self {
        a.encode(&mut self.0);
        self
    }
}

/// Reply body reader.
pub struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

fn short() -> RemoteError {
    RemoteError::Other("sftp: truncated packet from server".into())
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Rd { b, p: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.b.len() - self.p
    }
    pub fn u32(&mut self) -> RResult<u32> {
        let s = self.b.get(self.p..self.p + 4).ok_or_else(short)?;
        self.p += 4;
        Ok(u32::from_be_bytes(s.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> RResult<u64> {
        let s = self.b.get(self.p..self.p + 8).ok_or_else(short)?;
        self.p += 8;
        Ok(u64::from_be_bytes(s.try_into().unwrap()))
    }
    pub fn bytes(&mut self) -> RResult<&'a [u8]> {
        let n = self.u32()? as usize;
        let s = self.b.get(self.p..self.p.checked_add(n).ok_or_else(short)?).ok_or_else(short)?;
        self.p += n;
        Ok(s)
    }
    pub fn string(&mut self) -> RResult<String> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }
    pub fn attrs(&mut self) -> RResult<Attrs> {
        let flags = self.u32()?;
        let mut a = Attrs::default();
        if flags & ATTR_SIZE != 0 {
            a.size = Some(self.u64()?);
        }
        if flags & ATTR_UIDGID != 0 {
            a.uid_gid = Some((self.u32()?, self.u32()?));
        }
        if flags & ATTR_PERMISSIONS != 0 {
            a.perm = Some(self.u32()?);
        }
        if flags & ATTR_ACMODTIME != 0 {
            a.times = Some((self.u32()?, self.u32()?));
        }
        if flags & ATTR_EXTENDED != 0 {
            let n = self.u32()?;
            for _ in 0..n {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Ok(a)
    }
}

/// SFTP v3 file attributes (times are whole seconds in v3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    pub size: Option<u64>,
    pub uid_gid: Option<(u32, u32)>,
    /// Full st_mode (type + permission bits).
    pub perm: Option<u32>,
    /// (atime, mtime).
    pub times: Option<(u32, u32)>,
}

impl Attrs {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut flags = 0;
        if self.size.is_some() {
            flags |= ATTR_SIZE;
        }
        if self.uid_gid.is_some() {
            flags |= ATTR_UIDGID;
        }
        if self.perm.is_some() {
            flags |= ATTR_PERMISSIONS;
        }
        if self.times.is_some() {
            flags |= ATTR_ACMODTIME;
        }
        out.extend_from_slice(&flags.to_be_bytes());
        if let Some(s) = self.size {
            out.extend_from_slice(&s.to_be_bytes());
        }
        if let Some((u, g)) = self.uid_gid {
            out.extend_from_slice(&u.to_be_bytes());
            out.extend_from_slice(&g.to_be_bytes());
        }
        if let Some(p) = self.perm {
            out.extend_from_slice(&p.to_be_bytes());
        }
        if let Some((a, m)) = self.times {
            out.extend_from_slice(&a.to_be_bytes());
            out.extend_from_slice(&m.to_be_bytes());
        }
    }
}

/// Map an SSH_FXP_STATUS code (+ server message) to a RemoteError. `FX_FAILURE` becomes
/// `Other(..)`; callers refine it (EEXIST / ENOTEMPTY / EISDIR all arrive as FAILURE from
/// OpenSSH's v3 server).
pub fn status_error(code: u32, msg: &str) -> RemoteError {
    let m = if msg.is_empty() { "" } else { msg };
    match code {
        FX_EOF => RemoteError::Other("unexpected EOF".into()),
        FX_NO_SUCH_FILE => RemoteError::NotFound,
        FX_PERMISSION_DENIED => RemoteError::Perm,
        FX_FAILURE => RemoteError::Other(format!("sftp failure: {}", if m.is_empty() { "Failure" } else { m })),
        FX_BAD_MESSAGE => RemoteError::Other(format!("sftp bad message: {m}")),
        FX_NO_CONNECTION | FX_CONNECTION_LOST => RemoteError::Disconnected(format!("sftp: {m}")),
        FX_OP_UNSUPPORTED => RemoteError::Unsupported(format!("sftp: {m}")),
        c => RemoteError::Other(format!("sftp status {c}: {m}")),
    }
}

/// True for errors that came from a generic SSH_FX_FAILURE / BAD_MESSAGE status.
pub fn is_failure(e: &RemoteError) -> bool {
    matches!(e, RemoteError::Other(m) if m.starts_with("sftp failure") || m.starts_with("sftp bad message"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attrs_roundtrip() {
        let a = Attrs { size: Some(5), uid_gid: Some((1, 2)), perm: Some(0o100644), times: Some((3, 4)) };
        let mut v = Vec::new();
        a.encode(&mut v);
        assert_eq!(Rd::new(&v).attrs().unwrap(), a);
        let e = Attrs::default();
        let mut v = Vec::new();
        e.encode(&mut v);
        assert_eq!(v, vec![0, 0, 0, 0]);
    }

    #[test]
    fn reader_rejects_truncation() {
        let b = Buf::new().str("hello").0;
        assert_eq!(Rd::new(&b).string().unwrap(), "hello");
        assert!(Rd::new(&b[..6]).string().is_err());
        assert!(Rd::new(&[0, 0]).u32().is_err());
    }
}
