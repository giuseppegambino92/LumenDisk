//! Throughput of the Shadow overlay on a big copy (one 200 MB file + 2000 small files), with
//! and without the persistent journal. In-process (no NFS), stage on the local SSD.
//!
//!   cargo test -p lumendisk-core --release --test shadow_bench -- --ignored --nocapture
mod common;
use common::*;

use std::sync::Arc;
use std::time::Instant;

use lumendisk_core::block::{BlockDev, FileDev};
use lumendisk_core::vfs::shadow::ShadowOverlay;
use lumendisk_core::vfs::{ext4_fs, testutil, FileSystem, FsType, SetAttr, Timestamp};

fn lower(dir: &std::path::Path) -> Box<dyn FileSystem> {
    let img = dir.join("bench.img");
    if !img.exists() {
        make_sparse(&img, 1024);
        let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
        ext4_fs::format(dev, FsType::Ext4, "BENCH").unwrap();
    }
    let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(&img, false).unwrap());
    Box::new(ext4_fs::Ext4Fs::open(dev, false).unwrap())
}

/// The copy as the macOS NFS client issues it: 64 KiB writes, then create + write + setattr
/// per small file.
fn copy(fs: &mut dyn FileSystem, big: &[u8], small: &[Vec<u8>]) -> f64 {
    let t0 = Instant::now();
    let r = fs.root_ino();
    let f = fs.create(r, "big.bin", 0o644, 501, 20).unwrap();
    for (i, c) in big.chunks(64 * 1024).enumerate() {
        fs.write(f, (i * 64 * 1024) as u64, c).unwrap();
    }
    let d = fs.mkdir(r, "small", 0o755, 501, 20).unwrap();
    for (i, data) in small.iter().enumerate() {
        let f = fs.create(d, &format!("file{i:04}.txt"), 0o644, 501, 20).unwrap();
        fs.write(f, 0, data).unwrap();
        fs.setattr(f, &SetAttr { mtime: Some(Timestamp { secs: 1_700_000_000, nsecs: 0 }), ..Default::default() }).unwrap();
    }
    fs.sync().unwrap();
    t0.elapsed().as_secs_f64()
}

fn data() -> (Vec<u8>, Vec<Vec<u8>>) {
    let big = testutil::pattern(200 << 20, 3);
    let small = (0..2000).map(|i| testutil::pattern(1000 + (i * 37) % 7000, i as u32)).collect();
    (big, small)
}

#[test]
#[ignore]
fn bench_shadow_in_memory_overlay() {
    let dir = workdir("shadow_bench_mem");
    let (big, small) = data();
    let mut best = f64::MAX;
    for _ in 0..3 {
        let mut s = ShadowOverlay::new(lower(&dir), dir.join("x.stage")).unwrap();
        best = best.min(copy(&mut s, &big, &small));
    }
    println!("    in-memory overlay: 200 MB + 2000 files in {best:.2} s ({:.0} MB/s)", 214.0 / best);
}

#[test]
#[ignore]
fn bench_shadow_persistent_session() {
    use lumendisk_core::vfs::shadow_store::Identity;
    let dir = workdir("shadow_bench_persist");
    let (big, small) = data();
    let mut best = f64::MAX;
    for i in 0..3 {
        let id = Identity { kind: "local".into(), fs_type: "EXT4".into(), uuid: "bench".into(), ..Default::default() };
        let mut s = ShadowOverlay::open_session(lower(&dir), dir.join(format!("s{i}")), id).unwrap();
        best = best.min(copy(&mut s, &big, &small));
    }
    println!("    persistent session: 200 MB + 2000 files in {best:.2} s ({:.0} MB/s)", 214.0 / best);
}
