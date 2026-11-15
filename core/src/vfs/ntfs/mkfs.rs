//! Minimal `mkntfs`: NTFS 3.1, 4096-byte clusters, 1024-byte MFT records, 4096-byte index
//! blocks, system files 0..=15 plus $Extend/{$Quota,$ObjId,$Reparse}. The structure mirrors
//! what ntfs-3g's mkntfs writes (security descriptors, $Secure streams/indexes, $UpCase and
//! its $Info stream, $AttrDef) so that ntfs-3g, ntfsfix and Windows accept the result.

use std::cmp::Ordering;
use std::sync::Arc;

use super::layout::*;
use super::upcase;
use crate::block::BlockDev;
use crate::vfs::{FsError, FsResult};

const CS: u64 = 4096;
const RS: usize = 1024;
const IBS: usize = 4096;
const MFT_RECORDS: u64 = 64;

// $Secure contents as produced by mkntfs (two descriptors, ids 0x100 and 0x101).
const SDS_ENTRIES: &str = "f01203f80001000000000000000000007c0000000100048048000000580000000000000014000000020034000200000000001400890012000101000000000005120000000000180089001200010200000000000520000000200200000102000000000005200000002002000001020000000000052000000020020000000000005124b3000101000080000000000000007c00000001000480480000005800000000000000140000000200340002000000000014009f011200010100000000000512000000000018009f01120001020000000000052000000020020000010200000000000520000000200200000102000000000005200000002002000000000000";
const SDH_ROOT: &str = "0000000012000000001000000100000010000000800000008000000000000000180014000000000030000800000000005124b300010100005124b3000101000080000000000000007c0000004900490018001400000000003000080000000000f01203f800010000f01203f80001000000000000000000007c0000004900490000000000000000001000000002000000";
const SII_ROOT: &str = "00000000100000000010000001000000100000007000000070000000000000001400140000000000280004000000000000010000f01203f80001000000000000000000007c00000014001400000000002800040000000000010100005124b3000101000080000000000000007c00000000000000000000001000000002000000";
const QUOTA_O_ROOT: &str = "00000000110000000010000001000000100000004800000048000000000000002000040000000000280010000000000001020000000000052000000020020000000100002000000000000000000000001000000002000000";
const QUOTA_Q_ROOT: &str = "0000000010000000001000000100000010000000c0000000c00000000000000014003000000000004800040000000000010000000200000001000000000000000000000080428f2ce274dd01ffffffffffffffffffffffffffffffff00000000000000000000000014004000000000005800040000000000000100000200000001000000000000000000000080428f2ce274dd01ffffffffffffffffffffffffffffffff0000000000000000010200000000000520000000200200000000000000000000000000001000000002000000";
const OBJID_O_ROOT: &str = "000000001300000000100000010000001000000020000000200000000000000000000000000000001000000002000000";
const REPARSE_R_ROOT: &str = "000000001300000000100000010000001000000020000000200000000000000000000000000000001000000002000000";
const BOOT_CODE: &str = "0e1fbe717cac22c0740b56b40ebb0700cd105eebf032e4cd16cd19ebfe54686973206973206e6f74206120626f6f7461626c65206469736b2e20506c6561736520696e73657274206120626f6f7461626c6520666c6f70707920616e640d0a707265737320616e79206b657920746f2074727920616761696e202e2e2e200d0a";

/// (name, type, flags, min, max) — the standard NTFS 3.1 attribute definitions.
const ATTRDEF: &[(&str, u32, u32, u64, u64)] = &[
    ("$STANDARD_INFORMATION", 0x10, 0x40, 48, 72),
    ("$ATTRIBUTE_LIST", 0x20, 0x80, 0, u64::MAX),
    ("$FILE_NAME", 0x30, 0x42, 68, 578),
    ("$OBJECT_ID", 0x40, 0x40, 0, 256),
    ("$SECURITY_DESCRIPTOR", 0x50, 0x80, 0, u64::MAX),
    ("$VOLUME_NAME", 0x60, 0x40, 2, 256),
    ("$VOLUME_INFORMATION", 0x70, 0x40, 12, 12),
    ("$DATA", 0x80, 0x00, 0, u64::MAX),
    ("$INDEX_ROOT", 0x90, 0x40, 0, u64::MAX),
    ("$INDEX_ALLOCATION", 0xA0, 0x80, 0, u64::MAX),
    ("$BITMAP", 0xB0, 0x80, 0, u64::MAX),
    ("$REPARSE_POINT", 0xC0, 0x80, 0, 16384),
    ("$EA_INFORMATION", 0xD0, 0x40, 8, 8),
    ("$EA", 0xE0, 0x00, 0, 65536),
    ("$LOGGED_UTILITY_STREAM", 0x100, 0x80, 0, 65536),
];

fn logfile_size(vol: u64) -> u64 {
    const K: u64 = 1024;
    const M: u64 = 1024 * K;
    match vol {
        v if v < 2 * M => 256 * K,
        v if v < 32 * M => 512 * K,
        v if v < 256 * M => 2 * M,
        v if v < 512 * M => 4 * M,
        v if v < 2048 * M => 8 * M,
        v if v < 4096 * M => 16 * M,
        v if v < 8192 * M => 32 * M,
        _ => 64 * M,
    }
}

struct Bitmap {
    b: Vec<u8>,
    clusters: u64,
}

impl Bitmap {
    fn used(&self, c: u64) -> bool {
        self.b[(c / 8) as usize] & (1 << (c % 8)) != 0
    }
    fn mark(&mut self, lcn: u64, len: u64) {
        for c in lcn..lcn + len {
            self.b[(c / 8) as usize] |= 1 << (c % 8);
        }
    }
    /// First free run of `len` clusters at or after `from` (wrapping once).
    fn place(&mut self, len: u64, from: u64) -> FsResult<u64> {
        for (a, b) in [(from.min(self.clusters), self.clusters), (0, from.min(self.clusters))] {
            let mut c = a;
            while c + len <= b {
                match (c..c + len).find(|&x| self.used(x)) {
                    Some(u) => c = u + 1,
                    None => {
                        self.mark(c, len);
                        return Ok(c);
                    }
                }
            }
        }
        Err(FsError::NoSpace)
    }
}

fn run(lcn: u64, len: u64) -> Vec<Run> {
    vec![Run { vcn: 0, lcn: Some(lcn), len }]
}

fn si48(t: u64, attrs: u32) -> Vec<u8> {
    let mut v = vec![];
    StdInfo { crtime: t, mtime: t, ctime: t, atime: t, attrs }.store(&mut v);
    v
}
fn si72(t: u64, attrs: u32, sec: u32) -> Vec<u8> {
    StdInfo { crtime: t, mtime: t, ctime: t, atime: t, attrs }.new_value(sec)
}

fn fname(t: u64, parent: u64, name: &str, alloc: u64, size: u64, flags: u32) -> Vec<u8> {
    FileName { parent, crtime: t, mtime: t, ctime: t, atime: t, alloc, size, flags, reparse: 0, ns: NS_WIN32_DOS, name: to_utf16(name) }.to_bytes()
}

fn res(ty: u32, name: &str, value: Vec<u8>, rflags: u8) -> Attribute {
    let mut a = Attribute::resident(ty, &to_utf16(name), value);
    a.body = Body::Res { value: match a.body { Body::Res { value, .. } => value, _ => unreachable!() }, rflags };
    a
}

fn nonres(ty: u32, name: &str, runs: Vec<Run>, size: u64) -> Attribute {
    Attribute::nonresident(ty, &to_utf16(name), runs, size, CS)
}

fn cmp_names(up: &[u16], a: &[u16], b: &[u16]) -> Ordering {
    let u = |c: u16| up[c as usize];
    for (x, y) in a.iter().zip(b) {
        match u(*x).cmp(&u(*y)) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

pub fn format(dev: Arc<dyn BlockDev>, label: &str) -> FsResult<()> {
    if dev.read_only() {
        return Err(FsError::ReadOnly);
    }
    let sectors = dev.size() / 512;
    if sectors < 4096 {
        return Err(FsError::Invalid("device too small for NTFS (minimum 2 MiB)".into()));
    }
    let total_sectors = sectors - 1; // last sector holds the backup boot sector
    let clusters = total_sectors / (CS / 512);
    if clusters > u32::MAX as u64 {
        return Err(FsError::Invalid("volume too large for 4 KiB clusters".into()));
    }
    let mut label16 = to_utf16(label);
    label16.truncate(128);
    let now = ntfs_now();
    let up = upcase::default_upcase();

    // ---- cluster layout
    let bm_bytes = align8(clusters.div_ceil(8) as usize) as u64;
    let mut bm = Bitmap { b: vec![0u8; bm_bytes as usize], clusters };
    for c in clusters..bm_bytes * 8 {
        bm.b[(c / 8) as usize] |= 1 << (c % 8); // bits past the end of the volume are "in use"
    }
    let mft_cl = MFT_RECORDS * RS as u64 / CS;
    bm.mark(0, 2); // $Boot
    bm.mark(2, 1); // $MFT:$BITMAP
    let mft_lcn = 4;
    bm.mark(mft_lcn, mft_cl);
    let mirr_lcn = bm.place(1, clusters / 2)?;
    let log_bytes = logfile_size(clusters * CS);
    let log_cl = log_bytes / CS;
    let log_lcn = bm.place(log_cl, mirr_lcn + 1)?;
    let zone = (mft_lcn + mft_cl).max(mft_lcn + clusters / 8);
    let attrdef_lcn = bm.place(1, zone)?;
    let rootia_lcn = bm.place(1, zone)?;
    let bm_cl = bm_bytes.div_ceil(CS);
    let bitmap_lcn = bm.place(bm_cl, zone)?;
    let sds_cl = 0x41000 / CS;
    let sds_lcn = bm.place(sds_cl, zone)?;
    let upcase_lcn = bm.place(32, zone)?;

    // ---- records
    let mref = |no: u64, seq: u16| mk_ref(no, seq);
    let seq_of = |no: u64| -> u16 {
        match no {
            0 => 1,
            1..=23 => no as u16,
            _ => 1,
        }
    };
    let root_ref = mref(5, 5);
    let ext_ref = mref(11, 11);
    let hs = FILE_ATTR_HIDDEN | FILE_ATTR_SYSTEM;
    let sd_sys = build_sd(&sid_system(), &sid_admins(), &[(0, 0x0012_019F, sid_system()), (0, 0x0012_019F, sid_admins())]);
    let sd_root = build_sd(
        &sid_admins(),
        &sid_system(),
        &[
            (0x00, 0x001F_01FF, sid_admins()),
            (0x0B, 0x1000_0000, sid_admins()),
            (0x00, 0x001F_01FF, sid_system()),
            (0x0B, 0x1000_0000, sid_system()),
            (0x00, 0x0013_01BF, sid_auth_users()),
            (0x0B, 0xE001_0000, sid_auth_users()),
            (0x00, 0x0012_00A9, sid_users()),
            (0x0B, 0xA000_0000, sid_users()),
        ],
    );

    let mut recs: Vec<Option<MftRecord>> = (0..MFT_RECORDS).map(|_| None).collect();
    // (record, name, FILE_NAME value) of files listed in the root directory
    let mut root_entries: Vec<(u64, Vec<u8>)> = vec![];
    let mut ext_entries: Vec<(u64, Vec<u8>)> = vec![];

    let mut mk = |no: u64, flags: u16, attrs: Vec<Attribute>, list: Option<(bool, Vec<u8>)>| {
        let mut r = MftRecord::new(seq_of(no), MFT_IN_USE | flags);
        for a in attrs {
            r.add_attr(a);
        }
        r.link_count = r.attrs.iter().filter(|a| a.ty == AT_FILE_NAME).count() as u16;
        if let Some((in_root, key)) = list {
            if in_root {
                root_entries.push((no, key));
            } else {
                ext_entries.push((no, key));
            }
        }
        recs[no as usize] = Some(r);
    };

    let fn_mft = fname(now, root_ref, "$MFT", mft_cl * CS, mft_cl * CS, hs);
    mk(
        0,
        0,
        vec![
            res(AT_STANDARD_INFORMATION, "", si72(now, hs, 0x100), 0),
            res(AT_FILE_NAME, "", fn_mft.clone(), 1),
            nonres(AT_DATA, "", run(mft_lcn, mft_cl), mft_cl * CS),
            nonres(AT_BITMAP, "", run(2, 1), MFT_RECORDS / 8),
        ],
        Some((true, fn_mft)),
    );
    let f = fname(now, root_ref, "$MFTMirr", CS, CS, hs);
    mk(1, 0, vec![res(AT_STANDARD_INFORMATION, "", si72(now, hs, 0x100), 0), res(AT_FILE_NAME, "", f.clone(), 1), nonres(AT_DATA, "", run(mirr_lcn, 1), CS)], Some((true, f)));
    let f = fname(now, root_ref, "$LogFile", log_bytes, log_bytes, hs);
    mk(2, 0, vec![res(AT_STANDARD_INFORMATION, "", si72(now, hs, 0x100), 0), res(AT_FILE_NAME, "", f.clone(), 1), nonres(AT_DATA, "", run(log_lcn, log_cl), log_bytes)], Some((true, f)));
    let f = fname(now, root_ref, "$Volume", 0, 0, hs);
    let mut volinfo = vec![0u8; 12];
    volinfo[8] = 3;
    volinfo[9] = 1;
    mk(
        3,
        0,
        vec![
            res(AT_STANDARD_INFORMATION, "", si48(now, hs), 0),
            res(AT_FILE_NAME, "", f.clone(), 1),
            res(AT_SECURITY_DESCRIPTOR, "", sd_sys.clone(), 0),
            res(AT_VOLUME_NAME, "", utf16_bytes(&label16), 0),
            res(AT_VOLUME_INFORMATION, "", volinfo, 0),
            res(AT_DATA, "", vec![], 0),
        ],
        Some((true, f)),
    );
    let mut attrdef = vec![0u8; 2560];
    for (i, (name, ty, flags, min, max)) in ATTRDEF.iter().enumerate() {
        let e = &mut attrdef[i * 160..(i + 1) * 160];
        let nb = utf16_bytes(&to_utf16(name));
        e[..nb.len()].copy_from_slice(&nb);
        put32(e, 128, *ty);
        put32(e, 140, *flags);
        put64(e, 144, *min);
        put64(e, 152, *max);
    }
    let f = fname(now, root_ref, "$AttrDef", CS, 2560, hs);
    mk(
        4,
        0,
        vec![
            res(AT_STANDARD_INFORMATION, "", si48(now, hs), 0),
            res(AT_FILE_NAME, "", f.clone(), 1),
            res(AT_SECURITY_DESCRIPTOR, "", sd_sys.clone(), 0),
            nonres(AT_DATA, "", run(attrdef_lcn, 1), 2560),
        ],
        Some((true, f)),
    );
    let f = fname(now, root_ref, ".", 0, 0, hs | FILE_ATTR_DUP_DIR);
    let root_hdr = IndexRootHdr { ty: AT_FILE_NAME, collation: 1, block_size: IBS as u32, clusters_per_block: 1 };
    let mut bitmap_i30 = vec![0u8; 8];
    bitmap_i30[0] = 1;
    mk(
        5,
        MFT_IS_DIR,
        vec![
            res(AT_STANDARD_INFORMATION, "", si48(now, hs), 0),
            res(AT_FILE_NAME, "", f.clone(), 1),
            res(AT_SECURITY_DESCRIPTOR, "", sd_root, 0),
            res(AT_INDEX_ROOT, "$I30", index_root_value(&root_hdr, &Node { entries: vec![IEntry::end(Some(0))] }), 0),
            nonres(AT_INDEX_ALLOCATION, "$I30", run(rootia_lcn, 1), IBS as u64),
            res(AT_BITMAP, "$I30", bitmap_i30, 0),
        ],
        Some((true, f)),
    );
    let f = fname(now, root_ref, "$Bitmap", bm_cl * CS, bm_bytes, hs);
    mk(6, 0, vec![res(AT_STANDARD_INFORMATION, "", si72(now, hs, 0x100), 0), res(AT_FILE_NAME, "", f.clone(), 1), nonres(AT_DATA, "", run(bitmap_lcn, bm_cl), bm_bytes)], Some((true, f)));
    let f = fname(now, root_ref, "$Boot", 8192, 8192, hs);
    mk(
        7,
        0,
        vec![
            res(AT_STANDARD_INFORMATION, "", si48(now, hs), 0),
            res(AT_FILE_NAME, "", f.clone(), 1),
            res(AT_SECURITY_DESCRIPTOR, "", sd_sys.clone(), 0),
            nonres(AT_DATA, "", run(0, 2), 8192),
        ],
        Some((true, f)),
    );
    let f = fname(now, root_ref, "$BadClus", 0, 0, hs);
    let mut bad = nonres(AT_DATA, "$Bad", vec![Run { vcn: 0, lcn: None, len: clusters }], clusters * CS);
    if let Body::NonRes(n) = &mut bad.body {
        n.init = 0;
    }
    mk(8, 0, vec![res(AT_STANDARD_INFORMATION, "", si72(now, hs, 0x100), 0), res(AT_FILE_NAME, "", f.clone(), 1), res(AT_DATA, "", vec![], 0), bad], Some((true, f)));
    let vi = hs | FILE_ATTR_DUP_VIEW_INDEX;
    let f = fname(now, root_ref, "$Secure", 0, 0, vi);
    mk(
        9,
        0x0008,
        vec![
            res(AT_STANDARD_INFORMATION, "", si72(now, vi, 0x101), 0),
            res(AT_FILE_NAME, "", f.clone(), 1),
            nonres(AT_DATA, "$SDS", run(sds_lcn, sds_cl), 0x400FC),
            res(AT_INDEX_ROOT, "$SDH", hex(SDH_ROOT), 0),
            res(AT_INDEX_ROOT, "$SII", hex(SII_ROOT), 0),
        ],
        Some((true, f)),
    );
    let f = fname(now, root_ref, "$UpCase", 131072, 131072, hs);
    mk(
        10,
        0,
        vec![
            res(AT_STANDARD_INFORMATION, "", si72(now, hs, 0x100), 0),
            res(AT_FILE_NAME, "", f.clone(), 1),
            nonres(AT_DATA, "", run(upcase_lcn, 32), 131072),
            res(AT_DATA, "$Info", hex(upcase::UPCASE_INFO), 0),
        ],
        Some((true, f)),
    );
    for no in 12..16u64 {
        mk(no, 0, vec![res(AT_STANDARD_INFORMATION, "", si48(now, hs), 0), res(AT_SECURITY_DESCRIPTOR, "", sd_sys.clone(), 0), res(AT_DATA, "", vec![], 0)], None);
    }
    let qa = hs | FILE_ATTR_ARCHIVE | FILE_ATTR_DUP_VIEW_INDEX;
    let f = fname(now, ext_ref, "$Quota", 0, 0, qa);
    mk(24, 0x000C, vec![res(AT_STANDARD_INFORMATION, "", si72(now, qa, 0x101), 0), res(AT_FILE_NAME, "", f.clone(), 1), res(AT_INDEX_ROOT, "$O", hex(QUOTA_O_ROOT), 0), res(AT_INDEX_ROOT, "$Q", hex(QUOTA_Q_ROOT), 0)], Some((false, f)));
    let f = fname(now, ext_ref, "$ObjId", 0, 0, qa);
    mk(25, 0x000C, vec![res(AT_STANDARD_INFORMATION, "", si72(now, qa, 0x101), 0), res(AT_FILE_NAME, "", f.clone(), 1), res(AT_INDEX_ROOT, "$O", hex(OBJID_O_ROOT), 0)], Some((false, f)));
    let f = fname(now, ext_ref, "$Reparse", 0, 0, qa);
    mk(26, 0x000C, vec![res(AT_STANDARD_INFORMATION, "", si72(now, qa, 0x101), 0), res(AT_FILE_NAME, "", f.clone(), 1), res(AT_INDEX_ROOT, "$R", hex(REPARSE_R_ROOT), 0)], Some((false, f)));

    // $Extend (needs its children's names for the index)
    let sort_entries = |v: &mut Vec<(u64, Vec<u8>)>| {
        v.sort_by(|a, b| {
            let na = FileName::parse(&a.1).map(|f| f.name).unwrap_or_default();
            let nb = FileName::parse(&b.1).map(|f| f.name).unwrap_or_default();
            cmp_names(&up, &na, &nb)
        })
    };
    sort_entries(&mut ext_entries);
    let mut ext_node = Node { entries: vec![] };
    for (no, key) in &ext_entries {
        ext_node.entries.push(IEntry { data: mref(*no, seq_of(*no)), key: key.clone(), extra: vec![], child: None, last: false });
    }
    ext_node.entries.push(IEntry::end(None));
    let f = fname(now, root_ref, "$Extend", 0, 0, hs | FILE_ATTR_DUP_DIR);
    root_entries.push((11, f.clone()));
    {
        let mut r = MftRecord::new(seq_of(11), MFT_IN_USE | MFT_IS_DIR);
        r.add_attr(res(AT_STANDARD_INFORMATION, "", si72(now, hs, 0x101), 0));
        r.add_attr(res(AT_FILE_NAME, "", f, 1));
        r.add_attr(res(AT_INDEX_ROOT, "$I30", index_root_value(&root_hdr, &ext_node), 0));
        r.link_count = 1;
        recs[11] = Some(r);
    }

    // root index block
    sort_entries(&mut root_entries);
    let mut root_node = Node { entries: vec![] };
    for (no, key) in &root_entries {
        root_node.entries.push(IEntry { data: mref(*no, seq_of(*no)), key: key.clone(), extra: vec![], child: None, last: false });
    }
    root_node.entries.push(IEntry::end(None));
    let root_block = serialize_indx(&root_node, 0, IBS, 1).ok_or_else(|| FsError::Corrupt("root index overflow".into()))?;

    // ---- serialise the MFT
    let mut mft = vec![0u8; MFT_RECORDS as usize * RS];
    for no in 0..MFT_RECORDS {
        let raw = match &mut recs[no as usize] {
            Some(r) => r.serialize(no, RS).ok_or_else(|| FsError::Corrupt(format!("system record {no} overflow")))?,
            None => empty_record(no, RS, seq_of(no)),
        };
        mft[no as usize * RS..(no as usize + 1) * RS].copy_from_slice(&raw);
    }
    let mut mft_bitmap = vec![0u8; CS as usize];
    mft_bitmap[0] = 0xFF;
    mft_bitmap[1] = 0xFF;
    mft_bitmap[3] = 0x07;

    // ---- boot sector
    let mut boot = vec![0u8; 512];
    boot[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    boot[3..11].copy_from_slice(b"NTFS    ");
    put16(&mut boot, 0x0B, 512);
    boot[0x0D] = (CS / 512) as u8;
    boot[0x15] = 0xF8;
    put16(&mut boot, 0x18, 63);
    put16(&mut boot, 0x1A, 255);
    boot[0x24] = 0x80;
    boot[0x26] = 0x80;
    put64(&mut boot, 0x28, total_sectors);
    put64(&mut boot, 0x30, mft_lcn);
    put64(&mut boot, 0x38, mirr_lcn);
    boot[0x40] = (-10i8) as u8; // 2^10 = 1024-byte records
    boot[0x44] = 1; // one cluster per index block
    let serial = uuid::Uuid::new_v4();
    boot[0x48..0x50].copy_from_slice(&serial.as_bytes()[..8]);
    let code = hex(BOOT_CODE);
    boot[0x54..0x54 + code.len()].copy_from_slice(&code);
    boot[510] = 0x55;
    boot[511] = 0xAA;

    // ---- write everything
    let w = |lcn: u64, data: &[u8]| -> FsResult<()> { Ok(dev.write_at(lcn * CS, data)?) };
    let mut boot_area = vec![0u8; 8192];
    boot_area[..512].copy_from_slice(&boot);
    w(0, &boot_area)?;
    w(2, &mft_bitmap)?;
    w(mft_lcn, &mft)?;
    let mut mirr = vec![0u8; CS as usize];
    mirr.copy_from_slice(&mft[..CS as usize]);
    w(mirr_lcn, &mirr)?;
    let ff = vec![0xFFu8; 1 << 20];
    let mut p = 0;
    while p < log_bytes {
        let n = (log_bytes - p).min(ff.len() as u64) as usize;
        dev.write_at(log_lcn * CS + p, &ff[..n])?;
        p += n as u64;
    }
    let mut ad = attrdef.clone();
    ad.resize(CS as usize, 0);
    w(attrdef_lcn, &ad)?;
    w(rootia_lcn, &root_block)?;
    let mut bmv = bm.b.clone();
    bmv.resize((bm_cl * CS) as usize, 0);
    w(bitmap_lcn, &bmv)?;
    let mut sds = vec![0u8; (sds_cl * CS) as usize];
    let ent = hex(SDS_ENTRIES);
    sds[..ent.len()].copy_from_slice(&ent);
    sds[0x40000..0x40000 + ent.len()].copy_from_slice(&ent);
    w(sds_lcn, &sds)?;
    let upb: Vec<u8> = up.iter().flat_map(|c| c.to_le_bytes()).collect();
    w(upcase_lcn, &upb)?;
    dev.write_at(total_sectors * 512, &boot)?;
    dev.flush()?;
    Ok(())
}
