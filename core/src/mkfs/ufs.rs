//! mkfs.ufs — a fresh UFS2 filesystem, as FreeBSD's `newfs` builds it.
//!
//! Reference: FreeBSD 14 `sbin/newfs/newfs.c` + `mkfs.c`, `sys/ufs/ffs/fs.h`,
//! `sys/ufs/ufs/dinode.h`, `sys/ufs/ufs/dir.h`, `lib/libufs` (`sbput`, `cgput`, `putinode`),
//! `sys/ufs/ffs/ffs_subr.c` (`ffs_calc_sbhash`, `validate_sblock`) and fsck_ffs `pass5.c`.
//! The geometry (cylinder-group size, inodes per group, summary area…) is computed with the
//! same steps as `mkfs()` so a LumenDisk volume and `newfs -b B -f B/8` of the same size have
//! the same layout (checked by the e2e suite with `dumpfs` in a FreeBSD VM).
//!
//! Defaults matched (FreeBSD 14 `newfs` without options):
//! - UFS2, 32 KiB blocks / 4 KiB fragments (`MkfsOptions::cluster_size` picks the block size,
//!   4096…65536, and the fragment is always block / 8 as `newfs -b B -f B/8`);
//! - one inode per 2 fragments (`NFPI`), minfree 8 %, optimisation "time", maxcontig =
//!   MAXPHYS (1 MiB) / block, average file size 16 KiB, 64 files per directory, metaspace =
//!   half of minfree;
//! - **soft updates on**: `newfs.c` sets `Uflag` for every UFS2 ("Use soft updates by default
//!   for UFS2 and above"), so a plain `newfs` gives `FS_DOSOFTDEP`. Journaled soft updates
//!   (`newfs -j`, what the installer uses) is not enabled: it needs a `.sujournal` file that
//!   `tunefs -j enable` / the kernel can add later;
//! - metadata check-hashes for superblock, cylinder groups and inodes (`FS_METACKHASH`,
//!   CRC32C), as FreeBSD ≥ 12 writes them;
//! - root directory (inode 2, 0755) with `.snap` (inode 3, 0775, group `operator` = 5), the
//!   recovery block in front of the superblock (`struct fsrecovery`) and a backup superblock
//!   in every cylinder group.
//!
//! Identity: `fs_volname` is the label limited to newfs's rules (`[A-Za-z0-9_-]`, at most 31
//! bytes; any other character becomes `_`). `fs_id` (what FreeBSD shows as `ufsid` and
//! `blkid` as the UUID) is the first 8 bytes of `MkfsOptions::uuid` when given, else the
//! creation time and a random number like `newfs`.
//!
//! Not known through a [`crate::block::BlockDev`]: the sector size (512 is assumed, so 4Kn
//! devices need a fragment ≥ 4096, i.e. a block ≥ 32 KiB) and the partition's position.
//! The first 64 KiB (boot area, old UFS1 superblock) are zeroed instead of being left as is.

use std::sync::Arc;

use crate::block::BlockDev;
use crate::forge::MkfsOptions;
use crate::i18n::t;
use crate::vfs::ext4_fs::crc32c_raw;
use crate::vfs::{FsError, FsResult};

// ─────────────────────────────── constants (fs.h, dinode.h, newfs.h) ───────────────────────────────

const DEV_BSIZE: u64 = 512;
const SBLOCK_UFS2: u64 = 65536;
const SBLOCKSIZE: u64 = 8192;
const FS_UFS2_MAGIC: u32 = 0x1954_0119;
const CG_MAGIC: u32 = 0x0009_0255;
const MINBSIZE: u32 = 4096;
const MAXBSIZE: u32 = 65536;
const MAXFRAG: u32 = 8;
const DFL_BLKSIZE: u32 = 32768;
/// `NFPI`: fragments per inode.
const NFPI: u64 = 2;
const MINFREE: i32 = 8;
const AVFILESIZ: u32 = 16384;
const AFPDIR: u32 = 64;
const MINCYLGRPS: u64 = 4;
const FS_MAXCONTIG: u32 = 16;
/// `MAXPHYS` of FreeBSD on 64-bit platforms (1 MiB since FreeBSD 13).
const MAXPHYS: u32 = 1024 * 1024;
const MAXBLKSPERCG: u64 = 0x7fff_ffff;
const CGSIZEFUDGE: u64 = 8;
const SIZEOF_FS: u64 = 1376;
const SIZEOF_CG: u64 = 168;
const SIZEOF_CSUM: u64 = 16;
const SIZEOF_DINODE: u64 = 256;
const UFS_ROOTINO: u32 = 2;
const UFS_NDADDR: u64 = 12;
const UFS_NIADDR: u64 = 3;
const DIRBLKSIZ: u64 = 512;
const MAXVOLLEN: usize = 32;
/// `operator` group of FreeBSD, owner group of `.snap`.
const OPERATOR_GID: u32 = 5;

const FS_DOSOFTDEP: i32 = 0x0002;
const FS_FLAGS_UPDATED: i8 = 0x80u8 as i8;
const FS_METACKHASH: i32 = 0x0200;
const CK_SUPERBLOCK: u32 = 0x0001;
const CK_CYLGRP: u32 = 0x0002;
const CK_INODE: u32 = 0x0004;
const FS_OPTTIME: i32 = 0;
const IFDIR: u16 = 0o040000;
const DT_DIR: u8 = 4;

// ───────────────────────────────────────── geometry ─────────────────────────────────────────

/// The superblock geometry `mkfs()` computes (all addresses / counts in fragments unless
/// stated).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub bsize: u32,
    pub fsize: u32,
    pub frag: u32,
    pub bshift: u32,
    pub fshift: u32,
    pub fragshift: u32,
    pub fsbtodb: u32,
    /// Filesystem size in fragments.
    pub size: u64,
    pub providersize: u64,
    pub sblkno: u64,
    pub cblkno: u64,
    pub iblkno: u64,
    pub dblkno: u64,
    pub ipg: u64,
    pub fpg: u64,
    pub ncg: u64,
    pub cgsize: u64,
    pub cssize: u64,
    pub csaddr: u64,
    pub sbsize: u64,
    pub metaspace: u64,
    pub maxbpg: u32,
    pub maxcontig: u32,
    pub contigsumsize: u32,
    pub maxfilesize: u64,
    pub nindir: u32,
    pub inopb: u32,
    pub dsize: u64,
    /// Totals before the root directory is created (what the backup superblocks record).
    pub nbfree: u64,
    pub nffree: u64,
    pub nifree: u64,
}

fn howmany(x: u64, y: u64) -> u64 {
    x.div_ceil(y)
}
fn roundup(x: u64, y: u64) -> u64 {
    x.div_ceil(y) * y
}

fn invalid(it: &str, en: &str) -> FsError {
    FsError::Invalid(t(it, en))
}

fn too_small(size: u64, bsize: u32) -> FsError {
    let min = min_bytes(bsize);
    let fmt = |b: u64| if b >= 1 << 20 { format!("{:.1} MiB", b as f64 / 1048576.0) } else { format!("{} KiB", b >> 10) };
    FsError::Invalid(t(
        &format!("UFS2 con blocchi da {} KiB richiede almeno {}: il dispositivo ha {}", bsize >> 10, fmt(min), fmt(size)),
        &format!("UFS2 with {} KiB blocks needs at least {}: the device has {}", bsize >> 10, fmt(min), fmt(size)),
    ))
}

/// Smallest device size (bytes) [`geometry`] accepts for `bsize`.
pub fn min_bytes(bsize: u32) -> u64 {
    let fsize = (bsize / MAXFRAG).max(DEV_BSIZE as u32) as u64;
    // Geometry is not strictly monotonic in the size, so scan up from a lower bound.
    let mut s = 0;
    while s <= 64 << 20 {
        if geometry_inner(s, bsize).is_ok() {
            return s;
        }
        s += fsize;
    }
    s
}

/// `CGSIZE(fs)` for UFS2 (no old rotational tables).
fn cgsize_of(ipg: u64, fpg: u64, frag: u64, contigsumsize: u32) -> u64 {
    let mut s = SIZEOF_CG + howmany(ipg, 8) + howmany(fpg, 8) + 4;
    if contigsumsize > 0 {
        s += contigsumsize as u64 * 4 + howmany(fpg / frag, 8);
    }
    s
}

/// Validate the block size and compute the layout for a device of `bytes` bytes.
pub fn geometry(bytes: u64, bsize: u32) -> FsResult<Geometry> {
    if !(MINBSIZE..=MAXBSIZE).contains(&bsize) || !bsize.is_power_of_two() {
        return Err(invalid(
            &format!("dimensione del blocco UFS2 non valida: {bsize} (ammesse 4096, 8192, 16384, 32768, 65536)"),
            &format!("invalid UFS2 block size: {bsize} (allowed: 4096, 8192, 16384, 32768, 65536)"),
        ));
    }
    geometry_inner(bytes, bsize).map_err(|e| match e {
        Inner::TooSmall => too_small(bytes, bsize),
        Inner::TooBig => invalid(
            "il dispositivo è troppo grande per questa dimensione del blocco UFS2: sceglierne una più grande",
            "the device is too large for this UFS2 block size: choose a larger one",
        ),
    })
}

#[derive(Debug)]
enum Inner {
    TooSmall,
    TooBig,
}

fn geometry_inner(bytes: u64, bsize: u32) -> Result<Geometry, Inner> {
    let fsize = bsize / MAXFRAG; // newfs -b B -f B/8; ≥ 512 since bsize ≥ 4096
    let frag = bsize / fsize;
    let (b, f, fr) = (bsize as u64, fsize as u64, frag as u64);
    let fshift = fsize.trailing_zeros();
    let fsbtodb = (fsize / DEV_BSIZE as u32).trailing_zeros();
    // fssize in DEV_BSIZE sectors, then in fragments (dbtofsb).
    let fssize = bytes / DEV_BSIZE;
    let size = fssize >> fsbtodb;
    if size == 0 {
        return Err(Inner::TooSmall);
    }
    let maxcontig = (MAXPHYS / bsize).max(1);
    let contigsumsize = if maxcontig > 1 { maxcontig.min(FS_MAXCONTIG) } else { 0 };
    let nindir = bsize / 8;
    let inopb = (b / SIZEOF_DINODE) as u32;
    let inopf = f / SIZEOF_DINODE; // INOPF = INOPB / frag
    let sblkno = roundup(howmany(SBLOCK_UFS2 + SBLOCKSIZE, f), fr);
    let cblkno = sblkno + roundup(howmany(SBLOCKSIZE, f), fr);
    let iblkno = cblkno + fr;
    let mut maxfilesize = b * UFS_NDADDR - 1;
    let mut sizepb = b;
    for _ in 0..UFS_NIADDR {
        sizepb = sizepb.saturating_mul(nindir as u64);
        maxfilesize = maxfilesize.saturating_add(sizepb);
    }

    // Inode density and frags per group (mkfs.c "retry:" … "Start packing").
    let ino = inopb as u64;
    let maxinum = (1u64 << 32) - ino;
    let minfragsperinode = 1 + size / maxinum;
    let density = NFPI.max(minfragsperinode) * f;
    let fragsperinode = (density >> fshift).max(1);
    if fragsperinode < minfragsperinode {
        // newfs would double the block size and start over.
        return Err(Inner::TooBig);
    }
    let cgsize = |ipg: u64, fpg: u64| cgsize_of(ipg, fpg, fr, contigsumsize);
    let ipg_for = |fpg: u64| roundup(howmany(fpg, fragsperinode), ino);
    let mut minfpg = fragsperinode * ino;
    if minfpg > size {
        minfpg = size;
    }
    let mut ipg = ino;
    let mut fpg = roundup(iblkno + ipg / inopf, fr).max(minfpg);
    ipg = ipg_for(fpg);
    fpg = roundup(iblkno + ipg / inopf, fr).max(minfpg);
    ipg = ipg_for(fpg);
    if cgsize(ipg, fpg) >= b - CGSIZEFUDGE {
        // mkfs lowers the density until the minimal group fits; with the default density and
        // MINBSIZE ≤ bsize this never happens.
        return Err(Inner::TooBig);
    }
    while fpg < MAXBLKSPERCG {
        ipg = ipg_for(fpg);
        if size / fpg < MINCYLGRPS {
            break;
        }
        let cs = cgsize(ipg, fpg);
        if cs < b - CGSIZEFUDGE {
            fpg += fr;
            continue;
        }
        if cs == b - CGSIZEFUDGE {
            break;
        }
        fpg -= fr;
        ipg = ipg_for(fpg);
        break;
    }
    // Make the last group big enough to hold its metadata (or exactly full).
    let ncg;
    loop {
        let n = howmany(size, fpg);
        let lastminfpg = roundup(iblkno + ipg / inopf, fr);
        if size < lastminfpg {
            return Err(Inner::TooSmall);
        }
        if size % fpg >= lastminfpg || size.is_multiple_of(fpg) {
            ncg = n;
            break;
        }
        if fpg <= lastminfpg + fr || fpg < 3 * fr {
            return Err(Inner::TooSmall);
        }
        fpg -= fr;
        ipg = ipg_for(fpg);
    }
    let cgsize_bytes = roundup(cgsize(ipg, fpg), f);
    let dblkno = iblkno + ipg / inopf;
    let csaddr = dblkno; // cgdmin(fs, 0)
    let cssize = roundup(ncg.checked_mul(SIZEOF_CSUM).ok_or(Inner::TooBig)?, f);
    let sbsize = roundup(SIZEOF_FS, f).clamp(DEV_BSIZE, SBLOCKSIZE);
    let metaspace = ((fpg * MINFREE as u64) / 200) & !(fr - 1); // blknum()
    let maxbpg = bsize / 8; // MAXBLKPG(bsize)
    let csfrags = howmany(cssize, f);

    // validate_sblock limits.
    if ipg.checked_mul(ncg).is_none_or(|n| n > (1u64 << 32) - ino) || ncg > u32::MAX as u64 || fpg > i32::MAX as u64 {
        return Err(Inner::TooBig);
    }
    if size < 8 * fr || fpg < 3 * fr {
        return Err(Inner::TooSmall);
    }
    // The summary area must stay inside cylinder group 0, and group 0 needs two free blocks
    // (root and .snap) after it.
    let cg0_ndblk = fpg.min(size);
    let cs_end = csaddr + csfrags;
    if cs_end > cg0_ndblk {
        return Err(if ncg > 1 { Inner::TooBig } else { Inner::TooSmall });
    }
    // cs_end ≤ cg0_ndblk, but rounding it up to a block can pass the end of a group whose
    // size is not a whole number of blocks: that leaves no free block at all.
    if cg0_ndblk.checked_sub(roundup(cs_end, fr)).is_none_or(|free| free / fr < 2) {
        return Err(Inner::TooSmall);
    }

    let dsize = size - sblkno - ncg * (dblkno - sblkno);
    let fragnum = |x: u64| x & (fr - 1);
    let nbfree = (dsize >> frag.trailing_zeros()) - howmany(csfrags, fr);
    let nffree = fragnum(size) + if fragnum(csfrags) > 0 { fr - fragnum(csfrags) } else { 0 };
    let nifree = ncg * ipg - UFS_ROOTINO as u64;

    Ok(Geometry {
        bsize,
        fsize,
        frag,
        bshift: bsize.trailing_zeros(),
        fshift,
        fragshift: frag.trailing_zeros(),
        fsbtodb,
        size,
        providersize: (bytes / DEV_BSIZE) >> fsbtodb,
        sblkno,
        cblkno,
        iblkno,
        dblkno,
        ipg,
        fpg,
        ncg,
        cgsize: cgsize_bytes,
        cssize,
        csaddr,
        sbsize,
        metaspace,
        maxbpg,
        maxcontig,
        contigsumsize,
        maxfilesize,
        nindir,
        inopb,
        dsize: dsize - csfrags,
        nbfree,
        nffree,
        nifree,
    })
}

impl Geometry {
    fn cgbase(&self, c: u64) -> u64 {
        self.fpg * c
    }
    fn cgsblock(&self, c: u64) -> u64 {
        self.cgbase(c) + self.sblkno
    }
    fn cgtod(&self, c: u64) -> u64 {
        self.cgbase(c) + self.cblkno
    }
    fn cgimin(&self, c: u64) -> u64 {
        self.cgbase(c) + self.iblkno
    }
    fn byte(&self, frag_addr: u64) -> u64 {
        frag_addr * self.fsize as u64
    }
    /// `cg_initediblk`: inodes newfs initialises in each group.
    fn initediblk(&self) -> u64 {
        self.ipg.min(2 * self.inopb as u64)
    }
}

// ───────────────────────────────────────── encoding ─────────────────────────────────────────

fn put16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// `calculate_crc32c(~0L, buf, len)` of FreeBSD's libkern: CRC32C without final inversion.
fn ckhash(data: &[u8]) -> u32 {
    crc32c_raw(!0, data)
}

/// The label as newfs accepts it: `[A-Za-z0-9_-]`, at most `MAXVOLLEN − 1` bytes.
pub fn volume_label(label: &str) -> String {
    label.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(MAXVOLLEN - 1).collect()
}

/// Superblock-wide totals (`struct csum_total`) and per-copy values.
#[derive(Clone, Copy, Debug, Default)]
struct Totals {
    ndir: u64,
    nbfree: u64,
    nifree: u64,
    nffree: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Params {
    pub label: String,
    pub fs_id: [u32; 2],
    /// Creation time (seconds since the epoch).
    pub time: u64,
    /// Seed of the inode generation numbers.
    pub seed: u64,
}

/// `struct fs` (1376 bytes, padded to `fs_sbsize`). Offsets from fs.h on LP64.
fn superblock(g: &Geometry, p: &Params, tot: Totals, actualloc: u64) -> Vec<u8> {
    let mut s = vec![0u8; g.sbsize as usize];
    put32(&mut s, 8, g.sblkno as u32);
    put32(&mut s, 12, g.cblkno as u32);
    put32(&mut s, 16, g.iblkno as u32);
    put32(&mut s, 20, g.dblkno as u32);
    // fs_old_cgoffset / old_cgmask / old_time / old_size / old_dsize stay 0 for UFS2.
    put32(&mut s, 44, g.ncg as u32);
    put32(&mut s, 48, g.bsize);
    put32(&mut s, 52, g.fsize);
    put32(&mut s, 56, g.frag);
    put32(&mut s, 60, MINFREE as u32);
    put32(&mut s, 72, !(g.bsize - 1)); // fs_bmask
    put32(&mut s, 76, !(g.fsize - 1)); // fs_fmask
    put32(&mut s, 80, g.bshift);
    put32(&mut s, 84, g.fshift);
    put32(&mut s, 88, g.maxcontig);
    put32(&mut s, 92, g.maxbpg);
    put32(&mut s, 96, g.fragshift);
    put32(&mut s, 100, g.fsbtodb);
    put32(&mut s, 104, g.sbsize as u32);
    put32(&mut s, 116, g.nindir);
    put32(&mut s, 120, g.inopb);
    put32(&mut s, 128, FS_OPTTIME as u32);
    put32(&mut s, 144, p.fs_id[0]);
    put32(&mut s, 148, p.fs_id[1]);
    put32(&mut s, 156, g.cssize as u32);
    put32(&mut s, 160, g.cgsize as u32);
    put32(&mut s, 184, g.ipg as u32);
    put32(&mut s, 188, g.fpg as u32);
    s[209] = 1; // fs_clean
    s[211] = FS_FLAGS_UPDATED as u8; // fs_old_flags
    let label = volume_label(&p.label);
    s[680..680 + label.len()].copy_from_slice(label.as_bytes()); // fs_volname
    put32(&mut s, 860, g.bsize); // fs_maxbsize
    put64(&mut s, 872, g.providersize);
    put64(&mut s, 880, g.metaspace);
    put64(&mut s, 992, actualloc); // fs_sblockactualloc
    put64(&mut s, 1000, SBLOCK_UFS2); // fs_sblockloc
    put64(&mut s, 1008, tot.ndir);
    put64(&mut s, 1016, tot.nbfree);
    put64(&mut s, 1024, tot.nifree);
    put64(&mut s, 1032, tot.nffree);
    put64(&mut s, 1072, p.time); // fs_time
    put64(&mut s, 1080, g.size);
    put64(&mut s, 1088, g.dsize);
    put64(&mut s, 1096, g.csaddr);
    put32(&mut s, 1196, AVFILESIZ);
    put32(&mut s, 1200, AFPDIR);
    put32(&mut s, 1308, CK_SUPERBLOCK | CK_CYLGRP | CK_INODE); // fs_metackhash
    put32(&mut s, 1312, (FS_DOSOFTDEP | FS_METACKHASH) as u32);
    put32(&mut s, 1316, g.contigsumsize);
    put32(&mut s, 1320, ((UFS_NDADDR + UFS_NIADDR) * 8) as u32); // fs_maxsymlinklen
    put64(&mut s, 1328, g.maxfilesize);
    put64(&mut s, 1336, (g.bsize - 1) as u64); // fs_qbmask
    put64(&mut s, 1344, (g.fsize - 1) as u64); // fs_qfmask
    put32(&mut s, 1372, FS_UFS2_MAGIC);
    // fs_ckhash (@1304): CRC32C of fs_sbsize bytes with the field zeroed.
    let h = ckhash(&s);
    put32(&mut s, 1304, h);
    s
}

/// A cylinder group block (`struct cg` + maps), built like `initcg()`.
struct Cg {
    buf: Vec<u8>,
    iusedoff: usize,
    freeoff: usize,
    clustersumoff: usize,
    clusteroff: usize,
}

impl Cg {
    fn cs_add(&mut self, field: usize, delta: i64) {
        // cg_cs at 24: ndir, nbfree, nifree, nffree (int32).
        let off = 24 + 4 * field;
        let v = get32(&self.buf, off) as i32 as i64 + delta;
        put32(&mut self.buf, off, v as i32 as u32);
    }
    fn frsum_add(&mut self, n: u64) {
        let off = 52 + 4 * n as usize;
        let v = get32(&self.buf, off) + 1;
        put32(&mut self.buf, off, v);
    }
    fn setbit(&mut self, base: usize, i: u64) {
        self.buf[base + (i / 8) as usize] |= 1 << (i % 8);
    }
    fn clrbit(&mut self, base: usize, i: u64) {
        self.buf[base + (i / 8) as usize] &= !(1 << (i % 8));
    }
    fn isset(&self, base: usize, i: u64) -> bool {
        self.buf[base + (i / 8) as usize] & (1 << (i % 8)) != 0
    }
    /// `setblock` / `isblock` / `clrblock`: all frags of block `h` in the free map.
    fn set_block(&mut self, frag: u64, h: u64, free: bool) {
        for i in h * frag..(h + 1) * frag {
            if free {
                self.setbit(self.freeoff, i)
            } else {
                self.clrbit(self.freeoff, i)
            }
        }
    }
    fn is_block(&self, frag: u64, h: u64) -> bool {
        (h * frag..(h + 1) * frag).all(|i| self.isset(self.freeoff, i))
    }
    fn seal(&mut self) {
        put32(&mut self.buf, 132, 0);
        let h = ckhash(&self.buf);
        put32(&mut self.buf, 132, h);
    }
    fn csum(&self) -> [u32; 4] {
        [get32(&self.buf, 24), get32(&self.buf, 28), get32(&self.buf, 32), get32(&self.buf, 36)]
    }
}

fn init_cg(g: &Geometry, c: u64, time: u64) -> Cg {
    let fr = g.frag as u64;
    let cbase = g.cgbase(c);
    let dmax = (cbase + g.fpg).min(g.size);
    let dlower = g.cgsblock(c) - cbase;
    let mut dupper = g.dblkno; // cgdmin − cbase
    if c == 0 {
        dupper += howmany(g.cssize, g.fsize as u64);
    }
    let ndblk = dmax - cbase;
    let iusedoff = SIZEOF_CG as usize;
    let freeoff = iusedoff + howmany(g.ipg, 8) as usize;
    let mut nextfreeoff = freeoff + howmany(g.fpg, 8) as usize;
    let (mut clustersumoff, mut clusteroff) = (0, 0);
    if g.contigsumsize > 0 {
        clustersumoff = roundup(nextfreeoff as u64, 4) as usize - 4;
        clusteroff = clustersumoff + (g.contigsumsize as usize + 1) * 4;
        nextfreeoff = clusteroff + howmany(g.fpg / fr, 8) as usize;
    }
    let mut cg = Cg { buf: vec![0u8; g.cgsize as usize], iusedoff, freeoff, clustersumoff, clusteroff };
    let b = &mut cg.buf;
    put32(b, 4, CG_MAGIC);
    put32(b, 12, c as u32); // cg_cgx
    put32(b, 20, ndblk as u32);
    put32(b, 92, iusedoff as u32);
    put32(b, 96, freeoff as u32);
    put32(b, 100, nextfreeoff as u32);
    put32(b, 104, clustersumoff as u32);
    put32(b, 108, clusteroff as u32);
    if g.contigsumsize > 0 {
        put32(b, 112, (ndblk / fr) as u32); // cg_nclusterblks
    }
    put32(b, 116, g.ipg as u32); // cg_niblk
    put32(b, 120, g.initediblk() as u32);
    put64(b, 136, time); // cg_time
    cg.cs_add(2, g.ipg as i64);
    if c == 0 {
        for i in 0..UFS_ROOTINO as u64 {
            cg.setbit(iusedoff, i);
            cg.cs_add(2, -1);
        }
    }
    if c > 0 {
        // Below the backup superblock: free data (the boot area exists only in group 0).
        let mut d = 0;
        while d < dlower {
            cg.set_block(fr, d / fr, true);
            if g.contigsumsize > 0 {
                cg.setbit(clusteroff, d / fr);
            }
            cg.cs_add(1, 1);
            d += fr;
        }
    }
    let i = dupper % fr;
    if i != 0 {
        cg.frsum_add(fr - i);
        let end = dupper + fr - i;
        while dupper < end {
            cg.setbit(freeoff, dupper);
            cg.cs_add(3, 1);
            dupper += 1;
        }
    }
    let mut d = dupper;
    while d + fr <= ndblk {
        cg.set_block(fr, d / fr, true);
        if g.contigsumsize > 0 {
            cg.setbit(clusteroff, d / fr);
        }
        cg.cs_add(1, 1);
        d += fr;
    }
    if d < ndblk {
        cg.frsum_add(ndblk - d);
        while d < ndblk {
            cg.setbit(freeoff, d);
            cg.cs_add(3, 1);
            d += 1;
        }
    }
    cluster_sums(g, &mut cg, ndblk / fr);
    cg
}

/// Count runs of free blocks into `cg_clustersum` (capped at `fs_contigsumsize`).
fn cluster_sums(g: &Geometry, cg: &mut Cg, nclusterblks: u64) {
    if g.contigsumsize == 0 {
        return;
    }
    for k in 1..=g.contigsumsize as usize {
        put32(&mut cg.buf, cg.clustersumoff + 4 * k, 0);
    }
    let mut run = 0u32;
    let mut sums = vec![0u32; g.contigsumsize as usize + 1];
    for i in 0..nclusterblks {
        if cg.isset(cg.clusteroff, i) {
            run += 1;
        } else if run != 0 {
            sums[run.min(g.contigsumsize) as usize] += 1;
            run = 0;
        }
    }
    if run != 0 {
        sums[run.min(g.contigsumsize) as usize] += 1;
    }
    for (k, v) in sums.iter().enumerate().skip(1) {
        put32(&mut cg.buf, cg.clustersumoff + 4 * k, *v);
    }
}

/// `alloc(fsize, IFDIR)` of mkfs.c: the first entirely free block of group 0, of which only
/// the first fragment is used. Returns its fragment address.
fn alloc_dir_frag(g: &Geometry, cg: &mut Cg, tot: &mut Totals) -> Option<u64> {
    let fr = g.frag as u64;
    let ndblk = get32(&cg.buf, 20) as u64;
    let mut d = 0;
    while d < ndblk {
        if cg.is_block(fr, d / fr) {
            break;
        }
        d += fr;
    }
    if d >= ndblk {
        return None;
    }
    let blkno = d / fr;
    cg.set_block(fr, blkno, false);
    if g.contigsumsize > 0 {
        cg.clrbit(cg.clusteroff, blkno);
    }
    cg.cs_add(1, -1);
    tot.nbfree -= 1;
    cg.cs_add(0, 1);
    tot.ndir += 1;
    if fr > 1 {
        cg.cs_add(3, fr as i64 - 1);
        tot.nffree += fr - 1;
        cg.frsum_add(fr - 1);
        for i in 1..fr {
            cg.setbit(cg.freeoff, d + i);
        }
    }
    // The cluster summary follows the cluster map (mkfs only clears the bit; fsck_ffs pass 5
    // recomputes the sums, so keep them consistent).
    let n = get32(&cg.buf, 112) as u64;
    cluster_sums(g, cg, n);
    Some(d)
}

/// A directory block of `DIRBLKSIZ` bytes (`makedir`), padded with zeros to a fragment.
fn make_dir(g: &Geometry, entries: &[(u32, &str)]) -> Vec<u8> {
    let mut b = vec![0u8; g.fsize as usize];
    let mut off = 0usize;
    for (k, (ino, name)) in entries.iter().enumerate() {
        let dirsiz = roundup(8 + name.len() as u64 + 1, 4) as usize;
        let reclen = if k + 1 == entries.len() { DIRBLKSIZ as usize - off } else { dirsiz };
        put32(&mut b, off, *ino);
        put16(&mut b, off + 4, reclen as u16);
        b[off + 6] = DT_DIR;
        b[off + 7] = name.len() as u8;
        b[off + 8..off + 8 + name.len()].copy_from_slice(name.as_bytes());
        off += reclen;
    }
    b
}

/// `struct ufs2_dinode` of a directory, with its check-hash (`ffs_update_dinode_ckhash`).
fn dir_inode(g: &Geometry, mode: u16, nlink: u16, gid: u32, block: u64, depth: u32, time: u64) -> [u8; 256] {
    let mut d = [0u8; 256];
    put16(&mut d, 0, IFDIR | mode);
    put16(&mut d, 2, nlink);
    put32(&mut d, 8, gid);
    put64(&mut d, 16, DIRBLKSIZ); // di_size
    put64(&mut d, 24, roundup(DIRBLKSIZ, g.fsize as u64) / DEV_BSIZE); // di_blocks
    for off in [32, 40, 48, 56] {
        put64(&mut d, off, time); // atime, mtime, ctime, birthtime
    }
    put64(&mut d, 112, block); // di_db[0]
    put32(&mut d, 240, depth); // di_dirdepth
    let h = ckhash(&d);
    put32(&mut d, 244, h);
    d
}

fn io(e: std::io::Error) -> FsError {
    FsError::Io(e.to_string())
}

/// xorshift64* for the inode generation numbers (`arc4random` in newfs; any value is valid).
fn next_rand(state: &mut u64) -> u32 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
}

/// Create the filesystem.
pub fn format(dev: Arc<dyn BlockDev>, opts: &MkfsOptions) -> FsResult<()> {
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let rnd = *uuid::Uuid::new_v4().as_bytes();
    let fs_id = match opts.uuid {
        // Big-endian words, so FreeBSD's ufsid (`%08x%08x`) shows the UUID's first 16 digits.
        Some(u) => [u32::from_be_bytes([u[0], u[1], u[2], u[3]]).max(1), u32::from_be_bytes([u[4], u[5], u[6], u[7]]).max(1)],
        None => [(time as u32).max(1), u32::from_le_bytes([rnd[0], rnd[1], rnd[2], rnd[3]]).max(1)],
    };
    let seed = u64::from_le_bytes([rnd[8], rnd[9], rnd[10], rnd[11], rnd[12], rnd[13], rnd[14], rnd[15]]) | 1;
    let params = Params { label: opts.label.clone(), fs_id, time, seed };
    format_with(dev.as_ref(), opts.cluster_size.unwrap_or(DFL_BLKSIZE), &params)
}

/// [`format`] with fixed time / id / generation seed (tests and comparisons).
#[doc(hidden)]
pub fn format_reproducible(dev: Arc<dyn BlockDev>, opts: &MkfsOptions, time: u64, fs_id: [u32; 2]) -> FsResult<()> {
    let params = Params { label: opts.label.clone(), fs_id, time, seed: 0x9E37_79B9_7F4A_7C15 };
    format_with(dev.as_ref(), opts.cluster_size.unwrap_or(DFL_BLKSIZE), &params)
}

pub(crate) fn format_with(dev: &dyn BlockDev, bsize: u32, p: &Params) -> FsResult<()> {
    let g = geometry(dev.size(), bsize)?;
    let mut rng = p.seed;

    // Boot area + old UFS1 superblock + primary superblock: zeroed first, so an interrupted
    // format never looks like a valid filesystem.
    dev.write_at(0, &vec![0u8; (SBLOCK_UFS2 + SBLOCKSIZE) as usize]).map_err(io)?;

    let mut tot = Totals { ndir: 0, nbfree: g.nbfree, nifree: g.nifree, nffree: g.nffree };
    let mut csums: Vec<[u32; 4]> = Vec::with_capacity(g.ncg as usize);
    let mut cg0 = None;
    let inode_bytes = (g.initediblk() * SIZEOF_DINODE) as usize;
    let mut inodes = vec![0u8; inode_bytes];
    for c in 0..g.ncg {
        let mut cg = init_cg(&g, c, p.time);
        csums.push(cg.csum());
        // Backup superblock of this group (pre-root totals, as newfs leaves them).
        let loc = g.byte(g.cgsblock(c));
        dev.write_at(loc, &superblock(&g, p, tot, loc)).map_err(io)?;
        // The first inodes, zeroed with fresh generation numbers.
        inodes.fill(0);
        for i in 0..g.initediblk() as usize {
            put32(&mut inodes, i * SIZEOF_DINODE as usize + 80, next_rand(&mut rng));
        }
        dev.write_at(g.byte(g.cgimin(c)), &inodes).map_err(io)?;
        if c == 0 {
            cg0 = Some(cg);
        } else {
            cg.seal();
            dev.write_at(g.byte(g.cgtod(c)), &cg.buf).map_err(io)?;
        }
    }
    let mut cg0 = cg0.ok_or_else(|| too_small(dev.size(), bsize))?;

    // Root directory and .snap (fsinit).
    let root_blk = alloc_dir_frag(&g, &mut cg0, &mut tot).ok_or_else(|| too_small(dev.size(), bsize))?;
    let snap_blk = alloc_dir_frag(&g, &mut cg0, &mut tot).ok_or_else(|| too_small(dev.size(), bsize))?;
    dev.write_at(g.byte(root_blk), &make_dir(&g, &[(UFS_ROOTINO, "."), (UFS_ROOTINO, ".."), (UFS_ROOTINO + 1, ".snap")])).map_err(io)?;
    dev.write_at(g.byte(snap_blk), &make_dir(&g, &[(UFS_ROOTINO + 1, "."), (UFS_ROOTINO, "..")])).map_err(io)?;
    for ino in [UFS_ROOTINO, UFS_ROOTINO + 1] {
        cg0.setbit(cg0.iusedoff, ino as u64);
        cg0.cs_add(2, -1);
        tot.nifree -= 1;
    }
    // Inodes 2 and 3 sit in the first inode block of group 0.
    let mut iblk = vec![0u8; g.bsize as usize];
    dev.read_at(g.byte(g.cgimin(0)), &mut iblk).map_err(io)?;
    let root = dir_inode(&g, 0o755, 3, 0, root_blk, 0, p.time);
    let snap = dir_inode(&g, 0o775, 2, OPERATOR_GID, snap_blk, 1, p.time);
    iblk[512..768].copy_from_slice(&root);
    iblk[768..1024].copy_from_slice(&snap);
    dev.write_at(g.byte(g.cgimin(0)), &iblk).map_err(io)?;
    csums[0] = cg0.csum();
    cg0.seal();
    dev.write_at(g.byte(g.cgtod(0)), &cg0.buf).map_err(io)?;

    // Summary area (struct csum per group) then the primary superblock and recovery data.
    let mut cs = vec![0u8; g.cssize as usize];
    for (i, v) in csums.iter().enumerate() {
        for (k, x) in v.iter().enumerate() {
            put32(&mut cs, i * 16 + 4 * k, *x);
        }
    }
    dev.write_at(g.byte(g.csaddr), &cs).map_err(io)?;
    dev.flush().map_err(io)?;

    // struct fsrecovery in the last 20 bytes before the superblock.
    let mut rec = vec![0u8; DEV_BSIZE as usize];
    let r = DEV_BSIZE as usize - 20;
    put32(&mut rec, r, FS_UFS2_MAGIC);
    put32(&mut rec, r + 4, g.fsbtodb);
    put32(&mut rec, r + 8, g.sblkno as u32);
    put32(&mut rec, r + 12, g.fpg as u32);
    put32(&mut rec, r + 16, g.ncg as u32);
    dev.write_at(SBLOCK_UFS2 - DEV_BSIZE, &rec).map_err(io)?;
    dev.write_at(SBLOCK_UFS2, &superblock(&g, p, tot, SBLOCK_UFS2)).map_err(io)?;
    dev.flush().map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemDev;

    fn params() -> Params {
        Params { label: "LumenUFS".into(), fs_id: [0x0123_4567, 0x89ab_cdef], time: 1_700_000_000, seed: 42 }
    }
    fn get64(b: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
    }

    #[test]
    fn geometry_defaults() {
        let g = geometry(1 << 30, 32768).unwrap();
        assert_eq!((g.bsize, g.fsize, g.frag, g.fsbtodb), (32768, 4096, 8, 3));
        assert_eq!((g.sblkno, g.cblkno, g.iblkno), (24, 32, 40));
        assert_eq!((g.maxcontig, g.contigsumsize, g.maxbpg), (32, 16, 4096));
        assert_eq!(g.size, (1 << 30) / 4096);
        assert!(g.ncg >= 4);
        assert!(cgsize_of(g.ipg, g.fpg, 8, 16) <= 32768 - 8);
        assert_eq!(g.dblkno, g.iblkno + g.ipg / 16);
        // Sizes and offsets stay within the format's limits for every block size.
        for bs in [4096, 8192, 16384, 32768, 65536] {
            for bytes in [min_bytes(bs), 3 << 20, 64 << 20, (777 << 20) + 12345, 50 << 30, 2 << 40] {
                let g = match geometry(bytes, bs) {
                    Ok(g) => g,
                    Err(e) => panic!("{bs} {bytes}: {e}"),
                };
                assert!(g.size <= g.ncg * g.fpg && g.size > (g.ncg - 1) * g.fpg, "{bs} {bytes}");
                assert!(cgsize_of(g.ipg, g.fpg, g.frag as u64, g.contigsumsize) <= g.bsize as u64);
                assert!(g.ipg % g.inopb as u64 == 0);
                let last = g.size - (g.ncg - 1) * g.fpg;
                assert!(last >= g.dblkno, "{bs} {bytes}: last group too small");
                assert!(g.csaddr + g.cssize / g.fsize as u64 <= g.fpg.min(g.size));
            }
        }
    }

    #[test]
    fn validation_and_minimum() {
        for bs in [0, 512, 2048, 3000, 131072] {
            assert!(matches!(geometry(1 << 30, bs), Err(FsError::Invalid(_))), "{bs}");
        }
        for bs in [4096u32, 32768, 65536] {
            let min = min_bytes(bs);
            assert!(min > 0 && min < 8 << 20, "{bs}: {min}");
            geometry(min, bs).unwrap();
            match geometry(min - 1, bs) {
                Err(FsError::Invalid(m)) => assert!(m.contains("UFS2"), "{m}"),
                other => panic!("{other:?}"),
            }
            for s in [0, 1, 512, 65536, 100_000] {
                assert!(geometry(s, bs).is_err());
            }
        }
        // Huge but unreasonable for small blocks: a clear error, not a panic.
        assert!(geometry(u64::MAX, 4096).is_err());
        assert!(geometry(u64::MAX, 65536).is_err());
    }

    #[test]
    fn labels() {
        assert_eq!(volume_label("My Disk.1"), "My_Disk_1");
        assert_eq!(volume_label("ab-c_D9"), "ab-c_D9");
        assert_eq!(volume_label("è"), "_");
        assert_eq!(volume_label(&"x".repeat(40)).len(), 31);
    }

    #[test]
    fn writes_consistent_structures() {
        let size = 96 << 20;
        let dev = MemDev::from_vec(vec![0x5A; size]);
        format_with(&dev, 32768, &params()).unwrap();
        let img = dev.snapshot();
        let g = geometry(size as u64, 32768).unwrap();
        let sb = &img[65536..65536 + g.sbsize as usize];
        assert_eq!(get32(sb, 1372), FS_UFS2_MAGIC);
        let mut z = sb.to_vec();
        put32(&mut z, 1304, 0);
        assert_eq!(get32(sb, 1304), ckhash(&z), "superblock check-hash");
        assert_eq!(&sb[680..688], b"LumenUFS");
        assert_eq!((get32(sb, 144), get32(sb, 148)), (0x0123_4567, 0x89ab_cdef));
        assert_eq!(get32(sb, 1312) & FS_DOSOFTDEP as u32, FS_DOSOFTDEP as u32);
        assert!(img[..65024].iter().all(|&b| b == 0));
        assert_eq!(get32(&img, 65516), FS_UFS2_MAGIC);

        // Totals of the primary superblock = sum of the cylinder groups = summary area.
        let (mut ndir, mut nbfree, mut nifree, mut nffree) = (0u64, 0u64, 0u64, 0u64);
        for c in 0..g.ncg {
            let off = g.byte(g.cgtod(c)) as usize;
            let cg = &img[off..off + g.cgsize as usize];
            assert_eq!(get32(cg, 4), CG_MAGIC);
            let mut z = cg.to_vec();
            put32(&mut z, 132, 0);
            assert_eq!(get32(cg, 132), ckhash(&z), "cg {c} check-hash");
            let cs = g.byte(g.csaddr) as usize + 16 * c as usize;
            assert_eq!(&img[cs..cs + 16], &cg[24..40], "csum of cg {c}");
            // Free-block count = full free blocks in the map.
            let freeoff = get32(cg, 96) as usize;
            let ndblk = get32(cg, 20) as u64;
            let full = (0..ndblk / 8).filter(|h| cg[freeoff + *h as usize] == 0xff).count() as u32;
            assert_eq!(get32(cg, 28), full, "cg {c} nbfree");
            ndir += get32(cg, 24) as u64;
            nbfree += get32(cg, 28) as u64;
            nifree += get32(cg, 32) as u64;
            nffree += get32(cg, 36) as u64;
            // Backup superblock with its own location.
            let bs = g.byte(g.cgsblock(c)) as usize;
            assert_eq!(get32(&img, bs + 1372), FS_UFS2_MAGIC);
            assert_eq!(get64(&img, bs + 992), bs as u64);
        }
        assert_eq!((get64(sb, 1008), get64(sb, 1016), get64(sb, 1024), get64(sb, 1032)), (ndir, nbfree, nifree, nffree));
        assert_eq!(ndir, 2);
        assert_eq!(nifree, g.ncg * g.ipg - 4);

        // Root inode → its directory block.
        let ioff = g.byte(g.cgimin(0)) as usize;
        let root = &img[ioff + 512..ioff + 768];
        assert_eq!(u16::from_le_bytes([root[0], root[1]]), 0o040755);
        let mut z = root.to_vec();
        put32(&mut z, 244, 0);
        assert_eq!(get32(root, 244), ckhash(&z), "inode check-hash");
        let blk = get64(root, 112);
        let d = &img[g.byte(blk) as usize..g.byte(blk) as usize + 512];
        assert_eq!((get32(d, 0), d[7], d[8]), (2, 1, b'.'));
        assert_eq!(&d[24 + 8..24 + 13], b".snap");
        assert_eq!(u16::from_le_bytes([d[24 + 4], d[24 + 5]]), 488);
        let snap = &img[ioff + 768..ioff + 1024];
        assert_eq!(u16::from_le_bytes([snap[0], snap[1]]), 0o040775);
        assert_eq!(get32(snap, 8), OPERATOR_GID);
    }

    #[test]
    fn every_block_size_and_odd_sizes() {
        for (bs, size) in [(4096u32, (5 << 20) + 777), (8192, 7 << 20), (16384, (13 << 20) + 4097), (65536, 20 << 20)] {
            let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(size));
            format(dev.clone(), &MkfsOptions { label: "t".into(), cluster_size: Some(bs), uuid: Some([0xAB; 16]) }).unwrap();
            let mut sb = vec![0u8; 1376];
            dev.read_at(65536, &mut sb).unwrap();
            assert_eq!(get32(&sb, 48), bs);
            assert_eq!(get32(&sb, 52), bs / 8);
            assert_eq!(get32(&sb, 144), 0xABAB_ABAB);
        }
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(100_000));
        assert!(format(dev, &MkfsOptions::default()).is_err());
    }

    #[test]
    fn no_panic_on_any_size() {
        // Small devices of every size class: a filesystem or a clean error.
        for bs in [4096u32, 8192, 16384, 32768, 65536] {
            let mut s = 0usize;
            while s < (3 << 20) {
                let dev = MemDev::new(s);
                if let Ok(()) = format_with(&dev, bs, &params()) {
                    assert!(s as u64 >= min_bytes(bs), "{bs} {s}");
                }
                s += 37 * 512 + 1;
            }
        }
        // Huge sizes: geometry only (no device that large here).
        for bs in [4096u32, 32768, 65536] {
            for shift in 30..64 {
                let _ = geometry((1u64 << shift) - 1, bs);
                let _ = geometry(1u64 << shift, bs);
            }
        }
    }
}
