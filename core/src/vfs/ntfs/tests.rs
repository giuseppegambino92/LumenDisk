//! NTFS driver tests.
//!
//! Unit tests run on volumes made by our own `format()`. The `#[ignore]`d integration tests
//! need Docker and the `lumendisk-toolbox` image (ntfs-3g tools):
//!
//! ```sh
//! cargo test -p lumendisk-core --lib ntfs -- --ignored
//! ```

use super::*;
use crate::block::{FileDev, MemDev};
use crate::vfs::testutil::{check_persisted, conformance, pattern};
use std::path::{Path, PathBuf};
use std::process::Command;

fn defaults() -> MountDefaults {
    MountDefaults { uid: 501, gid: 20, file_perm: 0o644, dir_perm: 0o755 }
}

fn fresh(size: usize) -> Arc<MemDev> {
    let dev = Arc::new(MemDev::new(size));
    format(dev.clone(), "LUMEN").unwrap();
    dev
}

#[test]
fn ntfs_format_probe_open() {
    let dev = fresh(64 << 20);
    assert!(probe(dev.as_ref()));
    let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
    let vi = fs.volume_info();
    assert_eq!(vi.label, "LUMEN");
    assert_eq!(vi.fs_type, FsType::Ntfs);
    assert_eq!(fs.readdir(5).unwrap(), vec![], "system files must be hidden");
    assert_eq!(fs.lookup(5, "$MFT"), Err(FsError::NotFound));
    let a = fs.getattr(5).unwrap();
    assert_eq!((a.kind, a.uid, a.gid, a.perm), (FileKind::Dir, 501, 20, 0o755));
    assert_eq!(fs.dirty_state(), DirtyState::Clean);
    let st = fs.statfs().unwrap();
    assert_eq!(st.block_size, 4096);
    assert!(st.free_bytes > 50 << 20);
    // through the generic entry points too
    assert_eq!(crate::vfs::probe(dev.as_ref()), Some(FsType::Ntfs));
    assert!(!probe(&MemDev::new(1 << 20)));
}

#[test]
fn ntfs_conformance_own_format() {
    let dev = fresh(64 << 20);
    {
        let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
        conformance(&mut fs, true);
    }
    let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
    check_persisted(&mut fs);
    assert_eq!(fs.dirty_state(), DirtyState::Clean);
    // read-only open refuses writes
    let mut ro = NtfsFs::open(dev, true, defaults()).unwrap();
    check_persisted(&mut ro);
    assert_eq!(ro.create(5, "x", 0o644, 0, 0), Err(FsError::ReadOnly));
}

#[test]
fn ntfs_case_insensitive_and_names() {
    let dev = fresh(32 << 20);
    let mut fs = NtfsFs::open(dev, false, defaults()).unwrap();
    let f = fs.create(5, "MixedCase.TXT", 0o644, 0, 0).unwrap();
    assert_eq!(fs.lookup(5, "mixedcase.txt").unwrap(), f);
    assert_eq!(fs.lookup(5, "MIXEDCASE.TXT").unwrap(), f);
    assert_eq!(fs.create(5, "mixedcase.txt", 0o644, 0, 0), Err(FsError::Exists));
    assert_eq!(fs.readdir(5).unwrap()[0].name, "MixedCase.TXT");
    // case-only rename
    fs.rename(5, "mixedcase.txt", 5, "mixedcase.txt").unwrap();
    assert_eq!(fs.readdir(5).unwrap()[0].name, "mixedcase.txt");
    assert_eq!(fs.lookup(5, "MIXEDCASE.txt").unwrap(), f);
    // non-Win32 names (POSIX namespace) and long names
    let p = fs.create(5, "a:b?c", 0o644, 0, 0).unwrap();
    assert_eq!(fs.lookup(5, "a:b?c").unwrap(), p);
    let long: String = "é".repeat(255);
    fs.create(5, &long, 0o644, 0, 0).unwrap();
    assert_eq!(fs.create(5, &"x".repeat(256), 0o644, 0, 0), Err(FsError::NameTooLong));
    // moving a directory into itself
    let d = fs.mkdir(5, "d", 0o755, 0, 0).unwrap();
    fs.mkdir(d, "e", 0o755, 0, 0).unwrap();
    let e = fs.lookup(d, "e").unwrap();
    assert!(fs.rename(5, "d", e, "d").is_err());
    assert_eq!(fs.lookup(e, "..").unwrap(), d);
}

#[test]
fn ntfs_big_directory_tree_ops() {
    // Exercise multi-level B+trees (splits, merges, root collapse) with random-ish order.
    let dev = fresh(64 << 20);
    let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
    let d = fs.mkdir(5, "big", 0o755, 0, 0).unwrap();
    let n = 3000;
    let name = |i: u32| format!("entry {:05} {}", (i * 7919) % 10007, "z".repeat((i % 40) as usize));
    for i in 0..n {
        fs.create(d, &name(i), 0o644, 0, 0).unwrap();
    }
    let mut want: Vec<String> = (0..n).map(name).collect();
    want.sort_by(|a, b| fs.cmp_full(&to_utf16(a), &to_utf16(b)));
    let got: Vec<String> = fs.readdir(d).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(got, want);
    for i in (0..n).filter(|i| i % 3 != 0) {
        fs.unlink(d, &name(i)).unwrap();
    }
    for i in (0..n).filter(|i| i % 3 == 0) {
        assert!(fs.lookup(d, &name(i)).is_ok(), "lost {}", name(i));
    }
    for i in (0..n).filter(|i| i % 3 == 0) {
        fs.unlink(d, &name(i)).unwrap();
    }
    assert!(fs.readdir(d).unwrap().is_empty());
    let di = fs.load_inode(d).unwrap();
    assert!(di.find(AT_INDEX_ALLOCATION, I30).is_none(), "empty index should collapse to the root");
    fs.rmdir(5, "big").unwrap();
    drop(fs);
    let mut fs = NtfsFs::open(dev, false, defaults()).unwrap();
    assert!(fs.readdir(5).unwrap().is_empty());
}

#[test]
fn ntfs_sparse_truncate_and_free_space() {
    let dev = fresh(32 << 20);
    let mut fs = NtfsFs::open(dev, false, defaults()).unwrap();
    fs.create(5, "warmup", 0o644, 0, 0).unwrap(); // first create grows the MFT
    let free0 = fs.statfs().unwrap().free_bytes;
    let f = fs.create(5, "f.bin", 0o644, 0, 0).unwrap();
    fs.setattr(f, &SetAttr { size: Some(5 << 20), ..Default::default() }).unwrap();
    assert_eq!(fs.read(f, 3 << 20, 16).unwrap(), vec![0u8; 16]);
    fs.write(f, 4 << 20, b"tail").unwrap();
    assert_eq!(fs.read(f, (4 << 20) - 2, 8).unwrap(), b"\0\0tail\0\0");
    let a = fs.getattr(f).unwrap();
    assert_eq!(a.size, 5 << 20);
    fs.setattr(f, &SetAttr { size: Some(100), ..Default::default() }).unwrap();
    assert_eq!(fs.getattr(f).unwrap().size, 100);
    fs.unlink(5, "f.bin").unwrap();
    let free1 = fs.statfs().unwrap().free_bytes;
    assert_eq!(free0, free1, "clusters leaked");
}

#[test]
fn ntfs_mft_growth_and_reuse() {
    let dev = fresh(64 << 20);
    let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
    let mut inos = vec![];
    for i in 0..300 {
        inos.push(fs.create(5, &format!("f{i}"), 0o644, 0, 0).unwrap());
    }
    assert!(fs.mft_records() >= 364);
    for i in 0..300 {
        fs.unlink(5, &format!("f{i}")).unwrap();
    }
    let again = fs.create(5, "again", 0o644, 0, 0).unwrap();
    assert!(inos.contains(&again), "freed MFT records should be reused");
    // sequence number was bumped
    let r = fs.read_record(again).unwrap();
    assert!(r.seq >= 2);
    drop(fs);
    let mut fs = NtfsFs::open(dev, false, defaults()).unwrap();
    assert_eq!(fs.lookup(5, "again").unwrap(), again);
}

#[test]
fn ntfs_attribute_list_relayout() {
    // Many hard-link-like names are not reachable through the VFS API, so force a record
    // overflow with a very fragmented file instead.
    let dev = fresh(64 << 20);
    let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
    let f = fs.create(5, "frag.bin", 0o644, 0, 0).unwrap();
    let g = fs.create(5, "spacer.bin", 0o644, 0, 0).unwrap();
    let chunk = vec![0xABu8; 4096];
    for i in 0..200u64 {
        fs.write(f, i * 4096, &chunk).unwrap();
        fs.write(g, i * 4096, &chunk).unwrap(); // interleave allocations -> fragmentation
    }
    let ino = fs.load_inode(f).unwrap();
    assert!(ino.has_al, "expected the $DATA runlist to move to an extension record");
    let want: Vec<u8> = (0..200 * 4096).map(|_| 0xAB).collect();
    assert_eq!(fs.read(f, 0, want.len() as u32).unwrap(), want);
    drop(fs);
    let mut fs = NtfsFs::open(dev, false, defaults()).unwrap();
    assert_eq!(fs.read(f, 0, want.len() as u32).unwrap(), want);
    fs.unlink(5, "frag.bin").unwrap();
    fs.unlink(5, "spacer.bin").unwrap();
}

/// `$Extend/$Reparse:$R` entries as (tag, record number).
fn r_entries(fs: &mut NtfsFs) -> Vec<(u32, u64)> {
    let no = fs.reparse_index().unwrap().expect("volume has $Extend/$Reparse");
    fs.view_list(no, R_INDEX, Coll::Ulongs)
        .unwrap()
        .into_iter()
        .map(|e| {
            assert_eq!(e.key.len(), 12);
            assert_eq!(e.data, R_ENTRY_DATA);
            (le32(&e.key, 0).unwrap(), le64(&e.key, 4).unwrap())
        })
        .map(|(t, r)| (t, mref_no(r)))
        .collect()
}

/// Every reparse point reachable from `dir` must be in $R with its current sequence number,
/// and $R must not contain anything else (checked by the caller via the count).
/// `strict`: also require REPARSE_POINT + the tag in the $FILE_NAME duplicate info and the
/// index entries. ntfs-3g does not maintain them (tag 0 after a rename, flag missing after
/// setting reparse data through its xattr), so only volumes written by us alone are strict.
fn reparse_points(fs: &mut NtfsFs, dir: Ino, out: &mut Vec<(u32, u64)>, strict: bool) {
    for e in fs.readdir(dir).unwrap() {
        let i = fs.load_inode(e.ino).unwrap();
        if let Some(tag) = fs.reparse_tag_of(&i).unwrap() {
            out.push((tag, e.ino));
            let si = i.std_info();
            assert!(si.attrs & FILE_ATTR_REPARSE != 0, "{}: SI lacks REPARSE_POINT", e.name);
            for (_, f) in i.file_names() {
                assert!(!strict || f.flags & FILE_ATTR_REPARSE != 0 && f.reparse == tag, "{}: $FILE_NAME dup info flags {:#x} reparse {:#x}", e.name, f.flags, f.reparse);
            }
            let ie = fs.dir_find(dir, &to_utf16(&e.name), true).unwrap().unwrap();
            let f = FileName::parse(&ie.key).unwrap();
            assert!(!strict || f.flags & FILE_ATTR_REPARSE != 0 && f.reparse == tag, "{}: index entry dup info flags {:#x} reparse {:#x}", e.name, f.flags, f.reparse);
        }
        if e.kind == FileKind::Dir {
            reparse_points(fs, e.ino, out, strict);
        }
    }
}

fn check_r_consistent(fs: &mut NtfsFs) {
    check_r(fs, true)
}

fn check_r(fs: &mut NtfsFs, strict: bool) {
    let mut want = vec![];
    reparse_points(fs, 5, &mut want, strict);
    let mut got = r_entries(fs);
    want.sort();
    got.sort();
    assert_eq!(got, want, "$R index out of sync with the reparse points on the volume");
}

/// Volume flags and $LogFile must look clean to ntfs-3g / Windows.
fn assert_clean(fs: &mut NtfsFs) {
    assert_eq!(fs.volume_flags().unwrap() & 0x0001, 0, "VOLUME_IS_DIRTY set");
    assert_eq!(fs.logfile_state().unwrap(), None);
    assert_eq!(fs.dirty_state(), DirtyState::Clean);
}

#[test]
fn ntfs_symlinks_own_format() {
    let dev = fresh(64 << 20);
    let (rel, abs, dl, deep, bs);
    {
        let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
        let t = fs.create(5, "target.txt", 0o644, 0, 0).unwrap();
        fs.write(t, 0, b"target content").unwrap();
        let d = fs.mkdir(5, "dir", 0o755, 0, 0).unwrap();
        fs.create(d, "inner.txt", 0o644, 0, 0).unwrap();
        rel = fs.symlink(5, "rel", "target.txt", 0, 0).unwrap();
        abs = fs.symlink(5, "abs", "/Volumes/Other Disk/ü file", 0, 0).unwrap();
        dl = fs.symlink(5, "dirlink", "dir", 0, 0).unwrap();
        deep = fs.symlink(d, "up", "../target.txt", 0, 0).unwrap();
        bs = fs.symlink(5, "backslash", "a\\b/c", 0, 0).unwrap();
        assert_eq!(fs.symlink(5, "rel", "x", 0, 0), Err(FsError::Exists));
        assert_eq!(fs.symlink(5, "REL", "x", 0, 0), Err(FsError::Exists));
        assert!(matches!(fs.symlink(5, "empty", "", 0, 0), Err(FsError::Invalid(_))));
        assert_eq!(fs.symlink(5, "long", &"x".repeat(5000), 0, 0), Err(FsError::NameTooLong));
        // formats
        let tag = |fs: &mut NtfsFs, i| fs.reparse_tag_of(&fs.load_inode(i).unwrap()).unwrap();
        assert_eq!(tag(&mut fs, rel), Some(IO_REPARSE_TAG_SYMLINK));
        assert_eq!(tag(&mut fs, deep), Some(IO_REPARSE_TAG_SYMLINK));
        assert_eq!(tag(&mut fs, abs), Some(IO_REPARSE_TAG_SYMLINK), "absolute: \\??\\C:\\...");
        assert_eq!(tag(&mut fs, bs), Some(IO_REPARSE_TAG_LX_SYMLINK), "not a Windows path");
        let colon = fs.symlink(5, "colon", "a:b", 0, 0).unwrap();
        assert_eq!(tag(&mut fs, colon), Some(IO_REPARSE_TAG_LX_SYMLINK), "':' would name a stream on Windows");
        // an absolute link is a file record even if the same path exists on this volume
        let absdir = fs.symlink(5, "absdir", "/dir", 0, 0).unwrap();
        assert_eq!(tag(&mut fs, absdir), Some(IO_REPARSE_TAG_SYMLINK));
        assert!(!fs.load_inode(absdir).unwrap().is_dir());
        assert_eq!(fs.readlink(absdir).unwrap(), "/dir");
        assert_eq!(fs.readlink(colon).unwrap(), "a:b");
        fs.unlink(5, "colon").unwrap();
        fs.unlink(5, "absdir").unwrap();
        fs.sync().unwrap(); // pending-dir-link fixups must ignore absolute links
        assert!(!fs.load_inode(abs).unwrap().is_dir());
        assert_eq!(tag(&mut fs, t), None);
        // a link to an existing directory is a directory record (like `mklink /D`)
        assert!(fs.load_inode(dl).unwrap().is_dir());
        assert!(!fs.load_inode(rel).unwrap().is_dir());
        assert!(!fs.load_inode(deep).unwrap().is_dir());
        let up_dir = fs.symlink(d, "updir", "..", 0, 0).unwrap();
        assert!(fs.load_inode(up_dir).unwrap().is_dir());
        let via = fs.symlink(5, "via", "DIR/./../dir/", 0, 0).unwrap();
        assert!(fs.load_inode(via).unwrap().is_dir(), "case-insensitive, ./.. and trailing slash");
        let through = fs.symlink(5, "through", "dirlink", 0, 0).unwrap();
        assert!(!fs.load_inode(through).unwrap().is_dir(), "stops at another link");
        let dangling = fs.symlink(5, "dangling", "nope/dir", 0, 0).unwrap();
        assert!(!fs.load_inode(dangling).unwrap().is_dir());
        for n in ["via", "through", "dangling"] {
            fs.unlink(5, n).unwrap();
        }
        fs.unlink(d, "updir").unwrap();
        // a long (non-resident) target still works
        let long = format!("{}/end", "segment/".repeat(400));
        let lg = fs.symlink(5, "long", &long, 0, 0).unwrap();
        assert_eq!(fs.readlink(lg).unwrap(), long);
        check_r_consistent(&mut fs);
        assert_eq!(r_entries(&mut fs).len(), 6);
        fs.unlink(5, "long").unwrap();
        assert_eq!(r_entries(&mut fs).len(), 5);
    }
    let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
    assert_clean(&mut fs);
    for (i, want) in [(rel, "target.txt"), (abs, "/Volumes/Other Disk/ü file"), (dl, "dir"), (deep, "../target.txt"), (bs, "a\\b/c")] {
        assert_eq!(fs.readlink(i).unwrap(), want);
        let a = fs.getattr(i).unwrap();
        assert_eq!((a.kind, a.size, a.perm), (FileKind::Symlink, want.len() as u64, 0o777));
        assert_eq!(fs.read(i, 0, 4096).unwrap(), want.as_bytes());
        assert!(matches!(fs.write(i, 0, b"x"), Err(FsError::Invalid(_))));
        assert!(matches!(fs.setattr(i, &SetAttr { size: Some(0), ..Default::default() }), Err(FsError::Invalid(_))));
    }
    assert_eq!(fs.read(rel, 3, 3).unwrap(), b"get");
    assert!(matches!(fs.readlink(5), Err(FsError::Invalid(_))));
    let t = fs.lookup(5, "target.txt").unwrap();
    assert!(matches!(fs.readlink(t), Err(FsError::Invalid(_))));
    // times can still be set on a symlink
    let tm = Timestamp { secs: 1_500_000_000, nsecs: 0 };
    assert_eq!(fs.setattr(rel, &SetAttr { mtime: Some(tm), ..Default::default() }).unwrap().mtime, tm);
    let kinds: std::collections::HashMap<String, FileKind> = fs.readdir(5).unwrap().into_iter().map(|e| (e.name, e.kind)).collect();
    assert_eq!(kinds["rel"], FileKind::Symlink);
    assert_eq!(kinds["abs"], FileKind::Symlink);
    assert_eq!(kinds["dirlink"], FileKind::Symlink);
    assert_eq!(kinds["dir"], FileKind::Dir);
    assert_eq!(kinds["target.txt"], FileKind::File);
    assert_eq!(fs.rmdir(5, "dirlink"), Err(FsError::NotDir));
    // rename: same dir, across dirs, onto an existing file, and a file onto a symlink
    fs.rename(5, "rel", 5, "rel2").unwrap();
    assert_eq!(fs.lookup(5, "rel2").unwrap(), rel);
    let d = fs.lookup(5, "dir").unwrap();
    fs.rename(5, "rel2", d, "moved").unwrap();
    let mv = fs.lookup(d, "moved").unwrap();
    assert_eq!(fs.readlink(mv).unwrap(), "target.txt");
    fs.rename(d, "moved", d, "inner.txt").unwrap();
    assert_eq!(fs.lookup(d, "inner.txt").unwrap(), rel);
    let victim = fs.create(5, "victim", 0o644, 0, 0).unwrap();
    fs.write(victim, 0, b"v").unwrap();
    fs.rename(5, "victim", 5, "backslash").unwrap(); // replaces the LX symlink
    let b2 = fs.lookup(5, "backslash").unwrap();
    assert_eq!(fs.getattr(b2).unwrap().kind, FileKind::File);
    assert_eq!(fs.rename(5, "dir", 5, "abs"), Err(FsError::NotDir));
    check_r_consistent(&mut fs);
    assert_eq!(r_entries(&mut fs).len(), 4);
    fs.sync().unwrap();
    assert_clean(&mut fs);
    // unlink frees the records and the $R entries
    let used_before = fs.statfs().unwrap().free_inodes;
    fs.unlink(d, "inner.txt").unwrap();
    fs.unlink(5, "abs").unwrap();
    fs.unlink(5, "dirlink").unwrap();
    fs.unlink(d, "up").unwrap();
    assert_eq!(r_entries(&mut fs), vec![]);
    assert_eq!(fs.statfs().unwrap().free_inodes, used_before + 4);
    assert_eq!(fs.getattr(abs), Err(FsError::Stale));
    fs.rmdir(5, "dir").unwrap();
    drop(fs);
    let mut fs = NtfsFs::open(dev, false, defaults()).unwrap();
    assert_clean(&mut fs);
    let mut n: Vec<String> = fs.readdir(5).unwrap().into_iter().map(|e| e.name).collect();
    n.sort();
    assert_eq!(n, vec!["backslash", "target.txt"]);
}

#[test]
fn ntfs_symlinks_many_grow_reparse_index() {
    // Enough links to push $R out of its $INDEX_ROOT into INDX blocks, then remove them all.
    let dev = fresh(64 << 20);
    let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
    let d = fs.mkdir(5, "links", 0o755, 0, 0).unwrap();
    for i in 0..700 {
        let t = if i % 3 == 0 { format!("/abs/target/{i}") } else { format!("../rel/target {i}") };
        fs.symlink(d, &format!("link {i:04}"), &t, 0, 0).unwrap();
    }
    let no = fs.reparse_index().unwrap().unwrap();
    assert!(fs.load_inode(no).unwrap().find(AT_INDEX_ALLOCATION, R_INDEX).is_some(), "$R should have grown into INDX blocks");
    check_r_consistent(&mut fs);
    assert_eq!(r_entries(&mut fs).len(), 700);
    drop(fs);
    let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
    let mut got = r_entries(&mut fs);
    let sorted = {
        let mut v = got.clone();
        v.sort();
        v
    };
    assert_eq!(got, sorted, "$R not in collation order");
    for i in (0..700).step_by(2) {
        fs.unlink(d, &format!("link {i:04}")).unwrap();
    }
    check_r_consistent(&mut fs);
    for i in (1..700).step_by(2) {
        let l = fs.lookup(d, &format!("link {i:04}")).unwrap();
        let want = if i % 3 == 0 { format!("/abs/target/{i}") } else { format!("../rel/target {i}") };
        assert_eq!(fs.readlink(l).unwrap(), want);
        fs.unlink(d, &format!("link {i:04}")).unwrap();
    }
    got = r_entries(&mut fs);
    assert!(got.is_empty());
    assert!(fs.load_inode(no).unwrap().find(AT_INDEX_ALLOCATION, R_INDEX).is_none(), "$R should shrink back");
    fs.rmdir(5, "links").unwrap();
    assert_clean(&mut fs);
}

/// Turn an existing node into a reparse point with an arbitrary buffer (simulates Windows).
fn set_reparse(fs: &mut NtfsFs, ino: Ino, value: Vec<u8>) {
    let mut i = fs.load_inode(ino).unwrap();
    i.add(Attribute::resident(AT_REPARSE_POINT, &[], value.clone()));
    let mut si = i.std_info();
    si.attrs |= FILE_ATTR_REPARSE;
    i.set_std_info(&si);
    fs.store_inode(&mut i).unwrap();
    fs.r_insert(tag_of(&value).unwrap(), i.mref()).unwrap();
    fs.sync_dup(ino).unwrap();
}

fn ms_reparse(tag: u32, subst: &str, print: &str, flags: Option<u32>) -> Vec<u8> {
    let s = utf16_bytes(&to_utf16(subst));
    let p = utf16_bytes(&to_utf16(print));
    let mut d = vec![0u8; if flags.is_some() { 12 } else { 8 }];
    put16(&mut d, 0, 0);
    put16(&mut d, 2, s.len() as u16);
    put16(&mut d, 4, s.len() as u16);
    put16(&mut d, 6, p.len() as u16);
    if let Some(f) = flags {
        put32(&mut d, 8, f);
    }
    d.extend_from_slice(&s);
    d.extend_from_slice(&p);
    let mut v = vec![0u8; 8];
    put32(&mut v, 0, tag);
    put16(&mut v, 4, d.len() as u16);
    v.extend_from_slice(&d);
    v
}

#[test]
fn ntfs_foreign_reparse_points() {
    let dev = fresh(32 << 20);
    let mut fs = NtfsFs::open(dev, false, defaults()).unwrap();
    // Windows absolute file symlink
    let f = fs.create(5, "winabs", 0o644, 0, 0).unwrap();
    set_reparse(&mut fs, f, ms_reparse(IO_REPARSE_TAG_SYMLINK, "\\??\\C:\\Users\\x\\f.txt", "C:\\Users\\x\\f.txt", Some(0)));
    assert_eq!(fs.readlink(f).unwrap(), "/Users/x/f.txt");
    assert_eq!(fs.getattr(f).unwrap().size, "/Users/x/f.txt".len() as u64);
    // junction (directory record)
    let j = fs.mkdir(5, "junction", 0o755, 0, 0).unwrap();
    set_reparse(&mut fs, j, ms_reparse(IO_REPARSE_TAG_MOUNT_POINT, "\\??\\C:\\Program Files", "C:\\Program Files", None));
    assert_eq!(fs.getattr(j).unwrap().kind, FileKind::Symlink);
    assert_eq!(fs.readlink(j).unwrap(), "/Program Files");
    // Windows relative directory symlink (directory record)
    let wd = fs.mkdir(5, "winreldir", 0o755, 0, 0).unwrap();
    set_reparse(&mut fs, wd, ms_reparse(IO_REPARSE_TAG_SYMLINK, "..\\sub dir", "..\\sub dir", Some(1)));
    assert_eq!(fs.readlink(wd).unwrap(), "../sub dir");
    // dedup / OneDrive placeholders stay regular files and dirs
    let dd = fs.create(5, "dedup.bin", 0o644, 0, 0).unwrap();
    fs.write(dd, 0, b"payload").unwrap();
    let mut v = vec![0u8; 8];
    put32(&mut v, 0, 0x8000_0013);
    put16(&mut v, 4, 16);
    v.extend_from_slice(&[7u8; 16]);
    set_reparse(&mut fs, dd, v);
    let od = fs.mkdir(5, "onedrive", 0o755, 0, 0).unwrap();
    let mut v = vec![0u8; 8];
    put32(&mut v, 0, 0x9000_601A);
    put16(&mut v, 4, 8);
    v.extend_from_slice(&[1u8; 8]);
    set_reparse(&mut fs, od, v);
    let a = fs.getattr(dd).unwrap();
    assert_eq!((a.kind, a.size), (FileKind::File, 7));
    assert_eq!(fs.read(dd, 0, 100).unwrap(), b"payload");
    assert!(matches!(fs.readlink(dd), Err(FsError::Invalid(_))));
    assert_eq!(fs.getattr(od).unwrap().kind, FileKind::Dir);
    fs.create(od, "inside", 0o644, 0, 0).unwrap();
    let kinds: std::collections::HashMap<String, FileKind> = fs.readdir(5).unwrap().into_iter().map(|e| (e.name, e.kind)).collect();
    assert_eq!(kinds["winabs"], FileKind::Symlink);
    assert_eq!(kinds["junction"], FileKind::Symlink);
    assert_eq!(kinds["winreldir"], FileKind::Symlink);
    assert_eq!(kinds["dedup.bin"], FileKind::File);
    assert_eq!(kinds["onedrive"], FileKind::Dir);
    check_r_consistent(&mut fs);
    // directory-record links are removed with unlink (not rmdir), like any symlink
    assert_eq!(fs.rmdir(5, "junction"), Err(FsError::NotDir));
    fs.unlink(5, "junction").unwrap();
    fs.unlink(5, "winreldir").unwrap();
    fs.unlink(5, "winabs").unwrap();
    fs.unlink(5, "dedup.bin").unwrap();
    assert_eq!(fs.unlink(5, "onedrive"), Err(FsError::IsDir));
    fs.unlink(od, "inside").unwrap();
    fs.rmdir(5, "onedrive").unwrap();
    assert!(r_entries(&mut fs).is_empty());
    // Interix symlink, as ntfs-3g's `ln -s` writes it
    let ix = fs.create(5, "interix", 0o644, 0, 0).unwrap();
    let mut data = INTERIX_MAGIC.to_vec();
    data.extend_from_slice(&utf16_bytes(&to_utf16("some/where ü")));
    fs.write(ix, 0, &data).unwrap();
    assert_eq!(fs.getattr(ix).unwrap().kind, FileKind::File, "not SYSTEM yet");
    let mut i = fs.load_inode(ix).unwrap();
    let mut si = i.std_info();
    si.attrs |= FILE_ATTR_SYSTEM;
    i.set_std_info(&si);
    fs.store_inode(&mut i).unwrap();
    fs.sync_dup(ix).unwrap();
    assert_eq!(fs.readlink(ix).unwrap(), "some/where ü");
    assert_eq!(fs.getattr(ix).unwrap().size, "some/where ü".len() as u64);
    assert_eq!(fs.readdir(5).unwrap().iter().find(|e| e.name == "interix").unwrap().kind, FileKind::Symlink);
    fs.unlink(5, "interix").unwrap();
}

fn set_volume_flags(fs: &mut NtfsFs, flags: u16) {
    let mut v = fs.load_inode(3).unwrap();
    let l = v.find(AT_VOLUME_INFORMATION, &[]).unwrap();
    if let Body::Res { value, .. } = &mut v.attr_mut(l).body {
        put16(value, 10, flags);
    }
    fs.store_inode(&mut v).unwrap();
}

#[test]
fn ntfs_dirty_doctor() {
    let dev = fresh(32 << 20);
    let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
    assert_eq!(fs.dirty_state(), DirtyState::Clean);
    // 1. dirty flag
    set_volume_flags(&mut fs, 0x0001);
    assert!(matches!(fs.dirty_state(), DirtyState::Dirty(r) if r.contains("dirty flag")));
    let rep = fs.repair().unwrap();
    assert!(rep.contains("dirty flag"), "{rep}");
    assert_eq!(fs.dirty_state(), DirtyState::Clean);
    assert_eq!(fs.volume_flags().unwrap() & 1, 0);
    // 2. unclean $LogFile (restart page with a client in use and no CLEAN flag)
    let lf = fs.load_inode(2).unwrap();
    let Some(Stream::NonRes { runs, .. }) = fs.stream(&lf, AT_DATA, &[]).unwrap() else { panic!() };
    let mut page = vec![0u8; 4096];
    page[0..4].copy_from_slice(b"RSTR");
    put16(&mut page, 4, 0x1E);
    put16(&mut page, 6, 9);
    put32(&mut page, 0x10, 4096);
    put32(&mut page, 0x14, 4096);
    put16(&mut page, 0x18, 0x30);
    put16(&mut page, 0x1C, 1);
    put16(&mut page, 0x1E, 1);
    put64(&mut page, 0x30, 0x1234); // current lsn
    put16(&mut page, 0x30 + 0x0C, 0); // client in use
    put16(&mut page, 0x30 + 0x0E, 0); // not clean
    fixup_write(&mut page, 1);
    fs.write_runs(&runs, 0, &page).unwrap();
    fs.write_runs(&runs, 4096, &page).unwrap();
    assert!(matches!(fs.dirty_state(), DirtyState::Dirty(r) if r.contains("LogFile")));
    // a clean restart area is accepted
    let mut clean = page.clone();
    fixup_read(&mut clean).unwrap();
    put16(&mut clean, 0x30 + 0x0E, 2);
    fixup_write(&mut clean, 2);
    fs.write_runs(&runs, 0, &clean).unwrap();
    fs.write_runs(&runs, 4096, &clean).unwrap();
    assert_eq!(fs.dirty_state(), DirtyState::Clean);
    fs.write_runs(&runs, 0, &page).unwrap();
    fs.write_runs(&runs, 4096, &page).unwrap();
    let rep = fs.repair().unwrap();
    assert!(rep.contains("LogFile"), "{rep}");
    assert_eq!(fs.dirty_state(), DirtyState::Clean);
    assert!(fs.read_stream(&fs.stream(&lf, AT_DATA, &[]).unwrap().unwrap(), 0, 8192).unwrap().iter().all(|&b| b == 0xFF));
    // 3. hibernated Windows
    let h = fs.create(5, "hiberfil.sys", 0o644, 0, 0).unwrap();
    let mut hib = vec![0u8; 8192];
    hib[..4].copy_from_slice(b"HIBR");
    fs.write(h, 0, &hib).unwrap();
    assert!(matches!(fs.dirty_state(), DirtyState::Dirty(r) if r.contains("hibernated")));
    let rep = fs.repair().unwrap();
    assert!(rep.contains("hibernated"), "{rep}");
    fs.write(h, 0, &[0u8; 4]).unwrap();
    assert_eq!(fs.dirty_state(), DirtyState::Clean);
    // 4. persists across reopen
    set_volume_flags(&mut fs, 0x0001);
    drop(fs);
    let mut fs = NtfsFs::open(dev, false, defaults()).unwrap();
    assert!(matches!(fs.dirty_state(), DirtyState::Dirty(_)));
    fs.repair().unwrap();
    assert_eq!(fs.dirty_state(), DirtyState::Clean);
}

#[test]
fn ntfs_corrupt_input_never_panics() {
    let dev = fresh(16 << 20);
    {
        let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
        let d = fs.mkdir(5, "d", 0o755, 0, 0).unwrap();
        for i in 0..80 {
            let f = fs.create(d, &format!("some file name {i}"), 0o644, 0, 0).unwrap();
            fs.write(f, 0, &pattern(5000, i)).unwrap();
        }
    }
    let img = dev.snapshot();
    let mut seed = 0x1234_5678u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for _round in 0..150 {
        let mem = Arc::new(MemDev::new(img.len()));
        mem.write_at(0, &img).unwrap();
        // corrupt the first 4 MiB (boot, MFT, root index, bitmaps...)
        for _ in 0..40 {
            let off = (rnd() as usize) % (4 << 20);
            let v = [rnd() as u8];
            mem.write_at(off as u64, &v).unwrap();
        }
        if let Ok(mut fs) = NtfsFs::open(mem.clone(), false, defaults()) {
            let _ = fs.dirty_state();
            if let Ok(ents) = fs.readdir(5) {
                for e in ents {
                    let _ = fs.getattr(e.ino);
                    if let Ok(sub) = fs.readdir(e.ino) {
                        for s in sub.iter().take(20) {
                            let _ = fs.read(s.ino, 0, 10000);
                        }
                        if let Some(s) = sub.first() {
                            let _ = fs.unlink(e.ino, &s.name);
                        }
                    }
                }
            }
            let _ = fs.create(5, "new", 0o644, 0, 0);
            let _ = fs.statfs();
        }
    }
}

#[test]
fn ntfs_lznt1() {
    // "abcabcabcabc..." compressed by hand: literal "abc" + back-reference (disp 3, len 9)
    let mut chunk = vec![0b0000_1000u8, b'a', b'b', b'c'];
    // pos=3 → length bits = 12 - 0 (pos-1 = 2 < 16): disp-1 = 2 → 2<<12, len-3 = 6
    let tok: u16 = (2 << 12) | 6;
    chunk.extend_from_slice(&tok.to_le_bytes());
    let hdr: u16 = 0x8000 | 0x3000 | (chunk.len() as u16 - 1);
    let mut src = hdr.to_le_bytes().to_vec();
    src.extend_from_slice(&chunk);
    src.extend_from_slice(&[0, 0]);
    let mut dst = vec![0u8; 16];
    lznt1_decompress(&src, &mut dst).unwrap();
    assert_eq!(&dst[..12], b"abcabcabcabc");
}

/// BlockDev wrapper that records every write as `(offset, len, through write_data_at)`.
struct CountingDev {
    inner: Arc<MemDev>,
    log: std::sync::Mutex<Vec<(u64, u64, bool)>>,
}

impl BlockDev for CountingDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        self.log.lock().unwrap().push((offset, buf.len() as u64, false));
        self.inner.write_at(offset, buf)
    }
    fn write_data_at(&self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        self.log.lock().unwrap().push((offset, buf.len() as u64, true));
        self.inner.write_at(offset, buf)
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn read_only(&self) -> bool {
        false
    }
}

/// Physical byte extents `[a, b)` of the allocated runs of a run list.
fn phys_extents(runs: &[Run], cs: u64) -> Vec<(u64, u64)> {
    runs.iter().filter_map(|r| r.lcn.map(|l| (l * cs, (l + r.len) * cs))).collect()
}

fn overlaps(a: u64, len: u64, ext: &[(u64, u64)]) -> bool {
    ext.iter().any(|&(s, e)| a < e && s < a + len)
}

/// Ordered-mode hint: file payload (and the zero-fill of its new clusters) goes through
/// `write_data_at`; MFT records, $MFTMirr, $Bitmap, $MFT:$BITMAP, index blocks and reparse
/// data go through `write_at`.
#[test]
fn ntfs_write_data_at_only_for_file_payload() {
    let dev = Arc::new(CountingDev { inner: fresh(64 << 20), log: Default::default() });
    let mut fs = NtfsFs::open(dev.clone(), false, defaults()).unwrap();
    let cs = fs.cluster;
    fs.create(5, "warmup", 0o644, 0, 0).unwrap(); // first create grows the MFT
    dev.log.lock().unwrap().clear();

    // 4 MB file in 256 KiB writes (the first one converts resident -> non-resident), then
    // extend it and write past the initialized size (zero-fill of the gap).
    let big = fs.create(5, "big.bin", 0o644, 0, 0).unwrap();
    let payload = pattern(4 << 20, 11);
    for (i, c) in payload.chunks(256 << 10).enumerate() {
        fs.write(big, (i as u64) << 18, c).unwrap();
    }
    fs.setattr(big, &SetAttr { size: Some(6 << 20), ..Default::default() }).unwrap();
    fs.write(big, (5 << 20) + 100, b"tail").unwrap();
    // A sparse file: writing into a hole allocates and zero-fills clusters (fill_holes).
    let sp = fs.create(5, "sparse.bin", 0o644, 0, 0).unwrap();
    fs.write(sp, 0, &vec![7u8; 8192]).unwrap();
    {
        let mut i = fs.load_inode(sp).unwrap();
        let l = i.find(AT_DATA, &[]).unwrap();
        let a = i.attr_mut(l);
        a.flags |= ATTR_SPARSE;
        if let Body::NonRes(n) = &mut a.body {
            n.comp_size = n.alloc;
        }
        i.recs[0].dirty = true;
        fs.store_inode(&mut i).unwrap();
    }
    fs.setattr(sp, &SetAttr { size: Some(1 << 20), ..Default::default() }).unwrap();
    fs.write(sp, 512 << 10, b"in the hole").unwrap();
    // metadata-heavy operations: a directory big enough for INDX blocks, a non-resident
    // reparse point, a resident file
    let d = fs.mkdir(5, "dir", 0o755, 0, 0).unwrap();
    for i in 0..150 {
        fs.create(d, &format!("a fairly long file name number {i:04}"), 0o644, 0, 0).unwrap();
    }
    fs.symlink(5, "long link", &format!("{}/end", "segment/".repeat(400)), 0, 0).unwrap();
    let small = fs.create(5, "small.txt", 0o644, 0, 0).unwrap();
    fs.write(small, 0, b"resident").unwrap();
    fs.sync().unwrap();

    let log = dev.log.lock().unwrap().clone();
    let data_runs = |fs: &NtfsFs, ino: Ino| {
        let i = fs.load_inode(ino).unwrap();
        let s = fs.stream(&i, AT_DATA, &[]).unwrap().unwrap();
        let Stream::NonRes { runs, .. } = s else { panic!("expected non-resident data") };
        phys_extents(&runs, cs)
    };
    let big_ext = data_runs(&fs, big);
    let sp_ext = data_runs(&fs, sp);
    let file_ext: Vec<(u64, u64)> = big_ext.iter().chain(&sp_ext).copied().collect();
    assert_eq!(big_ext.iter().map(|(a, b)| b - a).sum::<u64>(), 6 << 20);

    // 1. every write_data_at lands inside regular-file data clusters
    let data_writes: Vec<_> = log.iter().filter(|w| w.2).collect();
    assert!(!data_writes.is_empty());
    for &&(off, len, _) in &data_writes {
        assert!(file_ext.iter().any(|&(s, e)| off >= s && off + len <= e), "write_data_at({off:#x}, {len}) outside file payload");
    }
    // 2. ... and covers all of it (payload, gap zero-fill, hole zero-fill) up to the
    //    initialized size (clusters past it are allocated but never need to be written)
    let mut must = sp_ext.clone();
    {
        let i = fs.load_inode(big).unwrap();
        let Some(Stream::NonRes { runs, init, .. }) = fs.stream(&i, AT_DATA, &[]).unwrap() else { panic!() };
        assert_eq!(init, (5 << 20) + 104);
        fs.map_runs(&runs, 0, init, |_, p, n| {
            must.push((p.unwrap(), p.unwrap() + n));
            Ok(())
        })
        .unwrap();
    }
    for &(s, e) in &must {
        let mut covered = vec![false; (e - s).div_ceil(512) as usize];
        for &&(off, len, _) in &data_writes {
            let (a, b) = (off.max(s), (off + len).min(e));
            for k in (a..b).step_by(512) {
                covered[((k - s) / 512) as usize] = true;
            }
        }
        assert!(covered.iter().all(|&c| c), "payload extent {s:#x}..{e:#x} not fully written via write_data_at");
    }
    let payload_bytes: u64 = data_writes.iter().map(|w| w.1).sum();
    // big.bin: 4 MiB payload + zero gap up to 5 MiB+100 + "tail"; sparse.bin: 8 KiB (resident ->
    // non-resident), one zero-filled hole cluster, then the 11-byte write into it
    assert_eq!(payload_bytes, (5 << 20) + 104 + 8192 + cs + 11);
    // 3. no metadata write touches file payload, and metadata structures got write_at only
    for &(off, len, data) in &log {
        if !data {
            assert!(!overlaps(off, len, &file_ext), "write_at({off:#x}, {len}) over file payload");
        }
    }
    let meta_writes = |ext: &[(u64, u64)]| log.iter().filter(|w| overlaps(w.0, w.1, ext)).map(|w| w.2).collect::<Vec<bool>>();
    let rs = fs.rec_size as u64;
    let mut rec_ext = vec![];
    fs.map_runs(&fs.mft_runs.clone(), big * rs, rs, |_, p, n| {
        rec_ext.push((p.unwrap(), p.unwrap() + n));
        Ok(())
    })
    .unwrap();
    let w = meta_writes(&rec_ext);
    assert!(!w.is_empty() && w.iter().all(|d| !d), "big.bin's MFT record: {w:?}");
    let w = meta_writes(&phys_extents(&fs.mft_runs, cs));
    assert!(w.len() > 10 && w.iter().all(|d| !d), "$MFT");
    let w = meta_writes(&phys_extents(&fs.vbm_runs, cs));
    assert!(!w.is_empty() && w.iter().all(|d| !d), "$Bitmap");
    let w = meta_writes(&phys_extents(&fs.mirr_runs, cs));
    assert!(!w.is_empty() && w.iter().all(|d| !d), "$MFTMirr");
    let m = fs.load_inode(0).unwrap();
    if let Some(Stream::NonRes { runs, .. }) = fs.stream(&m, AT_BITMAP, &[]).unwrap() {
        assert!(meta_writes(&phys_extents(&runs, cs)).iter().all(|d| !d), "$MFT:$BITMAP");
    }
    let di = fs.load_inode(d).unwrap();
    let Some(Stream::NonRes { runs, .. }) = fs.stream(&di, AT_INDEX_ALLOCATION, I30).unwrap() else { panic!("no INDX blocks") };
    let w = meta_writes(&phys_extents(&runs, cs));
    assert!(!w.is_empty() && w.iter().all(|d| !d), "$INDEX_ALLOCATION");

    // content is intact
    assert_eq!(fs.read(big, 0, 1 << 20).unwrap(), payload[..1 << 20]);
    assert_eq!(fs.read(big, 3 << 20, 1 << 20).unwrap(), payload[3 << 20..]);
    assert_eq!(fs.read(big, 4 << 20, 1 << 20).unwrap(), vec![0u8; 1 << 20]);
    assert_eq!(fs.read(big, (5 << 20) + 98, 8).unwrap(), b"\0\0tail\0\0");
    assert_eq!(fs.read(sp, 8190, 4).unwrap(), b"\x07\x07\0\0");
    assert_eq!(fs.read(sp, (512 << 10) - 1, 12).unwrap(), b"\0in the hole");
    assert_eq!(fs.read(small, 0, 100).unwrap(), b"resident");
}

// ---------------------------------------------------------------------------------------------
// Integration tests against the real ntfs-3g tools (Docker image `lumendisk-toolbox`).

fn it_dir() -> PathBuf {
    let d = std::env::var("LUMENDISK_NTFS_IT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/ntfs-it"));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn docker(dir: &Path, script: &str) -> (bool, String) {
    let out = Command::new("docker")
        .args(["run", "--rm", "-v", &format!("{}:/w", dir.display()), "lumendisk-toolbox", "sh", "-c", script])
        .output()
        .expect("docker not available");
    let s = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

fn docker_privileged(dir: &Path, script: &str) -> (bool, String) {
    let out = Command::new("docker")
        .args(["run", "--rm", "--privileged", "-v", &format!("{}:/w", dir.display()), "lumendisk-toolbox", "sh", "-c", script])
        .output()
        .expect("docker not available");
    let s = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

fn make_image(path: &Path, size: u64) {
    let f = std::fs::File::create(path).unwrap();
    f.set_len(size).unwrap();
}

fn open_img(path: &Path) -> NtfsFs {
    let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(path, false).unwrap());
    NtfsFs::open(dev, false, defaults()).unwrap()
}

/// Validate with the upstream tools the state that `conformance` leaves behind.
fn validate_with_tools(dir: &Path, img: &str) {
    let (ok, out) = docker(dir, &format!("ntfsfix -n /w/{img}"));
    assert!(ok && out.contains("processed successfully"), "ntfsfix -n failed:\n{out}");
    let (ok, out) = docker(dir, &format!("ntfsls -R -a /w/{img} 2>&1 | head -100"));
    assert!(ok, "ntfsls failed:\n{out}");
    assert!(out.contains("keep") && out.contains("persist.bin") && out.contains("hello.txt"), "ntfsls -R:\n{out}");
    let (ok, out) = docker(dir, &format!("ntfscat /w/{img} keep/sub/hello.txt"));
    assert!(ok && out.trim_end() == "hello lumendisk", "ntfscat hello.txt:\n{out}");
    let (ok, out) = docker(dir, &format!("ntfscat /w/{img} keep/persist.bin > /w/{img}.persist && echo done"));
    assert!(ok, "ntfscat persist.bin: {out}");
    let got = std::fs::read(dir.join(format!("{img}.persist"))).unwrap();
    assert!(got == pattern(300 * 1024, 7), "persist.bin content differs when read by ntfscat");
    let (ok, out) = docker(dir, &format!("ntfsinfo -m /w/{img}"));
    assert!(ok, "ntfsinfo failed:\n{out}");
}

/// Try a real kernel/FUSE mount; returns None when mounting is impossible in this Docker.
fn try_mount_check(dir: &Path, img: &str) -> Option<String> {
    let script = format!(
        "set -e; mkdir -p /mnt/t; \
         (mount -t ntfs3 -o loop,ro,iocharset=utf8 /w/{img} /mnt/t 2>/dev/null && echo DRIVER=ntfs3) || \
         (ntfs-3g -o ro /w/{img} /mnt/t && echo DRIVER=ntfs-3g); \
         ls -laR /mnt/t | head -50; cat /mnt/t/keep/sub/hello.txt; echo; \
         cmp /mnt/t/keep/persist.bin /w/{img}.persist && echo CMP_OK; umount /mnt/t"
    );
    let (ok, out) = docker_privileged(dir, &script);
    if !ok {
        eprintln!("mount check not possible / failed:\n{out}");
        return None;
    }
    assert!(out.contains("hello lumendisk") && out.contains("CMP_OK"), "mounted content wrong:\n{out}");
    Some(out)
}

#[test]
#[ignore]
fn ntfs_it_real_mkntfs_image() {
    let dir = it_dir();
    let img = "mkntfs.img";
    let path = dir.join(img);
    make_image(&path, 128 << 20);
    let (ok, out) = docker(&dir, &format!("mkntfs -F -Q -L REAL /w/{img}"));
    assert!(ok, "mkntfs failed:\n{out}");
    {
        let mut fs = open_img(&path);
        assert_eq!(fs.volume_info().label, "REAL");
        assert_eq!(fs.dirty_state(), DirtyState::Clean);
        assert!(fs.readdir(5).unwrap().is_empty());
        conformance(&mut fs, true);
    }
    {
        let mut fs = open_img(&path);
        check_persisted(&mut fs);
    }
    validate_with_tools(&dir, img);
    if let Some(out) = try_mount_check(&dir, img) {
        eprintln!("real mount OK:\n{out}");
    }
}

#[test]
#[ignore]
fn ntfs_it_own_format_accepted_by_tools() {
    let dir = it_dir();
    let img = "ours.img";
    let path = dir.join(img);
    make_image(&path, 96 << 20);
    {
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&path, false).unwrap());
        format(dev, "OURS").unwrap();
    }
    let (ok, out) = docker(&dir, &format!("ntfsfix -n /w/{img} && ntfsinfo -m /w/{img} && ntfsls -a -s /w/{img}"));
    assert!(ok && out.contains("processed successfully") && out.contains("Volume Name: OURS"), "fresh format rejected:\n{out}");
    assert!(out.contains("$MFT") && out.contains("$Extend"), "{out}");
    {
        let mut fs = open_img(&path);
        conformance(&mut fs, true);
    }
    {
        let mut fs = open_img(&path);
        check_persisted(&mut fs);
    }
    validate_with_tools(&dir, img);
    // Let ntfs-3g write to our volume, then read its changes back with our driver.
    let (ok, out) = docker_privileged(
        &dir,
        &format!("mkdir -p /mnt/t && ntfs-3g /w/{img} /mnt/t && mkdir /mnt/t/from3g && echo written-by-ntfs3g > /mnt/t/from3g/x.txt && umount /mnt/t && echo MOUNT_RW_OK"),
    );
    if ok && out.contains("MOUNT_RW_OK") {
        let mut fs = open_img(&path);
        let d = fs.lookup(5, "from3g").unwrap();
        let x = fs.lookup(d, "x.txt").unwrap();
        assert_eq!(fs.read(x, 0, 100).unwrap(), b"written-by-ntfs3g\n");
        check_persisted(&mut fs);
    } else {
        eprintln!("ntfs-3g rw mount not possible:\n{out}");
    }
    if let Some(out) = try_mount_check(&dir, img) {
        eprintln!("real mount OK:\n{out}");
    }
}

#[test]
#[ignore]
fn ntfs_it_dirty_flag_vs_ntfsfix() {
    let dir = it_dir();
    let img = "dirty.img";
    let path = dir.join(img);
    make_image(&path, 64 << 20);
    let (ok, out) = docker(&dir, &format!("mkntfs -F -Q /w/{img}"));
    assert!(ok, "{out}");
    {
        let mut fs = open_img(&path);
        set_volume_flags(&mut fs, 0x0001);
        assert!(matches!(fs.dirty_state(), DirtyState::Dirty(_)));
    }
    // ntfs-3g must see the flag we set
    let (_, out) = docker(&dir, &format!("ntfsinfo -f -m /w/{img} | grep 'Volume Flags'"));
    assert!(out.contains("0x0001 DIRTY"), "{out}");
    {
        let mut fs = open_img(&path);
        fs.repair().unwrap();
        assert_eq!(fs.dirty_state(), DirtyState::Clean);
    }
    let (ok, out) = docker(&dir, &format!("ntfsinfo -m /w/{img} | grep 'Volume Flags'; ntfsfix -n /w/{img}"));
    assert!(ok && out.contains("0x0000") && out.contains("processed successfully"), "{out}");
}

/// Recursively copy the volume content to a host directory (for `diff -r` in Docker).
fn export_tree(fs: &mut NtfsFs, dir: Ino, host: &Path) {
    std::fs::create_dir_all(host).unwrap();
    for e in fs.readdir(dir).unwrap() {
        let p = host.join(&e.name);
        match e.kind {
            FileKind::Dir => export_tree(fs, e.ino, &p),
            _ => {
                let size = fs.getattr(e.ino).unwrap().size;
                let mut data = vec![];
                while (data.len() as u64) < size {
                    let r = fs.read(e.ino, data.len() as u64, 1 << 20).unwrap();
                    assert!(!r.is_empty());
                    data.extend(r);
                }
                std::fs::write(&p, &data).unwrap();
            }
        }
    }
}

/// Compare the volume against a host directory tree (exported by Docker from a real mount).
fn compare_tree(fs: &mut NtfsFs, dir: Ino, host: &Path) -> usize {
    let mut n = 0;
    let mut want: Vec<String> = std::fs::read_dir(host).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    want.sort();
    let mut got: Vec<String> = fs.readdir(dir).unwrap().into_iter().map(|e| e.name).collect();
    got.retain(|n| n != "System Volume Information");
    got.sort();
    assert_eq!(got, want, "listing of {}", host.display());
    for name in got {
        let ino = fs.lookup(dir, &name).unwrap();
        let p = host.join(&name);
        if p.is_dir() {
            n += compare_tree(fs, ino, &p);
        } else {
            let want = std::fs::read(&p).unwrap();
            assert_eq!(fs.getattr(ino).unwrap().size, want.len() as u64, "size of {}", p.display());
            let mut got = vec![];
            while got.len() < want.len() {
                let r = fs.read(ino, got.len() as u64, 1 << 20).unwrap();
                assert!(!r.is_empty());
                got.extend(r);
            }
            assert!(got == want, "content of {}", p.display());
            n += 1;
        }
    }
    n
}

fn populate_stress(fs: &mut NtfsFs) {
    let root = 5;
    let tree = fs.mkdir(root, "tree", 0o755, 0, 0).unwrap();
    let moved = fs.mkdir(root, "moved", 0o755, 0, 0).unwrap();
    let name = |i: u32| format!("node {:05} with a long-ish file name {}", (i * 7919) % 10007, "q".repeat((i % 50) as usize));
    for i in 0..1500 {
        let f = fs.create(tree, &name(i), 0o644, 0, 0).unwrap();
        if i % 10 == 0 {
            fs.write(f, 0, &pattern(100 + i as usize * 37, i)).unwrap();
        }
    }
    for i in (0..1500).filter(|i| i % 3 != 0) {
        fs.unlink(tree, &name(i)).unwrap();
    }
    for i in (0..1500).filter(|i| i % 3 == 0 && i % 2 == 0).take(100) {
        fs.rename(tree, &name(i), moved, &format!("renamed {i}")).unwrap();
    }
    let small = fs.create(root, "small.txt", 0o644, 0, 0).unwrap();
    fs.write(small, 0, b"resident data").unwrap();
    let multi = fs.create(root, "multi.bin", 0o644, 0, 0).unwrap();
    fs.write(multi, 0, &pattern(3 << 20, 11)).unwrap();
    let sp = fs.create(root, "sparse-ish.bin", 0o644, 0, 0).unwrap();
    fs.setattr(sp, &SetAttr { size: Some(10 << 20), ..Default::default() }).unwrap();
    fs.write(sp, 9 << 20, b"end marker").unwrap();
    let frag = fs.create(root, "frag.bin", 0o644, 0, 0).unwrap();
    let spacer = fs.create(root, "spacer.bin", 0o644, 0, 0).unwrap();
    for i in 0..220u64 {
        fs.write(frag, i * 4096, &pattern(4096, i as u32)).unwrap();
        fs.write(spacer, i * 4096, &pattern(4096, 1000 + i as u32)).unwrap();
    }
    assert!(fs.load_inode(frag).unwrap().has_al);
    let mut d = root;
    for lvl in 0..6 {
        d = fs.mkdir(d, &format!("level{lvl}"), 0o755, 0, 0).unwrap();
    }
    let deep = fs.create(d, "Città già.txt", 0o644, 0, 0).unwrap();
    fs.write(deep, 0, b"deep unicode file").unwrap();
    fs.sync().unwrap();
}

fn interop_round(dir: &Path, img: &str) {
    let path = dir.join(img);
    let ours = dir.join(format!("{img}.ours"));
    let after = dir.join(format!("{img}.after"));
    let _ = std::fs::remove_dir_all(&ours);
    let _ = std::fs::remove_dir_all(&after);
    {
        let mut fs = open_img(&path);
        populate_stress(&mut fs);
        export_tree(&mut fs, 5, &ours);
    }
    // 1. ntfsfix, kernel ntfs3 (read-only) and ntfs-3g must see exactly what we wrote.
    let script = format!(
        "set -e; ntfsfix -n /w/{img} | tail -1; mkdir -p /mnt/t; \
         N=$(dmesg | wc -l); \
         if mount -t ntfs3 -o loop,ro,iocharset=utf8 /w/{img} /mnt/t 2>/dev/null; then \
           diff -r -x 'System Volume Information' /mnt/t /w/{img}.ours && echo NTFS3_DIFF_OK; \
           ls -R /mnt/t > /dev/null; umount /mnt/t; \
           dmesg | tail -n +$((N+1)) | grep -i ntfs || true; \
         else echo NTFS3_UNAVAILABLE; fi; \
         ntfs-3g /w/{img} /mnt/t; \
         diff -r -x 'System Volume Information' /mnt/t /w/{img}.ours && echo NTFS3G_DIFF_OK; \
         cd /mnt/t/tree; ls | head -400 | tail -200 | while IFS= read -r f; do rm -- \"$f\"; done; \
         for i in $(seq 1 300); do echo \"from ntfs-3g $i\" > \"ntfs3g created file number $i\"; done; \
         mkdir /mnt/t/comp; python3 -c \"import os; os.setxattr('/mnt/t/comp', 'system.ntfs_attrib_be', (0x10|0x800).to_bytes(4,'big'))\"; \
         python3 -c \"import sys; sys.stdout.buffer.write(b''.join(b'compressible line %d\\n' % (i % 50) for i in range(200000)))\" > /mnt/t/comp/text.txt; \
         head -c 300000 /dev/urandom > /mnt/t/comp/random.bin; \
         truncate -s 20M /mnt/t/holey.bin; echo middle | dd of=/mnt/t/holey.bin bs=1 seek=7000000 conv=notrunc 2>/dev/null; \
         rm /mnt/t/spacer.bin; mv /mnt/t/moved /mnt/t/tree/moved-by-3g; \
         cd /; sync; umount /mnt/t; \
         ntfsinfo -F /comp/text.txt /w/{img} | grep -i -E 'compress|flags' | head -5; \
         ntfsinfo -F /holey.bin /w/{img} | grep -i -E 'sparse|flags' | head -5; \
         ntfs-3g -o ro /w/{img} /mnt/t; cp -a /mnt/t/. /w/{img}.after/; umount /mnt/t; \
         rm -rf '/w/{img}.after/System Volume Information'; \
         ntfsfix -n /w/{img} | tail -1"
    );
    let (ok, out) = docker_privileged(dir, &script);
    eprintln!("{out}");
    assert!(ok, "docker interop script failed:\n{out}");
    assert!(out.contains("NTFS3G_DIFF_OK"), "ntfs-3g sees different content:\n{out}");
    assert!(out.contains("NTFS3_DIFF_OK") || out.contains("NTFS3_UNAVAILABLE"), "kernel ntfs3 sees different content:\n{out}");
    assert!(!out.to_lowercase().contains("corrupt"), "driver complained:\n{out}");
    assert!(out.matches("processed successfully").count() == 2, "{out}");
    // 2. We must read back everything ntfs-3g wrote (incl. compressed + sparse files) ...
    {
        let mut fs = open_img(&path);
        let n = compare_tree(&mut fs, 5, &after);
        assert!(n > 500, "only {n} files compared");
        let c = fs.lookup(5, "comp").unwrap();
        let t = fs.lookup(c, "text.txt").unwrap();
        let ti = fs.load_inode(t).unwrap();
        let flags = ti.attr(ti.find(AT_DATA, &[]).unwrap()).flags;
        assert!(flags & ATTR_COMPRESSION_MASK != 0, "ntfs-3g did not compress text.txt (flags {flags:#x})");
        assert!(matches!(fs.write(t, 0, b"x"), Err(FsError::Unsupported(_))));
        // ... and keep modifying the volume afterwards.
        let tree = fs.lookup(5, "tree").unwrap();
        let mut names: Vec<String> = fs.readdir(tree).unwrap().into_iter().filter(|e| e.kind == FileKind::File).map(|e| e.name).collect();
        names.sort();
        for nm in names.iter().step_by(2) {
            fs.unlink(tree, nm).unwrap();
        }
        fs.unlink(c, "text.txt").unwrap();
        let h = fs.lookup(5, "holey.bin").unwrap();
        fs.write(h, 15_000_000, b"written by lumendisk into a hole").unwrap();
        fs.sync().unwrap();
    }
    let script = format!(
        "set -e; ntfsfix -n /w/{img} | tail -1; mkdir -p /mnt/t; ntfs-3g -o ro /w/{img} /mnt/t; \
         ls /mnt/t/tree | wc -l; ls /mnt/t/comp; dd if=/mnt/t/holey.bin bs=1 skip=15000000 count=32 2>/dev/null; echo; \
         dd if=/mnt/t/holey.bin bs=1 skip=7000000 count=6 2>/dev/null; echo; umount /mnt/t"
    );
    let (ok, out) = docker_privileged(dir, &script);
    eprintln!("{out}");
    assert!(ok && out.contains("processed successfully"), "{out}");
    assert!(out.contains("written by lumendisk into a hole") && out.contains("middle"), "{out}");
    assert!(!out.contains("text.txt") && out.contains("random.bin"), "{out}");
}

#[test]
#[ignore]
fn ntfs_it_interop_stress_own_format() {
    let dir = it_dir();
    let img = "stress-ours.img";
    let path = dir.join(img);
    make_image(&path, 256 << 20);
    format(Arc::new(FileDev::open(&path, false).unwrap()), "STRESS").unwrap();
    interop_round(&dir, img);
}

#[test]
#[ignore]
fn ntfs_it_interop_stress_mkntfs() {
    let dir = it_dir();
    let img = "stress-mkntfs.img";
    let path = dir.join(img);
    make_image(&path, 256 << 20);
    let (ok, out) = docker(&dir, &format!("mkntfs -F -Q -L STRESS /w/{img}"));
    assert!(ok, "{out}");
    interop_round(&dir, img);
}

#[test]
#[ignore]
fn ntfs_it_other_cluster_sizes() {
    let dir = it_dir();
    for cs in [512u32, 65536] {
        let img = format!("cs{cs}.img");
        let path = dir.join(&img);
        make_image(&path, 128 << 20);
        let (ok, out) = docker(&dir, &format!("mkntfs -F -Q -c {cs} /w/{img}"));
        assert!(ok, "{out}");
        {
            let mut fs = open_img(&path);
            assert_eq!(fs.statfs().unwrap().block_size, cs);
            conformance(&mut fs, true);
            let d = fs.mkdir(5, "many", 0o755, 0, 0).unwrap();
            for i in 0..400 {
                fs.create(d, &format!("a reasonably long file name number {i:04}"), 0o644, 0, 0).unwrap();
            }
            for i in (0..400).step_by(3) {
                fs.unlink(d, &format!("a reasonably long file name number {i:04}")).unwrap();
            }
        }
        {
            let mut fs = open_img(&path);
            check_persisted(&mut fs);
            let d = fs.lookup(5, "many").unwrap();
            assert_eq!(fs.readdir(d).unwrap().len(), 266);
        }
        validate_with_tools(&dir, &img);
        let (ok, out) = docker(&dir, &format!("ntfsls -p /many /w/{img} | grep -c '^a reasonably'"));
        assert!(ok && out.trim() == "266", "cluster size {cs}: ntfsls sees {out}");
        try_mount_check(&dir, &img);
    }
}

/// Rough performance sanity check (run in release): `cargo test --release ... ntfs_perf -- --ignored --nocapture`
#[test]
#[ignore]
fn ntfs_perf_smoke() {
    let dir = it_dir();
    let path = dir.join("perf.img");
    make_image(&path, 1 << 30);
    format(Arc::new(FileDev::open(&path, false).unwrap()), "PERF").unwrap();
    let mut fs = open_img(&path);
    let t = std::time::Instant::now();
    let d = fs.mkdir(5, "d", 0o755, 0, 0).unwrap();
    for i in 0..20_000 {
        fs.create(d, &format!("file number {i:06} with some padding.txt"), 0o644, 0, 0).unwrap();
    }
    eprintln!("20k creates: {:?}", t.elapsed());
    let t = std::time::Instant::now();
    for i in (0..20_000).step_by(7) {
        fs.lookup(d, &format!("FILE NUMBER {i:06} WITH SOME PADDING.TXT")).unwrap();
    }
    assert_eq!(fs.readdir(d).unwrap().len(), 20_000);
    eprintln!("3k lookups + readdir: {:?}", t.elapsed());
    let t = std::time::Instant::now();
    let f = fs.create(5, "big.bin", 0o644, 0, 0).unwrap();
    let chunk = vec![0x5Au8; 1 << 20];
    for i in 0..300u64 {
        fs.write(f, i << 20, &chunk).unwrap();
    }
    fs.sync().unwrap();
    eprintln!("300 MiB sequential write: {:?}", t.elapsed());
    let t = std::time::Instant::now();
    for i in 0..300u64 {
        assert_eq!(fs.read(f, i << 20, 1 << 20).unwrap().len(), 1 << 20);
    }
    eprintln!("300 MiB read: {:?}", t.elapsed());
    let t = std::time::Instant::now();
    for i in 0..20_000 {
        fs.unlink(d, &format!("file number {i:06} with some padding.txt")).unwrap();
    }
    eprintln!("20k unlinks: {:?}", t.elapsed());
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------------------------
// Symlink interoperability with ntfs-3g (FUSE) and the Linux kernel ntfs3 driver.

/// `$R` entries as ntfsinfo sees them: (tag, record number).
fn ntfsinfo_r_entries(dir: &Path, img: &str) -> Vec<(u32, u64)> {
    let (ok, out) = docker(dir, &format!("ntfsinfo -i 26 -v /w/{img} | grep -E 'Key (reparse tag|file id)'"));
    assert!(ok || out.trim().is_empty(), "ntfsinfo $Reparse failed:\n{out}");
    let mut v = vec![];
    let mut tag = None;
    for line in out.lines() {
        let val = line.split(':').nth(1).unwrap_or("").split_whitespace().next().unwrap_or("");
        if line.contains("Key reparse tag") {
            tag = Some(u32::from_str_radix(val.trim_start_matches("0x"), 16).unwrap());
        } else if line.contains("Key file id") {
            v.push((tag.take().unwrap(), mref_no(val.parse::<u64>().unwrap())));
        }
    }
    v.sort();
    v
}

fn r_sorted(fs: &mut NtfsFs) -> Vec<(u32, u64)> {
    let mut v = r_entries(fs);
    v.sort();
    v
}

/// Lines `PREFIX:name=value` printed by a docker script.
fn tagged(out: &str, prefix: &str) -> std::collections::BTreeMap<String, String> {
    out.lines()
        .filter_map(|l| l.strip_prefix(prefix))
        .filter_map(|l| l.split_once('='))
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

/// Mount the image with `mount_cmd`, print `L<tag>:<link>=<readlink>` and `C<tag>:<link>=<content>`.
fn follow_script(img: &str, mount_cmd: &str, tag: &str, links: &[(&str, &str)]) -> String {
    let mut s = format!("{mount_cmd} /w/{img} /mnt/t 2>&1; echo MOUNT_{tag}_RC=$?; ls -l /mnt/t /mnt/t/dir; ");
    for (l, follow) in links {
        s += &format!("echo \"L{tag}:{l}=$(readlink '/mnt/t/{l}')\"; ");
        if !follow.is_empty() {
            s += &format!("echo \"C{tag}:{l}=$(cat '/mnt/t/{follow}' 2>&1)\"; ");
        }
    }
    s += "umount /mnt/t; ";
    s
}

#[test]
#[ignore]
fn ntfs_it_symlinks_lumendisk_to_ntfs3g_and_ntfs3() {
    let dir = it_dir();
    let img = "links-out.img";
    let path = dir.join(img);
    make_image(&path, 96 << 20);
    let (ok, out) = docker(&dir, &format!("mkntfs -F -Q -L LINKS /w/{img}"));
    assert!(ok, "mkntfs failed:\n{out}");
    // (link, expected target, path to `cat` through the link, expected content)
    let links: &[(&str, &str, &str, &str)] = &[
        ("rel", "target.txt", "rel", "target content"),
        ("dir/up", "../target.txt", "dir/up", "target content"),
        ("dirlink", "dir", "dirlink/inner.txt", "inner content"),
        ("abs", "/mnt/t/target.txt", "abs", "target content"),
        ("absdir", "/mnt/t/dir", "absdir/inner.txt", "inner content"),
        ("abs host", "/etc/hostname", "abs host", ""),
        ("abs città", "/città/ü x/", "", ""),
        ("abs root", "/", "", ""),
        ("abs onvol", "/target.txt", "abs onvol", ""),
        ("link città", "città ü.txt", "link città", "unicode content"),
        ("dangling", "does/not/exist", "", ""),
        ("renamed", "dir/inner.txt", "renamed", "inner content"),
        ("lx colon", "a:b/c", "", ""),
    ];
    {
        let mut fs = open_img(&path);
        let t = fs.create(5, "target.txt", 0o644, 0, 0).unwrap();
        fs.write(t, 0, b"target content").unwrap();
        let u = fs.create(5, "città ü.txt", 0o644, 0, 0).unwrap();
        fs.write(u, 0, b"unicode content").unwrap();
        let d = fs.mkdir(5, "dir", 0o755, 0, 0).unwrap();
        let i = fs.create(d, "inner.txt", 0o644, 0, 0).unwrap();
        fs.write(i, 0, b"inner content").unwrap();
        for (l, target, _, _) in links {
            let (parent, name) = match l.split_once('/') {
                Some((_, n)) => (d, n),
                None => (5, *l),
            };
            let name = if *l == "renamed" { "tmp link" } else { name };
            fs.symlink(parent, name, target, 0, 0).unwrap();
        }
        fs.rename(5, "tmp link", 5, "renamed").unwrap();
        fs.symlink(5, "gone", "target.txt", 0, 0).unwrap();
        fs.unlink(5, "gone").unwrap();
        check_r_consistent(&mut fs);
        fs.sync().unwrap();
        assert_clean(&mut fs);
    }
    // $R as seen by ntfsinfo == what we think is there
    let mut fs = open_img(&path);
    let ours = r_sorted(&mut fs);
    drop(fs);
    assert_eq!(ours.len(), links.len());
    assert_eq!(ntfsinfo_r_entries(&dir, img), ours, "ntfsinfo sees a different $R");
    {
        let mut fs = open_img(&path);
        for (l, target, _, _) in links {
            let (p, n) = match l.split_once('/') {
                Some((a, b)) => (fs.lookup(5, a).unwrap(), b),
                None => (5, *l),
            };
            let i = fs.lookup(p, n).unwrap();
            let want = if *l == "lx colon" { IO_REPARSE_TAG_LX_SYMLINK } else { IO_REPARSE_TAG_SYMLINK };
            assert_eq!(fs.reparse_tag_of(&fs.load_inode(i).unwrap()).unwrap(), Some(want), "{l}");
            assert_eq!(fs.readlink(i).unwrap(), *target);
        }
    }

    let lf: Vec<(&str, &str)> = links.iter().map(|(l, _, f, _)| (*l, *f)).collect();
    let script = format!(
        "ntfsfix -n /w/{img} | tail -1; ntfsinfo -m /w/{img} | grep 'Volume Flags'; mkdir -p /mnt/t; N=$(dmesg | wc -l); {} {} \
         dmesg | tail -n +$((N+1)) | grep -i ntfs || true",
        follow_script(img, "ntfs-3g -o ro", "3G", &lf),
        follow_script(img, "mount -t ntfs3 -o loop,ro,iocharset=utf8", "K", &lf),
    );
    let (ok, out) = docker_privileged(&dir, &script);
    eprintln!("{out}");
    assert!(ok, "{out}");
    assert!(out.contains("processed successfully"), "ntfsfix -n:\n{out}");
    assert!(out.contains("Volume Flags: 0x0000"), "volume not clean after our unmount:\n{out}");
    assert!(out.contains("MOUNT_3G_RC=0") && out.contains("MOUNT_K_RC=0"), "mount failed:\n{out}");
    let low = out.to_lowercase();
    assert!(!low.contains("unclean") && !low.contains("corrupt") && !low.contains("error"), "driver complained:\n{out}");
    assert!(out.contains("dirlink -> dir") && out.contains("drwx"), "ls -l:\n{out}");
    assert!(out.contains("abs -> /mnt/t/target.txt") && out.contains("abs host -> /etc/hostname"), "ls -l (ntfs3):\n{out}");
    // What each reader must report (see the table in reparse.rs):
    // * kernel ntfs3 returns the PrintName: every IO_REPARSE_TAG_SYMLINK link verbatim,
    //   absolute ones included. It has no WSL support: the LX fallback is unreadable.
    // * ntfs-3g returns relative links verbatim (except dangling ones: "unsupported reparse
    //   tag"), and by design maps absolute `\??\C:\x` links under its own mount point:
    //   `<mnt>/x` if `x` exists on the volume, else `<mnt>/.NTFS-3G/C:/x`. It reads LX verbatim.
    let expect = |tag: &str, l: &str, target: &str| -> String {
        match (tag, l) {
            // ntfs3 lists the LX link as an empty regular file
            ("K", "lx colon") => String::new(),
            // ntfs-3g returns this text as the "target" of a relative link it cannot resolve
            ("3G", "dangling") => "unsupported reparse tag 0xa000000c".into(),
            ("3G", "abs onvol") => format!("/mnt/t{target}"),
            ("3G", _) if target.starts_with('/') => format!("/mnt/t/.NTFS-3G/C:{target}"),
            _ => target.to_string(),
        }
    };
    for tag in ["3G", "K"] {
        let rl = tagged(&out, &format!("L{tag}:"));
        let ct = tagged(&out, &format!("C{tag}:"));
        for (l, target, f, content) in links {
            let want = expect(tag, l, target);
            assert_eq!(rl.get(*l).map(String::as_str), Some(want.as_str()), "{tag} readlink {l}:\n{out}");
            if want != *target {
                eprintln!("{tag}: by design, {l} -> {want:?} (created as {target:?})");
            }
            // follow through the link where the reader resolves it to the original object
            let follows = !f.is_empty() && (tag == "K" || !target.starts_with('/'));
            if follows && !content.is_empty() {
                assert_eq!(ct.get(*l).map(String::as_str), Some(*content), "{tag} follow {l}:\n{out}");
            }
        }
        // the target resolved on the volume by ntfs-3g / on the host by ntfs3
        let host = ct.get("abs host").cloned().unwrap_or_default();
        if tag == "K" {
            assert!(!host.is_empty() && !host.contains("No such file"), "ntfs3 follows /etc/hostname: {host:?}");
        }
        let onvol = ct.get("abs onvol").cloned().unwrap_or_default();
        if tag == "3G" {
            assert_eq!(onvol, "target content", "ntfs-3g resolves /target.txt on its volume");
        } else {
            assert!(onvol.contains("No such file"), "ntfs3 resolves /target.txt on the host: {onvol:?}");
        }
    }
    assert!(out.contains("lrwxrwxrwx") && out.contains("rel -> target.txt"), "ls -l:\n{out}");

    // Both drivers delete / move our links (they look the $R entries up and remove them).
    let script = format!(
        "set -e; mkdir -p /mnt/t; ntfs-3g /w/{img} /mnt/t; rm /mnt/t/rel; mv /mnt/t/abs /mnt/t/dir/abs-moved; umount /mnt/t; \
         mount -t ntfs3 -o loop,iocharset=utf8 /w/{img} /mnt/t; rm /mnt/t/dirlink; ls -l /mnt/t; umount /mnt/t; \
         ntfsfix -n /w/{img} | tail -1"
    );
    let (ok, out) = docker_privileged(&dir, &script);
    eprintln!("{out}");
    assert!(ok && out.contains("processed successfully"), "{out}");
    {
        let mut fs = open_img(&path);
        check_r(&mut fs, false);
        assert_eq!(r_entries(&mut fs).len(), links.len() - 2);
        let d = fs.lookup(5, "dir").unwrap();
        let m = fs.lookup(d, "abs-moved").unwrap();
        assert_eq!(fs.readlink(m).unwrap(), "/mnt/t/target.txt");
        assert_eq!(fs.lookup(5, "rel"), Err(FsError::NotFound));
        // and we keep working on the volume after them
        fs.unlink(5, "absdir").unwrap();
        fs.symlink(5, "new", "dir/inner.txt", 0, 0).unwrap();
        check_r(&mut fs, false);
        let r = r_sorted(&mut fs);
        drop(fs);
        assert_eq!(ntfsinfo_r_entries(&dir, img), r);
    }
    // Clean after our unmount: flags, ntfsfix, and ntfs-3g mounts read-write without complaint.
    let (ok, out) = docker_privileged(
        &dir,
        &format!(
            "set -e; ntfsinfo -m /w/{img} | grep 'Volume Flags'; ntfsfix -n /w/{img} | tail -1; mkdir -p /mnt/t; \
             ntfs-3g /w/{img} /mnt/t 2>&1; readlink /mnt/t/new; cat /mnt/t/new; echo; umount /mnt/t; echo RW_MOUNT_OK"
        ),
    );
    eprintln!("{out}");
    assert!(ok && out.contains("Volume Flags: 0x0000") && out.contains("RW_MOUNT_OK") && out.contains("inner content"), "{out}");
    assert!(!out.to_lowercase().contains("unclean"), "{out}");
}

#[test]
#[ignore]
fn ntfs_it_symlinks_ntfs3g_and_ntfs3_to_lumendisk() {
    let dir = it_dir();
    let img = "links-in.img";
    let path = dir.join(img);
    let comp = dir.join(format!("{img}.comp"));
    let _ = std::fs::remove_dir_all(&comp);
    make_image(&path, 96 << 20);
    let (ok, out) = docker(&dir, &format!("mkntfs -F -Q -L LINKSIN /w/{img}"));
    assert!(ok, "mkntfs failed:\n{out}");
    // A Windows absolute symlink and a junction, written as raw reparse data through ntfs-3g.
    let win_abs = ms_reparse(IO_REPARSE_TAG_SYMLINK, "\\??\\C:\\x\\y.txt", "C:\\x\\y.txt", Some(0));
    let junction = ms_reparse(IO_REPARSE_TAG_MOUNT_POINT, "\\??\\C:\\Windows\\System32", "C:\\Windows\\System32", None);
    let hexs = |v: &[u8]| v.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let script = format!(
        "set -e; mkdir -p /mnt/t; ntfs-3g /w/{img} /mnt/t; cd /mnt/t; \
         printf 'file content' > f.txt; mkdir d; printf 'in d' > d/in.txt; \
         ln -s f.txt g_rel; ln -s /mnt/t/f.txt g_abs; ln -s d g_dir; ln -s ../f.txt d/g_up; ln -s 'ü target/x' 'g_ü'; \
         touch winabs; mkdir junction; \
         python3 -c \"import os; os.setxattr('winabs', 'system.ntfs_reparse_data', bytes.fromhex('{}')); os.setxattr('junction', 'system.ntfs_reparse_data', bytes.fromhex('{}'))\"; \
         mkdir comp; python3 -c \"import os; os.setxattr('comp', 'system.ntfs_attrib_be', (0x10|0x800).to_bytes(4,'big'))\"; \
         python3 -c \"import sys; sys.stdout.buffer.write(b''.join(b'compressible line %d of the lznt1 regression\\n' % (i % 97) for i in range(60000)))\" > comp/text.txt; \
         head -c 200000 /dev/urandom > comp/random.bin; python3 -c \"import sys; sys.stdout.buffer.write(bytes(70000) + b'tail')\" > comp/zeros.bin; \
         ls -l /mnt/t || true; cd /; umount /mnt/t; \
         ntfs-3g -o special_files=wsl /w/{img} /mnt/t; cd /mnt/t; ln -s /mnt/t/f.txt w_abs; ln -s 'd/in.txt' w_rel; cd /; umount /mnt/t; \
         mount -t ntfs3 -o loop,iocharset=utf8 /w/{img} /mnt/t; cd /mnt/t; ln -s f.txt k_rel; ln -s /mnt/t/f.txt k_abs; ln -s d k_dir; cd /; umount /mnt/t; \
         ntfs-3g -o ro /w/{img} /mnt/t; for l in g_rel g_abs g_dir d/g_up g_ü winabs junction w_abs w_rel; do echo \"L3G:$l=$(readlink /mnt/t/$l)\"; done; \
         mkdir -p /w/{img}.comp; cp /mnt/t/comp/* /w/{img}.comp/; umount /mnt/t; \
         mount -t ntfs3 -o loop,ro,iocharset=utf8 /w/{img} /mnt/t; for l in k_rel k_abs k_dir g_rel; do echo \"LK:$l=$(readlink /mnt/t/$l)\"; done; umount /mnt/t; \
         ntfsinfo -F /comp/text.txt /w/{img} | grep -i -E 'compress' | head -3; ntfsfix -n /w/{img} | tail -1",
        hexs(&win_abs),
        hexs(&junction)
    );
    let (ok, out) = docker_privileged(&dir, &script);
    eprintln!("{out}");
    assert!(ok && out.contains("processed successfully"), "{out}");
    let l3g = tagged(&out, "L3G:");
    let lk = tagged(&out, "LK:");
    let expect = [
        ("g_rel", "f.txt"),
        ("g_abs", "/mnt/t/f.txt"),
        ("g_dir", "d"),
        ("d/g_up", "../f.txt"),
        ("g_ü", "ü target/x"),
        ("k_rel", "f.txt"),
        ("k_abs", "/mnt/t/f.txt"),
        ("k_dir", "d"),
        ("winabs", "/x/y.txt"),
        ("junction", "/Windows/System32"),
        ("w_abs", "/mnt/t/f.txt"),
        ("w_rel", "d/in.txt"),
    ];
    let before_r;
    {
        let mut fs = open_img(&path);
        for (l, want) in expect {
            let (p, n) = match l.split_once('/') {
                Some((a, b)) => (fs.lookup(5, a).unwrap(), b),
                None => (5, l),
            };
            let i = fs.lookup(p, n).unwrap();
            let got = fs.readlink(i).unwrap();
            assert_eq!(got, want, "readlink {l}");
            // identical to what the creating driver itself reports
            // (ntfs-3g maps the Windows absolute ones under its own mount point instead)
            if let Some(t) = l3g.get(l).filter(|_| !l.starts_with('k') && !matches!(l, "winabs" | "junction")) {
                assert_eq!(&got, t, "ntfs-3g reports {l} -> {t}");
            }
            if let Some(t) = lk.get(l).filter(|_| l.starts_with('k')) {
                assert_eq!(&got, t, "ntfs3 reports {l} -> {t}");
            }
            let a = fs.getattr(i).unwrap();
            assert_eq!((a.kind, a.size), (FileKind::Symlink, want.len() as u64), "getattr {l}");
            let ent = fs.readdir(p).unwrap().into_iter().find(|e| e.name == n).unwrap();
            assert_eq!(ent.kind, FileKind::Symlink, "readdir kind of {l}");
        }
        // LZNT1: ntfs-3g-compressed files read back byte-for-byte
        let c = fs.lookup(5, "comp").unwrap();
        for name in ["text.txt", "random.bin", "zeros.bin"] {
            let want = std::fs::read(comp.join(name)).unwrap();
            let f = fs.lookup(c, name).unwrap();
            assert_eq!(fs.getattr(f).unwrap().size, want.len() as u64, "{name}");
            let mut got = vec![];
            while got.len() < want.len() {
                let r = fs.read(f, got.len() as u64, 50_000).unwrap();
                assert!(!r.is_empty());
                got.extend(r);
            }
            assert!(got == want, "{name}: LZNT1 content differs");
        }
        let t = fs.lookup(c, "text.txt").unwrap();
        let ti = fs.load_inode(t).unwrap();
        assert!(ti.attr(ti.find(AT_DATA, &[]).unwrap()).flags & ATTR_COMPRESSION_MASK != 0, "text.txt not compressed by ntfs-3g");
        // $R as written by ntfs-3g / ntfs3 matches the reparse points on the volume
        check_r(&mut fs, false);
        assert_eq!(r_entries(&mut fs).len(), 7, "k_rel, k_abs, k_dir, winabs, junction, w_abs, w_rel");
        let tag = |fs: &mut NtfsFs, n: &str| {
            let i = fs.lookup(5, n).unwrap();
            fs.reparse_tag_of(&fs.load_inode(i).unwrap()).unwrap()
        };
        assert_eq!(tag(&mut fs, "w_abs"), Some(IO_REPARSE_TAG_LX_SYMLINK));
        assert_eq!(tag(&mut fs, "g_rel"), None, "ntfs-3g default: Interix");
        // delete some of their links (reparse and Interix), rename others
        fs.unlink(5, "k_abs").unwrap();
        fs.unlink(5, "winabs").unwrap();
        fs.unlink(5, "junction").unwrap();
        fs.unlink(5, "g_rel").unwrap();
        fs.unlink(5, "w_rel").unwrap();
        fs.rename(5, "k_dir", 5, "k_dir renamed").unwrap();
        fs.rename(5, "g_dir", 5, "g_dir renamed").unwrap();
        check_r(&mut fs, false);
        before_r = r_sorted(&mut fs);
        assert_eq!(before_r.len(), 3);
        fs.sync().unwrap();
        assert_clean(&mut fs);
    }
    assert_eq!(ntfsinfo_r_entries(&dir, img), before_r);
    let (ok, out) = docker_privileged(
        &dir,
        &format!(
            "set -e; ntfsfix -n /w/{img} | tail -1; ntfsinfo -m /w/{img} | grep 'Volume Flags'; mkdir -p /mnt/t; ntfs-3g /w/{img} /mnt/t 2>&1; \
             ls /mnt/t; readlink '/mnt/t/g_dir renamed'; cat '/mnt/t/g_dir renamed/in.txt'; echo; umount /mnt/t; \
             mount -t ntfs3 -o loop,ro /w/{img} /mnt/t; readlink '/mnt/t/k_dir renamed'; umount /mnt/t"
        ),
    );
    eprintln!("{out}");
    assert!(ok && out.contains("processed successfully") && out.contains("Volume Flags: 0x0000"), "{out}");
    assert!(out.contains("in d") && !out.contains("winabs") && !out.contains("g_rel") && !out.contains("k_abs"), "{out}");
    assert!(!out.to_lowercase().contains("unclean"), "{out}");
}
