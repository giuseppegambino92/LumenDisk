//! Writing an image: DD mode (with verification), and dispatch to ISO mode
//! ([`super::linux::write`]), Windows media ([`super::windows::write`]) and Raspberry Pi
//! customisation ([`super::rpi::customize`]).
//!
//! Owner: the image / DD workstream.
//!
//! DD pipeline: the image is decoded on its own thread ([`Image::pipe`]), [`CHUNK`]-sized
//! chunks (4 MiB, a multiple of every sector size) ahead of the writer through a bounded
//! channel; the job thread writes each chunk at its offset (the final partial sector is padded
//! with zeros on disks); two hashing threads compute, off the write path, the SHA-256 of the
//! image and the SHA-256 of every written chunk.
//!
//! Verification reads the target back and compares each chunk's SHA-256 with the one recorded
//! while writing. Why digests rather than a byte comparison against a re-read of the source:
//! re-reading a compressed source means decompressing it a second time (XZ is CPU-bound: that
//! would double the time for the most common downloads), whereas digests recorded on the way
//! in cost nothing on the write path and let the verify pass run at the device's read speed.
//! SHA-256 per 4 MiB chunk is collision-resistant, so a match proves the bytes are equal; on
//! a mismatch, only the first differing chunk is fetched from the source again to report the
//! exact first differing byte.
//!
//! On macOS the whole disk is written through `/dev/rdiskN` ([`target::open_for_write`]) and
//! the descriptor stays open for the whole job, so DiskArbitration doesn't mount the new
//! partitions half-way through.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::hash::{self, Expected};
use super::image::{self, Image, CHUNK};
use super::job::{JobCtx, Phase};
use super::plan::{FlashRequest, WriteMode};
use super::target::{self, OpenTarget, TargetSpec};
use super::{ForgeError, ForgeResult};
use crate::block::BlockDev;
use crate::i18n::t;

/// "1.2 GB" (decimal units, as Finder shows sizes).
pub fn human(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.1} kB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

fn too_large(image: u64, target: u64) -> ForgeError {
    ForgeError::Invalid(t(
        &format!("l'immagine ({}) è più grande della destinazione ({}): scegli un disco più capiente", human(image), human(target)),
        &format!("the image ({}) is larger than the destination ({}): choose a larger disk", human(image), human(target)),
    ))
}

/// Where DD writes: an open device (disk, or a file of fixed size), or a plain file that
/// grows to the image's size.
enum Dest {
    Dev(Arc<dyn BlockDev>),
    Grow(File),
}

impl Dest {
    fn capacity(&self) -> Option<u64> {
        match self {
            Dest::Dev(d) => Some(d.size()),
            Dest::Grow(_) => None,
        }
    }
    fn write_at(&self, off: u64, buf: &[u8]) -> io::Result<()> {
        match self {
            Dest::Dev(d) => d.write_at(off, buf),
            Dest::Grow(f) => f.write_all_at(buf, off),
        }
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        match self {
            Dest::Dev(d) => d.read_at(off, buf),
            Dest::Grow(f) => f.read_exact_at(buf, off),
        }
    }
    fn flush(&self) -> io::Result<()> {
        match self {
            Dest::Dev(d) => d.flush(),
            Dest::Grow(f) => f.sync_all(),
        }
    }
}

/// What the write pass produced.
struct Written {
    /// Image bytes.
    bytes: u64,
    /// Bytes written on the target (the image + the zero padding of its last sector).
    padded: u64,
    secs: f64,
    sha256: String,
    chunks: Vec<[u8; 32]>,
}

/// Off-the-write-path hashing: the image's SHA-256 and each written chunk's SHA-256.
struct Hashers {
    whole: Option<(SyncSender<(Arc<Vec<u8>>, usize)>, JoinHandle<String>)>,
    chunks: Option<(SyncSender<Arc<Vec<u8>>>, JoinHandle<Vec<[u8; 32]>>)>,
}

impl Hashers {
    fn new() -> Self {
        let (wtx, wrx): (SyncSender<(Arc<Vec<u8>>, usize)>, Receiver<_>) = sync_channel(4);
        let whole = std::thread::Builder::new()
            .name("lumendisk-flash-sha".into())
            .spawn(move || {
                let mut h = Sha256::new();
                for (c, n) in wrx {
                    h.update(&c[..n]);
                }
                hex(&h.finalize())
            })
            .expect("spawn hash thread");
        let (ctx_, crx): (SyncSender<Arc<Vec<u8>>>, Receiver<_>) = sync_channel(4);
        let chunks = std::thread::Builder::new()
            .name("lumendisk-flash-chunks".into())
            .spawn(move || crx.into_iter().map(|c| Sha256::digest(&c[..]).into()).collect())
            .expect("spawn hash thread");
        Hashers { whole: Some((wtx, whole)), chunks: Some((ctx_, chunks)) }
    }
    fn feed(&self, padded: &Arc<Vec<u8>>, len: usize) {
        if let Some((tx, _)) = &self.whole {
            let _ = tx.send((padded.clone(), len));
        }
        if let Some((tx, _)) = &self.chunks {
            let _ = tx.send(padded.clone());
        }
    }
    fn finish(mut self) -> (String, Vec<[u8; 32]>) {
        let (wtx, wh) = self.whole.take().unwrap();
        let (ctx_, ch) = self.chunks.take().unwrap();
        drop((wtx, ctx_));
        (wh.join().unwrap_or_default(), ch.join().unwrap_or_default())
    }
}

impl Drop for Hashers {
    fn drop(&mut self) {
        // Cancelled / failed: let the threads end without waiting for them.
        self.whole.take();
        self.chunks.take();
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Write the decoded image at offset 0 of `dest`.
fn dd_write(img: &Image, dest: &Dest, sector: usize, ctx: &JobCtx) -> ForgeResult<Written> {
    let sector = sector.max(1);
    ctx.phase(Phase::Writing, img.size.or(img.size_hint).unwrap_or(0));
    let t0 = Instant::now();
    let mut pipe = img.pipe(CHUNK, 4);
    let hashers = Hashers::new();
    let mut off = 0u64;
    let mut padded_total = 0u64;
    while let Some(c) = pipe.next_chunk() {
        ctx.check()?;
        let c = c.map_err(|e| image::damaged(img, e))?;
        let len = c.len();
        if let Some(cap) = dest.capacity() {
            if off + len as u64 > cap {
                return Err(too_large(img.size.unwrap_or(off + len as u64), cap));
            }
        }
        let mut c = c;
        let rem = len % sector;
        if rem != 0 {
            // The last sector of a disk can only be written whole: pad with zeros.
            c.resize(len + sector - rem, 0);
            if let Some(cap) = dest.capacity() {
                if off + c.len() as u64 > cap {
                    c.truncate(len);
                }
            }
        }
        let c = Arc::new(c);
        hashers.feed(&c, len);
        dest.write_at(off, &c).map_err(|e| ForgeError::Io(t(&format!("errore di scrittura a {off}: {e}"), &format!("write error at byte {off}: {e}"))))?;
        off += len as u64;
        padded_total += c.len() as u64;
        ctx.advance(len as u64)?;
        if img.size.is_none() {
            let used = pipe.consumed.load(std::sync::atomic::Ordering::Relaxed).max(1);
            ctx.set_total(((off as f64) * img.file_size as f64 / used as f64) as u64);
        }
    }
    ctx.set_total(off);
    ctx.info(t("scrittura delle cache del disco", "flushing the disk's caches"));
    dest.flush().map_err(|e| ForgeError::Io(format!("flush: {e}")))?;
    let secs = t0.elapsed().as_secs_f64();
    let (sha256, chunks) = hashers.finish();
    Ok(Written { bytes: off, padded: padded_total, secs, sha256, chunks })
}

/// The first byte where `dest` differs from the image within chunk `i`.
fn first_difference(img: &Image, dest: &Dest, i: usize, w: &Written) -> ForgeResult<Option<u64>> {
    let start = i as u64 * CHUNK as u64;
    let len = (w.bytes - start.min(w.bytes)).min(CHUNK as u64) as usize;
    let mut src = vec![0u8; len];
    match img.native_device()? {
        Some(d) => d.read_at(start, &mut src)?,
        None => {
            let mut pipe = img.pipe(CHUNK, 2);
            let mut k = 0;
            loop {
                let c = pipe.next_chunk().ok_or_else(|| ForgeError::Io("source ended early".into()))?.map_err(|e| image::damaged(img, e))?;
                if k == i {
                    src.copy_from_slice(&c[..len]);
                    break;
                }
                k += 1;
            }
        }
    }
    let padded_len = (w.padded - start).min(CHUNK as u64) as usize;
    let mut got = vec![0u8; padded_len];
    dest.read_at(start, &mut got)?;
    if let Some(p) = src.iter().zip(&got).position(|(a, b)| a != b) {
        return Ok(Some(start + p as u64));
    }
    // The zero padding of the last sector.
    Ok(got[len..].iter().position(|&b| b != 0).map(|p| start + (len + p) as u64))
}

#[derive(Debug)]
struct Verified {
    secs: f64,
    sha256: String,
}

fn verify(img: &Image, dest: &Dest, w: &Written, ctx: &JobCtx) -> ForgeResult<Verified> {
    ctx.phase(Phase::Verifying, w.bytes);
    let t0 = Instant::now();
    let mut whole = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    for (i, want) in w.chunks.iter().enumerate() {
        let start = i as u64 * CHUNK as u64;
        let n = (w.padded - start).min(CHUNK as u64) as usize;
        dest.read_at(start, &mut buf[..n]).map_err(|e| ForgeError::Io(t(&format!("errore di lettura a {start} durante la verifica: {e}"), &format!("read error at byte {start} while verifying: {e}"))))?;
        let image_part = (w.bytes - start).min(n as u64) as usize;
        whole.update(&buf[..image_part]);
        if Sha256::digest(&buf[..n]).as_slice() != want {
            let at = first_difference(img, dest, i, w)?.unwrap_or(start);
            return Err(ForgeError::Mismatch(t(
                &format!("verifica fallita: i dati scritti differiscono dall'immagine a partire dal byte {at} (blocco da 4 MiB n. {i}). Il disco potrebbe essere difettoso o contraffatto."),
                &format!("verification failed: the written data differs from the image from byte {at} on (4 MiB chunk #{i}). The disk may be faulty or counterfeit."),
            )));
        }
        ctx.advance(image_part as u64)?;
    }
    let sha256 = hex(&whole.finalize());
    if sha256 != w.sha256 {
        return Err(ForgeError::Mismatch(t("verifica fallita: SHA-256 diverso", "verification failed: SHA-256 differs")));
    }
    Ok(Verified { secs: t0.elapsed().as_secs_f64(), sha256 })
}

/// The expected checksum: the request's, else a checksum file next to the image.
fn expected(r: &FlashRequest, img: &Image) -> ForgeResult<Option<(Expected, String)>> {
    if let Some(s) = &r.expect_hash {
        if !s.trim().is_empty() {
            return Ok(Some((hash::parse_expected(s)?, "request".into())));
        }
    }
    Ok(hash::find_sums_entry(&img.path).map(|e| (Expected { algorithm: e.algorithm, hex: e.expected }, e.path)))
}

fn target_capacity(spec: &TargetSpec) -> ForgeResult<Option<u64>> {
    Ok(match spec {
        TargetSpec::Disk { bsd } => Some(target::disk_info(bsd)?.size),
        TargetSpec::File { size, .. } => *size,
    })
}

pub fn run(r: &FlashRequest, ctx: &JobCtx) -> ForgeResult<Value> {
    let t_all = Instant::now();
    ctx.phase(Phase::Preparing, 0);
    let img = Image::open(&r.source)?;
    ctx.info(format!(
        "{}: {} image, {} file, {}",
        img.path.display(),
        img.format.name(),
        human(img.file_size),
        img.size.map(|s| format!("{} disk image", human(s))).unwrap_or_else(|| "size known after decompressing".into())
    ));
    let quick = image::inspect_image(&img, ctx, false)?;
    let mut windows = quick.windows.is_some();
    let mut src_dev: Option<Arc<dyn BlockDev>> = None;
    let wants_tree = match r.mode {
        WriteMode::Auto => quick.iso.as_ref().is_some_and(|i| !i.hybrid),
        WriteMode::Iso | WriteMode::Windows => true,
        WriteMode::Dd => false,
    };
    if wants_tree {
        if quick.iso.is_none() {
            return Err(ForgeError::Invalid(t(
                "le modalità ISO e Windows richiedono un'immagine ISO: questa è un'immagine disco, usa la modalità DD",
                "ISO and Windows modes need an ISO image: this is a disk image, use DD mode",
            )));
        }
        if !quick.random_access {
            ctx.phase(Phase::Extracting, img.size.unwrap_or(0));
        }
        let dev = img.device(ctx, true, None)?;
        if r.mode == WriteMode::Auto && !windows {
            if let Some(mut tree) = image::open_iso_tree(dev.clone())? {
                windows = super::windows::detect(tree.as_mut())?.is_some();
            }
        }
        src_dev = Some(dev);
    }
    let mode = match r.mode {
        WriteMode::Auto => image::recommend(quick.iso.as_ref(), windows, quick.rpi.is_some()).0,
        m => m,
    };
    ctx.info(format!("mode: {mode:?}"));

    // Checksum before anything is written.
    let t_hash = Instant::now();
    let mut expect_json = Value::Null;
    if let Some((exp, origin)) = expected(r, &img)? {
        let matched = hash::verify_expected(&img, &exp, ctx, &origin)?;
        ctx.info(t(&format!("checksum {} corretto ({matched})", exp.algorithm.name()), &format!("{} checksum matches ({matched})", exp.algorithm.name())));
        expect_json = json!({"algorithm": exp.algorithm, "hex": exp.hex, "source": origin, "matched": matched});
    }
    let hash_secs = t_hash.elapsed().as_secs_f64();

    // Refuse an image that can't fit before touching the target.
    let capacity = target_capacity(&r.target)?;
    if mode == WriteMode::Dd {
        if let (Some(sz), Some(cap)) = (img.size, capacity) {
            if sz > cap {
                return Err(too_large(sz, cap));
            }
        }
    }

    let (target_path, disk_bsd) = match &r.target {
        TargetSpec::Disk { bsd } => (None, Some(bsd.clone())),
        TargetSpec::File { path, .. } => (Some(path.clone()), None),
    };
    let mut result = serde_json::Map::new();
    result.insert("mode".into(), json!(mode));
    result.insert("source".into(), json!(img.path));
    result.insert("format".into(), json!(img.format));
    let mut durations = serde_json::Map::new();
    durations.insert("hashing".into(), json!(hash_secs));

    // A file target without a size grows to the image's size.
    let grow = matches!(r.target, TargetSpec::File { size: None, .. }) && mode == WriteMode::Dd;
    let open_spec = match (&r.target, grow) {
        (TargetSpec::File { path, .. }, true) => TargetSpec::File { path: path.clone(), size: Some(0) },
        (s, _) => s.clone(),
    };
    let target = target::open_for_write(&open_spec, ctx)?;
    result.insert("target".into(), json!(target.path));
    match mode {
        WriteMode::Dd => {
            let dest = if grow {
                Dest::Grow(std::fs::OpenOptions::new().read(true).write(true).open(target_path.as_ref().unwrap())?)
            } else {
                Dest::Dev(target.dev.clone())
            };
            if let (Some(sz), Some(cap)) = (img.size, dest.capacity()) {
                if sz > cap {
                    return Err(too_large(sz, cap));
                }
            }
            let sector = match (&target.info, grow) {
                (_, true) => 1,
                (Some(i), _) => (i.block_size as usize).max(512),
                (None, _) => 512,
            };
            let w = dd_write(&img, &dest, sector, ctx)?;
            if let Dest::Grow(f) = &dest {
                f.set_len(w.bytes)?;
            }
            ctx.info(format!("{} written in {:.1} s ({:.1} MB/s)", human(w.bytes), w.secs, w.bytes as f64 / 1e6 / w.secs.max(1e-6)));
            durations.insert("writing".into(), json!(w.secs));
            result.insert("image_size".into(), json!(w.bytes));
            result.insert("bytes_written".into(), json!(w.padded));
            result.insert("write_mb_s".into(), json!(w.bytes as f64 / 1e6 / w.secs.max(1e-6)));
            let verification = if r.verify {
                let v = verify(&img, &dest, &w, ctx)?;
                ctx.info(format!("verified in {:.1} s ({:.1} MB/s)", v.secs, w.bytes as f64 / 1e6 / v.secs.max(1e-6)));
                durations.insert("verifying".into(), json!(v.secs));
                result.insert("verify_mb_s".into(), json!(w.bytes as f64 / 1e6 / v.secs.max(1e-6)));
                json!({"performed": true, "ok": true, "method": "sha256 of every 4 MiB chunk, recorded while writing, compared with a read-back of the target", "target_sha256": v.sha256, "first_mismatch": null})
            } else {
                json!({"performed": false, "ok": null})
            };
            result.insert("verification".into(), verification);
            result.insert("hashes".into(), json!({"image_sha256": w.sha256, "expected": expect_json}));
            if let Some(opts) = &r.rpi {
                ctx.phase(Phase::Customizing, 0);
                let t_c = Instant::now();
                let dev: Arc<dyn BlockDev> = match &dest {
                    Dest::Dev(d) => d.clone(),
                    Dest::Grow(_) => Arc::new(crate::block::FileDev::open(target_path.as_ref().unwrap(), false)?),
                };
                let v = super::rpi::customize(dev, opts, ctx).map_err(|e| prefix_err(e, "immagine scritta e verificata, ma la personalizzazione non è riuscita", "image written and verified, but the customisation failed"))?;
                durations.insert("customizing".into(), json!(t_c.elapsed().as_secs_f64()));
                result.insert("rpi".into(), v);
            }
        }
        WriteMode::Iso | WriteMode::Windows => {
            let src = src_dev.ok_or_else(|| ForgeError::Io("source device".into()))?;
            let t_w = Instant::now();
            let v = if mode == WriteMode::Iso { super::linux::write(r, src, &target, ctx)? } else { super::windows::write(r, src, &target, ctx)? };
            durations.insert("writing".into(), json!(t_w.elapsed().as_secs_f64()));
            result.insert("result".into(), v);
            result.insert("hashes".into(), json!({"expected": expect_json}));
        }
        WriteMode::Auto => unreachable!("resolved above"),
    }
    finish(target, disk_bsd, r.eject, ctx, &mut result);
    durations.insert("total".into(), json!(t_all.elapsed().as_secs_f64()));
    result.insert("durations".into(), Value::Object(durations));
    Ok(Value::Object(result))
}

fn prefix_err(e: ForgeError, it: &str, en: &str) -> ForgeError {
    let msg = format!("{}: {e}", t(it, en));
    match e {
        ForgeError::Cancelled => ForgeError::Cancelled,
        ForgeError::Unsupported(_) => ForgeError::Unsupported(msg),
        ForgeError::Invalid(_) => ForgeError::Invalid(msg),
        _ => ForgeError::Io(msg),
    }
}

/// Close the target (releasing the claim) and eject the disk when asked.
fn finish(target: OpenTarget, disk: Option<String>, eject: bool, ctx: &JobCtx, result: &mut serde_json::Map<String, Value>) {
    drop(target);
    let mut ejected = false;
    if let (Some(bsd), true) = (disk, eject) {
        ctx.phase(Phase::Finishing, 0);
        match target::eject(&bsd) {
            Ok(()) => {
                ejected = true;
                ctx.info(t(&format!("{bsd} espulso: si può scollegare"), &format!("{bsd} ejected: it can be unplugged")));
            }
            Err(e) => ctx.warn(t(&format!("espulsione di {bsd} non riuscita: {e}"), &format!("could not eject {bsd}: {e}"))),
        }
    }
    result.insert("ejected".into(), json!(ejected));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::plan::FlashRequest;

    fn req(source: &std::path::Path, target: TargetSpec) -> FlashRequest {
        serde_json::from_value(json!({"source": source, "target": target, "eject": false})).unwrap()
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ld-flash-{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn data(n: usize) -> Vec<u8> {
        let mut x = 0x1234_5678u32;
        (0..n)
            .map(|i| {
                if (i / 70_000) % 3 == 0 {
                    return 0;
                }
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    #[test]
    fn dd_to_files_of_every_kind_with_verification() {
        let d = tmp("dd");
        // Not a multiple of 512: the last sector is padded on fixed-size targets.
        let raw = data(CHUNK * 2 + 1000);
        let src = d.join("src.img.gz");
        {
            use std::io::Write;
            let mut e = flate2::write::GzEncoder::new(File::create(&src).unwrap(), flate2::Compression::fast());
            e.write_all(&raw).unwrap();
            e.finish().unwrap();
        }
        // A file that grows to the image size (unknown in advance for gzip).
        let out = d.join("out.img");
        let ctx = JobCtx::detached(false);
        let v = run(&req(&src, TargetSpec::File { path: out.clone(), size: None }), &ctx).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), raw);
        assert_eq!(v["verification"]["ok"], true);
        assert_eq!(v["image_size"], raw.len() as u64);
        assert_eq!(v["hashes"]["image_sha256"], hex(&Sha256::digest(&raw)));
        assert_eq!(v["verification"]["target_sha256"], v["hashes"]["image_sha256"]);
        // A fixed-size "disk": the image, zero padding to the sector, old data after it.
        let disk = d.join("disk.img");
        std::fs::write(&disk, vec![0xEEu8; CHUNK * 3]).unwrap();
        let v = run(&req(&src, TargetSpec::File { path: disk.clone(), size: Some((CHUNK * 3) as u64) }), &ctx).unwrap();
        let got = std::fs::read(&disk).unwrap();
        assert_eq!(&got[..raw.len()], &raw[..]);
        assert!(got[raw.len()..raw.len().next_multiple_of(512)].iter().all(|&b| b == 0));
        assert_eq!(v["bytes_written"], raw.len().next_multiple_of(512) as u64);
        // Too small: refused before writing.
        let small = d.join("small.img");
        let e = run(&req(&src, TargetSpec::File { path: small.clone(), size: Some(CHUNK as u64) }), &ctx).unwrap_err();
        assert!(e.to_string().contains("larger") || e.to_string().contains("più grande"), "{e}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn expected_checksum_mismatch_is_refused_before_writing() {
        let d = tmp("hash");
        let raw = data(300_000);
        let src = d.join("a.img");
        std::fs::write(&src, &raw).unwrap();
        let out = d.join("out.img");
        let mut r = req(&src, TargetSpec::File { path: out.clone(), size: None });
        r.expect_hash = Some(format!("sha256:{}", "0".repeat(64)));
        let e = run(&r, &JobCtx::detached(false)).unwrap_err();
        assert!(matches!(e, ForgeError::Mismatch(_)), "{e}");
        assert!(!out.exists(), "nothing may be written");
        r.expect_hash = Some(hex(&Sha256::digest(&raw)));
        let v = run(&r, &JobCtx::detached(false)).unwrap();
        assert_eq!(v["hashes"]["expected"]["matched"], "file");
        // A checksum file next to the image is honoured too.
        std::fs::write(d.join("a.img.sha256"), format!("{}  a.img\n", "1".repeat(64))).unwrap();
        r.expect_hash = None;
        assert!(matches!(run(&r, &JobCtx::detached(false)).unwrap_err(), ForgeError::Mismatch(_)));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn verification_reports_the_first_differing_byte() {
        let d = tmp("verify");
        let raw = data(CHUNK + 5000);
        let src = d.join("a.img");
        std::fs::write(&src, &raw).unwrap();
        let img = Image::open(&src).unwrap();
        let out = d.join("t.img");
        std::fs::write(&out, vec![0u8; CHUNK * 2]).unwrap();
        let dest = Dest::Dev(Arc::new(crate::block::FileDev::open(&out, false).unwrap()));
        let ctx = JobCtx::detached(false);
        let w = dd_write(&img, &dest, 512, &ctx).unwrap();
        verify(&img, &dest, &w, &ctx).unwrap();
        // Corrupt one byte in the second chunk.
        let at = CHUNK as u64 + 1234;
        dest.write_at(at, &[raw[at as usize] ^ 0xFF]).unwrap();
        let e = verify(&img, &dest, &w, &ctx).unwrap_err();
        assert!(e.to_string().contains(&format!("{at}")), "{e}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn cancelling_stops_the_write() {
        let d = tmp("cancel");
        let src = d.join("a.img");
        std::fs::write(&src, data(CHUNK * 8)).unwrap();
        let ctx = JobCtx::detached(false);
        ctx.cancel();
        let e = run(&req(&src, TargetSpec::File { path: d.join("o.img"), size: None }), &ctx).unwrap_err();
        assert!(matches!(e, ForgeError::Cancelled));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn iso_modes_need_an_iso() {
        let d = tmp("mode");
        let src = d.join("a.img");
        std::fs::write(&src, data(100_000)).unwrap();
        let mut r = req(&src, TargetSpec::File { path: d.join("o.img"), size: Some(1 << 20) });
        r.mode = WriteMode::Iso;
        assert!(matches!(run(&r, &JobCtx::detached(false)).unwrap_err(), ForgeError::Invalid(_)));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
