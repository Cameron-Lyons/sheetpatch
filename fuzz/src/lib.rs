//! Fixtures and invariants shared by the bounded fuzz targets.

#[path = "../../tests/common/mod.rs"]
mod common;

use sheetpatch::{CellRef, CellValue, Workbook};
use std::collections::BTreeSet;

pub const MAX_INPUT: usize = 1 << 20;
const SHEET: &str = "xl/worksheets/sheet1.xml";
const OPAQUE: &str = "custom/opaque.bin";

/// The independent test writer puts arbitrary XML into a valid package, so
/// mutation reaches worksheet parsing without first solving ZIP checksums.
pub fn fixture(worksheet: &[u8], opaque: &[u8]) -> Vec<u8> {
    common::stored_archive(&[
        ("[Content_Types].xml", b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>"),
        ("_rels/.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='office' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ("xl/workbook.xml", b"<workbook xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' xmlns:r='http://schemas.openxmlformats.org/officeDocument/2006/relationships'><sheets><sheet name='Data' sheetId='1' r:id='sheet'/></sheets></workbook>"),
        ("xl/_rels/workbook.xml.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='sheet' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet' Target='worksheets/sheet1.xml'/></Relationships>"),
        (SHEET, worksheet),
        (OPAQUE, opaque),
    ])
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
            for address in view.cells().take(64) {
                let _ = view.get_cell_at(address);
            }
        }
        let before = book.to_bytes().ok();
        if book.set_cell(&name, "A1", true).is_err() {
            if let Some(before) = before {
                assert_eq!(book.to_bytes().unwrap(), before, "failed edits are atomic");
            }
            continue;
        }
        changed.insert(path);
        for (output, preserve_gaps) in [(book.to_bytes(), true), (book.to_bytes_compact(), false)] {
            // Layout ambiguity and opaque compaction gaps may refuse a save.
            if let Ok(saved) = output {
                let reopened =
                    Workbook::from_bytes(saved.clone()).expect("successful saves must reopen");
                check_preserved(&original, &saved, &changed, preserve_gaps);
                assert_eq!(
                    reopened.get_cell(&name, "A1").unwrap().value,
                    CellValue::Bool(true)
                );
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
