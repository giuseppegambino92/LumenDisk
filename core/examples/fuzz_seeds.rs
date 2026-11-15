//! Seed corpora for the libFuzzer targets in `core/fuzz` (used by `scripts/fuzz.sh`).
//!
//! ```sh
//! cargo run --release --example fuzz_seeds -- <corpus dir>
//! cargo run --release --example fuzz_seeds -- <corpus dir> --import <driver> <image>...
//! ```
//!
//! The first form writes valid, populated images made by our own mkfs (small trees) to
//! `<corpus dir>/<driver>/`. `--import` adds images made by other tools (e.g. `mkfs.ext4 -d`,
//! `mkfs.vfat` + `mcopy`, `mkntfs` + `ntfscp`, `mkfs.exfat`), as they are and once more after
//! LumenDisk wrote the standard test tree into them.
//!
//! Fuzz inputs are `u32 LE device size in sectors` + the image without its trailing zeros
//! (see `lumendisk_core::fuzzkit::decode_fuzz_input`).

use std::path::{Path, PathBuf};

use lumendisk_core::fuzzkit::{self, Driver};

fn write(dir: &Path, driver: Driver, name: &str, image: &[u8]) {
    let d = dir.join(driver.name());
    std::fs::create_dir_all(&d).unwrap();
    let input = fuzzkit::encode_fuzz_input(image);
    println!("{:>6} {:<40} {:>8} KiB input ({} KiB image)", driver.name(), name, input.len() >> 10, image.len() >> 10);
    std::fs::write(d.join(name), input).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(out) = args.first().map(PathBuf::from) else {
        eprintln!("usage: fuzz_seeds <corpus dir> [--import <driver> <image>...]");
        std::process::exit(2);
    };
    if args.get(1).map(String::as_str) == Some("--regression") {
        // fuzz_seeds <dir> --regression <driver> <crash input>... : compact regression files
        // for core/tests/corrupt_regressions (replayed by `cargo test --test corrupted_images`).
        let driver = args.get(2).and_then(|d| Driver::from_name(d)).expect("driver: ext4 | ntfs | fat | exfat");
        std::fs::create_dir_all(&out).unwrap();
        for p in &args[3..] {
            let input = std::fs::read(p).unwrap_or_else(|e| panic!("{p}: {e}"));
            let (ext, bytes) = fuzzkit::regression_file(driver, &input).expect("not a fuzz input");
            let h = input.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
            let path = out.join(format!("{}-fuzz-{:08x}.{ext}", driver.name(), h as u32));
            std::fs::write(&path, &bytes).unwrap();
            println!("{} ({} bytes)", path.display(), bytes.len());
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--import") {
        let driver = args.get(2).and_then(|d| Driver::from_name(d)).expect("driver: ext4 | ntfs | fat | exfat");
        for p in &args[3..] {
            let img = std::fs::read(p).unwrap_or_else(|e| panic!("{p}: {e}"));
            let base = Path::new(p).file_name().unwrap().to_string_lossy().into_owned();
            write(&out, driver, &format!("real-{base}"), &img);
            match fuzzkit::populate_image(driver, img) {
                Some(pop) => write(&out, driver, &format!("real-{base}-populated"), &pop),
                None => eprintln!("{base}: LumenDisk could not populate it (kept as is)"),
            }
        }
        return;
    }
    for d in Driver::ALL {
        for s in fuzzkit::seeds_for(d, true) {
            write(&out, d, &format!("own-{}", s.name), &s.image);
        }
    }
}
