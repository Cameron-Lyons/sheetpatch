//! Fixtures and invariants shared by the bounded fuzz targets.

#[path = "../../tests/common/mod.rs"]
mod common;

pub mod parts;

use sheetpatch::{CellContent, CellRange, CellRef, CellValue, Workbook, WorksheetView};
use std::collections::BTreeSet;
use std::mem;

pub const MAX_INPUT: usize = 1 << 20;
const SHEET: &str = "xl/worksheets/sheet1.xml";
const OPAQUE: &str = "custom/opaque.bin";

/// The independent test writer puts arbitrary XML into a valid package, so
/// mutation reaches worksheet parsing without first solving ZIP checksums.
pub fn fixture(worksheet: &[u8], opaque: &[u8]) -> Vec<u8> {
    fixture_with_parts(worksheet, opaque, &[])
}

/// Rebuilds ZIP headers and checksums around mutated package XML. Replacing
/// one part leaves its relationships and the other parts valid, allowing
/// mutation to reach the selected parser directly.
pub fn fixture_with_parts<'a>(
    worksheet: &'a [u8],
    opaque: &'a [u8],
    replacements: &[(&'a str, &'a [u8])],
) -> Vec<u8> {
    let mut parts: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>"),
        ("_rels/.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='office' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ("xl/workbook.xml", b"<workbook xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' xmlns:r='http://schemas.openxmlformats.org/officeDocument/2006/relationships'><sheets><sheet name='Data' sheetId='1' r:id='sheet'/></sheets></workbook>"),
        ("xl/_rels/workbook.xml.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='sheet' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet' Target='worksheets/sheet1.xml'/></Relationships>"),
        (SHEET, worksheet),
        (OPAQUE, opaque),
    ];
    for &(name, xml) in replacements {
        if let Some(part) = parts.iter_mut().find(|part| part.0 == name) {
            part.1 = xml;
        } else {
            parts.push((name, xml));
        }
    }
    common::stored_archive(&parts)
}

fn compare_read(
    actual: sheetpatch::Result<(CellRef, CellContent)>,
    expected: sheetpatch::Result<(CellRef, CellContent)>,
) {
    match (actual, expected) {
        (Ok(actual), Ok(expected)) => assert_eq!(actual, expected),
        (Err(actual), Err(expected)) => {
            assert_eq!(mem::discriminant(&actual), mem::discriminant(&expected));
            assert_eq!(actual.to_string(), expected.to_string());
        }
        pair => panic!("streamed and individual reads disagree: {pair:?}"),
    }
}

/// Whether a read refused a cell's style index, the one refusal that an edit
/// leaving the cell unchanged keeps; any other is replaced by a change.
pub fn refuses_style_index(error: &sheetpatch::Error) -> bool {
    matches!(error, sheetpatch::Error::InvalidWorkbook(message)
        if message.ends_with("invalid cell style index"))
}

/// The first bounded prefix includes successes and failures, so one malformed
/// value cannot hide later cells. Clone the cursor after a prefix and verify
/// that replay starts at the same address, even after an error.
fn check_view(view: &WorksheetView<'_>) {
    let addresses: Vec<_> = view.cells().take(64).collect();
    for (&address, result) in addresses.iter().zip(view.cell_contents()) {
        compare_read(
            result,
            view.get_cell_at(address).map(|content| (address, content)),
        );
    }
    let range = addresses
        .first()
        .zip(addresses.last())
        .map(|(&first, &last)| CellRange::new(first, last));
    let single = addresses.first().copied().map(CellRange::from);
    if let Some(range) = range {
        let mut dense = view.range_contents(range);
        for address in range.cells().take(64) {
            compare_read(
                dense
                    .next()
                    .expect("every requested address must be visited"),
                view.get_cell_at(address).map(|content| (address, content)),
            );
        }
    }
    for range in [None, range, single] {
        let expected: Vec<_> = view
            .cells()
            .filter(|&address| range.is_none_or(|range| range.contains(address)))
            .take(64)
            .collect();
        let split = expected.len() / 2;
        let mut cursor = view.cell_contents_in(range).take(64);
        for &address in &expected[..split] {
            compare_read(
                cursor.next().expect("every selected cell must be visited"),
                view.get_cell_at(address).map(|content| (address, content)),
            );
        }
        let checkpoint = cursor.clone();
        for mut tail in [cursor, checkpoint] {
            for &address in &expected[split..] {
                compare_read(
                    tail.next()
                        .expect("a cloned iterator must retain its position"),
                    view.get_cell_at(address).map(|content| (address, content)),
                );
            }
            assert!(
                tail.next().is_none(),
                "an iterator visited an unselected cell"
            );
        }
    }
}

/// Reads are bounded independently of a hostile worksheet's cell count. An
/// invalid or unsupported input is an ordinary outcome, never a fuzz failure.
pub fn exercise(mut book: Workbook) {
    // Callers may already have pending edits. This successful serialization is
    // their baseline, so later comparisons do not mistake those edits for loss.
    let Ok(original) = book.to_bytes() else {
        return;
    };
    let sheets: Vec<_> = book
        .sheets()
        .iter()
        .take(4)
        .map(|sheet| (sheet.name().to_owned(), sheet.path().to_owned()))
        .collect();
    let mut changed = BTreeSet::new();
    for (name, path) in sheets {
        if let Ok(view) = book.prepare_sheet(&name) {
            check_view(&view);
        }
        let mut edited = None;
        // The table/header and formula seeds can protect A1. A second target
        // lets those inputs still exercise a successful edit and save.
        for address in ["A1", "B2"] {
            let before = book.to_bytes().ok();
            let refused = book
                .get_cell(&name, address)
                .err()
                .map(|error| error.to_string());
            if book.set_cell(&name, address, true).is_ok() {
                edited = Some((address, before, refused));
                break;
            }
            if let Some(before) = before {
                assert_eq!(book.to_bytes().unwrap(), before, "failed edits are atomic");
            }
        }
        let Some((address, before, refused)) = edited else {
            continue;
        };
        changed.insert(path);
        for (output, preserve_gaps) in [(book.to_bytes(), true), (book.to_bytes_compact(), false)] {
            // Layout ambiguity and opaque compaction gaps may refuse a save.
            if let Ok(saved) = output {
                let reopened =
                    Workbook::from_bytes(saved.clone()).expect("successful saves must reopen");
                check_preserved(&original, &saved, &changed, preserve_gaps);
                match reopened.get_cell(&name, address) {
                    Ok(cell) => assert_eq!(cell.value, CellValue::Bool(true)),
                    // An edit that changes nothing succeeds even where reads
                    // refuse the cell's style index, which it leaves as it was.
                    Err(error) => {
                        assert!(
                            refuses_style_index(&error),
                            "a changed cell must read: {error}"
                        );
                        assert_eq!(Some(error.to_string()), refused, "a changed cell must read");
                        assert_eq!(book.to_bytes().ok(), before, "and nothing changed");
                        // So the cell already held `true`: a change is refused.
                        let mut changed = Workbook::from_bytes(saved).unwrap();
                        assert!(
                            changed
                                .set_cell(&name, address, false)
                                .is_err_and(|error| refuses_style_index(&error)),
                            "a change to the cell must be refused"
                        );
                    }
                }
            }
        }
    }
}

/// An independent descriptor check, including unsigned descriptors whose CRC
/// happens to equal the optional signature. Only accepted archives reach it.
fn local_end(bytes: &[u8], entry: &common::ZipEntry<'_>) -> usize {
    let end = entry.local_offset + entry.local.len();
    if common::u16_at(entry.central, 8) & 8 == 0 {
        return end;
    }
    let sizes = &entry.central[16..28];
    let unsigned = bytes.get(end..end + 12) == Some(sizes);
    let signed = bytes.get(end..end + 4) == Some(b"PK\x07\x08")
        && bytes.get(end + 4..end + 16) == Some(sizes);
    end + match (unsigned, signed) {
        (true, false) => 12,
        (false, true) => 16,
        _ => panic!("accepted ZIP entry has an invalid or ambiguous data descriptor"),
    }
}

fn gaps<'a>(bytes: &'a [u8], entries: &[common::ZipEntry<'_>]) -> Vec<&'a [u8]> {
    let mut physical: Vec<_> = entries.iter().collect();
    physical.sort_unstable_by_key(|entry| entry.local_offset);
    let mut cursor = 0;
    let mut gaps = Vec::with_capacity(entries.len() + 1);
    for entry in physical {
        gaps.push(&bytes[cursor..entry.local_offset]);
        cursor = local_end(bytes, entry);
    }
    gaps.push(&bytes[cursor..common::central_directory_offset(bytes)]);
    gaps
}

/// Compare every untouched member without inflating opaque or malformed parts.
/// The package must have opened successfully before using this independent ZIP
/// inspector. Offset fields may change; all other central metadata must survive.
pub fn check_preserved(
    original: &[u8],
    saved: &[u8],
    changed: &BTreeSet<String>,
    preserve_gaps: bool,
) {
    let originals = common::zip_entries(original);
    let replacements = common::zip_entries(saved);
    assert_eq!(
        originals.len(),
        replacements.len(),
        "member count must not change"
    );
    assert_eq!(
        common::archive_comment(original),
        common::archive_comment(saved)
    );
    for (before, after) in originals.iter().zip(&replacements) {
        assert_eq!(before.name, after.name, "directory order must not change");
        if changed.contains(&before.name) {
            continue;
        }
        assert_eq!(
            &original[before.local_offset..local_end(original, before)],
            &saved[after.local_offset..local_end(saved, after)],
            "untouched local records, compressed streams and descriptors must be identical"
        );
        assert_eq!(
            before.central_before_offset(),
            after.central_before_offset()
        );
        assert_eq!(before.central_after_offset(), after.central_after_offset());
    }
    if preserve_gaps {
        assert_eq!(
            gaps(original, &originals),
            gaps(saved, &replacements),
            "opaque gaps must be preserved"
        );
    }
}

pub fn check_worksheet_preservation(original: &[u8], saved: &[u8]) {
    check_preserved(original, saved, &BTreeSet::from([SHEET.to_owned()]), true);
}

pub fn text_round_trip(text: &str) {
    if text.encode_utf16().count() > 32_767 {
        return;
    }
    let original = fixture(b"<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><sheetData/></worksheet>", b"opaque\0payload");
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    let address = CellRef::try_new(1, 1).unwrap();
    book.set_cell_at("Data", address, text).unwrap();
    // A valid edit may still produce a ZIP layout refused as ambiguous.
    let Ok(saved) = book.to_bytes() else { return };
    check_worksheet_preservation(&original, &saved);
    let mut reopened = Workbook::from_bytes(saved.clone()).unwrap();
    assert_eq!(
        reopened.get_cell_at("Data", address).unwrap().value,
        CellValue::from(text)
    );
    reopened.set_cell_at("Data", address, text).unwrap();
    assert_eq!(
        reopened.to_bytes().unwrap(),
        saved,
        "rewriting current text must preserve bytes"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_that_change_nothing_keep_a_refused_read() {
        // `true` is already A1's value, so its edit changes nothing and
        // succeeds, while reads still refuse the cell's style index.
        let sheet = b"<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><sheetData><row r='1'><c r='A1' t='b' s='x'><v>1</v></c></row></sheetData></worksheet>";
        let book = Workbook::from_bytes(fixture(sheet, b"opaque")).unwrap();
        // So `exercise` edits A1 rather than falling back to B2, and reaches
        // the oracles' refused-read branch.
        let mut edited = Workbook::from_bytes(fixture(sheet, b"opaque")).unwrap();
        edited.set_cell("Data", "A1", true).unwrap();
        assert!(!edited.has_changes());
        assert!(
            edited
                .get_cell("Data", "A1")
                .is_err_and(|error| refuses_style_index(&error))
        );
        exercise(book);
    }

    #[test]
    fn edits_replace_values_that_reads_refuse() {
        // Reads refuse the payload `x`, which an edit replaces, so the edit
        // must leave a readable `true`: the oracles allow a refused read only
        // for an unreadable style index.
        let sheet = b"<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><sheetData><row r='1'><c r='A1' t='b'><v>x</v></c></row></sheetData></worksheet>";
        let book = Workbook::from_bytes(fixture(sheet, b"opaque")).unwrap();
        assert!(
            book.get_cell("Data", "A1")
                .is_err_and(|error| !refuses_style_index(&error))
        );
        // So `exercise` edits A1 rather than falling back to B2.
        let mut edited = Workbook::from_bytes(fixture(sheet, b"opaque")).unwrap();
        edited.set_cell("Data", "A1", true).unwrap();
        assert_eq!(
            edited.get_cell("Data", "A1").unwrap().value,
            CellValue::Bool(true)
        );
        exercise(book);
    }
}
