//! Geometry against `mkfs.xfs` 6.13 (values from `mkfs.xfs -N` / `xfs_db` on the same sizes)
//! and structural checks of the written image with a small independent reader.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};

use super::*;

const GIB: u64 = 1 << 30;

fn opts(label: &str, bs: Option<u32>) -> MkfsOptions {
    MkfsOptions { label: label.into(), cluster_size: bs, uuid: None }
}

/// Sparse in-memory device: only pages with non-zero bytes are kept, so filesystems of
/// any size can be formatted in tests.
struct SparseDev {
    size: u64,
    pages: Mutex<BTreeMap<u64, Vec<u8>>>,
}

const PAGE: u64 = 4096;

impl SparseDev {
    fn new(size: u64) -> Self {
        SparseDev { size, pages: Mutex::new(BTreeMap::new()) }
    }
}

impl BlockDev for SparseDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        assert!(offset + buf.len() as u64 <= self.size);
        let pages = self.pages.lock().unwrap();
        for (i, b) in buf.iter_mut().enumerate() {
            let o = offset + i as u64;
            *b = pages.get(&(o / PAGE)).map(|p| p[(o % PAGE) as usize]).unwrap_or(0);
        }
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.size {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "beyond end"));
        }
        let mut pages = self.pages.lock().unwrap();
        let mut i = 0usize;
        while i < buf.len() {
            let o = offset + i as u64;
            let (pn, po) = (o / PAGE, (o % PAGE) as usize);
            let n = (PAGE as usize - po).min(buf.len() - i);
            let src = &buf[i..i + n];
            if src.iter().all(|b| *b == 0) {
                if let Some(p) = pages.get_mut(&pn) {
                    p[po..po + n].fill(0);
                    if p.iter().all(|b| *b == 0) {
                        pages.remove(&pn);
                    }
                }
            } else {
                let p = pages.entry(pn).or_insert_with(|| vec![0u8; PAGE as usize]);
                p[po..po + n].copy_from_slice(src);
            }
            i += n;
        }
        Ok(())
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    fn read_only(&self) -> bool {
        false
    }
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn be64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_be_bytes(a)
}

/// (size, block, sector) -> (agcount, agsize, dblocks, logblocks, logstart, rootino, icount,
/// fdblocks) as mkfs.xfs 6.13 makes them (fdblocks / rootino only where they were dumped).
#[test]
fn geometry_matches_mkfs_xfs() {
    #[allow(clippy::type_complexity)]
    let cases: &[(u64, u32, u32, u64, u64, u64, u64)] = &[
        (300 << 20, 4096, 512, 4, 19200, 76800, 16384),
        (300 << 20, 1024, 512, 4, 76800, 307200, 65536),
        (400 << 20, 4096, 512, 4, 25600, 102400, 16384),
        (511 << 20, 4096, 512, 4, 32704, 130816, 16384),
        (512 << 20, 4096, 512, 4, 32768, 131072, 16384),
        (513 << 20, 4096, 512, 4, 32832, 131328, 16384),
        (2 * GIB, 4096, 512, 4, 131072, 524288, 16384),
        (16 * GIB, 4096, 512, 4, 1048576, 4194304, 16384),
        (50 * GIB, 4096, 512, 4, 3276800, 13107200, 16384),
        (600 * GIB, 4096, 512, 4, 39321600, 157286400, 76800),
        (5 << 40, 4096, 512, 5, 268435455, 1342177275, 521728),
        (GIB, 1024, 512, 4, 262144, 1048576, 65536),
        (GIB, 2048, 512, 4, 131072, 524288, 32768),
        (GIB, 8192, 512, 4, 32768, 131072, 8192),
        (GIB, 16384, 512, 4, 16384, 65536, 4096),
        (GIB, 32768, 512, 4, 8192, 32768, 2605),
        (GIB, 65536, 512, 4, 4096, 16384, 2180),
        (GIB, 4096, 4096, 4, 65536, 262144, 16384),
    ];
    for &(size, bs, ss, agcount, agsize, dblocks, logblocks) in cases {
        let g = Geometry::new(size, bs, ss).unwrap_or_else(|e| panic!("{size} {bs}: {e}"));
        assert_eq!((g.agcount, g.agsize, g.dblocks, g.logblocks), (agcount, agsize, dblocks, logblocks), "size {size} block {bs} sector {ss}");
    }
    assert_eq!(Geometry::new(5 << 40, 4096, 512).unwrap().imaxpct, 5);
    // mkfs.xfs: "max log size 1188 smaller than min log size 1984, filesystem is too small".
    assert!(Geometry::new(300 << 20, 65536, 512).is_err());
    assert!(Geometry::new(300 << 20, 32768, 512).is_ok());
    assert_eq!(Geometry::new(300 << 20, 32768, 512).unwrap().logblocks, 2213);
}

#[test]
fn layout_matches_mkfs_xfs() {
    // (block, sector) -> (logstart, rootino, icount, inoalignmt, spino_align, dirblklog, fdblocks, agfl)
    let cases: &[(u32, u32, u64, u64, u64, u64, u64, u8, u64, u64)] = &[
        (4096, 512, 131079, 128, 64, 8, 4, 0, 245724, 6),
        (1024, 512, 524296, 64, 64, 32, 16, 2, 982976, 6),
        (32768, 512, 16391, 832, 64, 1, 0, 0, 30134, 6),
        (65536, 512, 8199, 1152, 128, 0, 0, 0, 14175, 2),
        (4096, 4096, 131082, 128, 64, 8, 4, 0, 245712, 6),
    ];
    for &(bs, ss, logstart, rootino, icount, inoalign, spino, dirblklog, fdblocks, agfl) in cases {
        let img = build(GIB, &opts("lbl", Some(bs)), ss).unwrap();
        let g = &img.params.geo;
        assert_eq!(g.logstart(), logstart, "block {bs} sector {ss}");
        assert_eq!(g.rootino(), rootino);
        assert_eq!(g.chunk_inodes, icount);
        assert_eq!((g.inoalignmt, g.spino_align, g.dirblklog), (inoalign, spino, dirblklog));
        assert_eq!(g.agfl_count, agfl);
        let sb = &img.primary();
        assert_eq!(be64(sb, SB_FDBLOCKS), fdblocks, "fdblocks, block {bs} sector {ss}");
        assert_eq!(be64(sb, SB_ICOUNT), icount);
        assert_eq!(be16(sb, SB_VERSIONNUM), if ss == 512 { 0xb4a5 } else { 0xbca5 });
        assert_eq!(be32(sb, SB_FEATURES2), 0x18a);
        assert_eq!(be32(sb, SB_RO_COMPAT), 0xf);
        assert_eq!(be32(sb, SB_INCOMPAT), 0x2b);
    }
    // Log stripe unit / first record (mkfs.xfs: logsunit = max(sector, block) on 4 KiB
    // sectors; records > 32 KiB need extended headers).
    for (bs, ss, sunit, h_len, unmount_bb) in [(4096u32, 512u32, 1u64, 512u32, 1usize), (4096, 4096, 4096, 3584, 1), (16384, 4096, 16384, 15872, 1), (65536, 4096, 65536, 64512, 2)] {
        let img = build(5 * GIB, &opts("", Some(bs)), ss).unwrap();
        assert_eq!(img.params.geo.logsunit(), sunit);
        assert_eq!(be32(&img.primary(), 196) as u64, sunit);
        let rec = img.log().2;
        assert_eq!(be32(&rec, 12), h_len, "block {bs} sector {ss}");
        assert_eq!(rec[unmount_bb * 512 + 8], 0xAA, "unmount record position");
    }
    // AG sizes that aren't powers of two: logstart is segmented (agno << agblklog | agbno).
    // (size, block, sector) -> (logstart, agblocks, dblocks, fdblocks, rootino, log daddr)
    let odd: &[(u64, u32, u32, u64, u64, u64, u64, u64, u64)] = &[
        (300 << 20, 4096, 512, 65543, 19200, 76800, 60380, 128, 307256),
        (734013440, 2048, 512, 262151, 89602, 358405, 325593, 64, 716844),
        (1073754169, 1024, 512, 1048584, 262147, 1048588, 982988, 64, 1048604),
        (2147680356, 65536, 512, 32775, 8193, 32771, 30172, 1664, 2098304),
        (1610612736, 4096, 4096, 262154, 98304, 393216, 376784, 128, 1572944),
    ];
    for &(size, bs, ss, logstart, agblocks, dblocks, fdblocks, rootino, log_daddr) in odd {
        let img = build(size, &opts("", Some(bs)), ss).unwrap();
        let g = &img.params.geo;
        assert_eq!((g.logstart(), g.agsize, g.dblocks, img.fdblocks, g.rootino()), (logstart, agblocks, dblocks, fdblocks, rootino), "size {size} block {bs}");
        assert_eq!(img.log().0 / 512, log_daddr);
    }
}

/// Format on a sparse device and check every structure.
fn check(size: u64, bs: u32, ss: u32) {
    let dev = Arc::new(SparseDev::new(size));
    format_with_sector(dev.clone(), &opts("check", Some(bs)), ss).unwrap();
    let rd = |off: u64, len: u64| dev.read_vec(off, len as usize).unwrap();
    let sb = rd(0, ss as u64);
    assert_eq!(be32(&sb, 0), SB_MAGIC);
    assert!(crc_ok(&sb, SB_CRC));
    let g = Geometry::new(size, bs, ss).unwrap();
    assert_eq!(be32(&sb, SB_AGCOUNT) as u64, g.agcount);
    let b = g.blocksize;
    let s = g.sectsize;
    let mut fd = 0u64;
    let mut inodes = 0u64;
    for agno in 0..g.agcount {
        let base = g.byte(agno, 0);
        let len = g.ag_len(agno);
        let ssb = rd(base, s);
        assert_eq!(be32(&ssb, 0), SB_MAGIC);
        assert!(crc_ok(&ssb, SB_CRC));
        assert_eq!(be64(&ssb, SB_DBLOCKS), g.dblocks);
        assert_eq!(&ssb[SB_UUID..SB_UUID + 16], &sb[SB_UUID..SB_UUID + 16]);
        let agf = rd(base + s, s);
        assert_eq!(be32(&agf, 0), AGF_MAGIC);
        assert!(crc_ok(&agf, 216));
        assert_eq!(be32(&agf, 8) as u64, agno);
        assert_eq!(be32(&agf, 12) as u64, len);
        let agi = rd(base + 2 * s, s);
        assert_eq!(be32(&agi, 0), AGI_MAGIC);
        assert!(crc_ok(&agi, 312));
        let agfl = rd(base + 3 * s, s);
        assert_eq!(be32(&agfl, 0), AGFL_MAGIC);
        assert!(crc_ok(&agfl, 32));
        let (flfirst, fllast, flcount) = (be32(&agf, 40), be32(&agf, 44), be32(&agf, 48));
        assert_eq!(fllast - flfirst + 1, flcount);
        let agfl_blocks: Vec<u64> = (flfirst..=fllast).map(|i| be32(&agfl, 36 + 4 * i as usize) as u64).collect();
        // Btree roots.
        let root = |off: usize, magic: u32| {
            let bno = be32(&agf, off) as u64;
            let blk = rd(g.byte(agno, bno), b);
            assert_eq!(be32(&blk, 0), magic);
            assert!(crc_ok(&blk, 52));
            assert_eq!(be64(&blk, 16), g.daddr(agno, bno));
            assert_eq!(be32(&blk, 48) as u64, agno);
            blk
        };
        let bno = root(16, BNO_MAGIC);
        let cnt = root(20, CNT_MAGIC);
        let rmap = root(24, RMAP_MAGIC);
        let refc = root(88, REFC_MAGIC);
        assert_eq!(be16(&refc, 6), 0);
        let ino_root = be32(&agi, 20) as u64;
        let ino = rd(g.byte(agno, ino_root), b);
        assert_eq!(be32(&ino, 0), INO_MAGIC);
        assert!(crc_ok(&ino, 52));
        let fino = rd(g.byte(agno, be32(&agi, 328) as u64), b);
        assert_eq!(be32(&fino, 0), FINO_MAGIC);
        assert!(crc_ok(&fino, 52));
        // Free extents: same set in both trees, sorted the two ways.
        let recs = |blk: &[u8]| -> Vec<(u64, u64)> {
            (0..be16(blk, 6) as usize).map(|i| (be32(blk, BTREE_SHDR_LEN + 8 * i) as u64, be32(blk, BTREE_SHDR_LEN + 8 * i + 4) as u64)).collect()
        };
        let free = recs(&bno);
        let mut free_c = recs(&cnt);
        assert!(free.windows(2).all(|w| w[0].0 + w[0].1 < w[1].0), "bnobt sorted, not adjacent");
        assert!(free_c.windows(2).all(|w| (w[0].1, w[0].0) < (w[1].1, w[1].0)));
        free_c.sort();
        assert_eq!(free, free_c);
        let freeblks: u64 = free.iter().map(|e| e.1).sum();
        assert_eq!(be32(&agf, 52) as u64, freeblks);
        assert_eq!(be32(&agf, 56) as u64, free.iter().map(|e| e.1).max().unwrap_or(0));
        fd += freeblks + flcount as u64;
        // Reverse mappings + free space cover the AG exactly once.
        let mut owner = vec![0u8; len as usize];
        for i in 0..be16(&rmap, 6) as usize {
            let o = BTREE_SHDR_LEN + 24 * i;
            let (st, l) = (be32(&rmap, o) as u64, be32(&rmap, o + 4) as u64);
            for x in st..st + l {
                assert_eq!(owner[x as usize], 0, "block {x} mapped twice");
                owner[x as usize] = 1;
            }
        }
        for (st, l) in &free {
            for x in *st..st + l {
                assert_eq!(owner[x as usize], 0, "free block {x} also mapped");
                owner[x as usize] = 2;
            }
        }
        assert!(owner.iter().all(|o| *o != 0), "AG {agno}: unaccounted blocks");
        for a in &agfl_blocks {
            assert_eq!(owner[*a as usize], 1);
        }
        // Inodes.
        let n = be16(&ino, 6) as usize;
        let mut count = 0;
        let mut freecount = 0;
        for i in 0..n {
            let o = BTREE_SHDR_LEN + 16 * i;
            let start = be32(&ino, o) as u64;
            let mask = be64(&ino, o + 8);
            count += ino[o + 6] as u64;
            freecount += ino[o + 7] as u64;
            assert_eq!(ino[o + 7] as u32, mask.count_ones());
            for k in 0..64u64 {
                let agino = start + k;
                let off = g.byte(agno, agino >> g.inopblog) + (agino & (g.inopblock - 1)) * INODE_SIZE as u64;
                let d = rd(off, INODE_SIZE as u64);
                assert_eq!(be16(&d, 0), DINODE_MAGIC);
                assert!(crc_ok(&d, 100), "inode {agino} crc");
                assert_eq!(be64(&d, 152), agino);
                assert_eq!(be16(&d, 2) == 0, mask & (1 << k) != 0, "inode {agino} mode vs free mask");
            }
        }
        assert_eq!(be32(&agi, 16) as u64, count);
        assert_eq!(be32(&agi, 28) as u64, freecount);
        inodes += count;
    }
    assert_eq!(be64(&sb, SB_FDBLOCKS), fd);
    assert_eq!(be64(&sb, SB_ICOUNT), inodes);
    // Root directory.
    let r = be64(&sb, SB_ROOTINO);
    let d = rd((r >> g.inopblog) * b + (r & (g.inopblock - 1)) * INODE_SIZE as u64, INODE_SIZE as u64);
    assert_eq!(be16(&d, 2), 0o040755);
    assert_eq!(d[5], 1);
    assert_eq!(be64(&d, 56), 6);
    assert_eq!(be32(&d, DINODE_CORE_V3 + 2) as u64, r);
    // Log: the first record, then zeros.
    let lfsb = be64(&sb, SB_LOGSTART);
    let ls = g.byte(lfsb >> g.agblklog, lfsb & ((1 << g.agblklog) - 1));
    let log = rd(ls, 4 * s);
    assert_eq!(be32(&log, 0), 0xFEED_BABE);
    assert_eq!(&log[304..320], &sb[SB_UUID..SB_UUID + 16]);
    let logend = ls + be32(&sb, SB_LOGBLOCKS) as u64 * b;
    assert!(rd(logend - 4096, 4096).iter().all(|x| *x == 0));
}

#[test]
fn structures_4k() {
    check(GIB, 4096, 512);
}

#[test]
fn structures_minimum_and_odd() {
    check(MIN_DEV_SIZE, 4096, 512);
    check(GIB + 12345, 1024, 512);
    check(700 * (1 << 20) + 2048 * 5, 2048, 512);
}

#[test]
fn structures_big_blocks_and_4k_sectors() {
    check(2 * GIB + 65536 * 3 + 100, 65536, 512);
    check(GIB, 32768, 4096);
    check(3 * GIB / 2, 4096, 4096);
    check(GIB, 8192, 512);
    check(GIB, 16384, 4096);
}

#[test]
fn structures_large() {
    check(50 * GIB, 4096, 512);
    check(5 << 40, 4096, 512);
}

#[test]
fn label_and_uuid() {
    let u = [9u8; 16];
    let img = build(GIB, &MkfsOptions { label: "abcdefghijklmnop".into(), cluster_size: None, uuid: Some(u) }, 512).unwrap();
    assert_eq!(&img.primary()[SB_FNAME..SB_FNAME + 12], b"abcdefghijkl");
    assert_eq!(&img.primary()[SB_UUID..SB_UUID + 16], &u);
    // Never split a UTF-8 character.
    let img = build(GIB, &opts("ààààààà", None), 512).unwrap();
    assert_eq!(&img.primary()[SB_FNAME..SB_FNAME + 12], "àààààà".as_bytes());
    let img = build(GIB, &opts("12345678901é", None), 512).unwrap();
    assert_eq!(&img.primary()[SB_FNAME..SB_FNAME + 12], b"12345678901\0");
}

#[test]
fn refusals() {
    assert!(matches!(build(MIN_DEV_SIZE - 1, &opts("", None), 512), Err(FsError::Invalid(_))));
    assert!(matches!(build(0, &opts("", None), 512), Err(FsError::Invalid(_))));
    for bad in [0u32, 512, 3000, 131072] {
        assert!(matches!(build(GIB, &opts("", Some(bad)), 512), Err(FsError::Invalid(_))), "block {bad}");
    }
    assert!(matches!(build(GIB, &opts("", Some(1024)), 4096), Err(FsError::Invalid(_))));
    assert!(matches!(build(GIB, &opts("", None), 2048), Err(FsError::Invalid(_))));
    // Huge sizes don't overflow.
    assert!(build(u64::MAX, &opts("", None), 512).is_ok());
    assert!(build(u64::MAX, &opts("", Some(1024)), 512).is_ok());
}

#[test]
fn wipes_old_signatures() {
    let size = MIN_DEV_SIZE + 3 * 4096 + 17;
    let dev = Arc::new(SparseDev::new(size));
    dev.write_at(65536, &[0xAA; 4096]).unwrap();
    dev.write_at(size - 4096, &[0xAA; 4096]).unwrap();
    format(dev.clone(), &opts("", None)).unwrap();
    assert!(dev.read_vec(65536, 4096).unwrap().iter().all(|b| *b == 0) || Geometry::new(size, 4096, 512).unwrap().chunk_agbno * 4096 <= 65536);
    assert!(dev.read_vec(size - 4096, 4096).unwrap().iter().all(|b| *b == 0));
}
