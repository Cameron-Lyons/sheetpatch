#![no_main]

use libfuzzer_sys::fuzz_target;
use sheetpatch::Workbook;
use sheetpatch_fuzz::parts;

fn exercise(xml: &[u8]) {
    if let Ok(book) = Workbook::from_bytes(parts::shared_strings(xml)) {
        sheetpatch_fuzz::exercise(book);
    }
}

fuzz_target!(|xml: &[u8]| {
    if xml.len() > sheetpatch_fuzz::MAX_INPUT {
        return;
    }
    exercise(xml);
    // Mutating individual entries need not preserve the document wrapper.
    let mut wrapped =
        b"<sst xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'>".to_vec();
    wrapped.extend_from_slice(xml);
    wrapped.extend_from_slice(b"</sst>");
    exercise(&wrapped);
});
