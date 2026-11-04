#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| cce_storage::fuzz_target("manifest", data));
