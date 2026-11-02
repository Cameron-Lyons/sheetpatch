#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() <= 131_068
        && let Ok(text) = std::str::from_utf8(bytes)
    {
        sheetpatch_fuzz::text_round_trip(text);
    }
});
