//! ext2 / ext3 / ext4 — pure user-space read/write driver.
//!
//! Supported for read/write: block maps (ext2/3) and extent trees (ext4, any depth), 32/64-bit
//! group descriptors, flex_bg, meta_bg, sparse_super(2), uninit_bg / metadata_csum (crc32c on
//! superblock, group descriptors, bitmaps, inodes, extent blocks, directory blocks, xattr
//! blocks), huge_file, dir_nlink, extra_isize, large_dir, htree directories (read natively;
//! converted to a valid linear layout on first modification), fast & slow symlinks.
//!
//! Volumes using features we cannot write safely (inline_data, bigalloc, encryption,
//! casefold, quota, verity, ...) are opened read-only with an explanation.
//!
//! Journal Doctor: `dirty_state()` reports `needs_recovery`, `ERROR_FS`, a not-cleanly
//! unmounted state and pending orphans; `repair()` replays the jbd2 journal (descriptor /
//! revoke / commit blocks, csum v2/v3, 64-bit tags), processes the orphan chain,
//! clears error state and reconciles free block/inode counters from the bitmaps.
//!
//! The driver never writes to the journal: every metadata update is written in place, in an
//! order that keeps the filesystem consistent between operations (e2fsck -fn stays clean).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::*;
use crate::block::BlockDev;

// ───────────────────────────── little helpers ─────────────────────────────

#[inline]
fn r16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
fn r32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline]
fn w16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
#[inline]
fn w32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
#[inline]
fn rbe32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline]
fn wbe32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

static CRC32C_TABLE: once_cell::sync::Lazy<[u32; 256]> = once_cell::sync::Lazy::new(|| {
    let mut t = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0x82F6_3B78 ^ (c >> 1) } else { c >> 1 };
        }
        t[i as usize] = c;
    }
    t
});

/// Raw crc32c (Castagnoli) without pre/post inversion — matches the kernel's `ext4_chksum`.
pub fn crc32c_raw(mut crc: u32, data: &[u8]) -> u32 {
    let t = &*CRC32C_TABLE;
    for &b in data {
        crc = t[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

static CRC16_TABLE: once_cell::sync::Lazy<[u16; 256]> = once_cell::sync::Lazy::new(|| {
    let mut t = [0u16; 256];
    for i in 0..256u16 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xA001 ^ (c >> 1) } else { c >> 1 };
        }
        t[i as usize] = c;
    }
    t
});

fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    let t = &*CRC16_TABLE;
    for &b in data {
        crc = (crc >> 8) ^ t[((crc ^ b as u16) & 0xFF) as usize];
    }
    crc
}

fn corrupt<T>(s: impl Into<String>) -> FsResult<T> {
    Err(FsError::Corrupt(s.into()))
}

// ───────────────────────────── on-disk constants ─────────────────────────────

const EXT4_MAGIC: u16 = 0xEF53;

const COMPAT_HAS_JOURNAL: u32 = 0x4;
const COMPAT_EXT_ATTR: u32 = 0x8;
const COMPAT_DIR_INDEX: u32 = 0x20;
const COMPAT_SPARSE_SUPER2: u32 = 0x200;

const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
const INCOMPAT_META_BG: u32 = 0x10;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_MMP: u32 = 0x100;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_LARGEDIR: u32 = 0x4000;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const INCOMPAT_SUPPORTED_RW: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_META_BG
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_FLEX_BG
    | INCOMPAT_CSUM_SEED
    | INCOMPAT_LARGEDIR;
const INCOMPAT_SUPPORTED_RO: u32 = INCOMPAT_SUPPORTED_RW | INCOMPAT_INLINE_DATA | INCOMPAT_MMP | 0x10000 | 0x20000 | 0x400;

const RO_SPARSE_SUPER: u32 = 0x1;
const RO_LARGE_FILE: u32 = 0x2;
const RO_HUGE_FILE: u32 = 0x8;
const RO_GDT_CSUM: u32 = 0x10;
const RO_DIR_NLINK: u32 = 0x20;
const RO_EXTRA_ISIZE: u32 = 0x40;
const RO_METADATA_CSUM: u32 = 0x400;
const RO_ORPHAN_PRESENT: u32 = 0x10000;
const RO_SUPPORTED_RW: u32 = RO_SPARSE_SUPER
    | RO_LARGE_FILE
    | 0x4
    | RO_HUGE_FILE
    | RO_GDT_CSUM
    | RO_DIR_NLINK
    | RO_EXTRA_ISIZE
    | RO_METADATA_CSUM
    | 0x2000 /* project ids: preserved, not enforced */
    | RO_ORPHAN_PRESENT;

const BG_INODE_UNINIT: u16 = 0x1;
const BG_BLOCK_UNINIT: u16 = 0x2;
const BG_INODE_ZEROED: u16 = 0x4;

const S_IFMT: u16 = 0xF000;
const S_IFREG: u16 = 0x8000;
const S_IFDIR: u16 = 0x4000;
const S_IFLNK: u16 = 0xA000;

const FL_INDEX: u32 = 0x1000;
const FL_HUGE_FILE: u32 = 0x40000;
const FL_EXTENTS: u32 = 0x80000;
const FL_INLINE_DATA: u32 = 0x1000_0000;

const EXT_MAGIC: u16 = 0xF30A;
const EXT_INIT_MAX_LEN: u32 = 32768;

const ROOT_INO: u32 = 2;
const JOURNAL_INO: u32 = 8;

// Inode field offsets.
const I_MODE: usize = 0x00;
const I_UID: usize = 0x02;
const I_SIZE_LO: usize = 0x04;
const I_ATIME: usize = 0x08;
const I_CTIME: usize = 0x0C;
const I_MTIME: usize = 0x10;
const I_DTIME: usize = 0x14;
const I_GID: usize = 0x18;
const I_LINKS: usize = 0x1A;
const I_BLOCKS_LO: usize = 0x1C;
const I_FLAGS: usize = 0x20;
const I_BLOCK: usize = 0x28;
const I_GENERATION: usize = 0x64;
const I_FILE_ACL_LO: usize = 0x68;
const I_SIZE_HI: usize = 0x6C;
const I_BLOCKS_HI: usize = 0x74;
const I_FILE_ACL_HI: usize = 0x76;
const I_UID_HI: usize = 0x78;
const I_GID_HI: usize = 0x7A;
const I_CSUM_LO: usize = 0x7C;
const I_EXTRA_ISIZE: usize = 0x80;
const I_CSUM_HI: usize = 0x82;
const I_CTIME_EXTRA: usize = 0x84;
const I_MTIME_EXTRA: usize = 0x88;
const I_ATIME_EXTRA: usize = 0x8C;
const I_CRTIME: usize = 0x90;
const I_CRTIME_EXTRA: usize = 0x94;

// Superblock field offsets (relative to the 1024-byte superblock).
const S_INODES_COUNT: usize = 0x00;
const S_BLOCKS_LO: usize = 0x04;
const S_FREE_BLOCKS_LO: usize = 0x0C;
const S_FREE_INODES: usize = 0x10;
const S_FIRST_DATA_BLOCK: usize = 0x14;
const S_LOG_BLOCK: usize = 0x18;
const S_LOG_CLUSTER: usize = 0x1C;
const S_BLOCKS_PER_GROUP: usize = 0x20;
const S_INODES_PER_GROUP: usize = 0x28;
const S_WTIME: usize = 0x30;
const S_MAGIC: usize = 0x38;
const S_STATE: usize = 0x3A;
const S_REV: usize = 0x4C;
const S_FIRST_INO: usize = 0x54;
const S_INODE_SIZE: usize = 0x58;
const S_COMPAT: usize = 0x5C;
const S_INCOMPAT: usize = 0x60;
const S_RO_COMPAT: usize = 0x64;
const S_UUID: usize = 0x68;
const S_VOLNAME: usize = 0x78;
const S_RESERVED_GDT: usize = 0xCE;
const S_JOURNAL_INUM: usize = 0xE0;
const S_LAST_ORPHAN: usize = 0xE8;
const S_DESC_SIZE: usize = 0xFE;
const S_FIRST_META_BG: usize = 0x104;
const S_BLOCKS_HI: usize = 0x150;
const S_FREE_BLOCKS_HI: usize = 0x158;
const S_ERROR_COUNT: usize = 0x194;
const S_BACKUP_BGS: usize = 0x24C;
const S_CSUM_SEED: usize = 0x270;
const S_CHECKSUM: usize = 0x3FC;

// ───────────────────────────── data structures ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ext {
    lblk: u32,
    pblk: u64,
    len: u32,
    uninit: bool,
}

/// An in-memory copy of an on-disk inode.
#[derive(Clone)]
struct Inode {
    ino: u32,
    raw: Vec<u8>,
}

impl Inode {
    fn mode(&self) -> u16 {
        r16(&self.raw, I_MODE)
    }
    fn kind(&self) -> FileKind {
        match self.mode() & S_IFMT {
            S_IFDIR => FileKind::Dir,
            S_IFLNK => FileKind::Symlink,
            _ => FileKind::File,
        }
    }
    fn size(&self) -> u64 {
        r32(&self.raw, I_SIZE_LO) as u64 | (r32(&self.raw, I_SIZE_HI) as u64) << 32
    }
    fn set_size(&mut self, s: u64) {
        w32(&mut self.raw, I_SIZE_LO, s as u32);
        w32(&mut self.raw, I_SIZE_HI, (s >> 32) as u32);
    }
    fn flags(&self) -> u32 {
        r32(&self.raw, I_FLAGS)
    }
    fn set_flags(&mut self, f: u32) {
        w32(&mut self.raw, I_FLAGS, f)
    }
    fn links(&self) -> u16 {
        r16(&self.raw, I_LINKS)
    }
    fn set_links(&mut self, l: u16) {
        w16(&mut self.raw, I_LINKS, l)
    }
    fn generation(&self) -> u32 {
        r32(&self.raw, I_GENERATION)
    }
    fn uid(&self) -> u32 {
        r16(&self.raw, I_UID) as u32 | (r16(&self.raw, I_UID_HI) as u32) << 16
    }
    fn gid(&self) -> u32 {
        r16(&self.raw, I_GID) as u32 | (r16(&self.raw, I_GID_HI) as u32) << 16
    }
    fn set_uid(&mut self, u: u32) {
        w16(&mut self.raw, I_UID, u as u16);
        w16(&mut self.raw, I_UID_HI, (u >> 16) as u16);
    }
    fn set_gid(&mut self, g: u32) {
        w16(&mut self.raw, I_GID, g as u16);
        w16(&mut self.raw, I_GID_HI, (g >> 16) as u16);
    }
    fn file_acl(&self) -> u64 {
        r32(&self.raw, I_FILE_ACL_LO) as u64 | (r16(&self.raw, I_FILE_ACL_HI) as u64) << 32
    }
    fn extra_isize(&self) -> usize {
        if self.raw.len() > 128 {
            r16(&self.raw, I_EXTRA_ISIZE) as usize
        } else {
            0
        }
    }
    fn has_extra(&self, field_end: usize) -> bool {
        self.raw.len() > 128 && 128 + self.extra_isize() >= field_end
    }
    fn time(&self, base: usize, extra: usize) -> Timestamp {
        let lo = r32(&self.raw, base) as i32 as i64;
        if extra != 0 && self.has_extra(extra + 4) {
            let ex = r32(&self.raw, extra);
            let epoch = (ex & 3) as i64;
            Timestamp { secs: lo + (epoch << 32), nsecs: (ex >> 2).min(999_999_999) }
        } else {
            Timestamp { secs: lo, nsecs: 0 }
        }
    }
    fn set_time(&mut self, base: usize, extra: usize, t: Timestamp) {
        w32(&mut self.raw, base, t.secs as u32);
        if extra != 0 && self.has_extra(extra + 4) {
            let epoch = (((t.secs - (t.secs as i32 as i64)) >> 32) & 3) as u32;
            w32(&mut self.raw, extra, (t.nsecs << 2) | epoch);
        }
    }
    fn i_block(&self) -> &[u8] {
        &self.raw[I_BLOCK..I_BLOCK + 60]
    }
    fn i_block_mut(&mut self) -> &mut [u8] {
        &mut self.raw[I_BLOCK..I_BLOCK + 60]
    }
    fn uses_extents(&self) -> bool {
        self.flags() & FL_EXTENTS != 0
    }
}

pub struct Ext4Fs {
    dev: Arc<dyn BlockDev>,
    ro: bool,
    ro_reason: Option<String>,
    sb: Vec<u8>,
    bs: u64,
    fdb: u64,
    blocks_count: u64,
    bpg: u64,
    ipg: u32,
    inode_size: usize,
    ngroups: u32,
    desc_size: usize,
    gds: Vec<Vec<u8>>,
    compat: u32,
    incompat: u32,
    ro_compat: u32,
    csum: bool,
    gdt_csum: bool,
    seed: u32,
    first_ino: u32,
    itb: u64,
    bb_cache: HashMap<u32, Vec<u8>>,
    ib_cache: HashMap<u32, Vec<u8>>,
    fs_type: FsType,
    /// Rotating allocation hint for new directories.
    dir_group_hint: u32,
    /// Set when anything was written since the last sync (avoids idle superblock writes).
    modified: bool,
    /// A group descriptor points outside the volume: mounted read-only, Doctor refuses.
    bad_gd: Option<String>,
}

/// Detect ext2/3/4.
pub fn probe(dev: &dyn BlockDev) -> Option<FsType> {
    if dev.size() < 2048 {
        return None;
    }
    let sb = dev.read_vec(1024, 1024).ok()?;
    if r16(&sb, S_MAGIC) != EXT4_MAGIC {
        return None;
    }
    Some(classify(&sb))
}

fn classify(sb: &[u8]) -> FsType {
    let compat = r32(sb, S_COMPAT);
    let incompat = r32(sb, S_INCOMPAT);
    let ro = r32(sb, S_RO_COMPAT);
    if incompat & (INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG) != 0
        || ro & (RO_HUGE_FILE | RO_GDT_CSUM | RO_DIR_NLINK | RO_EXTRA_ISIZE | RO_METADATA_CSUM) != 0
    {
        FsType::Ext4
    } else if compat & COMPAT_HAS_JOURNAL != 0 {
        FsType::Ext3
    } else {
        FsType::Ext2
    }
}

fn rec_len_for(name_len: usize) -> usize {
    (8 + name_len + 3) & !3
}

fn ftype_code(kind: FileKind) -> u8 {
    match kind {
        FileKind::File => 1,
        FileKind::Dir => 2,
        FileKind::Symlink => 7,
    }
}

struct RawDirent {
    off: usize,
    inode: u32,
    rec_len: usize,
    name_len: usize,
    ftype: u8,
}

impl Ext4Fs {
    // ─────────────── open / superblock ───────────────

    pub fn open(dev: Arc<dyn BlockDev>, read_only: bool) -> FsResult<Self> {
        let fs = Self::open_raw(dev, read_only)?;
        let root = fs.read_inode(ROOT_INO)?;
        if root.kind() != FileKind::Dir {
            return corrupt("root inode is not a directory");
        }
        Ok(fs)
    }

    fn open_raw(dev: Arc<dyn BlockDev>, read_only: bool) -> FsResult<Self> {
        let sb = dev.read_vec(1024, 1024)?;
        if r16(&sb, S_MAGIC) != EXT4_MAGIC {
            return Err(FsError::Unsupported("not an ext2/3/4 filesystem".into()));
        }
        let log = r32(&sb, S_LOG_BLOCK);
        if log > 6 {
            return corrupt("bad block size");
        }
        let bs = 1024u64 << log;
        let incompat = r32(&sb, S_INCOMPAT);
        let ro_compat = r32(&sb, S_RO_COMPAT);
        let compat = r32(&sb, S_COMPAT);
        let sixty_four = incompat & INCOMPAT_64BIT != 0;
        let blocks_count = r32(&sb, S_BLOCKS_LO) as u64 | if sixty_four { (r32(&sb, S_BLOCKS_HI) as u64) << 32 } else { 0 };
        let fdb = r32(&sb, S_FIRST_DATA_BLOCK) as u64;
        let bpg = r32(&sb, S_BLOCKS_PER_GROUP) as u64;
        let ipg = r32(&sb, S_INODES_PER_GROUP);
        let rev = r32(&sb, S_REV);
        let inode_size = if rev == 0 { 128 } else { r16(&sb, S_INODE_SIZE) as usize };
        let first_ino = if rev == 0 { 11 } else { r32(&sb, S_FIRST_INO) };
        // Same limits the kernel enforces (ext4_fill_super): one bitmap block per group, power
        // of two inode size, first data block 0/1. Everything below relies on them.
        if bpg == 0
            || bpg > 8 * bs
            || ipg == 0
            || ipg as u64 > 8 * bs
            || inode_size < 128
            || !inode_size.is_power_of_two()
            || inode_size as u64 > bs
            || (ipg as u64) < bs / inode_size as u64
            || fdb > 1
            || blocks_count <= fdb
            || (rev != 0 && first_ino < 11)
        {
            return corrupt("invalid superblock geometry");
        }
        let fs_bytes = blocks_count.checked_mul(bs).unwrap_or(u64::MAX);
        if fs_bytes > dev.size().saturating_add(bs) {
            return corrupt(format!("filesystem ({fs_bytes} bytes) is larger than the device ({} bytes)", dev.size()));
        }
        let ngroups64 = (blocks_count - fdb).div_ceil(bpg);
        if ngroups64.checked_mul(ipg as u64).map_or(true, |n| n > u32::MAX as u64) {
            return corrupt("too many inodes for the group count");
        }
        let ngroups = ngroups64 as u32;
        let desc_size = if sixty_four { (r16(&sb, S_DESC_SIZE) as usize).max(32) } else { 32 };
        if !desc_size.is_power_of_two() || desc_size as u64 > bs.min(1024) {
            return corrupt(format!("invalid group descriptor size {desc_size}"));
        }
        let csum = ro_compat & RO_METADATA_CSUM != 0;
        let gdt_csum = !csum && ro_compat & RO_GDT_CSUM != 0;
        let seed = if incompat & INCOMPAT_CSUM_SEED != 0 {
            r32(&sb, S_CSUM_SEED)
        } else {
            crc32c_raw(!0, &sb[S_UUID..S_UUID + 16])
        };
        let itb = (ipg as u64 * inode_size as u64).div_ceil(bs);

        let mut ro_reason = None;
        if incompat & !INCOMPAT_SUPPORTED_RO != 0 {
            return Err(FsError::Unsupported(format!("unknown ext4 incompat features 0x{:x}", incompat & !INCOMPAT_SUPPORTED_RO)));
        }
        if incompat & !INCOMPAT_SUPPORTED_RW != 0 {
            ro_reason = Some(format!(
                "feature ext4 non supportate in scrittura (incompat 0x{:x}: inline_data/encrypt/casefold/mmp)",
                incompat & !INCOMPAT_SUPPORTED_RW
            ));
        }
        if ro_compat & !RO_SUPPORTED_RW != 0 {
            ro_reason = Some(format!(
                "feature ext4 non supportate in scrittura (ro_compat 0x{:x}: quota/bigalloc/verity)",
                ro_compat & !RO_SUPPORTED_RW
            ));
        }
        if incompat & INCOMPAT_JOURNAL_DEV != 0 {
            return Err(FsError::Unsupported("external journal device".into()));
        }
        if dev.read_only() && ro_reason.is_none() && !read_only {
            ro_reason = Some("il dispositivo è aperto in sola lettura".into());
        }
        let fs_type = classify(&sb);
        let mut fs = Ext4Fs {
            dev,
            ro: read_only || ro_reason.is_some(),
            ro_reason,
            sb,
            bs,
            fdb,
            blocks_count,
            bpg,
            ipg,
            inode_size,
            ngroups,
            desc_size,
            gds: vec![],
            compat,
            incompat,
            ro_compat,
            csum,
            gdt_csum,
            seed,
            first_ino,
            itb,
            bb_cache: HashMap::new(),
            ib_cache: HashMap::new(),
            fs_type,
            dir_group_hint: 0,
            modified: false,
            bad_gd: None,
        };
        if incompat & INCOMPAT_META_BG != 0 && r32(&fs.sb, S_FIRST_META_BG) as u64 > fs.gdt_blocks() {
            return corrupt("first meta block group too large");
        }
        if r16(&fs.sb, S_RESERVED_GDT) as u64 > bs / 4 {
            return corrupt("too many reserved GDT blocks");
        }
        fs.load_gds()?;
        fs.check_descriptors();
        Ok(fs)
    }

    fn desc_per_block(&self) -> u64 {
        self.bs / self.desc_size as u64
    }

    fn group_first_block(&self, g: u32) -> u64 {
        self.fdb + g as u64 * self.bpg
    }

    fn blocks_in_group(&self, g: u32) -> u64 {
        let start = self.group_first_block(g);
        (self.blocks_count - start).min(self.bpg)
    }

    fn has_super(&self, g: u32) -> bool {
        if g == 0 {
            return true;
        }
        if self.compat & COMPAT_SPARSE_SUPER2 != 0 {
            return g == r32(&self.sb, S_BACKUP_BGS) || g == r32(&self.sb, S_BACKUP_BGS + 4);
        }
        if self.ro_compat & RO_SPARSE_SUPER == 0 {
            return true;
        }
        if g == 1 {
            return true;
        }
        for base in [3u32, 5, 7] {
            let mut p = base;
            while p < g {
                p = match p.checked_mul(base) {
                    Some(v) => v,
                    None => break,
                };
            }
            if p == g {
                return true;
            }
        }
        false
    }

    fn gdt_blocks(&self) -> u64 {
        (self.ngroups as u64).div_ceil(self.desc_per_block())
    }

    /// Block holding group descriptor `g`.
    fn gd_location(&self, g: u32) -> u64 {
        let per = self.desc_per_block();
        let dblk = g as u64 / per;
        let first_meta = r32(&self.sb, S_FIRST_META_BG) as u64;
        if self.incompat & INCOMPAT_META_BG == 0 || dblk < first_meta {
            self.fdb + 1 + dblk
        } else {
            let grp = (dblk * per) as u32;
            self.group_first_block(grp) + self.has_super(grp) as u64
        }
    }

    /// Superblock / group-descriptor / reserved-GDT blocks inside group `g`.
    fn sb_bgd_blocks(&self, g: u32) -> Vec<u64> {
        let start = self.group_first_block(g);
        let mut v = vec![];
        let hs = self.has_super(g);
        if hs {
            v.push(start);
        }
        let per = self.desc_per_block();
        let first_meta = r32(&self.sb, S_FIRST_META_BG) as u64;
        let meta = self.incompat & INCOMPAT_META_BG != 0;
        if !meta || (g as u64 / per) < first_meta {
            if hs {
                let old = if meta { first_meta } else { self.gdt_blocks() + r16(&self.sb, S_RESERVED_GDT) as u64 };
                for i in 0..old {
                    v.push(start + 1 + i);
                }
            }
        }
        if meta {
            let idx = g as u64 % per;
            if (g as u64 / per) >= first_meta && (idx == 0 || idx == 1 || idx == per - 1) {
                v.push(start + hs as u64);
            }
        }
        v
    }

    fn load_gds(&mut self) -> FsResult<()> {
        let mut gds = Vec::with_capacity(self.ngroups as usize);
        let mut cache: HashMap<u64, Vec<u8>> = HashMap::new();
        for g in 0..self.ngroups {
            let loc = self.gd_location(g);
            if !cache.contains_key(&loc) {
                cache.insert(loc, self.read_block(loc)?);
            }
            let blk = &cache[&loc];
            let off = (g as u64 % self.desc_per_block()) as usize * self.desc_size;
            gds.push(blk[off..off + self.desc_size].to_vec());
        }
        self.gds = gds;
        Ok(())
    }

    /// Like the kernel's ext4_check_descriptors: bitmaps and inode tables must lie inside the
    /// volume. A bad descriptor makes the volume read-only (what is reachable stays readable).
    fn check_descriptors(&mut self) {
        let inside = |b: u64, n: u64| b >= self.fdb && b.checked_add(n).map_or(false, |e| e <= self.blocks_count);
        for g in 0..self.ngroups {
            let ok = inside(self.gd_block_bitmap(g), 1) && inside(self.gd_inode_bitmap(g), 1) && inside(self.gd_inode_table(g), self.itb);
            if !ok {
                let why = format!("il descrittore del gruppo {g} punta fuori dal volume (group descriptor {g} is corrupt)");
                self.ro = true;
                if self.ro_reason.is_none() {
                    self.ro_reason = Some(why.clone());
                }
                self.bad_gd = Some(why);
                return;
            }
        }
    }

    // group descriptor accessors
    fn gd_u64(&self, g: u32, lo: usize, hi: usize) -> u64 {
        let d = &self.gds[g as usize];
        r32(d, lo) as u64 | if self.desc_size >= 64 { (r32(d, hi) as u64) << 32 } else { 0 }
    }
    fn gd_u32(&self, g: u32, lo: usize, hi: usize) -> u32 {
        let d = &self.gds[g as usize];
        r16(d, lo) as u32 | if self.desc_size >= 64 { (r16(d, hi) as u32) << 16 } else { 0 }
    }
    fn set_gd_u32(&mut self, g: u32, lo: usize, hi: usize, v: u32) {
        let big = self.desc_size >= 64;
        let d = &mut self.gds[g as usize];
        w16(d, lo, v as u16);
        if big {
            w16(d, hi, (v >> 16) as u16);
        }
    }
    fn gd_block_bitmap(&self, g: u32) -> u64 {
        self.gd_u64(g, 0x0, 0x20)
    }
    fn gd_inode_bitmap(&self, g: u32) -> u64 {
        self.gd_u64(g, 0x4, 0x24)
    }
    fn gd_inode_table(&self, g: u32) -> u64 {
        self.gd_u64(g, 0x8, 0x28)
    }
    fn gd_free_blocks(&self, g: u32) -> u32 {
        self.gd_u32(g, 0xC, 0x2C)
    }
    fn gd_free_inodes(&self, g: u32) -> u32 {
        self.gd_u32(g, 0xE, 0x2E)
    }
    fn gd_used_dirs(&self, g: u32) -> u32 {
        self.gd_u32(g, 0x10, 0x30)
    }
    fn gd_itable_unused(&self, g: u32) -> u32 {
        self.gd_u32(g, 0x1C, 0x32)
    }
    fn gd_flags(&self, g: u32) -> u16 {
        r16(&self.gds[g as usize], 0x12)
    }
    fn set_gd_flags(&mut self, g: u32, f: u16) {
        w16(&mut self.gds[g as usize], 0x12, f)
    }

    fn uninit_bg(&self) -> bool {
        self.csum || self.gdt_csum
    }

    fn gd_checksum(&self, g: u32) -> u16 {
        let d = &self.gds[g as usize];
        if self.csum {
            let mut c = crc32c_raw(self.seed, &g.to_le_bytes());
            c = crc32c_raw(c, &d[..0x1E]);
            c = crc32c_raw(c, &[0, 0]);
            if self.desc_size > 0x20 {
                c = crc32c_raw(c, &d[0x20..self.desc_size]);
            }
            (c & 0xFFFF) as u16
        } else if self.gdt_csum {
            let mut c = crc16(!0, &self.sb[S_UUID..S_UUID + 16]);
            c = crc16(c, &g.to_le_bytes());
            c = crc16(c, &d[..0x1E]);
            if self.desc_size > 0x20 {
                c = crc16(c, &d[0x20..self.desc_size]);
            }
            c
        } else {
            0
        }
    }

    fn write_gd(&mut self, g: u32) -> FsResult<()> {
        if self.uninit_bg() {
            let c = self.gd_checksum(g);
            w16(&mut self.gds[g as usize], 0x1E, c);
        }
        let loc = self.gd_location(g);
        let off = (g as u64 % self.desc_per_block()) * self.desc_size as u64;
        self.dev.write_at(loc * self.bs + off, &self.gds[g as usize])?;
        Ok(())
    }

    fn write_sb(&mut self) -> FsResult<()> {
        self.modified = true;
        if self.csum {
            let c = crc32c_raw(!0, &self.sb[..S_CHECKSUM]);
            w32(&mut self.sb, S_CHECKSUM, c);
        }
        self.dev.write_at(1024, &self.sb)?;
        Ok(())
    }

    fn sb_free_blocks(&self) -> u64 {
        r32(&self.sb, S_FREE_BLOCKS_LO) as u64
            | if self.incompat & INCOMPAT_64BIT != 0 { (r32(&self.sb, S_FREE_BLOCKS_HI) as u64) << 32 } else { 0 }
    }
    fn set_sb_free_blocks(&mut self, v: u64) {
        w32(&mut self.sb, S_FREE_BLOCKS_LO, v as u32);
        if self.incompat & INCOMPAT_64BIT != 0 {
            w32(&mut self.sb, S_FREE_BLOCKS_HI, (v >> 32) as u32);
        }
    }

    fn label(&self) -> String {
        let n = &self.sb[S_VOLNAME..S_VOLNAME + 16];
        let end = n.iter().position(|&c| c == 0).unwrap_or(16);
        String::from_utf8_lossy(&n[..end]).into_owned()
    }

    fn uuid_string(&self) -> String {
        let u = &self.sb[S_UUID..S_UUID + 16];
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]
        )
    }

    // ─────────────── raw block I/O ───────────────

    fn read_block(&self, blk: u64) -> FsResult<Vec<u8>> {
        if blk >= self.blocks_count {
            return corrupt(format!("block {blk} out of range"));
        }
        Ok(self.dev.read_vec(blk * self.bs, self.bs as usize)?)
    }

    fn write_block(&self, blk: u64, data: &[u8]) -> FsResult<()> {
        if blk >= self.blocks_count || blk == 0 && self.fdb == 0 && data.len() as u64 == self.bs {
            // Never overwrite block 0 on 4K filesystems (it holds the superblock).
            return corrupt(format!("refusing to write block {blk}"));
        }
        self.dev.write_at(blk * self.bs, data)?;
        Ok(())
    }

    fn check_rw(&self) -> FsResult<()> {
        if self.ro {
            Err(FsError::ReadOnly)
        } else {
            Ok(())
        }
    }

    // ─────────────── bitmaps & allocation ───────────────

    fn block_bitmap(&mut self, g: u32) -> FsResult<&mut Vec<u8>> {
        if !self.bb_cache.contains_key(&g) {
            let bm = if self.uninit_bg() && self.gd_flags(g) & BG_BLOCK_UNINIT != 0 {
                self.synth_block_bitmap(g)
            } else {
                self.read_block(self.gd_block_bitmap(g))?
            };
            self.bb_cache.insert(g, bm);
        }
        Ok(self.bb_cache.get_mut(&g).unwrap())
    }

    /// Compute the content of a BLOCK_UNINIT bitmap: only metadata blocks are in use.
    fn synth_block_bitmap(&self, g: u32) -> Vec<u8> {
        let mut bm = vec![0u8; self.bs as usize];
        let start = self.group_first_block(g);
        let n = self.blocks_in_group(g);
        let mut set = |b: u64| {
            if b >= start && b < start + n {
                let i = (b - start) as usize;
                bm[i / 8] |= 1 << (i % 8);
            }
        };
        for b in self.sb_bgd_blocks(g) {
            set(b);
        }
        for h in 0..self.ngroups {
            set(self.gd_block_bitmap(h));
            set(self.gd_inode_bitmap(h));
            let it = self.gd_inode_table(h);
            for k in 0..self.itb {
                set(it.saturating_add(k));
            }
        }
        for i in n as usize..(self.bs as usize * 8) {
            bm[i / 8] |= 1 << (i % 8);
        }
        bm
    }

    fn inode_bitmap(&mut self, g: u32) -> FsResult<&mut Vec<u8>> {
        if !self.ib_cache.contains_key(&g) {
            let bm = if self.uninit_bg() && self.gd_flags(g) & BG_INODE_UNINIT != 0 {
                let mut v = vec![0u8; self.bs as usize];
                for i in self.ipg as usize..self.bs as usize * 8 {
                    v[i / 8] |= 1 << (i % 8);
                }
                v
            } else {
                self.read_block(self.gd_inode_bitmap(g))?
            };
            self.ib_cache.insert(g, bm);
        }
        Ok(self.ib_cache.get_mut(&g).unwrap())
    }

    fn flush_block_bitmap(&mut self, g: u32) -> FsResult<()> {
        let bm = self.bb_cache[&g].clone();
        self.write_block(self.gd_block_bitmap(g), &bm)?;
        let mut f = self.gd_flags(g);
        if f & BG_BLOCK_UNINIT != 0 {
            f &= !BG_BLOCK_UNINIT;
            self.set_gd_flags(g, f);
        }
        if self.csum {
            let c = crc32c_raw(self.seed, &bm[..(self.bpg / 8) as usize]);
            let big = self.desc_size >= 64;
            let d = &mut self.gds[g as usize];
            w16(d, 0x18, c as u16);
            if big {
                w16(d, 0x38, (c >> 16) as u16);
            }
        }
        Ok(())
    }

    fn flush_inode_bitmap(&mut self, g: u32) -> FsResult<()> {
        let bm = self.ib_cache[&g].clone();
        self.write_block(self.gd_inode_bitmap(g), &bm)?;
        let mut f = self.gd_flags(g);
        if f & BG_INODE_UNINIT != 0 {
            f &= !BG_INODE_UNINIT;
            self.set_gd_flags(g, f);
        }
        if self.csum {
            let c = crc32c_raw(self.seed, &bm[..(self.ipg / 8) as usize]);
            let big = self.desc_size >= 64;
            let d = &mut self.gds[g as usize];
            w16(d, 0x1A, c as u16);
            if big {
                w16(d, 0x3A, (c >> 16) as u16);
            }
        }
        Ok(())
    }

    /// Allocate up to `want` blocks, contiguous if possible, starting the search at `goal`.
    /// Returns runs (start, len) totalling exactly `want` blocks.
    fn alloc_blocks(&mut self, goal: u64, want: u64) -> FsResult<Vec<(u64, u64)>> {
        self.check_rw()?;
        if want == 0 {
            return Ok(vec![]);
        }
        if self.sb_free_blocks() < want {
            return Err(FsError::NoSpace);
        }
        let mut runs: Vec<(u64, u64)> = vec![];
        let mut remaining = want;
        let goal = goal.clamp(self.fdb, self.blocks_count - 1);
        let g0 = ((goal - self.fdb) / self.bpg) as u32;
        let mut touched: Vec<(u32, u32)> = vec![];
        for step in 0..self.ngroups {
            if remaining == 0 {
                break;
            }
            let g = (g0 + step) % self.ngroups;
            if self.gd_free_blocks(g) == 0 {
                continue;
            }
            let start = self.group_first_block(g);
            let n = self.blocks_in_group(g) as usize;
            let begin = if step == 0 { (goal - start) as usize } else { 0 };
            let mut got_here: u64 = 0;
            {
                let bm = self.block_bitmap(g)?;
                // Two passes: from `begin` to end, then from 0 to begin.
                for (lo, hi) in [(begin, n), (0, begin)] {
                    let mut i = lo;
                    while i < hi && remaining > 0 {
                        if bm[i / 8] == 0xFF && i % 8 == 0 {
                            i += 8;
                            continue;
                        }
                        if bm[i / 8] & (1 << (i % 8)) == 0 {
                            let s = i;
                            while i < hi && (remaining > (i - s) as u64) && bm[i / 8] & (1 << (i % 8)) == 0 {
                                bm[i / 8] |= 1 << (i % 8);
                                i += 1;
                            }
                            let len = (i - s) as u64;
                            remaining -= len;
                            got_here += len;
                            let pb = start + s as u64;
                            match runs.last_mut() {
                                Some(last) if last.0 + last.1 == pb => last.1 += len,
                                _ => runs.push((pb, len)),
                            }
                        } else {
                            i += 1;
                        }
                    }
                }
            }
            if got_here > 0 {
                let fb = self.gd_free_blocks(g).saturating_sub(got_here as u32);
                self.set_gd_u32(g, 0xC, 0x2C, fb);
                touched.push((g, got_here as u32));
            }
        }
        if remaining > 0 {
            // Roll back: clear the bits we set and restore the group counters.
            for &(s, l) in &runs {
                self.mark_blocks(s, l, false, false)?;
            }
            for (g, got) in touched {
                let fb = self.gd_free_blocks(g).saturating_add(got);
                self.set_gd_u32(g, 0xC, 0x2C, fb);
                self.flush_block_bitmap(g)?;
                self.write_gd(g)?;
            }
            return Err(FsError::NoSpace);
        }
        for (g, _) in touched {
            self.flush_block_bitmap(g)?;
            self.write_gd(g)?;
        }
        let fb = self.sb_free_blocks() - want;
        self.set_sb_free_blocks(fb);
        self.write_sb()?;
        Ok(runs)
    }

    /// Set/clear bits for a block range. When `account` is set, free counters are updated
    /// and everything is flushed.
    fn mark_blocks(&mut self, start: u64, len: u64, used: bool, account: bool) -> FsResult<()> {
        if !used {
            self.dev.note_freed(start * self.bs, len * self.bs);
        }
        let mut b = start;
        let end = start + len;
        let mut touched = HashSet::new();
        let mut delta: i64 = 0;
        while b < end {
            if b < self.fdb || b >= self.blocks_count {
                return corrupt(format!("freeing block {b} out of range"));
            }
            let g = ((b - self.fdb) / self.bpg) as u32;
            let gstart = self.group_first_block(g);
            let gend = (gstart + self.blocks_in_group(g)).min(end);
            let mut changed: i64 = 0;
            {
                let bm = self.block_bitmap(g)?;
                while b < gend {
                    let i = (b - gstart) as usize;
                    let was = bm[i / 8] & (1 << (i % 8)) != 0;
                    if used && !was {
                        bm[i / 8] |= 1 << (i % 8);
                        changed -= 1;
                    } else if !used && was {
                        bm[i / 8] &= !(1 << (i % 8));
                        changed += 1;
                    }
                    b += 1;
                }
            }
            if account && changed != 0 {
                let fb = (self.gd_free_blocks(g) as i64 + changed) as u32;
                self.set_gd_u32(g, 0xC, 0x2C, fb);
            }
            delta += changed;
            touched.insert(g);
        }
        if account {
            for g in touched {
                self.flush_block_bitmap(g)?;
                self.write_gd(g)?;
            }
            let fb = (self.sb_free_blocks() as i64 + delta) as u64;
            self.set_sb_free_blocks(fb);
            self.write_sb()?;
        }
        Ok(())
    }

    fn free_blocks(&mut self, runs: &[(u64, u64)]) -> FsResult<()> {
        for &(s, l) in runs {
            if l > 0 {
                self.mark_blocks(s, l, false, true)?;
            }
        }
        Ok(())
    }

    fn alloc_inode(&mut self, parent: u32, is_dir: bool) -> FsResult<u32> {
        self.check_rw()?;
        if r32(&self.sb, S_FREE_INODES) == 0 {
            return Err(FsError::NoSpace);
        }
        let pg = (parent - 1) / self.ipg;
        let order: Vec<u32> = if is_dir {
            // Spread directories: pick the group with the most free inodes and blocks.
            let mut best = None;
            let mut best_score = 0u64;
            for k in 0..self.ngroups {
                let g = (self.dir_group_hint + k) % self.ngroups;
                let fi = self.gd_free_inodes(g) as u64;
                if fi == 0 {
                    continue;
                }
                let score = fi * 4 + self.gd_free_blocks(g) as u64;
                if best.is_none() || score > best_score {
                    best = Some(g);
                    best_score = score;
                }
            }
            self.dir_group_hint = (self.dir_group_hint + 1) % self.ngroups;
            let mut v = vec![];
            if let Some(b) = best {
                v.push(b);
            }
            v.extend((0..self.ngroups).map(|k| (pg + k) % self.ngroups));
            v
        } else {
            (0..self.ngroups).map(|k| (pg + k) % self.ngroups).collect()
        };
        for g in order {
            if self.gd_free_inodes(g) == 0 {
                continue;
            }
            let ipg = self.ipg as usize;
            let min = if g == 0 { self.first_ino as usize - 1 } else { 0 };
            let idx = {
                let bm = self.inode_bitmap(g)?;
                let mut found = None;
                for i in min..ipg {
                    if bm[i / 8] & (1 << (i % 8)) == 0 {
                        bm[i / 8] |= 1 << (i % 8);
                        found = Some(i);
                        break;
                    }
                }
                found
            };
            let Some(i) = idx else { continue };
            let was_uninit = self.gd_flags(g) & BG_INODE_UNINIT != 0;
            self.flush_inode_bitmap(g)?;
            let fi = self.gd_free_inodes(g) - 1;
            self.set_gd_u32(g, 0xE, 0x2E, fi);
            if is_dir {
                let d = self.gd_used_dirs(g) + 1;
                self.set_gd_u32(g, 0x10, 0x30, d);
            }
            if self.uninit_bg() {
                let unused = self.gd_itable_unused(g);
                let used_hw = self.ipg.saturating_sub(unused);
                if was_uninit || (i as u32) >= used_hw {
                    let nu = self.ipg - (i as u32 + 1);
                    // Inodes between the old high watermark and i may contain garbage if the
                    // table was never zeroed: zero them so e2fsck sees empty inodes.
                    if self.gd_flags(g) & BG_INODE_ZEROED == 0 {
                        let from = if was_uninit { 0 } else { used_hw };
                        let it = self.gd_inode_table(g);
                        let zero = vec![0u8; self.inode_size];
                        for k in from..=(i as u32) {
                            self.dev.write_at(it * self.bs + k as u64 * self.inode_size as u64, &zero)?;
                        }
                    }
                    self.set_gd_u32(g, 0x1C, 0x32, nu);
                }
            }
            self.write_gd(g)?;
            let f = r32(&self.sb, S_FREE_INODES) - 1;
            w32(&mut self.sb, S_FREE_INODES, f);
            self.write_sb()?;
            return Ok(g * self.ipg + i as u32 + 1);
        }
        Err(FsError::NoSpace)
    }

    fn free_inode(&mut self, ino: u32, is_dir: bool) -> FsResult<()> {
        let g = (ino - 1) / self.ipg;
        let i = ((ino - 1) % self.ipg) as usize;
        {
            let bm = self.inode_bitmap(g)?;
            bm[i / 8] &= !(1 << (i % 8));
        }
        self.flush_inode_bitmap(g)?;
        let fi = self.gd_free_inodes(g).saturating_add(1);
        self.set_gd_u32(g, 0xE, 0x2E, fi);
        if is_dir {
            let d = self.gd_used_dirs(g).saturating_sub(1);
            self.set_gd_u32(g, 0x10, 0x30, d);
        }
        self.write_gd(g)?;
        let f = r32(&self.sb, S_FREE_INODES).saturating_add(1);
        w32(&mut self.sb, S_FREE_INODES, f);
        self.write_sb()?;
        Ok(())
    }

    // ─────────────── inodes ───────────────

    fn inode_loc(&self, ino: u32) -> FsResult<u64> {
        if ino == 0 || ino > self.ipg * self.ngroups {
            return Err(FsError::Stale);
        }
        let g = (ino - 1) / self.ipg;
        let i = (ino - 1) % self.ipg;
        let it = self.gd_inode_table(g);
        if it < self.fdb || it.checked_add(self.itb).map_or(true, |e| e > self.blocks_count) {
            return corrupt(format!("inode table of group {g} is outside the volume"));
        }
        Ok(it * self.bs + i as u64 * self.inode_size as u64)
    }

    fn read_inode(&self, ino: u32) -> FsResult<Inode> {
        let loc = self.inode_loc(ino)?;
        let raw = self.dev.read_vec(loc, self.inode_size)?;
        Ok(Inode { ino, raw })
    }

    fn inode_seed(&self, ino: u32, gen: u32) -> u32 {
        let c = crc32c_raw(self.seed, &ino.to_le_bytes());
        crc32c_raw(c, &gen.to_le_bytes())
    }

    fn write_inode(&mut self, inode: &mut Inode) -> FsResult<()> {
        self.check_rw()?;
        self.modified = true;
        if self.csum {
            let has_hi = inode.has_extra(I_CSUM_HI + 2);
            w16(&mut inode.raw, I_CSUM_LO, 0);
            if has_hi {
                w16(&mut inode.raw, I_CSUM_HI, 0);
            }
            let seed = self.inode_seed(inode.ino, inode.generation());
            let c = crc32c_raw(seed, &inode.raw);
            w16(&mut inode.raw, I_CSUM_LO, c as u16);
            if has_hi {
                w16(&mut inode.raw, I_CSUM_HI, (c >> 16) as u16);
            }
        }
        let loc = self.inode_loc(inode.ino)?;
        self.dev.write_at(loc, &inode.raw)?;
        Ok(())
    }

    fn new_inode(&self, ino: u32, mode: u16, uid: u32, gid: u32) -> Inode {
        let mut raw = vec![0u8; self.inode_size];
        if self.inode_size > 128 {
            let extra = if self.ro_compat & RO_EXTRA_ISIZE != 0 {
                (r16(&self.sb, 0x15E) as usize).max(32)
            } else {
                32
            };
            w16(&mut raw, I_EXTRA_ISIZE, extra.min(self.inode_size - 128) as u16);
        }
        let mut n = Inode { ino, raw };
        w16(&mut n.raw, I_MODE, mode);
        n.set_uid(uid);
        n.set_gid(gid);
        let now = Timestamp::now();
        n.set_time(I_ATIME, I_ATIME_EXTRA, now);
        n.set_time(I_CTIME, I_CTIME_EXTRA, now);
        n.set_time(I_MTIME, I_MTIME_EXTRA, now);
        if n.has_extra(I_CRTIME_EXTRA + 4) {
            n.set_time(I_CRTIME, I_CRTIME_EXTRA, now);
        }
        // Pseudo-random generation number.
        let gen = crc32c_raw(ino ^ now.nsecs, &now.secs.to_le_bytes());
        w32(&mut n.raw, I_GENERATION, gen);
        if self.incompat & INCOMPAT_EXTENTS != 0 && (mode & S_IFMT) != S_IFLNK {
            n.set_flags(FL_EXTENTS);
            Self::init_extent_root(n.i_block_mut());
        }
        n
    }

    fn init_extent_root(ib: &mut [u8]) {
        ib.fill(0);
        w16(ib, 0, EXT_MAGIC);
        w16(ib, 2, 0);
        w16(ib, 4, 4);
        w16(ib, 6, 0);
    }

    fn blocks_unit(&self, inode: &Inode) -> u64 {
        if self.ro_compat & RO_HUGE_FILE != 0 && inode.flags() & FL_HUGE_FILE != 0 {
            self.bs
        } else {
            512
        }
    }

    fn i_blocks(&self, inode: &Inode) -> u64 {
        let mut v = r32(&inode.raw, I_BLOCKS_LO) as u64;
        if self.ro_compat & RO_HUGE_FILE != 0 {
            v |= (r16(&inode.raw, I_BLOCKS_HI) as u64) << 32;
        }
        v.saturating_mul(self.blocks_unit(inode))
    }

    /// Set i_blocks from a count of filesystem blocks (data + metadata + xattr block).
    fn set_i_blocks(&self, inode: &mut Inode, fs_blocks: u64) {
        let mut flags = inode.flags() & !FL_HUGE_FILE;
        let sectors = fs_blocks * (self.bs / 512);
        let v = if sectors >> 48 != 0 && self.ro_compat & RO_HUGE_FILE != 0 {
            flags |= FL_HUGE_FILE;
            fs_blocks
        } else {
            sectors
        };
        inode.set_flags(flags);
        w32(&mut inode.raw, I_BLOCKS_LO, v as u32);
        if self.ro_compat & RO_HUGE_FILE != 0 {
            w16(&mut inode.raw, I_BLOCKS_HI, (v >> 32) as u16);
        }
    }

    fn ino32(&self, ino: Ino) -> FsResult<u32> {
        if ino == 0 || ino > (self.ipg as u64 * self.ngroups as u64) {
            return Err(FsError::Stale);
        }
        Ok(ino as u32)
    }

    fn live_inode(&self, ino: Ino) -> FsResult<Inode> {
        let i = self.read_inode(self.ino32(ino)?)?;
        if i.links() == 0 && i.ino != ROOT_INO {
            return Err(FsError::Stale);
        }
        Ok(i)
    }

    fn attr_of(&self, i: &Inode) -> Attr {
        Attr {
            ino: i.ino as Ino,
            kind: i.kind(),
            size: i.size(),
            perm: i.mode() & 0o7777,
            nlink: i.links() as u32,
            uid: i.uid(),
            gid: i.gid(),
            atime: i.time(I_ATIME, I_ATIME_EXTRA),
            mtime: i.time(I_MTIME, I_MTIME_EXTRA),
            ctime: i.time(I_CTIME, I_CTIME_EXTRA),
            used_bytes: self.i_blocks(i),
        }
    }

    // ─────────────── block mapping (extents + block maps) ───────────────

    /// Return the mapping and the list of metadata (tree / indirect) blocks.
    ///
    /// Corrupt mappings fail with `Corrupt` instead of looping or exhausting memory: every
    /// tree / indirect block is visited at most once (cycles and cross-links are errors), the
    /// number of mapped blocks can't exceed the volume, and the result is sorted,
    /// non-overlapping and inside the volume (`read_data` / `ensure_blocks` rely on that).
    fn get_mapping(&self, inode: &Inode) -> FsResult<(Vec<Ext>, Vec<u64>)> {
        let mut exts = vec![];
        let mut meta = vec![];
        if inode.flags() & FL_INLINE_DATA != 0 {
            return Err(FsError::Unsupported("inline_data files".into()));
        }
        let mut seen = HashSet::new();
        if inode.uses_extents() {
            self.walk_extents(inode.i_block(), None, &mut exts, &mut meta, &mut seen)?;
        } else {
            if inode.kind() == FileKind::Symlink && self.is_fast_symlink(inode) {
                return Ok((exts, meta));
            }
            let ib = inode.i_block();
            let per = self.bs / 4;
            let mut budget = self.blocks_count;
            for k in 0..12u64 {
                self.map_push(&mut exts, k, r32(ib, k as usize * 4) as u64, &mut budget)?;
            }
            let roots = [(r32(ib, 48) as u64, 1u32, 12u64), (r32(ib, 52) as u64, 2, 12 + per), (r32(ib, 56) as u64, 3, 12 + per + per * per)];
            for (blk, level, base) in roots {
                if blk != 0 {
                    self.walk_indirect(blk, level, base, &mut exts, &mut meta, &mut seen, &mut budget)?;
                }
            }
        }
        self.check_mapping(&exts)?;
        Ok((exts, meta))
    }

    /// Append logical block `l` -> physical `p` of a block-mapped file (0 = hole).
    fn map_push(&self, exts: &mut Vec<Ext>, l: u64, p: u64, budget: &mut u64) -> FsResult<()> {
        if p == 0 {
            return Ok(());
        }
        if l > u32::MAX as u64 {
            return corrupt("block map beyond the maximum file size");
        }
        *budget = budget.checked_sub(1).ok_or_else(|| FsError::Corrupt("block map references more blocks than the volume holds".into()))?;
        if let Some(last) = exts.last_mut() {
            if last.lblk as u64 + last.len as u64 == l && last.pblk + last.len as u64 == p && last.len < u32::MAX {
                last.len += 1;
                return Ok(());
            }
        }
        exts.push(Ext { lblk: l as u32, pblk: p, len: 1, uninit: false });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_indirect(&self, blk: u64, level: u32, base: u64, exts: &mut Vec<Ext>, meta: &mut Vec<u64>, seen: &mut HashSet<u64>, budget: &mut u64) -> FsResult<()> {
        if !seen.insert(blk) {
            return corrupt(format!("indirect block {blk} referenced twice"));
        }
        let data = self.read_block(blk)?;
        meta.push(blk);
        let per = self.bs / 4;
        let span = per.pow(level - 1);
        for k in 0..per {
            let p = r32(&data, (k * 4) as usize) as u64;
            if p == 0 {
                continue;
            }
            if level == 1 {
                self.map_push(exts, base + k, p, budget)?;
            } else {
                self.walk_indirect(p, level - 1, base + k * span, exts, meta, seen, budget)?;
            }
        }
        Ok(())
    }

    /// `want_depth`: the depth the parent index node implies (None for the root in the inode).
    fn walk_extents(&self, node: &[u8], want_depth: Option<u16>, exts: &mut Vec<Ext>, meta: &mut Vec<u64>, seen: &mut HashSet<u64>) -> FsResult<()> {
        if r16(node, 0) != EXT_MAGIC {
            return corrupt("bad extent header magic");
        }
        let entries = r16(node, 2) as usize;
        let max = r16(node, 4) as usize;
        let depth = r16(node, 6);
        if entries > max || 12 + max * 12 > node.len() {
            return corrupt("bad extent header");
        }
        if depth > 5 || want_depth.map_or(false, |d| d != depth) {
            return corrupt("bad extent tree depth");
        }
        for k in 0..entries {
            let e = &node[12 + k * 12..24 + k * 12];
            if depth == 0 {
                let lblk = r32(e, 0);
                let mut len = r16(e, 4) as u32;
                let mut uninit = false;
                if len > EXT_INIT_MAX_LEN {
                    len -= EXT_INIT_MAX_LEN;
                    uninit = true;
                }
                let pblk = (r16(e, 6) as u64) << 32 | r32(e, 8) as u64;
                if len > 0 {
                    if exts.len() as u64 >= self.blocks_count {
                        return corrupt("extent tree maps more blocks than the volume holds");
                    }
                    exts.push(Ext { lblk, pblk, len, uninit });
                }
            } else {
                let leaf = (r16(e, 8) as u64) << 32 | r32(e, 4) as u64;
                if !seen.insert(leaf) {
                    return corrupt(format!("extent tree block {leaf} referenced twice"));
                }
                let child = self.read_block(leaf)?;
                meta.push(leaf);
                self.walk_extents(&child, Some(depth - 1), exts, meta, seen)?;
            }
        }
        Ok(())
    }

    /// Extents must be sorted, non-overlapping, below 2^32 logical blocks and inside the volume.
    fn check_mapping(&self, exts: &[Ext]) -> FsResult<()> {
        let mut end = 0u64;
        for e in exts {
            let s = e.lblk as u64;
            if s < end {
                return corrupt("unsorted or overlapping extents");
            }
            end = s + e.len as u64;
            if end > 1 << 32 {
                return corrupt("extent beyond the maximum file size");
            }
            if e.pblk == 0 || e.pblk.checked_add(e.len as u64).map_or(true, |x| x > self.blocks_count) {
                return corrupt(format!("extent at block {} points outside the volume", e.pblk));
            }
        }
        Ok(())
    }

    /// Rebuild the on-disk mapping structure for `inode` from `exts` (sorted, non-overlapping).
    /// Reuses `old_meta` blocks where possible; frees what is no longer needed.
    /// Also updates i_blocks. Does NOT write the inode.
    fn set_mapping(&mut self, inode: &mut Inode, exts: &[Ext], old_meta: Vec<u64>) -> FsResult<()> {
        let mut pool = old_meta;
        pool.reverse();
        let goal = exts.first().map(|e| e.pblk).unwrap_or_else(|| self.inode_goal(inode.ino));
        let mut used_meta: Vec<u64> = vec![];
        if inode.uses_extents() {
            self.build_extent_tree(inode, exts, &mut pool, &mut used_meta, goal)?;
        } else {
            self.build_blockmap(inode, exts, &mut pool, &mut used_meta, goal)?;
        }
        // Free leftover metadata blocks.
        let leftovers: Vec<(u64, u64)> = pool.into_iter().map(|b| (b, 1)).collect();
        self.free_blocks(&leftovers)?;
        let data: u64 = exts.iter().map(|e| e.len as u64).sum();
        let acl = (inode.file_acl() != 0) as u64;
        self.set_i_blocks(inode, data + used_meta.len() as u64 + acl);
        Ok(())
    }

    fn take_meta_block(&mut self, pool: &mut Vec<u64>, goal: u64) -> FsResult<u64> {
        if let Some(b) = pool.pop() {
            return Ok(b);
        }
        let r = self.alloc_blocks(goal, 1)?;
        Ok(r[0].0)
    }

    fn build_extent_tree(&mut self, inode: &mut Inode, exts: &[Ext], pool: &mut Vec<u64>, used: &mut Vec<u64>, goal: u64) -> FsResult<()> {
        // Split extents longer than the on-disk maximum.
        let mut list: Vec<Ext> = vec![];
        for e in exts {
            let maxlen = if e.uninit { EXT_INIT_MAX_LEN - 1 } else { EXT_INIT_MAX_LEN };
            let mut off = 0;
            while off < e.len {
                let l = (e.len - off).min(maxlen);
                list.push(Ext { lblk: e.lblk + off, pblk: e.pblk + off as u64, len: l, uninit: e.uninit });
                off += l;
            }
        }
        let per_block = ((self.bs - 12) / 12) as usize;
        let gen = inode.generation();
        let seed = self.inode_seed(inode.ino, gen);
        let tail_ok = self.csum;
        let ib = inode.i_block_mut();
        let old_gen = r32(ib, 8);
        if list.len() <= 4 {
            ib.fill(0);
            w16(ib, 0, EXT_MAGIC);
            w16(ib, 2, list.len() as u16);
            w16(ib, 4, 4);
            w16(ib, 6, 0);
            w32(ib, 8, old_gen);
            for (k, e) in list.iter().enumerate() {
                Self::put_leaf(&mut ib[12 + k * 12..24 + k * 12], e);
            }
            return Ok(());
        }
        // Build leaves, then index levels until the top fits into the inode (4 entries).
        // Each level: Vec<(first_lblk, block)>.
        let mut level_nodes: Vec<(u32, u64)> = vec![];
        for chunk in list.chunks(per_block) {
            let blk = self.take_meta_block(pool, goal)?;
            used.push(blk);
            let mut b = vec![0u8; self.bs as usize];
            w16(&mut b, 0, EXT_MAGIC);
            w16(&mut b, 2, chunk.len() as u16);
            w16(&mut b, 4, per_block as u16);
            w16(&mut b, 6, 0);
            for (k, e) in chunk.iter().enumerate() {
                Self::put_leaf(&mut b[12 + k * 12..24 + k * 12], e);
            }
            if tail_ok {
                let t = 12 + per_block * 12;
                let c = crc32c_raw(seed, &b[..t]);
                w32(&mut b, t, c);
            }
            self.write_block(blk, &b)?;
            level_nodes.push((chunk[0].lblk, blk));
        }
        let mut depth = 1u16;
        while level_nodes.len() > 4 {
            let mut next = vec![];
            for chunk in level_nodes.chunks(per_block) {
                let blk = self.take_meta_block(pool, goal)?;
                used.push(blk);
                let mut b = vec![0u8; self.bs as usize];
                w16(&mut b, 0, EXT_MAGIC);
                w16(&mut b, 2, chunk.len() as u16);
                w16(&mut b, 4, per_block as u16);
                w16(&mut b, 6, depth);
                for (k, (l, child)) in chunk.iter().enumerate() {
                    Self::put_index(&mut b[12 + k * 12..24 + k * 12], *l, *child);
                }
                if tail_ok {
                    let t = 12 + per_block * 12;
                    let c = crc32c_raw(seed, &b[..t]);
                    w32(&mut b, t, c);
                }
                self.write_block(blk, &b)?;
                next.push((chunk[0].0, blk));
            }
            level_nodes = next;
            depth += 1;
        }
        let ib = inode.i_block_mut();
        ib.fill(0);
        w16(ib, 0, EXT_MAGIC);
        w16(ib, 2, level_nodes.len() as u16);
        w16(ib, 4, 4);
        w16(ib, 6, depth);
        w32(ib, 8, old_gen);
        for (k, (l, child)) in level_nodes.iter().enumerate() {
            Self::put_index(&mut ib[12 + k * 12..24 + k * 12], *l, *child);
        }
        Ok(())
    }

    fn put_leaf(e: &mut [u8], x: &Ext) {
        w32(e, 0, x.lblk);
        w16(e, 4, if x.uninit { (x.len + EXT_INIT_MAX_LEN) as u16 } else { x.len as u16 });
        w16(e, 6, (x.pblk >> 32) as u16);
        w32(e, 8, x.pblk as u32);
    }

    fn put_index(e: &mut [u8], lblk: u32, child: u64) {
        w32(e, 0, lblk);
        w32(e, 4, child as u32);
        w16(e, 8, (child >> 32) as u16);
        w16(e, 10, 0);
    }

    fn build_blockmap(&mut self, inode: &mut Inode, exts: &[Ext], pool: &mut Vec<u64>, used: &mut Vec<u64>, goal: u64) -> FsResult<()> {
        let per = self.bs / 4;
        let mut direct = [0u32; 12];
        // level-1 tables: key = index of the single-indirect table (in units of `per` logical blocks after 12)
        let mut tables: std::collections::BTreeMap<u64, Vec<u32>> = std::collections::BTreeMap::new();
        for e in exts {
            for k in 0..e.len as u64 {
                let l = e.lblk as u64 + k;
                let p = e.pblk + k;
                if p > u32::MAX as u64 {
                    return Err(FsError::Unsupported("block above 2^32 in block-mapped file".into()));
                }
                if l < 12 {
                    direct[l as usize] = p as u32;
                } else {
                    let rel = l - 12;
                    let t = rel / per;
                    tables.entry(t).or_insert_with(|| vec![0u32; per as usize])[(rel % per) as usize] = p as u32;
                }
            }
        }
        if tables.keys().any(|&t| t >= 1 + per + per * per) {
            return Err(FsError::NoSpace);
        }
        let write_table = |this: &mut Self, entries: &[u32], used: &mut Vec<u64>, pool: &mut Vec<u64>| -> FsResult<u64> {
            let blk = this.take_meta_block(pool, goal)?;
            used.push(blk);
            let mut b = vec![0u8; this.bs as usize];
            for (k, v) in entries.iter().enumerate() {
                w32(&mut b, k * 4, *v);
            }
            this.write_block(blk, &b)?;
            Ok(blk)
        };
        let mut ind = 0u64;
        let mut dind_entries = vec![0u32; per as usize];
        let mut tind_l2: std::collections::BTreeMap<u64, Vec<u32>> = std::collections::BTreeMap::new();
        for (t, entries) in &tables {
            let blk = write_table(self, entries, used, pool)?;
            if *t == 0 {
                ind = blk;
            } else if *t <= per {
                dind_entries[(*t - 1) as usize] = blk as u32;
            } else {
                let r = *t - 1 - per; // index among triple-indirect leaf tables
                tind_l2.entry(r / per).or_insert_with(|| vec![0u32; per as usize])[(r % per) as usize] = blk as u32;
            }
        }
        let dind = if dind_entries.iter().any(|&x| x != 0) { write_table(self, &dind_entries, used, pool)? } else { 0 };
        let tind = if !tind_l2.is_empty() {
            let mut top = vec![0u32; per as usize];
            for (k, entries) in &tind_l2 {
                top[*k as usize] = write_table(self, entries, used, pool)? as u32;
            }
            write_table(self, &top, used, pool)?
        } else {
            0
        };
        let ib = inode.i_block_mut();
        ib.fill(0);
        for (k, v) in direct.iter().enumerate() {
            w32(ib, k * 4, *v);
        }
        w32(ib, 48, ind as u32);
        w32(ib, 52, dind as u32);
        w32(ib, 56, tind as u32);
        Ok(())
    }

    fn inode_goal(&self, ino: u32) -> u64 {
        let g = (ino - 1) / self.ipg;
        self.group_first_block(g)
    }

    fn is_fast_symlink(&self, inode: &Inode) -> bool {
        if inode.flags() & FL_EXTENTS != 0 || inode.flags() & FL_INLINE_DATA != 0 {
            return false;
        }
        let acl_blocks = if inode.file_acl() != 0 { self.bs } else { 0 };
        self.i_blocks(inode) <= acl_blocks && inode.size() < 60
    }

    // ─────────────── file data ───────────────

    fn lookup_block(exts: &[Ext], lblk: u32) -> Option<(usize, u64, bool)> {
        // binary search
        let idx = exts.partition_point(|e| e.lblk as u64 + e.len as u64 <= lblk as u64);
        if idx < exts.len() && exts[idx].lblk <= lblk {
            let e = &exts[idx];
            return Some((idx, e.pblk + (lblk - e.lblk) as u64, e.uninit));
        }
        None
    }

    fn read_data(&self, inode: &Inode, exts: &[Ext], offset: u64, len: usize) -> FsResult<Vec<u8>> {
        let size = inode.size();
        if offset >= size {
            return Ok(vec![]);
        }
        let len = (len as u64).min(size - offset) as usize;
        let mut out = vec![0u8; len];
        let bs = self.bs;
        let mut pos = offset;
        // Logical blocks are u32: bytes at or beyond 2^32 blocks can only be a hole (a corrupt
        // i_size can claim them). Without the clamp `pos / bs` would wrap and map them onto
        // the first blocks of the file.
        let end = (offset + len as u64).min((1u64 << 32) * bs);
        while pos < end {
            let lblk = (pos / bs) as u32;
            let in_blk = pos % bs;
            match Self::lookup_block(exts, lblk) {
                Some((idx, pblk, uninit)) => {
                    let e = &exts[idx];
                    // contiguous bytes available in this extent
                    let ext_end_byte = (e.lblk as u64 + e.len as u64) * bs;
                    let chunk = (end.min(ext_end_byte) - pos) as usize;
                    let o = (pos - offset) as usize;
                    if !uninit {
                        self.dev.read_at(pblk * bs + in_blk, &mut out[o..o + chunk])?;
                    }
                    pos += chunk as u64;
                }
                None => {
                    // hole until next extent
                    // Hole: the output is pre-zeroed, skip to the next extent (or the end).
                    let idx = exts.partition_point(|e| e.lblk <= lblk);
                    let next = if idx < exts.len() { exts[idx].lblk as u64 * bs } else { end };
                    pos = next.min(end);
                }
            }
        }
        Ok(out)
    }

    /// Ensure blocks [first, last] are allocated & initialised; return updated mapping.
    fn ensure_blocks(&mut self, inode: &Inode, exts: &mut Vec<Ext>, first: u32, last: u32, write_lo: u64, write_hi: u64) -> FsResult<bool> {
        let bs = self.bs;
        let mut changed = false;
        // 1) Initialise uninitialised extents overlapping the range (zero the parts not
        //    covered by the write, then flag as initialised).
        let mut k = 0;
        while k < exts.len() {
            let e = exts[k];
            let e_end = e.lblk as u64 + e.len as u64;
            if e.uninit && (e.lblk as u64) <= last as u64 && e_end > first as u64 {
                let s = (e.lblk.max(first)) as u64;
                let t = (e_end - 1).min(last as u64);
                // split into [e.lblk, s) uninit, [s, t] init, (t, e_end) uninit
                let mut parts = vec![];
                if s > e.lblk as u64 {
                    parts.push(Ext { lblk: e.lblk, pblk: e.pblk, len: (s - e.lblk as u64) as u32, uninit: true });
                }
                parts.push(Ext { lblk: s as u32, pblk: e.pblk + (s - e.lblk as u64), len: (t - s + 1) as u32, uninit: false });
                if t + 1 < e_end {
                    parts.push(Ext { lblk: (t + 1) as u32, pblk: e.pblk + (t + 1 - e.lblk as u64), len: (e_end - t - 1) as u32, uninit: true });
                }
                // zero the bytes of [s,t] not covered by the write
                for b in s..=t {
                    let bstart = b * bs;
                    let bend = bstart + bs;
                    if write_lo <= bstart && write_hi >= bend {
                        continue;
                    }
                    let pb = e.pblk + (b - e.lblk as u64);
                    self.dev.write_data_at(pb * bs, &vec![0u8; bs as usize])?;
                }
                let n = parts.len();
                exts.splice(k..k + 1, parts);
                k += n;
                changed = true;
            } else {
                k += 1;
            }
        }
        // 2) Fill holes.
        let mut l = first;
        while l <= last {
            if let Some((idx, _, _)) = Self::lookup_block(exts, l) {
                let e = exts[idx];
                l = (e.lblk + e.len).max(l + 1);
                continue;
            }
            // hole from l to min(last, next extent start - 1)
            let idx = exts.partition_point(|e| e.lblk <= l);
            let hole_end = if idx < exts.len() { (exts[idx].lblk - 1).min(last) } else { last };
            let count = (hole_end - l + 1) as u64;
            let goal = {
                let prev = exts.partition_point(|e| e.lblk < l);
                if prev > 0 {
                    let p = exts[prev - 1];
                    p.pblk + p.len as u64 + (l - (p.lblk + p.len)) as u64
                } else {
                    self.inode_goal(inode.ino)
                }
            };
            let runs = self.alloc_blocks(goal, count)?;
            let mut cur = l;
            for (pb, len) in runs {
                // zero partial blocks not fully covered by the write
                for k in 0..len {
                    let lb = cur as u64 + k;
                    let bstart = lb * bs;
                    if !(write_lo <= bstart && write_hi >= bstart + bs) {
                        self.dev.write_data_at((pb + k) * bs, &vec![0u8; bs as usize])?;
                    }
                }
                let ne = Ext { lblk: cur, pblk: pb, len: len as u32, uninit: false };
                let pos = exts.partition_point(|e| e.lblk < cur);
                exts.insert(pos, ne);
                cur += len as u32;
            }
            changed = true;
            l = hole_end + 1;
        }
        if changed {
            Self::merge_exts(exts);
        }
        Ok(changed)
    }

    fn merge_exts(exts: &mut Vec<Ext>) {
        let mut out: Vec<Ext> = Vec::with_capacity(exts.len());
        for e in exts.drain(..) {
            if let Some(last) = out.last_mut() {
                if last.uninit == e.uninit
                    && last.lblk + last.len == e.lblk
                    && last.pblk + last.len as u64 == e.pblk
                    && (last.len as u64 + e.len as u64) < (u32::MAX as u64)
                {
                    last.len += e.len;
                    continue;
                }
            }
            out.push(e);
        }
        *exts = out;
    }

    fn write_data(&mut self, inode: &mut Inode, offset: u64, data: &[u8]) -> FsResult<()> {
        if data.is_empty() {
            return Ok(());
        }
        let bs = self.bs;
        let end = offset.checked_add(data.len() as u64).ok_or_else(|| FsError::Invalid("file too large".into()))?;
        let max_blocks = if inode.uses_extents() { u32::MAX as u64 } else { (12 + (bs / 4) + (bs / 4).pow(2) + (bs / 4).pow(3)).min(u32::MAX as u64) };
        if end.div_ceil(bs) > max_blocks {
            return Err(FsError::Invalid("file too large".into()));
        }
        let (mut exts, meta) = self.get_mapping(inode)?;
        let first = (offset / bs) as u32;
        let last = ((end - 1) / bs) as u32;
        // If extending past EOF, the old last partial block's tail is already zero on disk
        // (we zero on truncate and on allocation), so no extra work is needed.
        let changed = self.ensure_blocks(inode, &mut exts, first, last, offset, end)?;
        // Write payload in physically contiguous chunks.
        let mut pos = offset;
        while pos < end {
            let lblk = (pos / bs) as u32;
            let (idx, pblk, _) = Self::lookup_block(&exts, lblk).ok_or_else(|| FsError::Corrupt("mapping hole after allocation".into()))?;
            let e = exts[idx];
            let ext_end = (e.lblk as u64 + e.len as u64) * bs;
            let chunk = (end.min(ext_end) - pos) as usize;
            let o = (pos - offset) as usize;
            self.dev.write_data_at(pblk * bs + pos % bs, &data[o..o + chunk])?;
            pos += chunk as u64;
        }
        if changed {
            self.set_mapping(inode, &exts, meta)?;
        }
        if end > inode.size() {
            inode.set_size(end);
            self.maybe_set_large_file(end)?;
        }
        Ok(())
    }

    fn maybe_set_large_file(&mut self, size: u64) -> FsResult<()> {
        if size > 0x7FFF_FFFF && self.ro_compat & RO_LARGE_FILE == 0 {
            self.ro_compat |= RO_LARGE_FILE;
            w32(&mut self.sb, S_RO_COMPAT, self.ro_compat);
            self.write_sb()?;
        }
        Ok(())
    }

    fn truncate(&mut self, inode: &mut Inode, new_size: u64) -> FsResult<()> {
        let old = inode.size();
        if new_size >= old {
            inode.set_size(new_size);
            self.maybe_set_large_file(new_size)?;
            return Ok(());
        }
        let bs = self.bs;
        let (mut exts, meta) = self.get_mapping(inode)?;
        let keep_blocks = new_size.div_ceil(bs).min(u32::MAX as u64) as u32;
        let mut to_free = vec![];
        let mut out = vec![];
        for e in exts.drain(..) {
            if e.lblk >= keep_blocks {
                to_free.push((e.pblk, e.len as u64));
            } else if e.lblk + e.len > keep_blocks {
                let keep = keep_blocks - e.lblk;
                to_free.push((e.pblk + keep as u64, (e.len - keep) as u64));
                out.push(Ext { len: keep, ..e });
            } else {
                out.push(e);
            }
        }
        // zero the tail of the new last block
        if new_size % bs != 0 && new_size / bs <= u32::MAX as u64 {
            let lblk = (new_size / bs) as u32;
            if let Some((_, pblk, uninit)) = Self::lookup_block(&out, lblk) {
                if !uninit {
                    let o = new_size % bs;
                    self.dev.write_data_at(pblk * bs + o, &vec![0u8; (bs - o) as usize])?;
                }
            }
        }
        self.set_mapping(inode, &out, meta)?;
        self.free_blocks(&to_free)?;
        inode.set_size(new_size);
        Ok(())
    }

    /// Release every block of an inode (data, tree, xattr) — used on final unlink.
    fn release_inode_blocks(&mut self, inode: &mut Inode) -> FsResult<()> {
        let fast = inode.kind() == FileKind::Symlink && self.is_fast_symlink(inode);
        if !fast && inode.flags() & FL_INLINE_DATA == 0 {
            let (exts, meta) = self.get_mapping(inode)?;
            let mut runs: Vec<(u64, u64)> = exts.iter().map(|e| (e.pblk, e.len as u64)).collect();
            runs.extend(meta.iter().map(|&b| (b, 1)));
            self.free_blocks(&runs)?;
        }
        let acl = inode.file_acl();
        if acl != 0 {
            let mut b = self.read_block(acl)?;
            if r32(&b, 0) == 0xEA02_0000 {
                let refc = r32(&b, 4);
                if refc <= 1 {
                    self.free_blocks(&[(acl, 1)])?;
                } else {
                    w32(&mut b, 4, refc - 1);
                    if self.csum {
                        w32(&mut b, 0x10, 0);
                        let mut c = crc32c_raw(self.seed, &acl.to_le_bytes());
                        c = crc32c_raw(c, &b);
                        w32(&mut b, 0x10, c);
                    }
                    self.write_block(acl, &b)?;
                }
            }
            w32(&mut inode.raw, I_FILE_ACL_LO, 0);
            w16(&mut inode.raw, I_FILE_ACL_HI, 0);
        }
        if inode.uses_extents() {
            Self::init_extent_root(inode.i_block_mut());
        } else {
            inode.i_block_mut().fill(0);
        }
        self.set_i_blocks(inode, 0);
        inode.set_size(0);
        Ok(())
    }

    // ─────────────── directories ───────────────

    fn parse_dir_block(&self, b: &[u8]) -> Vec<RawDirent> {
        let mut v = vec![];
        let mut off = 0usize;
        let ft = self.incompat & INCOMPAT_FILETYPE != 0;
        while off + 8 <= b.len() {
            let inode = r32(b, off);
            let rec_len = r16(b, off + 4) as usize;
            let rec_len = if rec_len == 0 && self.bs >= 65536 { 65536 } else { rec_len };
            let (name_len, ftype) = if ft { (b[off + 6] as usize, b[off + 7]) } else { (r16(b, off + 6) as usize, 0) };
            if rec_len < 8 || off + rec_len > b.len() || (inode != 0 && 8 + name_len > rec_len) {
                break;
            }
            v.push(RawDirent { off, inode, rec_len, name_len, ftype });
            off += rec_len;
        }
        v
    }

    fn is_tail(d: &RawDirent, bs: usize) -> bool {
        d.inode == 0 && d.rec_len == 12 && d.name_len == 0 && d.ftype == 0xDE && d.off == bs - 12
    }

    fn dir_blocks(&self, dir: &Inode) -> FsResult<Vec<(u32, u64)>> {
        let (exts, _) = self.get_mapping(dir)?;
        // A directory can't be larger than the volume (nor than 2^32 blocks).
        let n = dir.size().div_ceil(self.bs).min(self.blocks_count).min(u32::MAX as u64);
        let mut v = vec![];
        for e in &exts {
            for k in 0..e.len as u64 {
                let l = e.lblk as u64 + k;
                if l >= n {
                    break;
                }
                v.push((l as u32, e.pblk + k));
            }
        }
        Ok(v)
    }

    /// Iterate (name bytes, inode, ftype) entries.
    fn dir_entries(&self, dir: &Inode) -> FsResult<Vec<(Vec<u8>, u32, u8)>> {
        let mut out = vec![];
        for (_, pblk) in self.dir_blocks(dir)? {
            let b = self.read_block(pblk)?;
            for d in self.parse_dir_block(&b) {
                if d.inode != 0 && d.name_len > 0 {
                    out.push((b[d.off + 8..d.off + 8 + d.name_len].to_vec(), d.inode, d.ftype));
                }
            }
        }
        Ok(out)
    }

    /// Inode numbers a directory entry may name (the kernel's ext4_valid_inum): the root or a
    /// non-reserved inode that exists. Anything else (journal, resize inode, out of range) is
    /// corruption and must never be unlinked or followed.
    fn valid_dirent_ino(&self, ino: u32) -> bool {
        ino == ROOT_INO || (ino >= self.first_ino && ino as u64 <= self.ipg as u64 * self.ngroups as u64)
    }

    fn find_entry(&self, dir: &Inode, name: &[u8]) -> FsResult<Option<(u32, u8)>> {
        for (_, pblk) in self.dir_blocks(dir)? {
            let b = self.read_block(pblk)?;
            for d in self.parse_dir_block(&b) {
                if d.inode != 0 && d.name_len == name.len() && &b[d.off + 8..d.off + 8 + d.name_len] == name {
                    if !self.valid_dirent_ino(d.inode) {
                        return corrupt(format!("directory entry names invalid inode {}", d.inode));
                    }
                    return Ok(Some((d.inode, d.ftype)));
                }
            }
        }
        Ok(None)
    }

    fn set_dir_tail_csum(&self, dir: &Inode, b: &mut [u8]) {
        if !self.csum {
            return;
        }
        let bs = b.len();
        if r32(b, bs - 12) == 0 && r16(b, bs - 8) == 12 && b[bs - 5] == 0xDE {
            let seed = self.inode_seed(dir.ino, dir.generation());
            let c = crc32c_raw(seed, &b[..bs - 12]);
            w32(b, bs - 4, c);
        }
    }

    fn write_dir_block(&self, dir: &Inode, pblk: u64, b: &mut [u8]) -> FsResult<()> {
        self.set_dir_tail_csum(dir, b);
        self.write_block(pblk, b)
    }

    fn put_tail(b: &mut [u8]) {
        let bs = b.len();
        b[bs - 12..].fill(0);
        w16(b, bs - 8, 12);
        b[bs - 5] = 0xDE;
    }

    fn put_dirent(&self, b: &mut [u8], off: usize, inode: u32, rec_len: usize, name: &[u8], ftype: u8) {
        w32(b, off, inode);
        w16(b, off + 4, rec_len as u16);
        if self.incompat & INCOMPAT_FILETYPE != 0 {
            b[off + 6] = name.len() as u8;
            b[off + 7] = ftype;
        } else {
            w16(b, off + 6, name.len() as u16);
        }
        b[off + 8..off + 8 + name.len()].copy_from_slice(name);
        // zero padding
        let pad_end = (off + rec_len_for(name.len())).min(off + rec_len);
        for x in &mut b[off + 8 + name.len()..pad_end] {
            *x = 0;
        }
    }

    fn empty_dir_block(&self) -> Vec<u8> {
        let bs = self.bs as usize;
        let mut b = vec![0u8; bs];
        let tail = if self.csum { 12 } else { 0 };
        w32(&mut b, 0, 0);
        w16(&mut b, 4, (bs - tail) as u16);
        if tail > 0 {
            Self::put_tail(&mut b);
        }
        b
    }

    /// Convert an htree-indexed directory into an equivalent linear one (valid ext2 layout).
    fn linearize_dir(&mut self, dir: &mut Inode) -> FsResult<()> {
        if dir.flags() & FL_INDEX == 0 {
            return Ok(());
        }
        let blocks = self.dir_blocks(dir)?;
        let bs = self.bs as usize;
        let tail = if self.csum { 12 } else { 0 };
        let map: HashMap<u32, u64> = blocks.iter().cloned().collect();
        // collect dx internal node logical blocks
        let mut nodes: HashSet<u32> = HashSet::new();
        if let Some(&p0) = map.get(&0) {
            let root = self.read_block(p0)?;
            // dx_root: at most 3 index levels (largedir), entries bounded by the block.
            let levels = (root[0x1E] as u32).min(3);
            let count = (r16(&root, 0x22) as usize).min((bs - 0x20) / 8);
            let mut frontier: Vec<u32> = (0..count).map(|i| r32(&root, 0x20 + 8 * i + 4) & 0x0FFF_FFFF).collect();
            for _ in 0..levels {
                let mut next = vec![];
                for l in frontier {
                    let Some(&p) = map.get(&l) else { continue };
                    let nb = self.read_block(p)?;
                    // Only blocks that really look like dx nodes (one empty fake dirent
                    // spanning the block) are wiped below: with a corrupt `levels` the
                    // frontier may name leaf blocks, whose entries must survive.
                    let rl = r16(&nb, 4) as usize;
                    let fake = r32(&nb, 0) == 0 && (rl == bs || (rl == 0 && bs >= 65536));
                    if l != 0 && fake && nodes.insert(l) {
                        let c = r16(&nb, 0x0A) as usize;
                        for i in 0..c.min((bs - 8) / 8) {
                            next.push(r32(&nb, 0x08 + 8 * i + 4) & 0x0FFF_FFFF);
                        }
                    }
                }
                frontier = next;
            }
            // rewrite root block: keep '.' and '..', drop dx data
            let mut nb = vec![0u8; bs];
            nb[..24].copy_from_slice(&root[..24]);
            w16(&mut nb, 4, 12);
            w16(&mut nb, 16, (bs - 12 - tail) as u16);
            if tail > 0 {
                Self::put_tail(&mut nb);
            }
            self.write_dir_block(dir, p0, &mut nb)?;
        }
        for l in nodes {
            if let Some(&p) = map.get(&l) {
                let mut nb = self.empty_dir_block();
                self.write_dir_block(dir, p, &mut nb)?;
            }
        }
        // make sure every leaf block has a csum tail if required
        if self.csum {
            for &(l, p) in &blocks {
                if l == 0 {
                    continue;
                }
                let mut b = self.read_block(p)?;
                let ents = self.parse_dir_block(&b);
                if ents.last().map(|d| Self::is_tail(d, bs)).unwrap_or(false) {
                    continue;
                }
                // Try to carve out a tail from the last entry's slack.
                if let Some(last) = ents.last() {
                    let used = if last.inode != 0 { rec_len_for(last.name_len) } else { 0 };
                    if last.off + last.rec_len == bs && last.rec_len - used >= 12 {
                        w16(&mut b, last.off + 4, (last.rec_len - 12) as u16);
                        Self::put_tail(&mut b);
                        self.write_dir_block(dir, p, &mut b)?;
                        continue;
                    }
                }
                return Err(FsError::Unsupported("htree leaf without room for checksum tail".into()));
            }
        }
        let f = dir.flags() & !FL_INDEX;
        dir.set_flags(f);
        self.write_inode(dir)?;
        Ok(())
    }

    fn add_entry(&mut self, dir: &mut Inode, name: &[u8], ino: u32, kind: FileKind) -> FsResult<()> {
        self.linearize_dir(dir)?;
        let need = rec_len_for(name.len());
        let bs = self.bs as usize;
        for (_, pblk) in self.dir_blocks(dir)? {
            let mut b = self.read_block(pblk)?;
            let ents = self.parse_dir_block(&b);
            for d in &ents {
                if Self::is_tail(d, bs) {
                    continue;
                }
                let used = if d.inode != 0 { rec_len_for(d.name_len) } else { 0 };
                if d.rec_len >= used + need {
                    if d.inode == 0 {
                        self.put_dirent(&mut b, d.off, ino, d.rec_len, name, ftype_code(kind));
                    } else {
                        w16(&mut b, d.off + 4, used as u16);
                        let no = d.off + used;
                        self.put_dirent(&mut b, no, ino, d.rec_len - used, name, ftype_code(kind));
                    }
                    self.write_dir_block(dir, pblk, &mut b)?;
                    return Ok(());
                }
            }
        }
        // Need a new block.
        let lblk = (dir.size() / self.bs) as u32;
        let (mut exts, meta) = self.get_mapping(dir)?;
        let goal = exts.last().map(|e| e.pblk + e.len as u64).unwrap_or_else(|| self.inode_goal(dir.ino));
        let r = self.alloc_blocks(goal, 1)?;
        let pblk = r[0].0;
        let mut b = self.empty_dir_block();
        let rl = r16(&b, 4) as usize;
        self.put_dirent(&mut b, 0, ino, rl, name, ftype_code(kind));
        self.write_dir_block(dir, pblk, &mut b)?;
        exts.push(Ext { lblk, pblk, len: 1, uninit: false });
        Self::merge_exts(&mut exts);
        self.set_mapping(dir, &exts, meta)?;
        dir.set_size(dir.size() + self.bs);
        self.write_inode(dir)?;
        Ok(())
    }

    fn remove_entry(&mut self, dir: &mut Inode, name: &[u8]) -> FsResult<u32> {
        self.linearize_dir(dir)?;
        for (_, pblk) in self.dir_blocks(dir)? {
            let mut b = self.read_block(pblk)?;
            let ents = self.parse_dir_block(&b);
            for (k, d) in ents.iter().enumerate() {
                if d.inode != 0 && d.name_len == name.len() && &b[d.off + 8..d.off + 8 + d.name_len] == name {
                    let ino = d.inode;
                    if k > 0 {
                        let p = &ents[k - 1];
                        w16(&mut b, p.off + 4, (p.rec_len + d.rec_len) as u16);
                    } else {
                        w32(&mut b, d.off, 0);
                    }
                    self.write_dir_block(dir, pblk, &mut b)?;
                    return Ok(ino);
                }
            }
        }
        Err(FsError::NotFound)
    }

    fn set_dotdot(&mut self, dir: &Inode, parent: u32) -> FsResult<()> {
        let blocks = self.dir_blocks(dir)?;
        let Some(&(_, p0)) = blocks.iter().find(|(l, _)| *l == 0) else {
            return corrupt("directory without blocks");
        };
        let mut b = self.read_block(p0)?;
        let ents = self.parse_dir_block(&b);
        if ents.len() < 2 || ents[1].name_len != 2 || &b[ents[1].off + 8..ents[1].off + 10] != b".." {
            return corrupt("directory without '..'");
        }
        w32(&mut b, ents[1].off, parent);
        self.write_dir_block(dir, p0, &mut b)
    }

    fn dir_is_empty(&self, dir: &Inode) -> FsResult<bool> {
        Ok(self.dir_entries(dir)?.iter().all(|(n, _, _)| n == b"." || n == b".."))
    }

    fn inc_links(&self, i: &mut Inode) {
        let l = i.links();
        if i.kind() == FileKind::Dir && (l >= 64999 || l == 1) && self.ro_compat & RO_DIR_NLINK != 0 {
            i.set_links(1);
        } else {
            i.set_links(l + 1);
        }
    }

    fn dec_links(&self, i: &mut Inode) {
        let l = i.links();
        if i.kind() == FileKind::Dir && l == 1 {
            return; // dir_nlink overflow mode
        }
        i.set_links(l.saturating_sub(1));
    }

    fn touch_mc(i: &mut Inode) {
        let now = Timestamp::now();
        i.set_time(I_MTIME, I_MTIME_EXTRA, now);
        i.set_time(I_CTIME, I_CTIME_EXTRA, now);
    }

    fn make_node(&mut self, parent: Ino, name: &str, mode: u16, uid: u32, gid: u32) -> FsResult<(Inode, Inode)> {
        self.check_rw()?;
        check_name(name, 255)?;
        let mut p = self.live_inode(parent)?;
        if p.kind() != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        if self.find_entry(&p, name.as_bytes())?.is_some() {
            return Err(FsError::Exists);
        }
        let is_dir = mode & S_IFMT == S_IFDIR;
        let ino = self.alloc_inode(p.ino, is_dir)?;
        let n = self.new_inode(ino, mode, uid, gid);
        let _ = &mut p;
        Ok((p, n))
    }

    fn finish_unlink(&mut self, mut target: Inode) -> FsResult<()> {
        self.dec_links(&mut target);
        let now = Timestamp::now();
        target.set_time(I_CTIME, I_CTIME_EXTRA, now);
        if target.links() == 0 {
            self.release_inode_blocks(&mut target)?;
            w32(&mut target.raw, I_DTIME, now.secs as u32);
            self.write_inode(&mut target)?;
            self.free_inode(target.ino, false)?;
        } else {
            self.write_inode(&mut target)?;
        }
        Ok(())
    }

    fn remove_dir_inode(&mut self, parent: &mut Inode, mut d: Inode) -> FsResult<()> {
        d.set_links(0);
        self.release_inode_blocks(&mut d)?;
        w32(&mut d.raw, I_DTIME, Timestamp::now().secs as u32);
        self.write_inode(&mut d)?;
        self.free_inode(d.ino, true)?;
        self.dec_links(parent);
        Ok(())
    }

    fn is_ancestor(&self, anc: u32, mut node: u32) -> FsResult<bool> {
        for _ in 0..4096 {
            if node == anc {
                return Ok(true);
            }
            if node == ROOT_INO {
                return Ok(false);
            }
            let i = self.read_inode(node)?;
            node = match self.find_entry(&i, b"..")? {
                Some((p, _)) => p,
                None => return Ok(false),
            };
        }
        corrupt("directory loop")
    }

    // ─────────────── Journal Doctor ───────────────

    fn journal_inode(&self) -> Option<u32> {
        if self.compat & COMPAT_HAS_JOURNAL == 0 {
            return None;
        }
        let j = r32(&self.sb, S_JOURNAL_INUM);
        Some(if j == 0 { JOURNAL_INO } else { j })
    }

    /// Replay a dirty jbd2 journal. Returns (transactions, blocks replayed, revoked).
    fn replay_journal(&mut self) -> FsResult<(u32, u32, u32)> {
        let Some(jino) = self.journal_inode() else { return Ok((0, 0, 0)) };
        let ji = self.read_inode(jino)?;
        let (exts, _) = self.get_mapping(&ji)?;
        let jmap = |l: u64| -> FsResult<u64> {
            Self::lookup_block(&exts, l as u32).map(|(_, p, _)| p).ok_or_else(|| FsError::Corrupt("journal hole".into()))
        };
        let jsb_blk = jmap(0)?;
        let mut jsb = self.read_block(jsb_blk)?;
        if rbe32(&jsb, 0) != 0xC03B_3998 {
            return corrupt("bad journal superblock magic");
        }
        let jbs = rbe32(&jsb, 0x0C) as u64;
        if jbs != self.bs {
            return Err(FsError::Unsupported("journal block size differs from fs".into()));
        }
        // The log can't be longer than the journal inode actually maps.
        let mapped = exts.last().map_or(0, |e| e.lblk as u64 + e.len as u64).min(ji.size() / self.bs);
        let maxlen = (rbe32(&jsb, 0x10) as u64).min(mapped);
        let first = rbe32(&jsb, 0x14) as u64;
        let mut seq = rbe32(&jsb, 0x18);
        let start = rbe32(&jsb, 0x1C) as u64;
        let v2 = rbe32(&jsb, 4) == 4;
        let incompat = if v2 { rbe32(&jsb, 0x28) } else { 0 };
        if start == 0 {
            return Ok((0, 0, 0));
        }
        if first == 0 || first >= maxlen || start < first || start >= maxlen {
            return corrupt("journal superblock: log bounds outside the journal");
        }
        let is64 = incompat & 0x2 != 0;
        let csum_v2 = incompat & 0x8 != 0;
        let csum_v3 = incompat & 0x10 != 0;
        let tag_bytes: usize = if csum_v3 {
            16
        } else {
            let mut sz = 12;
            if csum_v2 {
                sz += 2;
            }
            if is64 {
                sz
            } else {
                sz - 4
            }
        };
        let desc_tail = if csum_v2 || csum_v3 { 4 } else { 0 };
        let next = |b: u64| if b + 1 >= maxlen { first } else { b + 1 };

        // Pass 1: scan & collect.
        struct Logged {
            seq: u32,
            target: u64,
            jblk: u64,
            escape: bool,
        }
        let mut logged: Vec<Logged> = vec![];
        let mut revoked: HashMap<u64, u32> = HashMap::new();
        let mut pos = start;
        let mut txns = 0u32;
        let mut guard = 0u64;
        'outer: loop {
            let mut pending: Vec<Logged> = vec![];
            let mut pending_rev: Vec<u64> = vec![];
            loop {
                guard += 1;
                if guard > maxlen * 2 {
                    break 'outer;
                }
                let b = self.read_block(jmap(pos)?)?;
                if rbe32(&b, 0) != 0xC03B_3998 || rbe32(&b, 8) != seq {
                    break 'outer;
                }
                match rbe32(&b, 4) {
                    1 => {
                        // descriptor
                        let mut off = 12;
                        let limit = self.bs as usize - desc_tail;
                        let mut dpos = next(pos);
                        loop {
                            if off + tag_bytes > limit {
                                break;
                            }
                            let lo = rbe32(&b, off) as u64;
                            let (flags, hi) = if csum_v3 {
                                (rbe32(&b, off + 4), if is64 { rbe32(&b, off + 8) as u64 } else { 0 })
                            } else {
                                let f = u16::from_be_bytes([b[off + 6], b[off + 7]]) as u32;
                                (f, if is64 { rbe32(&b, off + 8) as u64 } else { 0 })
                            };
                            pending.push(Logged { seq, target: hi << 32 | lo, jblk: dpos, escape: flags & 1 != 0 });
                            if (pending.len() + logged.len()) as u64 > maxlen {
                                return corrupt("journal logs more blocks than it holds");
                            }
                            dpos = next(dpos);
                            off += tag_bytes;
                            if flags & 2 == 0 {
                                off += 16;
                            }
                            if flags & 8 != 0 {
                                break;
                            }
                        }
                        pos = dpos;
                    }
                    2 => {
                        // commit
                        logged.extend(pending.drain(..));
                        for r in pending_rev.drain(..) {
                            let e = revoked.entry(r).or_insert(seq);
                            if *e < seq {
                                *e = seq;
                            }
                        }
                        txns += 1;
                        seq = seq.wrapping_add(1);
                        pos = next(pos);
                        break;
                    }
                    5 => {
                        let count = rbe32(&b, 12) as usize;
                        let rsz = if is64 { 8 } else { 4 };
                        let mut off = 16;
                        while off + rsz <= count.min(self.bs as usize) {
                            let blk = if is64 {
                                (rbe32(&b, off) as u64) << 32 | rbe32(&b, off + 4) as u64
                            } else {
                                rbe32(&b, off) as u64
                            };
                            pending_rev.push(blk);
                            off += rsz;
                        }
                        pos = next(pos);
                    }
                    _ => break 'outer,
                }
            }
        }
        // Validate everything before the first write: a malformed log must not be half-applied.
        for l in &logged {
            if l.target >= self.blocks_count || l.target == 0 && self.fdb == 1 {
                return corrupt(format!("journal block target {} out of range", l.target));
            }
            jmap(l.jblk)?;
        }
        // Pass 2: replay.
        let mut replayed = 0u32;
        let mut skipped = 0u32;
        for l in &logged {
            if let Some(&rs) = revoked.get(&l.target) {
                if rs >= l.seq {
                    skipped += 1;
                    continue;
                }
            }
            let mut data = self.read_block(jmap(l.jblk)?)?;
            if l.escape {
                wbe32(&mut data, 0, 0xC03B_3998);
            }
            self.dev.write_at(l.target * self.bs, &data)?;
            replayed += 1;
        }
        // Mark journal empty.
        wbe32(&mut jsb, 0x18, seq);
        wbe32(&mut jsb, 0x1C, 0);
        if csum_v2 || csum_v3 {
            // jbd2_superblock_csum: raw crc32c(~0, sb[0..1024]) with s_checksum zeroed, big-endian.
            w32(&mut jsb, 0xFC, 0);
            let raw = crc32c_raw(!0, &jsb[..1024]);
            wbe32(&mut jsb, 0xFC, raw);
        }
        self.dev.write_at(jsb_blk * self.bs, &jsb)?;
        self.dev.flush()?;
        Ok((txns, replayed, skipped))
    }

    /// Recompute group / superblock free counters from the bitmaps.
    fn reconcile_counts(&mut self) -> FsResult<(i64, i64)> {
        self.bb_cache.clear();
        self.ib_cache.clear();
        let mut total_fb = 0u64;
        let mut total_fi = 0u64;
        let mut db = 0i64;
        let mut di = 0i64;
        for g in 0..self.ngroups {
            let n = self.blocks_in_group(g) as usize;
            let fb = {
                let bm = self.block_bitmap(g)?;
                (0..n).filter(|&i| bm[i / 8] & (1 << (i % 8)) == 0).count() as u32
            };
            let ipg = self.ipg as usize;
            let fi = {
                let bm = self.inode_bitmap(g)?;
                (0..ipg).filter(|&i| bm[i / 8] & (1 << (i % 8)) == 0).count() as u32
            };
            db += fb as i64 - self.gd_free_blocks(g) as i64;
            di += fi as i64 - self.gd_free_inodes(g) as i64;
            self.set_gd_u32(g, 0xC, 0x2C, fb);
            self.set_gd_u32(g, 0xE, 0x2E, fi);
            self.write_gd(g)?;
            total_fb += fb as u64;
            total_fi += fi as u64;
        }
        self.set_sb_free_blocks(total_fb);
        w32(&mut self.sb, S_FREE_INODES, total_fi as u32);
        self.write_sb()?;
        Ok((db, di))
    }

    /// Process the classic orphan list (s_last_orphan chain).
    fn process_orphans(&mut self) -> FsResult<u32> {
        let mut n = 0;
        let mut ino = r32(&self.sb, S_LAST_ORPHAN);
        // A cyclic chain must not free the same inode twice (or spin).
        let mut seen = HashSet::new();
        while ino != 0 && seen.insert(ino) {
            if ino < self.first_ino {
                return corrupt(format!("orphan list names reserved inode {ino}"));
            }
            let mut i = self.read_inode(ino)?;
            let next = r32(&i.raw, I_DTIME);
            if i.links() == 0 {
                let is_dir = i.kind() == FileKind::Dir;
                self.release_inode_blocks(&mut i)?;
                w32(&mut i.raw, I_DTIME, Timestamp::now().secs as u32);
                self.write_inode(&mut i)?;
                self.free_inode(ino, is_dir)?;
            } else {
                let sz = i.size();
                w32(&mut i.raw, I_DTIME, 0);
                self.truncate(&mut i, sz)?;
                self.write_inode(&mut i)?;
            }
            n += 1;
            ino = next;
        }
        w32(&mut self.sb, S_LAST_ORPHAN, 0);
        self.write_sb()?;
        Ok(n)
    }

    pub fn forced_ro_reason(&self) -> Option<String> {
        self.ro_reason.clone()
    }
}

// ─────────────── test hooks (seed images for fuzzkit.rs; not a stable API) ───────────────

#[doc(hidden)]
impl Ext4Fs {
    /// Byte offset of an inode in the volume.
    pub fn debug_inode_offset(&self, ino: Ino) -> FsResult<u64> {
        self.inode_loc(self.ino32(ino)?)
    }

    /// Physical blocks of a file, in logical order.
    pub fn debug_file_blocks(&mut self, ino: Ino) -> FsResult<Vec<u64>> {
        let i = self.live_inode(ino)?;
        let (exts, _) = self.get_mapping(&i)?;
        Ok(exts.iter().flat_map(|e| (0..e.len as u64).map(move |k| e.pblk + k)).collect())
    }

    /// Turn a linear directory into a one-level htree (dx_root in block 0, every other block a
    /// leaf), the layout `e2fsck -D` produces. Hashes are synthetic: the driver reads htree
    /// directories linearly and linearizes them before modifying them.
    pub fn debug_make_htree(&mut self, dir: Ino) -> FsResult<()> {
        self.check_rw()?;
        let mut d = self.live_inode(dir)?;
        let bs = self.bs as usize;
        let tail = if self.csum { 12 } else { 0 };
        let blocks = self.dir_blocks(&d)?;
        let Some(&(_, p0)) = blocks.iter().find(|(l, _)| *l == 0) else { return corrupt("directory without blocks") };
        let b0 = self.read_block(p0)?;
        let ents = self.parse_dir_block(&b0);
        // Block 0 may only hold '.' and '..': move the rest to a new leaf.
        let mut leaf = self.empty_dir_block();
        let mut off = 0usize;
        let mut last = None;
        for e in ents.iter().skip(2).filter(|e| e.inode != 0 && !Self::is_tail(e, bs)) {
            let name = b0[e.off + 8..e.off + 8 + e.name_len].to_vec();
            let rl = rec_len_for(e.name_len);
            self.put_dirent(&mut leaf, off, e.inode, rl, &name, e.ftype);
            last = Some(off);
            off += rl;
        }
        if let Some(l) = last {
            w16(&mut leaf, l + 4, (bs - tail - l) as u16);
        }
        let lblk = (d.size() / self.bs) as u32;
        let (mut exts, meta) = self.get_mapping(&d)?;
        let goal = exts.last().map(|e| e.pblk + e.len as u64).unwrap_or_else(|| self.inode_goal(d.ino));
        let pnew = self.alloc_blocks(goal, 1)?[0].0;
        self.write_dir_block(&d, pnew, &mut leaf)?;
        exts.push(Ext { lblk, pblk: pnew, len: 1, uninit: false });
        Self::merge_exts(&mut exts);
        self.set_mapping(&mut d, &exts, meta)?;
        d.set_size(d.size() + self.bs);
        let parent = ents.get(1).map(|e| e.inode).unwrap_or(ROOT_INO);
        let mut root = vec![0u8; bs];
        self.put_dirent(&mut root, 0, d.ino, 12, b".", 2);
        self.put_dirent(&mut root, 12, parent, bs - 12, b"..", 2);
        root[0x1C] = 1; // hash version: half_md4
        root[0x1D] = 8; // info length
        let limit = (bs - 0x20 - if self.csum { 8 } else { 0 }) / 8;
        let leaves: Vec<u32> = (1..=lblk).collect();
        w16(&mut root, 0x20, limit as u16);
        w16(&mut root, 0x22, leaves.len() as u16);
        w32(&mut root, 0x24, leaves[0]);
        for (i, &l) in leaves.iter().enumerate().skip(1) {
            w32(&mut root, 0x20 + 8 * i, (i as u32) << 24);
            w32(&mut root, 0x24 + 8 * i, l);
        }
        self.write_block(p0, &root)?;
        d.set_flags(d.flags() | FL_INDEX);
        self.write_inode(&mut d)
    }

    /// Log `blocks` (target block, new content) as one committed jbd2 transaction that has not
    /// been checkpointed yet, and flag the volume `needs_recovery` (what a crash leaves behind).
    pub fn debug_journal_transaction(&mut self, blocks: &[(u64, Vec<u8>)]) -> FsResult<()> {
        self.check_rw()?;
        let jino = self.journal_inode().ok_or_else(|| FsError::Unsupported("no journal".into()))?;
        let ji = self.read_inode(jino)?;
        let (exts, _) = self.get_mapping(&ji)?;
        let jmap = |l: u64| Self::lookup_block(&exts, l as u32).map(|(_, p, _)| p).ok_or_else(|| FsError::Corrupt("journal hole".into()));
        let bs = self.bs as usize;
        let jsb_blk = jmap(0)?;
        let mut jsb = self.read_block(jsb_blk)?;
        if rbe32(&jsb, 0x28) & (0x2 | 0x8 | 0x10) != 0 || blocks.is_empty() {
            return Err(FsError::Unsupported("only classic 32-bit journal tags".into()));
        }
        let first = rbe32(&jsb, 0x14) as u64;
        let seq = rbe32(&jsb, 0x18);
        let header = |t: u32| {
            let mut b = vec![0u8; bs];
            wbe32(&mut b, 0, 0xC03B_3998);
            wbe32(&mut b, 4, t);
            wbe32(&mut b, 8, seq);
            b
        };
        let mut desc = header(1);
        let mut off = 12;
        for (k, (target, data)) in blocks.iter().enumerate() {
            wbe32(&mut desc, off, *target as u32);
            let flags: u16 = if k > 0 { 2 } else { 0 } | if k + 1 == blocks.len() { 8 } else { 0 };
            desc[off + 6..off + 8].copy_from_slice(&flags.to_be_bytes());
            off += 8;
            if k == 0 {
                desc[off..off + 16].copy_from_slice(&self.sb[S_UUID..S_UUID + 16]);
                off += 16;
            }
            self.dev.write_at(jmap(first + 1 + k as u64)? * self.bs, data)?;
        }
        self.dev.write_at(jmap(first)? * self.bs, &desc)?;
        self.dev.write_at(jmap(first + 1 + blocks.len() as u64)? * self.bs, &header(2))?;
        wbe32(&mut jsb, 0x1C, first as u32);
        self.dev.write_at(jsb_blk * self.bs, &jsb)?;
        self.incompat |= INCOMPAT_RECOVER;
        w32(&mut self.sb, S_INCOMPAT, self.incompat);
        self.write_sb()
    }
}

impl Ext4Fs {
    fn repair_steps(&mut self) -> FsResult<String> {
        let mut report = vec![];
        if self.incompat & INCOMPAT_RECOVER != 0 {
            let (t, b, r) = self.replay_journal()?;
            report.push(format!("Journal replayed: {t} transactions, {b} blocks written, {r} revoked blocks skipped."));
            // The replay may have rewritten the superblock and descriptors: reload.
            let dev = self.dev.clone();
            let fresh = Ext4Fs::open_raw(dev, false)?;
            let keep_reason = self.ro_reason.clone();
            *self = fresh;
            self.ro_reason = keep_reason;
            if let Some(why) = self.bad_gd.clone() {
                return Err(FsError::Corrupt(why));
            }
            self.ro = false;
            self.incompat &= !INCOMPAT_RECOVER;
            w32(&mut self.sb, S_INCOMPAT, self.incompat);
            self.write_sb()?;
        }
        if r32(&self.sb, S_LAST_ORPHAN) != 0 {
            let n = self.process_orphans()?;
            report.push(format!("Processed {n} orphan inode(s)."));
        }
        if self.ro_compat & RO_ORPHAN_PRESENT != 0 {
            report.push("Orphan file entries present: cleared the flag (orphans will be reclaimed by e2fsck).".into());
            self.ro_compat &= !RO_ORPHAN_PRESENT;
            w32(&mut self.sb, S_RO_COMPAT, self.ro_compat);
        }
        let state = r16(&self.sb, S_STATE);
        if state & 2 != 0 {
            report.push("Cleared the EXT4_ERROR_FS flag (a full e2fsck is still recommended).".into());
            w32(&mut self.sb, S_ERROR_COUNT, 0);
        }
        if state & 1 == 0 {
            report.push("Marked the volume as cleanly unmounted.".into());
        }
        w16(&mut self.sb, S_STATE, 1);
        self.write_sb()?;
        let (db, di) = self.reconcile_counts()?;
        if db != 0 || di != 0 {
            report.push(format!("Reconciled free counters (blocks {db:+}, inodes {di:+})."));
        }
        self.dev.flush()?;
        if report.is_empty() {
            report.push("Volume was already clean.".into());
        }
        Ok(report.join(" "))
    }
}

impl FileSystem for Ext4Fs {
    fn volume_info(&self) -> VolumeInfo {
        VolumeInfo { fs_type: self.fs_type, label: self.label(), uuid: self.uuid_string(), read_only: self.ro }
    }

    fn statfs(&mut self) -> FsResult<FsStat> {
        let total = self.blocks_count - self.fdb;
        Ok(FsStat {
            block_size: self.bs as u32,
            total_bytes: total * self.bs,
            free_bytes: self.sb_free_blocks().min(total).saturating_mul(self.bs),
            total_inodes: r32(&self.sb, S_INODES_COUNT) as u64,
            free_inodes: r32(&self.sb, S_FREE_INODES) as u64,
        })
    }

    fn root_ino(&self) -> Ino {
        ROOT_INO as Ino
    }

    fn lookup(&mut self, parent: Ino, name: &str) -> FsResult<Ino> {
        let p = self.live_inode(parent)?;
        if p.kind() != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        match self.find_entry(&p, name.as_bytes())? {
            Some((ino, _)) => Ok(ino as Ino),
            None => Err(FsError::NotFound),
        }
    }

    fn getattr(&mut self, ino: Ino) -> FsResult<Attr> {
        let i = self.live_inode(ino)?;
        Ok(self.attr_of(&i))
    }

    fn setattr(&mut self, ino: Ino, set: &SetAttr) -> FsResult<Attr> {
        self.check_rw()?;
        let mut i = self.live_inode(ino)?;
        if let Some(p) = set.perm {
            let m = (i.mode() & S_IFMT) | (p & 0o7777);
            w16(&mut i.raw, I_MODE, m);
        }
        if let Some(u) = set.uid {
            i.set_uid(u);
        }
        if let Some(g) = set.gid {
            i.set_gid(g);
        }
        if let Some(sz) = set.size {
            if i.kind() == FileKind::Dir {
                return Err(FsError::IsDir);
            }
            if sz != i.size() {
                self.truncate(&mut i, sz)?;
                Self::touch_mc(&mut i);
            }
        }
        if let Some(t) = set.atime {
            i.set_time(I_ATIME, I_ATIME_EXTRA, t);
        }
        if let Some(t) = set.mtime {
            i.set_time(I_MTIME, I_MTIME_EXTRA, t);
        }
        i.set_time(I_CTIME, I_CTIME_EXTRA, Timestamp::now());
        self.write_inode(&mut i)?;
        Ok(self.attr_of(&i))
    }

    fn read(&mut self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>> {
        let i = self.live_inode(ino)?;
        match i.kind() {
            FileKind::Dir => return Err(FsError::IsDir),
            FileKind::Symlink if self.is_fast_symlink(&i) => {
                let s = i.size() as usize;
                let o = (offset as usize).min(s);
                let e = (o + len as usize).min(s);
                return Ok(i.i_block()[o..e].to_vec());
            }
            _ => {}
        }
        let (exts, _) = self.get_mapping(&i)?;
        self.read_data(&i, &exts, offset, len as usize)
    }

    fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<u32> {
        self.check_rw()?;
        let mut i = self.live_inode(ino)?;
        if i.kind() == FileKind::Dir {
            return Err(FsError::IsDir);
        }
        self.write_data(&mut i, offset, data)?;
        Self::touch_mc(&mut i);
        self.write_inode(&mut i)?;
        Ok(data.len() as u32)
    }

    fn readdir(&mut self, dir: Ino) -> FsResult<Vec<DirEntry>> {
        let d = self.live_inode(dir)?;
        if d.kind() != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        let mut out = vec![];
        for (name, ino, ft) in self.dir_entries(&d)? {
            if name == b"." || name == b".." || !self.valid_dirent_ino(ino) {
                continue;
            }
            let kind = match ft {
                2 => FileKind::Dir,
                7 => FileKind::Symlink,
                1 => FileKind::File,
                _ => self.read_inode(ino).map(|i| i.kind()).unwrap_or(FileKind::File),
            };
            out.push(DirEntry { name: String::from_utf8_lossy(&name).into_owned(), ino: ino as Ino, kind });
        }
        Ok(out)
    }

    fn create(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino> {
        let (mut p, mut n) = self.make_node(parent, name, S_IFREG | (perm & 0o7777), uid, gid)?;
        n.set_links(1);
        self.write_inode(&mut n)?;
        if let Err(e) = self.add_entry(&mut p, name.as_bytes(), n.ino, FileKind::File) {
            n.set_links(0);
            self.write_inode(&mut n)?;
            self.free_inode(n.ino, false)?;
            return Err(e);
        }
        let mut p = self.read_inode(p.ino)?;
        Self::touch_mc(&mut p);
        self.write_inode(&mut p)?;
        Ok(n.ino as Ino)
    }

    fn mkdir(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino> {
        let (mut p, mut n) = self.make_node(parent, name, S_IFDIR | (perm & 0o7777), uid, gid)?;
        let ino = n.ino;
        // first block with '.' and '..'
        let goal = self.inode_goal(ino);
        let r = match self.alloc_blocks(goal, 1) {
            Ok(r) => r,
            Err(e) => {
                self.free_inode(ino, true)?;
                return Err(e);
            }
        };
        let pblk = r[0].0;
        let bs = self.bs as usize;
        let tail = if self.csum { 12 } else { 0 };
        let mut b = vec![0u8; bs];
        self.put_dirent(&mut b, 0, ino, 12, b".", 2);
        self.put_dirent(&mut b, 12, p.ino, bs - 12 - tail, b"..", 2);
        if tail > 0 {
            Self::put_tail(&mut b);
        }
        n.set_links(2);
        n.set_size(self.bs);
        self.set_mapping(&mut n, &[Ext { lblk: 0, pblk, len: 1, uninit: false }], vec![])?;
        self.write_dir_block(&n, pblk, &mut b)?;
        self.write_inode(&mut n)?;
        if let Err(e) = self.add_entry(&mut p, name.as_bytes(), ino, FileKind::Dir) {
            self.free_blocks(&[(pblk, 1)])?;
            n.set_links(0);
            self.write_inode(&mut n)?;
            self.free_inode(ino, true)?;
            return Err(e);
        }
        let mut p = self.read_inode(p.ino)?;
        self.inc_links(&mut p);
        Self::touch_mc(&mut p);
        self.write_inode(&mut p)?;
        Ok(ino as Ino)
    }

    fn symlink(&mut self, parent: Ino, name: &str, target: &str, uid: u32, gid: u32) -> FsResult<Ino> {
        if target.is_empty() || target.len() >= self.bs as usize {
            return Err(FsError::NameTooLong);
        }
        let (mut p, mut n) = self.make_node(parent, name, S_IFLNK | 0o777, uid, gid)?;
        n.set_links(1);
        let t = target.as_bytes();
        if t.len() < 60 {
            n.set_flags(n.flags() & !FL_EXTENTS);
            n.i_block_mut().fill(0);
            n.i_block_mut()[..t.len()].copy_from_slice(t);
            n.set_size(t.len() as u64);
        } else {
            if self.incompat & INCOMPAT_EXTENTS != 0 {
                n.set_flags(n.flags() | FL_EXTENTS);
                Self::init_extent_root(n.i_block_mut());
            }
            self.write_data(&mut n, 0, t)?;
        }
        self.write_inode(&mut n)?;
        if let Err(e) = self.add_entry(&mut p, name.as_bytes(), n.ino, FileKind::Symlink) {
            self.release_inode_blocks(&mut n)?;
            n.set_links(0);
            self.write_inode(&mut n)?;
            self.free_inode(n.ino, false)?;
            return Err(e);
        }
        let mut p = self.read_inode(p.ino)?;
        Self::touch_mc(&mut p);
        self.write_inode(&mut p)?;
        Ok(n.ino as Ino)
    }

    fn readlink(&mut self, ino: Ino) -> FsResult<String> {
        let i = self.live_inode(ino)?;
        if i.kind() != FileKind::Symlink {
            return Err(FsError::Invalid("not a symlink".into()));
        }
        // A target is shorter than a block (and than PATH_MAX): anything else is corruption,
        // not a reason to allocate i_size bytes.
        if i.size() >= self.bs.max(4096) {
            return corrupt(format!("symlink inode {ino} has an impossible size {}", i.size()));
        }
        let data = if self.is_fast_symlink(&i) {
            i.i_block()[..(i.size() as usize).min(60)].to_vec()
        } else {
            let (exts, _) = self.get_mapping(&i)?;
            self.read_data(&i, &exts, 0, i.size() as usize)?
        };
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    fn unlink(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.check_rw()?;
        let mut p = self.live_inode(parent)?;
        if p.kind() != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        let (ino, _) = self.find_entry(&p, name.as_bytes())?.ok_or(FsError::NotFound)?;
        let t = self.read_inode(ino)?;
        if t.kind() == FileKind::Dir {
            return Err(FsError::IsDir);
        }
        self.remove_entry(&mut p, name.as_bytes())?;
        let mut p = self.read_inode(p.ino)?;
        Self::touch_mc(&mut p);
        self.write_inode(&mut p)?;
        self.finish_unlink(t)
    }

    fn rmdir(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.check_rw()?;
        let p = self.live_inode(parent)?;
        if p.kind() != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        let (ino, _) = self.find_entry(&p, name.as_bytes())?.ok_or(FsError::NotFound)?;
        let d = self.read_inode(ino)?;
        if d.kind() != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        if !self.dir_is_empty(&d)? {
            return Err(FsError::NotEmpty);
        }
        let mut p = p;
        self.remove_entry(&mut p, name.as_bytes())?;
        let mut p = self.read_inode(p.ino)?;
        self.remove_dir_inode(&mut p, d)?;
        Self::touch_mc(&mut p);
        self.write_inode(&mut p)
    }

    fn rename(&mut self, from_parent: Ino, from_name: &str, to_parent: Ino, to_name: &str) -> FsResult<()> {
        self.check_rw()?;
        check_name(to_name, 255)?;
        let sp = self.live_inode(from_parent)?;
        let dp = self.live_inode(to_parent)?;
        if sp.kind() != FileKind::Dir || dp.kind() != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        let (sino, _) = self.find_entry(&sp, from_name.as_bytes())?.ok_or(FsError::NotFound)?;
        let src = self.read_inode(sino)?;
        let is_dir = src.kind() == FileKind::Dir;
        if is_dir && self.is_ancestor(sino, dp.ino)? {
            return Err(FsError::Invalid("cannot move a directory inside itself".into()));
        }
        if let Some((dino, _)) = self.find_entry(&dp, to_name.as_bytes())? {
            if dino == sino {
                return Ok(());
            }
            let dst = self.read_inode(dino)?;
            match (is_dir, dst.kind() == FileKind::Dir) {
                (true, false) => return Err(FsError::NotDir),
                (false, true) => return Err(FsError::IsDir),
                (true, true) => {
                    if !self.dir_is_empty(&dst)? {
                        return Err(FsError::NotEmpty);
                    }
                    self.rmdir(to_parent, to_name)?;
                }
                (false, false) => self.unlink(to_parent, to_name)?,
            }
        }
        let mut dp = self.read_inode(dp.ino)?;
        self.add_entry(&mut dp, to_name.as_bytes(), sino, src.kind())?;
        let mut sp = self.read_inode(from_parent as u32)?;
        self.remove_entry(&mut sp, from_name.as_bytes())?;
        let mut src = self.read_inode(sino)?;
        if is_dir && from_parent != to_parent {
            self.set_dotdot(&src, to_parent as u32)?;
            let mut sp = self.read_inode(from_parent as u32)?;
            self.dec_links(&mut sp);
            Self::touch_mc(&mut sp);
            self.write_inode(&mut sp)?;
            let mut dp = self.read_inode(to_parent as u32)?;
            self.inc_links(&mut dp);
            Self::touch_mc(&mut dp);
            self.write_inode(&mut dp)?;
        } else {
            let mut sp = self.read_inode(from_parent as u32)?;
            Self::touch_mc(&mut sp);
            self.write_inode(&mut sp)?;
            if from_parent != to_parent {
                let mut dp = self.read_inode(to_parent as u32)?;
                Self::touch_mc(&mut dp);
                self.write_inode(&mut dp)?;
            }
        }
        src.set_time(I_CTIME, I_CTIME_EXTRA, Timestamp::now());
        self.write_inode(&mut src)
    }

    fn sync(&mut self) -> FsResult<()> {
        if !self.ro && self.modified {
            w32(&mut self.sb, S_WTIME, Timestamp::now().secs as u32);
            self.write_sb()?;
            self.modified = false;
        }
        if !self.ro {
            self.dev.flush()?;
        }
        Ok(())
    }

    fn read_only_reason(&self) -> Option<String> {
        self.ro_reason.clone()
    }

    fn dirty_state(&mut self) -> DirtyState {
        let state = r16(&self.sb, S_STATE);
        let mut reasons = vec![];
        if self.incompat & INCOMPAT_RECOVER != 0 {
            reasons.push("il journal ext4 contiene transazioni non applicate (needs_recovery)");
        }
        if state & 2 != 0 {
            reasons.push("il superblocco segnala errori del filesystem (EXT4_ERROR_FS)");
        }
        if state & 1 == 0 {
            reasons.push("il volume non è stato smontato correttamente");
        }
        if self.ro_compat & RO_ORPHAN_PRESENT != 0 || r32(&self.sb, S_LAST_ORPHAN) != 0 {
            reasons.push("ci sono inode orfani da completare");
        }
        if reasons.is_empty() {
            DirtyState::Clean
        } else {
            DirtyState::Dirty(reasons.join("; "))
        }
    }

    fn repair(&mut self) -> FsResult<String> {
        if self.dev.read_only() {
            return Err(FsError::ReadOnly);
        }
        if let Some(why) = &self.bad_gd {
            return Err(FsError::Corrupt(format!("{why}: the Doctor can't fix this, run e2fsck")));
        }
        let saved_ro = self.ro;
        self.ro = false;
        let r = self.repair_steps();
        // Also on failure: never leave a read-only volume writable.
        self.ro = saved_ro || self.ro_reason.is_some() || self.bad_gd.is_some();
        r
    }
}

// ───────────────────────────── mkfs ─────────────────────────────

/// Create a fresh ext2/ext3/ext4 filesystem on `dev`.
pub fn format(dev: Arc<dyn BlockDev>, fs: FsType, label: &str) -> FsResult<()> {
    let size = dev.size();
    if size < 8 * 1024 * 1024 {
        return Err(FsError::Invalid("ext filesystems need at least 8 MiB".into()));
    }
    let bs: u64 = if size >= 256 * 1024 * 1024 { 4096 } else { 1024 };
    let log = (bs / 1024).trailing_zeros();
    let fdb: u64 = if bs == 1024 { 1 } else { 0 };
    let mut blocks = size / bs;
    let bpg = bs * 8;
    let inode_size: u64 = 256;
    let ext4 = fs == FsType::Ext4;
    let journal = fs != FsType::Ext2;
    let csum = ext4;
    let desc_size: u64 = 32;
    // inode table sizing: one inode per 16 KiB
    let mut ngroups = (blocks - fdb).div_ceil(bpg);
    let inodes_target = (size / 16384).max(64);
    let per_block_inodes = bs / inode_size;
    let mut ipg = inodes_target.div_ceil(ngroups);
    ipg = ipg.div_ceil(per_block_inodes) * per_block_inodes;
    ipg = ipg.div_ceil(8) * 8;
    ipg = ipg.min(bs * 8).max(per_block_inodes.max(16));
    let itb = ipg * inode_size / bs;
    let gdt_blocks = |ng: u64| (ng * desc_size).div_ceil(bs);
    let has_super = |g: u64| -> bool {
        if g <= 1 {
            return true;
        }
        for base in [3u64, 5, 7] {
            let mut p = base;
            while p < g {
                p *= base;
            }
            if p == g {
                return true;
            }
        }
        false
    };
    // Drop a too-small last group.
    loop {
        let last = ngroups - 1;
        let last_size = blocks - fdb - last * bpg;
        let overhead = if has_super(last) { 1 + gdt_blocks(ngroups) } else { 0 } + 2 + itb;
        if last_size < overhead + 64 && ngroups > 1 {
            blocks = fdb + last * bpg;
            ngroups -= 1;
        } else {
            break;
        }
    }
    let gdtb = gdt_blocks(ngroups);
    let inodes_count = ipg * ngroups;

    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&super::random_id());
    let mut hash_seed = [0u8; 16];
    hash_seed.copy_from_slice(&super::random_id());
    let now = Timestamp::now().secs as u32;

    // Layout per group.
    struct G {
        bb: u64,
        ib: u64,
        it: u64,
        used: u64,
    }
    let mut groups = vec![];
    for g in 0..ngroups {
        let start = fdb + g * bpg;
        let mut p = start;
        if has_super(g) {
            p += 1 + gdtb;
        }
        let bb = p;
        let ib = p + 1;
        let it = p + 2;
        groups.push(G { bb, ib, it, used: it + itb - start });
    }

    // Superblock.
    let mut sb = vec![0u8; 1024];
    w32(&mut sb, S_INODES_COUNT, inodes_count as u32);
    w32(&mut sb, S_BLOCKS_LO, blocks as u32);
    w32(&mut sb, 0x08, (blocks / 20) as u32); // reserved 5%
    w32(&mut sb, S_FIRST_DATA_BLOCK, fdb as u32);
    w32(&mut sb, S_LOG_BLOCK, log);
    w32(&mut sb, S_LOG_CLUSTER, log);
    w32(&mut sb, S_BLOCKS_PER_GROUP, bpg as u32);
    w32(&mut sb, 0x24, bpg as u32);
    w32(&mut sb, S_INODES_PER_GROUP, ipg as u32);
    w32(&mut sb, S_WTIME, now);
    w16(&mut sb, 0x36, 0xFFFF); // max mount count = -1
    w16(&mut sb, S_MAGIC, EXT4_MAGIC);
    w16(&mut sb, S_STATE, 1);
    w16(&mut sb, 0x3C, 1); // errors=continue
    w32(&mut sb, 0x40, now); // lastcheck
    w32(&mut sb, S_REV, 1);
    w32(&mut sb, S_FIRST_INO, 11);
    w16(&mut sb, S_INODE_SIZE, inode_size as u16);
    let mut compat = COMPAT_EXT_ATTR | COMPAT_DIR_INDEX;
    if journal {
        compat |= COMPAT_HAS_JOURNAL;
    }
    let mut incompat = INCOMPAT_FILETYPE;
    if ext4 {
        incompat |= INCOMPAT_EXTENTS;
    }
    let mut ro_compat = RO_SPARSE_SUPER | RO_LARGE_FILE;
    if ext4 {
        ro_compat |= RO_HUGE_FILE | RO_DIR_NLINK | RO_EXTRA_ISIZE | RO_METADATA_CSUM;
    }
    w32(&mut sb, S_COMPAT, compat);
    w32(&mut sb, S_INCOMPAT, incompat);
    w32(&mut sb, S_RO_COMPAT, ro_compat);
    sb[S_UUID..S_UUID + 16].copy_from_slice(&uuid);
    let lb = label.as_bytes();
    let ln = lb.len().min(16);
    sb[S_VOLNAME..S_VOLNAME + ln].copy_from_slice(&lb[..ln]);
    for k in 0..4 {
        w32(&mut sb, 0xEC + k * 4, r32(&hash_seed, k * 4));
    }
    sb[0xFC] = 1; // half_md4
    if journal {
        w32(&mut sb, S_JOURNAL_INUM, JOURNAL_INO);
    }
    w32(&mut sb, 0x108, now); // mkfs_time
    if ext4 {
        w16(&mut sb, 0x15C, 32);
        w16(&mut sb, 0x15E, 32);
        w32(&mut sb, 0x160, 0x2); // s_flags: unsigned hash
        sb[0x175] = 1; // checksum type crc32c
    } else {
        w32(&mut sb, 0x160, 0x2);
    }
    w32(&mut sb, 0x100, 0x0C); // default mount opts: user_xattr, acl

    // Write zeroed metadata areas.
    let zero_blk = vec![0u8; bs as usize];
    // zero first 2 blocks / 4KiB region (boot area + superblock)
    dev.write_at(0, &vec![0u8; (fdb * bs + 2048).max(4096) as usize])?;
    for (g, gd) in groups.iter().enumerate() {
        let _ = g;
        dev.write_at(gd.bb * bs, &zero_blk)?;
        dev.write_at(gd.ib * bs, &zero_blk)?;
        let chunk = vec![0u8; (bs * itb.min(256)) as usize];
        let mut k = 0;
        while k < itb {
            let n = (itb - k).min(256);
            dev.write_at((gd.it + k) * bs, &chunk[..(n * bs) as usize])?;
            k += n;
        }
    }

    // Bitmaps & descriptors (free counts computed here; root/l+f/journal allocated later
    // through the regular driver allocator which keeps counters right).
    let mut gdt = vec![0u8; (gdtb * bs) as usize];
    let mut free_total = 0u64;
    for (g, gd) in groups.iter().enumerate() {
        let g = g as u64;
        let start = fdb + g * bpg;
        let n = (blocks - start).min(bpg);
        let mut bm = vec![0u8; bs as usize];
        for i in 0..gd.used {
            bm[(i / 8) as usize] |= 1 << (i % 8);
        }
        for i in n..bs * 8 {
            bm[(i / 8) as usize] |= 1 << (i % 8);
        }
        dev.write_at(gd.bb * bs, &bm)?;
        let mut ibm = vec![0u8; bs as usize];
        if g == 0 {
            for i in 0..10 {
                ibm[i / 8] |= 1 << (i % 8);
            }
        }
        for i in ipg..bs * 8 {
            ibm[(i / 8) as usize] |= 1 << (i % 8);
        }
        dev.write_at(gd.ib * bs, &ibm)?;
        let free_b = n - gd.used;
        free_total += free_b;
        let d = &mut gdt[(g * desc_size) as usize..((g + 1) * desc_size) as usize];
        w32(d, 0x0, gd.bb as u32);
        w32(d, 0x4, gd.ib as u32);
        w32(d, 0x8, gd.it as u32);
        w16(d, 0xC, free_b as u16);
        w16(d, 0xE, (if g == 0 { ipg - 10 } else { ipg }) as u16);
        w16(d, 0x10, 0);
        w16(d, 0x12, if csum { BG_INODE_ZEROED } else { 0 });
        if csum {
            let seed = crc32c_raw(!0, &uuid);
            let c = crc32c_raw(seed, &bm[..(bpg / 8) as usize]);
            w16(d, 0x18, c as u16);
            let c = crc32c_raw(seed, &ibm[..(ipg / 8) as usize]);
            w16(d, 0x1A, c as u16);
            w16(d, 0x1C, (if g == 0 { ipg - 10 } else { ipg }) as u16);
        }
    }
    w32(&mut sb, S_FREE_BLOCKS_LO, free_total as u32);
    w32(&mut sb, S_FREE_INODES, (inodes_count - 10) as u32);
    // write primary + backup superblocks and GDTs
    for (g, _) in groups.iter().enumerate() {
        let g = g as u64;
        if !has_super(g) {
            continue;
        }
        let start = fdb + g * bpg;
        let mut s = sb.clone();
        w16(&mut s, 0x5A, g as u16); // s_block_group_nr
        if csum {
            let c = crc32c_raw(!0, &s[..S_CHECKSUM]);
            w32(&mut s, S_CHECKSUM, c);
        }
        if g == 0 {
            dev.write_at(1024, &s)?;
        } else {
            dev.write_at(start * bs, &s)?;
        }
        // descriptors with checksums
        let mut gd_copy = gdt.clone();
        if csum {
            let seed = crc32c_raw(!0, &uuid);
            for k in 0..ngroups {
                let d = &mut gd_copy[(k * desc_size) as usize..((k + 1) * desc_size) as usize];
                let mut c = crc32c_raw(seed, &(k as u32).to_le_bytes());
                c = crc32c_raw(c, &d[..0x1E]);
                c = crc32c_raw(c, &[0, 0]);
                w16(d, 0x1E, c as u16);
            }
        }
        dev.write_at((start + 1) * bs, &gd_copy)?;
    }
    dev.flush()?;

    // Now use the driver to build the root directory, lost+found and the journal.
    let mut f = Ext4Fs::open_raw(dev.clone(), false)?;
    // root
    let mut root = f.new_inode(ROOT_INO, S_IFDIR | 0o755, 0, 0);
    let r = f.alloc_blocks(f.inode_goal(ROOT_INO), 1)?;
    let rb = r[0].0;
    let bsz = bs as usize;
    let tail = if csum { 12 } else { 0 };
    let mut b = vec![0u8; bsz];
    f.put_dirent(&mut b, 0, ROOT_INO, 12, b".", 2);
    f.put_dirent(&mut b, 12, ROOT_INO, bsz - 12 - tail, b"..", 2);
    if tail > 0 {
        Ext4Fs::put_tail(&mut b);
    }
    root.set_links(2);
    root.set_size(bs);
    f.set_mapping(&mut root, &[Ext { lblk: 0, pblk: rb, len: 1, uninit: false }], vec![])?;
    f.write_dir_block(&root, rb, &mut b)?;
    f.write_inode(&mut root)?;
    {
        let g = 0;
        let d = f.gd_used_dirs(g) + 1;
        f.set_gd_u32(g, 0x10, 0x30, d);
        f.write_gd(g)?;
    }
    // lost+found (inode 11) with 16 KiB of directory blocks
    let lf = f.mkdir(ROOT_INO as Ino, "lost+found", 0o700, 0, 0)? as u32;
    {
        let mut li = f.read_inode(lf)?;
        let want = (16384 / bs).max(1);
        let (mut exts, meta) = f.get_mapping(&li)?;
        for k in 1..want {
            let goal = exts.last().map(|e| e.pblk + e.len as u64).unwrap_or(0);
            let r = f.alloc_blocks(goal, 1)?;
            let mut eb = f.empty_dir_block();
            f.write_dir_block(&li, r[0].0, &mut eb)?;
            exts.push(Ext { lblk: k as u32, pblk: r[0].0, len: 1, uninit: false });
        }
        Ext4Fs::merge_exts(&mut exts);
        f.set_mapping(&mut li, &exts, meta)?;
        li.set_size(want * bs);
        f.write_inode(&mut li)?;
    }
    // journal
    if journal {
        let fsblocks = blocks;
        let jblocks: u64 = if fsblocks < 32768 {
            1024
        } else if fsblocks < 256 * 1024 {
            4096
        } else if fsblocks < 512 * 1024 {
            8192
        } else if fsblocks < 4096 * 1024 {
            16384
        } else {
            32768
        };
        let mut ji = f.new_inode(JOURNAL_INO, S_IFREG | 0o600, 0, 0);
        ji.set_links(1);
        let mid_group = (f.ngroups / 2) as u64;
        let goal = f.group_first_block(mid_group as u32);
        let runs = f.alloc_blocks(goal, jblocks)?;
        let mut exts = vec![];
        let mut l = 0u32;
        for (p, n) in &runs {
            exts.push(Ext { lblk: l, pblk: *p, len: *n as u32, uninit: false });
            let z = vec![0u8; (bs * 256) as usize];
            let mut k = 0;
            while k < *n {
                let c = (*n - k).min(256);
                dev.write_at((p + k) * bs, &z[..(c * bs) as usize])?;
                k += c;
            }
            l += *n as u32;
        }
        f.set_mapping(&mut ji, &exts, vec![])?;
        ji.set_size(jblocks * bs);
        f.write_inode(&mut ji)?;
        // jbd2 superblock v2
        let mut js = vec![0u8; bs as usize];
        wbe32(&mut js, 0, 0xC03B_3998);
        wbe32(&mut js, 4, 4);
        wbe32(&mut js, 8, 0);
        wbe32(&mut js, 0x0C, bs as u32);
        wbe32(&mut js, 0x10, jblocks as u32);
        wbe32(&mut js, 0x14, 1);
        wbe32(&mut js, 0x18, 1);
        wbe32(&mut js, 0x1C, 0);
        js[0x30..0x40].copy_from_slice(&uuid);
        wbe32(&mut js, 0x40, 1); // nr_users
        js[0x100..0x110].copy_from_slice(&uuid);
        dev.write_at(runs[0].0 * bs, &js)?;
        // superblock journal backup (s_jnl_blocks)
        let ji = f.read_inode(JOURNAL_INO)?;
        for k in 0..15 {
            w32(&mut f.sb, 0x10C + k * 4, r32(ji.i_block(), k * 4));
        }
        w32(&mut f.sb, 0x10C + 15 * 4, r32(&ji.raw, I_SIZE_HI));
        w32(&mut f.sb, 0x10C + 16 * 4, r32(&ji.raw, I_SIZE_LO));
        f.sb[0xFD] = 1; // EXT3_JNL_BACKUP_BLOCKS
        f.write_sb()?;
    }
    f.sync()?;
    // Refresh the backup superblock in group 1 so free counts look consistent.
    Ok(())
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemDev;
    use crate::vfs::testutil;

    fn fresh(fs: FsType, mb: usize) -> Arc<dyn BlockDev> {
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(mb * 1024 * 1024));
        format(dev.clone(), fs, "LUMENTEST").unwrap();
        dev
    }

    #[test]
    fn crc32c_known_vector() {
        // crc32c("123456789") with standard init/final xor = 0xE3069283
        assert_eq!(!crc32c_raw(!0, b"123456789"), 0xE306_9283);
    }

    #[test]
    fn ext4_conformance() {
        let dev = fresh(FsType::Ext4, 64);
        let mut fs = Ext4Fs::open(dev.clone(), false).unwrap();
        assert_eq!(fs.volume_info().fs_type, FsType::Ext4);
        assert_eq!(fs.volume_info().label, "LUMENTEST");
        assert_eq!(fs.dirty_state(), DirtyState::Clean);
        testutil::conformance(&mut fs, true);
        drop(fs);
        let mut fs = Ext4Fs::open(dev, false).unwrap();
        testutil::check_persisted(&mut fs);
    }

    #[test]
    fn ext4_4k_blocks_and_deep_extent_tree() {
        let dev = fresh(FsType::Ext4, 300);
        let mut fs = Ext4Fs::open(dev.clone(), false).unwrap();
        assert_eq!(fs.bs, 4096);
        let root = fs.root_ino();
        // Fragment a file heavily: write every other block, forcing > 4 extents and a depth-1 tree.
        let f = fs.create(root, "frag.bin", 0o644, 1000, 1000).unwrap();
        let g = fs.create(root, "filler.bin", 0o644, 1000, 1000).unwrap();
        for k in 0..2000u64 {
            fs.write(f, k * 8192, &[k as u8; 4096]).unwrap();
            fs.write(g, k * 4096, &[0xAA; 4096]).unwrap();
        }
        let i = fs.read_inode(f as u32).unwrap();
        assert!(r16(i.i_block(), 6) >= 1, "expected a deeper extent tree");
        for k in (0..2000u64).step_by(97) {
            assert_eq!(fs.read(f, k * 8192, 4096).unwrap(), vec![k as u8; 4096]);
            assert_eq!(fs.read(f, k * 8192 + 4096, 16).unwrap(), vec![0u8; 16]);
        }
        fs.setattr(f, &SetAttr { size: Some(4096 * 3), ..Default::default() }).unwrap();
        fs.unlink(root, "frag.bin").unwrap();
        fs.unlink(root, "filler.bin").unwrap();
        testutil::conformance(&mut fs, true);
        drop(fs);
        let mut fs = Ext4Fs::open(dev, false).unwrap();
        testutil::check_persisted(&mut fs);
    }

    #[test]
    fn ext2_and_ext3_blockmaps() {
        for t in [FsType::Ext2, FsType::Ext3] {
            let dev = fresh(t, 40);
            let mut fs = Ext4Fs::open(dev.clone(), false).unwrap();
            assert_eq!(fs.volume_info().fs_type, t);
            testutil::conformance(&mut fs, true);
            // big file through double-indirect blocks (1 KiB blocks: > 12 + 256 blocks)
            let root = fs.root_ino();
            let f = fs.create(root, "ind.bin", 0o644, 0, 0).unwrap();
            let data = testutil::pattern(3 * 1024 * 1024, 11);
            fs.write(f, 0, &data).unwrap();
            assert!(fs.read(f, 0, data.len() as u32).unwrap() == data);
            fs.setattr(f, &SetAttr { size: Some(100_000), ..Default::default() }).unwrap();
            assert!(fs.read(f, 0, 200_000).unwrap() == data[..100_000]);
            fs.unlink(root, "ind.bin").unwrap();
            drop(fs);
            let mut fs = Ext4Fs::open(dev, false).unwrap();
            testutil::check_persisted(&mut fs);
        }
    }

    #[test]
    fn long_symlink_and_counters() {
        let dev = fresh(FsType::Ext4, 32);
        let mut fs = Ext4Fs::open(dev, false).unwrap();
        let before = fs.statfs().unwrap();
        let root = fs.root_ino();
        let target = "x/".repeat(100);
        let s = fs.symlink(root, "long", &target, 0, 0).unwrap();
        assert_eq!(fs.readlink(s).unwrap(), target);
        fs.unlink(root, "long").unwrap();
        let after = fs.statfs().unwrap();
        assert_eq!(before.free_bytes, after.free_bytes);
        assert_eq!(before.free_inodes, after.free_inodes);
        let (db, di) = fs.reconcile_counts().unwrap();
        assert_eq!((db, di), (0, 0), "counters drifted");
    }

    #[test]
    fn doctor_clears_error_state() {
        let dev = fresh(FsType::Ext4, 32);
        {
            let mut fs = Ext4Fs::open(dev.clone(), false).unwrap();
            w16(&mut fs.sb, S_STATE, 2);
            fs.write_sb().unwrap();
        }
        let mut fs = Ext4Fs::open(dev.clone(), false).unwrap();
        assert!(matches!(fs.dirty_state(), DirtyState::Dirty(_)));
        let rep = fs.repair().unwrap();
        assert!(rep.contains("ERROR_FS"), "{rep}");
        assert_eq!(fs.dirty_state(), DirtyState::Clean);
    }
}
