//! Robustness kit: treat volumes as untrusted input.
//!
//! * [`seeds`] builds valid, populated images for every driver (our own `mkfs` + files written
//!   through the driver: extents / block maps, htree directories, slow and fast symlinks, NTFS
//!   index allocation, attribute lists, LZNT1-compressed files and reparse points, VFAT long
//!   names, exFAT entry sets, fragmented files, a pending ext4 journal transaction...).
//! * [`mutate`] corrupts such an image deterministically from a case number, biased towards
//!   metadata sectors (every non-zero sector that is not file payload).
//! * [`exercise`] mounts a (possibly corrupt) image read-only and read-write, walks the whole
//!   tree, reads every file, follows every symlink, runs a few mutations, `statfs`, the
//!   Doctor (`dirty_state` / `repair`) and the partition-table parser.
//!
//! Used by `tests/corrupted_images.rs` (deterministic suite) and by the libFuzzer targets in
//! `core/fuzz/`. Not a stable API.

use std::collections::HashSet;
use std::sync::Arc;

use crate::block::{self, BlockDev, MemDev};
use crate::vfs::testutil::pattern;
use crate::vfs::{self, *};

// ───────────────────────────── drivers ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Driver {
    Ext4,
    Ntfs,
    Fat,
    ExFat,
}

impl Driver {
    pub const ALL: [Driver; 4] = [Driver::Ext4, Driver::Ntfs, Driver::Fat, Driver::ExFat];

    pub fn name(self) -> &'static str {
        match self {
            Driver::Ext4 => "ext4",
            Driver::Ntfs => "ntfs",
            Driver::Fat => "fat",
            Driver::ExFat => "exfat",
        }
    }

    pub fn from_name(s: &str) -> Option<Driver> {
        Driver::ALL.into_iter().find(|d| d.name() == s)
    }

    /// Open `dev` with this driver (no probing: a corrupt signature must be rejected by the
    /// driver itself).
    pub fn open(self, dev: Arc<dyn BlockDev>, read_only: bool) -> FsResult<Box<dyn FileSystem>> {
        let d = defaults();
        Ok(match self {
            Driver::Ext4 => Box::new(vfs::ext4_fs::Ext4Fs::open(dev, read_only)?),
            Driver::Ntfs => Box::new(vfs::ntfs_fs::NtfsFs::open(dev, read_only, d)?),
            Driver::Fat => Box::new(vfs::fat_fs::FatFs::open(dev, read_only, d)?),
            Driver::ExFat => Box::new(vfs::exfat_fs::ExFatFs::open(dev, read_only, d)?),
        })
    }
}

fn defaults() -> MountDefaults {
    MountDefaults { uid: 501, gid: 20, file_perm: 0o644, dir_perm: 0o755 }
}

// ───────────────────────────── deterministic RNG ─────────────────────────────

/// SplitMix64: tiny, deterministic, good enough to pick mutations.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

// ───────────────────────────── seed images ─────────────────────────────

/// A valid, populated image plus where its metadata lives.
pub struct Seed {
    pub name: String,
    pub driver: Driver,
    pub image: Vec<u8>,
    /// Indexes of 512-byte sectors holding metadata: non-zero and not file payload.
    pub hot: Vec<u32>,
}

/// Compressible text (for NTFS LZNT1 and directory-ish content).
fn text(len: usize, seed: u32) -> Vec<u8> {
    let words = ["lumen", "disk ", "ext4 ", "ntfs ", "fat\n", "exfat ", "cluster ", "inode ", "record ", "index "];
    let mut out = Vec::with_capacity(len + 16);
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(7);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.extend_from_slice(words[(x % words.len() as u32) as usize].as_bytes());
    }
    out.truncate(len);
    out
}

struct Pop<'a> {
    fs: &'a mut dyn FileSystem,
    contents: Vec<Vec<u8>>,
}

impl Pop<'_> {
    fn file(&mut self, dir: Ino, name: &str, data: &[u8]) -> Ino {
        let f = self.fs.create(dir, name, 0o644, 1000, 1000).unwrap_or_else(|e| panic!("seed: create {name}: {e}"));
        if !data.is_empty() {
            self.fs.write(f, 0, data).unwrap_or_else(|e| panic!("seed: write {name}: {e}"));
        }
        self.contents.push(data.to_vec());
        f
    }
    fn dir(&mut self, dir: Ino, name: &str) -> Ino {
        self.fs.mkdir(dir, name, 0o755, 1000, 1000).unwrap_or_else(|e| panic!("seed: mkdir {name}: {e}"))
    }
}

/// Size knobs for [`populate`].
struct Scale {
    big: usize,
    many: u32,
    frag_chunks: u64,
    symlinks: bool,
    sparse: bool,
}

/// Common tree: small and big files, nested dirs, a large directory with long names,
/// two interleaved (fragmented) files, symlinks and a sparse file where supported.
fn populate(fs: &mut dyn FileSystem, sc: &Scale) -> Vec<Vec<u8>> {
    let root = fs.root_ino();
    populate_at(fs, root, sc)
}

fn populate_at(fs: &mut dyn FileSystem, root: Ino, sc: &Scale) -> Vec<Vec<u8>> {
    let mut p = Pop { fs, contents: vec![] };
    p.file(root, "hello.txt", b"hello lumendisk\n");
    p.file(root, "A long file name with spaces and UPPER case.txt", &text(2000, 1));
    p.file(root, "big.bin", &pattern(sc.big, 2));
    p.file(root, "empty", b"");
    p.dir(root, "emptydir");
    let docs = p.dir(root, "docs");
    let nested = p.dir(docs, "nested");
    let deeper = p.dir(nested, "deeper");
    p.file(deeper, "x.txt", &text(700, 3));
    p.file(nested, "readme.md", &text(5000, 4));
    let many = p.dir(root, "many");
    for i in 0..sc.many {
        let data = pattern((i as usize * 37) % 1500, 100 + i);
        p.file(many, &format!("file number {i:03} with a rather long name.dat"), &data);
    }
    // Interleaved writes: fragmented extents / cluster chains / run lists.
    let fa = p.fs.create(root, "frag-a.bin", 0o644, 0, 0).unwrap();
    let fb = p.fs.create(docs, "frag-b.bin", 0o644, 0, 0).unwrap();
    let (mut da, mut db) = (vec![], vec![]);
    for i in 0..sc.frag_chunks {
        let ca = pattern(4096, 1000 + i as u32);
        let cb = pattern(4096, 5000 + i as u32);
        p.fs.write(fa, i * 4096, &ca).unwrap();
        p.fs.write(fb, i * 4096, &cb).unwrap();
        da.extend_from_slice(&ca);
        db.extend_from_slice(&cb);
    }
    p.contents.push(da);
    p.contents.push(db);
    if sc.symlinks {
        p.fs.symlink(root, "link-rel", "docs/nested", 1000, 1000).unwrap();
        p.fs.symlink(docs, "link-up", "../hello.txt", 1000, 1000).unwrap();
        let long = format!("/{}", "a-rather-long-symlink-target-component/".repeat(6));
        p.fs.symlink(root, "link-long", &long, 1000, 1000).unwrap();
    }
    if sc.sparse {
        let s = p.fs.create(root, "sparse.bin", 0o644, 0, 0).unwrap();
        p.fs.write(s, 3 << 20, b"tail after a hole").unwrap();
        p.contents.push(b"tail after a hole".to_vec());
    }
    p.fs.sync().unwrap();
    p.contents
}

fn hash_sector(s: &[u8]) -> u64 {
    // FNV-1a over the (zero-padded) sector.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for i in 0..512 {
        h ^= *s.get(i).unwrap_or(&0) as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Non-zero sectors that are not (sector-aligned) file payload.
pub fn hot_sectors(image: &[u8], contents: &[Vec<u8>]) -> Vec<u32> {
    let mut data = HashSet::new();
    for c in contents {
        for ch in c.chunks(512) {
            data.insert(hash_sector(ch));
        }
    }
    image
        .chunks(512)
        .enumerate()
        .filter(|(_, s)| s.iter().any(|&b| b != 0) && !data.contains(&hash_sector(s)))
        .map(|(i, _)| i as u32)
        .collect()
}

fn finish(name: &str, driver: Driver, dev: &MemDev, contents: &[Vec<u8>]) -> Seed {
    let image = dev.snapshot();
    let hot = hot_sectors(&image, contents);
    Seed { name: name.into(), driver, image, hot }
}

/// Smaller trees for fuzz corpora (libFuzzer works best on small inputs).
fn sc(small: bool, s: Scale) -> Scale {
    if small {
        Scale { big: 9000, many: s.many.min(40), frag_chunks: s.frag_chunks.min(4), symlinks: s.symlinks, sparse: false }
    } else {
        s
    }
}

fn ext_seed(kind: FsType, mb: usize, htree: bool, journal_txn: bool, small: bool) -> Seed {
    let dev = Arc::new(MemDev::new(mb << 20));
    vfs::ext4_fs::format(dev.clone(), kind, "SEED").unwrap();
    let mut contents;
    {
        let mut fs = vfs::ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
        contents = populate(&mut fs, &sc(small, Scale { big: 150_000, many: 150, frag_chunks: 24, symlinks: true, sparse: true }));
        let root = fs.root_ino();
        if htree {
            let many = fs.lookup(root, "many").unwrap();
            fs.debug_make_htree(many).unwrap();
        }
        if journal_txn {
            // A committed but not yet checkpointed transaction: replayed by repair().
            let f = fs.lookup(root, "hello.txt").unwrap();
            let blocks = fs.debug_file_blocks(f).unwrap();
            let mut img = b"journal replay was here\n".to_vec();
            img.resize(fs.statfs().unwrap().block_size as usize, 0);
            contents.push(img.clone());
            fs.debug_journal_transaction(&[(blocks[0], img)]).unwrap();
        }
        fs.sync().unwrap();
    }
    let name = match (kind, htree, journal_txn) {
        (FsType::Ext4, _, true) => "ext4-journal",
        (FsType::Ext4, _, _) => "ext4",
        (FsType::Ext3, ..) => "ext3",
        _ => "ext2",
    };
    finish(name, Driver::Ext4, &dev, &contents)
}

fn ntfs_seed(small: bool) -> Seed {
    let dev = Arc::new(MemDev::new(if small { 2 << 20 } else { 16 << 20 }));
    vfs::ntfs_fs::format(dev.clone(), "SEED").unwrap();
    let mut contents;
    {
        let mut fs = vfs::ntfs_fs::NtfsFs::open(dev.clone(), false, defaults()).unwrap();
        contents = populate(&mut fs, &sc(small, Scale { big: 150_000, many: 160, frag_chunks: 160, symlinks: true, sparse: true }));
        let root = fs.root_ino();
        // LZNT1-compressed file (Windows "compress contents"): read-only for us, so built by
        // a test hook.
        let c = fs.create(root, "compressed.txt", 0o644, 0, 0).unwrap();
        fs.debug_make_compressed(c, &text(if small { 70_000 } else { 150_000 }, 9)).unwrap();
        // Foreign reparse points: a junction and an unknown tag.
        let j = fs.mkdir(root, "junction", 0o755, 0, 0).unwrap();
        fs.debug_set_reparse(j, vfs::ntfs_fs::debug_junction_value("\\??\\C:\\Windows", "C:\\Windows")).unwrap();
        let u = fs.create(root, "dedup.bin", 0o644, 0, 0).unwrap();
        let mut v = vec![0x13, 0, 0, 0x80, 8, 0, 0, 0];
        v.extend_from_slice(b"opaque!!");
        fs.debug_set_reparse(u, v).unwrap();
        fs.sync().unwrap();
        let _ = &mut contents;
    }
    finish("ntfs", Driver::Ntfs, &dev, &contents)
}

fn fat_seed(kind: FsType, small: bool) -> Seed {
    let (mb, scale) = match kind {
        FsType::Fat12 => (1, Scale { big: 60_000, many: 60, frag_chunks: 12, symlinks: false, sparse: false }),
        FsType::Fat16 => (8, Scale { big: 150_000, many: 150, frag_chunks: 24, symlinks: false, sparse: false }),
        _ => (34, Scale { big: 150_000, many: 150, frag_chunks: 24, symlinks: false, sparse: false }),
    };
    let mb = if small && kind == FsType::Fat16 { 3 } else { mb };
    let dev = Arc::new(MemDev::new(mb << 20));
    vfs::fat_fs::format(dev.clone(), kind, "SEED").unwrap();
    let contents = {
        let mut fs = vfs::fat_fs::FatFs::open(dev.clone(), false, defaults()).unwrap();
        populate(&mut fs, &sc(small, scale))
    };
    finish(&kind.label().to_lowercase(), Driver::Fat, &dev, &contents)
}

fn exfat_seed(small: bool) -> Seed {
    let dev = Arc::new(MemDev::new(if small { 2 << 20 } else { 4 << 20 }));
    vfs::exfat_fs::format(dev.clone(), "SEED").unwrap();
    let contents = {
        let mut fs = vfs::exfat_fs::ExFatFs::open(dev.clone(), false, defaults()).unwrap();
        populate(&mut fs, &sc(small, Scale { big: 150_000, many: 150, frag_chunks: 24, symlinks: false, sparse: false }))
    };
    finish("exfat", Driver::ExFat, &dev, &contents)
}

/// Every seed image (valid volumes), all drivers.
pub fn seeds() -> Vec<Seed> {
    Driver::ALL.into_iter().flat_map(|d| seeds_for(d, false)).collect()
}

/// Seeds for one driver; `small` = reduced trees (fuzz corpora).
///
/// Seeds are byte-for-byte reproducible (frozen clock, fixed ids: `vfs::set_deterministic`),
/// so a regression can be stored as a small patch against a seed ([`Patch`]).
pub fn seeds_for(d: Driver, small: bool) -> Vec<Seed> {
    struct Det;
    impl Drop for Det {
        fn drop(&mut self) {
            vfs::set_deterministic(false);
        }
    }
    vfs::set_deterministic(true);
    let _det = Det;
    match d {
        Driver::Ext4 => vec![
            ext_seed(FsType::Ext4, 8, true, false, small),
            ext_seed(FsType::Ext4, 8, false, true, small),
            ext_seed(FsType::Ext2, 8, false, false, small),
        ],
        Driver::Ntfs => vec![ntfs_seed(small)],
        Driver::Fat => vec![fat_seed(FsType::Fat12, small), fat_seed(FsType::Fat16, small), fat_seed(FsType::Fat32, small)],
        Driver::ExFat => vec![exfat_seed(small)],
    }
}

/// Populate an existing (e.g. real `mkfs`-made) volume with the standard small tree through
/// the driver. Returns the image, or None if the driver can't mount it read-write.
pub fn populate_image(d: Driver, image: Vec<u8>) -> Option<Vec<u8>> {
    let dev = Arc::new(MemDev::from_vec(image));
    {
        let mut fs = d.open(dev.clone(), false).ok()?;
        let symlinks = matches!(d, Driver::Ext4 | Driver::Ntfs);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // In a subdirectory: the image may already hold a tree of its own.
            let root = fs.root_ino();
            let base = fs.mkdir(root, "lumendisk-seed", 0o755, 0, 0).unwrap();
            populate_at(fs.as_mut(), base, &sc(true, Scale { big: 9000, many: 40, frag_chunks: 4, symlinks, sparse: false }))
        }))
        .ok()?;
    }
    Some(dev.snapshot())
}

// ───────────────────────────── mutations ─────────────────────────────

const INTERESTING: [u64; 24] = [
    0,
    1,
    2,
    3,
    7,
    8,
    0x10,
    0x7F,
    0x80,
    0xFF,
    0x100,
    0x200,
    0x1000,
    0x7FFF,
    0x8000,
    0xFFFF,
    0x1_0000,
    0x7FFF_FFFF,
    0x8000_0000,
    0xFFFF_FFFF,
    0x1_0000_0000,
    0x7FFF_FFFF_FFFF_FFFF,
    0x8000_0000_0000_0000,
    u64::MAX,
];

fn rd(img: &[u8], off: usize, w: usize) -> u64 {
    let mut v = 0u64;
    for i in 0..w {
        v |= (*img.get(off + i).unwrap_or(&0) as u64) << (8 * i);
    }
    v
}

fn wr(img: &mut [u8], off: usize, w: usize, v: u64) {
    for i in 0..w {
        if let Some(b) = img.get_mut(off + i) {
            *b = (v >> (8 * i)) as u8;
        }
    }
}

/// One mutation; returns a short description.
fn mutate_once(seed: &Seed, img: &mut Vec<u8>, r: &mut Rng) -> String {
    let nsec = (img.len() / 512).max(1) as u64;
    let hot = |r: &mut Rng| -> usize {
        if seed.hot.is_empty() || r.chance(3) {
            r.below(nsec) as usize
        } else {
            seed.hot[r.below(seed.hot.len() as u64) as usize] as usize
        }
    };
    let roll = r.below(100);
    match roll {
        // bit flips inside metadata
        0..=24 => {
            let n = 1 + r.below(4);
            let mut d = String::from("bitflip");
            for _ in 0..n {
                let off = hot(r) * 512 + r.below(512) as usize;
                let bit = r.below(8);
                if off < img.len() {
                    img[off] ^= 1 << bit;
                }
                d.push_str(&format!(" {off:#x}.{bit}"));
            }
            d
        }
        // integer field := interesting / neighbour / dictionary value
        25..=54 => {
            let w = [1usize, 2, 4, 4, 8][r.below(5) as usize];
            let sec = if r.chance(10) { r.below(8.min(nsec)) as usize } else { hot(r) };
            let off = sec * 512 + (r.below(512) as usize / w) * w;
            let old = rd(img, off, w);
            let v = match r.below(6) {
                0 | 1 => INTERESTING[r.below(INTERESTING.len() as u64) as usize],
                2 => old.wrapping_add(1 + r.below(16)),
                3 => old.wrapping_sub(1 + r.below(16)),
                4 => {
                    // value seen elsewhere in metadata (cross-links, self references)
                    let s2 = hot(r) * 512 + (r.below(512) as usize / w) * w;
                    rd(img, s2, w)
                }
                _ => r.below(1 << (4 * w.min(8))),
            };
            wr(img, off, w, v);
            format!("set{} {off:#x} {old:#x}->{v:#x}", w * 8)
        }
        // random bytes
        55..=64 => {
            let off = hot(r) * 512 + r.below(512) as usize;
            let n = 1 + r.below(16) as usize;
            for i in 0..n {
                let v = r.next() as u8;
                if let Some(b) = img.get_mut(off + i) {
                    *b = v;
                }
            }
            format!("bytes {off:#x}+{n}")
        }
        // copy one metadata sector over another (cycles, cross-linked structures)
        65..=72 => {
            let a = hot(r);
            let b = hot(r);
            if (a + 1) * 512 <= img.len() && (b + 1) * 512 <= img.len() {
                let src = img[a * 512..(a + 1) * 512].to_vec();
                img[b * 512..(b + 1) * 512].copy_from_slice(&src);
            }
            format!("copy sector {a}->{b}")
        }
        // wipe / fill a metadata sector
        73..=78 => {
            let a = hot(r);
            let v = if r.chance(70) { 0 } else { 0xFF };
            if (a + 1) * 512 <= img.len() {
                img[a * 512..(a + 1) * 512].fill(v);
            }
            format!("fill sector {a} with {v:#x}")
        }
        // random flips anywhere
        79..=88 => {
            let n = 1 + r.below(32);
            for _ in 0..n {
                let off = r.below(img.len().max(1) as u64) as usize;
                if off < img.len() {
                    img[off] ^= 1 << r.below(8);
                }
            }
            format!("{n} random bitflips")
        }
        // truncation
        89..=93 => {
            let len = img.len() as u64;
            let new = if r.chance(50) { r.below(len) } else { len - r.below(len / 8 + 1) };
            let new = if r.chance(70) { new / 512 * 512 } else { new };
            img.truncate(new.max(512) as usize);
            format!("truncate to {}", img.len())
        }
        // a run of interesting 32-bit values over a metadata sector (descriptor tables)
        _ => {
            let off = hot(r) * 512;
            let v = INTERESTING[r.below(INTERESTING.len() as u64) as usize];
            let n = 1 + r.below(8) as usize;
            for i in 0..n {
                wr(img, off + (r.below(128) as usize) * 4, 4, v.wrapping_add(i as u64));
            }
            format!("spray {n}x {v:#x} at sector {}", off / 512)
        }
    }
}

/// Make a mutant pass the integrity checks that would otherwise reject it at the door, so
/// the corruption reaches the code behind them: exFAT boot-region, entry-set and up-case
/// checksums, NTFS update-sequence fixups of MFT / index records. (ext4 checksums are not
/// verified by the driver; FAT has none.)
pub fn fix_checksums(img: &mut [u8]) {
    if img.len() >= 512 && &img[3..11] == b"EXFAT   " {
        fix_exfat(img);
    }
    if img.len() >= 512 && &img[3..11] == b"NTFS    " {
        fix_ntfs_fixups(img);
    }
}

fn fix_exfat(img: &mut [u8]) {
    let (bps_sh, spc_sh) = (img[108] as usize, img[109] as usize);
    if !(9..=12).contains(&bps_sh) || spc_sh > 25 - bps_sh {
        return;
    }
    let bps = 1usize << bps_sh;
    let cs = bps << spc_sh;
    // boot regions (main + backup): 11 sectors checksummed into the 12th
    for base in [0, 12 * bps] {
        if base + 12 * bps <= img.len() {
            let c = vfs::exfat_fs::boot_checksum(&img[base..base + 11 * bps]);
            for k in 0..bps / 4 {
                wr(img, base + 11 * bps + 4 * k, 4, c as u64);
            }
        }
    }
    let heap = rd(img, 88, 4) as usize * bps;
    let end = (heap.saturating_add((rd(img, 92, 4) as usize).saturating_mul(cs))).min(img.len());
    let mut i = heap;
    while i + 64 <= end {
        match img[i] {
            0x85 => {
                let sc = img[i + 1] as usize;
                let n = (1 + sc) * 32;
                if (2..=18).contains(&sc) && i + n <= end && img[i + 32] == 0xC0 {
                    let c = vfs::exfat_fs::set_checksum(&img[i..i + n]);
                    wr(img, i + 2, 2, c as u64);
                }
            }
            0x82 => {
                // up-case table: contiguous clusters from FirstCluster
                let first = rd(img, i + 20, 4) as usize;
                let len = rd(img, i + 24, 8) as usize;
                let at = heap.saturating_add(first.saturating_sub(2).saturating_mul(cs));
                if first >= 2 && len <= 128 << 10 && at.saturating_add(len) <= img.len() {
                    let c = vfs::exfat_fs::table_checksum(&img[at..at + len]);
                    wr(img, i + 4, 4, c as u64);
                }
            }
            _ => {}
        }
        i += 32;
    }
}

fn fix_ntfs_fixups(img: &mut [u8]) {
    let mut i = 0;
    while i + 512 <= img.len() {
        let magic = &img[i..i + 4];
        if magic == b"FILE" || magic == b"INDX" {
            let usa = rd(img, i + 4, 2) as usize;
            let count = rd(img, i + 6, 2) as usize;
            if count >= 2 && count <= 129 && usa + 2 * count <= 512 && i + (count - 1) * 512 <= img.len() {
                let usn = rd(img, i + usa, 2);
                for k in 1..count {
                    wr(img, i + k * 512 - 2, 2, usn);
                }
            }
        }
        i += 512;
    }
}

/// Deterministically corrupt `seed` for case number `case`. Returns the mutant and a
/// description of what was done.
pub fn mutate(seed: &Seed, case: u64) -> (Vec<u8>, String) {
    let name = seed.name.bytes().fold(0u64, |h, b| h.wrapping_mul(131).wrapping_add(b as u64));
    let mut r = Rng::new(case.wrapping_mul(0xA24B_AED4_963E_E407) ^ name);
    let mut img = seed.image.clone();
    let n = if r.chance(70) { 1 } else { 2 + r.below(6) };
    let mut desc = vec![];
    for _ in 0..n {
        desc.push(mutate_once(seed, &mut img, &mut r));
    }
    if r.chance(60) {
        fix_checksums(&mut img);
        desc.push("checksums fixed".into());
    }
    (img, desc.join("; "))
}

// ───────────────────────────── exercising a volume ─────────────────────────────

/// Bounds on what one exercise does (they keep a legitimately huge sparse file from turning
/// a test case into a minutes-long read).
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub read_bytes: u64,
    pub entries: usize,
    pub depth: usize,
    /// Run the read-write phase (mutations, Doctor repair).
    pub write: bool,
}

impl Default for Budget {
    fn default() -> Self {
        Budget { read_bytes: 32 << 20, entries: 20_000, depth: 48, write: true }
    }
}

/// What a walk found (used by tests to check the seeds are really walked).
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub opened_ro: bool,
    pub opened_rw: bool,
    pub entries: usize,
    pub bytes_read: u64,
    pub errors: usize,
}

struct Node {
    parent: Ino,
    name: String,
    ino: Ino,
    kind: FileKind,
}

fn walk(fs: &mut dyn FileSystem, b: &Budget, st: &mut Stats, nodes: &mut Vec<Node>) {
    let err = |st: &mut Stats| st.errors += 1;
    let _ = fs.volume_info();
    let _ = fs.read_only_reason();
    if fs.statfs().is_err() {
        err(st);
    }
    let _ = fs.dirty_state();
    let root = fs.root_ino();
    if fs.getattr(root).is_err() {
        err(st);
    }
    let mut seen = HashSet::new();
    seen.insert(root);
    let mut stack = vec![(root, 0usize)];
    let mut budget = b.read_bytes;
    let mut count = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let ents = match fs.readdir(dir) {
            Ok(e) => e,
            Err(_) => {
                err(st);
                continue;
            }
        };
        for e in ents {
            count += 1;
            if count > b.entries {
                return;
            }
            st.entries += 1;
            match fs.lookup(dir, &e.name) {
                Ok(i) if i != e.ino => err(st),
                Ok(_) => {}
                Err(_) => err(st),
            }
            let attr = match fs.getattr(e.ino) {
                Ok(a) => a,
                Err(_) => {
                    err(st);
                    continue;
                }
            };
            if nodes.len() < 64 {
                nodes.push(Node { parent: dir, name: e.name.clone(), ino: e.ino, kind: attr.kind });
            }
            match attr.kind {
                FileKind::File => {
                    let mut off = 0u64;
                    while off < attr.size && budget > 0 {
                        match fs.read(e.ino, off, 65536) {
                            Ok(d) if d.is_empty() => break,
                            Ok(d) => {
                                off += d.len() as u64;
                                budget = budget.saturating_sub(d.len() as u64);
                                st.bytes_read += d.len() as u64;
                            }
                            Err(_) => {
                                err(st);
                                break;
                            }
                        }
                    }
                    // tail, past EOF and offsets near the top of the u64 range
                    let _ = fs.read(e.ino, attr.size.saturating_sub(100), 4096);
                    let _ = fs.read(e.ino, attr.size.saturating_add(1 << 20), 10);
                    let _ = fs.read(e.ino, u64::MAX - 10, 4096);
                    // offsets whose block number no longer fits in 32 bits (16 / 64 TiB)
                    let _ = fs.read(e.ino, (1 << 42) + 5000, 4096);
                    let _ = fs.read(e.ino, (1 << 44) + 5000, 4096);
                }
                FileKind::Symlink => {
                    if fs.readlink(e.ino).is_err() {
                        err(st);
                    }
                }
                FileKind::Dir => {
                    if depth < b.depth && seen.insert(e.ino) {
                        stack.push((e.ino, depth + 1));
                    }
                }
            }
        }
    }
}

/// A handful of namespace and data mutations on a writable (possibly corrupt) volume.
fn modify(fs: &mut dyn FileSystem, nodes: &[Node]) {
    let root = fs.root_ino();
    let data = pattern(70_000, 77);
    if let Ok(f) = fs.create(root, "fz-new.bin", 0o644, 501, 20) {
        let _ = fs.write(f, 0, &data);
        let _ = fs.write(f, 300_000, b"after a gap");
        let _ = fs.read(f, 0, 65536);
        let _ = fs.setattr(f, &SetAttr { size: Some(10_000), ..Default::default() });
    }
    if let Ok(d) = fs.mkdir(root, "fz-dir", 0o755, 501, 20) {
        let _ = fs.create(d, "inner", 0o644, 0, 0);
        let _ = fs.rename(root, "fz-new.bin", d, "moved.bin");
        let _ = fs.symlink(d, "fz-link", "moved.bin", 0, 0);
        let _ = fs.readdir(d);
    }
    let mut did_file = 0;
    let mut did_dir = 0;
    for n in nodes {
        match n.kind {
            FileKind::File if did_file < 3 => {
                did_file += 1;
                let _ = fs.write(n.ino, 0, b"overwritten");
                let _ = fs.write(n.ino, 5000, &data[..9000]);
                if let Ok(a) = fs.getattr(n.ino) {
                    let _ = fs.setattr(n.ino, &SetAttr { size: Some(a.size / 2), mtime: Some(Timestamp::now()), ..Default::default() });
                }
                if did_file == 1 {
                    let _ = fs.unlink(n.parent, &n.name);
                } else if did_file == 2 {
                    let _ = fs.rename(n.parent, &n.name, root, "fz-renamed");
                }
            }
            FileKind::Dir if did_dir < 3 => {
                did_dir += 1;
                let _ = fs.create(n.ino, "fz-child", 0o644, 0, 0);
                let _ = fs.mkdir(n.ino, "fz-subdir", 0o755, 0, 0);
                // moving a directory below itself must fail cleanly (ancestor walk)
                let _ = fs.rename(n.parent, &n.name, n.ino, "fz-loop");
                let _ = fs.rmdir(n.parent, &n.name);
                let _ = fs.unlink(n.ino, "fz-child");
            }
            FileKind::Symlink => {
                let _ = fs.unlink(n.parent, &n.name);
            }
            _ => {}
        }
    }
    let _ = fs.unlink(root, "fz-renamed");
    let _ = fs.statfs();
    let _ = fs.sync();
}

/// Mount `image` with `driver` every way the app can and use it. Never panics or hangs on a
/// correct driver, whatever the bytes.
pub fn exercise(driver: Driver, image: Vec<u8>, b: &Budget) -> Stats {
    let mut st = Stats::default();
    let dev: Arc<dyn BlockDev> = Arc::new(MemDev::from_vec(image));
    // What the app runs before mounting: partition table + probing.
    let _ = block::parse_partitions(dev.as_ref());
    let _ = vfs::probe(dev.as_ref());

    let mut nodes = vec![];
    if let Ok(mut fs) = driver.open(dev.clone(), true) {
        st.opened_ro = true;
        walk(fs.as_mut(), b, &mut st, &mut nodes);
    }
    if !b.write {
        return st;
    }
    if let Ok(mut fs) = driver.open(dev.clone(), false) {
        st.opened_rw = true;
        let mut nodes = vec![];
        let mut b2 = *b;
        b2.read_bytes = b.read_bytes / 4;
        walk(fs.as_mut(), &b2, &mut st, &mut nodes);
        modify(fs.as_mut(), &nodes);
        drop(fs);
        // what we wrote must not make the next mount crash either
        if let Ok(mut fs) = driver.open(dev.clone(), true) {
            walk(fs.as_mut(), &b2, &mut st, &mut vec![]);
        }
    }
    // Journal Doctor: replay / repair, then use the result.
    if let Ok(mut fs) = driver.open(dev.clone(), false) {
        let _ = fs.dirty_state();
        let _ = fs.repair();
        let mut b3 = *b;
        b3.read_bytes = b.read_bytes / 4;
        walk(fs.as_mut(), &b3, &mut st, &mut vec![]);
    }
    st
}

// ───────────────────────────── fuzz inputs ─────────────────────────────

/// Largest device a fuzz input may describe.
pub const FUZZ_MAX_DEV: u64 = 64 << 20;

/// Fuzz input format: `u32 LE device size in 512-byte sectors` (bit 31: run
/// [`fix_checksums`] on the image), then the image prefix (the device is zero-filled past
/// it). Lets a 34 MiB FAT32 volume be a ~1 MiB input.
pub fn decode_fuzz_input(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 {
        return None;
    }
    let want = (u32::from_le_bytes(data[..4].try_into().unwrap()) & 0x7FFF_FFFF) as u64 * 512;
    let payload = &data[4..];
    let size = (payload.len() as u64).max(want.min(FUZZ_MAX_DEV));
    if size > FUZZ_MAX_DEV {
        return None;
    }
    let mut img = vec![0u8; size as usize];
    img[..payload.len()].copy_from_slice(payload);
    if data[3] & 0x80 != 0 {
        fix_checksums(&mut img);
    }
    Some(img)
}

pub fn encode_fuzz_input(image: &[u8]) -> Vec<u8> {
    let used = image.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
    let used = used.div_ceil(512) * 512;
    // Bit 31 of the size word: repair checksums after mutation (set for seeds; the fuzzer
    // can flip it to explore the integrity checks themselves).
    let mut out = ((image.len() as u64).div_ceil(512) as u32 | 0x8000_0000).to_le_bytes().to_vec();
    out.extend_from_slice(&image[..used.min(image.len())]);
    out
}

/// libFuzzer entry point shared by all targets.
pub fn fuzz_one(driver: Driver, data: &[u8]) {
    if let Some(img) = decode_fuzz_input(data) {
        exercise(driver, img, &Budget { read_bytes: 8 << 20, entries: 5000, depth: 32, write: true });
    }
}

// ───────────────────────────── partition tables ─────────────────────────────

/// What the app does with a whole disk before mounting anything: parse MBR / GPT (incl. the
/// EBR chain), then probe and open every partition read-only. Panics if the parser returns a
/// partition outside the device (it must filter those).
pub fn exercise_partitions(image: Vec<u8>) -> usize {
    let dev: Arc<dyn BlockDev> = Arc::new(MemDev::from_vec(image));
    let Ok(parts) = block::parse_partitions(dev.as_ref()) else { return 0 };
    for p in &parts {
        let pd = block::PartitionDev::new(dev.clone(), p.start, p.size).unwrap_or_else(|e| panic!("partition {} outside the device: {e}", p.index));
        let pd: Arc<dyn BlockDev> = Arc::new(pd);
        if vfs::probe(pd.as_ref()).is_some() {
            if let Ok(mut fs) = vfs::open_fs(pd, true, defaults()) {
                let _ = fs.statfs();
                let r = fs.root_ino();
                let _ = fs.readdir(r);
            }
        }
    }
    parts.len()
}

/// A disk with an MBR (two primaries + an extended partition holding a 2-entry EBR chain)
/// and, as `gpt`, a protective MBR + GPT with three entries. Partitions hold small FAT12
/// volumes so probing finds something.
pub fn partition_seed(gpt: bool) -> Vec<u8> {
    const S: usize = 512;
    let mut img = vec![0u8; 4 << 20];
    let fat = |img: &mut Vec<u8>, lba: usize, sectors: usize| {
        let dev = Arc::new(MemDev::new(sectors * S));
        vfs::fat_fs::format(dev.clone(), FsType::Fat12, "PART").unwrap();
        img[lba * S..(lba + sectors) * S].copy_from_slice(&dev.snapshot());
    };
    let put = |img: &mut Vec<u8>, at: usize, t: u8, lba: u32, n: u32| {
        img[at + 4] = t;
        img[at + 8..at + 12].copy_from_slice(&lba.to_le_bytes());
        img[at + 12..at + 16].copy_from_slice(&n.to_le_bytes());
    };
    if gpt {
        put(&mut img, 446, 0xEE, 1, 8191);
        img[510] = 0x55;
        img[511] = 0xAA;
        let mut h = vec![0u8; 92];
        h[..8].copy_from_slice(b"EFI PART");
        h[72..80].copy_from_slice(&2u64.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        img[S..S + 92].copy_from_slice(&h);
        for (k, (first, last)) in [(64u64, 2111u64), (2112, 4159), (4160, 8000)].iter().enumerate() {
            let e = 2 * S + k * 128;
            img[e] = 0xA2 + k as u8; // any non-zero type GUID
            img[e + 32..e + 40].copy_from_slice(&first.to_le_bytes());
            img[e + 40..e + 48].copy_from_slice(&last.to_le_bytes());
            for (j, c) in "part".encode_utf16().enumerate() {
                img[e + 56 + 2 * j..e + 58 + 2 * j].copy_from_slice(&c.to_le_bytes());
            }
            fat(&mut img, *first as usize, 1024);
        }
    } else {
        put(&mut img, 446, 0x01, 64, 1024);
        put(&mut img, 462, 0x83, 1100, 1024);
        put(&mut img, 478, 0x05, 2200, 4000);
        img[510] = 0x55;
        img[511] = 0xAA;
        fat(&mut img, 64, 1024);
        fat(&mut img, 1100, 1024);
        // EBR 1 at 2200: logical at +64, next EBR at base+1200
        let e1 = 2200 * S;
        put(&mut img, e1 + 446, 0x01, 64, 1024);
        put(&mut img, e1 + 462, 0x05, 1200, 1100);
        img[e1 + 510] = 0x55;
        img[e1 + 511] = 0xAA;
        fat(&mut img, 2264, 1024);
        let e2 = 3400 * S;
        put(&mut img, e2 + 446, 0x01, 64, 1024);
        img[e2 + 510] = 0x55;
        img[e2 + 511] = 0xAA;
        fat(&mut img, 3464, 1024);
    }
    img
}

/// Corrupt a partition seed: table sectors (MBR, GPT header + entries, EBRs) get most hits.
pub fn mutate_partitions(seed: &[u8], case: u64) -> Vec<u8> {
    let mut r = Rng::new(case ^ 0x7A87_1E5);
    let mut img = seed.to_vec();
    let table_secs = [0usize, 1, 2, 3, 2200, 3400];
    let n = 1 + r.below(6);
    for _ in 0..n {
        let sec = table_secs[r.below(table_secs.len() as u64) as usize];
        let w = [1usize, 2, 4, 8][r.below(4) as usize];
        let off = sec * 512 + (r.below(512) as usize / w) * w;
        let v = match r.below(4) {
            0 => INTERESTING[r.below(INTERESTING.len() as u64) as usize],
            1 => r.next(),
            2 => rd(&img, off, w).wrapping_add(1 + r.below(4000)),
            _ => [2200u64, 3400, 0, 1, 2][r.below(5) as usize], // EBR / table self-references
        };
        wr(&mut img, off, w, v);
    }
    if r.chance(10) {
        let len = img.len() as u64;
        img.truncate((r.below(len) as usize / 512 * 512).max(1024));
    }
    img
}

// ───────────────────────────── compact storage for regression inputs ─────────────────────────────

/// A seed by name (`ext4`, `ext4-journal`, `ext2`, `ntfs`, `fat12`, `fat16`, `fat32`, `exfat`).
pub fn seed_by_name(name: &str, small: bool) -> Option<Seed> {
    Driver::ALL.into_iter().find_map(|d| {
        if !seed_names(d).contains(&name) {
            return None;
        }
        seeds_for(d, small).into_iter().find(|s| s.name == name)
    })
}

fn seed_names(d: Driver) -> &'static [&'static str] {
    match d {
        Driver::Ext4 => &["ext4", "ext4-journal", "ext2"],
        Driver::Ntfs => &["ntfs"],
        Driver::Fat => &["fat12", "fat16", "fat32"],
        Driver::ExFat => &["exfat"],
    }
}

/// `LDP1` regression file: an image stored as the byte runs that differ from a reproducible
/// seed. Layout: magic, u8 small, u8 name length, name, u64 image length, then
/// `[u64 offset][u32 length][bytes]`*.
pub fn make_patch(seed: &Seed, small: bool, image: &[u8]) -> Vec<u8> {
    let mut out = b"LDP1".to_vec();
    out.push(small as u8);
    out.push(seed.name.len() as u8);
    out.extend_from_slice(seed.name.as_bytes());
    out.extend_from_slice(&(image.len() as u64).to_le_bytes());
    let base = &seed.image;
    let differs = |i: usize| base.get(i) != Some(&image[i]);
    let mut i = 0;
    while i < image.len() {
        if !differs(i) {
            i += 1;
            continue;
        }
        let s = i;
        let mut same = 0;
        while i < image.len() && same < 16 {
            if differs(i) {
                same = 0;
            } else {
                same += 1;
            }
            i += 1;
        }
        let e = i - same;
        out.extend_from_slice(&(s as u64).to_le_bytes());
        out.extend_from_slice(&((e - s) as u32).to_le_bytes());
        out.extend_from_slice(&image[s..e]);
    }
    out
}

/// Rebuild the image of an `LDP1` file: (driver, image).
pub fn apply_patch(enc: &[u8]) -> Option<(Driver, Vec<u8>)> {
    if enc.len() < 6 || &enc[..4] != b"LDP1" {
        return None;
    }
    let small = enc[4] != 0;
    let nl = enc[5] as usize;
    let name = std::str::from_utf8(enc.get(6..6 + nl)?).ok()?;
    let mut p = 6 + nl;
    let len = u64::from_le_bytes(enc.get(p..p + 8)?.try_into().ok()?) as usize;
    p += 8;
    let seed = seed_by_name(name, small)?;
    let mut img = seed.image;
    img.resize(len, 0);
    while p < enc.len() {
        let off = u64::from_le_bytes(enc.get(p..p + 8)?.try_into().ok()?) as usize;
        let n = u32::from_le_bytes(enc.get(p + 8..p + 12)?.try_into().ok()?) as usize;
        p += 12;
        img.get_mut(off..off + n)?.copy_from_slice(enc.get(p..p + n)?);
        p += n;
    }
    Some((seed.driver, img))
}

/// Turn a crashing fuzz input into a regression file: a patch against the closest small seed
/// when that is compact, the zero-run-encoded input otherwise. Returns (extension, bytes).
pub fn regression_file(driver: Driver, fuzz_input: &[u8]) -> Option<(&'static str, Vec<u8>)> {
    let img = decode_fuzz_input(fuzz_input)?;
    let best = seeds_for(driver, true).into_iter().map(|s| (make_patch(&s, true, &img), s)).min_by_key(|(p, _)| p.len());
    match best {
        Some((p, _)) if p.len() < 256 << 10 => Some(("ldp", p)),
        _ => Some(("ldz", sparse_encode(fuzz_input))),
    }
}

/// Zero-run-length encoding (`LDZ1`, total length, then `[zeros u32][literal len u32][literal]`*):
/// keeps checked-in crash images small.
pub fn sparse_encode(data: &[u8]) -> Vec<u8> {
    let mut out = b"LDZ1".to_vec();
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    let mut i = 0;
    while i < data.len() {
        let z0 = i;
        while i < data.len() && data[i] == 0 {
            i += 1;
        }
        let zeros = i - z0;
        let l0 = i;
        // literal until a run of >= 16 zeros
        while i < data.len() {
            if data[i] == 0 && data[i..].iter().take(16).all(|&b| b == 0) && data.len() - i >= 16 {
                break;
            }
            i += 1;
        }
        out.extend_from_slice(&(zeros as u32).to_le_bytes());
        out.extend_from_slice(&((i - l0) as u32).to_le_bytes());
        out.extend_from_slice(&data[l0..i]);
    }
    out
}

pub fn sparse_decode(enc: &[u8]) -> Option<Vec<u8>> {
    if enc.len() < 12 || &enc[..4] != b"LDZ1" {
        return None;
    }
    let total = u64::from_le_bytes(enc[4..12].try_into().ok()?);
    if total > 256 << 20 {
        return None;
    }
    let mut out = Vec::with_capacity(total as usize);
    let mut p = 12;
    while p + 8 <= enc.len() {
        let z = u32::from_le_bytes(enc[p..p + 4].try_into().ok()?) as usize;
        let l = u32::from_le_bytes(enc[p + 4..p + 8].try_into().ok()?) as usize;
        p += 8;
        if out.len() + z + l > total as usize || p + l > enc.len() {
            return None;
        }
        out.resize(out.len() + z, 0);
        out.extend_from_slice(&enc[p..p + l]);
        p += l;
    }
    out.resize(total as usize, 0);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_roundtrip() {
        let mut v = vec![0u8; 5120];
        v[10] = 1;
        v[4000..4100].fill(7);
        v[5119] = 9;
        assert_eq!(sparse_decode(&sparse_encode(&v)).unwrap(), v);
        assert_eq!(sparse_decode(&sparse_encode(&[])).unwrap(), Vec::<u8>::new());
        let enc = encode_fuzz_input(&v);
        assert_eq!(decode_fuzz_input(&enc).unwrap(), v);
    }
}
