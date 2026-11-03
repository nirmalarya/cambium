#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| cambium_core::fuzzcheck::path(data));
