//! Secure erase: zeros, random, DoD 5220.22-M (3 / 7 passes), Gutmann (35 passes).
//!
//! Every pass overwrites the whole target, front to back, in large aligned chunks, and is
//! flushed to the medium before the next one starts (otherwise a cache could merge passes).
//! Progress is reported in bytes over *all* passes (size × passes), so the ETA covers the whole
//! job; the last pass is then read back and checked ([`WipeRequest::verify`], default on).
//!
//! **Random passes** use a ChaCha20 keystream keyed from the operating system's CSPRNG: one
//! 32-byte master key per job (never logged, never returned), and per pass a key derived from
//! it. Verifying a random pass regenerates the same keystream instead of keeping a copy.
//!
//! **Patterns**:
//! - `zeros`: 0x00. `random`: one random pass.
//! - `dod3` (DoD 5220.22-M): 0x00, 0xFF, random; the last pass is verified.
//! - `dod7` (DoD 5220.22-M ECE: E, C, E): 0x00, 0xFF, random, random, 0x00, 0xFF, random.
//! - `gutmann`: the 35 passes of Peter Gutmann's "Secure Deletion of Data from Magnetic and
//!   Solid-State Memory" (1996): 4 random, the 27 fixed patterns of the paper's table (passes
//!   5–31, in a random order as the paper prescribes; the order is in the result), 4 random.
//!   The three-byte patterns are laid down continuously from byte 0 of the target.
//!
//! At the end the partition table areas (first and last MiB) are zeroed, so the disk shows up
//! as empty rather than unreadable. A warning is always logged: on SSDs, USB sticks and SD cards
//! wear levelling and over-provisioning keep copies of data the host can't address, so no
//! number of overwrite passes guarantees erasure there: one pass plus the device's own secure
//! erase (when it has one) is the recommendation.
//!
//! Owner: the Raspberry Pi / utilities workstream.

use std::time::Instant;

use rand_chacha::rand_core::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::job::{JobCtx, Phase};
use super::plan::{WipeMethod, WipeRequest};
use super::{ForgeError, ForgeResult};
use crate::block::BlockDev;
use crate::i18n::t;

/// I/O chunk: a multiple of 3 (three-byte patterns stay in phase from chunk to chunk), of 4
/// (keystream words) and of 4096 (sector / page alignment).
pub const CHUNK: usize = 12 << 20;
/// Zeroed at both ends at the end of the job (MBR / GPT and the GPT backup).
const TABLE_AREA: u64 = 1 << 20;

/// What one pass writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    Byte(u8),
    /// Repeats from byte 0 of the target: offset `o` holds `p[o % 3]`.
    Triple([u8; 3]),
    Random,
}

impl Pattern {
    pub fn describe(&self) -> String {
        match self {
            Pattern::Byte(b) => format!("0x{b:02X}"),
            Pattern::Triple([a, b, c]) => format!("0x{a:02X} 0x{b:02X} 0x{c:02X}"),
            Pattern::Random => "random".into(),
        }
    }
}

/// Passes 5–31 of Gutmann's table, in the paper's order.
pub const GUTMANN_FIXED: [Pattern; 27] = [
    Pattern::Byte(0x55),
    Pattern::Byte(0xAA),
    Pattern::Triple([0x92, 0x49, 0x24]),
    Pattern::Triple([0x49, 0x24, 0x92]),
    Pattern::Triple([0x24, 0x92, 0x49]),
    Pattern::Byte(0x00),
    Pattern::Byte(0x11),
    Pattern::Byte(0x22),
    Pattern::Byte(0x33),
    Pattern::Byte(0x44),
    Pattern::Byte(0x55),
    Pattern::Byte(0x66),
    Pattern::Byte(0x77),
    Pattern::Byte(0x88),
    Pattern::Byte(0x99),
    Pattern::Byte(0xAA),
    Pattern::Byte(0xBB),
    Pattern::Byte(0xCC),
    Pattern::Byte(0xDD),
    Pattern::Byte(0xEE),
    Pattern::Byte(0xFF),
    Pattern::Triple([0x92, 0x49, 0x24]),
    Pattern::Triple([0x49, 0x24, 0x92]),
    Pattern::Triple([0x24, 0x92, 0x49]),
    Pattern::Triple([0x6D, 0xB6, 0xDB]),
    Pattern::Triple([0xB6, 0xDB, 0x6D]),
    Pattern::Triple([0xDB, 0x6D, 0xB6]),
];

/// The passes of `method`. `master` keys the shuffle of Gutmann's fixed passes.
pub fn passes(method: WipeMethod, master: &[u8; 32]) -> Vec<Pattern> {
    use Pattern::*;
    match method {
        WipeMethod::Zeros => vec![Byte(0)],
        WipeMethod::Random => vec![Random],
        WipeMethod::Dod3 => vec![Byte(0x00), Byte(0xFF), Random],
        WipeMethod::Dod7 => vec![Byte(0x00), Byte(0xFF), Random, Random, Byte(0x00), Byte(0xFF), Random],
        WipeMethod::Gutmann => {
            let mut fixed = GUTMANN_FIXED.to_vec();
            // Fisher–Yates with the job's keystream (the paper: "performed in a random order").
            let mut rng = ChaCha20Rng::from_seed(derive(master, u32::MAX));
            for i in (1..fixed.len()).rev() {
                let j = (rng.next_u64() % (i as u64 + 1)) as usize;
                fixed.swap(i, j);
            }
            let mut v = vec![Random; 4];
            v.extend(fixed);
            v.extend([Random; 4]);
            v
        }
    }
}

fn method_name(m: WipeMethod) -> &'static str {
    match m {
        WipeMethod::Zeros => "zeros",
        WipeMethod::Random => "random",
        WipeMethod::Dod3 => "dod3",
        WipeMethod::Dod7 => "dod7",
        WipeMethod::Gutmann => "gutmann",
    }
}

/// Key of pass `index` (0-based) derived from the job's master key.
fn derive(master: &[u8; 32], index: u32) -> [u8; 32] {
    Sha256::new().chain_update(b"LumenDisk wipe pass v1").chain_update(master).chain_update(index.to_le_bytes()).finalize().into()
}

/// The bytes a random pass `index` keyed by `master` writes at `offset` (a multiple of 4).
/// Public for tests that check a wiped target from the outside.
pub fn keystream(master: &[u8; 32], index: u32, offset: u64, buf: &mut [u8]) {
    keystream_with(&derive(master, index), offset, buf);
}

fn keystream_with(key: &[u8; 32], offset: u64, buf: &mut [u8]) {
    let mut rng = ChaCha20Rng::from_seed(*key);
    rng.set_word_pos((offset / 4) as u128);
    rng.fill_bytes(buf);
}

/// Below this a chunk is generated on the calling thread.
const PARALLEL_MIN: usize = 1 << 20;

/// [`keystream_with`] split across threads: the keystream is seekable, so each slice (a whole
/// number of 64-byte ChaCha blocks) is generated independently; the bytes are the same as a
/// sequential fill. One core makes ~0.7 GB/s on Apple silicon, slower than a fast SSD.
fn keystream_parallel(key: &[u8; 32], offset: u64, buf: &mut [u8]) {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 8);
    if threads == 1 || buf.len() < PARALLEL_MIN {
        return keystream_with(key, offset, buf);
    }
    let per = buf.len().div_ceil(threads).div_ceil(64) * 64;
    std::thread::scope(|s| {
        let mut at = offset;
        for part in buf.chunks_mut(per) {
            let start = at;
            at += part.len() as u64;
            s.spawn(move || keystream_with(key, start, part));
        }
    });
}

/// Fills chunks of one pass.
struct Filler {
    pattern: Pattern,
    key: Option<[u8; 32]>,
}

impl Filler {
    fn new(pattern: Pattern, master: &[u8; 32], index: u32) -> Self {
        let key = (pattern == Pattern::Random).then(|| derive(master, index));
        Filler { pattern, key }
    }

    /// Fill `buf` with what goes at `offset` (a multiple of 4: chunks are multiples of 12).
    fn fill(&mut self, offset: u64, buf: &mut [u8]) {
        match self.pattern {
            Pattern::Byte(b) => buf.fill(b),
            Pattern::Triple(p) => {
                let phase = (offset % 3) as usize;
                for (i, x) in buf.iter_mut().enumerate() {
                    *x = p[(phase + i) % 3];
                }
            }
            Pattern::Random => {
                if let Some(k) = &self.key {
                    keystream_parallel(k, offset, buf)
                }
            }
        }
    }

    /// A fixed pattern is the same for every full chunk: filled once.
    fn is_fixed(&self) -> bool {
        self.key.is_none()
    }
}

/// A page-aligned byte buffer (raw device I/O is fastest, and DMA-friendly, from aligned memory).
struct AlignedBuf {
    pages: Vec<Page>,
    len: usize,
}

#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct Page([u8; 4096]);

impl AlignedBuf {
    fn new(len: usize) -> Self {
        AlignedBuf { pages: vec![Page([0; 4096]); len.div_ceil(4096)], len }
    }
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: `pages` is a contiguous allocation of `pages.len() * 4096 >= len` initialised
        // bytes, borrowed mutably through `self`.
        unsafe { std::slice::from_raw_parts_mut(self.pages.as_mut_ptr() as *mut u8, self.len) }
    }
}

fn mb_s(bytes: u64, secs: f64) -> f64 {
    if secs > 0.0 {
        ((bytes as f64 / 1e6 / secs) * 10.0).round() / 10.0
    } else {
        0.0
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Run a wipe request: open the target (with every safety check of [`super::target`]) and
/// erase it.
pub fn run(r: &WipeRequest, ctx: &JobCtx) -> ForgeResult<Value> {
    let mut master = [0u8; 32];
    getrandom::getrandom(&mut master).map_err(|e| ForgeError::Io(format!("random generator: {e}")))?;
    run_seeded(r, ctx, master)
}

/// [`run`] with a given master key, so that a test can recompute the random passes with
/// [`keystream`]. The app always goes through [`run`].
#[doc(hidden)]
pub fn run_seeded(r: &WipeRequest, ctx: &JobCtx, master: [u8; 32]) -> ForgeResult<Value> {
    let target = super::target::open_for_write(&r.target, ctx)?;
    let mut v = wipe_dev(target.dev.as_ref(), r.method, r.verify, ctx, &master, CHUNK)?;
    v["target"] = json!(target.path);
    Ok(v)
}

/// Erase `dev` (the whole device).
pub fn wipe_dev(dev: &dyn BlockDev, method: WipeMethod, verify: bool, ctx: &JobCtx, master: &[u8; 32], chunk: usize) -> ForgeResult<Value> {
    let size = dev.size();
    if size == 0 {
        return Err(ForgeError::Invalid(t("la destinazione è vuota (0 byte): niente da cancellare", "the target is empty (0 bytes): nothing to erase")));
    }
    if chunk == 0 || chunk % 12 != 0 {
        return Err(ForgeError::Invalid(format!("wipe chunk {chunk} is not a multiple of 12")));
    }
    let plan = passes(method, master);
    let n = plan.len() as u64;
    let total = size.checked_mul(n).ok_or_else(|| ForgeError::Invalid("target too large".into()))?;

    ctx.warn(t(
        "Su SSD, chiavette USB e schede SD la sovrascrittura (anche a più passaggi) non garantisce la cancellazione: il wear levelling e l'over-provisioning conservano copie dei dati in aree che il computer non può indirizzare. Consigliato: un solo passaggio e, se il dispositivo lo offre, la sua cancellazione sicura integrata (Secure Erase / Sanitize).",
        "On SSDs, USB sticks and SD cards overwriting (with any number of passes) does not guarantee erasure: wear levelling and over-provisioning keep copies of data in areas the computer cannot address. Recommended: a single pass and, when the device offers one, its built-in secure erase (Secure Erase / Sanitize).",
    ));
    ctx.info(format!("{}: {} {}, {} bytes", method_name(method), plan.len(), t("passaggi", "passes"), size));

    ctx.phase_units(Phase::Wiping, total, "bytes");
    let started = Instant::now();
    let mut buf = AlignedBuf::new(chunk.min(usize::try_from(size).unwrap_or(usize::MAX)).max(12));
    let mut pass_results = vec![];
    for (i, pat) in plan.iter().enumerate() {
        ctx.check()?;
        ctx.info(format!("{} {}/{}: {}", t("passaggio", "pass"), i + 1, plan.len(), pat.describe()));
        let t0 = Instant::now();
        let mut filler = Filler::new(*pat, master, i as u32);
        let b = buf.as_mut();
        let mut prepared = false;
        let mut off = 0u64;
        while off < size {
            let len = (chunk as u64).min(size - off) as usize;
            let slice = &mut b[..len];
            if !filler.is_fixed() || !prepared || len < chunk {
                filler.fill(off, slice);
                prepared = len == chunk;
            }
            dev.write_at(off, slice).map_err(|e| ForgeError::Io(format!("{} @{off}: {e}", t("scrittura", "write"))))?;
            off += len as u64;
            ctx.advance(len as u64)?;
        }
        dev.flush()?;
        let secs = t0.elapsed().as_secs_f64();
        pass_results.push(json!({"pass": i + 1, "pattern": pat.describe(), "bytes": size, "secs": round3(secs), "mb_s": mb_s(size, secs)}));
    }
    let write_secs = started.elapsed().as_secs_f64();

    let last = plan.len() - 1;
    let verification = if verify {
        ctx.phase_units(Phase::Verifying, size, "bytes");
        let t0 = Instant::now();
        let mut filler = Filler::new(plan[last], master, last as u32);
        let mut want = AlignedBuf::new(buf.len);
        let got = buf.as_mut();
        let want = want.as_mut();
        let mut off = 0u64;
        while off < size {
            let len = (chunk as u64).min(size - off) as usize;
            filler.fill(off, &mut want[..len]);
            dev.read_at(off, &mut got[..len]).map_err(|e| ForgeError::Io(format!("{} @{off}: {e}", t("lettura", "read"))))?;
            if got[..len] != want[..len] {
                let at = off + got[..len].iter().zip(&want[..len]).position(|(a, b)| a != b).unwrap_or(0) as u64;
                return Err(ForgeError::Mismatch(t(
                    &format!("verifica fallita: il byte {at} non contiene ciò che è stato scritto (supporto difettoso o scrittura non riuscita)"),
                    &format!("verification failed: byte {at} does not hold what was written (faulty media or failed write)"),
                )));
            }
            off += len as u64;
            ctx.advance(len as u64)?;
        }
        let secs = t0.elapsed().as_secs_f64();
        ctx.info(t("verifica dell'ultimo passaggio riuscita", "last pass verified"));
        json!({"ok": true, "pass": last + 1, "bytes": size, "secs": round3(secs), "mb_s": mb_s(size, secs)})
    } else {
        Value::Null
    };

    // The partition table areas (MBR / primary GPT, backup GPT): zeroed unless they already are.
    let cleared = plan[last] != Pattern::Byte(0);
    if cleared {
        ctx.phase_units(Phase::Finishing, 0, "bytes");
        let zeros = vec![0u8; TABLE_AREA.min(size) as usize];
        dev.write_at(0, &zeros)?;
        if size > TABLE_AREA {
            let tail = TABLE_AREA.min(size - TABLE_AREA);
            dev.write_at(size - tail, &zeros[..tail as usize])?;
        }
        dev.flush()?;
        ctx.info(t("tabella delle partizioni azzerata", "partition table cleared"));
    }

    let written = total;
    Ok(json!({
        "method": method_name(method),
        "pass_count": plan.len(),
        "passes": pass_results,
        "bytes": size,
        "bytes_written": written,
        "secs": round3(write_secs),
        "mb_s": mb_s(written, write_secs),
        "verification": verification,
        "partition_table_cleared": cleared,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemDev;
    use std::sync::atomic::{AtomicU64, Ordering};

    const SEED: [u8; 32] = [7; 32];

    fn expected(plan: &[Pattern], size: usize) -> Vec<u8> {
        let last = plan.len() - 1;
        let mut v = vec![0u8; size];
        let mut f = Filler::new(plan[last], &SEED, last as u32);
        f.fill(0, &mut v);
        if plan[last] != Pattern::Byte(0) {
            let a = (TABLE_AREA as usize).min(size);
            v[..a].fill(0);
            if size > TABLE_AREA as usize {
                let tail = (TABLE_AREA as usize).min(size - TABLE_AREA as usize);
                v[size - tail..].fill(0);
            }
        }
        v
    }

    #[test]
    fn every_method_has_the_documented_passes() {
        use Pattern::*;
        assert_eq!(passes(WipeMethod::Zeros, &SEED), vec![Byte(0)]);
        assert_eq!(passes(WipeMethod::Random, &SEED), vec![Random]);
        assert_eq!(passes(WipeMethod::Dod3, &SEED), vec![Byte(0), Byte(0xFF), Random]);
        assert_eq!(passes(WipeMethod::Dod7, &SEED), vec![Byte(0), Byte(0xFF), Random, Random, Byte(0), Byte(0xFF), Random]);
        let g = passes(WipeMethod::Gutmann, &SEED);
        assert_eq!(g.len(), 35);
        assert!(g[..4].iter().chain(&g[31..]).all(|p| *p == Random));
        // Passes 5–31 are exactly the paper's 27 patterns, in some order…
        let mut mid: Vec<String> = g[4..31].iter().map(|p| p.describe()).collect();
        let mut paper: Vec<String> = GUTMANN_FIXED.iter().map(|p| p.describe()).collect();
        assert_ne!(mid, paper, "shuffled");
        mid.sort();
        paper.sort();
        assert_eq!(mid, paper);
        // …a different order for another key, the same for the same key.
        assert_eq!(passes(WipeMethod::Gutmann, &SEED), g);
        assert_ne!(passes(WipeMethod::Gutmann, &[8; 32]), g);
        // The paper's table, spot-checked.
        assert_eq!(GUTMANN_FIXED[2], Triple([0x92, 0x49, 0x24]));
        assert_eq!(GUTMANN_FIXED[24], Triple([0x6D, 0xB6, 0xDB]));
        assert_eq!(GUTMANN_FIXED.iter().filter(|p| matches!(p, Byte(_))).count(), 18);
    }

    #[test]
    fn triple_patterns_stay_in_phase_across_chunks() {
        let mut f = Filler::new(Pattern::Triple([1, 2, 3]), &SEED, 0);
        let mut a = vec![0u8; 24];
        f.fill(0, &mut a[..12]);
        let (x, y) = a.split_at_mut(12);
        let _ = x;
        f.fill(12, y);
        assert_eq!(a, [1, 2, 3].repeat(8));
        let mut odd = vec![0u8; 4];
        f.fill(4, &mut odd);
        assert_eq!(odd, [2, 3, 1, 2]);
    }

    #[test]
    fn the_keystream_is_reproducible_and_seekable_and_differs_per_pass() {
        let mut f = Filler::new(Pattern::Random, &SEED, 2);
        let mut stream = vec![0u8; 96];
        f.fill(0, &mut stream[..48]);
        f.fill(48, &mut stream[48..]);
        let mut whole = vec![0u8; 96];
        keystream(&SEED, 2, 0, &mut whole);
        assert_eq!(stream, whole);
        let mut tail = vec![0u8; 40];
        keystream(&SEED, 2, 56, &mut tail);
        assert_eq!(tail, whole[56..]);
        let mut other = vec![0u8; 96];
        keystream(&SEED, 3, 0, &mut other);
        assert_ne!(other, whole);
        assert!(whole.iter().any(|&b| b != 0));

        // Generated in parallel slices: byte for byte the sequential stream.
        let key = derive(&SEED, 1);
        let len = (5 << 20) + 12 * 7;
        let mut seq = vec![0u8; len];
        let mut par = vec![0u8; len];
        keystream_with(&key, 12 << 20, &mut seq);
        keystream_parallel(&key, 12 << 20, &mut par);
        assert!(seq == par);
    }

    #[test]
    fn each_method_leaves_the_last_pattern_verified_and_the_tables_cleared() {
        // 3 MiB + 7 bytes: chunks don't divide it and a tail of both areas is partial.
        let size = (3 << 20) + 7;
        for m in [WipeMethod::Zeros, WipeMethod::Random, WipeMethod::Dod3, WipeMethod::Dod7, WipeMethod::Gutmann] {
            let dev = MemDev::from_vec(vec![0x5Au8; size]);
            let ctx = JobCtx::detached(false);
            let v = wipe_dev(&dev, m, true, &ctx, &SEED, 12 * 4096).unwrap();
            let plan = passes(m, &SEED);
            assert_eq!(dev.snapshot(), expected(&plan, size), "{m:?}");
            assert_eq!(v["pass_count"], plan.len());
            assert_eq!(v["bytes_written"], size as u64 * plan.len() as u64);
            assert_eq!(v["verification"]["ok"], true);
            assert_eq!(v["partition_table_cleared"], m != WipeMethod::Zeros);
            let logs = ctx.snapshot(0).logs;
            assert!(logs.iter().any(|l| l.msg.contains("wear levelling")), "{m:?}: warning logged");
            let phases = ctx.snapshot(0).phases;
            assert!(phases.contains(&Phase::Wiping) && phases.contains(&Phase::Verifying), "{phases:?}");
        }
    }

    #[test]
    fn small_devices_are_wiped_whole() {
        for size in [1usize, 11, 13, 4096, 1 << 20, (1 << 20) + 1] {
            let dev = MemDev::from_vec(vec![0x5Au8; size]);
            wipe_dev(&dev, WipeMethod::Dod3, true, &JobCtx::detached(false), &SEED, CHUNK).unwrap();
            assert!(dev.snapshot().iter().all(|&b| b == 0) || size > (2 << 20), "{size}: random region fully cleared");
            let plan = passes(WipeMethod::Dod3, &SEED);
            assert_eq!(dev.snapshot(), expected(&plan, size), "{size}");
        }
        let empty = MemDev::new(0);
        assert!(matches!(wipe_dev(&empty, WipeMethod::Zeros, true, &JobCtx::detached(false), &SEED, CHUNK), Err(ForgeError::Invalid(_))));
    }

    /// Cancels the job after a number of writes.
    struct CancelAfter {
        inner: MemDev,
        writes: AtomicU64,
        after: u64,
        ctx: JobCtx,
    }
    impl BlockDev for CancelAfter {
        fn read_at(&self, o: u64, b: &mut [u8]) -> std::io::Result<()> {
            self.inner.read_at(o, b)
        }
        fn write_at(&self, o: u64, b: &[u8]) -> std::io::Result<()> {
            if self.writes.fetch_add(1, Ordering::SeqCst) + 1 == self.after {
                self.ctx.cancel();
            }
            self.inner.write_at(o, b)
        }
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn read_only(&self) -> bool {
            false
        }
    }

    #[test]
    fn cancel_stops_mid_pass() {
        let chunk = 12 * 4096;
        let ctx = JobCtx::detached(false);
        // 10 chunks per pass; cancel during the 2nd pass (write 14).
        let dev = CancelAfter { inner: MemDev::from_vec(vec![0x5A; chunk * 10]), writes: AtomicU64::new(0), after: 14, ctx: ctx.clone() };
        let r = wipe_dev(&dev, WipeMethod::Dod3, true, &ctx, &SEED, chunk);
        assert!(matches!(r, Err(ForgeError::Cancelled)), "{:?}", r.err());
        assert_eq!(dev.writes.load(Ordering::SeqCst), 14, "no write after the cancel");
        let snap = dev.inner.snapshot();
        assert!(snap[..chunk * 4].iter().all(|&b| b == 0xFF), "pass 2 had begun");
        assert!(snap[chunk * 4..].iter().all(|&b| b == 0x00), "the rest still holds pass 1");
    }

    /// Corrupts what it stores at one offset, like failing media.
    struct BadByte(MemDev, u64);
    impl BlockDev for BadByte {
        fn read_at(&self, o: u64, b: &mut [u8]) -> std::io::Result<()> {
            self.0.read_at(o, b)?;
            if (o..o + b.len() as u64).contains(&self.1) {
                b[(self.1 - o) as usize] ^= 1;
            }
            Ok(())
        }
        fn write_at(&self, o: u64, b: &[u8]) -> std::io::Result<()> {
            self.0.write_at(o, b)
        }
        fn size(&self) -> u64 {
            self.0.size()
        }
        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn read_only(&self) -> bool {
            false
        }
    }

    #[test]
    fn verification_catches_media_that_does_not_keep_the_data() {
        let dev = BadByte(MemDev::new(1 << 20), 123_457);
        let e = wipe_dev(&dev, WipeMethod::Random, true, &JobCtx::detached(false), &SEED, 12 * 4096).unwrap_err();
        assert!(matches!(&e, ForgeError::Mismatch(m) if m.contains("123457")), "{e}");
        // Without verification it isn't noticed.
        let dev = BadByte(MemDev::new(1 << 20), 123_457);
        let v = wipe_dev(&dev, WipeMethod::Random, false, &JobCtx::detached(false), &SEED, 12 * 4096).unwrap();
        assert!(v["verification"].is_null());
    }

    #[test]
    fn requests_through_the_target_layer_on_a_file() {
        let dir = std::env::temp_dir().join(format!("ld-wipe-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.img");
        std::fs::write(&path, vec![0xA5u8; 5 << 20]).unwrap();
        let req: WipeRequest = serde_json::from_value(json!({"target": {"kind": "file", "path": path, "size": null}, "method": "dod3"})).unwrap();
        assert!(req.verify, "verify defaults to true");
        let ctx = JobCtx::detached(false);
        let v = run_seeded(&req, &ctx, SEED).unwrap();
        assert_eq!(v["bytes"], 5u64 << 20);
        let plan = passes(WipeMethod::Dod3, &SEED);
        assert_eq!(std::fs::read(&path).unwrap(), expected(&plan, 5 << 20));
        assert_eq!(v["passes"].as_array().unwrap().len(), 3);
        assert!(v["mb_s"].as_f64().unwrap() > 0.0);
        // A fresh random master key per job: two random wipes differ.
        run(&WipeRequest { method: WipeMethod::Random, ..req.clone() }, &ctx).unwrap();
        let a = std::fs::read(&path).unwrap();
        run(&WipeRequest { method: WipeMethod::Random, ..req }, &ctx).unwrap();
        assert_ne!(a, std::fs::read(&path).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
