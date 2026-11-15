//! Corrupted-image suite: volumes are untrusted input.
//!
//! For every driver a valid, populated image is built in memory (our mkfs + files written
//! through the driver, see `lumendisk_core::fuzzkit`), then thousands of seeded mutations are
//! applied, biased to metadata (superblocks / boot sectors, group descriptors, bitmaps, inode
//! tables, extent and htree blocks, MFT records and index blocks, FATs, directory entries,
//! exFAT entry sets...), plus random bit flips and truncations. Each mutant is mounted
//! read-only and read-write, walked completely and modified (see `fuzzkit::exercise`).
//!
//! Every case must finish without a panic, within a time limit (a hang is a failure) and
//! without a runaway allocation (a counting global allocator caps it).
//!
//! Knobs:
//! * `LUMENDISK_CORRUPT_CASES=N`  number of mutants (default 16000; partition tables get half that)
//! * `LUMENDISK_CORRUPT_SEED=S`   start of the case-number range (explore other mutants)
//! * `LUMENDISK_CORRUPT_ONLY=name` only seeds whose name contains `name` (ext4, ntfs, fat12...)
//! * `LUMENDISK_CORRUPT_VERBOSE=1` print every case before running it
//! * `LUMENDISK_CORRUPT_BUDGET_SECS=S` stop starting new mutants after S seconds (default 60;
//!   an unoptimized `cargo test` build gets through fewer cases, the count run is printed)
//! * `LUMENDISK_REPLAY=<driver>:<image>` replay one saved mutant (`replay_image`)
//!
//! Failing mutants are written to `target/tmp/corrupt-failures/` for reproduction
//! (`cargo test --profile robust --test corrupted_images replay_image` with LUMENDISK_REPLAY).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lumendisk_core::fuzzkit::{self, Budget, Driver, Seed};

// ───────────────────────────── allocation cap ─────────────────────────────

/// A single allocation above this is a failure: it is what an on-disk size fed to
/// `Vec::with_capacity` / `vec![0; n]` looks like, and in the app it would abort the process.
/// The test allocator serves it lazily from virtual memory (so the run can go on and report
/// the case) up to [`ABORT_CAP`], and aborts above that.
const HARD_CAP: usize = 1 << 30;
const ABORT_CAP: usize = 64 << 30;
/// Peak live bytes one case may add on its worker thread, on top of the image itself: what a
/// driver legitimately caches (FAT, bitmaps, a directory, a 64 KiB read) is far below this.
/// Anything above means an on-disk size sized a buffer.
const SOFT_CAP: isize = 48 << 20;

struct Counting;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
    static HUGE: Cell<usize> = const { Cell::new(0) };
    static PANIC_MSG: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Name of the case running on this thread (printed if it hits the abort cap).
    static CASE: Cell<[u8; 200]> = const { Cell::new([0; 200]) };
}
static BIGGEST: AtomicUsize = AtomicUsize::new(0);

fn set_case(s: &str) {
    let mut b = [0u8; 200];
    let n = s.len().min(199);
    b[..n].copy_from_slice(&s.as_bytes()[..n]);
    let _ = CASE.try_with(|c| c.set(b));
}

/// Allocation-free report from inside the allocator, right before the process aborts.
fn abort_cap_hit() {
    let msg = b"\ncorrupted_images: allocation over the abort cap in case: ";
    let case = CASE.try_with(|c| c.get()).unwrap_or([0; 200]);
    let n = case.iter().position(|&b| b == 0).unwrap_or(200);
    // SAFETY: plain write(2) of valid buffers.
    unsafe {
        libc::write(2, msg.as_ptr() as *const _, msg.len());
        libc::write(2, case.as_ptr() as *const _, n);
        libc::write(2, b"\n".as_ptr() as *const _, 1);
    }
}

fn account(delta: isize) {
    let _ = LIVE.try_with(|l| {
        let v = l.get() + delta;
        l.set(v);
        let _ = PEAK.try_with(|p| {
            if v > p.get() {
                p.set(v)
            }
        });
    });
}

/// Oversized request: note it and hand out lazily-mapped (zero) pages.
unsafe fn huge(size: usize) -> *mut u8 {
    let _ = HUGE.try_with(|h| h.set(h.get().max(size)));
    if size > ABORT_CAP {
        abort_cap_hit();
        return std::ptr::null_mut();
    }
    let p = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0);
    if p == libc::MAP_FAILED {
        abort_cap_hit();
        return std::ptr::null_mut();
    }
    p as *mut u8
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        BIGGEST.fetch_max(l.size(), Ordering::Relaxed);
        if l.size() > HARD_CAP {
            return huge(l.size());
        }
        let p = System.alloc(l);
        if !p.is_null() {
            account(l.size() as isize);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        BIGGEST.fetch_max(l.size(), Ordering::Relaxed);
        if l.size() > HARD_CAP {
            return huge(l.size());
        }
        let p = System.alloc_zeroed(l);
        if !p.is_null() {
            account(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if l.size() > HARD_CAP {
            libc::munmap(p as *mut _, l.size());
            return;
        }
        account(-(l.size() as isize));
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        BIGGEST.fetch_max(new, Ordering::Relaxed);
        if new > HARD_CAP || l.size() > HARD_CAP {
            let nl = Layout::from_size_align_unchecked(new, l.align());
            let q = self.alloc(nl);
            if !q.is_null() {
                std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
                self.dealloc(p, l);
            }
            return q;
        }
        let q = System.realloc(p, l, new);
        if !q.is_null() {
            account(new as isize - l.size() as isize);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

// ───────────────────────────── runner ─────────────────────────────

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn quiet_panics() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| info.payload().downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let _ = PANIC_MSG.try_with(|m| *m.borrow_mut() = Some(format!("{msg} at {loc}")));
    }));
}

fn fail_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("corrupt-failures")
}

#[derive(Debug)]
struct Failure {
    what: String,
}

/// One case on the current thread: Err(reason) on panic or allocation blow-up.
fn run_case(driver: Driver, img: Vec<u8>, budget: &Budget) -> Result<fuzzkit::Stats, String> {
    let cap = SOFT_CAP + 2 * img.len() as isize;
    let base = LIVE.with(|l| l.get());
    PEAK.with(|p| p.set(base));
    HUGE.with(|h| h.set(0));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fuzzkit::exercise(driver, img, budget)));
    let peak = PEAK.with(|p| p.get()) - base;
    let huge = HUGE.with(|h| h.get());
    match r {
        _ if huge > 0 => Err(format!("single allocation of {} MiB", huge >> 20)),
        Err(_) => Err(format!("panic: {}", PANIC_MSG.with(|m| m.borrow_mut().take()).unwrap_or_default())),
        Ok(_) if peak > cap => Err(format!("allocated {} MiB at peak", peak >> 20)),
        Ok(st) => Ok(st),
    }
}

static RAN: AtomicU64 = AtomicU64::new(0);
static COUNT_RO: AtomicU64 = AtomicU64::new(0);
static COUNT_RW: AtomicU64 = AtomicU64::new(0);
static COUNT_ENTRIES: AtomicU64 = AtomicU64::new(0);
static COUNT_ERRS: AtomicU64 = AtomicU64::new(0);

/// Run `cases` mutants spread over `seeds` on all cores with a per-case time limit.
fn run_mutants(seeds: Arc<Vec<Seed>>, first: u64, cases: u64, limit: Duration) -> Vec<Failure> {
    // Unoptimized builds are ~10x slower: stop starting new cases after the time budget
    // (the count actually run is reported).
    let budget = Duration::from_secs(env_u64("LUMENDISK_CORRUPT_BUDGET_SECS", 60));
    let start = Instant::now();
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    let next = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(Mutex::new(Vec::<Failure>::new()));
    let slots: Arc<Vec<Mutex<Option<(u64, Instant)>>>> = Arc::new((0..workers).map(|_| Mutex::new(None)).collect());
    let done = Arc::new(AtomicUsize::new(0));
    let verbose = std::env::var_os("LUMENDISK_CORRUPT_VERBOSE").is_some();
    for w in 0..workers {
        let (seeds, next, failures, slots, done) = (seeds.clone(), next.clone(), failures.clone(), slots.clone(), done.clone());
        std::thread::Builder::new()
            .name(format!("corrupt-{w}"))
            .stack_size(16 << 20)
            .spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= cases || start.elapsed() > budget {
                        break;
                    }
                    RAN.fetch_add(1, Ordering::Relaxed);
                    let seed = &seeds[(i % seeds.len() as u64) as usize];
                    let case = first + i / seeds.len() as u64;
                    let (img, desc) = fuzzkit::mutate(seed, case);
                    if verbose {
                        eprintln!("case {} #{case}: {desc}", seed.name);
                    }
                    set_case(&format!("{} #{case} [{desc}]", seed.name));
                    *slots[w].lock().unwrap() = Some((i, Instant::now()));
                    let r = run_case(seed.driver, img, &Budget::default());
                    *slots[w].lock().unwrap() = None;
                    if let Ok(st) = &r {
                        COUNT_RO.fetch_add(st.opened_ro as u64, Ordering::Relaxed);
                        COUNT_RW.fetch_add(st.opened_rw as u64, Ordering::Relaxed);
                        COUNT_ENTRIES.fetch_add(st.entries as u64, Ordering::Relaxed);
                        COUNT_ERRS.fetch_add((st.errors > 0) as u64, Ordering::Relaxed);
                    }
                    if let Err(e) = r {
                        let _ = std::fs::create_dir_all(fail_dir());
                        let path = fail_dir().join(format!("{}-{case}.img", seed.name));
                        let _ = std::fs::write(&path, fuzzkit::mutate(seed, case).0);
                        failures.lock().unwrap().push(Failure { what: format!("{} case {case} [{desc}]: {e} (image: {})", seed.name, path.display()) });
                    }
                }
                done.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
    }
    // Watchdog: a case over the limit is a hang. Its thread can't be killed; stop waiting
    // for it and report.
    let mut hung = vec![];
    loop {
        std::thread::sleep(Duration::from_millis(50));
        for (w, s) in slots.iter().enumerate() {
            if let Some((i, t)) = *s.lock().unwrap() {
                if t.elapsed() > limit && !hung.contains(&w) {
                    hung.push(w);
                    let seed = &seeds[(i % seeds.len() as u64) as usize];
                    let case = first + i / seeds.len() as u64;
                    let (img, desc) = fuzzkit::mutate(seed, case);
                    let _ = std::fs::create_dir_all(fail_dir());
                    let path = fail_dir().join(format!("{}-{case}-hang.img", seed.name));
                    let _ = std::fs::write(&path, &img);
                    failures.lock().unwrap().push(Failure { what: format!("{} case {case} [{desc}]: HANG (> {limit:?}) (image: {})", seed.name, path.display()) });
                }
            }
        }
        if done.load(Ordering::SeqCst) + hung.len() >= workers {
            break;
        }
    }
    let mut f = failures.lock().unwrap();
    std::mem::take(&mut *f)
}

fn report(failures: &[Failure]) {
    if failures.is_empty() {
        return;
    }
    for f in failures.iter().take(60) {
        eprintln!("  ✗ {}", f.what);
    }
    panic!("{} corrupted-image case(s) failed (mutants saved in {})", failures.len(), fail_dir().display());
}

// ───────────────────────────── tests ─────────────────────────────

/// The harness itself: every seed mounts, is walked completely without a single error and
/// survives the read-write exercise.
#[test]
fn seeds_are_valid_and_fully_walked() {
    quiet_panics();
    for s in fuzzkit::seeds() {
        let st = fuzzkit::exercise(s.driver, s.image.clone(), &Budget::default());
        println!("seed {:8} {:>6} KiB, {:>5} metadata sectors: {st:?}", s.name, s.image.len() >> 10, s.hot.len());
        assert!(st.opened_ro && st.opened_rw, "{}: did not open", s.name);
        assert!(st.entries > 150, "{}: only {} entries walked", s.name, st.entries);
        assert_eq!(st.errors, 0, "{}: errors while walking a valid volume", s.name);
        assert!(s.hot.len() > 8, "{}: no metadata found", s.name);
    }
}

#[test]
fn corrupted_images_never_crash_hang_or_blow_up() {
    quiet_panics();
    let t0 = Instant::now();
    let only = std::env::var("LUMENDISK_CORRUPT_ONLY").ok();
    let mut seeds = fuzzkit::seeds();
    if let Some(o) = &only {
        seeds.retain(|s| s.name.contains(o.as_str()));
    }
    assert!(!seeds.is_empty());
    let cases = env_u64("LUMENDISK_CORRUPT_CASES", 16000);
    let first = env_u64("LUMENDISK_CORRUPT_SEED", 0);
    let nseeds = seeds.len();
    let drivers: std::collections::HashSet<_> = seeds.iter().map(|s| s.driver).collect();
    let build = t0.elapsed();
    let failures = run_mutants(Arc::new(seeds), first, cases, Duration::from_secs(20));
    let ran = RAN.load(Ordering::Relaxed);
    if ran < cases {
        println!("  (time budget reached: {ran} of {cases} cases run; set LUMENDISK_CORRUPT_BUDGET_SECS to run them all)");
    }
    println!(
        "corrupted images: {ran} cases, {} filesystems ({nseeds} seed images), {:.1} s (seeds built in {:.1} s), biggest single allocation {} KiB",
        drivers.len(),
        t0.elapsed().as_secs_f64(),
        build.as_secs_f64(),
        BIGGEST.load(Ordering::Relaxed) >> 10
    );
    println!(
        "  mutants mounted read-only {}, read-write {}, with errors while walking {}, {} entries walked",
        COUNT_RO.load(Ordering::Relaxed),
        COUNT_RW.load(Ordering::Relaxed),
        COUNT_ERRS.load(Ordering::Relaxed),
        COUNT_ENTRIES.load(Ordering::Relaxed)
    );
    report(&failures);
}

/// Crash inputs found by fuzzing, stored compactly in `core/tests/corrupt_regressions/`:
/// `<driver>-*.ldp` (patch against a reproducible seed, `fuzzkit::make_patch`) or
/// `<driver>-*.ldz` (zero-run-encoded fuzz input). Create them with
/// `cargo run --example fuzz_seeds -- core/tests/corrupt_regressions --regression <driver> <crash file>`.
#[test]
fn fuzz_regressions() {
    quiet_panics();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("corrupt_regressions");
    let mut failures = vec![];
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut paths: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            let Some(driver) = name.split('-').next().and_then(Driver::from_name) else { continue };
            let enc = std::fs::read(&p).unwrap();
            let img = if name.ends_with(".ldp") {
                fuzzkit::apply_patch(&enc).unwrap_or_else(|| panic!("{name}: bad patch")).1
            } else if name.ends_with(".ldz") {
                let input = fuzzkit::sparse_decode(&enc).unwrap_or_else(|| panic!("{name}: bad encoding"));
                fuzzkit::decode_fuzz_input(&input).unwrap_or_else(|| panic!("{name}: bad fuzz input"))
            } else {
                continue;
            };
            n += 1;
            if let Err(e) = run_with_timeout(driver, img, Duration::from_secs(20)) {
                failures.push(Failure { what: format!("{name}: {e}") });
            }
        }
    }
    println!("{n} fuzz regression inputs replayed");
    report(&failures);
}

// ───────────────────────────── hand-built regressions ─────────────────────────────
//
// One per bug found by reading the drivers or by the suites above, built on the reproducible
// seeds so each corruption hits exactly the structure in question.

fn rd(b: &[u8], o: usize, w: usize) -> u64 {
    (0..w).fold(0u64, |v, i| v | (b[o + i] as u64) << (8 * i))
}

fn wr(b: &mut [u8], o: usize, w: usize, v: u64) {
    for i in 0..w {
        b[o + i] = (v >> (8 * i)) as u8;
    }
}

struct Ext4Seed {
    img: Vec<u8>,
    bs: usize,
}

impl Ext4Seed {
    fn new(name: &str) -> (Self, Box<dyn lumendisk_core::vfs::FileSystem>) {
        let img = fuzzkit::seed_by_name(name, false).unwrap().image;
        let dev: Arc<dyn lumendisk_core::block::BlockDev> = Arc::new(lumendisk_core::block::MemDev::from_vec(img.clone()));
        let fs = Driver::Ext4.open(dev, true).unwrap();
        let bs = 1024usize << rd(&img, 1024 + 0x18, 4);
        (Ext4Seed { img, bs }, fs)
    }
}

/// Inode offset, data blocks and inode number of a path in an ext4 seed.
fn ext4_node(name: &str, path: &[&str]) -> (Ext4Seed, u64, Vec<u64>, u64) {
    let (s, _) = Ext4Seed::new(name);
    let dev = Arc::new(lumendisk_core::block::MemDev::from_vec(s.img.clone()));
    let mut fs = lumendisk_core::vfs::ext4_fs::Ext4Fs::open(dev, true).unwrap();
    use lumendisk_core::vfs::FileSystem;
    let mut ino = fs.root_ino();
    for c in path {
        ino = fs.lookup(ino, c).unwrap();
    }
    let off = fs.debug_inode_offset(ino).unwrap();
    let blocks = fs.debug_file_blocks(ino).unwrap_or_default();
    (s, off, blocks, ino)
}

fn must_survive(what: &str, driver: Driver, img: Vec<u8>) {
    if let Err(e) = run_with_timeout(driver, img, Duration::from_secs(20)) {
        panic!("{what}: {e}");
    }
}

#[test]
fn ext4_extent_tree_fan_out_and_huge_offsets() {
    quiet_panics();
    let (s, off, _, ino) = ext4_node("ext4", &["frag-a.bin"]);
    let (_, _, big, _) = ext4_node("ext4", &["big.bin"]);
    let ib = off as usize + 0x28;
    let per = (s.bs - 12) / 12;
    // 1) A depth-5 tree whose every index entry at each level points to the same node: only
    //    5 distinct blocks, but 4 * per^4 node visits and 4 * per^5 extents (hang, then OOM).
    //    Each node is now visited once.
    let mut img = s.img.clone();
    let node = |k: usize| big[k] as usize;
    let header = |img: &mut Vec<u8>, at: usize, n: usize, max: usize, depth: u64| {
        wr(img, at, 2, 0xF30A);
        wr(img, at + 2, 2, n as u64);
        wr(img, at + 4, 2, max as u64);
        wr(img, at + 6, 2, depth);
    };
    header(&mut img, ib, 4, 4, 5);
    for k in 0..4 {
        wr(&mut img, ib + 12 + 12 * k + 4, 4, node(4) as u64);
        wr(&mut img, ib + 12 + 12 * k + 8, 2, 0);
    }
    for d in (1..=4).rev() {
        let at = node(d) * s.bs;
        header(&mut img, at, per, per, d as u64);
        for k in 0..per {
            wr(&mut img, at + 12 + 12 * k, 4, k as u64);
            wr(&mut img, at + 12 + 12 * k + 4, 4, node(d - 1) as u64);
            wr(&mut img, at + 12 + 12 * k + 8, 2, 0);
        }
    }
    let at = node(0) * s.bs;
    header(&mut img, at, per, per, 0);
    for k in 0..per {
        wr(&mut img, at + 12 + 12 * k, 4, k as u64);
        wr(&mut img, at + 12 + 12 * k + 4, 2, 1);
        wr(&mut img, at + 12 + 12 * k + 8, 4, node(0) as u64);
    }
    must_survive("extent tree fan-out", Driver::Ext4, img);
    // 2) i_size near 2^48 and a read at 4 TiB + 5 KiB: the block number wrapped to 5 in u32,
    //    so an extent of the start of the file was "found" past the offset (subtract overflow).
    let mut img = s.img.clone();
    wr(&mut img, off as usize + 0x6C, 4, 0xFFFF);
    let dev: Arc<dyn lumendisk_core::block::BlockDev> = Arc::new(lumendisk_core::block::MemDev::from_vec(img.clone()));
    let mut fs = Driver::Ext4.open(dev, false).unwrap();
    let r = fs.read(ino, (1 << 42) + 5 * 1024, 4096).unwrap();
    assert!(r.iter().all(|&b| b == 0), "data mapped beyond 2^32 blocks");
    let _ = fs.setattr(ino, &lumendisk_core::vfs::SetAttr { size: Some((1 << 42) + 5000), ..Default::default() });
    drop(fs);
    must_survive("huge offsets", Driver::Ext4, img);
}

#[test]
fn ext4_impossible_sizes_and_geometry() {
    quiet_panics();
    // A slow symlink claiming 1 TiB: readlink used to allocate i_size bytes.
    let (s, off, _, _) = ext4_node("ext4", &["link-long"]);
    let mut img = s.img.clone();
    wr(&mut img, off as usize + 0x6C, 4, 0x100);
    must_survive("1 TiB symlink", Driver::Ext4, img);
    // 64-bit descriptors larger than a block: desc_per_block == 0 (division by zero).
    let mut img = s.img.clone();
    let inc = rd(&img, 1024 + 0x60, 4);
    wr(&mut img, 1024 + 0x60, 4, inc | 0x80);
    wr(&mut img, 1024 + 0xFE, 2, 8192);
    must_survive("descriptor size > block", Driver::Ext4, img);
    // Blocks per group beyond one bitmap block / inodes per group overflowing u32 counts.
    let mut img = s.img.clone();
    wr(&mut img, 1024 + 0x20, 4, 0xFFFF_FFFF);
    must_survive("blocks per group", Driver::Ext4, img);
    let mut img = s.img.clone();
    wr(&mut img, 1024 + 0x28, 4, 0x8000_0000);
    must_survive("inodes per group", Driver::Ext4, img);
}

#[test]
fn ext2_indirect_block_cycle() {
    quiet_panics();
    // big.bin (150 KB, 1 KiB blocks) uses the single indirect block. Point the triple-indirect
    // slot at it and fill it with its own block number: the walk used to expand that into
    // 256^3 mappings (hundreds of MB, seconds).
    let (s, off, _, _) = ext4_node("ext2", &["big.bin"]);
    let ib = off as usize + 0x28;
    let ind = rd(&s.img, ib + 48, 4);
    assert!(ind != 0);
    let mut img = s.img.clone();
    wr(&mut img, ib + 56, 4, ind);
    for k in 0..s.bs / 4 {
        wr(&mut img, ind as usize * s.bs + 4 * k, 4, ind);
    }
    must_survive("indirect cycle", Driver::Ext4, img);
}

#[test]
fn ext4_htree_root_count_overflow() {
    quiet_panics();
    // dx_root count 0xFFFF in an htree directory: linearize_dir (first write into the
    // directory) indexed past the block.
    let (s, _, blocks, ino) = ext4_node("ext4", &["many"]);
    let mut img = s.img.clone();
    let root = blocks[0] as usize * s.bs;
    assert_eq!(img[root + 0x1D], 8, "seed layout changed: expected an htree root");
    wr(&mut img, root + 0x22, 2, 0xFFFF);
    wr(&mut img, root + 0x1E, 1, 0xFF);
    let dev: Arc<dyn lumendisk_core::block::BlockDev> = Arc::new(lumendisk_core::block::MemDev::from_vec(img.clone()));
    let mut fs = Driver::Ext4.open(dev, false).unwrap();
    let _ = fs.create(ino, "new-entry", 0o644, 0, 0);
    let _ = fs.readdir(ino);
    must_survive("htree count", Driver::Ext4, img);
}

#[test]
fn ext4_orphan_chain_cycle_is_processed_once() {
    quiet_panics();
    let (s, off, _, ino) = ext4_node("ext4", &["hello.txt"]);
    let mut img = s.img.clone();
    wr(&mut img, 1024 + 0xE8, 4, ino); // s_last_orphan
    wr(&mut img, off as usize + 0x14, 4, ino); // i_dtime = next orphan = itself
    let dev: Arc<dyn lumendisk_core::block::BlockDev> = Arc::new(lumendisk_core::block::MemDev::from_vec(img));
    let mut fs = Driver::Ext4.open(dev, false).unwrap();
    let t = Instant::now();
    let rep = fs.repair().unwrap();
    assert!(rep.contains("Processed 1 orphan"), "{rep}");
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn ext4_journal_replay_is_all_or_nothing() {
    quiet_panics();
    use lumendisk_core::vfs::FileSystem;
    // A transaction whose second block targets a block beyond the volume must not be
    // half-applied (the first, valid block used to be written before the error).
    let img = fuzzkit::seed_by_name("ext4", false).unwrap().image;
    let dev = Arc::new(lumendisk_core::block::MemDev::from_vec(img));
    let (target, before) = {
        let mut fs = lumendisk_core::vfs::ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
        let f = fs.lookup(2, "hello.txt").unwrap();
        let b = fs.debug_file_blocks(f).unwrap()[0];
        let bs = fs.statfs().unwrap().block_size as usize;
        let blocks = rd(&dev.snapshot(), 1024 + 4, 4);
        fs.debug_journal_transaction(&[(b, vec![0xAB; bs]), (blocks + 5, vec![0xCD; bs])]).unwrap();
        (b as usize * bs, dev.snapshot()[b as usize * bs..b as usize * bs + bs].to_vec())
    };
    let mut fs = lumendisk_core::vfs::ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
    assert!(fs.repair().is_err());
    assert_eq!(&dev.snapshot()[target..target + before.len()], &before[..], "journal partially replayed");
}

#[test]
fn ext4_dirent_naming_a_reserved_inode_is_not_unlinked() {
    quiet_panics();
    use lumendisk_core::vfs::FileSystem;
    // hello.txt's directory entry rewritten to name the journal inode (8): unlink used to free
    // the journal.
    let (s, _, root_blocks, ino) = ext4_node("ext4", &[]);
    let _ = ino;
    let mut img = s.img.clone();
    let rb = root_blocks[0] as usize * s.bs;
    let pos = img[rb..rb + s.bs].windows(9).position(|w| w == b"hello.txt").expect("dirent") + rb - 8;
    wr(&mut img, pos, 4, 8);
    let dev = Arc::new(lumendisk_core::block::MemDev::from_vec(img));
    let mut fs = lumendisk_core::vfs::ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
    assert!(matches!(fs.unlink(2, "hello.txt"), Err(lumendisk_core::vfs::FsError::Corrupt(_))));
    drop(fs);
    let mut fs = lumendisk_core::vfs::ext4_fs::Ext4Fs::open(dev, false).unwrap();
    let jo = fs.debug_inode_offset(8).unwrap() as usize;
    let snap = fs.debug_file_blocks(8).unwrap();
    assert!(snap.len() >= 1024, "journal inode was released");
    let _ = jo;
}

/// Rewrite the entry set of `name` in the root directory of an exFAT image (checksum fixed).
fn exfat_patch_root_entry(img: &mut [u8], name: &str, f: impl FnOnce(&mut [u8], u64, usize)) {
    let bps = 1usize << img[108];
    let cs = bps << img[109];
    let heap = rd(img, 88, 4) as usize * bps;
    let root = rd(img, 96, 4);
    let root_off = heap + (root as usize - 2) * cs;
    let mut i = root_off;
    while i < root_off + cs {
        if img[i] == 0x85 {
            let sc = img[i + 1] as usize;
            let end = i + (1 + sc) * 32;
            let nl = img[i + 35] as usize;
            let n: Vec<u16> = (0..nl).map(|k| rd(img, i + 64 + (k / 15) * 32 + 2 + (k % 15) * 2, 2) as u16).collect();
            if String::from_utf16_lossy(&n) == name {
                f(&mut img[i..end], root, cs);
                let mut c: u16 = 0;
                for (k, &b) in img[i..end].iter().enumerate() {
                    if k != 2 && k != 3 {
                        c = c.rotate_right(1).wrapping_add(b as u16);
                    }
                }
                wr(img, i + 2, 2, c as u64);
                return;
            }
            i = end;
        } else {
            i += 32;
        }
    }
    panic!("seed layout changed: {name} not found in the exFAT root directory");
}

#[test]
fn exfat_directory_loop_does_not_hang_the_doctor() {
    quiet_panics();
    // The "docs" subdirectory's stream entry pointed at the root directory's cluster: the
    // Doctor's usage scan walked the loop until its 1M-directory guard (minutes).
    let mut img = fuzzkit::seed_by_name("exfat", false).unwrap().image;
    exfat_patch_root_entry(&mut img, "docs", |set, root, cs| {
        set[33] = 0x01; // allocated, FAT chain (like the root)
        wr(set, 52, 4, root);
        wr(set, 56, 8, cs as u64);
        wr(set, 40, 8, cs as u64);
    });
    must_survive("exFAT directory loop", Driver::ExFat, img);
}

#[test]
fn exfat_impossible_stream_sizes() {
    quiet_panics();
    // DataLength = 2^64 - 1: allocation arithmetic (size.div_ceil(cs) * cs) overflowed in
    // getattr / read / unlink; contiguous streams starting past the heap.
    for (name, first, nofat) in [("big.bin", None, true), ("big.bin", None, false), ("hello.txt", Some(0xFFFF_FFF0u32), true)] {
        let mut img = fuzzkit::seed_by_name("exfat", false).unwrap().image;
        exfat_patch_root_entry(&mut img, name, |set, _, _| {
            wr(set, 56, 8, u64::MAX);
            wr(set, 40, 8, u64::MAX);
            set[33] = if nofat { 0x03 } else { 0x01 };
            if let Some(f) = first {
                wr(set, 52, 4, f as u64);
            }
        });
        must_survive("exFAT stream sizes", Driver::ExFat, img);
    }
}

/// Seeds are reproducible (regression patches depend on it).
#[test]
fn seeds_are_reproducible() {
    for d in Driver::ALL {
        let a = fuzzkit::seeds_for(d, true);
        let b = fuzzkit::seeds_for(d, true);
        for (x, y) in a.iter().zip(&b) {
            assert!(x.image == y.image, "{} differs between two builds", x.name);
        }
    }
}

fn run_with_timeout(driver: Driver, img: Vec<u8>, limit: Duration) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(move || {
            let _ = tx.send(run_case(driver, img, &Budget::default()).map(|_| ()));
        })
        .unwrap();
    rx.recv_timeout(limit).unwrap_or_else(|_| Err(format!("HANG (> {limit:?})")))
}

/// Debug helper: `LUMENDISK_REPLAY=<driver>:<image file>` runs one saved mutant in the
/// foreground (no timeout), e.g. to `sample` a hang or see a panic's backtrace.
#[test]
fn replay_image() {
    let Ok(spec) = std::env::var("LUMENDISK_REPLAY") else { return };
    let (d, path) = spec.split_once(':').expect("driver:path");
    let driver = Driver::from_name(d).expect("driver name");
    let img = std::fs::read(path).unwrap();
    let st = fuzzkit::exercise(driver, img, &Budget::default());
    println!("{st:?}");
}

/// MBR (incl. EBR chains) and GPT tables, mutated: the parser never panics, never loops on
/// a cyclic EBR chain and never returns a partition outside the device.
#[test]
fn partition_tables_never_crash() {
    quiet_panics();
    let cases = env_u64("LUMENDISK_CORRUPT_CASES", 16000) / 2;
    let seeds = [fuzzkit::partition_seed(false), fuzzkit::partition_seed(true)];
    assert_eq!(fuzzkit::exercise_partitions(seeds[0].clone()), 4);
    assert_eq!(fuzzkit::exercise_partitions(seeds[1].clone()), 3);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut failures = vec![];
        for case in 0..cases {
            let img = fuzzkit::mutate_partitions(&seeds[(case % 2) as usize], case);
            if std::panic::catch_unwind(|| fuzzkit::exercise_partitions(img)).is_err() {
                failures.push(Failure { what: format!("partition case {case}: {}", PANIC_MSG.with(|m| m.borrow_mut().take()).unwrap_or_default()) });
            }
        }
        let _ = tx.send(failures);
    });
    let failures = rx.recv_timeout(Duration::from_secs(120)).expect("partition cases hung");
    println!("partition tables: {cases} mutants");
    report(&failures);
}
