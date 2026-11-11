#![no_main]

use libfuzzer_sys::fuzz_target;
use sheetpatch::Workbook;
use sheetpatch_fuzz::parts::{self, Part};

fn exercise(part: &Part, xml: &[u8]) {
    if let Ok(book) = Workbook::from_bytes(parts::package_part(part, xml)) {
        sheetpatch_fuzz::exercise(book);
    }
}

fuzz_target!(|input: &[u8]| {
    if input.len() > sheetpatch_fuzz::MAX_INPUT {
        return;
    }
    let Some((&selection, xml)) = input.split_first() else {
        return;
    };
    let part = &parts::PACKAGE_PARTS[usize::from(selection) % parts::PACKAGE_PARTS.len()];
    exercise(part, xml);
    let mut wrapped = part.open.to_vec();
    wrapped.extend_from_slice(xml);
    wrapped.extend_from_slice(part.close);
    exercise(part, &wrapped);
});
