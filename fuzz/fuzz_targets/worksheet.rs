#![no_main]

use libfuzzer_sys::fuzz_target;
use sheetpatch::Workbook;

fn exercise_xml(xml: &[u8]) {
    let original = sheetpatch_fuzz::fixture(xml, b"unchanged arbitrary part");
    let Ok(mut book) = Workbook::from_bytes(original.clone()) else {
        return;
    };
    if book.set_cell("Data", "B2", "fuzzed <&> _xD800_").is_ok()
        && let Ok(saved) = book.to_bytes()
    {
        sheetpatch_fuzz::check_worksheet_preservation(&original, &saved);
        let reopened = Workbook::from_bytes(saved).expect("successful saves must reopen");
        assert_eq!(
            reopened.get_cell("Data", "B2").unwrap().value.as_text(),
            Some("fuzzed <&> _xD800_")
        );
    }
    sheetpatch_fuzz::exercise(book);
}

fuzz_target!(|xml: &[u8]| {
    if xml.len() > sheetpatch_fuzz::MAX_INPUT {
        return;
    }
    exercise_xml(xml);
    // Also treat the input as row/cell fragments inside a valid outer
    // worksheet, so mutation need not preserve its namespace and root tags.
    let mut wrapped =
        b"<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><sheetData>"
            .to_vec();
    wrapped.extend_from_slice(xml);
    wrapped.extend_from_slice(b"</sheetData></worksheet>");
    exercise_xml(&wrapped);
});
