//! Decoder tests: every format round-trips; truncated and damaged inputs fail cleanly; random
//! mutations never panic.

use std::io::{Read, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use super::*;
use crate::forge::image::dmg::Kind;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ld-image-{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A small "disk": an MBR with two partitions, pseudo-random data, long zero runs, and a
/// size that is a multiple of 512 but not of the 4 MiB chunk.
pub(crate) fn disk(size: usize) -> Vec<u8> {
    let mut v = vec![0u8; size];
    let mut x = 0x9E37_79B9u32;
    for (i, b) in v.iter_mut().enumerate() {
        if (i / 50_000) % 3 == 1 {
            continue;
        }
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        // Low-entropy text-like bytes compress, as real images do.
        *b = if (i / 20_000) % 2 == 0 { b"lumendisk "[(x % 10) as usize] } else { x as u8 };
    }
    v[440..446].fill(0);
    for (k, (ty, start, len)) in [(0x0Cu8, 2048u32, 1024u32), (0x83, 4096, 1024)].iter().enumerate() {
        let e = 446 + k * 16;
        v[e..e + 16].fill(0);
        v[e + 4] = *ty;
        v[e + 8..e + 12].copy_from_slice(&start.to_le_bytes());
        v[e + 12..e + 16].copy_from_slice(&len.to_le_bytes());
    }
    v[462 + 16..510].fill(0);
    v[510] = 0x55;
    v[511] = 0xAA;
    v
}

fn gzip(data: &[u8]) -> Vec<u8> {
    // Two members, as `pigz` / concatenated gzip files produce.
    let mut out = vec![];
    let half = data.len() / 2;
    for part in [&data[..half], &data[half..]] {
        let mut e = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
        e.write_all(part).unwrap();
        out.extend(e.finish().unwrap());
    }
    out
}

fn xz(data: &[u8], block: Option<u64>) -> Vec<u8> {
    let mut o = lzma_rust2::XzOptions::with_preset(1);
    o.set_block_size(block.and_then(NonZeroU64::new));
    let mut w = lzma_rust2::XzWriter::new(vec![], o).unwrap();
    w.write_all(data).unwrap();
    w.finish().unwrap()
}

fn bzip2(data: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    let half = data.len() / 3;
    for part in [&data[..half], &data[half..]] {
        let mut e = bzip2::write::BzEncoder::new(vec![], bzip2::Compression::fast());
        e.write_all(part).unwrap();
        out.extend(e.finish().unwrap());
    }
    out
}

/// A zstd frame made of raw blocks (RFC 8878 §3.1.1), declaring its content size and with a
/// no checksum.
fn zstd_raw_frame(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x28, 0xB5, 0x2F, 0xFD, 0xA0]; // FCS 4 bytes, single segment
    out.extend((data.len() as u32).to_le_bytes());
    let blocks: Vec<&[u8]> = data.chunks(128 << 10).collect();
    for (i, b) in blocks.iter().enumerate() {
        let last = (i + 1 == blocks.len()) as u32;
        let h = ((b.len() as u32) << 3) | last;
        out.extend(&h.to_le_bytes()[..3]);
        out.extend(*b);
    }
    out
}

fn zstd(data: &[u8]) -> Vec<u8> {
    // A skippable frame, then two frames.
    let mut out = vec![0x50, 0x2A, 0x4D, 0x18, 5, 0, 0, 0, 1, 2, 3, 4, 5];
    let half = data.len() / 2;
    for part in [&data[..half], &data[half..]] {
        out.extend(ruzstd::encoding::compress_to_vec(part, ruzstd::encoding::CompressionLevel::Fastest));
    }
    out
}

struct ZipFile<'a> {
    name: &'a str,
    data: &'a [u8],
    method: u16,
}

/// A ZIP writer following APPNOTE (local headers, central directory, EOCD), optionally with
/// Zip64 extra fields for the sizes.
fn zip(files: &[ZipFile], zip64: bool) -> Vec<u8> {
    let mut out = vec![];
    let mut cd = vec![];
    for f in files {
        let enc = match f.method {
            0 => f.data.to_vec(),
            8 => {
                let mut e = flate2::write::DeflateEncoder::new(vec![], flate2::Compression::fast());
                e.write_all(f.data).unwrap();
                e.finish().unwrap()
            }
            _ => unreachable!(),
        };
        let crc = crc32fast::hash(f.data);
        let off = out.len() as u32;
        let (cs, us) = if zip64 { (u32::MAX, u32::MAX) } else { (enc.len() as u32, f.data.len() as u32) };
        let mut extra = vec![];
        if zip64 {
            extra.extend(1u16.to_le_bytes());
            extra.extend(16u16.to_le_bytes());
            extra.extend((f.data.len() as u64).to_le_bytes());
            extra.extend((enc.len() as u64).to_le_bytes());
        }
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend([20, 0, 0, 0]);
        out.extend(f.method.to_le_bytes());
        out.extend([0, 0, 0, 0]);
        out.extend(crc.to_le_bytes());
        out.extend(cs.to_le_bytes());
        out.extend(us.to_le_bytes());
        out.extend((f.name.len() as u16).to_le_bytes());
        out.extend((extra.len() as u16).to_le_bytes());
        out.extend(f.name.as_bytes());
        out.extend(&extra);
        out.extend(&enc);
        cd.extend(0x0201_4b50u32.to_le_bytes());
        cd.extend([20, 3, 20, 0, 0, 0]);
        cd.extend(f.method.to_le_bytes());
        cd.extend([0, 0, 0, 0]);
        cd.extend(crc.to_le_bytes());
        cd.extend(cs.to_le_bytes());
        cd.extend(us.to_le_bytes());
        cd.extend((f.name.len() as u16).to_le_bytes());
        cd.extend((extra.len() as u16).to_le_bytes());
        cd.extend([0u8; 10]); // comment length, disk, internal and external attributes
        cd.extend(off.to_le_bytes());
        cd.extend(f.name.as_bytes());
        cd.extend(&extra);
    }
    let cd_off = out.len() as u32;
    out.extend(&cd);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0, 0, 0, 0]);
    out.extend((files.len() as u16).to_le_bytes());
    out.extend((files.len() as u16).to_le_bytes());
    out.extend((cd.len() as u32).to_le_bytes());
    out.extend(cd_off.to_le_bytes());
    out.extend(3u16.to_le_bytes());
    out.extend(b"hey");
    out
}

/// Every format of the same disk: (name, file bytes, expected format, size known up front).
fn samples(d: &[u8]) -> Vec<(&'static str, Vec<u8>, Format, bool)> {
    vec![
        ("raw", d.to_vec(), Format::Raw, true),
        ("gzip", gzip(d), Format::Gzip, false),
        ("xz-single-block", xz(d, None), Format::Xz, true),
        ("xz-multi-block", xz(d, Some(256 << 10)), Format::Xz, true),
        ("bzip2", bzip2(d), Format::Bzip2, false),
        ("zstd", zstd(d), Format::Zstd, false),
        ("zstd-sized", zstd_raw_frame(d), Format::Zstd, true),
        ("zip-deflate", zip(&[ZipFile { name: "readme.txt", data: b"notes", method: 0 }, ZipFile { name: "disk.img", data: d, method: 8 }], false), Format::Zip, true),
        ("zip-stored-zip64", zip(&[ZipFile { name: "dir/disk.img", data: d, method: 0 }, ZipFile { name: "disk.img.sha256", data: b"x", method: 0 }], true), Format::Zip, true),
        ("vhd-fixed", vhd::build::fixed(d), Format::Vhd, true),
        ("vhd-dynamic", vhd::build::dynamic(d, 64 << 10), Format::Vhd, true),
        ("dmg-udro", dmg::build::dmg(d, 1 << 20, &[Kind::Raw]), Format::Dmg, true),
        ("dmg-udzo", dmg::build::dmg(d, 1 << 20, &[Kind::Zlib]), Format::Dmg, true),
        ("dmg-mixed", dmg::build::dmg(d, 256 << 10, &[Kind::Zlib, Kind::Bzip2, Kind::Lzfse, Kind::Lzma, Kind::Adc, Kind::Raw]), Format::Dmg, true),
    ]
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

fn decode_all(img: &Image) -> io::Result<Vec<u8>> {
    let mut v = vec![];
    img.pipe(1 << 20, 2).read_to_end(&mut v)?;
    Ok(v)
}

#[test]
fn every_format_round_trips_as_a_stream_and_with_random_access() {
    let dir = tmpdir("roundtrip");
    let d = disk(3 * 1024 * 1024 + 512 * 37);
    let ctx = JobCtx::detached(false);
    for (name, bytes, fmt, sized) in samples(&d) {
        let p = write(&dir, name, &bytes);
        let img = Image::open(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(img.format, fmt, "{name}");
        assert_eq!(img.size.is_some(), sized, "{name}: size known");
        if sized {
            assert_eq!(img.size, Some(d.len() as u64), "{name}");
        }
        assert_eq!(decode_all(&img).unwrap_or_else(|e| panic!("{name}: {e}")), d, "{name}: stream");
        let dev = img.device(&ctx, false, None).unwrap();
        for (off, len) in [(0usize, 512usize), (1_000_003, 70_001), (d.len() - 4096, 4096), (262_100, 300)] {
            let mut b = vec![0u8; len];
            dev.read_at(off as u64, &mut b).unwrap_or_else(|e| panic!("{name} @{off}: {e}"));
            assert_eq!(b, &d[off..off + len], "{name} @{off}");
        }
        let parts = crate::block::parse_partitions(dev.as_ref()).unwrap();
        assert_eq!(parts.len(), 2, "{name}: partitions seen through the device");
        // Quick inspection never fails on a valid image and reports what it found.
        let v = inspect_quick(&p.to_string_lossy()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(v["format"], fmt.name(), "{name}");
        assert_eq!(v["partition_scheme"], "MBR", "{name}");
        assert_eq!(v["recommended_mode"], "dd", "{name}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn xz_layout_gives_exact_size_and_enables_parallel_decoding() {
    let dir = tmpdir("xz");
    let d = disk(2 << 20);
    let multi = write(&dir, "m.xz", &xz(&d, Some(128 << 10)));
    let img = Image::open(&multi).unwrap();
    // (Blocks are at least the dictionary size: 1 MiB at preset 1.)
    assert!(matches!(&img.detail, Detail::Xz(Some(l)) if l.parallel() && l.blocks == 2));
    assert_eq!(decode_all(&img).unwrap(), d);
    // Two concatenated streams with stream padding between them.
    let mut two = xz(&d[..1 << 20], Some(256 << 10));
    two.extend([0u8; 8]);
    two.extend(xz(&d[1 << 20..], None));
    let p = write(&dir, "two.xz", &two);
    let img = Image::open(&p).unwrap();
    assert_eq!(img.size, Some(d.len() as u64));
    assert_eq!(decode_all(&img).unwrap(), d);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn truncated_images_fail_with_an_error() {
    let dir = tmpdir("trunc");
    let d = disk(1 << 20);
    for (name, bytes, fmt, _) in samples(&d) {
        if fmt == Format::Raw {
            continue; // a shorter raw image is still a raw image
        }
        for cut in [bytes.len() / 2, bytes.len() - 1, 40, 3] {
            if fmt == Format::Vhd && cut == bytes.len() - 1 {
                continue; // a 511-byte footer is valid (VHD spec: images before Virtual PC 2004)
            }
            if fmt == Format::Zip && cut + 3 >= bytes.len() {
                continue; // only the archive comment is cut: the entry is intact (CRC checked)
            }
            let p = write(&dir, &format!("{name}-{cut}"), &bytes[..cut]);
            let r = Image::open(&p).map_err(|e| e.to_string()).and_then(|img| decode_all(&img).map_err(|e| e.to_string()));
            match r {
                Err(_) => {}
                // Cutting a ZIP's tail removes its directory: then it's not a ZIP anymore and
                // reads as a raw image, which is fine — but it must not decode to the disk.
                Ok(v) => assert_ne!(v, d, "{name} cut at {cut} decoded to the full disk"),
            }
            if fmt != Format::Zip && cut > 40 {
                assert!(r_is_err(&p), "{name} cut at {cut} must fail");
            }
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

fn r_is_err(p: &Path) -> bool {
    match Image::open(p) {
        Err(_) => true,
        Ok(img) => img.format == Format::Raw || decode_all(&img).is_err(),
    }
}

#[test]
fn damaged_data_is_detected_by_the_formats_checks() {
    let dir = tmpdir("corrupt");
    let d = disk(1 << 20);
    for (name, mut bytes, fmt, _) in samples(&d) {
        // Formats with a checksum over the data (gzip / bzip2 CRC, xz check, zip CRC-32).
        if !matches!(fmt, Format::Gzip | Format::Bzip2 | Format::Xz | Format::Zip) {
            continue;
        }
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x55;
        bytes[mid + 1] ^= 0xAA;
        let p = write(&dir, name, &bytes);
        let r = Image::open(&p).map_err(|e| e.to_string()).and_then(|img| decode_all(&img).map_err(|e| e.to_string()));
        assert!(r.is_err() || r.as_ref().unwrap() != &d, "{name}");
        assert!(r.is_err(), "{name}: corruption must be reported, got {} bytes", r.map(|v| v.len()).unwrap_or(0));
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ambiguous_and_unsupported_containers_are_refused_clearly() {
    let dir = tmpdir("refuse");
    let d = disk(64 << 10);
    let two = write(&dir, "two.zip", &zip(&[ZipFile { name: "a.img", data: &d, method: 0 }, ZipFile { name: "b.iso", data: &d, method: 8 }], false));
    let e = Image::open(&two).unwrap_err().to_string();
    assert!(e.contains("a.img") && e.contains("b.iso"), "{e}");
    let none = write(&dir, "none.zip", &zip(&[ZipFile { name: "readme.txt", data: b"hi", method: 0 }], false));
    assert!(Image::open(&none).is_err());
    let mut diff = vhd::build::dynamic(&d, 4096 * 4);
    // Differencing disk (type 4): footer at both ends, checksums fixed up.
    for at in [0, diff.len() - 512] {
        let mut f = diff[at..at + 512].to_vec();
        f[60..64].copy_from_slice(&4u32.to_be_bytes());
        f[64..68].fill(0);
        let sum = !f.iter().fold(0u32, |s, &b| s.wrapping_add(b as u32));
        f[64..68].copy_from_slice(&sum.to_be_bytes());
        diff[at..at + 512].copy_from_slice(&f);
    }
    let p = write(&dir, "diff.vhd", &diff);
    assert!(matches!(Image::open(&p), Err(ForgeError::Unsupported(m)) if m.contains("VHD")));
    for (name, head) in [("x.vhdx", b"vhdxfile".to_vec()), ("x.qcow2", b"QFI\xFB\0\0\0\x03".to_vec()), ("x.7z", vec![b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C, 0, 4])] {
        let mut b = head;
        b.resize(4096, 0);
        let p = write(&dir, name, &b);
        assert!(matches!(Image::open(&p), Err(ForgeError::Unsupported(_))), "{name}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn iso_descriptors_el_torito_and_hybrid_tables_are_recognised() {
    let mut iso = vec![0u8; 40 * 2048];
    let pvd = 16 * 2048;
    iso[pvd] = 1;
    iso[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
    iso[pvd + 40..pvd + 72].copy_from_slice(b"UBUNTU 24.04 AMD64              ");
    iso[pvd + 80..pvd + 84].copy_from_slice(&40u32.to_le_bytes());
    iso[pvd + 128..pvd + 130].copy_from_slice(&2048u16.to_le_bytes());
    let br = 17 * 2048;
    iso[br + 1..br + 6].copy_from_slice(b"CD001");
    iso[br + 7..br + 30].copy_from_slice(b"EL TORITO SPECIFICATION");
    iso[br + 71..br + 75].copy_from_slice(&20u32.to_le_bytes());
    let term = 18 * 2048;
    iso[term] = 255;
    iso[term + 1..term + 6].copy_from_slice(b"CD001");
    let cat = 20 * 2048;
    iso[cat] = 1;
    iso[cat + 30] = 0x55;
    iso[cat + 31] = 0xAA;
    iso[cat + 64] = 0x91;
    iso[cat + 65] = 0xEF;
    iso[cat + 66] = 1;
    let dev = crate::block::MemDev::from_vec(iso.clone());
    let i = isoinfo::probe(&dev).unwrap();
    assert_eq!(i.label, "UBUNTU 24.04 AMD64");
    assert_eq!(i.size, 40 * 2048);
    assert!(i.el_torito && i.efi_boot && !i.hybrid);
    assert_eq!(recommend(Some(&i), false, false).0, WriteMode::Iso);
    // isohybrid: an MBR partition entry in the system area.
    iso[446 + 4] = 0x17;
    iso[446 + 12] = 40;
    iso[510] = 0x55;
    iso[511] = 0xAA;
    let i = isoinfo::probe(&crate::block::MemDev::from_vec(iso)).unwrap();
    assert!(i.hybrid);
    assert_eq!(i.hybrid_tables, vec!["mbr"]);
    assert_eq!(recommend(Some(&i), false, false).0, WriteMode::Dd);
    assert_eq!(recommend(Some(&i), true, false).0, WriteMode::Windows);
    assert!(isoinfo::probe(&crate::block::MemDev::from_vec(disk(1 << 20))).is_none());
}

#[test]
fn lazy_device_respects_its_budget_and_cleans_up() {
    let dir = tmpdir("lazy");
    let d = disk(3 << 20);
    let p = write(&dir, "a.xz", &xz(&d, None));
    let img = Image::open(&p).unwrap();
    let dev = img.device(&JobCtx::detached(false), false, Some(1 << 20)).unwrap();
    let mut b = vec![0u8; 4096];
    dev.read_at(0, &mut b).unwrap();
    assert_eq!(&b[..], &d[..4096]);
    assert!(dev.read_at(2 << 20, &mut b).is_err(), "beyond the budget");
    let ctx = JobCtx::detached(false);
    let full = img.device(&ctx, true, None).unwrap();
    assert_eq!(ctx.done(), d.len() as u64, "progress of the full decompression");
    full.read_at(d.len() as u64 - 4096, &mut b).unwrap();
    assert_eq!(&b[..], &d[d.len() - 4096..]);
    // The temporary file is already unlinked: nothing named lumendisk-image-* stays behind
    // for this device.
    drop((dev, full));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// xorshift PRNG, deterministic across runs.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[test]
fn random_mutations_never_panic_or_hang() {
    let dir = tmpdir("fuzz");
    let d = disk(256 << 10);
    let ctx = JobCtx::detached(false);
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let mut outcomes = std::collections::BTreeMap::<String, (u32, u32)>::new();
    let iterations = std::env::var("LUMENDISK_FUZZ_ITER").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
    for (name, bytes, _, _) in samples(&d) {
        for it in 0..iterations {
            let mut m = bytes.clone();
            match rng.below(4) {
                0 => {
                    for _ in 0..1 + rng.below(8) {
                        let i = rng.below(m.len());
                        m[i] ^= 1 << rng.below(8);
                    }
                }
                1 => {
                    // Damage headers / trailers, where the sizes live.
                    for _ in 0..1 + rng.below(4) {
                        let i = if rng.below(2) == 0 { rng.below(m.len().min(1024)) } else { m.len() - 1 - rng.below(m.len().min(1024)) };
                        m[i] = rng.next() as u8;
                    }
                }
                2 => {
                    let n = rng.below(m.len());
                    m.truncate(n.max(1));
                }
                _ if m.len() >= 8 => {
                    // Huge values in likely size fields.
                    let i = rng.below(m.len().saturating_sub(8));
                    m[i..i + 8].copy_from_slice(&(u64::MAX - rng.below(1000) as u64).to_be_bytes());
                }
                _ => m.truncate(1),
            }
            let p = write(&dir, &format!("{name}-{it}"), &m);
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
                let img = Image::open(&p).map_err(|e| e.to_string())?;
                let v = decode_all(&img).map_err(|e| e.to_string())?;
                if let Some(s) = img.size {
                    assert_eq!(v.len() as u64, s, "a decoded image always has its announced size");
                }
                let dev = img.device(&ctx, false, Some(4 << 20)).map_err(|e| e.to_string())?;
                let mut b = vec![0u8; 8192];
                let _ = dev.read_at(0, &mut b);
                let _ = dev.read_at(dev.size().saturating_sub(8192), &mut b);
                let _ = inspect_quick(&p.to_string_lossy());
                Ok(())
            }));
            let e = outcomes.entry(name.to_string()).or_default();
            match r {
                Ok(Ok(())) => e.0 += 1,
                Ok(Err(msg)) => {
                    assert!(!msg.contains("decoder failure"), "{name} #{it}: a decoder panicked: {msg}");
                    e.1 += 1;
                }
                Err(_) => panic!("{name} mutation #{it} panicked"),
            }
            let _ = std::fs::remove_file(&p);
        }
    }
    eprintln!("fuzz outcomes (decoded, refused): {outcomes:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}
