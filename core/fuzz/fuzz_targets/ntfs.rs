#![no_main]
//! ntfs driver: the input is a whole volume image (see `fuzzkit::decode_fuzz_input`), mounted
//! read-only and read-write, walked, modified and repaired by `fuzzkit::exercise`.
use libfuzzer_sys::fuzz_target;
use lumendisk_core::fuzzkit::{fuzz_one, Driver};

fuzz_target!(|data: &[u8]| fuzz_one(Driver::Ntfs, data));
