#![no_main]
//! Partition tables (MBR + EBR chain, GPT): the input is a disk image (see
//! `fuzzkit::decode_fuzz_input`); every partition found is probed and opened read-only.
use libfuzzer_sys::fuzz_target;
use lumendisk_core::fuzzkit;

fuzz_target!(|data: &[u8]| {
    if let Some(img) = fuzzkit::decode_fuzz_input(data) {
        fuzzkit::exercise_partitions(img);
    }
});
