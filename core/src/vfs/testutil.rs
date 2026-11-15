//! Driver-agnostic conformance tests. Every driver's unit tests call [`conformance`] on a
//! freshly formatted volume (and re-open it afterwards to check persistence).

use super::*;

fn names(fs: &mut dyn FileSystem, dir: Ino) -> Vec<String> {
    let mut v: Vec<String> = fs.readdir(dir).unwrap().into_iter().map(|e| e.name).collect();
    v.sort();
    v
}

/// Deterministic pseudo-random bytes.
pub fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761).wrapping_add(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x & 0xFF) as u8
        })
        .collect()
}

/// Exercise the full mutable API. `symlinks`: whether the FS supports symlinks.
/// Leaves behind `/keep/persist.bin` (300 KiB pattern seed 7) and `/keep/sub/hello.txt`
/// ("hello lumendisk") for [`check_persisted`].
pub fn conformance(fs: &mut dyn FileSystem, symlinks: bool) {
    let root = fs.root_ino();
    let ra = fs.getattr(root).unwrap();
    assert_eq!(ra.kind, FileKind::Dir);

    // create + write + read
    let f = fs.create(root, "a.txt", 0o644, 1000, 1000).unwrap();
    assert_eq!(fs.lookup(root, "a.txt").unwrap(), f);
    assert_eq!(fs.write(f, 0, b"hello world").unwrap(), 11);
    assert_eq!(fs.read(f, 0, 100).unwrap(), b"hello world");
    assert_eq!(fs.read(f, 6, 5).unwrap(), b"world");
    assert_eq!(fs.read(f, 11, 5).unwrap(), b"");
    assert_eq!(fs.getattr(f).unwrap().size, 11);
    assert_eq!(fs.getattr(f).unwrap().kind, FileKind::File);
    assert_eq!(fs.create(root, "a.txt", 0o644, 0, 0), Err(FsError::Exists));

    // overwrite in the middle, extend past EOF with a hole
    fs.write(f, 6, b"WORLD").unwrap();
    assert_eq!(fs.read(f, 0, 100).unwrap(), b"hello WORLD");
    fs.write(f, 20, b"!").unwrap();
    let r = fs.read(f, 0, 100).unwrap();
    assert_eq!(r.len(), 21);
    assert_eq!(&r[11..20], &[0u8; 9]);

    // truncate down / up
    fs.setattr(f, &SetAttr { size: Some(5), ..Default::default() }).unwrap();
    assert_eq!(fs.read(f, 0, 100).unwrap(), b"hello");
    fs.setattr(f, &SetAttr { size: Some(8), ..Default::default() }).unwrap();
    assert_eq!(fs.read(f, 0, 100).unwrap(), b"hello\0\0\0");
    fs.setattr(f, &SetAttr { size: Some(0), ..Default::default() }).unwrap();
    assert_eq!(fs.getattr(f).unwrap().size, 0);

    // times
    let t = Timestamp { secs: 1_600_000_000, nsecs: 0 };
    let a = fs.setattr(f, &SetAttr { mtime: Some(t), ..Default::default() }).unwrap();
    assert!((a.mtime.secs - t.secs).abs() <= 2, "mtime {:?}", a.mtime);

    // large multi-cluster file written in chunks, read back across boundaries
    let big = pattern(1_234_567, 3);
    let b = fs.create(root, "big.bin", 0o644, 0, 0).unwrap();
    let mut off = 0;
    for chunk in big.chunks(65536) {
        fs.write(b, off as u64, chunk).unwrap();
        off += chunk.len();
    }
    assert_eq!(fs.getattr(b).unwrap().size, big.len() as u64);
    let mut back = vec![];
    let mut o = 0u64;
    loop {
        let r = fs.read(b, o, 100_000).unwrap();
        if r.is_empty() {
            break;
        }
        o += r.len() as u64;
        back.extend(r);
    }
    assert!(back == big, "big file mismatch");
    assert_eq!(fs.read(b, 1_000_000, 10).unwrap(), &big[1_000_000..1_000_010]);

    // directories
    let d = fs.mkdir(root, "dir", 0o755, 0, 0).unwrap();
    assert_eq!(fs.getattr(d).unwrap().kind, FileKind::Dir);
    assert_eq!(fs.mkdir(root, "dir", 0o755, 0, 0), Err(FsError::Exists));
    let mut created = vec![];
    for i in 0..150 {
        let n = format!("file with a fairly long name number {i:03}.dat");
        let id = fs.create(d, &n, 0o644, 0, 0).unwrap();
        fs.write(id, 0, n.as_bytes()).unwrap();
        created.push(n);
    }
    created.sort();
    assert_eq!(names(fs, d), created);
    for (i, n) in created.iter().enumerate() {
        if i % 2 == 0 {
            fs.unlink(d, n).unwrap();
        }
    }
    let remain: Vec<String> = created.iter().enumerate().filter(|(i, _)| i % 2 == 1).map(|(_, n)| n.clone()).collect();
    assert_eq!(names(fs, d), remain);
    let id = fs.lookup(d, &remain[3]).unwrap();
    assert_eq!(fs.read(id, 0, 200).unwrap(), remain[3].as_bytes());
    assert_eq!(fs.rmdir(root, "dir"), Err(FsError::NotEmpty));
    assert_eq!(fs.unlink(root, "dir"), Err(FsError::IsDir));
    assert_eq!(fs.rmdir(root, "a.txt"), Err(FsError::NotDir));
    for n in &remain {
        fs.unlink(d, n).unwrap();
    }
    fs.rmdir(root, "dir").unwrap();
    assert_eq!(fs.lookup(root, "dir"), Err(FsError::NotFound));

    // rename: same dir, across dirs, replace existing, dir move
    let x = fs.mkdir(root, "x", 0o755, 0, 0).unwrap();
    let y = fs.mkdir(root, "y", 0o755, 0, 0).unwrap();
    let m = fs.create(x, "m.txt", 0o644, 0, 0).unwrap();
    fs.write(m, 0, b"move me").unwrap();
    fs.rename(x, "m.txt", x, "n.txt").unwrap();
    assert_eq!(fs.lookup(x, "m.txt"), Err(FsError::NotFound));
    let n = fs.lookup(x, "n.txt").unwrap();
    assert_eq!(fs.read(n, 0, 100).unwrap(), b"move me");
    fs.rename(x, "n.txt", y, "n.txt").unwrap();
    assert_eq!(names(fs, x), Vec::<String>::new());
    let n = fs.lookup(y, "n.txt").unwrap();
    assert_eq!(fs.read(n, 0, 100).unwrap(), b"move me");
    let o = fs.create(y, "o.txt", 0o644, 0, 0).unwrap();
    fs.write(o, 0, b"old").unwrap();
    fs.rename(y, "n.txt", y, "o.txt").unwrap();
    assert_eq!(names(fs, y), vec!["o.txt".to_string()]);
    let o = fs.lookup(y, "o.txt").unwrap();
    assert_eq!(fs.read(o, 0, 100).unwrap(), b"move me");
    fs.rename(root, "y", x, "y2").unwrap();
    assert_eq!(fs.lookup(root, "y"), Err(FsError::NotFound));
    let y2 = fs.lookup(x, "y2").unwrap();
    let o = fs.lookup(y2, "o.txt").unwrap();
    assert_eq!(fs.read(o, 0, 100).unwrap(), b"move me");
    fs.unlink(y2, "o.txt").unwrap();
    fs.rmdir(x, "y2").unwrap();
    fs.rmdir(root, "x").unwrap();

    if symlinks {
        let s = fs.symlink(root, "link", "a.txt", 0, 0).unwrap();
        assert_eq!(fs.getattr(s).unwrap().kind, FileKind::Symlink);
        assert_eq!(fs.readlink(s).unwrap(), "a.txt");
        fs.unlink(root, "link").unwrap();
    }

    // unicode names
    let u = fs.create(root, "Città già – ü.txt", 0o644, 0, 0).unwrap();
    fs.write(u, 0, b"u").unwrap();
    assert_eq!(fs.lookup(root, "Città già – ü.txt").unwrap(), u);

    fs.unlink(root, "a.txt").unwrap();
    fs.unlink(root, "big.bin").unwrap();
    fs.unlink(root, "Città già – ü.txt").unwrap();

    // leave data for persistence check
    let k = fs.mkdir(root, "keep", 0o755, 0, 0).unwrap();
    let p = fs.create(k, "persist.bin", 0o644, 0, 0).unwrap();
    fs.write(p, 0, &pattern(300 * 1024, 7)).unwrap();
    let s = fs.mkdir(k, "sub", 0o755, 0, 0).unwrap();
    let h = fs.create(s, "hello.txt", 0o644, 0, 0).unwrap();
    fs.write(h, 0, b"hello lumendisk").unwrap();
    let st = fs.statfs().unwrap();
    assert!(st.free_bytes < st.total_bytes);
    fs.sync().unwrap();
}

/// Run on a re-opened volume after [`conformance`].
pub fn check_persisted(fs: &mut dyn FileSystem) {
    let root = fs.root_ino();
    let k = fs.lookup(root, "keep").unwrap();
    let p = fs.lookup(k, "persist.bin").unwrap();
    let want = pattern(300 * 1024, 7);
    let mut got = vec![];
    let mut o = 0;
    loop {
        let r = fs.read(p, o, 65536).unwrap();
        if r.is_empty() {
            break;
        }
        o += r.len() as u64;
        got.extend(r);
    }
    assert!(got == want, "persist.bin mismatch after reopen");
    let s = fs.lookup(k, "sub").unwrap();
    let h = fs.lookup(s, "hello.txt").unwrap();
    assert_eq!(fs.read(h, 0, 100).unwrap(), b"hello lumendisk");
    let mut n = names(fs, root);
    n.retain(|x| x == "keep");
    assert_eq!(n, vec!["keep".to_string()]);
}
