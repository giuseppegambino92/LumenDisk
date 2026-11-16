//! mkfs.f2fs — a fresh, empty F2FS, laid out exactly like `mkfs.f2fs` 1.16 with its defaults.
//!
//! Reference: f2fs-tools 1.16.0 (`mkfs/f2fs_format.c`, `include/f2fs_fs.h`, `lib/libf2fs.c`) and
//! the kernel's `include/linux/f2fs_fs.h`. Every number below (area sizes, overprovisioning,
//! current segments, checkpoint fields) follows the same arithmetic as `mkfs.f2fs` so a
//! LumenDisk volume and a `mkfs.f2fs` volume of the same size have the same geometry.
//!
//! Defaults matched (what `mkfs.f2fs` 1.16 does with no option, as shipped by Debian):
//! - 4 KiB blocks, 512 blocks (2 MiB) per segment, 1 segment per section, 1 section per zone;
//! - no optional feature (`feature = 0`: no extra_attr, quota, encryption, checksums…);
//! - overprovisioning ratio chosen by `get_best_overprovision`;
//! - "normal" (non heap) allocation: hot / warm / cold node logs in segments 0, 1, 2, hot data
//!   in 3, cold data around 1/4 and warm data around 1/2 of the main area;
//! - default media / hot extension lists in the superblock;
//! - root directory (inode 3, mode 0755, uid / gid 0) with one dentry block (`.` and `..`).
//!
//! Differences from `mkfs.f2fs` (none affects the kernel or `fsck.f2fs`):
//! - the partition's start sector is not known through a [`BlockDev`], so the first segment is
//!   aligned to 2 MiB from the start of the device / partition (as `mkfs.f2fs` does on an image
//!   file), with 512-byte logical sectors;
//! - there is no discard: the areas the kernel reads before trusting them (first SIT copy,
//!   first NAT copy, both checkpoint segments, the first warm node block) are zeroed instead,
//!   and the checkpoint does not claim `CP_TRIMMED_FLAG`;
//! - the space in front of segment 0 is zeroed too, so no stale signature (ISO 9660, an old
//!   partition boot sector…) survives next to the F2FS superblocks;
//! - the "kernel version" strings in the superblock name LumenDisk instead of `uname`.
//!
//! `MkfsOptions::cluster_size` has no meaning here: F2FS supports only 4 KiB blocks, and
//! segment / section sizes are fixed by the defaults above, so it is ignored.

use std::sync::Arc;

use crate::block::BlockDev;
use crate::forge::MkfsOptions;
use crate::i18n::t;
use crate::vfs::{FsError, FsResult};

// ───────────────────────────── on-disk constants (f2fs_fs.h) ─────────────────────────────

const F2FS_SUPER_MAGIC: u32 = 0xF2F5_2010;
/// Version written by f2fs-tools 1.16 (`F2FS_MAJOR_VERSION` / `F2FS_MINOR_VERSION`).
const MAJOR_VER: u16 = 1;
const MINOR_VER: u16 = 16;
const BLKSIZE: u64 = 4096;
const LOG_BLOCKSIZE: u32 = 12;
const SECTOR_SIZE: u64 = 512;
const LOG_SECTORSIZE: u32 = 9;
const LOG_SECTORS_PER_BLOCK: u32 = 3;
const BLKS_PER_SEG: u64 = 512;
const LOG_BLKS_PER_SEG: u32 = 9;
const SEGMENT_BYTES: u64 = BLKSIZE * BLKS_PER_SEG;
/// Superblock offset inside blocks 0 and 1.
const SUPER_OFFSET: usize = 1024;
const SB_SIZE: usize = 3072;
/// `NR_CURSEG_TYPE`: three data logs and three node logs.
const NR_CURSEG_TYPE: u32 = 6;
const F2FS_NUMBER_OF_CHECKPOINT_PACK: u32 = 2;
/// `F2FS_MIN_SEGMENTS`: SB + 2 × (CP + SIT + NAT) + SSA + MAIN.
const F2FS_MIN_SEGMENTS: u64 = 9;
/// `F2FS_MAX_SEGMENT`: block addresses are 32-bit, so at most 16 TiB.
const F2FS_MAX_SEGMENT: u64 = 16 * 1024 * 1024 / 2;
/// `sizeof(struct f2fs_sit_entry)` = 74 → 55 entries per block.
const SIT_ENTRY_SIZE: usize = 74;
const SIT_ENTRY_PER_BLOCK: u64 = BLKSIZE / SIT_ENTRY_SIZE as u64;
/// `sizeof(struct f2fs_nat_entry)` = 9 → 455 entries per block.
const NAT_ENTRY_SIZE: usize = 9;
const NAT_ENTRY_PER_BLOCK: u64 = BLKSIZE / NAT_ENTRY_SIZE as u64;
/// `CP_CHKSUM_OFFSET`: the checkpoint CRC sits in the last 4 bytes of the block.
const CP_CHKSUM_OFFSET: usize = 4092;
/// `CP_BITMAP_OFFSET` = `sizeof(struct f2fs_checkpoint)`.
const CP_BITMAP_OFFSET: u64 = 192;
const MIN_NAT_BITMAP_SIZE: u64 = 64;
const MAX_SIT_BITMAP_SIZE_IN_CKPT: u64 = CP_CHKSUM_OFFSET as u64 - CP_BITMAP_OFFSET - MIN_NAT_BITMAP_SIZE;
const MAX_BITMAP_SIZE_IN_CKPT: u64 = CP_CHKSUM_OFFSET as u64 - CP_BITMAP_OFFSET;
/// `MAX_SIT_BITMAP_SIZE` = SEG_ALIGN(SIZE_ALIGN(F2FS_MAX_SEGMENT, SIT_ENTRY_PER_BLOCK)) × blks_per_seg / 8.
const MAX_SIT_BITMAP_SIZE: u64 = F2FS_MAX_SEGMENT.div_ceil(SIT_ENTRY_PER_BLOCK).div_ceil(BLKS_PER_SEG) * BLKS_PER_SEG / 8;

// Checkpoint flags.
const CP_UMOUNT_FLAG: u32 = 0x0001;
const CP_COMPACT_SUM_FLAG: u32 = 0x0004;
const CP_NAT_BITS_FLAG: u32 = 0x0080;

// Log types (`CURSEG_*`), used in SIT entries' type bits.
const CURSEG_HOT_DATA: u16 = 0;
const CURSEG_WARM_DATA: u16 = 1;
const CURSEG_COLD_DATA: u16 = 2;
const CURSEG_HOT_NODE: u16 = 3;
const CURSEG_WARM_NODE: u16 = 4;
const CURSEG_COLD_NODE: u16 = 5;
const SIT_VBLOCKS_SHIFT: u16 = 10;

// Summary blocks.
const SUMMARY_SIZE: usize = 7;
const ENTRIES_IN_SUM: usize = 512;
const SUM_FOOTER_SIZE: usize = 5;
/// `SUM_JOURNAL_SIZE` = 4096 − 5 − 7 × 512 = 507 (2-byte count + 505 bytes of entries).
const SUM_JOURNAL_SIZE: usize = BLKSIZE as usize - SUM_FOOTER_SIZE - SUMMARY_SIZE * ENTRIES_IN_SUM;
const SUM_TYPE_NODE: u8 = 1;
const NAT_JOURNAL_ENTRY_SIZE: usize = 13;
const SIT_JOURNAL_ENTRY_SIZE: usize = 78;

const ROOT_INO: u32 = 3;
const NODE_INO: u32 = 1;
const META_INO: u32 = 2;
const NEXT_FREE_NID: u32 = 4;
const MAX_VOLUME_NAME: usize = 512;
const VERSION_LEN: usize = 256;
const F2FS_FT_DIR: u8 = 2;

// Dentry block: 27-byte bitmap, 3 reserved bytes, 214 × 11-byte entries, 214 × 8-byte names.
const NR_DENTRY_IN_BLOCK: usize = 214;
const SIZE_OF_DIR_ENTRY: usize = 11;
const SIZE_OF_DENTRY_BITMAP: usize = NR_DENTRY_IN_BLOCK.div_ceil(8);
const DENTRY_OFFSET: usize = BLKSIZE as usize - (SIZE_OF_DIR_ENTRY + 8) * NR_DENTRY_IN_BLOCK;
const FILENAME_OFFSET: usize = DENTRY_OFFSET + SIZE_OF_DIR_ENTRY * NR_DENTRY_IN_BLOCK;

// Node block: 4072-byte body + 24-byte footer.
const NODE_FOOTER_OFFSET: usize = 4072;

/// `media_ext_lists` and `hot_ext_lists` of f2fs_format.c (non-Android build).
const MEDIA_EXTENSIONS: [&str; 36] = [
    "mp", "wm", "og", "jp", "avi", "m4v", "m4p", "mkv", "mov", "webm", "wav", "m4a", "3gp", "opus", "flac", "gif", "png", "svg", "webp", "jar", "deb", "iso", "gz", "xz", "zst",
    "pdf", "pyc", "ttc", "ttf", "exe", "apk", "cnt", "exo", "odex", "vdex", "so",
];
const HOT_EXTENSIONS: [&str; 4] = ["db", "vmdk", "vdi", "qcow2"];

/// Smallest device `mkfs.f2fs` 1.16 accepts with the defaults (checked by the unit tests).
pub const MIN_BYTES: u64 = 44 * 1024 * 1024;
/// Largest: 32-bit block addresses (`F2FS_MAX_SEGMENT` × 2 MiB = 16 TiB).
pub const MAX_BYTES: u64 = F2FS_MAX_SEGMENT * SEGMENT_BYTES;

// ───────────────────────────────────── checksums ─────────────────────────────────────

static CRC32_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// `f2fs_cal_crc32` (lib/libf2fs.c): reflected CRC-32 (poly 0xEDB88320) with the caller's seed
/// and no final inversion. Superblock and checkpoint use `F2FS_SUPER_MAGIC` as the seed.
pub fn f2fs_crc32(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

// ───────────────────────────────────── layout ─────────────────────────────────────

/// Geometry of a volume, as `f2fs_prepare_super_block` and `f2fs_write_check_point_pack`
/// compute it (all addresses in 4 KiB blocks, counts in segments).
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub block_count: u64,
    pub segment_count: u32,
    pub segment_count_sit: u32,
    pub segment_count_nat: u32,
    pub segment_count_ssa: u32,
    pub segment_count_main: u32,
    pub section_count: u32,
    pub segment0_blkaddr: u32,
    pub sit_blkaddr: u32,
    pub nat_blkaddr: u32,
    pub ssa_blkaddr: u32,
    pub main_blkaddr: u32,
    pub cp_payload: u32,
    /// Overprovisioning ratio in percent (`c.overprovision`).
    pub overprovision: f64,
    pub reserved_segments: u32,
    pub overprov_segments: u32,
    pub free_segments: u32,
    pub user_block_count: u64,
    /// Current segment of each log, indexed by `CURSEG_*`.
    pub cur_seg: [u32; 6],
}

fn too_small(size: u64) -> FsError {
    // MiB with one decimal, truncated (43.9 MiB must not print as "44.0").
    let tenths = size.saturating_mul(10) >> 20;
    let have = format!("{}.{} MiB", tenths / 10, tenths % 10);
    FsError::Invalid(t(
        &format!("F2FS richiede almeno {} MiB: il dispositivo ha {have}", MIN_BYTES >> 20),
        &format!("F2FS needs at least {} MiB: the device has {have}", MIN_BYTES >> 20),
    ))
}

fn too_big() -> FsError {
    FsError::Invalid(t("F2FS supporta al massimo 16 TiB", "F2FS supports at most 16 TiB"))
}

/// `get_best_overprovision` (f2fs_fs.h), with the same floating-point steps so the chosen ratio
/// is bit-for-bit the one `mkfs.f2fs` picks.
fn best_overprovision(segment_count_main: u32, section_count: u32, segs_per_sec: u32) -> f64 {
    let usable = segment_count_main as f64;
    let (mut candidate, end, diff) = if segment_count_main < 256 { (10.0f64, 95.0f64, 5.0f64) } else { (0.01f64, 10.0f64, 0.01f64) };
    let per_sec = segment_count_main.div_ceil(section_count.max(1)) as f64;
    let (mut max_ovp, mut max_space) = (0.0f64, 0.0f64);
    while candidate <= end {
        let reserved = (100.0 / candidate + 1.0 + NR_CURSEG_TYPE as f64) * per_sec;
        let ovp = (usable - reserved) * candidate / 100.0;
        if ovp >= 0.0 {
            let space = usable - if reserved > ovp { reserved } else { ovp } - (2 * segs_per_sec) as f64;
            if max_space < space {
                max_space = space;
                max_ovp = candidate;
            }
        }
        candidate += diff;
    }
    max_ovp
}

/// Compute the layout for a device of `size` bytes, or refuse it like `mkfs.f2fs` does.
pub fn layout(size: u64) -> FsResult<Layout> {
    if size > MAX_BYTES {
        return Err(too_big());
    }
    let total_sectors = size / SECTOR_SIZE;
    let block_count = total_sectors >> LOG_SECTORS_PER_BLOCK;
    // segs_per_sec = secs_per_zone = segs_per_zone = 1 (defaults), so a "zone" is a segment.
    let segs_per_sec: u32 = 1;
    // zone_align_start_offset with start_sector 0: round 2 × 4 KiB (the two superblocks) up to
    // a zone (2 MiB).
    let zone_align_start_offset = (2 * BLKSIZE).div_ceil(SEGMENT_BYTES) * SEGMENT_BYTES;
    let dev_bytes = total_sectors * SECTOR_SIZE;
    if dev_bytes < zone_align_start_offset {
        return Err(too_small(size));
    }
    let total_segments = (dev_bytes - zone_align_start_offset) / SEGMENT_BYTES;
    if total_segments < F2FS_MIN_SEGMENTS {
        return Err(too_small(size));
    }
    let segment_count = total_segments; // ≤ F2FS_MAX_SEGMENT, fits u32
    let segment0_blkaddr = zone_align_start_offset / BLKSIZE;
    let segment_count_ckpt = F2FS_NUMBER_OF_CHECKPOINT_PACK as u64;
    let sit_blkaddr = segment0_blkaddr + segment_count_ckpt * BLKS_PER_SEG;

    let blocks_for_sit = segment_count.div_ceil(SIT_ENTRY_PER_BLOCK);
    let sit_segments = blocks_for_sit.div_ceil(BLKS_PER_SEG);
    let segment_count_sit = sit_segments * 2;
    let nat_blkaddr = sit_blkaddr + segment_count_sit * BLKS_PER_SEG;

    let meta = segment_count_ckpt + segment_count_sit;
    let avail = segment_count.checked_sub(meta).ok_or_else(|| too_small(size))? * BLKS_PER_SEG;
    let blocks_for_nat = avail.div_ceil(NAT_ENTRY_PER_BLOCK);
    let mut segment_count_nat = blocks_for_nat.div_ceil(BLKS_PER_SEG);

    // The SIT / NAT version bitmaps must fit in the checkpoint block (no large_nat_bitmap).
    let sit_bitmap_size = ((segment_count_sit / 2) << LOG_BLKS_PER_SEG) / 8;
    let max_sit_bitmap_size = sit_bitmap_size.min(MAX_SIT_BITMAP_SIZE);
    let (max_nat_bitmap_size, cp_payload) = if max_sit_bitmap_size > MAX_SIT_BITMAP_SIZE_IN_CKPT {
        (MAX_BITMAP_SIZE_IN_CKPT, max_sit_bitmap_size.div_ceil(BLKSIZE))
    } else {
        (MAX_BITMAP_SIZE_IN_CKPT - max_sit_bitmap_size, 0)
    };
    let max_nat_segments = (max_nat_bitmap_size * 8) >> LOG_BLKS_PER_SEG;
    segment_count_nat = segment_count_nat.min(max_nat_segments) * 2;
    let ssa_blkaddr = nat_blkaddr + segment_count_nat * BLKS_PER_SEG;

    let meta = meta + segment_count_nat;
    let avail = segment_count.checked_sub(meta).ok_or_else(|| too_small(size))? * BLKS_PER_SEG;
    let blocks_for_ssa = avail / BLKS_PER_SEG + 1;
    let segment_count_ssa = blocks_for_ssa.div_ceil(BLKS_PER_SEG);
    let total_meta_segments = meta + segment_count_ssa;
    let main_blkaddr = segment0_blkaddr + total_meta_segments * BLKS_PER_SEG;

    let total_zones = segment_count.checked_sub(total_meta_segments).ok_or_else(|| too_small(size))?;
    if total_zones == 0 {
        return Err(too_small(size));
    }
    let section_count = total_zones as u32;
    let segment_count_main = section_count * segs_per_sec;

    let overprovision = best_overprovision(segment_count_main, section_count, segs_per_sec);
    if overprovision == 0.0 {
        return Err(too_small(size));
    }
    let per_sec = segment_count_main.div_ceil(section_count) as f64;
    // C: `uint32_t = double` truncates toward zero.
    let reserved_segments = ((100.0 / overprovision + 1.0 + NR_CURSEG_TYPE as f64) * per_sec) as u32;
    if segment_count_main < NR_CURSEG_TYPE || segment_count_main - NR_CURSEG_TYPE < reserved_segments {
        return Err(too_small(size));
    }
    // "Need more zones": six logs need six distinct zones, plus at least one free.
    if total_zones <= NR_CURSEG_TYPE as u64 {
        return Err(too_small(size));
    }

    // Current segments (non-heap policy), then `verify_cur_segs`.
    let tz = total_zones as u32;
    let last_zone = |cur: u32| cur.saturating_sub(1);
    let mut cur_seg = [0u32; 6];
    cur_seg[CURSEG_HOT_NODE as usize] = 0;
    cur_seg[CURSEG_WARM_NODE as usize] = 1;
    cur_seg[CURSEG_COLD_NODE as usize] = 2;
    cur_seg[CURSEG_HOT_DATA as usize] = 3;
    cur_seg[CURSEG_COLD_DATA as usize] = last_zone(tz >> 2).max(cur_seg[CURSEG_HOT_DATA as usize] + 1);
    cur_seg[CURSEG_WARM_DATA as usize] = last_zone(tz >> 1).max(cur_seg[CURSEG_COLD_DATA as usize] + 1);
    let dup = (0..6).any(|i| (i + 1..6).any(|j| cur_seg[i] == cur_seg[j]));
    if dup {
        // verify_cur_segs: c.cur_seg[0] = 0, c.cur_seg[i] = next_zone(i − 1).
        for (i, s) in cur_seg.iter_mut().enumerate() {
            *s = i as u32;
        }
    }

    // Checkpoint counters (f2fs_write_check_point_pack).
    let usable = segment_count_main;
    let mut overprov_segments = ((usable - reserved_segments) as f64 * overprovision / 100.0) as u32;
    if overprov_segments < reserved_segments {
        overprov_segments = reserved_segments;
    }
    overprov_segments += 2 * segs_per_sec;
    if usable <= overprov_segments {
        return Err(too_small(size));
    }
    let free_segments = usable - NR_CURSEG_TYPE;
    let user_block_count = (free_segments + NR_CURSEG_TYPE - overprov_segments) as u64 * BLKS_PER_SEG;

    let u32_of = |v: u64| u32::try_from(v).map_err(|_| too_big());
    Ok(Layout {
        block_count,
        segment_count: u32_of(segment_count)?,
        segment_count_sit: u32_of(segment_count_sit)?,
        segment_count_nat: u32_of(segment_count_nat)?,
        segment_count_ssa: u32_of(segment_count_ssa)?,
        segment_count_main,
        section_count,
        segment0_blkaddr: u32_of(segment0_blkaddr)?,
        sit_blkaddr: u32_of(sit_blkaddr)?,
        nat_blkaddr: u32_of(nat_blkaddr)?,
        ssa_blkaddr: u32_of(ssa_blkaddr)?,
        main_blkaddr: u32_of(main_blkaddr)?,
        cp_payload: u32_of(cp_payload)?,
        overprovision,
        reserved_segments,
        overprov_segments,
        free_segments,
        user_block_count,
        cur_seg,
    })
}

impl Layout {
    fn seg_blk(&self, curseg: u16) -> u32 {
        self.main_blkaddr + self.cur_seg[curseg as usize] * BLKS_PER_SEG as u32
    }
    fn root_node_blk(&self) -> u32 {
        self.seg_blk(CURSEG_HOT_NODE)
    }
    fn root_dentry_blk(&self) -> u32 {
        self.seg_blk(CURSEG_HOT_DATA)
    }
    /// `nat_bits_bytes`: one bit per NAT block (first copy).
    fn nat_bits_bytes(&self) -> usize {
        (self.segment_count_nat as usize) << 5
    }
    fn nat_bits_blocks(&self) -> u32 {
        ((self.nat_bits_bytes() * 2 + 8 + BLKSIZE as usize - 1) / BLKSIZE as usize) as u32
    }
    fn cp_pack_total_block_count(&self) -> u32 {
        // cp page × 2, compact data summary × 1, node summaries × 3, plus the SIT bitmap payload.
        6 + self.cp_payload
    }
}

// ───────────────────────────────────── encoding ─────────────────────────────────────

fn put16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// The volume label as UTF-16LE code units (`utf8_to_utf16`): at most 512 units, never
/// splitting a surrogate pair; NUL-terminated when shorter.
pub fn label_utf16(label: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for c in label.chars() {
        let mut buf = [0u16; 2];
        let units = c.encode_utf16(&mut buf);
        if out.len() + units.len() > MAX_VOLUME_NAME {
            break;
        }
        out.extend_from_slice(units);
    }
    out
}

/// Everything not derived from the size: identity and the values `mkfs.f2fs` takes from the
/// clock / `rand()` (fixed in tests to compare with the reference tool).
#[derive(Clone, Debug)]
pub(crate) struct Params {
    pub uuid: [u8; 16],
    pub label: String,
    /// Seconds since the epoch for the root inode's times.
    pub time: u64,
    /// Checkpoint version of pack 1 (`rand() | 1`).
    pub cp_ver: u64,
}

/// `struct f2fs_super_block` (3072 bytes; stored at byte 1024 of blocks 0 and 1).
fn superblock(l: &Layout, p: &Params) -> Vec<u8> {
    let mut sb = vec![0u8; SB_SIZE];
    put32(&mut sb, 0, F2FS_SUPER_MAGIC);
    put16(&mut sb, 4, MAJOR_VER);
    put16(&mut sb, 6, MINOR_VER);
    put32(&mut sb, 8, LOG_SECTORSIZE);
    put32(&mut sb, 12, LOG_SECTORS_PER_BLOCK);
    put32(&mut sb, 16, LOG_BLOCKSIZE);
    put32(&mut sb, 20, LOG_BLKS_PER_SEG);
    put32(&mut sb, 24, 1); // segs_per_sec
    put32(&mut sb, 28, 1); // secs_per_zone
    put32(&mut sb, 32, 0); // checksum_offset: no F2FS_FEATURE_SB_CHKSUM
    put64(&mut sb, 36, l.block_count);
    put32(&mut sb, 44, l.section_count);
    put32(&mut sb, 48, l.segment_count);
    put32(&mut sb, 52, F2FS_NUMBER_OF_CHECKPOINT_PACK);
    put32(&mut sb, 56, l.segment_count_sit);
    put32(&mut sb, 60, l.segment_count_nat);
    put32(&mut sb, 64, l.segment_count_ssa);
    put32(&mut sb, 68, l.segment_count_main);
    put32(&mut sb, 72, l.segment0_blkaddr);
    put32(&mut sb, 76, l.segment0_blkaddr); // cp_blkaddr
    put32(&mut sb, 80, l.sit_blkaddr);
    put32(&mut sb, 84, l.nat_blkaddr);
    put32(&mut sb, 88, l.ssa_blkaddr);
    put32(&mut sb, 92, l.main_blkaddr);
    put32(&mut sb, 96, ROOT_INO);
    put32(&mut sb, 100, NODE_INO);
    put32(&mut sb, 104, META_INO);
    sb[108..124].copy_from_slice(&p.uuid);
    for (i, u) in label_utf16(&p.label).into_iter().enumerate() {
        put16(&mut sb, 124 + 2 * i, u);
    }
    // extension_count @1148, extension_list[64][8] @1152.
    for (i, e) in MEDIA_EXTENSIONS.iter().chain(HOT_EXTENSIONS.iter()).enumerate() {
        let off = 1152 + 8 * i;
        sb[off..off + e.len()].copy_from_slice(e.as_bytes());
    }
    put32(&mut sb, 1148, MEDIA_EXTENSIONS.len() as u32);
    put32(&mut sb, 1664, l.cp_payload);
    // version[256] @1668, init_version[256] @1924.
    let ver = format!("LumenDisk {} (mkfs.f2fs 1.16 layout)", env!("CARGO_PKG_VERSION"));
    let n = ver.len().min(VERSION_LEN - 1);
    sb[1668..1668 + n].copy_from_slice(&ver.as_bytes()[..n]);
    sb[1924..1924 + n].copy_from_slice(&ver.as_bytes()[..n]);
    // feature @2180 = 0, encryption fields, devs[8], qf_ino[3] all zero.
    sb[2757] = HOT_EXTENSIONS.len() as u8; // hot_ext_count
    // crc @3068 stays 0 without F2FS_FEATURE_SB_CHKSUM.
    sb
}

/// `struct f2fs_checkpoint` block of pack 1 (`ver` = cp_ver) or pack 2 (`ver` = 0).
fn checkpoint(l: &Layout, ver: u64, nat_bits: bool) -> Vec<u8> {
    let mut cp = vec![0u8; BLKSIZE as usize];
    put64(&mut cp, 0, ver);
    put64(&mut cp, 8, l.user_block_count);
    put64(&mut cp, 16, 2); // valid_block_count: root inode + root dentry block
    put32(&mut cp, 24, l.reserved_segments);
    put32(&mut cp, 28, l.overprov_segments);
    put32(&mut cp, 32, l.free_segments);
    // cur_node_segno[8] @36, cur_node_blkoff[8] @68, cur_data_segno[8] @84, cur_data_blkoff[8] @116.
    for i in 0..8 {
        put32(&mut cp, 36 + 4 * i, u32::MAX);
        put32(&mut cp, 84 + 4 * i, u32::MAX);
    }
    for (i, t) in [CURSEG_HOT_NODE, CURSEG_WARM_NODE, CURSEG_COLD_NODE].iter().enumerate() {
        put32(&mut cp, 36 + 4 * i, l.cur_seg[*t as usize]);
    }
    for (i, t) in [CURSEG_HOT_DATA, CURSEG_WARM_DATA, CURSEG_COLD_DATA].iter().enumerate() {
        put32(&mut cp, 84 + 4 * i, l.cur_seg[*t as usize]);
    }
    put16(&mut cp, 68, 1); // hot node: the root inode
    put16(&mut cp, 116, 1); // hot data: the root dentry block
    let mut flags = CP_UMOUNT_FLAG | CP_COMPACT_SUM_FLAG;
    if nat_bits {
        flags |= CP_NAT_BITS_FLAG;
    }
    put32(&mut cp, 132, flags);
    put32(&mut cp, 136, l.cp_pack_total_block_count());
    put32(&mut cp, 140, 1 + l.cp_payload); // cp_pack_start_sum
    put32(&mut cp, 144, 1); // valid_node_count
    put32(&mut cp, 148, 1); // valid_inode_count
    put32(&mut cp, 152, NEXT_FREE_NID);
    put32(&mut cp, 156, ((l.segment_count_sit / 2) << LOG_BLKS_PER_SEG) / 8);
    put32(&mut cp, 160, ((l.segment_count_nat / 2) << LOG_BLKS_PER_SEG) / 8);
    put32(&mut cp, 164, CP_CHKSUM_OFFSET as u32);
    // elapsed_time, alloc_type[16] (all SSR-free LFS = 0) and the version bitmaps stay 0.
    let crc = f2fs_crc32(F2FS_SUPER_MAGIC, &cp[..CP_CHKSUM_OFFSET]);
    put32(&mut cp, CP_CHKSUM_OFFSET, crc);
    cp
}

/// Set bit `nr` in an f2fs bitmap that is numbered MSB-first (`f2fs_set_bit`: SIT valid maps).
fn f2fs_set_bit(map: &mut [u8], nr: usize) {
    map[nr >> 3] |= 0x80 >> (nr & 7);
}

/// Compact data summary block (`CP_COMPACT_SUM_FLAG`): NAT journal, SIT journal, then the
/// summaries of the hot / warm / cold data logs one after the other.
fn compact_summary(l: &Layout) -> Vec<u8> {
    let mut b = vec![0u8; BLKSIZE as usize];
    // NAT journal: n_nats then entries {nid, version, ino, block_addr}.
    put16(&mut b, 0, 1);
    put32(&mut b, 2, ROOT_INO);
    b[6] = 0;
    put32(&mut b, 7, ROOT_INO);
    put32(&mut b, 11, l.root_node_blk());
    debug_assert!(15 <= 2 + NAT_JOURNAL_ENTRY_SIZE);
    // SIT journal: n_sits then entries {segno, vblocks, valid_map[64], mtime}.
    let sit = SUM_JOURNAL_SIZE;
    put16(&mut b, sit, 6);
    let entries: [(u16, u16); 6] = [(CURSEG_HOT_NODE, 1), (CURSEG_WARM_NODE, 0), (CURSEG_COLD_NODE, 0), (CURSEG_HOT_DATA, 1), (CURSEG_WARM_DATA, 0), (CURSEG_COLD_DATA, 0)];
    for (i, (ty, valid)) in entries.iter().enumerate() {
        let e = sit + 2 + i * SIT_JOURNAL_ENTRY_SIZE;
        put32(&mut b, e, l.cur_seg[*ty as usize]);
        put16(&mut b, e + 4, (ty << SIT_VBLOCKS_SHIFT) | valid);
        if *valid > 0 {
            f2fs_set_bit(&mut b[e + 6..e + 6 + 64], 0);
        }
    }
    // Hot data summary: block 0 of the hot data log belongs to the root (nid 3, ofs 0).
    let sum = 2 * SUM_JOURNAL_SIZE;
    put32(&mut b, sum, ROOT_INO);
    b
}

/// Node summary block of a node log; the hot one records the root inode at offset 0.
fn node_summary(with_root: bool) -> Vec<u8> {
    let mut b = vec![0u8; BLKSIZE as usize];
    if with_root {
        put32(&mut b, 0, ROOT_INO); // entries[0] = {nid 3, version 0, ofs_in_node 0}
    }
    b[BLKSIZE as usize - SUM_FOOTER_SIZE] = SUM_TYPE_NODE; // footer.entry_type
    b
}

/// The root directory's inode block (`f2fs_write_root_inode`).
fn root_inode(l: &Layout, p: &Params) -> Vec<u8> {
    let mut n = vec![0u8; BLKSIZE as usize];
    put16(&mut n, 0, 0o040755); // i_mode = S_IFDIR | 0755 (0x41ed)
    put32(&mut n, 4, 0); // i_uid
    put32(&mut n, 8, 0); // i_gid
    put32(&mut n, 12, 2); // i_links
    put64(&mut n, 16, BLKSIZE); // i_size: one dentry block
    put64(&mut n, 24, 2); // i_blocks (inode block + dentry block)
    put64(&mut n, 32, p.time); // i_atime
    put64(&mut n, 40, p.time); // i_ctime
    put64(&mut n, 48, p.time); // i_mtime
    put32(&mut n, 72, 1); // i_current_depth
    // i_dir_level @347 = DEF_DIR_LEVEL (0); i_ext @348 empty; i_addr[0] @360.
    put32(&mut n, 360, l.root_dentry_blk());
    // node_footer @4072: nid, ino, flag, cp_ver (packed u64), next_blkaddr.
    put32(&mut n, NODE_FOOTER_OFFSET, ROOT_INO);
    put32(&mut n, NODE_FOOTER_OFFSET + 4, ROOT_INO);
    put64(&mut n, NODE_FOOTER_OFFSET + 12, 1);
    put32(&mut n, NODE_FOOTER_OFFSET + 20, l.root_node_blk() + 1);
    n
}

/// The root's dentry block with `.` and `..` (`f2fs_add_default_dentry_root`).
fn root_dentries() -> Vec<u8> {
    let mut d = vec![0u8; BLKSIZE as usize];
    d[0] = 0b11; // slots 0 and 1 in use (little-endian bit order)
    for (i, name) in [&b"."[..], &b".."[..]].iter().enumerate() {
        let e = DENTRY_OFFSET + i * SIZE_OF_DIR_ENTRY;
        put32(&mut d, e, 0); // hash_code: F2FS_DOT_HASH
        put32(&mut d, e + 4, ROOT_INO);
        put16(&mut d, e + 8, name.len() as u16);
        d[e + 10] = F2FS_FT_DIR;
        let f = FILENAME_OFFSET + i * 8;
        d[f..f + name.len()].copy_from_slice(name);
    }
    debug_assert_eq!(DENTRY_OFFSET, SIZE_OF_DENTRY_BITMAP + 3);
    d
}

/// First NAT block: the node and meta inodes (block address 1, as mkfs writes them) and root.
fn nat_block(l: &Layout) -> Vec<u8> {
    let mut b = vec![0u8; BLKSIZE as usize];
    for (nid, addr) in [(NODE_INO, 1u32), (META_INO, 1), (ROOT_INO, l.root_node_blk())] {
        let e = nid as usize * NAT_ENTRY_SIZE;
        b[e] = 0; // version
        put32(&mut b, e + 1, nid); // ino
        put32(&mut b, e + 5, addr);
    }
    b
}

/// NAT bits (`CP_NAT_BITS_FLAG`): the pack's CRC tag, "full" bits (none), "empty" bits (all
/// NAT blocks but the first).
fn nat_bits(l: &Layout, cp1: &[u8]) -> Vec<u8> {
    let bytes = l.nat_bits_bytes();
    let mut b = vec![0u8; l.nat_bits_blocks() as usize * BLKSIZE as usize];
    let crc = u32::from_le_bytes([cp1[CP_CHKSUM_OFFSET], cp1[CP_CHKSUM_OFFSET + 1], cp1[CP_CHKSUM_OFFSET + 2], cp1[CP_CHKSUM_OFFSET + 3]]);
    let ver = u64::from_le_bytes(cp1[0..8].try_into().unwrap_or([0; 8]));
    put64(&mut b, 0, ver | ((crc as u64) << 32));
    let empty = 8 + bytes;
    b[empty..empty + bytes].fill(0xFF);
    b[empty] &= !1; // NAT block 0 holds nids 1–3 (test_and_clear_bit_le(0))
    b
}

fn io(e: std::io::Error) -> FsError {
    FsError::Io(e.to_string())
}

fn write_blk(dev: &dyn BlockDev, blk: u64, data: &[u8]) -> FsResult<()> {
    dev.write_at(blk * BLKSIZE, data).map_err(io)
}

/// Zero `count` blocks from `blk` (1 MiB writes).
fn zero_blocks(dev: &dyn BlockDev, blk: u64, count: u64) -> FsResult<()> {
    const CHUNK: u64 = 256;
    let zero = vec![0u8; (CHUNK * BLKSIZE) as usize];
    let mut done = 0;
    while done < count {
        let n = (count - done).min(CHUNK);
        dev.write_at((blk + done) * BLKSIZE, &zero[..(n * BLKSIZE) as usize]).map_err(io)?;
        done += n;
    }
    Ok(())
}

/// Create the filesystem.
pub fn format(dev: Arc<dyn BlockDev>, opts: &MkfsOptions) -> FsResult<()> {
    // opts.cluster_size is ignored: F2FS has 4 KiB blocks only (see the module comment).
    let rnd = *uuid::Uuid::new_v4().as_bytes();
    let params = Params {
        uuid: opts.uuid.unwrap_or_else(|| *uuid::Uuid::new_v4().as_bytes()),
        label: opts.label.clone(),
        time: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        // rand() | 1: a 31-bit odd number.
        cp_ver: (u32::from_le_bytes([rnd[0], rnd[1], rnd[2], rnd[3]]) & 0x7FFF_FFFF | 1) as u64,
    };
    format_with(dev.as_ref(), &params)
}

/// [`format`] with the clock and the random checkpoint version fixed, for comparing with
/// `mkfs.f2fs -T <time>` byte by byte (tests only).
#[doc(hidden)]
pub fn format_reproducible(dev: Arc<dyn BlockDev>, opts: &MkfsOptions, time: u64, cp_ver: u64) -> FsResult<()> {
    let params = Params { uuid: opts.uuid.unwrap_or([0; 16]), label: opts.label.clone(), time, cp_ver };
    format_with(dev.as_ref(), &params)
}

pub(crate) fn format_with(dev: &dyn BlockDev, p: &Params) -> FsResult<()> {
    let l = layout(dev.size())?;
    let seg0 = l.segment0_blkaddr as u64;
    let bps = BLKS_PER_SEG;

    // Wipe the superblocks first (an interrupted format must not look like a valid F2FS) and
    // the gap before segment 0 (stale signatures of the previous filesystem).
    zero_blocks(dev, 0, seg0)?;

    // SIT: first copy zeroed (f2fs_init_sit_area). The CP's SIT / NAT version bitmaps are all
    // zero, which selects the first copy of every block.
    zero_blocks(dev, l.sit_blkaddr as u64, (l.segment_count_sit as u64 / 2) * bps)?;
    // NAT: copies alternate segment by segment; zero the first copy (even segments).
    for i in 0..(l.segment_count_nat as u64 / 2) {
        zero_blocks(dev, l.nat_blkaddr as u64 + 2 * i * bps, bps)?;
    }

    // Root inode and its dentry block; the first warm node block is zeroed so roll-forward
    // recovery finds no stale fsync'd node chain (f2fs_discard_obsolete_dnode).
    write_blk(dev, l.root_node_blk() as u64, &root_inode(&l, p))?;
    zero_blocks(dev, l.seg_blk(CURSEG_WARM_NODE) as u64, 1)?;
    write_blk(dev, l.nat_blkaddr as u64, &nat_block(&l))?;
    write_blk(dev, l.root_dentry_blk() as u64, &root_dentries())?;

    // Checkpoint packs: both segments zeroed, then pack 1 (current, version cp_ver) and pack 2
    // (version 0, just its two cp pages).
    zero_blocks(dev, seg0, 2 * bps)?;
    let nat_bits_blocks = l.nat_bits_blocks();
    let with_nat_bits = l.cp_pack_total_block_count() as u64 + nat_bits_blocks as u64 <= bps;
    let cp1 = checkpoint(&l, p.cp_ver, with_nat_bits);
    let mut blk = seg0;
    write_blk(dev, blk, &cp1)?;
    blk += 1 + l.cp_payload as u64; // the payload (SIT bitmap overflow) is zero
    write_blk(dev, blk, &compact_summary(&l))?;
    write_blk(dev, blk + 1, &node_summary(true))?;
    write_blk(dev, blk + 2, &node_summary(false))?;
    write_blk(dev, blk + 3, &node_summary(false))?;
    write_blk(dev, blk + 4, &cp1)?;
    if with_nat_bits {
        write_blk(dev, seg0 + bps - nat_bits_blocks as u64, &nat_bits(&l, &cp1))?;
    }
    let cp2 = checkpoint(&l, 0, with_nat_bits);
    write_blk(dev, seg0 + bps, &cp2)?;
    write_blk(dev, seg0 + bps + l.cp_pack_total_block_count() as u64 - 1, &cp2)?;

    // Superblocks last.
    dev.flush().map_err(io)?;
    let mut blk0 = vec![0u8; BLKSIZE as usize];
    blk0[SUPER_OFFSET..SUPER_OFFSET + SB_SIZE].copy_from_slice(&superblock(&l, p));
    write_blk(dev, 0, &blk0)?;
    write_blk(dev, 1, &blk0)?;
    dev.flush().map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemDev;

    fn params() -> Params {
        Params { uuid: [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef], label: "TestLbl".into(), time: 1_700_000_000, cp_ver: 0x62ed_dde5 }
    }
    fn get32(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }
    fn get16(b: &[u8], o: usize) -> u16 {
        u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
    }
    fn get64(b: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
    }

    #[test]
    fn crc32_matches_standard_crc32() {
        // With seed !0 and a final inversion, f2fs_cal_crc32 is the IEEE CRC-32.
        assert_eq!(!f2fs_crc32(!0, b"123456789"), 0xCBF4_3926);
        assert_eq!(f2fs_crc32(F2FS_SUPER_MAGIC, b""), F2FS_SUPER_MAGIC);
    }

    /// Values printed by `mkfs.f2fs` 1.16 (Debian trixie) for the same sizes.
    #[test]
    fn layout_matches_mkfs_f2fs_1_16() {
        let l = layout(1 << 30).unwrap();
        assert_eq!((l.block_count, l.segment_count, l.segment_count_sit, l.segment_count_nat, l.segment_count_ssa, l.segment_count_main), (262144, 511, 2, 4, 1, 502));
        assert_eq!((l.sit_blkaddr, l.nat_blkaddr, l.ssa_blkaddr, l.main_blkaddr), (1536, 2560, 4608, 5120));
        assert_eq!((l.reserved_segments, l.overprov_segments, l.free_segments, l.user_block_count), (25, 27, 496, 243200));
        assert_eq!(l.cur_seg[..3], [3, 250, 124]);
        assert!((l.overprovision - 5.37).abs() < 0.001);

        let l = layout(1_000_000_000).unwrap();
        assert_eq!((l.block_count, l.segment_count, l.segment_count_main), (244140, 475, 466));
        assert_eq!((l.reserved_segments, l.overprov_segments, l.free_segments, l.user_block_count), (24, 26, 460, 225280));
        assert_eq!(l.cur_seg[..3], [3, 232, 115]);

        let l = layout(MIN_BYTES).unwrap();
        assert_eq!((l.block_count, l.segment_count, l.segment_count_main, l.main_blkaddr), (11264, 21, 14, 4096));
        assert_eq!((l.reserved_segments, l.overprov_segments, l.free_segments, l.user_block_count), (8, 10, 8, 2048));
        assert_eq!(l.cur_seg[..3], [3, 6, 4]);
        assert_eq!(l.overprovision, 95.0);
    }

    #[test]
    fn refuses_below_the_minimum() {
        assert!(layout(MIN_BYTES).is_ok());
        for s in [0, 1, 4096, 2 << 20, 20 << 20, MIN_BYTES - 4096, MIN_BYTES - 1] {
            match layout(s) {
                Err(FsError::Invalid(m)) => assert!(m.contains("44 MiB"), "{m}"),
                other => panic!("{s}: {other:?}"),
            }
        }
        // Nothing between 0 and the minimum is accepted, and every size above it is.
        for blocks in (0..MIN_BYTES / 4096).step_by(37) {
            assert!(layout(blocks * 4096).is_err());
        }
        for s in (MIN_BYTES..MIN_BYTES + (600 << 20)).step_by(999_983) {
            layout(s).unwrap();
        }
        assert!(matches!(layout(MAX_BYTES + 1), Err(FsError::Invalid(_))));
        let dev = MemDev::new(4 << 20);
        assert!(format_with(&dev, &params()).is_err());
    }

    #[test]
    fn huge_layouts_are_consistent() {
        for s in [MAX_BYTES, MAX_BYTES - 4095, 1 << 40, 50 << 30, (3 << 40) + 12345] {
            let l = layout(s).unwrap();
            let end = l.main_blkaddr as u64 + l.segment_count_main as u64 * BLKS_PER_SEG;
            assert!(end <= l.block_count, "{s}");
            assert_eq!(l.segment0_blkaddr as u64 + l.segment_count as u64 * BLKS_PER_SEG, end);
            // The SIT / NAT version bitmaps fit in the checkpoint (or its payload).
            let sit_bm = ((l.segment_count_sit / 2) << LOG_BLKS_PER_SEG) / 8;
            let nat_bm = ((l.segment_count_nat / 2) << LOG_BLKS_PER_SEG) / 8;
            assert!(sit_bm as u64 + nat_bm as u64 <= MAX_BITMAP_SIZE_IN_CKPT + l.cp_payload as u64 * BLKSIZE);
            assert!(l.cp_pack_total_block_count() < BLKS_PER_SEG as u32);
            let mut segs = l.cur_seg.to_vec();
            segs.sort();
            segs.dedup();
            assert_eq!(segs.len(), 6);
            assert!(l.cur_seg.iter().all(|&c| c < l.segment_count_main));
        }
        // 16 TiB needs a checkpoint payload for the SIT bitmap.
        assert!(layout(MAX_BYTES).unwrap().cp_payload > 0);
    }

    #[test]
    fn writes_a_consistent_volume() {
        let size = 64 << 20;
        let dev = MemDev::new(size);
        format_with(&dev, &params()).unwrap();
        let img = dev.snapshot();
        let l = layout(size as u64).unwrap();
        let blk = |b: u32| &img[b as usize * 4096..(b as usize + 1) * 4096];

        // Both superblocks, identical.
        let sb = &img[1024..4096];
        assert_eq!(get32(sb, 0), F2FS_SUPER_MAGIC);
        assert_eq!(&img[4096 + 1024..8192], sb);
        assert_eq!(&img[..1024], &[0u8; 1024][..]);
        assert_eq!(get64(sb, 36), (size / 4096) as u64);
        assert_eq!(get32(sb, 92), l.main_blkaddr);
        assert_eq!(&sb[108..124], &params().uuid);
        assert_eq!(get16(sb, 124), b'T' as u16);
        assert_eq!(get16(sb, 124 + 14), 0);
        assert_eq!(get32(sb, 1148), 36);
        assert_eq!(&sb[1152..1154], b"mp");
        assert_eq!(&sb[1152 + 8 * 39..1152 + 8 * 39 + 5], b"qcow2");

        // Checkpoint pack 1: valid CRC, both cp pages equal, newer than pack 2.
        let cp = blk(l.segment0_blkaddr);
        assert_eq!(get32(cp, 4092), f2fs_crc32(F2FS_SUPER_MAGIC, &cp[..4092]));
        assert_eq!(cp, blk(l.segment0_blkaddr + 5));
        assert_eq!(get64(cp, 0), params().cp_ver);
        assert_eq!(get32(cp, 132), CP_UMOUNT_FLAG | CP_COMPACT_SUM_FLAG | CP_NAT_BITS_FLAG);
        let cp2 = blk(l.segment0_blkaddr + 512);
        assert_eq!(get64(cp2, 0), 0);
        assert_eq!(get32(cp2, 4092), f2fs_crc32(F2FS_SUPER_MAGIC, &cp2[..4092]));
        assert_eq!(cp2, blk(l.segment0_blkaddr + 512 + 5));
        // NAT bits tag = version | crc << 32.
        let nb = blk(l.segment0_blkaddr + 512 - l.nat_bits_blocks());
        assert_eq!(get64(nb, 0), params().cp_ver | (get32(cp, 4092) as u64) << 32);

        // NAT: root → its inode block, which points to the dentry block.
        let nat = blk(l.nat_blkaddr);
        assert_eq!(get32(nat, 3 * 9 + 1), 3);
        assert_eq!(get32(nat, 3 * 9 + 5), l.main_blkaddr);
        let inode = blk(l.main_blkaddr);
        assert_eq!(get16(inode, 0), 0x41ed);
        assert_eq!(get32(inode, 360), l.main_blkaddr + 3 * 512);
        assert_eq!(get32(inode, 4072), 3);
        let dent = blk(l.main_blkaddr + 3 * 512);
        assert_eq!(dent[0], 3);
        assert_eq!(&dent[FILENAME_OFFSET..FILENAME_OFFSET + 3], b".\0\0");
        assert_eq!(&dent[FILENAME_OFFSET + 8..FILENAME_OFFSET + 10], b"..");

        // Compact summary: SIT journal marks block 0 of hot node and hot data valid.
        let sum = blk(l.segment0_blkaddr + 1);
        assert_eq!(get16(sum, 0), 1);
        assert_eq!(get16(sum, 507), 6);
        assert_eq!(get16(sum, 507 + 2 + 4), (3 << 10) | 1);
        assert_eq!(sum[507 + 2 + 6], 0x80);
        assert_eq!(get16(sum, 507 + 2 + 3 * 78 + 4), 1);
        assert_eq!(get32(sum, 1014), 3);
        assert_eq!(blk(l.segment0_blkaddr + 2)[4091], SUM_TYPE_NODE);
    }

    #[test]
    fn wipes_stale_metadata() {
        let size = 50 << 20;
        let dev = MemDev::from_vec(vec![0xA5; size]);
        format_with(&dev, &params()).unwrap();
        let img = dev.snapshot();
        let l = layout(size as u64).unwrap();
        let range = |b: u32, n: u32| &img[b as usize * 4096..(b + n) as usize * 4096];
        assert!(range(2, l.segment0_blkaddr - 2).iter().all(|&b| b == 0));
        assert!(range(l.sit_blkaddr, l.segment_count_sit / 2 * 512).iter().all(|&b| b == 0));
        assert!(range(l.nat_blkaddr + 1, 511).iter().all(|&b| b == 0));
        assert!(range(l.main_blkaddr + 512, 1).iter().all(|&b| b == 0));
        assert!(range(l.segment0_blkaddr + 512 + 1, 4).iter().all(|&b| b == 0));
    }

    #[test]
    fn labels() {
        assert_eq!(label_utf16("ab"), vec![b'a' as u16, b'b' as u16]);
        assert_eq!(label_utf16("é𝄞"), vec![0xE9, 0xD834, 0xDD1E]);
        assert_eq!(label_utf16(&"x".repeat(600)).len(), 512);
        // A surrogate pair is never split at the limit.
        let s = format!("{}𝄞", "x".repeat(511));
        assert_eq!(label_utf16(&s).len(), 511);
        let mut p = params();
        p.label = "Ünïcødé 卷".into();
        let dev = MemDev::new(48 << 20);
        format_with(&dev, &p).unwrap();
        let img = dev.snapshot();
        let units: Vec<u16> = (0..10).map(|i| get16(&img, 1024 + 124 + 2 * i)).collect();
        assert_eq!(String::from_utf16(&units[..9]).unwrap(), "Ünïcødé 卷");
        assert_eq!(units[9], 0);
    }

    #[test]
    fn options_and_odd_sizes() {
        // cluster_size is ignored; odd (non block-multiple) sizes are fine.
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new((45 << 20) + 513));
        let opts = MkfsOptions { label: "x".into(), cluster_size: Some(65536), uuid: Some([7; 16]) };
        format(dev.clone(), &opts).unwrap();
        let mut sb = vec![0u8; 3072];
        dev.read_at(1024, &mut sb).unwrap();
        assert_eq!(&sb[108..124], &[7; 16]);
        assert_eq!(get64(&sb, 36), ((45 << 20) + 513) / 4096);
        let mut cp = vec![0u8; 4096];
        dev.read_at(512 * 4096, &mut cp).unwrap();
        assert_eq!(get64(&cp, 0) & 1, 1);
    }
}
