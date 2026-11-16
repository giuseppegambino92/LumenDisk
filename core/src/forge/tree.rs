//! A read-only tree of files (the contents of an ISO, a folder) and copying it into a volume
//! formatted by LumenDisk (FAT32, exFAT, NTFS, ext4 via the `vfs` drivers).

use std::path::PathBuf;

use super::job::JobCtx;
use super::{ForgeError, ForgeResult};
use crate::vfs::{FileSystem, FsError, Ino, SetAttr, Timestamp};

/// One entry of a [`SourceTree`]. Paths are absolute inside the tree, `/`-separated, without
/// a trailing slash (`/sources/install.wim`); the root itself is not listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix permission bits when the source has them (Rock Ridge, folders); 0 otherwise.
    pub mode: u32,
    /// Seconds since the epoch.
    pub mtime: i64,
    pub symlink: Option<String>,
}

impl TreeEntry {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or("")
    }
    pub fn parent(&self) -> &str {
        match self.path.rfind('/') {
            Some(0) | None => "/",
            Some(i) => &self.path[..i],
        }
    }
}

pub trait SourceTree: Send {
    /// Every entry, parents before their children.
    fn entries(&mut self) -> ForgeResult<Vec<TreeEntry>>;
    /// Read file bytes at `offset`; returns how many were read (0 at the end).
    fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> ForgeResult<usize>;
    /// The volume label of the source (ISO volume id, folder name).
    fn label(&self) -> String;
}

/// Case-insensitive lookup helper: the entry whose path equals `path` ignoring ASCII case
/// (ISO 9660 names are upper case, Joliet / UDF keep the original case).
pub fn find<'a>(entries: &'a [TreeEntry], path: &str) -> Option<&'a TreeEntry> {
    entries.iter().find(|e| e.path.eq_ignore_ascii_case(path))
}

/// A folder on the Mac as a [`SourceTree`] (tests, and "copy this folder" requests).
pub struct DirTree {
    root: PathBuf,
}

impl DirTree {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DirTree { root: root.into() }
    }
    fn host(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
}

impl SourceTree for DirTree {
    fn entries(&mut self) -> ForgeResult<Vec<TreeEntry>> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mut out = vec![];
        let mut todo = vec![String::new()];
        while let Some(rel) = todo.pop() {
            let mut names: Vec<_> = std::fs::read_dir(self.host(&rel))?.flatten().collect();
            names.sort_by_key(|e| e.file_name());
            for e in names {
                let name = e.file_name().to_string_lossy().into_owned();
                let path = format!("{rel}/{name}");
                let md = std::fs::symlink_metadata(e.path())?;
                let symlink = md.file_type().is_symlink().then(|| std::fs::read_link(e.path()).map(|p| p.to_string_lossy().into_owned())).transpose()?;
                let is_dir = md.is_dir();
                out.push(TreeEntry { path: path.clone(), is_dir, size: if is_dir { 0 } else { md.len() }, mode: md.permissions().mode() & 0o7777, mtime: md.mtime(), symlink });
                if is_dir {
                    todo.push(path);
                }
            }
        }
        // parents before children
        out.sort_by(|a, b| a.path.split('/').count().cmp(&b.path.split('/').count()).then(a.path.cmp(&b.path)));
        Ok(out)
    }

    fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> ForgeResult<usize> {
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::open(self.host(path))?;
        Ok(f.read_at(buf, offset)?)
    }

    fn label(&self) -> String {
        self.root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// How [`copy_tree`] treats each entry.
pub enum CopyAction {
    Copy,
    Skip,
    /// Copy under another path (e.g. rename `/sources/install.wim` → handled elsewhere).
    Rename(String),
}

/// Copy every entry of `tree` into `fs` (an empty, freshly formatted volume), reporting bytes
/// in the current job phase. `filter` decides per entry. Files larger than `max_file` (FAT32:
/// 4 GiB − 1) fail with a clear message unless filtered out. Returns bytes copied.
pub fn copy_tree(
    tree: &mut dyn SourceTree,
    fs: &mut dyn FileSystem,
    ctx: &JobCtx,
    max_file: Option<u64>,
    mut filter: impl FnMut(&TreeEntry) -> CopyAction,
) -> ForgeResult<u64> {
    const CHUNK: usize = 4 << 20;
    let entries = tree.entries()?;
    let total: u64 = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
    ctx.set_total(total);
    let mut buf = vec![0u8; CHUNK];
    let mut copied = 0u64;
    for e in &entries {
        let dest = match filter(e) {
            CopyAction::Skip => continue,
            CopyAction::Copy => e.path.clone(),
            CopyAction::Rename(p) => p,
        };
        let (parent, name) = split(&dest);
        let pino = mkdir_p(fs, parent)?;
        if e.is_dir {
            match fs.mkdir(pino, name, 0o755, 0, 0) {
                Ok(_) | Err(FsError::Exists) => {}
                Err(err) => return Err(io_at(&dest, err)),
            }
            continue;
        }
        if let Some(target) = &e.symlink {
            match fs.symlink(pino, name, target, 0, 0) {
                Ok(_) => {}
                // FAT / exFAT have no symlinks: skip them, as Rufus does.
                Err(FsError::Unsupported(_)) | Err(FsError::Perm) => ctx.warn(format!("{dest}: symbolic link skipped")),
                Err(err) => return Err(io_at(&dest, err)),
            }
            continue;
        }
        if let Some(max) = max_file {
            if e.size > max {
                return Err(ForgeError::Unsupported(crate::i18n::t(
                    &format!("{dest} è più grande di 4 GB e non entra in FAT32"),
                    &format!("{dest} is larger than 4 GB and does not fit on FAT32"),
                )));
            }
        }
        let ino = fs.create(pino, name, (e.mode & 0o777).max(0o644) as u16, 0, 0).map_err(|err| io_at(&dest, err))?;
        let mut off = 0u64;
        while off < e.size {
            let want = CHUNK.min((e.size - off) as usize);
            let n = tree.read(&e.path, off, &mut buf[..want])?;
            if n == 0 {
                return Err(ForgeError::Io(format!("{}: source ended early ({} of {} bytes)", e.path, off, e.size)));
            }
            let mut w = 0usize;
            while w < n {
                let k = fs.write(ino, off + w as u64, &buf[w..n]).map_err(|err| io_at(&dest, err))? as usize;
                if k == 0 {
                    return Err(ForgeError::Io(format!("{dest}: short write")));
                }
                w += k;
            }
            off += n as u64;
            copied += n as u64;
            ctx.advance(n as u64)?;
        }
        if e.mtime > 0 {
            let ts = Timestamp { secs: e.mtime, nsecs: 0 };
            let _ = fs.setattr(ino, &SetAttr { mtime: Some(ts), ..Default::default() });
        }
    }
    fs.sync()?;
    Ok(copied)
}

fn io_at(path: &str, e: FsError) -> ForgeError {
    match e {
        FsError::NoSpace => ForgeError::Io(crate::i18n::t(&format!("spazio esaurito copiando {path}"), &format!("out of space copying {path}"))),
        other => ForgeError::Io(format!("{path}: {other}")),
    }
}

fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(0) => ("/", &path[1..]),
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("/", path),
    }
}

/// Inode of directory `path` in `fs`, creating missing components.
pub fn mkdir_p(fs: &mut dyn FileSystem, path: &str) -> ForgeResult<Ino> {
    let mut ino = fs.root_ino();
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        ino = match fs.lookup(ino, comp) {
            Ok(i) => i,
            Err(FsError::NotFound) => fs.mkdir(ino, comp, 0o755, 0, 0).map_err(|e| io_at(path, e))?,
            Err(e) => return Err(io_at(path, e)),
        };
    }
    Ok(ino)
}

/// Write a whole small file at `path` (creating parents), replacing it if it exists.
pub fn write_file(fs: &mut dyn FileSystem, path: &str, data: &[u8]) -> ForgeResult<()> {
    let (parent, name) = split(path);
    let pino = mkdir_p(fs, parent)?;
    match fs.lookup(pino, name) {
        Ok(_) => fs.unlink(pino, name).map_err(|e| io_at(path, e))?,
        Err(FsError::NotFound) => {}
        Err(e) => return Err(io_at(path, e)),
    }
    let ino = fs.create(pino, name, 0o644, 0, 0).map_err(|e| io_at(path, e))?;
    let mut off = 0usize;
    while off < data.len() {
        let n = fs.write(ino, off as u64, &data[off..]).map_err(|e| io_at(path, e))? as usize;
        if n == 0 {
            return Err(ForgeError::Io(format!("{path}: short write")));
        }
        off += n;
    }
    Ok(())
}

/// Read a whole file at `path` (None if missing). Case-insensitive on each component.
pub fn read_file(fs: &mut dyn FileSystem, path: &str) -> ForgeResult<Option<Vec<u8>>> {
    let mut ino = fs.root_ino();
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        let found = match fs.lookup(ino, comp) {
            Ok(i) => Some(i),
            Err(FsError::NotFound) => fs.readdir(ino)?.into_iter().find(|d| d.name.eq_ignore_ascii_case(comp)).map(|d| d.ino),
            Err(e) => return Err(io_at(path, e)),
        };
        match found {
            Some(i) => ino = i,
            None => return Ok(None),
        }
    }
    let size = fs.getattr(ino)?.size;
    let mut out = Vec::with_capacity(size as usize);
    while (out.len() as u64) < size {
        let chunk = fs.read(ino, out.len() as u64, (1 << 20).min(size - out.len() as u64) as u32)?;
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BlockDev, MemDev};
    use crate::vfs::{self, FsType, MountDefaults};
    use std::sync::Arc;

    #[test]
    fn a_folder_is_copied_into_fat32_and_ext4_byte_for_byte() {
        let src = std::env::temp_dir().join(format!("ld-tree-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(src.join("EFI/BOOT")).unwrap();
        std::fs::create_dir_all(src.join("empty")).unwrap();
        std::fs::write(src.join("EFI/BOOT/BOOTX64.EFI"), vec![7u8; 300_000]).unwrap();
        std::fs::write(src.join("readme.txt"), b"hello").unwrap();
        for fst in [FsType::Fat32, FsType::Ext4] {
            let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(64 << 20));
            vfs::format(dev.clone(), fst, "T").unwrap();
            let mut fs = vfs::open_fs(dev.clone(), false, MountDefaults::default()).unwrap();
            let ctx = JobCtx::detached(false);
            let mut tree = DirTree::new(&src);
            let n = copy_tree(&mut tree, fs.as_mut(), &ctx, Some(u32::MAX as u64), |_| CopyAction::Copy).unwrap();
            assert_eq!(n, 300_005);
            assert_eq!(ctx.done(), 300_005);
            assert_eq!(read_file(fs.as_mut(), "/efi/boot/bootx64.efi").unwrap().unwrap(), vec![7u8; 300_000], "{fst:?}");
            assert_eq!(read_file(fs.as_mut(), "/readme.txt").unwrap().unwrap(), b"hello");
            assert!(read_file(fs.as_mut(), "/missing").unwrap().is_none());
            write_file(fs.as_mut(), "/a/b/c.cfg", b"x=1").unwrap();
            write_file(fs.as_mut(), "/a/b/c.cfg", b"x=2").unwrap();
            assert_eq!(read_file(fs.as_mut(), "/a/b/c.cfg").unwrap().unwrap(), b"x=2");
        }
        std::fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn files_too_big_for_the_target_are_refused_with_their_name() {
        let src = std::env::temp_dir().join(format!("ld-tree-big-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("big.wim"), vec![0u8; 2000]).unwrap();
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(64 << 20));
        vfs::format(dev.clone(), FsType::Fat32, "T").unwrap();
        let mut fs = vfs::open_fs(dev, false, MountDefaults::default()).unwrap();
        let e = copy_tree(&mut DirTree::new(&src), fs.as_mut(), &JobCtx::detached(false), Some(1000), |_| CopyAction::Copy).unwrap_err();
        assert!(e.to_string().contains("big.wim"), "{e}");
        std::fs::remove_dir_all(&src).unwrap();
    }
}
