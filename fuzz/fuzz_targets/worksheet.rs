#![no_main]

use libfuzzer_sys::fuzz_target;
use sheetpatch::Workbook;

fn exercise_xml(xml: &[u8]) {
    let original = sheetpatch_fuzz::fixture(xml, b"unchanged arbitrary part");
    let Ok(mut book) = Workbook::from_bytes(original.clone()) else {
        return;
    };
    let refused = book
        .get_cell("Data", "B2")
        .err()
        .map(|error| error.to_string());
    if book.set_cell("Data", "B2", "fuzzed <&> _xD800_").is_ok()
        && let Ok(saved) = book.to_bytes()
    {
        sheetpatch_fuzz::check_worksheet_preservation(&original, &saved);
        let reopened = Workbook::from_bytes(saved.clone()).expect("successful saves must reopen");
        match reopened.get_cell("Data", "B2") {
            Ok(cell) => assert_eq!(cell.value.as_text(), Some("fuzzed <&> _xD800_")),
            // An edit that changes nothing succeeds even where reads refuse
            // the cell's style index, which it leaves as it was.
            Err(error) => {
                assert!(
                    sheetpatch_fuzz::refuses_style_index(&error),
                    "a changed cell must read: {error}"
                );
                assert_eq!(Some(error.to_string()), refused, "a changed cell must read");
                let unedited = Workbook::from_bytes(original.clone()).unwrap().to_bytes();
                assert_eq!(Some(saved), unedited.ok(), "and nothing changed");
                // So the cell already held that text: a change is refused.
                let mut changed = Workbook::from_bytes(original.clone()).unwrap();
                assert!(
                    changed
                        .set_cell("Data", "B2", "changed")
                        .is_err_and(|error| sheetpatch_fuzz::refuses_style_index(&error)),
                    "a change to the cell must be refused"
                );
            }
        }
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
