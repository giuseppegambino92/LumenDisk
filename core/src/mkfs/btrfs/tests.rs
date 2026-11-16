//! Structural checks of the Btrfs image: a small independent reader walks every tree from
//! the superblock, verifying checksums, ordering and cross references.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::*;
use crate::block::MemDev;

const MB: u64 = 1 << 20;

fn opts(label: &str, nodesize: Option<u32>) -> MkfsOptions {
    MkfsOptions { label: label.into(), cluster_size: nodesize, uuid: None }
}

/// Minimal reader over a formatted image.
struct Reader {
    img: Vec<u8>,
    nodesize: usize,
    chunks: Vec<(u64, u64, Vec<u64>)>, // logical, length, physical stripes
}

impl Reader {
    fn open(img: Vec<u8>) -> Reader {
        let sb = img[65536..65536 + SUPER_SIZE].to_vec();
        assert_eq!(get64(&sb, SB_MAGIC), MAGIC);
        assert_eq!(get32(&sb, 0), crc32c(&sb[32..]), "superblock csum");
        let nodesize = get32(&sb, SB_NODESIZE) as usize;
        let mut r = Reader { img, nodesize, chunks: Vec::new() };
        // Bootstrap from the system chunk array, then read the chunk tree.
        let n = get32(&sb, SB_SYS_ARRAY_SIZE) as usize;
        let arr = &sb[SB_SYS_CHUNK_ARRAY..SB_SYS_CHUNK_ARRAY + n];
        let mut o = 0;
        while o < n {
            let k = Key::read(&arr[o..]);
            assert_eq!(k.ty, CHUNK_ITEM_KEY);
            let c = &arr[o + 17..];
            let ns = u16::from_le_bytes([c[44], c[45]]) as usize;
            r.chunks.push((k.offset, get64(c, 0), (0..ns).map(|i| get64(c, 48 + 32 * i + 8)).collect()));
            o += 17 + 48 + 32 * ns;
        }
        let chunk_root = get64(&sb, SB_CHUNK_ROOT);
        let items = r.walk(chunk_root, CHUNK_TREE_OBJECTID, &mut Vec::new());
        r.chunks.clear();
        for (k, d) in &items {
            if k.ty == CHUNK_ITEM_KEY {
                let ns = u16::from_le_bytes([d[44], d[45]]) as usize;
                assert_eq!(d.len(), 48 + 32 * ns);
                r.chunks.push((k.offset, get64(d, 0), (0..ns).map(|i| get64(d, 48 + 32 * i + 8)).collect()));
            }
        }
        r
    }

    fn block(&self, logical: u64) -> Vec<u8> {
        let (l, len, stripes) = self.chunks.iter().find(|c| logical >= c.0 && logical < c.0 + c.1).expect("unmapped block");
        assert!(logical + self.nodesize as u64 <= l + len);
        let mut first: Option<Vec<u8>> = None;
        for s in stripes {
            let p = (s + logical - l) as usize;
            let b = self.img[p..p + self.nodesize].to_vec();
            if let Some(f) = &first {
                assert_eq!(f, &b, "DUP copies differ at {logical}");
            }
            first = Some(b);
        }
        first.unwrap()
    }

    /// All items of the tree rooted at `root`, checking headers, checksums and key order;
    /// `seen` collects (address, level) of every block.
    fn walk(&self, root: u64, owner: u64, seen: &mut Vec<(u64, u8)>) -> Vec<(Key, Vec<u8>)> {
        let b = self.block(root);
        assert_eq!(get32(&b, 0), crc32c(&b[32..]), "csum of block {root}");
        assert_eq!(get64(&b, 48), root, "bytenr");
        assert_eq!(get64(&b, 88), owner, "owner of {root}");
        assert_eq!(get64(&b, 80), GEN);
        let n = get32(&b, 96) as usize;
        let level = b[100];
        seen.push((root, level));
        let mut out = Vec::new();
        if level == 0 {
            let mut prev: Option<Key> = None;
            let mut data_floor = self.nodesize - HEADER_SIZE;
            for i in 0..n {
                let io = HEADER_SIZE + i * ITEM_SIZE;
                let k = Key::read(&b[io..]);
                if let Some(p) = prev {
                    assert!(p < k, "keys out of order");
                }
                prev = Some(k);
                let off = get32(&b, io + 17) as usize;
                let sz = get32(&b, io + 21) as usize;
                assert_eq!(off + sz, data_floor, "item data packed");
                data_floor = off;
                assert!(HEADER_SIZE + n * ITEM_SIZE <= HEADER_SIZE + off);
                out.push((k, b[HEADER_SIZE + off..HEADER_SIZE + off + sz].to_vec()));
            }
        } else {
            for i in 0..n {
                let o = HEADER_SIZE + i * KEY_PTR_SIZE;
                let k = Key::read(&b[o..]);
                let child = get64(&b, o + 17);
                let cb = self.block(child);
                assert_eq!(cb[100] + 1, level);
                let sub = self.walk(child, owner, seen);
                assert_eq!(sub.first().map(|s| s.0), Some(k), "node key = child's first key");
                out.extend(sub);
            }
        }
        out
    }
}

struct Walked {
    sb: Vec<u8>,
    trees: BTreeMap<u64, Vec<(Key, Vec<u8>)>>,
    blocks: Vec<(u64, u8, u64)>,
}

fn walk_all(img: Vec<u8>) -> (Reader, Walked) {
    let r = Reader::open(img);
    let sb = r.img[65536..65536 + SUPER_SIZE].to_vec();
    let mut trees = BTreeMap::new();
    let mut blocks = Vec::new();
    let mut seen = Vec::new();
    let chunk_items = r.walk(get64(&sb, SB_CHUNK_ROOT), CHUNK_TREE_OBJECTID, &mut seen);
    blocks.extend(seen.drain(..).map(|(a, l)| (a, l, CHUNK_TREE_OBJECTID)));
    trees.insert(CHUNK_TREE_OBJECTID, chunk_items);
    let root_items = r.walk(get64(&sb, SB_ROOT), ROOT_TREE_OBJECTID, &mut seen);
    assert_eq!(seen.iter().find(|s| s.0 == get64(&sb, SB_ROOT)).unwrap().1, sb[SB_ROOT_LEVEL]);
    blocks.extend(seen.drain(..).map(|(a, l)| (a, l, ROOT_TREE_OBJECTID)));
    for (k, d) in &root_items {
        if k.ty == ROOT_ITEM_KEY {
            let o = 160;
            let bytenr = get64(d, o + 16);
            assert_eq!(get64(d, o), GEN);
            assert_eq!(get64(d, o + 79), GEN, "generation_v2");
            let items = r.walk(bytenr, k.objectid, &mut seen);
            assert_eq!(seen.iter().find(|s| s.0 == bytenr).unwrap().1, d[o + 78], "root level");
            assert_eq!(get64(d, o + 32), (seen.len() * r.nodesize) as u64, "root bytes_used");
            blocks.extend(seen.drain(..).map(|(a, l)| (a, l, k.objectid)));
            trees.insert(k.objectid, items);
        }
    }
    trees.insert(ROOT_TREE_OBJECTID, root_items);
    (r, Walked { sb, trees, blocks })
}

fn format_mem(size: u64, o: &MkfsOptions) -> Vec<u8> {
    let dev = Arc::new(MemDev::new(size as usize));
    format(dev.clone(), o).expect("format");
    dev.snapshot()
}

fn check_image(img: Vec<u8>, nodesize: u32) -> Walked {
    let size = img.len() as u64;
    let (r, w) = walk_all(img);
    let sb = &w.sb;
    assert_eq!(get32(sb, SB_NODESIZE), nodesize);
    assert_eq!(get32(sb, SB_SECTORSIZE), 4096);
    assert_eq!(get64(sb, SB_TOTAL_BYTES), size / 4096 * 4096);
    let ns = nodesize as u64;
    // Every expected tree is present.
    for id in [ROOT_TREE_OBJECTID, CHUNK_TREE_OBJECTID, EXTENT_TREE_OBJECTID, DEV_TREE_OBJECTID, FS_TREE_OBJECTID, CSUM_TREE_OBJECTID, UUID_TREE_OBJECTID, FREE_SPACE_TREE_OBJECTID, DATA_RELOC_TREE_OBJECTID] {
        assert!(w.trees.contains_key(&id), "tree {id} missing");
    }
    // Extent tree: exactly one skinny metadata item per tree block, with its level & owner.
    let ext = &w.trees[&EXTENT_TREE_OBJECTID];
    let meta: BTreeSet<(u64, u64, u64)> = ext
        .iter()
        .filter(|(k, _)| k.ty == METADATA_ITEM_KEY)
        .map(|(k, d)| {
            assert_eq!(get64(d, 0), 1);
            assert_eq!(d[24], TREE_BLOCK_REF_KEY);
            (k.objectid, k.offset, get64(d, 25))
        })
        .collect();
    let blocks: BTreeSet<(u64, u64, u64)> = w.blocks.iter().map(|(a, l, o)| (*a, *l as u64, *o)).collect();
    assert_eq!(meta, blocks, "extent tree vs reachable blocks");
    // Block groups match chunks; used = tree blocks inside; sum = superblock bytes_used.
    let mut total_used = 0;
    for (k, d) in ext.iter().filter(|(k, _)| k.ty == BLOCK_GROUP_ITEM_KEY) {
        let c = r.chunks.iter().find(|c| c.0 == k.objectid).expect("bg without chunk");
        assert_eq!(c.1, k.offset);
        let inside = w.blocks.iter().filter(|b| b.0 >= k.objectid && b.0 < k.objectid + k.offset).count() as u64;
        assert_eq!(get64(d, 0), inside * ns);
        total_used += get64(d, 0);
    }
    assert_eq!(get64(sb, SB_BYTES_USED), total_used);
    // Free space tree: per block group, info count matches and free + used = length.
    let fst = &w.trees[&FREE_SPACE_TREE_OBJECTID];
    for (k, d) in ext.iter().filter(|(k, _)| k.ty == BLOCK_GROUP_ITEM_KEY) {
        let info = fst.iter().find(|(fk, _)| fk.ty == FREE_SPACE_INFO_KEY && fk.objectid == k.objectid).expect("fs info");
        assert_eq!(info.0.offset, k.offset);
        let exts: Vec<_> = fst.iter().filter(|(fk, _)| fk.ty == FREE_SPACE_EXTENT_KEY && fk.objectid >= k.objectid && fk.objectid < k.objectid + k.offset).collect();
        assert_eq!(get32(&info.1, 0) as usize, exts.len());
        let free: u64 = exts.iter().map(|e| e.0.offset).sum();
        assert_eq!(free + get64(d, 0), k.offset);
        for e in exts {
            for b in &w.blocks {
                assert!(b.0 + ns <= e.0.objectid || b.0 >= e.0.objectid + e.0.offset, "free extent overlaps a tree block");
            }
        }
    }
    // Device extents mirror the chunk stripes; dev item bytes_used is their sum.
    let dev = &w.trees[&DEV_TREE_OBJECTID];
    let mut stripes: Vec<(u64, u64, u64)> = Vec::new();
    for c in &r.chunks {
        for s in &c.2 {
            stripes.push((*s, c.0, c.1));
        }
    }
    stripes.sort();
    let dexts: Vec<(u64, u64, u64)> = dev.iter().filter(|(k, _)| k.ty == DEV_EXTENT_KEY).map(|(k, d)| (k.offset, get64(d, 16), get64(d, 24))).collect();
    assert_eq!(dexts, stripes);
    let mut end = MB;
    for (p, _, len) in &dexts {
        assert!(*p >= end, "dev extents overlap or hit the first MiB");
        end = p + len;
    }
    assert!(end <= get64(sb, SB_TOTAL_BYTES));
    let dev_used: u64 = dexts.iter().map(|d| d.2).sum();
    assert_eq!(get64(sb, SB_DEV_ITEM + 16), dev_used);
    // FS tree root dir and uuid tree entry.
    let fs = &w.trees[&FS_TREE_OBJECTID];
    assert_eq!(fs[0].0, Key::new(256, INODE_ITEM_KEY, 0));
    assert_eq!(get32(&fs[0].1, 52), 0o040755);
    let root = &w.trees[&ROOT_TREE_OBJECTID];
    let fs_ri = &root.iter().find(|(k, _)| *k == Key::new(FS_TREE_OBJECTID, ROOT_ITEM_KEY, 0)).unwrap().1;
    let uuid = &fs_ri[160 + 87..160 + 103];
    let ut = &w.trees[&UUID_TREE_OBJECTID];
    assert_eq!(ut.len(), 1);
    assert_eq!(ut[0].0, Key::new(get64(uuid, 0), UUID_KEY_SUBVOL, get64(uuid, 8)));
    assert_eq!(get64(&ut[0].1, 0), FS_TREE_OBJECTID);
    // Mirrors: identical except bytenr and csum.
    for off in SUPER_OFFSETS.iter().skip(1) {
        if off + 4096 <= size {
            let m = &r.img[*off as usize..*off as usize + 4096];
            assert_eq!(get64(m, SB_MAGIC), MAGIC);
            assert_eq!(get64(m, SB_BYTENR), *off);
            assert_eq!(get32(m, 0), crc32c(&m[32..]));
            assert_eq!(&m[SB_BYTENR + 8..], &sb[SB_BYTENR + 8..]);
        }
    }
    w
}

#[test]
fn name_hash_matches_mkfs() {
    // DIR_ITEM offset of "default" in a real mkfs.btrfs image.
    assert_eq!(name_hash(b"default"), 2378154706);
}

#[test]
fn minimum_size_default_nodesize() {
    let img = format_mem(MIN_DEV_SIZE, &opts("lbl", None));
    let w = check_image(img, DEFAULT_NODESIZE);
    assert_eq!(&w.sb[SB_LABEL..SB_LABEL + 4], b"lbl\0");
}

#[test]
fn all_nodesizes() {
    for ns in [4096u32, 8192, 16384, 32768, 65536] {
        let img = format_mem(160 * MB + 12345, &opts("n", Some(ns)));
        let w = check_image(img, ns);
        let incompat = get64(&w.sb, SB_INCOMPAT);
        assert_eq!(incompat & INCOMPAT_BIG_METADATA != 0, ns > 4096);
        assert_eq!(incompat & !INCOMPAT_BIG_METADATA, 0x341);
        assert_eq!(get64(&w.sb, SB_COMPAT_RO), 3);
    }
}

#[test]
fn multi_level_trees() {
    // 4 KiB nodes: 3995 bytes per leaf, 121 pointers per node.
    assert_eq!(tree_shape(&[], 4096).unwrap(), vec![1]);
    assert_eq!(tree_shape(&vec![33; 68], 4096).unwrap(), vec![1]);
    assert_eq!(tree_shape(&vec![33; 69], 4096).unwrap(), vec![2, 1]);
    assert_eq!(tree_shape(&vec![33; 68 * 121 + 1], 4096).unwrap(), vec![122, 2, 1]);
    assert!(tree_shape(&[4000], 4096).is_err());
    let p = Params::new(1 << 30, &opts("", Some(4096))).unwrap();
    let items: Vec<Item> = (0..10_000u64).map(|i| (Key::new(i, METADATA_ITEM_KEY, 0), metadata_item(5))).collect();
    let spec = TreeSpec::new(5, items);
    let shape = spec.shape(4096).unwrap();
    assert_eq!(shape.len(), 3);
    let addrs: Vec<u64> = (0..total_blocks(&shape) as u64).map(|i| (1 << 20) + i * 4096).collect();
    let t = build_tree(&p, &spec, &addrs).unwrap();
    assert_eq!(t.level, 2);
    let blocks: BTreeMap<u64, Vec<u8>> = t.blocks.into_iter().collect();
    fn walk(blocks: &BTreeMap<u64, Vec<u8>>, a: u64, out: &mut Vec<Key>) {
        let b = &blocks[&a];
        assert_eq!(get32(b, 0), crc32c(&b[32..]));
        let n = get32(b, 96) as usize;
        if b[100] == 0 {
            for i in 0..n {
                out.push(Key::read(&b[HEADER_SIZE + i * ITEM_SIZE..]));
            }
        } else {
            for i in 0..n {
                let o = HEADER_SIZE + i * KEY_PTR_SIZE;
                let first = out.len();
                walk(blocks, get64(b, o + 17), out);
                assert_eq!(out[first], Key::read(&b[o..]));
            }
        }
    }
    let mut keys = Vec::new();
    walk(&blocks, t.root, &mut keys);
    assert_eq!(keys.len(), 10_000);
    assert!(keys.windows(2).all(|w| w[0] < w[1]));
    // 4 KiB nodes end to end.
    check_image(format_mem(200 * MB, &opts("", Some(4096))), 4096);
}

#[test]
fn chunk_sizes_follow_mkfs() {
    let c = plan_chunks(1 << 30).unwrap();
    assert_eq!(c[0].length, 8 * MB);
    assert_eq!(c[1].flags, BG_SYSTEM | BG_DUP);
    assert_eq!(c[2].length, 53673984); // what mkfs.btrfs 6.14 makes on 1 GiB
    assert_eq!(plan_chunks(MIN_DEV_SIZE).unwrap()[2].length, 32 * MB);
    assert_eq!(plan_chunks(50 << 30).unwrap()[2].length, 256 * MB);
}

#[test]
fn uuid_and_long_label() {
    let u = [7u8; 16];
    let label: String = "è".repeat(200); // 400 bytes: cut at a char boundary <= 255
    let img = format_mem(MIN_DEV_SIZE, &MkfsOptions { label, cluster_size: None, uuid: Some(u) });
    let w = check_image(img.clone(), DEFAULT_NODESIZE);
    assert_eq!(&w.sb[SB_FSID..SB_FSID + 16], &u);
    assert_eq!(&w.sb[SB_DEV_ITEM + 82..SB_DEV_ITEM + 98], &u);
    let l = &w.sb[SB_LABEL..SB_LABEL + 256];
    let len = l.iter().position(|b| *b == 0).unwrap();
    assert_eq!(len, 254);
    assert!(std::str::from_utf8(&l[..len]).is_ok());
}

#[test]
fn refuses_too_small_and_bad_nodesize() {
    let dev = Arc::new(MemDev::new((MIN_DEV_SIZE - 4096) as usize));
    assert!(matches!(format(dev, &opts("", None)), Err(FsError::Invalid(_))));
    for bad in [0u32, 2048, 4095, 12288, 131072] {
        let r = build(1 << 30, &opts("", Some(bad)));
        assert!(matches!(r, Err(FsError::Invalid(_))), "nodesize {bad}");
    }
    assert!(matches!(build(0, &opts("", None)), Err(FsError::Invalid(_))));
    assert!(matches!(build(u64::MAX, &opts("", None)), Ok(_)));
}

#[test]
fn wipes_old_signatures() {
    let size = MIN_DEV_SIZE as usize + 3 * 4096 + 17;
    let mut v = vec![0xAAu8; size];
    v[..512].fill(0x55);
    let dev = Arc::new(MemDev::from_vec(v));
    format(dev.clone(), &opts("", None)).unwrap();
    let img = dev.snapshot();
    assert!(img[..65536].iter().all(|b| *b == 0));
    assert!(img[size - 4096..].iter().all(|b| *b == 0));
    check_image(img, DEFAULT_NODESIZE);
}
