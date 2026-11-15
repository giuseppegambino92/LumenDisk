//! Reparse points and symbolic links.
//!
//! Formats we *create* (chosen so that `readlink` returns the target byte-for-byte, and so
//! that the Linux kernel ntfs3 driver, ntfs-3g and Windows all see a standard symlink):
//! * relative targets: `IO_REPARSE_TAG_SYMLINK` with `SYMLINK_FLAG_RELATIVE`, '/' stored as
//!   '\', SubstituteName == PrintName (UTF-16LE). If the target resolves (at creation time, or
//!   later through `fix_pending_dir_links`) to an existing directory, the link is a directory
//!   record (empty $I30 index), like Windows' `mklink /D`: Windows and ntfs-3g only follow a
//!   relative symlink whose record type matches its target.
//! * absolute POSIX targets `/x/y`: `IO_REPARSE_TAG_SYMLINK`, flags 0, SubstituteName =
//!   the canonical NT path `\??\C:\x\y`, PrintName = `\x\y` (the POSIX path with '\').
//!   The mapping `/x/y` <-> `\??\C:\x\y` is independent of any mount point. The PrintName is
//!   deliberately the drive-less rooted path: Windows resolves only the SubstituteName and
//!   just displays the PrintName, while the kernel ntfs3 driver returns the *PrintName*
//!   ('\' -> '/') from `readlink`, so it reads `/x/y` back verbatim. Always a file record:
//!   the host namespace the target lives in is not ours to inspect.
//! * fallback, `IO_REPARSE_TAG_LX_SYMLINK` (WSL: `u32 version = 2` + UTF-8 target, the buffer
//!   ntfs-3g writes with `-o special_files=wsl`) when the target cannot be expressed as a
//!   Windows path without changing its meaning: a component containing '\', ':', '*', '?',
//!   '"', '<', '>', '|' or a control character (':' would name an alternate data stream on
//!   Windows, '\' would not survive the '/' <-> '\' mapping), or a target so long that the
//!   two UTF-16 names exceed the 16 KiB reparse data limit.
//!
//! What each implementation's `readlink` returns for what we write (measured with ntfs-3g
//! 2022.10.3 and kernel ntfs3 7.0, see the `ntfs_it_symlinks_*` tests):
//!
//! | link we create          | ntfs3                | ntfs-3g                                   | Windows               |
//! |-------------------------|----------------------|-------------------------------------------|-----------------------|
//! | `rel/x` (relative)      | `rel/x`              | `rel/x` (only if the target exists, else "unsupported reparse tag") | `rel\x` relative |
//! | `/x/y` (absolute)       | `/x/y`               | `<mnt>/x/y` if `x/y` exists on this volume, else `<mnt>/.NTFS-3G/C:/x/y` | `C:\x\y` |
//! | LX fallback             | not a link (an empty regular file) | verbatim                    | only inside WSL       |
//!
//! ntfs-3g cannot return an absolute target verbatim from any `IO_REPARSE_TAG_SYMLINK` by
//! design: it always resolves absolute (drive-letter) links under its own mount point
//! (`.NTFS-3G/C:` is meant to be a user-provided symlink to wherever drive C: is mounted).
//! The alternatives were measured too: the canonical buffer with PrintName `C:\x\y` (what
//! `mklink` writes) makes ntfs3 return `C:/x/y`; the form ntfs3 itself writes for
//! `ln -s /x/y` (relative flag + `\x\y`) means "root of the link's own volume" on Windows and
//! ntfs-3g reports it as "unsupported reparse tag" unless `x/y` exists on the volume.
//!
//! Formats we *read* as symlinks:
//! * `IO_REPARSE_TAG_SYMLINK` relative -> POSIX relative path ('\' -> '/'). This includes
//!   what the kernel ntfs3 driver writes for `ln -s /abs/path` (relative flag, `\abs\path`).
//! * `IO_REPARSE_TAG_SYMLINK` absolute and `IO_REPARSE_TAG_MOUNT_POINT` (junctions): the NT
//!   path `\??\C:\x\y` is reported as `/x/y`, i.e. the drive letter is dropped and the path is
//!   given from the root of the *host* namespace. (ntfs-3g instead reports
//!   `<its mount point>/x/y` when the target exists on the volume, or
//!   `<mount point>/.NTFS-3G/C:/x/y`; we do not know the macOS mount point and keep the
//!   target stable.) `\??\UNC\srv\share\x` -> `//srv/share/x`, `\??\Volume{GUID}\x` -> `/x`.
//! * `IO_REPARSE_TAG_LX_SYMLINK` (WSL, ntfs-3g `special_files=wsl`).
//! * Interix symlinks as created by ntfs-3g's default `ln -s` (no reparse point: a SYSTEM
//!   file whose data is `IntxLNK\x01` + UTF-16LE target).
//!
//! Every other reparse tag (dedup, OneDrive/cloud placeholders, AppExecLink, WCI, ...) is not
//! a symlink: those files and directories keep their regular kind.
//!
//! `$Extend/$Reparse:$R` is a view index (collation `NTOFS_ULONGS`) keyed by
//! `{u32 reparse_tag, u64 file_reference}` with no data (entry data_offset 0x1C, length 0),
//! exactly as ntfs-3g and ntfs3 write it. We insert on symlink creation and remove when the
//! record is freed; the duplicate info in $FILE_NAME / index entries carries
//! FILE_ATTRIBUTE_REPARSE_POINT and the tag, as Windows does.

use super::layout::*;

pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
pub const IO_REPARSE_TAG_LX_SYMLINK: u32 = 0xA000_001D;
pub const SYMLINK_FLAG_RELATIVE: u32 = 1;

/// Largest symlink target we create (bytes, UTF-8, like Linux PATH_MAX - 1).
pub const MAX_TARGET: usize = 4095;

/// Largest reparse point (header included) NTFS accepts (`MAXIMUM_REPARSE_DATA_BUFFER_SIZE`).
pub const MAX_REPARSE: usize = 16 * 1024;

pub const INTERIX_MAGIC: &[u8; 8] = b"IntxLNK\x01";

/// "$R" and "$Reparse".
pub const R_INDEX: &[u16] = &[0x24, 0x52];
pub const REPARSE_NAME: &str = "$Reparse";

/// Can `target` be stored as a Windows path (components joined by '\') without changing
/// its meaning?
fn windows_representable(target: &str) -> bool {
    !target.chars().any(|c| matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || (c as u32) < 0x20)
}

/// Build the `$REPARSE_POINT` value for a POSIX symlink target. Returns `(tag, value)`.
pub fn build_symlink(target: &str) -> (u32, Vec<u8>) {
    let win = |s: &str| -> Vec<u16> { s.encode_utf16().map(|c| if c == '/' as u16 { '\\' as u16 } else { c }).collect() };
    let sym = if !windows_representable(target) {
        None
    } else if target.starts_with('/') {
        let print = win(target);
        let mut subst = to_utf16("\\??\\C:");
        subst.extend_from_slice(&print);
        symlink_value(&subst, &print, 0)
    } else {
        let w = win(target);
        symlink_value(&w, &w, SYMLINK_FLAG_RELATIVE)
    };
    if let Some(v) = sym {
        return (IO_REPARSE_TAG_SYMLINK, v);
    }
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(target.as_bytes());
    (IO_REPARSE_TAG_LX_SYMLINK, wrap(IO_REPARSE_TAG_LX_SYMLINK, &data))
}

/// `IO_REPARSE_TAG_SYMLINK` value (SubstituteName first, then PrintName), or `None` if it
/// would exceed the NTFS reparse size limit.
fn symlink_value(subst: &[u16], print: &[u16], flags: u32) -> Option<Vec<u8>> {
    let (s, p) = (utf16_bytes(subst), utf16_bytes(print));
    if 8 + 12 + s.len() + p.len() > MAX_REPARSE {
        return None;
    }
    let mut data = vec![0u8; 12];
    put16(&mut data, 0, 0); // SubstituteNameOffset
    put16(&mut data, 2, s.len() as u16); // SubstituteNameLength
    put16(&mut data, 4, s.len() as u16); // PrintNameOffset
    put16(&mut data, 6, p.len() as u16); // PrintNameLength
    put32(&mut data, 8, flags);
    data.extend_from_slice(&s);
    data.extend_from_slice(&p);
    Some(wrap(IO_REPARSE_TAG_SYMLINK, &data))
}

/// True if a target built by [`build_symlink`] is a relative `IO_REPARSE_TAG_SYMLINK`, the
/// only kind whose record type (file/directory) we choose by resolving it on the volume.
pub fn is_relative_symlink(tag: u32, target: &str) -> bool {
    tag == IO_REPARSE_TAG_SYMLINK && !target.starts_with('/')
}

fn wrap(tag: u32, data: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    put32(&mut v, 0, tag);
    put16(&mut v, 4, data.len() as u16);
    v.extend_from_slice(data);
    v
}

pub fn tag_of(value: &[u8]) -> Option<u32> {
    le32(value, 0).ok()
}

/// POSIX target of a `$REPARSE_POINT` value, or `None` if it is not a (well-formed) link.
pub fn parse_symlink(value: &[u8]) -> Option<String> {
    let tag = le32(value, 0).ok()?;
    let dlen = le16(value, 4).ok()? as usize;
    let data = value.get(8..8 + dlen)?;
    match tag {
        IO_REPARSE_TAG_LX_SYMLINK => {
            if data.len() < 4 || le32(data, 0).ok()? != 2 {
                return None;
            }
            let s = String::from_utf8_lossy(&data[4..]).into_owned();
            (!s.is_empty()).then_some(s)
        }
        IO_REPARSE_TAG_SYMLINK | IO_REPARSE_TAG_MOUNT_POINT => {
            let (hdr, flags) = if tag == IO_REPARSE_TAG_SYMLINK { (12, le32(data, 8).ok()?) } else { (8, 0) };
            let path = data.get(hdr..)?;
            let name = |off: usize, len: usize| -> Option<Vec<u16>> {
                if len % 2 != 0 {
                    return None;
                }
                Some(bytes_utf16(path.get(off..off.checked_add(len)?)?))
            };
            let subst = name(le16(data, 0).ok()? as usize, le16(data, 2).ok()? as usize)?;
            let print = name(le16(data, 4).ok()? as usize, le16(data, 6).ok()? as usize).unwrap_or_default();
            let mut u = if subst.is_empty() { print } else { subst };
            while u.last() == Some(&0) {
                u.pop();
            }
            if u.is_empty() {
                return None;
            }
            let s = String::from_utf16_lossy(&u);
            if tag == IO_REPARSE_TAG_SYMLINK && flags & SYMLINK_FLAG_RELATIVE != 0 {
                return Some(s.replace('\\', "/"));
            }
            Some(nt_to_posix(&s))
        }
        _ => None,
    }
}

/// Map an absolute NT path (`\??\C:\x\y`, `C:\x\y`, `\??\UNC\srv\share`) to POSIX.
fn nt_to_posix(s: &str) -> String {
    let mut p = s;
    for pre in ["\\??\\", "\\\\?\\", "\\\\.\\"] {
        if let Some(r) = p.strip_prefix(pre) {
            p = r;
            break;
        }
    }
    if let Some(r) = p.strip_prefix("UNC\\") {
        return format!("//{}", r.replace('\\', "/"));
    }
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        p = &p[2..];
    } else if p.starts_with("Volume{") {
        p = p.find('\\').map_or("", |i| &p[i..]);
    }
    let out = p.replace('\\', "/");
    if out.starts_with('/') {
        out
    } else {
        format!("/{out}")
    }
}

/// Target of an Interix symlink (`IntxLNK\x01` + UTF-16LE), given the start of the data.
pub fn parse_interix(data: &[u8]) -> Option<String> {
    let rest = data.strip_prefix(&INTERIX_MAGIC[..])?;
    if rest.is_empty() || rest.len() % 2 != 0 {
        return None;
    }
    let mut u = bytes_utf16(rest);
    while u.last() == Some(&0) {
        u.pop();
    }
    (!u.is_empty()).then(|| String::from_utf16_lossy(&u))
}

/// `$R` index key: `{tag, file reference}` (12 bytes).
pub fn r_key(tag: u32, mref: u64) -> Vec<u8> {
    let mut k = vec![0u8; 12];
    put32(&mut k, 0, tag);
    put64(&mut k, 4, mref);
    k
}

/// `$R` entry "data" header word: data_offset = 0x1C (header + key), data_length = 0.
pub const R_ENTRY_DATA: u64 = 0x1C;

#[cfg(test)]
mod tests {
    use super::*;

    /// `(SubstituteName, PrintName, flags)` of an `IO_REPARSE_TAG_SYMLINK` value.
    fn names(v: &[u8]) -> (String, String, u32) {
        let d = &v[8..];
        let path = &d[12..];
        let get = |o: usize, l: usize| String::from_utf16(&bytes_utf16(&path[o..o + l])).unwrap();
        let (so, sl, po, pl) = (le16(d, 0).unwrap() as usize, le16(d, 2).unwrap() as usize, le16(d, 4).unwrap() as usize, le16(d, 6).unwrap() as usize);
        assert_eq!(le16(v, 4).unwrap() as usize, 12 + sl + pl, "ReparseDataLength");
        assert_eq!(d.len(), 12 + sl + pl);
        (get(so, sl), get(po, pl), le32(d, 8).unwrap())
    }

    #[test]
    fn ntfs_reparse_symlink_roundtrip() {
        for t in ["a.txt", "../x/y z/ü€😀", "dir/", ".", "a//b", "trailing. ", "a'b&c#d"] {
            let (tag, v) = build_symlink(t);
            assert_eq!(tag, IO_REPARSE_TAG_SYMLINK, "{t}");
            assert_eq!(parse_symlink(&v).as_deref(), Some(t));
            let (s, p, f) = names(&v);
            assert_eq!((s.as_str(), p.as_str(), f), (t.replace('/', "\\").as_str(), t.replace('/', "\\").as_str(), SYMLINK_FLAG_RELATIVE));
            assert!(is_relative_symlink(tag, t));
        }
        // absolute: canonical NT SubstituteName, drive-less PrintName (what ntfs3 shows), flags 0
        for t in ["/etc/passwd", "/", "/a b/ü😀", "/x/", "//srv/share/f", "/UNC/x", "/Volume{1}/x", "/mnt/t/../y"] {
            let (tag, v) = build_symlink(t);
            assert_eq!(tag, IO_REPARSE_TAG_SYMLINK, "{t}");
            assert_eq!(parse_symlink(&v).as_deref(), Some(t), "{t}");
            let (s, p, f) = names(&v);
            assert_eq!(s, format!("\\??\\C:{}", t.replace('/', "\\")));
            assert_eq!(p, t.replace('/', "\\"));
            assert_eq!(f, 0);
            assert!(!is_relative_symlink(tag, t));
        }
        let (_, v) = build_symlink("/etc/hosts");
        assert_eq!(names(&v), ("\\??\\C:\\etc\\hosts".into(), "\\etc\\hosts".into(), 0));
        // not representable as a Windows path: WSL format
        for t in ["back\\slash", "/x\\y", "a:b", "/c:/x", "/what?", "*", "q\"", "<a>", "p|q", "tab\there", "/nl\n"] {
            let (tag, v) = build_symlink(t);
            assert_eq!(tag, IO_REPARSE_TAG_LX_SYMLINK, "{t}");
            assert_eq!(&v[8..12], &[2, 0, 0, 0]);
            assert_eq!(parse_symlink(&v).as_deref(), Some(t));
            assert!(!is_relative_symlink(tag, t));
        }
        // too long for two UTF-16 names within 16 KiB: WSL format, still round-trips
        for t in ["x".repeat(MAX_TARGET), format!("/{}", "y/".repeat(2047))] {
            let (tag, v) = build_symlink(&t);
            assert_eq!(tag, IO_REPARSE_TAG_LX_SYMLINK, "len {}", t.len());
            assert!(v.len() <= MAX_REPARSE);
            assert_eq!(parse_symlink(&v).as_deref(), Some(t.as_str()));
        }
        let (tag, v) = build_symlink(&"x".repeat(4090));
        assert_eq!(tag, IO_REPARSE_TAG_SYMLINK);
        assert!(v.len() <= MAX_REPARSE);
        // relative symlink layout: tag, length, subst, print, flags
        let (_, v) = build_symlink("d/f");
        assert_eq!(le32(&v, 0).unwrap(), 0xA000_000C);
        assert_eq!(le16(&v, 4).unwrap() as usize, v.len() - 8);
        assert_eq!(le32(&v, 16).unwrap(), 1);
        assert_eq!(bytes_utf16(&v[20..26]), "d\\f".encode_utf16().collect::<Vec<_>>());
    }

    fn ms_link(tag: u32, subst: &str, print: &str, flags: u32) -> Vec<u8> {
        let s: Vec<u8> = utf16_bytes(&to_utf16(subst));
        let p: Vec<u8> = utf16_bytes(&to_utf16(print));
        let mut d = vec![0u8; if tag == IO_REPARSE_TAG_SYMLINK { 12 } else { 8 }];
        put16(&mut d, 0, p.len() as u16);
        put16(&mut d, 2, s.len() as u16);
        put16(&mut d, 4, 0);
        put16(&mut d, 6, p.len() as u16);
        if tag == IO_REPARSE_TAG_SYMLINK {
            put32(&mut d, 8, flags);
        }
        d.extend_from_slice(&p);
        d.extend_from_slice(&s);
        wrap(tag, &d)
    }

    #[test]
    fn ntfs_reparse_windows_absolute_and_junctions() {
        assert_eq!(parse_symlink(&ms_link(IO_REPARSE_TAG_SYMLINK, "\\??\\C:\\x\\y", "C:\\x\\y", 0)).as_deref(), Some("/x/y"));
        assert_eq!(parse_symlink(&ms_link(IO_REPARSE_TAG_MOUNT_POINT, "\\??\\D:\\Users\\me", "D:\\Users\\me", 0)).as_deref(), Some("/Users/me"));
        assert_eq!(parse_symlink(&ms_link(IO_REPARSE_TAG_MOUNT_POINT, "\\??\\C:\\", "", 0)).as_deref(), Some("/"));
        assert_eq!(parse_symlink(&ms_link(IO_REPARSE_TAG_SYMLINK, "\\??\\UNC\\srv\\share\\f", "", 0)).as_deref(), Some("//srv/share/f"));
        assert_eq!(parse_symlink(&ms_link(IO_REPARSE_TAG_MOUNT_POINT, "\\??\\Volume{1234-5678}\\dir", "", 0)).as_deref(), Some("/dir"));
        assert_eq!(parse_symlink(&ms_link(IO_REPARSE_TAG_SYMLINK, "..\\up\\f.txt", "..\\up\\f.txt", 1)).as_deref(), Some("../up/f.txt"));
        // kernel ntfs3 stores "/abs" as a *relative* symlink "\abs"
        assert_eq!(parse_symlink(&ms_link(IO_REPARSE_TAG_SYMLINK, "\\etc\\passwd", "\\etc\\passwd", 1)).as_deref(), Some("/etc/passwd"));
        // other tags and garbage are not links
        assert_eq!(parse_symlink(&wrap(0x8000_0013, &[1, 2, 3, 4])), None); // dedup
        assert_eq!(parse_symlink(&wrap(0x9000_001A, &[0; 16])), None); // cloud
        assert_eq!(parse_symlink(&wrap(0x8000_001B, &[0; 16])), None); // AppExecLink
        assert_eq!(parse_symlink(&[0x0C, 0, 0, 0xA0, 0xFF, 0xFF]), None);
        assert_eq!(parse_symlink(&wrap(IO_REPARSE_TAG_LX_SYMLINK, &[1, 0, 0, 0, b'x'])), None);
        let mut bad = ms_link(IO_REPARSE_TAG_SYMLINK, "abc", "abc", 1);
        put16(&mut bad, 8 + 2, 400);
        assert_eq!(parse_symlink(&bad), None);
    }

    #[test]
    fn ntfs_reparse_interix() {
        let mut d = INTERIX_MAGIC.to_vec();
        d.extend_from_slice(&utf16_bytes(&to_utf16("f.txt")));
        assert_eq!(parse_interix(&d).as_deref(), Some("f.txt"));
        assert_eq!(parse_interix(b"IntxLNK\x01"), None);
        assert_eq!(parse_interix(b"IntxCHR\x01abcd"), None);
    }
}
