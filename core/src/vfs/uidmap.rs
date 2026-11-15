//! Feature 3 — Transparent UID/GID Permission Bridge (Linux ⇄ macOS).
//!
//! On ext2/3/4 volumes files usually belong to the Linux user (1000:1000 on Debian/Ubuntu/
//! Raspberry Pi OS) while the Mac user is 501:20. The bridge presents files owned by the
//! Linux owner as owned by the Mac user, and translates the Mac user back to the Linux owner
//! for everything created or chown'ed from macOS — so the disk looks untouched when it goes
//! back to the Linux machine.

use std::sync::{Arc, RwLock};

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UidMap {
    pub enabled: bool,
    pub mac_uid: u32,
    pub mac_gid: u32,
    pub linux_uid: u32,
    pub linux_gid: u32,
}

impl UidMap {
    pub fn to_mac(&self, uid: u32, gid: u32) -> (u32, u32) {
        if !self.enabled {
            return (uid, gid);
        }
        let u = if uid == self.linux_uid { self.mac_uid } else { uid };
        let g = if gid == self.linux_gid { self.mac_gid } else { gid };
        (u, g)
    }
    pub fn uid_to_linux(&self, uid: u32) -> u32 {
        if self.enabled && uid == self.mac_uid {
            self.linux_uid
        } else {
            uid
        }
    }
    pub fn gid_to_linux(&self, gid: u32) -> u32 {
        if self.enabled && gid == self.mac_gid {
            self.linux_gid
        } else {
            gid
        }
    }
}

pub type SharedUidMap = Arc<RwLock<UidMap>>;

/// Guess the Linux owner of a volume: the most common non-root owner among the root
/// directory, its children and the children of `/home`. Falls back to 1000:1000.
pub fn detect_owner(fs: &mut dyn FileSystem) -> (u32, u32) {
    let mut counts: std::collections::HashMap<(u32, u32), usize> = Default::default();
    let root = fs.root_ino();
    let consider = |fs: &mut dyn FileSystem, dir: Ino, weight: usize, counts: &mut std::collections::HashMap<(u32, u32), usize>| {
        if let Ok(entries) = fs.readdir(dir) {
            for e in entries.iter().take(512) {
                if e.name == "lost+found" {
                    continue;
                }
                if let Ok(a) = fs.getattr(e.ino) {
                    if a.uid != 0 {
                        *counts.entry((a.uid, a.gid)).or_default() += weight;
                    }
                }
            }
        }
    };
    if let Ok(a) = fs.getattr(root) {
        if a.uid != 0 {
            *counts.entry((a.uid, a.gid)).or_default() += 3;
        }
    }
    consider(fs, root, 1, &mut counts);
    if let Ok(home) = fs.lookup(root, "home") {
        consider(fs, home, 5, &mut counts);
    }
    counts.into_iter().max_by_key(|(k, c)| (*c, std::cmp::Reverse(k.0))).map(|(k, _)| k).unwrap_or((1000, 1000))
}

pub struct PermissionBridge {
    pub inner: Box<dyn FileSystem>,
    pub map: SharedUidMap,
}

impl PermissionBridge {
    fn m(&self) -> UidMap {
        *self.map.read().unwrap()
    }
    fn fix(&self, mut a: Attr) -> Attr {
        let (u, g) = self.m().to_mac(a.uid, a.gid);
        a.uid = u;
        a.gid = g;
        a
    }
}

impl FileSystem for PermissionBridge {
    fn volume_info(&self) -> VolumeInfo {
        self.inner.volume_info()
    }
    fn statfs(&mut self) -> FsResult<FsStat> {
        self.inner.statfs()
    }
    fn root_ino(&self) -> Ino {
        self.inner.root_ino()
    }
    fn lookup(&mut self, parent: Ino, name: &str) -> FsResult<Ino> {
        self.inner.lookup(parent, name)
    }
    fn getattr(&mut self, ino: Ino) -> FsResult<Attr> {
        let a = self.inner.getattr(ino)?;
        Ok(self.fix(a))
    }
    fn setattr(&mut self, ino: Ino, set: &SetAttr) -> FsResult<Attr> {
        let m = self.m();
        let mut s = set.clone();
        s.uid = s.uid.map(|u| m.uid_to_linux(u));
        s.gid = s.gid.map(|g| m.gid_to_linux(g));
        let a = self.inner.setattr(ino, &s)?;
        Ok(self.fix(a))
    }
    fn read(&mut self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>> {
        self.inner.read(ino, offset, len)
    }
    fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<u32> {
        self.inner.write(ino, offset, data)
    }
    fn readdir(&mut self, dir: Ino) -> FsResult<Vec<DirEntry>> {
        self.inner.readdir(dir)
    }
    fn create(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino> {
        let m = self.m();
        self.inner.create(parent, name, perm, m.uid_to_linux(uid), m.gid_to_linux(gid))
    }
    fn mkdir(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino> {
        let m = self.m();
        self.inner.mkdir(parent, name, perm, m.uid_to_linux(uid), m.gid_to_linux(gid))
    }
    fn symlink(&mut self, parent: Ino, name: &str, target: &str, uid: u32, gid: u32) -> FsResult<Ino> {
        let m = self.m();
        self.inner.symlink(parent, name, target, m.uid_to_linux(uid), m.gid_to_linux(gid))
    }
    fn readlink(&mut self, ino: Ino) -> FsResult<String> {
        self.inner.readlink(ino)
    }
    fn unlink(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.inner.unlink(parent, name)
    }
    fn rmdir(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.inner.rmdir(parent, name)
    }
    fn rename(&mut self, a: Ino, b: &str, c: Ino, d: &str) -> FsResult<()> {
        self.inner.rename(a, b, c, d)
    }
    fn sync(&mut self) -> FsResult<()> {
        self.inner.sync()
    }
    fn dirty_state(&mut self) -> DirtyState {
        self.inner.dirty_state()
    }
    fn repair(&mut self) -> FsResult<String> {
        self.inner.repair()
    }
    fn read_only_reason(&self) -> Option<String> {
        self.inner.read_only_reason()
    }
    fn fingerprint(&mut self) -> Option<String> {
        self.inner.fingerprint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BlockDev, MemDev};
    use crate::vfs::ext4_fs;

    #[test]
    fn bridge_translates_both_ways() {
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(32 << 20));
        ext4_fs::format(dev.clone(), FsType::Ext4, "t").unwrap();
        let mut raw = ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
        let root = raw.root_ino();
        let home = raw.mkdir(root, "home", 0o755, 0, 0).unwrap();
        raw.mkdir(home, "pi", 0o755, 1000, 1000).unwrap();
        raw.create(home, "x", 0o644, 1000, 1000).unwrap();
        let (u, g) = detect_owner(&mut raw);
        assert_eq!((u, g), (1000, 1000));
        let map = Arc::new(RwLock::new(UidMap { enabled: true, mac_uid: 501, mac_gid: 20, linux_uid: u, linux_gid: g }));
        let mut b = PermissionBridge { inner: Box::new(raw), map };
        let pi = b.lookup(home, "pi").unwrap();
        let a = b.getattr(pi).unwrap();
        assert_eq!((a.uid, a.gid), (501, 20));
        let f = b.create(pi, "from_mac.txt", 0o644, 501, 20).unwrap();
        assert_eq!(b.getattr(f).unwrap().uid, 501);
        drop(b);
        let mut raw = ext4_fs::Ext4Fs::open(dev, false).unwrap();
        let a = raw.getattr(f).unwrap();
        assert_eq!((a.uid, a.gid), (1000, 1000), "new file must be owned by the Linux user on disk");
    }
}
