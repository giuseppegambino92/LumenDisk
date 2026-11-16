//! mkfs.btrfs: a fresh, empty, single-device Btrfs filesystem, equivalent to what
//! `mkfs.btrfs` (btrfs-progs 6.x) creates with its defaults.
//!
//! On-disk format references: the kernel's `include/uapi/linux/btrfs_tree.h` and
//! `fs/btrfs/accessors.h`, btrfs-progs `kernel-shared/ctree.h`, `mkfs/common.c`
//! (`make_btrfs`) and `mkfs/main.c`, and <https://btrfs.readthedocs.io/en/latest/dev/On-disk-format.html>.
//! Every multi-byte integer on disk is little-endian; checksums are crc32c.
//!
//! What gets written (one transaction, generation [`GEN`]):
//! - three chunks: DATA (single, 8 MiB), SYSTEM (DUP, 2 x 8 MiB) and METADATA (DUP,
//!   2 x 32..256 MiB), allocated from physical 1 MiB on (the first MiB is reserved), the same
//!   profiles and sizes `mkfs.btrfs` picks for one device;
//! - the chunk tree (SYSTEM chunk) and the root, extent, device, fs, checksum, uuid,
//!   free-space and data-relocation trees (METADATA chunk), every tree block in both DUP
//!   copies;
//! - the primary superblock at 64 KiB and the mirrors at 64 MiB / 256 GiB that fit.
//!
//! Features: mixed-backref, big-metadata (node > 4 KiB), extended-iref, skinny-metadata,
//! no-holes, free-space-tree (+ valid): the default set of `mkfs.btrfs` 6.x.
//! The block-group tree is not enabled (it is not a `mkfs.btrfs` default either).

use std::sync::Arc;

use crate::block::BlockDev;
use crate::forge::MkfsOptions;
use crate::i18n::t;
use crate::vfs::ext4_fs::crc32c_raw;
use crate::vfs::{FsError, FsResult};

#[cfg(test)]
mod tests;

/// Smallest device `mkfs.btrfs` 6.x accepts with the default profiles (its
/// `btrfs_min_dev_size()`: 114294784 bytes = 109 MiB, independent of the node size).
pub const MIN_DEV_SIZE: u64 = 114_294_784;
/// Data block size. 4 KiB is the only value every kernel (any page size) mounts.
pub const SECTORSIZE: u32 = 4096;
pub const DEFAULT_NODESIZE: u32 = 16384;
pub const MAX_NODESIZE: u32 = 65536;
/// The single generation (transaction id) everything is written with.
pub const GEN: u64 = 1;

const MIB: u64 = 1 << 20;
/// The first MiB of the device is never allocated (boot loaders, the primary superblock).
const RESERVED_START: u64 = MIB;
const STRIPE_LEN: u64 = 65536;
const DATA_CHUNK: u64 = 8 * MIB;
const SYSTEM_STRIPE: u64 = 8 * MIB;
const META_STRIPE_MIN: u64 = 32 * MIB;
const META_STRIPE_MAX: u64 = 256 * MIB;

/// Superblock copies (`btrfs_sb_offset()`): 64 KiB, 64 MiB, 256 GiB.
pub const SUPER_OFFSETS: [u64; 3] = [64 * 1024, 64 * MIB, 256 * 1024 * MIB];
pub const SUPER_SIZE: usize = 4096;
/// "_BHRfS_M" read as a little-endian u64.
pub const MAGIC: u64 = 0x4D5F_5366_5248_425F;

// ── object ids (btrfs_tree.h) ──
pub const ROOT_TREE_OBJECTID: u64 = 1;
pub const EXTENT_TREE_OBJECTID: u64 = 2;
pub const CHUNK_TREE_OBJECTID: u64 = 3;
pub const DEV_TREE_OBJECTID: u64 = 4;
pub const FS_TREE_OBJECTID: u64 = 5;
pub const ROOT_TREE_DIR_OBJECTID: u64 = 6;
pub const CSUM_TREE_OBJECTID: u64 = 7;
pub const UUID_TREE_OBJECTID: u64 = 9;
pub const FREE_SPACE_TREE_OBJECTID: u64 = 10;
pub const DEV_STATS_OBJECTID: u64 = 0;
pub const DATA_RELOC_TREE_OBJECTID: u64 = (-9i64) as u64;
pub const DEV_ITEMS_OBJECTID: u64 = 1;
pub const FIRST_CHUNK_TREE_OBJECTID: u64 = 256;
pub const FIRST_FREE_OBJECTID: u64 = 256;

// ── key types ──
pub const INODE_ITEM_KEY: u8 = 1;
pub const INODE_REF_KEY: u8 = 12;
pub const DIR_ITEM_KEY: u8 = 84;
pub const ROOT_ITEM_KEY: u8 = 132;
pub const METADATA_ITEM_KEY: u8 = 169;
pub const TREE_BLOCK_REF_KEY: u8 = 176;
pub const BLOCK_GROUP_ITEM_KEY: u8 = 192;
pub const FREE_SPACE_INFO_KEY: u8 = 198;
pub const FREE_SPACE_EXTENT_KEY: u8 = 199;
pub const DEV_EXTENT_KEY: u8 = 204;
pub const DEV_ITEM_KEY: u8 = 216;
pub const CHUNK_ITEM_KEY: u8 = 228;
pub const PERSISTENT_ITEM_KEY: u8 = 249;
pub const UUID_KEY_SUBVOL: u8 = 251;

// ── block group / chunk type flags ──
pub const BG_DATA: u64 = 1 << 0;
pub const BG_SYSTEM: u64 = 1 << 1;
pub const BG_METADATA: u64 = 1 << 2;
pub const BG_DUP: u64 = 1 << 5;

// ── feature flags ──
pub const COMPAT_RO_FREE_SPACE_TREE: u64 = 1 << 0;
pub const COMPAT_RO_FREE_SPACE_TREE_VALID: u64 = 1 << 1;
pub const INCOMPAT_MIXED_BACKREF: u64 = 1 << 0;
pub const INCOMPAT_BIG_METADATA: u64 = 1 << 5;
pub const INCOMPAT_EXTENDED_IREF: u64 = 1 << 6;
pub const INCOMPAT_SKINNY_METADATA: u64 = 1 << 8;
pub const INCOMPAT_NO_HOLES: u64 = 1 << 9;

/// Tree block header flag "written" (also used as the superblock flag) and the backref
/// revision stored in the header flags' top byte.
const HEADER_FLAG_WRITTEN: u64 = 1;
const MIXED_BACKREF_REV: u64 = 1;
const EXTENT_FLAG_TREE_BLOCK: u64 = 2;
/// `BTRFS_INODE_ROOT_ITEM_INIT`: the root item's embedded inode is initialised.
const INODE_ROOT_ITEM_INIT: u64 = 1 << 31;
const FT_DIR: u8 = 2;

/// struct btrfs_header: 101 bytes (csum 32, fsid 16, bytenr 8, flags 8, chunk_tree_uuid 16,
/// generation 8, owner 8, nritems 4, level 1).
pub const HEADER_SIZE: usize = 101;
/// struct btrfs_item: disk key (17) + data offset (4) + data size (4).
pub const ITEM_SIZE: usize = 25;
/// struct btrfs_key_ptr: disk key (17) + block pointer (8) + generation (8).
pub const KEY_PTR_SIZE: usize = 33;
const ROOT_ITEM_SIZE: usize = 439;
const INODE_ITEM_SIZE: usize = 160;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub objectid: u64,
    pub ty: u8,
    pub offset: u64,
}

impl Key {
    pub const fn new(objectid: u64, ty: u8, offset: u64) -> Self {
        Key { objectid, ty, offset }
    }
    fn write(&self, b: &mut [u8]) {
        put64(b, 0, self.objectid);
        b[8] = self.ty;
        put64(b, 9, self.offset);
    }
    pub fn read(b: &[u8]) -> Self {
        Key { objectid: get64(b, 0), ty: b[8], offset: get64(b, 9) }
    }
}

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
#[cfg(test)]
pub(crate) fn get32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
pub(crate) fn get64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// Standard crc32c (initial value and final xor ~0), as btrfs stores it.
pub fn crc32c(data: &[u8]) -> u32 {
    !crc32c_raw(!0, data)
}

/// `btrfs_name_hash()`: the kernel's raw crc32c seeded with ~1, no final inversion.
pub fn name_hash(name: &[u8]) -> u32 {
    crc32c_raw(!1, name)
}

/// Checksum of a tree block or superblock: crc32c of everything after the 32-byte csum field,
/// stored in its first 4 bytes.
fn seal(buf: &mut [u8]) {
    let c = crc32c(&buf[32..]);
    buf[..32].fill(0);
    put32(buf, 0, c);
}

fn invalid(it: &str, en: &str) -> FsError {
    FsError::Invalid(t(it, en))
}

fn now() -> (u64, u32) {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs(), d.subsec_nanos()),
        Err(_) => (0, 0),
    }
}

fn random_uuid() -> [u8; 16] {
    *uuid::Uuid::new_v4().as_bytes()
}

/// One chunk: a logical range mapped to one (single) or two (DUP) physical stripes.
#[derive(Clone, Debug)]
pub struct Chunk {
    pub logical: u64,
    pub length: u64,
    pub flags: u64,
    pub stripes: Vec<u64>,
}

impl Chunk {
    /// Physical offsets of every copy of the logical range starting at `logical`.
    fn physical(&self, logical: u64) -> Vec<u64> {
        self.stripes.iter().map(|s| s + (logical - self.logical)).collect()
    }
    /// Device bytes taken by the chunk (all stripes).
    fn dev_bytes(&self) -> u64 {
        self.length * self.stripes.len() as u64
    }
}

/// Chunk layout for a device of `total` usable bytes (already a multiple of [`SECTORSIZE`]).
///
/// Physical order: data, system copy 0, system copy 1, metadata copy 0, metadata copy 1 —
/// the order `mkfs.btrfs` ends up with. Logical addresses follow the same order from 1 MiB.
/// The metadata stripe is 5% of the device (10% for both DUP copies, the cap `mkfs.btrfs`
/// applies to any chunk), at least 32 MiB and at most 256 MiB, in 64 KiB units.
pub fn plan_chunks(total: u64) -> FsResult<Vec<Chunk>> {
    let meta = (total / 20 / STRIPE_LEN * STRIPE_LEN).clamp(META_STRIPE_MIN, META_STRIPE_MAX);
    let data_phys = RESERVED_START;
    let sys0 = data_phys + DATA_CHUNK;
    let sys1 = sys0 + SYSTEM_STRIPE;
    let meta0 = sys1 + SYSTEM_STRIPE;
    let meta1 = meta0 + meta;
    let end = meta1 + meta;
    if end > total {
        return Err(too_small());
    }
    let data_log = RESERVED_START;
    let sys_log = data_log + DATA_CHUNK;
    let meta_log = sys_log + SYSTEM_STRIPE;
    Ok(vec![
        Chunk { logical: data_log, length: DATA_CHUNK, flags: BG_DATA, stripes: vec![data_phys] },
        Chunk { logical: sys_log, length: SYSTEM_STRIPE, flags: BG_SYSTEM | BG_DUP, stripes: vec![sys0, sys1] },
        Chunk { logical: meta_log, length: meta, flags: BG_METADATA | BG_DUP, stripes: vec![meta0, meta1] },
    ])
}

fn too_small() -> FsError {
    invalid(
        &format!("dispositivo troppo piccolo per Btrfs: servono almeno {} MiB", MIN_DEV_SIZE.div_ceil(MIB)),
        &format!("device too small for Btrfs: at least {} MiB are needed", MIN_DEV_SIZE.div_ceil(MIB)),
    )
}

/// Validated geometry of one format run.
#[derive(Clone, Debug)]
pub struct Params {
    pub total_bytes: u64,
    pub nodesize: u32,
    pub label: Vec<u8>,
    pub fsid: [u8; 16],
    pub dev_uuid: [u8; 16],
    pub chunk_uuid: [u8; 16],
    pub fs_tree_uuid: [u8; 16],
    pub time: (u64, u32),
}

impl Params {
    pub fn new(dev_size: u64, opts: &MkfsOptions) -> FsResult<Self> {
        let nodesize = opts.cluster_size.unwrap_or(DEFAULT_NODESIZE);
        if !nodesize.is_power_of_two() || !(SECTORSIZE..=MAX_NODESIZE).contains(&nodesize) {
            return Err(invalid(
                &format!("dimensione nodo Btrfs non valida: {nodesize} (potenza di 2 tra 4096 e 65536)"),
                &format!("invalid Btrfs node size: {nodesize} (a power of 2 between 4096 and 65536)"),
            ));
        }
        let total_bytes = dev_size / SECTORSIZE as u64 * SECTORSIZE as u64;
        if total_bytes < MIN_DEV_SIZE {
            return Err(too_small());
        }
        // The superblock label is 256 bytes with a terminating NUL; never cut a UTF-8 char.
        let mut label = Vec::new();
        for c in opts.label.chars().filter(|c| *c != '\0') {
            let mut tmp = [0u8; 4];
            let s = c.encode_utf8(&mut tmp).as_bytes();
            if label.len() + s.len() > 255 {
                break;
            }
            label.extend_from_slice(s);
        }
        Ok(Params {
            total_bytes,
            nodesize,
            label,
            fsid: opts.uuid.unwrap_or_else(random_uuid),
            dev_uuid: random_uuid(),
            chunk_uuid: random_uuid(),
            fs_tree_uuid: random_uuid(),
            time: now(),
        })
    }
}

// ───────────────────────────── items ─────────────────────────────

type Item = (Key, Vec<u8>);

/// struct btrfs_inode_item (160 bytes) for an empty directory, as `mkfs.btrfs` writes the
/// top directory of a subvolume (nbytes = nodesize, like btrfs-progs' `btrfs_make_root_dir`).
fn dir_inode_item(p: &Params) -> Vec<u8> {
    let mut b = vec![0u8; INODE_ITEM_SIZE];
    put64(&mut b, 0, GEN); // generation
    put64(&mut b, 8, 0); // transid
    put64(&mut b, 16, 0); // size
    put64(&mut b, 24, p.nodesize as u64); // nbytes
    put32(&mut b, 40, 1); // nlink
    put32(&mut b, 52, 0o040755); // mode
    // atime, ctime, mtime, otime (btrfs_timespec: sec u64 + nsec u32) at 112..160
    for i in 0..4 {
        put64(&mut b, 112 + i * 12, p.time.0);
        put32(&mut b, 120 + i * 12, p.time.1);
    }
    b
}

/// struct btrfs_inode_ref: index (8), name_len (2), name.
fn inode_ref(index: u64, name: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 10 + name.len()];
    put64(&mut b, 0, index);
    put16(&mut b, 8, name.len() as u16);
    b[10..].copy_from_slice(name);
    b
}

/// struct btrfs_root_item (439 bytes).
fn root_item(p: &Params, bytenr: u64, level: u8, nblocks: u64, root_dirid: u64, uuid: Option<[u8; 16]>, with_times: bool) -> Vec<u8> {
    let mut b = vec![0u8; ROOT_ITEM_SIZE];
    // Embedded inode, as btrfs-progs initialises it for every new root.
    put64(&mut b, 0, 1); // generation
    put64(&mut b, 16, 3); // size
    put64(&mut b, 24, p.nodesize as u64); // nbytes
    put32(&mut b, 40, 1); // nlink
    put32(&mut b, 52, 0o040755); // mode
    put64(&mut b, 64, INODE_ROOT_ITEM_INIT); // flags
    let o = INODE_ITEM_SIZE;
    put64(&mut b, o, GEN); // generation
    put64(&mut b, o + 8, root_dirid);
    put64(&mut b, o + 16, bytenr);
    put64(&mut b, o + 24, 0); // byte_limit
    put64(&mut b, o + 32, nblocks * p.nodesize as u64); // bytes_used
    put64(&mut b, o + 40, 0); // last_snapshot
    put64(&mut b, o + 48, 0); // flags
    put32(&mut b, o + 56, 1); // refs
    // drop_progress (17) at o+60, drop_level at o+77
    b[o + 78] = level;
    put64(&mut b, o + 79, GEN); // generation_v2 (== generation: the v2 fields are valid)
    if let Some(u) = uuid {
        b[o + 87..o + 103].copy_from_slice(&u);
    }
    // parent_uuid o+103, received_uuid o+119, c/o/s/rtransid o+135..o+167
    if with_times {
        // ctime at o+167, otime at o+179
        put64(&mut b, o + 167, p.time.0);
        put32(&mut b, o + 175, p.time.1);
        put64(&mut b, o + 179, p.time.0);
        put32(&mut b, o + 187, p.time.1);
    }
    b
}

/// struct btrfs_dev_item (98 bytes).
fn dev_item(p: &Params, bytes_used: u64) -> Vec<u8> {
    let mut b = vec![0u8; 98];
    put64(&mut b, 0, 1); // devid
    put64(&mut b, 8, p.total_bytes);
    put64(&mut b, 16, bytes_used);
    put32(&mut b, 24, SECTORSIZE); // io_align
    put32(&mut b, 28, SECTORSIZE); // io_width
    put32(&mut b, 32, SECTORSIZE); // sector_size
    // type 36, generation 44, start_offset 52, dev_group 60, seek_speed 64, bandwidth 65
    b[66..82].copy_from_slice(&p.dev_uuid);
    b[82..98].copy_from_slice(&p.fsid);
    b
}

/// struct btrfs_chunk (48 bytes) + one struct btrfs_stripe (32 bytes) per copy.
fn chunk_item(p: &Params, c: &Chunk) -> Vec<u8> {
    let mut b = vec![0u8; 48 + 32 * c.stripes.len()];
    put64(&mut b, 0, c.length);
    put64(&mut b, 8, EXTENT_TREE_OBJECTID); // owner
    put64(&mut b, 16, STRIPE_LEN);
    put64(&mut b, 24, c.flags);
    put32(&mut b, 32, STRIPE_LEN as u32); // io_align
    put32(&mut b, 36, STRIPE_LEN as u32); // io_width
    put32(&mut b, 40, SECTORSIZE); // sector_size
    put16(&mut b, 44, c.stripes.len() as u16); // num_stripes
    put16(&mut b, 46, 1); // sub_stripes
    for (i, s) in c.stripes.iter().enumerate() {
        let o = 48 + 32 * i;
        put64(&mut b, o, 1); // devid
        put64(&mut b, o + 8, *s);
        b[o + 16..o + 32].copy_from_slice(&p.dev_uuid);
    }
    b
}

/// struct btrfs_dev_extent (48 bytes).
fn dev_extent(p: &Params, c: &Chunk) -> Vec<u8> {
    let mut b = vec![0u8; 48];
    put64(&mut b, 0, CHUNK_TREE_OBJECTID);
    put64(&mut b, 8, FIRST_CHUNK_TREE_OBJECTID);
    put64(&mut b, 16, c.logical);
    put64(&mut b, 24, c.length);
    b[32..48].copy_from_slice(&p.chunk_uuid);
    b
}

/// struct btrfs_block_group_item (24 bytes).
fn block_group_item(used: u64, flags: u64) -> Vec<u8> {
    let mut b = vec![0u8; 24];
    put64(&mut b, 0, used);
    put64(&mut b, 8, FIRST_CHUNK_TREE_OBJECTID);
    put64(&mut b, 16, flags);
    b
}

/// Skinny METADATA_ITEM: struct btrfs_extent_item (refs, generation, flags) followed by one
/// inline TREE_BLOCK_REF naming the owning tree.
fn metadata_item(owner: u64) -> Vec<u8> {
    let mut b = vec![0u8; 33];
    put64(&mut b, 0, 1); // refs
    put64(&mut b, 8, GEN);
    put64(&mut b, 16, EXTENT_FLAG_TREE_BLOCK);
    b[24] = TREE_BLOCK_REF_KEY;
    put64(&mut b, 25, owner);
    b
}

/// struct btrfs_dir_item (30 bytes) + name.
fn dir_item(location: Key, name: &[u8], ty: u8) -> Vec<u8> {
    let mut b = vec![0u8; 30 + name.len()];
    location.write(&mut b[0..17]);
    put64(&mut b, 17, GEN); // transid
    put16(&mut b, 25, 0); // data_len
    put16(&mut b, 27, name.len() as u16);
    b[29] = ty;
    b[30..].copy_from_slice(name);
    b
}

/// Items of a subvolume-like tree holding only its empty top directory (fs tree and data
/// relocation tree): the inode and its ".." reference to itself.
fn top_dir_items(p: &Params) -> Vec<Item> {
    vec![
        (Key::new(FIRST_FREE_OBJECTID, INODE_ITEM_KEY, 0), dir_inode_item(p)),
        (Key::new(FIRST_FREE_OBJECTID, INODE_REF_KEY, FIRST_FREE_OBJECTID), inode_ref(0, b"..")),
    ]
}

// ───────────────────────────── trees ─────────────────────────────

/// Number of blocks per level (index 0 = leaves) a tree with these item sizes needs.
/// Leaves are filled in key order; internal nodes hold up to (nodesize-101)/33 pointers.
pub fn tree_shape(item_sizes: &[usize], nodesize: u32) -> FsResult<Vec<usize>> {
    let cap = nodesize as usize - HEADER_SIZE;
    let mut leaves = 1usize;
    let mut used = 0usize;
    for &s in item_sizes {
        let need = ITEM_SIZE + s;
        if need > cap {
            return Err(FsError::Invalid(format!("btrfs: item of {s} bytes does not fit a {nodesize}-byte node")));
        }
        if used + need > cap {
            leaves += 1;
            used = 0;
        }
        used += need;
    }
    let fanout = cap / KEY_PTR_SIZE;
    let mut shape = vec![leaves];
    let mut n = leaves;
    while n > 1 {
        n = n.div_ceil(fanout);
        shape.push(n);
        if shape.len() > 8 {
            return Err(FsError::Invalid("btrfs: tree too deep".into()));
        }
    }
    Ok(shape)
}

fn total_blocks(shape: &[usize]) -> usize {
    shape.iter().sum()
}

/// A tree ready to be laid out: its owner and sorted items.
struct TreeSpec {
    owner: u64,
    items: Vec<Item>,
}

impl TreeSpec {
    fn new(owner: u64, mut items: Vec<Item>) -> Self {
        items.sort_by(|a, b| a.0.cmp(&b.0));
        TreeSpec { owner, items }
    }
    fn shape(&self, nodesize: u32) -> FsResult<Vec<usize>> {
        tree_shape(&self.items.iter().map(|i| i.1.len()).collect::<Vec<_>>(), nodesize)
    }
}

/// A built tree: its blocks (logical address, contents), root address and level.
pub struct BuiltTree {
    pub blocks: Vec<(u64, Vec<u8>)>,
    pub root: u64,
    pub level: u8,
}

fn header(buf: &mut [u8], p: &Params, bytenr: u64, owner: u64, nritems: u32, level: u8) {
    buf[32..48].copy_from_slice(&p.fsid);
    put64(buf, 48, bytenr);
    put64(buf, 56, HEADER_FLAG_WRITTEN | (MIXED_BACKREF_REV << 56));
    buf[64..80].copy_from_slice(&p.chunk_uuid);
    put64(buf, 80, GEN);
    put64(buf, 88, owner);
    put32(buf, 96, nritems);
    buf[100] = level;
}

/// Lay `spec` out on the blocks at `addrs` (as many as [`tree_shape`] says, leaves first).
fn build_tree(p: &Params, spec: &TreeSpec, addrs: &[u64]) -> FsResult<BuiltTree> {
    let ns = p.nodesize as usize;
    let cap = ns - HEADER_SIZE;
    let shape = spec.shape(p.nodesize)?;
    if addrs.len() != total_blocks(&shape) {
        return Err(FsError::Invalid("btrfs: tree block count mismatch".into()));
    }
    // Pack leaves exactly like tree_shape does.
    let mut groups: Vec<Vec<&Item>> = vec![Vec::new()];
    let mut used = 0usize;
    for it in &spec.items {
        let need = ITEM_SIZE + it.1.len();
        if used + need > cap {
            groups.push(Vec::new());
            used = 0;
        }
        used += need;
        if let Some(g) = groups.last_mut() {
            g.push(it);
        }
    }
    let mut blocks = Vec::with_capacity(addrs.len());
    let mut next = addrs.iter().copied();
    // (first key, address) of every block of the level just built.
    let mut level_refs: Vec<(Key, u64)> = Vec::new();
    for g in &groups {
        let addr = next.next().ok_or_else(|| FsError::Invalid("btrfs: tree block count mismatch".into()))?;
        let mut buf = vec![0u8; ns];
        header(&mut buf, p, addr, spec.owner, g.len() as u32, 0);
        let mut data_end = cap; // item data grows down from the end of the leaf
        for (i, (key, data)) in g.iter().enumerate() {
            data_end -= data.len();
            let io = HEADER_SIZE + i * ITEM_SIZE;
            key.write(&mut buf[io..io + 17]);
            put32(&mut buf, io + 17, data_end as u32);
            put32(&mut buf, io + 21, data.len() as u32);
            buf[HEADER_SIZE + data_end..HEADER_SIZE + data_end + data.len()].copy_from_slice(data);
        }
        seal(&mut buf);
        level_refs.push((g.first().map(|i| i.0).unwrap_or(Key::new(0, 0, 0)), addr));
        blocks.push((addr, buf));
    }
    let fanout = cap / KEY_PTR_SIZE;
    let mut level = 0u8;
    while level_refs.len() > 1 {
        level += 1;
        let mut up = Vec::new();
        for chunk in level_refs.chunks(fanout) {
            let addr = next.next().ok_or_else(|| FsError::Invalid("btrfs: tree block count mismatch".into()))?;
            let mut buf = vec![0u8; ns];
            header(&mut buf, p, addr, spec.owner, chunk.len() as u32, level);
            for (i, (key, child)) in chunk.iter().enumerate() {
                let o = HEADER_SIZE + i * KEY_PTR_SIZE;
                key.write(&mut buf[o..o + 17]);
                put64(&mut buf, o + 17, *child);
                put64(&mut buf, o + 25, GEN);
            }
            seal(&mut buf);
            up.push((chunk[0].0, addr));
            blocks.push((addr, buf));
        }
        level_refs = up;
    }
    let root = level_refs.first().map(|r| r.1).ok_or_else(|| FsError::Invalid("btrfs: empty tree".into()))?;
    Ok(BuiltTree { blocks, root, level })
}

/// (address, level) of every block of a tree of this shape laid out on `addrs`.
fn block_levels(shape: &[usize], addrs: &[u64]) -> Vec<(u64, u8)> {
    let mut out = Vec::new();
    let mut i = 0;
    for (lvl, &n) in shape.iter().enumerate() {
        for _ in 0..n {
            if let Some(a) = addrs.get(i) {
                out.push((*a, lvl as u8));
            }
            i += 1;
        }
    }
    out
}

// ───────────────────────────── the filesystem ─────────────────────────────

/// Everything `format` writes, computed in memory: (physical offset, bytes) pairs.
pub struct Image {
    pub params: Params,
    pub chunks: Vec<Chunk>,
    /// Tree blocks (every copy) at their physical offsets.
    pub writes: Vec<(u64, Vec<u8>)>,
    /// Superblock copies at their physical offsets.
    pub supers: Vec<(u64, Vec<u8>)>,
}

/// Lay out the whole filesystem for a device of `dev_size` bytes.
pub fn build(dev_size: u64, opts: &MkfsOptions) -> FsResult<Image> {
    let p = Params::new(dev_size, opts)?;
    let ns = p.nodesize as u64;
    let chunks = plan_chunks(p.total_bytes)?;
    let (data, sys, meta) = (&chunks[0], &chunks[1], &chunks[2]);

    // Trees whose contents don't depend on where tree blocks go.
    let chunk_tree = {
        let dev_bytes: u64 = chunks.iter().map(Chunk::dev_bytes).sum();
        let mut items = vec![(Key::new(DEV_ITEMS_OBJECTID, DEV_ITEM_KEY, 1), dev_item(&p, dev_bytes))];
        for c in &chunks {
            items.push((Key::new(FIRST_CHUNK_TREE_OBJECTID, CHUNK_ITEM_KEY, c.logical), chunk_item(&p, c)));
        }
        TreeSpec::new(CHUNK_TREE_OBJECTID, items)
    };
    let dev_tree = {
        let mut items = vec![(Key::new(DEV_STATS_OBJECTID, PERSISTENT_ITEM_KEY, 1), vec![0u8; 40])];
        for c in &chunks {
            for s in &c.stripes {
                items.push((Key::new(1, DEV_EXTENT_KEY, *s), dev_extent(&p, c)));
            }
        }
        TreeSpec::new(DEV_TREE_OBJECTID, items)
    };
    let fs_tree = TreeSpec::new(FS_TREE_OBJECTID, top_dir_items(&p));
    let reloc_tree = TreeSpec::new(DATA_RELOC_TREE_OBJECTID, top_dir_items(&p));
    let csum_tree = TreeSpec::new(CSUM_TREE_OBJECTID, Vec::new());
    let uuid_tree = {
        // UUID_KEY_SUBVOL: the subvolume uuid split into two little-endian u64s.
        let k = Key::new(get64(&p.fs_tree_uuid, 0), UUID_KEY_SUBVOL, get64(&p.fs_tree_uuid, 8));
        TreeSpec::new(UUID_TREE_OBJECTID, vec![(k, FS_TREE_OBJECTID.to_le_bytes().to_vec())])
    };
    // Root tree contents are only addresses: its shape is known from the item sizes.
    let root_items_shape = |p: &Params| -> Vec<Item> { root_tree_items(p, &[]) };
    let root_shape = TreeSpec::new(ROOT_TREE_OBJECTID, root_items_shape(&p)).shape(p.nodesize)?;
    // Free space tree: one FREE_SPACE_INFO per block group and one extent for its free tail
    // (tree blocks are allocated from the start of each group, so free space is contiguous).
    let fst_items_for = |sys_used: u64, meta_used: u64| -> Vec<Item> {
        let mut items = Vec::new();
        for (c, used) in [(data, 0), (sys, sys_used), (meta, meta_used)] {
            let free = c.length.saturating_sub(used);
            let count: u32 = if free > 0 { 1 } else { 0 };
            let mut info = vec![0u8; 8];
            put32(&mut info, 0, count); // extent_count
            put32(&mut info, 4, 0); // flags (extents, not bitmaps)
            items.push((Key::new(c.logical, FREE_SPACE_INFO_KEY, c.length), info));
            if free > 0 {
                items.push((Key::new(c.logical + used, FREE_SPACE_EXTENT_KEY, free), Vec::new()));
            }
        }
        items
    };

    let fixed_meta = [&dev_tree, &fs_tree, &reloc_tree, &csum_tree, &uuid_tree];
    let mut fixed_shapes = Vec::new();
    for t in fixed_meta {
        fixed_shapes.push(t.shape(p.nodesize)?);
    }
    let chunk_shape = chunk_tree.shape(p.nodesize)?;
    let n_chunk = total_blocks(&chunk_shape) as u64;

    // Fixed point: the extent tree has one item per tree block, its own included; the free
    // space tree's size does not depend on the counts (one extent per group either way,
    // as long as the groups are not full, which is checked below).
    let fst_shape = TreeSpec::new(FREE_SPACE_TREE_OBJECTID, fst_items_for(ns, ns)).shape(p.nodesize)?;
    let n_fixed: u64 = fixed_shapes.iter().map(|s| total_blocks(s) as u64).sum::<u64>()
        + total_blocks(&root_shape) as u64
        + total_blocks(&fst_shape) as u64;
    let mut n_extent = 1u64;
    let mut extent_shape;
    loop {
        let n_items = 3 + n_chunk + n_fixed + n_extent;
        let sizes: Vec<usize> = (0..n_items).map(|i| if i < 3 { 24 } else { 33 }).collect();
        extent_shape = tree_shape(&sizes, p.nodesize)?;
        let n = total_blocks(&extent_shape) as u64;
        if n == n_extent {
            break;
        }
        n_extent = n;
    }
    let n_meta = n_fixed + n_extent;
    let sys_used = n_chunk * ns;
    let meta_used = n_meta * ns;
    if sys_used >= sys.length || meta_used >= meta.length {
        return Err(FsError::Invalid("btrfs: metadata does not fit its chunk".into()));
    }

    // Addresses: chunk tree at the start of the system chunk; the other trees one after the
    // other from the start of the metadata chunk.
    let sys_addrs: Vec<u64> = (0..n_chunk).map(|i| sys.logical + i * ns).collect();
    let mut cursor = meta.logical;
    let mut take = |n: usize| -> Vec<u64> {
        let v: Vec<u64> = (0..n as u64).map(|i| cursor + i * ns).collect();
        cursor += n as u64 * ns;
        v
    };
    let root_addrs = take(total_blocks(&root_shape));
    let extent_addrs = take(total_blocks(&extent_shape));
    let fixed_addrs: Vec<Vec<u64>> = fixed_shapes.iter().map(|s| take(total_blocks(s))).collect();
    let fst_addrs = take(total_blocks(&fst_shape));

    // Extent tree: block groups + one skinny metadata item per tree block.
    let mut ext_items = vec![
        (Key::new(data.logical, BLOCK_GROUP_ITEM_KEY, data.length), block_group_item(0, data.flags)),
        (Key::new(sys.logical, BLOCK_GROUP_ITEM_KEY, sys.length), block_group_item(sys_used, sys.flags)),
        (Key::new(meta.logical, BLOCK_GROUP_ITEM_KEY, meta.length), block_group_item(meta_used, meta.flags)),
    ];
    let mut owners: Vec<(u64, &[usize], &[u64])> = vec![
        (CHUNK_TREE_OBJECTID, &chunk_shape, &sys_addrs),
        (ROOT_TREE_OBJECTID, &root_shape, &root_addrs),
        (EXTENT_TREE_OBJECTID, &extent_shape, &extent_addrs),
        (FREE_SPACE_TREE_OBJECTID, &fst_shape, &fst_addrs),
    ];
    for (i, t) in fixed_meta.iter().enumerate() {
        owners.push((t.owner, &fixed_shapes[i], &fixed_addrs[i]));
    }
    for (owner, shape, addrs) in &owners {
        for (a, lvl) in block_levels(shape, addrs) {
            ext_items.push((Key::new(a, METADATA_ITEM_KEY, lvl as u64), metadata_item(*owner)));
        }
    }
    let extent_tree = TreeSpec::new(EXTENT_TREE_OBJECTID, ext_items);
    let fst = TreeSpec::new(FREE_SPACE_TREE_OBJECTID, fst_items_for(sys_used, meta_used));

    let mut built = Vec::new();
    let chunk_built = build_tree(&p, &chunk_tree, &sys_addrs)?;
    let extent_built = build_tree(&p, &extent_tree, &extent_addrs)?;
    let fst_built = build_tree(&p, &fst, &fst_addrs)?;
    let mut roots: Vec<(u64, u64, u8, u64)> = vec![
        (EXTENT_TREE_OBJECTID, extent_built.root, extent_built.level, extent_built.blocks.len() as u64),
        (FREE_SPACE_TREE_OBJECTID, fst_built.root, fst_built.level, fst_built.blocks.len() as u64),
    ];
    for (i, t) in fixed_meta.iter().enumerate() {
        let b = build_tree(&p, t, &fixed_addrs[i])?;
        roots.push((t.owner, b.root, b.level, b.blocks.len() as u64));
        built.push(b);
    }
    let root_tree = TreeSpec::new(ROOT_TREE_OBJECTID, root_tree_items(&p, &roots));
    if root_tree.shape(p.nodesize)? != root_shape {
        return Err(FsError::Invalid("btrfs: root tree shape changed".into()));
    }
    let root_built = build_tree(&p, &root_tree, &root_addrs)?;

    let find_root = |id: u64| roots.iter().find(|r| r.0 == id).map(|r| (r.1, r.2)).unwrap_or((0, 0));
    let sb = SuperInfo {
        root: (root_built.root, root_built.level),
        chunk_root: (chunk_built.root, chunk_built.level),
        extent_root: find_root(EXTENT_TREE_OBJECTID),
        fs_root: find_root(FS_TREE_OBJECTID),
        dev_root: find_root(DEV_TREE_OBJECTID),
        csum_root: find_root(CSUM_TREE_OBJECTID),
        bytes_used: (n_chunk + n_meta) * ns,
        dev_bytes_used: chunks.iter().map(Chunk::dev_bytes).sum(),
    };

    let mut writes = Vec::new();
    built.push(chunk_built);
    built.push(extent_built);
    built.push(fst_built);
    built.push(root_built);
    for t in &built {
        for (logical, buf) in &t.blocks {
            let c = chunks
                .iter()
                .find(|c| *logical >= c.logical && *logical < c.logical + c.length)
                .ok_or_else(|| FsError::Invalid("btrfs: tree block outside every chunk".into()))?;
            for phys in c.physical(*logical) {
                writes.push((phys, buf.clone()));
            }
        }
    }
    let supers = SUPER_OFFSETS
        .iter()
        .filter(|off| **off + SUPER_SIZE as u64 <= p.total_bytes)
        .map(|off| (*off, superblock(&p, &sb, sys, *off)))
        .collect();
    Ok(Image { params: p, chunks, writes, supers })
}

/// Root tree: ROOT_ITEMs of every tree but itself and the chunk tree, plus the root tree
/// directory (objectid 6) with its "default" entry pointing at the fs tree, exactly like
/// `mkfs.btrfs` (`btrfs_make_root_dir` + the "default" dir item and back reference).
/// `roots`: (tree id, root block, level, block count); with an empty slice it yields placeholder items of
/// the right sizes (used to size the tree before addresses exist).
fn root_tree_items(p: &Params, roots: &[(u64, u64, u8, u64)]) -> Vec<Item> {
    let get = |id: u64| roots.iter().find(|r| r.0 == id).map(|r| (r.1, r.2, r.3)).unwrap_or((0, 0, 1));
    let mut items = Vec::new();
    for id in [EXTENT_TREE_OBJECTID, DEV_TREE_OBJECTID, CSUM_TREE_OBJECTID, UUID_TREE_OBJECTID, FREE_SPACE_TREE_OBJECTID] {
        let (b, l, n) = get(id);
        items.push((Key::new(id, ROOT_ITEM_KEY, 0), root_item(p, b, l, n, 0, None, false)));
    }
    let (b, l, n) = get(FS_TREE_OBJECTID);
    items.push((Key::new(FS_TREE_OBJECTID, ROOT_ITEM_KEY, 0), root_item(p, b, l, n, FIRST_FREE_OBJECTID, Some(p.fs_tree_uuid), true)));
    let (b, l, n) = get(DATA_RELOC_TREE_OBJECTID);
    items.push((Key::new(DATA_RELOC_TREE_OBJECTID, ROOT_ITEM_KEY, 0), root_item(p, b, l, n, FIRST_FREE_OBJECTID, None, false)));
    items.push((Key::new(ROOT_TREE_DIR_OBJECTID, INODE_ITEM_KEY, 0), dir_inode_item(p)));
    items.push((Key::new(ROOT_TREE_DIR_OBJECTID, INODE_REF_KEY, ROOT_TREE_DIR_OBJECTID), inode_ref(0, b"..")));
    items.push((
        Key::new(ROOT_TREE_DIR_OBJECTID, DIR_ITEM_KEY, name_hash(b"default") as u64),
        dir_item(Key::new(FS_TREE_OBJECTID, ROOT_ITEM_KEY, u64::MAX), b"default", FT_DIR),
    ));
    items.push((Key::new(FS_TREE_OBJECTID, INODE_REF_KEY, ROOT_TREE_DIR_OBJECTID), inode_ref(0, b"default")));
    items
}

struct SuperInfo {
    root: (u64, u8),
    chunk_root: (u64, u8),
    extent_root: (u64, u8),
    fs_root: (u64, u8),
    dev_root: (u64, u8),
    csum_root: (u64, u8),
    bytes_used: u64,
    dev_bytes_used: u64,
}

// struct btrfs_super_block field offsets.
pub const SB_FSID: usize = 0x20;
pub const SB_BYTENR: usize = 0x30;
pub const SB_FLAGS: usize = 0x38;
pub const SB_MAGIC: usize = 0x40;
pub const SB_GENERATION: usize = 0x48;
pub const SB_ROOT: usize = 0x50;
pub const SB_CHUNK_ROOT: usize = 0x58;
pub const SB_TOTAL_BYTES: usize = 0x70;
pub const SB_BYTES_USED: usize = 0x78;
pub const SB_ROOT_DIR: usize = 0x80;
pub const SB_NUM_DEVICES: usize = 0x88;
pub const SB_SECTORSIZE: usize = 0x90;
pub const SB_NODESIZE: usize = 0x94;
pub const SB_LEAFSIZE: usize = 0x98;
pub const SB_STRIPESIZE: usize = 0x9c;
pub const SB_SYS_ARRAY_SIZE: usize = 0xa0;
pub const SB_CHUNK_ROOT_GEN: usize = 0xa4;
pub const SB_COMPAT: usize = 0xac;
pub const SB_COMPAT_RO: usize = 0xb4;
pub const SB_INCOMPAT: usize = 0xbc;
pub const SB_CSUM_TYPE: usize = 0xc4;
pub const SB_ROOT_LEVEL: usize = 0xc6;
pub const SB_CHUNK_ROOT_LEVEL: usize = 0xc7;
pub const SB_DEV_ITEM: usize = 0xc9;
pub const SB_LABEL: usize = 0x12b;
pub const SB_CACHE_GEN: usize = 0x22b;
pub const SB_UUID_TREE_GEN: usize = 0x233;
pub const SB_SYS_CHUNK_ARRAY: usize = 0x32b;
pub const SB_SYS_CHUNK_ARRAY_MAX: usize = 2048;
pub const SB_BACKUP_ROOTS: usize = 0xb2b;

fn superblock(p: &Params, s: &SuperInfo, sys: &Chunk, bytenr: u64) -> Vec<u8> {
    let mut b = vec![0u8; SUPER_SIZE];
    b[SB_FSID..SB_FSID + 16].copy_from_slice(&p.fsid);
    put64(&mut b, SB_BYTENR, bytenr);
    put64(&mut b, SB_FLAGS, HEADER_FLAG_WRITTEN);
    put64(&mut b, SB_MAGIC, MAGIC);
    put64(&mut b, SB_GENERATION, GEN);
    put64(&mut b, SB_ROOT, s.root.0);
    put64(&mut b, SB_CHUNK_ROOT, s.chunk_root.0);
    put64(&mut b, SB_TOTAL_BYTES, p.total_bytes);
    put64(&mut b, SB_BYTES_USED, s.bytes_used);
    put64(&mut b, SB_ROOT_DIR, ROOT_TREE_DIR_OBJECTID);
    put64(&mut b, SB_NUM_DEVICES, 1);
    put32(&mut b, SB_SECTORSIZE, SECTORSIZE);
    put32(&mut b, SB_NODESIZE, p.nodesize);
    put32(&mut b, SB_LEAFSIZE, p.nodesize);
    put32(&mut b, SB_STRIPESIZE, SECTORSIZE);
    put64(&mut b, SB_CHUNK_ROOT_GEN, GEN);
    put64(&mut b, SB_COMPAT_RO, COMPAT_RO_FREE_SPACE_TREE | COMPAT_RO_FREE_SPACE_TREE_VALID);
    let mut incompat = INCOMPAT_MIXED_BACKREF | INCOMPAT_EXTENDED_IREF | INCOMPAT_SKINNY_METADATA | INCOMPAT_NO_HOLES;
    if p.nodesize > SECTORSIZE {
        incompat |= INCOMPAT_BIG_METADATA;
    }
    put64(&mut b, SB_INCOMPAT, incompat);
    put16(&mut b, SB_CSUM_TYPE, 0); // crc32c
    b[SB_ROOT_LEVEL] = s.root.1;
    b[SB_CHUNK_ROOT_LEVEL] = s.chunk_root.1;
    let di = dev_item(p, s.dev_bytes_used);
    b[SB_DEV_ITEM..SB_DEV_ITEM + di.len()].copy_from_slice(&di);
    b[SB_LABEL..SB_LABEL + p.label.len()].copy_from_slice(&p.label);
    put64(&mut b, SB_CACHE_GEN, 0); // no v1 space cache
    put64(&mut b, SB_UUID_TREE_GEN, GEN); // the uuid tree is complete: no rescan at mount
    // System chunk array: (disk key, chunk item) of every SYSTEM chunk.
    let ci = chunk_item(p, sys);
    let o = SB_SYS_CHUNK_ARRAY;
    Key::new(FIRST_CHUNK_TREE_OBJECTID, CHUNK_ITEM_KEY, sys.logical).write(&mut b[o..o + 17]);
    b[o + 17..o + 17 + ci.len()].copy_from_slice(&ci);
    put32(&mut b, SB_SYS_ARRAY_SIZE, (17 + ci.len()) as u32);
    // Backup root slot 0 (struct btrfs_root_backup, 168 bytes) = the current roots.
    let r = SB_BACKUP_ROOTS;
    let pairs = [s.root, s.chunk_root, s.extent_root, s.fs_root, s.dev_root, s.csum_root];
    for (i, (addr, _)) in pairs.iter().enumerate() {
        put64(&mut b, r + i * 16, *addr);
        put64(&mut b, r + i * 16 + 8, GEN);
    }
    put64(&mut b, r + 96, p.total_bytes);
    put64(&mut b, r + 104, s.bytes_used);
    put64(&mut b, r + 112, 1); // num_devices
    for (i, (_, lvl)) in pairs.iter().enumerate() {
        b[r + 152 + i] = *lvl;
    }
    seal(&mut b);
    b
}

/// Write the image to `dev`: wipe the first MiB (old signatures, boot code) and the last MiB
/// (old backup labels / RAID superblocks), then every structure.
fn write_image(dev: &dyn BlockDev, img: &Image) -> FsResult<()> {
    let total = img.params.total_bytes;
    let zero = vec![0u8; MIB as usize];
    dev.write_at(0, &zero)?;
    let tail = dev.size().saturating_sub(MIB) / SECTORSIZE as u64 * SECTORSIZE as u64;
    if tail >= MIB {
        // At most 1 MiB + one sector: the end of the device need not be sector-aligned.
        dev.write_at(tail, &vec![0u8; (dev.size() - tail) as usize])?;
    }
    // Stale superblock mirrors past the new filesystem's end would make blkid / the kernel
    // see a different, older filesystem.
    for off in SUPER_OFFSETS {
        if off + SUPER_SIZE as u64 > total && off + SUPER_SIZE as u64 <= dev.size() {
            dev.write_at(off, &zero[..SUPER_SIZE])?;
        }
    }
    // Superblocks last: until they exist the device holds no filesystem.
    for (off, buf) in &img.writes {
        dev.write_at(*off, buf)?;
    }
    dev.flush()?;
    for (off, buf) in &img.supers {
        dev.write_at(*off, buf)?;
    }
    dev.flush()?;
    Ok(())
}

pub fn format(dev: Arc<dyn BlockDev>, opts: &MkfsOptions) -> FsResult<()> {
    if dev.read_only() {
        return Err(FsError::ReadOnly);
    }
    let img = build(dev.size(), opts)?;
    write_image(dev.as_ref(), &img)
}
