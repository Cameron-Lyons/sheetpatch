#![no_main]

use libfuzzer_sys::fuzz_target;
use sheetpatch::Workbook;

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > sheetpatch_fuzz::MAX_INPUT {
        return;
    }
    if let Ok(book) = Workbook::from_bytes(bytes.to_vec()) {
        assert_eq!(
            book.to_bytes().unwrap(),
            bytes,
            "unchanged archives must be identical"
        );
        sheetpatch_fuzz::exercise(book);
    }
});
