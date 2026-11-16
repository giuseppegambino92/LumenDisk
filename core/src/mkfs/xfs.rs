//! mkfs.xfs: a fresh, empty XFS v5 filesystem, equivalent to what `mkfs.xfs` (xfsprogs 6.13)
//! creates with its defaults on a single device without a stripe geometry.
//!
//! On-disk format references: the kernel's `fs/xfs/libxfs/xfs_format.h`, `xfs_log_format.h`,
//! `xfs_da_format.h`, xfsprogs `mkfs/xfs_mkfs.c` (geometry, log size, AG headers) and
//! `libxfs/rdwr.c` (`libxfs_log_clear`), and the "XFS Algorithms & Data Structures" book.
//! Every integer is big-endian except the CRC fields, which are crc32c stored little-endian.
//!
//! Features (the 6.13 defaults, as printed by `mkfs.xfs -N`): crc (v5), finobt, sparse inodes,
//! rmapbt, reflink, bigtime, inobtcount, nrext64, ftype, lazy-count, attr2, projid32bit,
//! log v2. Not enabled, like 6.13: exchange-range, metadir, parent pointers, realtime.
//! 512-byte inodes, directory blocks of max(4096, block size), internal log.
//!
//! Layout of every allocation group (AG), in sector / block units of the AG:
//! - sectors 0..3: superblock (primary in AG 0, secondary copies elsewhere), AGF, AGI, AGFL;
//! - next blocks: root blocks of the by-block and by-size free space btrees, the inode and
//!   free inode btrees, the reverse mapping btree and the refcount btree;
//! - AG `agcount / 2` then holds the internal log;
//! - then the blocks of the AG free list (AGFL), and in AG 0 the first inode chunk (root
//!   directory + the realtime bitmap / summary inodes, required even without realtime);
//! - the rest is free space.

use std::sync::Arc;

use crate::block::BlockDev;
use crate::forge::MkfsOptions;
use crate::i18n::t;
use crate::vfs::ext4_fs::crc32c_raw;
use crate::vfs::{FsError, FsResult};

#[cfg(test)]
mod tests;

/// `mkfs.xfs` refuses filesystems smaller than 300 MiB ("Filesystem must be larger than 300MB").
pub const MIN_DEV_SIZE: u64 = 300 << 20;
pub const DEFAULT_BLOCKSIZE: u32 = 4096;
pub const INODE_SIZE: u32 = 512;
pub const LABEL_MAX: usize = 12;

const MIB: u64 = 1 << 20;
const BBSIZE: u64 = 512;
pub const NULLAGBLOCK: u32 = u32::MAX;
pub const NULLAGINO: u32 = u32::MAX;
pub const NULLFSINO: u64 = u64::MAX;
const INODES_PER_CHUNK: u64 = 64;
/// XFS_INODE_BIG_CLUSTER_SIZE (8 KiB) scaled by inode size / 256 on v5 filesystems.
const INODE_CLUSTER_SIZE: u64 = 8192 * (INODE_SIZE as u64 / 256);

// ── magic numbers ──
pub const SB_MAGIC: u32 = 0x5846_5342; // "XFSB"
pub const AGF_MAGIC: u32 = 0x5841_4746; // "XAGF"
pub const AGI_MAGIC: u32 = 0x5841_4749; // "XAGI"
pub const AGFL_MAGIC: u32 = 0x5841_464c; // "XAFL"
pub const BNO_MAGIC: u32 = 0x4142_3342; // "AB3B"
pub const CNT_MAGIC: u32 = 0x4142_3343; // "AB3C"
pub const INO_MAGIC: u32 = 0x4941_4233; // "IAB3"
pub const FINO_MAGIC: u32 = 0x4649_4233; // "FIB3"
pub const RMAP_MAGIC: u32 = 0x524d_4233; // "RMB3"
pub const REFC_MAGIC: u32 = 0x5233_4643; // "R3FC"
pub const DINODE_MAGIC: u16 = 0x494e; // "IN"
const LOG_MAGIC: u32 = 0xFEED_BABE;

// ── superblock feature bits ──
const VERSION_5: u16 = 5;
const VERSION_NLINKBIT: u16 = 0x0020;
const VERSION_ALIGNBIT: u16 = 0x0080;
const VERSION_LOGV2BIT: u16 = 0x0400;
const VERSION_SECTORBIT: u16 = 0x0800;
const VERSION_EXTFLGBIT: u16 = 0x1000;
const VERSION_DIRV2BIT: u16 = 0x2000;
const VERSION_MOREBITSBIT: u16 = 0x8000;
const VERSION2_LAZYSBCOUNT: u32 = 0x0002;
const VERSION2_ATTR2: u32 = 0x0008;
const VERSION2_PROJID32: u32 = 0x0080;
const VERSION2_CRC: u32 = 0x0100;
pub const RO_COMPAT_FINOBT: u32 = 1 << 0;
pub const RO_COMPAT_RMAPBT: u32 = 1 << 1;
pub const RO_COMPAT_REFLINK: u32 = 1 << 2;
pub const RO_COMPAT_INOBTCNT: u32 = 1 << 3;
pub const INCOMPAT_FTYPE: u32 = 1 << 0;
pub const INCOMPAT_SPINODES: u32 = 1 << 1;
pub const INCOMPAT_BIGTIME: u32 = 1 << 3;
pub const INCOMPAT_NREXT64: u32 = 1 << 5;

// ── inode flags ──
const DIFLAG_NEWRTBM: u16 = 1 << 2;
const DIFLAG2_BIGTIME: u64 = 1 << 3;
const DIFLAG2_NREXT64: u64 = 1 << 4;
const DINODE_FMT_LOCAL: u8 = 1;
const DINODE_FMT_EXTENTS: u8 = 2;

// ── reverse mapping owners (XFS_RMAP_OWN_*) ──
const OWN_FS: u64 = -3i64 as u64;
const OWN_LOG: u64 = -4i64 as u64;
const OWN_AG: u64 = -5i64 as u64;
const OWN_INOBT: u64 = -6i64 as u64;
const OWN_INODES: u64 = -7i64 as u64;
const OWN_REFC: u64 = -8i64 as u64;

/// Short-form btree block header with CRC (struct xfs_btree_block, XFS_BTREE_SBLOCK_CRC_LEN).
pub const BTREE_SHDR_LEN: usize = 56;
/// Size of the v3 inode core (struct xfs_dinode up to di_uuid).
const DINODE_CORE_V3: usize = 176;

/// Smallest internal log the kernel accepts for 32 and 64 KiB blocks, which can exceed the
/// 64 MiB `mkfs.xfs` uses by default (for blocks up to 16 KiB the 64 MiB default is always
/// larger than the minimum). The minimum (`xfs_log_calc_minimum_size()`) depends on the
/// heights of the per-AG btrees, hence on the AG size; these are the values `mkfs.xfs` 6.13
/// reports for this feature set ("minimum size is N blocks"), as (first AG size in blocks
/// where the value applies, minimum log blocks), for 512 / 4096-byte sectors.
const MIN_LOG_32K_512: &[(u64, u64)] = &[(0, 2018), (1364, 2213), (4089, 2605), (2_785_973, 2801), (5_571_945, 3192)];
const MIN_LOG_32K_4096: &[(u64, u64)] = &[(0, 2049), (1364, 2244), (4089, 2634), (2_785_973, 2832), (5_571_945, 3222)];
const MIN_LOG_64K_512: &[(u64, u64)] = &[(0, 1984), (2729, 2180), (8185, 2570), (11_162_977, 2766)];
const MIN_LOG_64K_4096: &[(u64, u64)] = &[(0, 2028), (2729, 2223), (8185, 2613), (11_162_977, 2808)];
/// XFS_MIN_LOG_BLOCKS, XFS_MIN_LOG_BYTES, XFS_MAX_LOG_BLOCKS, XFS_MAX_LOG_BYTES.
const MIN_LOG_BLOCKS: u64 = 512;
const MIN_LOG_BYTES: u64 = 10 * MIB;
const MAX_LOG_BLOCKS: u64 = 1024 * 1024;
const MAX_LOG_BYTES: u64 = 2048 * MIB - MIN_LOG_BYTES;
/// XFS_MIN_REALISTIC_LOG_BLOCKS: 64 MiB.
const REALISTIC_LOG_BYTES: u64 = 64 * MIB;

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_be_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}
fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_be_bytes());
}

/// crc32c of `buf` with its 4-byte CRC field at `off` taken as zero, stored there
/// little-endian (`xfs_update_cksum`).
fn seal(buf: &mut [u8], off: usize) {
    buf[off..off + 4].fill(0);
    let c = !crc32c_raw(!0, buf);
    buf[off..off + 4].copy_from_slice(&c.to_le_bytes());
}

/// Verify a CRC written by [`seal`].
pub fn crc_ok(buf: &[u8], off: usize) -> bool {
    let mut tmp = buf.to_vec();
    let stored = [tmp[off], tmp[off + 1], tmp[off + 2], tmp[off + 3]];
    seal(&mut tmp, off);
    tmp[off..off + 4] == stored
}

fn invalid(it: &str, en: &str) -> FsError {
    FsError::Invalid(t(it, en))
}

fn log2(v: u64) -> u8 {
    63 - v.leading_zeros() as u8
}

/// `xfs_highbit32(v - 1) + 1`: log2 rounded up.
fn log2_roundup(v: u64) -> u8 {
    if v <= 1 {
        0
    } else {
        64 - (v - 1).leading_zeros() as u8
    }
}

/// Bigtime timestamp: nanoseconds since 1901-12-13 (Unix time + 2^31 s).
fn bigtime(sec: u64, nsec: u32) -> u64 {
    (sec + (1u64 << 31)).saturating_mul(1_000_000_000).saturating_add(nsec as u64)
}

fn now() -> (u64, u32) {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs(), d.subsec_nanos()),
        Err(_) => (0, 0),
    }
}

/// Geometry of one filesystem.
#[derive(Clone, Debug)]
pub struct Geometry {
    pub blocksize: u64,
    pub blocklog: u8,
    pub sectsize: u64,
    pub sectlog: u8,
    pub dblocks: u64,
    pub agsize: u64,
    pub agcount: u64,
    pub agblklog: u8,
    pub inopblock: u64,
    pub inopblog: u8,
    pub dirblklog: u8,
    pub imaxpct: u8,
    pub logblocks: u64,
    pub logagno: u64,
    /// Log start as an AG block of `logagno`.
    pub log_agbno: u64,
    pub inoalignmt: u64,
    pub spino_align: u64,
    pub rextsize: u64,
    /// AG block of the AGFL sector's block + 1: the first btree root (by-block free space).
    pub bno_block: u64,
    /// First block after the btree roots (`xfs_prealloc_blocks`).
    pub prealloc: u64,
    /// Blocks on each AG's free list (`xfs_alloc_min_freelist` with one-level trees).
    pub agfl_count: u64,
    /// Inode chunk: AG 0 block and length in blocks; inodes it holds.
    pub chunk_agbno: u64,
    pub chunk_blocks: u64,
    pub chunk_inodes: u64,
}

impl Geometry {
    pub fn new(dev_size: u64, blocksize: u32, sectsize: u32) -> FsResult<Self> {
        if !blocksize.is_power_of_two() || !(1024..=65536).contains(&blocksize) {
            return Err(invalid(
                &format!("dimensione blocco XFS non valida: {blocksize} (potenza di 2 tra 1024 e 65536)"),
                &format!("invalid XFS block size: {blocksize} (a power of 2 between 1024 and 65536)"),
            ));
        }
        if sectsize != 512 && sectsize != 4096 {
            return Err(invalid(&format!("settore non supportato: {sectsize} byte"), &format!("unsupported sector size: {sectsize} bytes")));
        }
        if blocksize < sectsize {
            return Err(invalid(
                &format!("il blocco XFS ({blocksize}) non può essere più piccolo del settore del disco ({sectsize})"),
                &format!("the XFS block ({blocksize}) can't be smaller than the disk sector ({sectsize})"),
            ));
        }
        let b = blocksize as u64;
        let blocklog = log2(b);
        let sectlog = log2(sectsize as u64);
        let mut dblocks = dev_size / b;
        if dblocks.checked_mul(b).map_or(true, |v| v < MIN_DEV_SIZE) {
            return Err(too_small(MIN_DEV_SIZE));
        }
        // Allocation groups: calc_default_ag_geometry() for a single disk.
        let ag_max = ((1u64 << 40) - 1) >> blocklog;
        let ag_min = (16 * MIB) >> blocklog;
        // (Single disk: from 4 TiB on, the maximum AG size; from 128 MiB, 4 AGs.)
        let agsize = if dblocks >= (4u64 << 40) >> blocklog {
            ag_max
        } else if dblocks >= (128 * MIB) >> blocklog {
            let shift = 2; // XFS_NOMULTIDISK_AGLOG: 4 AGs
            let mut blocks = dblocks >> shift;
            if dblocks & ((1 << shift) - 1) != 0 && blocks < ag_max {
                blocks += 1;
            }
            blocks
        } else {
            return Err(too_small(MIN_DEV_SIZE));
        };
        let mut agcount = dblocks / agsize + u64::from(dblocks % agsize != 0);
        // A last AG smaller than the minimum is dropped with its blocks.
        if dblocks % agsize != 0 && dblocks % agsize < ag_min {
            agcount -= 1;
            dblocks = agcount * agsize;
        }
        let agblklog = log2_roundup(agsize);
        let inopblock = b / INODE_SIZE as u64;
        let inopblog = log2(inopblock);
        let dirblklog = log2(b.max(4096)) - blocklog;
        let imaxpct = if dblocks < (1u64 << 40) >> blocklog {
            25
        } else if dblocks < (50u64 << 40) >> blocklog {
            5
        } else {
            1
        };
        // Inode alignment with sparse inodes: chunks aligned to their own size, sparse
        // allocations to the inode cluster.
        let spino_align = INODE_CLUSTER_SIZE >> blocklog;
        let inoalignmt = (INODES_PER_CHUNK * INODE_SIZE as u64) >> blocklog;
        let rextsize = (4096 / b).max(1);

        // AG header sectors, then one root block per btree (bno, cnt, ino, fino, rmap, refc).
        let agfl_block = 3 * sectsize as u64 / b;
        let bno_block = agfl_block + 1;
        let prealloc = bno_block + 6;

        // AG free list: xfs_alloc_min_freelist() for one-level trees:
        // 2 * (min(2, alloc_maxlevels) * 2 - 2) + (min(2, rmap_maxlevels) * 2 - 2).
        let alloc_mnr = [(b - BTREE_SHDR_LEN as u64) / 8 / 2, (b - BTREE_SHDR_LEN as u64) / 12 / 2];
        let alloc_maxlevels = compute_maxlevels(&alloc_mnr, (agsize + 1) / 2);
        let rmap_mnr = [(b - BTREE_SHDR_LEN as u64) / 24 / 2, (b - BTREE_SHDR_LEN as u64) / 44 / 2];
        let rmap_maxlevels = space_to_height(&rmap_mnr, agsize);
        let agfl_count = 2 * (alloc_maxlevels.min(2) * 2 - 2) + (rmap_maxlevels.min(2) * 2 - 2);

        // Log size: calculate_log_size() + clamp_internal_log_size().
        let mut minlog = MIN_LOG_BLOCKS.max(min_log_blocks(blocksize, sectsize, agsize));
        if dblocks >= (1u64 << 30) >> blocklog {
            minlog = minlog.max(MIN_LOG_BYTES >> blocklog);
        }
        let mut logblocks = ((dblocks as u128 * b as u128 / 2048) >> blocklog) as u64;
        logblocks = logblocks.max(REALISTIC_LOG_BYTES >> blocklog).max(minlog);
        // xfs_alloc_ag_max_usable() - 1: AG headers, AGFL reserve, the six btree roots.
        let headers = (4 * sectsize as u64).div_ceil(b);
        let max_logblocks = agsize.saturating_sub(headers + 4 + 3 + 3 + 1);
        if max_logblocks < minlog {
            return Err(invalid(
                &format!("filesystem troppo piccolo per blocchi XFS da {blocksize} byte: il log richiede almeno {minlog} blocchi"),
                &format!("filesystem too small for {blocksize}-byte XFS blocks: the log needs at least {minlog} blocks"),
            ));
        }
        logblocks = logblocks.min(max_logblocks).min(MAX_LOG_BLOCKS);
        if logblocks << blocklog > MAX_LOG_BYTES {
            logblocks = MAX_LOG_BYTES >> blocklog;
        }
        let logagno = agcount / 2;
        let log_agbno = prealloc;

        // Root inode chunk in AG 0: after the headers (and the log if it is there) and the
        // AGFL, aligned to the chunk alignment.
        let chunk_blocks = ((INODES_PER_CHUNK * INODE_SIZE as u64) >> blocklog).max(1);
        let chunk_inodes = chunk_blocks * inopblock;
        let ag0_first_free = prealloc + if logagno == 0 { logblocks } else { 0 } + agfl_count;
        let align = inoalignmt.max(1);
        let chunk_agbno = ag0_first_free.div_ceil(align) * align;

        let g = Geometry {
            blocksize: b,
            blocklog,
            sectsize: sectsize as u64,
            sectlog,
            dblocks,
            agsize,
            agcount,
            agblklog,
            inopblock,
            inopblog,
            dirblklog,
            imaxpct,
            logblocks,
            logagno,
            log_agbno,
            inoalignmt,
            spino_align,
            rextsize,
            bno_block,
            prealloc,
            agfl_count,
            chunk_agbno,
            chunk_blocks,
            chunk_inodes,
        };
        // Everything the layout puts in an AG must fit it (the last AG can be shorter).
        if g.ag_len(g.agcount - 1) < g.prealloc + g.agfl_count + 1
            || g.ag_len(logagno) < g.log_agbno + g.logblocks + g.agfl_count
            || g.ag_len(0) < g.chunk_agbno + g.chunk_blocks
            || g.agblklog as u32 + g.inopblog as u32 > 32
        {
            return Err(too_small(MIN_DEV_SIZE));
        }
        Ok(g)
    }

    /// Blocks in AG `agno` (the last one may be shorter).
    pub fn ag_len(&self, agno: u64) -> u64 {
        if agno + 1 == self.agcount {
            self.dblocks - agno * self.agsize
        } else {
            self.agsize
        }
    }

    /// Byte offset of AG block `agbno` of AG `agno` (AGs are `agsize` blocks apart).
    pub fn byte(&self, agno: u64, agbno: u64) -> u64 {
        (agno * self.agsize + agbno) << self.blocklog
    }

    /// 512-byte disk address of an AG block (`XFS_AGB_TO_DADDR`).
    pub fn daddr(&self, agno: u64, agbno: u64) -> u64 {
        self.byte(agno, agbno) / BBSIZE
    }

    /// Root directory inode number (first inode of the chunk in AG 0).
    pub fn rootino(&self) -> u64 {
        self.chunk_agbno << self.inopblog
    }

    /// sb_logstart: an `xfs_fsblock_t`, i.e. segmented as (agno << agblklog) | agbno — not a
    /// linear block number (they only coincide when the AG size is a power of two).
    pub fn logstart(&self) -> u64 {
        (self.logagno << self.agblklog) | self.log_agbno
    }

    /// Byte length of the AGFL's block-number array (`xfs_agfl_size`).
    pub fn agfl_size(&self) -> u64 {
        (self.sectsize - 36) / 4
    }

    /// sb_logsunit in bytes: 1 ("none") with 512-byte sectors; otherwise the log sector
    /// rounded up to a whole block (it must be a multiple of the block size), like mkfs.xfs.
    pub fn logsunit(&self) -> u64 {
        if self.sectsize > 512 {
            self.sectsize.max(self.blocksize)
        } else {
            1
        }
    }

    /// Log record length in 512-byte units: one log stripe unit, or 2 without one.
    fn log_record_bbs(&self) -> u64 {
        if self.sectsize > 512 {
            self.logsunit() / BBSIZE
        } else {
            2
        }
    }
}

/// `xfs_btree_compute_maxlevels()`: height of a btree holding `records` with minimal fill.
fn compute_maxlevels(mnr: &[u64; 2], records: u64) -> u64 {
    let mut level_blocks = records.div_ceil(mnr[0].max(1));
    let mut height = 1;
    while level_blocks > 1 {
        level_blocks = level_blocks.div_ceil(mnr[1].max(2));
        height += 1;
    }
    height
}

/// `xfs_btree_space_to_height()`: height of a btree that could use `leaf_blocks` blocks.
fn space_to_height(mnr: &[u64; 2], leaf_blocks: u64) -> u64 {
    if leaf_blocks < 1 {
        return 0;
    }
    let mut node_blocks: u64 = 2;
    let mut blocks_left = leaf_blocks - 1;
    let mut height = 1;
    while node_blocks < blocks_left {
        blocks_left -= node_blocks;
        node_blocks = node_blocks.saturating_mul(mnr[1].max(2));
        height += 1;
    }
    height
}

/// Kernel minimum log size (blocks) for blocks of 32 / 64 KiB; 0 for smaller blocks, whose
/// 64 MiB default log is always above the minimum.
fn min_log_blocks(blocksize: u32, sectsize: u32, agsize: u64) -> u64 {
    let table = match (blocksize, sectsize) {
        (32768, 512) => MIN_LOG_32K_512,
        (32768, _) => MIN_LOG_32K_4096,
        (65536, 512) => MIN_LOG_64K_512,
        (65536, _) => MIN_LOG_64K_4096,
        _ => return 0,
    };
    table.iter().rev().find(|(from, _)| agsize >= *from).map(|e| e.1).unwrap_or(0)
}

fn too_small(min: u64) -> FsError {
    invalid(
        &format!("dispositivo troppo piccolo per XFS: servono almeno {} MiB", min.div_ceil(MIB)),
        &format!("device too small for XFS: at least {} MiB are needed", min.div_ceil(MIB)),
    )
}

/// Per-run values that aren't geometry.
#[derive(Clone, Debug)]
pub struct Params {
    pub geo: Geometry,
    pub uuid: [u8; 16],
    pub label: [u8; LABEL_MAX],
    pub time: (u64, u32),
    /// di_gen of the free inodes of the chunk and of the rt inodes.
    pub gens: [u32; 3],
}

impl Params {
    pub fn new(dev_size: u64, opts: &MkfsOptions, sectsize: u32) -> FsResult<Self> {
        let geo = Geometry::new(dev_size, opts.cluster_size.unwrap_or(DEFAULT_BLOCKSIZE), sectsize)?;
        let mut label = [0u8; LABEL_MAX];
        let mut n = 0;
        for c in opts.label.chars().filter(|c| *c != '\0') {
            let mut tmp = [0u8; 4];
            let s = c.encode_utf8(&mut tmp).as_bytes();
            if n + s.len() > LABEL_MAX {
                break;
            }
            label[n..n + s.len()].copy_from_slice(s);
            n += s.len();
        }
        let r = *uuid::Uuid::new_v4().as_bytes();
        Ok(Params {
            geo,
            uuid: opts.uuid.unwrap_or_else(|| *uuid::Uuid::new_v4().as_bytes()),
            label,
            time: now(),
            gens: [
                u32::from_le_bytes([r[0], r[1], r[2], r[3]]),
                u32::from_le_bytes([r[4], r[5], r[6], r[7]]),
                u32::from_le_bytes([r[8], r[9], r[10], r[11]]),
            ],
        })
    }
}

/// A free extent of an AG: (AG block, length).
type Extent = (u64, u64);

/// What one AG contains besides its headers: free extents and reverse mappings.
struct AgLayout {
    agfl: Vec<u64>,
    free: Vec<Extent>,
    rmaps: Vec<(u64, u64, u64)>, // (start, len, owner)
    has_chunk: bool,
}

fn ag_layout(g: &Geometry, agno: u64) -> AgLayout {
    let len = g.ag_len(agno);
    let mut rmaps = vec![
        (0, g.bno_block, OWN_FS),
        (g.bno_block, 2, OWN_AG),       // bnobt + cntbt roots
        (g.bno_block + 2, 2, OWN_INOBT), // inobt + finobt roots
        (g.bno_block + 4, 1, OWN_AG),   // rmapbt root
        (g.bno_block + 5, 1, OWN_REFC), // refcountbt root
    ];
    let mut next = g.prealloc;
    if agno == g.logagno {
        rmaps.push((g.log_agbno, g.logblocks, OWN_LOG));
        next = g.log_agbno + g.logblocks;
    }
    let agfl: Vec<u64> = (next..next + g.agfl_count).collect();
    rmaps.push((next, g.agfl_count, OWN_AG));
    next += g.agfl_count;
    let mut free = Vec::new();
    let has_chunk = agno == 0;
    if has_chunk {
        if g.chunk_agbno > next {
            free.push((next, g.chunk_agbno - next));
        }
        rmaps.push((g.chunk_agbno, g.chunk_blocks, OWN_INODES));
        next = g.chunk_agbno + g.chunk_blocks;
    }
    if len > next {
        free.push((next, len - next));
    }
    AgLayout { agfl, free, rmaps, has_chunk }
}

/// Inode btree records of the root chunk: (start agino, free mask) per 64 inodes.
fn inobt_records(g: &Geometry) -> Vec<(u64, u64)> {
    let first = g.rootino();
    (0..g.chunk_inodes / INODES_PER_CHUNK)
        .map(|i| {
            let start = first + i * INODES_PER_CHUNK;
            // Inodes 0, 1, 2 of the first record are in use (root, rbm, rsum).
            let free = if i == 0 { !0u64 << 3 } else { !0u64 };
            (start, free)
        })
        .collect()
}

/// One btree root block (short form, CRC) with `recs` already encoded.
fn btree_block(p: &Params, magic: u32, agno: u64, agbno: u64, recs: &[Vec<u8>]) -> Vec<u8> {
    let g = &p.geo;
    let mut b = vec![0u8; g.blocksize as usize];
    put32(&mut b, 0, magic);
    put16(&mut b, 4, 0); // level
    put16(&mut b, 6, recs.len() as u16);
    put32(&mut b, 8, NULLAGBLOCK); // leftsib
    put32(&mut b, 12, NULLAGBLOCK); // rightsib
    put64(&mut b, 16, g.daddr(agno, agbno));
    put64(&mut b, 24, 0); // lsn
    b[32..48].copy_from_slice(&p.uuid);
    put32(&mut b, 48, agno as u32); // owner: the AG
    let mut o = BTREE_SHDR_LEN;
    for r in recs {
        b[o..o + r.len()].copy_from_slice(r);
        o += r.len();
    }
    seal(&mut b, 52);
    b
}

fn alloc_rec(start: u64, len: u64) -> Vec<u8> {
    let mut r = vec![0u8; 8];
    put32(&mut r, 0, start as u32);
    put32(&mut r, 4, len as u32);
    r
}

fn inobt_rec(start: u64, free: u64) -> Vec<u8> {
    let mut r = vec![0u8; 16];
    put32(&mut r, 0, start as u32);
    put16(&mut r, 4, 0); // holemask: no holes
    r[6] = INODES_PER_CHUNK as u8; // count
    r[7] = free.count_ones() as u8; // freecount
    put64(&mut r, 8, free);
    r
}

fn rmap_rec(start: u64, len: u64, owner: u64) -> Vec<u8> {
    let mut r = vec![0u8; 24];
    put32(&mut r, 0, start as u32);
    put32(&mut r, 4, len as u32);
    put64(&mut r, 8, owner);
    put64(&mut r, 16, 0); // offset / flags
    r
}

// struct xfs_dsb field offsets.
pub const SB_BLOCKSIZE: usize = 4;
pub const SB_DBLOCKS: usize = 8;
pub const SB_UUID: usize = 32;
pub const SB_LOGSTART: usize = 48;
pub const SB_ROOTINO: usize = 56;
pub const SB_RBMINO: usize = 64;
pub const SB_RSUMINO: usize = 72;
pub const SB_AGBLOCKS: usize = 84;
pub const SB_AGCOUNT: usize = 88;
pub const SB_LOGBLOCKS: usize = 96;
pub const SB_VERSIONNUM: usize = 100;
pub const SB_SECTSIZE: usize = 102;
pub const SB_FNAME: usize = 108;
pub const SB_INPROGRESS: usize = 126;
pub const SB_ICOUNT: usize = 128;
pub const SB_IFREE: usize = 136;
pub const SB_FDBLOCKS: usize = 144;
pub const SB_FEATURES2: usize = 200;
pub const SB_RO_COMPAT: usize = 212;
pub const SB_INCOMPAT: usize = 216;
pub const SB_CRC: usize = 224;

/// Superblock. `primary`: the one in AG 0; secondaries are written like `mkfs.xfs` 6.13
/// leaves them (in-progress flag set, no counters or rt inodes, root inode only in the last
/// and middle AGs).
fn superblock(p: &Params, agno: u64, fdblocks: u64) -> Vec<u8> {
    let g = &p.geo;
    let primary = agno == 0;
    let mut b = vec![0u8; g.sectsize as usize];
    put32(&mut b, 0, SB_MAGIC);
    put32(&mut b, SB_BLOCKSIZE, g.blocksize as u32);
    put64(&mut b, SB_DBLOCKS, g.dblocks);
    put64(&mut b, 16, 0); // rblocks
    put64(&mut b, 24, 0); // rextents
    b[SB_UUID..SB_UUID + 16].copy_from_slice(&p.uuid);
    put64(&mut b, SB_LOGSTART, g.logstart());
    let root_here = primary || agno + 1 == g.agcount || (g.agcount > 2 && agno == (g.agcount - 1) / 2);
    put64(&mut b, SB_ROOTINO, if root_here { g.rootino() } else { NULLFSINO });
    put64(&mut b, SB_RBMINO, if primary { g.rootino() + 1 } else { NULLFSINO });
    put64(&mut b, SB_RSUMINO, if primary { g.rootino() + 2 } else { NULLFSINO });
    put32(&mut b, 80, g.rextsize as u32);
    put32(&mut b, SB_AGBLOCKS, g.agsize as u32);
    put32(&mut b, SB_AGCOUNT, g.agcount as u32);
    put32(&mut b, 92, 0); // rbmblocks
    put32(&mut b, SB_LOGBLOCKS, g.logblocks as u32);
    let mut version = VERSION_5 | VERSION_NLINKBIT | VERSION_ALIGNBIT | VERSION_LOGV2BIT | VERSION_EXTFLGBIT | VERSION_DIRV2BIT | VERSION_MOREBITSBIT;
    if g.sectsize != 512 {
        version |= VERSION_SECTORBIT;
    }
    put16(&mut b, SB_VERSIONNUM, version);
    put16(&mut b, SB_SECTSIZE, g.sectsize as u16);
    put16(&mut b, 104, INODE_SIZE as u16);
    put16(&mut b, 106, g.inopblock as u16);
    b[SB_FNAME..SB_FNAME + LABEL_MAX].copy_from_slice(&p.label);
    b[120] = g.blocklog;
    b[121] = g.sectlog;
    b[122] = log2(INODE_SIZE as u64);
    b[123] = g.inopblog;
    b[124] = g.agblklog;
    b[125] = 0; // rextslog
    b[SB_INPROGRESS] = u8::from(!primary);
    b[127] = g.imaxpct;
    let icount = if primary { g.chunk_inodes } else { 0 };
    put64(&mut b, SB_ICOUNT, icount);
    put64(&mut b, SB_IFREE, if primary { icount - 3 } else { 0 });
    put64(&mut b, SB_FDBLOCKS, if primary { fdblocks } else { fdblocks + g.chunk_blocks });
    put64(&mut b, 152, 0); // frextents
    // uquotino 160, gquotino 168, qflags 176, flags 178, shared_vn 179: 0
    put32(&mut b, 180, g.inoalignmt as u32);
    // unit 184, width 188: no stripe geometry
    b[192] = g.dirblklog;
    if g.sectsize != 512 {
        b[193] = g.sectlog; // logsectlog
        put16(&mut b, 194, g.sectsize as u16); // logsectsize
    }
    put32(&mut b, 196, g.logsunit() as u32);
    let f2 = VERSION2_LAZYSBCOUNT | VERSION2_ATTR2 | VERSION2_PROJID32 | VERSION2_CRC;
    put32(&mut b, SB_FEATURES2, f2);
    put32(&mut b, 204, f2); // bad_features2
    put32(&mut b, 208, 0); // compat
    put32(&mut b, SB_RO_COMPAT, RO_COMPAT_FINOBT | RO_COMPAT_RMAPBT | RO_COMPAT_REFLINK | RO_COMPAT_INOBTCNT);
    put32(&mut b, SB_INCOMPAT, INCOMPAT_FTYPE | INCOMPAT_SPINODES | INCOMPAT_BIGTIME | INCOMPAT_NREXT64);
    put32(&mut b, 220, 0); // log_incompat
    put32(&mut b, 228, g.spino_align as u32);
    // pquotino 232, lsn 240, meta_uuid 248 (unused without the meta_uuid feature): 0
    seal(&mut b, SB_CRC);
    b
}

fn agf(p: &Params, agno: u64, l: &AgLayout) -> Vec<u8> {
    let g = &p.geo;
    let mut b = vec![0u8; g.sectsize as usize];
    put32(&mut b, 0, AGF_MAGIC);
    put32(&mut b, 4, 1); // versionnum
    put32(&mut b, 8, agno as u32);
    put32(&mut b, 12, g.ag_len(agno) as u32);
    put32(&mut b, 16, g.bno_block as u32); // bno root
    put32(&mut b, 20, (g.bno_block + 1) as u32); // cnt root
    put32(&mut b, 24, (g.bno_block + 4) as u32); // rmap root
    put32(&mut b, 28, 1); // bno level
    put32(&mut b, 32, 1); // cnt level
    put32(&mut b, 36, 1); // rmap level
    put32(&mut b, 40, 1); // flfirst
    put32(&mut b, 44, l.agfl.len() as u32); // fllast
    put32(&mut b, 48, l.agfl.len() as u32); // flcount
    put32(&mut b, 52, l.free.iter().map(|e| e.1).sum::<u64>() as u32); // freeblks
    put32(&mut b, 56, l.free.iter().map(|e| e.1).max().unwrap_or(0) as u32); // longest
    put32(&mut b, 60, 0); // btreeblks
    b[64..80].copy_from_slice(&p.uuid);
    put32(&mut b, 80, 1); // rmap_blocks
    put32(&mut b, 84, 1); // refcount_blocks
    put32(&mut b, 88, (g.bno_block + 5) as u32); // refcount root
    put32(&mut b, 92, 1); // refcount level
    seal(&mut b, 216);
    b
}

fn agi(p: &Params, agno: u64, l: &AgLayout) -> Vec<u8> {
    let g = &p.geo;
    let mut b = vec![0u8; g.sectsize as usize];
    put32(&mut b, 0, AGI_MAGIC);
    put32(&mut b, 4, 1);
    put32(&mut b, 8, agno as u32);
    put32(&mut b, 12, g.ag_len(agno) as u32);
    let (count, free) = if l.has_chunk { (g.chunk_inodes, g.chunk_inodes - 3) } else { (0, 0) };
    put32(&mut b, 16, count as u32);
    put32(&mut b, 20, (g.bno_block + 2) as u32); // inobt root
    put32(&mut b, 24, 1); // level
    put32(&mut b, 28, free as u32);
    put32(&mut b, 32, if l.has_chunk { g.rootino() as u32 } else { NULLAGINO }); // newino
    put32(&mut b, 36, NULLAGINO); // dirino
    for i in 0..64 {
        put32(&mut b, 40 + 4 * i, NULLAGINO); // unlinked buckets
    }
    b[296..312].copy_from_slice(&p.uuid);
    put32(&mut b, 328, (g.bno_block + 3) as u32); // finobt root
    put32(&mut b, 332, 1); // finobt level
    put32(&mut b, 336, 1); // inobt blocks
    put32(&mut b, 340, 1); // finobt blocks
    seal(&mut b, 312);
    b
}

fn agfl(p: &Params, agno: u64, l: &AgLayout) -> Vec<u8> {
    let g = &p.geo;
    let mut b = vec![0u8; g.sectsize as usize];
    put32(&mut b, 0, AGFL_MAGIC);
    put32(&mut b, 4, agno as u32);
    b[8..24].copy_from_slice(&p.uuid);
    for i in 0..g.agfl_size() as usize {
        put32(&mut b, 36 + 4 * i, NULLAGBLOCK);
    }
    // Entries 1..=n, like mkfs.xfs (flfirst = 1).
    for (i, bno) in l.agfl.iter().enumerate() {
        put32(&mut b, 36 + 4 * (i + 1), *bno as u32);
    }
    seal(&mut b, 32);
    b
}

/// One v3 inode. `kind`: None = free.
fn inode(p: &Params, ino: u64, kind: Option<InodeKind>, gen: u32) -> Vec<u8> {
    let mut b = vec![0u8; INODE_SIZE as usize];
    put16(&mut b, 0, DINODE_MAGIC);
    b[4] = 3; // di_version
    put32(&mut b, 92, gen);
    put32(&mut b, 96, NULLAGINO); // next_unlinked
    put64(&mut b, 152, ino);
    b[160..176].copy_from_slice(&p.uuid);
    if let Some(k) = kind {
        let (s, ns) = p.time;
        let t = bigtime(s, ns);
        let (mode, format, nlink, flags) = match k {
            InodeKind::RootDir => (0o040755u16, DINODE_FMT_LOCAL, 2u32, 0u16),
            InodeKind::RtBitmap => (0o100000, DINODE_FMT_EXTENTS, 1, DIFLAG_NEWRTBM),
            InodeKind::RtSummary => (0o100000, DINODE_FMT_EXTENTS, 1, 0),
        };
        put16(&mut b, 2, mode);
        b[5] = format;
        put32(&mut b, 16, nlink);
        // The rt bitmap's atime is its allocation sequence number: 0.
        put64(&mut b, 32, if k == InodeKind::RtBitmap { bigtime(0, 0) } else { t }); // atime
        put64(&mut b, 40, t); // mtime
        put64(&mut b, 48, t); // ctime
        b[83] = DINODE_FMT_EXTENTS; // aformat
        put16(&mut b, 90, flags);
        put64(&mut b, 104, 1); // changecount
        put64(&mut b, 120, DIFLAG2_BIGTIME | DIFLAG2_NREXT64);
        put64(&mut b, 144, t); // crtime
        if k == InodeKind::RootDir {
            // Empty short-form directory: count 0, i8count 0, 4-byte parent = itself.
            let o = DINODE_CORE_V3;
            b[o] = 0;
            b[o + 1] = 0;
            put32(&mut b, o + 2, ino as u32);
            put64(&mut b, 56, 6); // di_size
        }
    }
    seal(&mut b, 100);
    b
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InodeKind {
    RootDir,
    RtBitmap,
    RtSummary,
}

/// The inode chunk: root directory, rt bitmap, rt summary, then free inodes.
fn inode_chunk(p: &Params) -> Vec<u8> {
    let g = &p.geo;
    let root = g.rootino();
    let mut out = Vec::with_capacity((g.chunk_blocks * g.blocksize) as usize);
    for i in 0..g.chunk_inodes {
        let (kind, gen) = match i {
            0 => (Some(InodeKind::RootDir), 0),
            1 => (Some(InodeKind::RtBitmap), p.gens[1]),
            2 => (Some(InodeKind::RtSummary), p.gens[2]),
            _ => (None, p.gens[0]),
        };
        out.extend_from_slice(&inode(p, root + i, kind, gen));
    }
    out
}

/// The first record of the internal log: a header and an unmount record at cycle 1,
/// as `libxfs_log_clear(..., XLOG_INIT_CYCLE)` writes it (`h_crc` stays 0, like mkfs).
fn log_record(p: &Params) -> Vec<u8> {
    let g = &p.geo;
    let bbs = g.log_record_bbs();
    // A record header covers 32 KiB of record (XLOG_HEADER_CYCLE_SIZE); bigger log stripe
    // units need extended headers, one 512-byte block each, right after the first one.
    let sunit = if g.sectsize > 512 { g.logsunit() } else { 0 };
    let hdrs = sunit.div_ceil(32768).max(1);
    let mut b = vec![0u8; (bbs * BBSIZE) as usize];
    let lsn: u64 = 1 << 32; // cycle 1, block 0
    put32(&mut b, 0, LOG_MAGIC);
    put32(&mut b, 4, 1); // h_cycle
    put32(&mut b, 8, 2); // h_version
    put32(&mut b, 12, ((bbs - hdrs) * BBSIZE) as u32); // h_len
    put64(&mut b, 16, lsn);
    put64(&mut b, 24, lsn); // h_tail_lsn
    put32(&mut b, 32, 0); // h_crc
    put32(&mut b, 36, u32::MAX); // h_prev_block = -1
    put32(&mut b, 40, 1); // h_num_logops
    put32(&mut b, 44, 0xb0c0_d0d0); // h_cycle_data[0]: the word the cycle replaced
    put32(&mut b, 300, 1); // h_fmt: XLOG_FMT_LINUX_LE
    b[304..320].copy_from_slice(&p.uuid);
    put32(&mut b, 320, sunit.max(32768) as u32); // h_size: max(stripe unit, XLOG_BIG_RECORD_BSIZE)
    // Extended headers: xh_cycle, the rest zero.
    for i in 1..hdrs as usize {
        put32(&mut b, i * BBSIZE as usize, 1);
    }
    // Unmount record (xlog_op_header + 8 data bytes) in the block after the headers, its
    // first word replaced by the cycle number.
    let o = (hdrs * BBSIZE) as usize;
    put32(&mut b, o, 1); // cycle (was oh_tid 0xb0c0d0d0)
    put32(&mut b, o + 4, 8); // oh_len
    b[o + 8] = 0xAA; // oh_clientid: XFS_LOG
    b[o + 9] = 0x20; // oh_flags: XLOG_UNMOUNT_TRANS
    b[o + 12..o + 14].copy_from_slice(&0x556eu16.to_le_bytes()); // XLOG_UNMOUNT_TYPE, host order
    // Remaining blocks of the record: zero, stamped with the cycle.
    for i in hdrs as usize + 1..bbs as usize {
        put32(&mut b, i * BBSIZE as usize, 1);
    }
    b
}

/// A laid-out filesystem. The per-AG structures are generated on demand ([`Image::ag_writes`])
/// so that devices with thousands of AGs never hold them all in memory.
pub struct Image {
    pub params: Params,
    /// sb_fdblocks: free blocks + AGFL blocks of every AG (= every block that isn't AG
    /// headers / btree roots, log or the inode chunk).
    pub fdblocks: u64,
}

pub fn build(dev_size: u64, opts: &MkfsOptions, sectsize: u32) -> FsResult<Image> {
    let p = Params::new(dev_size, opts, sectsize)?;
    let g = &p.geo;
    let fdblocks = g.dblocks - g.agcount * g.prealloc - g.logblocks - g.chunk_blocks;
    Ok(Image { fdblocks, params: p })
}

impl Image {
    /// The primary superblock (one sector at offset 0; written last).
    pub fn primary(&self) -> Vec<u8> {
        superblock(&self.params, 0, self.fdblocks)
    }

    /// Log: byte offset, byte length (to zero) and its first record.
    pub fn log(&self) -> (u64, u64, Vec<u8>) {
        let g = &self.params.geo;
        (g.byte(g.logagno, g.log_agbno), g.logblocks << g.blocklog, log_record(&self.params))
    }

    /// Headers, btree roots (and the inode chunk in AG 0) of AG `agno`, as (byte offset,
    /// bytes). AG 0's primary superblock sector is left out (see [`Image::primary`]).
    pub fn ag_writes(&self, agno: u64) -> Vec<(u64, Vec<u8>)> {
        let p = &self.params;
        let g = &p.geo;
        let l = &ag_layout(g, agno);
        let mut writes = Vec::new();
        // Header block(s): sb, agf, agi, agfl sectors, the rest of the block zeroed.
        let mut hdr = vec![0u8; (g.bno_block * g.blocksize) as usize];
        let s = g.sectsize as usize;
        if agno != 0 {
            hdr[..s].copy_from_slice(&superblock(p, agno, self.fdblocks));
        }
        hdr[s..2 * s].copy_from_slice(&agf(p, agno, l));
        hdr[2 * s..3 * s].copy_from_slice(&agi(p, agno, l));
        hdr[3 * s..4 * s].copy_from_slice(&agfl(p, agno, l));
        if agno == 0 {
            writes.push((g.byte(0, 0) + s as u64, hdr[s..].to_vec()));
        } else {
            writes.push((g.byte(agno, 0), hdr));
        }
        let mut by_bno: Vec<Extent> = l.free.clone();
        by_bno.sort();
        let mut by_cnt = by_bno.clone();
        by_cnt.sort_by_key(|e| (e.1, e.0));
        let inos = if l.has_chunk { inobt_records(&g) } else { Vec::new() };
        let mut rmaps = l.rmaps.clone();
        rmaps.sort();
        let blocks = [
            btree_block(&p, BNO_MAGIC, agno, g.bno_block, &by_bno.iter().map(|e| alloc_rec(e.0, e.1)).collect::<Vec<_>>()),
            btree_block(&p, CNT_MAGIC, agno, g.bno_block + 1, &by_cnt.iter().map(|e| alloc_rec(e.0, e.1)).collect::<Vec<_>>()),
            btree_block(&p, INO_MAGIC, agno, g.bno_block + 2, &inos.iter().map(|r| inobt_rec(r.0, r.1)).collect::<Vec<_>>()),
            btree_block(&p, FINO_MAGIC, agno, g.bno_block + 3, &inos.iter().filter(|r| r.1 != 0).map(|r| inobt_rec(r.0, r.1)).collect::<Vec<_>>()),
            btree_block(&p, RMAP_MAGIC, agno, g.bno_block + 4, &rmaps.iter().map(|r| rmap_rec(r.0, r.1, r.2)).collect::<Vec<_>>()),
            btree_block(&p, REFC_MAGIC, agno, g.bno_block + 5, &[]),
        ];
        writes.push((g.byte(agno, g.bno_block), blocks.concat()));
        if l.has_chunk {
            writes.push((g.byte(0, g.chunk_agbno), inode_chunk(p)));
        }
        writes
    }
}

fn write_zeros(dev: &dyn BlockDev, mut off: u64, len: u64) -> FsResult<()> {
    let chunk = vec![0u8; MIB as usize];
    let end = off + len;
    while off < end {
        let n = (end - off).min(MIB) as usize;
        dev.write_at(off, &chunk[..n])?;
        off += n as u64;
    }
    Ok(())
}

fn write_image(dev: &dyn BlockDev, img: &Image) -> FsResult<()> {
    let g = &img.params.geo;
    let size = dev.size();
    // Old signatures: the first and the last MiB (sector-aligned at the end).
    write_zeros(dev, 0, MIB.min(size))?;
    let tail = size.saturating_sub(MIB) / g.sectsize * g.sectsize;
    if tail >= MIB {
        write_zeros(dev, tail, size - tail)?;
    }
    // The log must read as zeros except for its first record.
    let (log_off, log_len, rec) = img.log();
    write_zeros(dev, log_off, log_len)?;
    dev.write_at(log_off, &rec)?;
    for agno in 0..g.agcount {
        for (off, buf) in img.ag_writes(agno) {
            dev.write_at(off, &buf)?;
        }
    }
    dev.flush()?;
    dev.write_at(0, &img.primary())?;
    dev.flush()?;
    Ok(())
}

pub fn format(dev: Arc<dyn BlockDev>, opts: &MkfsOptions) -> FsResult<()> {
    format_with_sector(dev, opts, 512)
}

/// Like [`format`] for a device whose logical sector is `sectsize` bytes (512 or 4096): XFS
/// records it, and the kernel refuses a filesystem whose sector is smaller than the device's.
pub fn format_with_sector(dev: Arc<dyn BlockDev>, opts: &MkfsOptions, sectsize: u32) -> FsResult<()> {
    if dev.read_only() {
        return Err(FsError::ReadOnly);
    }
    let img = build(dev.size(), opts, sectsize)?;
    write_image(dev.as_ref(), &img)
}
