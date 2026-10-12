//! Regressions for worksheet text decoding, value validation, and new cells.
use std::{collections::BTreeMap, io::Read};

use flate2::read::DeflateDecoder;
use sheetpatch::{CellEdit, CellValue, Error, Workbook};

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const SHEET_PATH: &str = "xl/worksheets/sheet1.xml";

fn stored_archive(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut directory = Vec::new();
    for &(name, content) in parts {
        let offset = archive.len() as u32;
        let crc = crc32fast::hash(content);
        archive.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        for value in [20u16, 0, 0, 0, 0] {
            archive.extend_from_slice(&value.to_le_bytes());
        }
        for value in [crc, content.len() as u32, content.len() as u32] {
            archive.extend_from_slice(&value.to_le_bytes());
        }
        archive.extend_from_slice(&(name.len() as u16).to_le_bytes());
        archive.extend_from_slice(&0u16.to_le_bytes());
        archive.extend_from_slice(name.as_bytes());
        archive.extend_from_slice(content);
        directory.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        for value in [20u16, 20, 0, 0, 0, 0] {
            directory.extend_from_slice(&value.to_le_bytes());
        }
        for value in [crc, content.len() as u32, content.len() as u32] {
            directory.extend_from_slice(&value.to_le_bytes());
        }
        directory.extend_from_slice(&(name.len() as u16).to_le_bytes());
        for _ in 0..4 {
            directory.extend_from_slice(&0u16.to_le_bytes());
        }
        directory.extend_from_slice(&0u32.to_le_bytes());
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name.as_bytes());
    }
    let offset = archive.len() as u32;
    archive.extend_from_slice(&directory);
    archive.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    for value in [0u16, 0, parts.len() as u16, parts.len() as u16] {
        archive.extend_from_slice(&value.to_le_bytes());
    }
    archive.extend_from_slice(&(directory.len() as u32).to_le_bytes());
    archive.extend_from_slice(&offset.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive
}

fn payloads(archive: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let u16_at = |at| u16::from_le_bytes(archive[at..at + 2].try_into().unwrap()) as usize;
    let u32_at = |at| u32::from_le_bytes(archive[at..at + 4].try_into().unwrap()) as usize;
    let end = archive.len() - 22;
    assert_eq!(&archive[end..end + 4], b"PK\x05\x06");
    let mut at = u32_at(end + 16);
    let mut parts = BTreeMap::new();
    for _ in 0..u16_at(end + 10) {
        assert_eq!(&archive[at..at + 4], b"PK\x01\x02");
        let name_len = u16_at(at + 28);
        let name = String::from_utf8(archive[at + 46..at + 46 + name_len].to_vec()).unwrap();
        let local = u32_at(at + 42);
        let start = local + 30 + u16_at(local + 26) + u16_at(local + 28);
        let compressed = &archive[start..start + u32_at(at + 20)];
        let bytes = match u16_at(at + 10) {
            0 => compressed.to_vec(),
            8 => {
                let mut bytes = Vec::new();
                DeflateDecoder::new(compressed)
                    .read_to_end(&mut bytes)
                    .unwrap();
                bytes
            }
            method => panic!("unexpected compression: {method}"),
        };
        parts.insert(name, bytes);
        at += 46 + name_len + u16_at(at + 30) + u16_at(at + 32);
    }
    parts
}

/// A one-sheet workbook named `Data`, optionally with a shared-string table.
fn workbook(body: &str, shared_strings: Option<&str>) -> Workbook {
    let sheet = format!("<worksheet xmlns='{MAIN_NS}'>{body}</worksheet>");
    let workbook = format!(
        "<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheets><sheet name='Data' sheetId='1' r:id='data'/></sheets></workbook>"
    );
    let strings_relationship = if shared_strings.is_some() {
        format!(
            "<Relationship Id='strings' Type='{OFFICE_NS}/sharedStrings' Target='sharedStrings.xml'/>"
        )
    } else {
        String::new()
    };
    let relationships = format!(
        "<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='data' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet1.xml'/>{strings_relationship}</Relationships>"
    );
    let strings = shared_strings
        .map(|items| format!("<sst xmlns='{MAIN_NS}'>{items}</sst>"))
        .unwrap_or_default();
    let mut parts: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='rels' ContentType='application/vnd.openxmlformats-package.relationships+xml'/><Default Extension='xml' ContentType='application/xml'/></Types>"),
        ("_rels/.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='office' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", relationships.as_bytes()),
        (SHEET_PATH, sheet.as_bytes()),
    ];
    if shared_strings.is_some() {
        parts.push(("xl/sharedStrings.xml", strings.as_bytes()));
    }
    Workbook::from_bytes(stored_archive(&parts)).unwrap()
}

fn row(cells: &str) -> String {
    format!("<sheetData><row r='1'>{cells}</row></sheetData>")
}

fn value(book: &Workbook, address: &str) -> CellValue {
    book.get_cell("Data", address).unwrap().value
}

fn sheet_xml(book: &Workbook) -> String {
    String::from_utf8(
        payloads(&book.to_bytes().unwrap())
            .remove(SHEET_PATH)
            .unwrap(),
    )
    .unwrap()
}

fn reopened(book: &Workbook) -> Workbook {
    Workbook::from_bytes(book.to_bytes().unwrap()).unwrap()
}

// Worksheet-level refusals name the worksheet, then the cell or row.
fn unsupported_message(result: sheetpatch::Result<()>) -> String {
    match result {
        Err(Error::Unsupported(message)) => message
            .strip_prefix("worksheet \"Data\": ")
            .unwrap_or_else(|| panic!("expected the worksheet's name: {message}"))
            .to_owned(),
        other => panic!("expected an unsupported edit, got {other:?}"),
    }
}

#[test]
fn leading_byte_order_marks_are_ordinary_cell_text() {
    let mut book = workbook(
        &row(concat!(
            "<c r='A1' t='inlineStr'><is><t>\u{feff}Name</t></is></c>",
            "<c r='B1' t='inlineStr'><is><r><t>a</t></r><r><t>\u{feff}b</t></r></is></c>",
            "<c r='C1' t='str'><f>\u{feff}T(\"x\")</f><v>\u{feff}x</v></c>",
            "<c r='D1' t='e'><v>\u{feff}#N/A</v></c>",
            "<c r='E1' t='s'><v>0</v></c>",
            "<c r='F1' t='inlineStr'><is><t>\u{feff}\u{feff}two</t></is></c>",
            "<c r='G1'><v>\u{feff}5</v></c>",
        )),
        Some("<si><t>\u{feff}Header</t></si>"),
    );
    assert_eq!(value(&book, "A1"), CellValue::from("\u{feff}Name"));
    assert_eq!(value(&book, "B1"), CellValue::from("a\u{feff}b"));
    let formula = book.get_cell("Data", "C1").unwrap();
    assert_eq!(formula.value, CellValue::from("\u{feff}x"));
    assert_eq!(formula.formula.as_deref(), Some("\u{feff}T(\"x\")"));
    assert_eq!(value(&book, "D1"), CellValue::Error("\u{feff}#N/A".into()));
    assert_eq!(value(&book, "E1"), CellValue::from("\u{feff}Header"));
    assert_eq!(value(&book, "F1"), CellValue::from("\u{feff}\u{feff}two"));
    // U+FEFF is not XML whitespace, so the value is not a number.
    assert!(matches!(
        book.get_cell("Data", "G1"),
        Err(Error::InvalidWorkbook(message))
            if message == "worksheet \"Data\": G1: invalid numeric cell value"
    ));

    // Removing the invisible character is a real edit, not a no-op.
    book.set_cell("Data", "A1", "Name").unwrap();
    assert!(book.has_changes());
    assert_eq!(value(&reopened(&book), "A1"), CellValue::from("Name"));
    book.set_cell("Data", "F1", "\u{feff}two").unwrap();
    assert_eq!(value(&book, "F1"), CellValue::from("\u{feff}two"));
    book.set_cell("Data", "G1", 5).unwrap();
    assert_eq!(value(&book, "G1"), CellValue::Number(5.0));

    // Written text keeps a leading U+FEFF, and writing it again changes nothing.
    book.set_cell("Data", "H1", "\u{feff}Total").unwrap();
    assert_eq!(value(&book, "H1"), CellValue::from("\u{feff}Total"));
    let reopened = reopened(&book);
    assert_eq!(value(&reopened, "H1"), CellValue::from("\u{feff}Total"));
    let saved = book.to_bytes().unwrap();
    book.set_cell("Data", "H1", "\u{feff}Total").unwrap();
    assert_eq!(book.to_bytes().unwrap(), saved);
}

#[test]
fn trailing_cells_follow_existing_cells_around_misplaced_extension_lists() {
    for (cells, expected) in [
        (
            "<extLst><ext uri='u'/></extLst><c r='A1'><v>1</v></c>",
            "<extLst><ext uri='u'/></extLst><c r='A1'><v>1</v></c><c r=\"D1\"><v>2</v></c>",
        ),
        (
            "<c r='A1'/><extLst/><c r='C1'/>",
            "<c r='A1'/><extLst/><c r='C1'/><c r=\"D1\"><v>2</v></c>",
        ),
        // Positive control: an extension list after every cell stays last.
        (
            "<c r='A1'><v>1</v></c><extLst><ext uri='u'/></extLst>",
            "<c r='A1'><v>1</v></c><c r=\"D1\"><v>2</v></c><extLst><ext uri='u'/></extLst>",
        ),
        (
            "<extLst/><c r='A1'/><extLst/>",
            "<extLst/><c r='A1'/><c r=\"D1\"><v>2</v></c><extLst/>",
        ),
    ] {
        let mut book = workbook(&row(cells), None);
        book.set_cell("Data", "D1", 2).unwrap();
        assert!(sheet_xml(&book).contains(&row(expected)), "{cells}");
        let reopened = reopened(&book);
        assert_eq!(value(&reopened, "D1"), CellValue::Number(2.0));
        reopened.get_cell("Data", "A1").unwrap();
        // Later edits to existing and new cells still find a sorted row.
        let mut book = reopened;
        book.set_cells("Data", [("A1", 5), ("B1", 6)]).unwrap();
        assert_eq!(value(&book, "A1"), CellValue::Number(5.0));
        assert_eq!(value(&book, "B1"), CellValue::Number(6.0));
    }
}

#[test]
fn undecodable_old_values_do_not_block_replacements() {
    let cells = concat!(
        "<c r='A1' t='inlineStr'><is><t>_xD83D_ truncated</t></is></c>",
        "<c r='B1' t='str'><v>_xD800_</v></c>",
        "<c r='C1' t='e'><v>#_xDC00_</v></c>",
        "<c r='D1' t='inlineStr'><is><r><t>ok</t></r><r><t>_xDEAD_</t></r></is></c>",
    );
    let mut book = workbook(&row(cells), None);
    for address in ["A1", "B1", "C1", "D1"] {
        assert!(matches!(book.get_cell("Data", address), Err(Error::Xml(_))));
    }
    book.set_cell("Data", "A1", "fixed").unwrap();
    book.set_cell("Data", "B1", "also fixed").unwrap();
    book.set_cell("Data", "C1", CellValue::Error("#N/A".into()))
        .unwrap();
    book.set_cell("Data", "D1", CellValue::Error("#REF!".into()))
        .unwrap();
    let book = reopened(&book);
    assert_eq!(value(&book, "A1"), CellValue::from("fixed"));
    assert_eq!(value(&book, "B1"), CellValue::from("also fixed"));
    assert_eq!(value(&book, "C1"), CellValue::Error("#N/A".into()));
    assert_eq!(value(&book, "D1"), CellValue::Error("#REF!".into()));

    // Positive control: other value types already replaced such cells.
    let mut book = workbook(&row(cells), None);
    book.set_cells(
        "Data",
        [("A1", CellValue::Number(1.0)), ("B1", CellValue::Blank)],
    )
    .unwrap();
    assert_eq!(value(&book, "A1"), CellValue::Number(1.0));
}

#[test]
fn text_with_xml_forbidden_characters_round_trips_through_excel_escapes() {
    let mut book = workbook(
        &row("<c r='A1' t='s'><v>0</v></c>"),
        Some("<si><t>Page_x000C_Break_x000B_Tab</t></si>"),
    );
    let read = value(&book, "A1");
    assert_eq!(read, CellValue::from("Page\u{c}Break\u{b}Tab"));
    book.set_cell("Data", "B1", read.clone()).unwrap();
    assert_eq!(value(&book, "B1"), read);
    assert!(sheet_xml(&book).contains("Page_x000C_Break_x000B_Tab"));

    let forbidden: String = (0..0x20u32)
        .filter(|&code| !matches!(code, 0x9 | 0xa | 0xd))
        .chain([0xfffe, 0xffff])
        .map(|code| char::from_u32(code).unwrap())
        .collect();
    let text = format!("a{forbidden}_x0041_\u{1}_x005F_b\r\n\u{1f600}");
    book.set_cell("Data", "C1", text.as_str()).unwrap();
    let xml = sheet_xml(&book);
    for escape in [
        "_x0000_", "_x0008_", "_x000B_", "_x000C_", "_x001F_", "_xFFFE_", "_xFFFF_",
    ] {
        assert!(xml.contains(escape), "{escape}");
    }
    assert!(xml.contains("_x005F_x0041__x0001__x005F_x005F_b&#13;\n"));
    assert_eq!(value(&book, "C1"), CellValue::Text(text.clone()));
    assert_eq!(value(&reopened(&book), "C1"), CellValue::Text(text.clone()));

    // Writing the same text again, or a shared string's own text, is a no-op.
    let saved = book.to_bytes().unwrap();
    book.set_cells(
        "Data",
        [("C1", text.as_str()), ("B1", "Page\u{c}Break\u{b}Tab")],
    )
    .unwrap();
    book.set_cell("Data", "A1", read).unwrap();
    assert_eq!(book.to_bytes().unwrap(), saved);

    // Error values are not free text: control characters remain invalid.
    assert!(matches!(
        book.set_cell("Data", "D1", CellValue::Error("#N/A\u{1}".into())),
        Err(Error::InvalidValue(_))
    ));
}

#[test]
fn error_values_must_be_excel_error_codes_but_reads_stay_lenient() {
    let mut book = workbook(&row("<c r='A1' t='e'><v>#FUTURE!</v></c>"), None);
    assert_eq!(value(&book, "A1"), CellValue::Error("#FUTURE!".into()));
    for invalid in ["#FOO", "##", "#1", "#DIV/0", "#N/A!", "#na", "#FUTURE!"] {
        let error = CellValue::Error(invalid.into());
        assert!(
            matches!(
                book.set_cell("Data", "B1", error.clone()),
                Err(Error::InvalidValue(_))
            ),
            "{invalid}"
        );
        assert!(CellEdit::new("Data", "B1", error).is_err(), "{invalid}");
    }
    assert!(!book.has_changes());
    let valid = [
        "#NULL!",
        "#DIV/0!",
        "#VALUE!",
        "#REF!",
        "#NAME?",
        "#NUM!",
        "#N/A",
        "#GETTING_DATA",
        "#SPILL!",
        "#CALC!",
        "#FIELD!",
        "#BLOCKED!",
        "#CONNECT!",
        "#BUSY!",
        "#UNKNOWN!",
        "#PYTHON!",
        "#EXTERNAL!",
    ];
    for (column, code) in valid.into_iter().enumerate() {
        let address = format!("{}2", char::from(b'A' + column as u8));
        book.set_cell("Data", &address, CellValue::Error(code.into()))
            .unwrap();
        assert_eq!(value(&book, &address), CellValue::Error(code.into()));
    }
}

#[test]
fn new_cells_inherit_row_then_column_default_styles() {
    let body = concat!(
        "<cols><col min='2' max='3' style='4'/><col min='5' max='5' style='0'/><col min='6' max='6' width='9'/></cols>",
        "<sheetData>",
        "<row r='2' s='7' customFormat='1'><c r='A2'><v>1</v></c></row>",
        "<row r='3' s='8'><c r='A3'><v>1</v></c></row>",
        "<row r='4' s=' 9 ' customFormat='true'/>",
        "<row r='5' s='3' customFormat='0'><c r='A5' s='2'><v>1</v></c></row>",
        "<row r='6' customFormat='1'><c r='A6'><v>1</v></c></row>",
        "</sheetData>",
    );
    let mut book = workbook(body, None);
    book.set_cells(
        "Data",
        [
            ("B1", 1),
            ("D1", 1),
            ("E1", 1),
            ("F1", 1),
            ("A2", 2),
            ("C2", 1),
            ("B3", 1),
            ("D3", 1),
            ("B4", 1),
            ("A5", 2),
            ("C5", 1),
            ("C6", 1),
        ],
    )
    .unwrap();
    let book = reopened(&book);
    for (address, expected) in [
        ("B1", Some(4)),
        ("D1", None),
        ("E1", None),
        ("F1", None),
        // Existing cells are never restyled.
        ("A2", None),
        ("C2", Some(7)),
        ("B3", Some(4)),
        ("D3", None),
        ("B4", Some(9)),
        ("A5", Some(2)),
        ("C5", Some(4)),
        // A custom row format without a style uses style zero.
        ("C6", None),
    ] {
        assert_eq!(
            book.get_cell("Data", address).unwrap().style_index,
            expected,
            "{address}"
        );
    }
    let xml = sheet_xml(&book);
    assert!(xml.contains("<c r=\"B1\" s=\"4\"><v>1</v></c><c r=\"D1\"><v>1</v></c>"));
    assert!(xml.contains("<c r=\"C2\" s=\"7\"><v>1</v></c>"));
    assert!(xml.contains("<row r='4' s=' 9 ' customFormat='true'><c r=\"B4\" s=\"9\">"));

    // Text keeps attribute order r, s, t.
    let mut book = workbook(body, None);
    book.set_cell("Data", "C1", "text").unwrap();
    assert!(sheet_xml(&book).contains("<c r=\"C1\" s=\"4\" t=\"inlineStr\">"));
}

#[test]
fn malformed_default_styles_only_block_new_cells_that_need_them() {
    for (body, message) in [
        (
            "<cols><col min='2' max='1' style='4'/></cols><sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData>",
            "B2: invalid column definition",
        ),
        (
            "<cols><col min='1' max='3' style='4'/><col min='3' max='4' style='5'/></cols><sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData>",
            "B2: overlapping column definitions",
        ),
        (
            "<cols><col min='1' max='3' style='x'/></cols><sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData>",
            "B2: invalid column definition",
        ),
    ] {
        let mut book = workbook(body, None);
        assert_eq!(
            unsupported_message(book.set_cell("Data", "B2", 1)),
            message,
            "{body}"
        );
        // Reads and edits of existing cells do not need a default style.
        book.set_cell("Data", "A1", 2).unwrap();
        assert_eq!(value(&book, "A1"), CellValue::Number(2.0));
    }
    let mut book = workbook(
        "<sheetData><row r='1' s='x' customFormat='1'><c r='A1'><v>1</v></c></row><row r='2' s='1' customFormat='yes'/></sheetData>",
        None,
    );
    assert_eq!(
        unsupported_message(book.set_cell("Data", "B1", 1)),
        "B1: invalid row style index"
    );
    assert_eq!(
        unsupported_message(book.set_cell("Data", "B2", 1)),
        "B2: invalid row format flag"
    );
    book.set_cells("Data", [("A1", 2), ("C3", 3)]).unwrap();
}

#[test]
fn rewriting_a_shared_string_cell_with_its_own_text_is_a_no_op() {
    let body = row("<c r='A1' t='s' s='1'><v> 0 </v></c><c r='B1' t='s'><v>1</v></c>");
    let mut book = workbook(
        &body,
        Some("<si><t>first</t></si><si><r><t>sec</t></r><r><t>ond</t></r></si>"),
    );
    let original = book.to_bytes().unwrap();
    book.set_cells("Data", [("A1", "first"), ("B1", "second")])
        .unwrap();
    assert!(!book.has_changes());
    assert_eq!(book.to_bytes().unwrap(), original);
    book.set_cell("Data", "A1", "changed").unwrap();
    assert!(book.has_changes());
    assert!(sheet_xml(&book).contains(
        "<c r='A1' t='inlineStr' s='1'><is><t xml:space=\"preserve\">changed</t></is></c>"
    ));

    // A missing or unreadable table, or an invalid index, still allows the edit.
    for (body, strings) in [
        (body.clone(), None),
        (body.clone(), Some("<si><t>first</si>")),
        (
            row("<c r='A1' t='s'><v>9</v></c>"),
            Some("<si><t>first</t></si>"),
        ),
        (
            row("<c r='A1' t='s'><v>first</v></c>"),
            Some("<si><t>first</t></si>"),
        ),
    ] {
        let mut book = workbook(&body, strings);
        book.set_cell("Data", "A1", "first").unwrap();
        assert!(book.has_changes());
        assert_eq!(value(&book, "A1"), CellValue::from("first"));
    }
}

#[test]
fn iso_8601_date_cells_read_as_excel_serial_numbers() {
    let cells = [
        ("2024-01-15T00:00:00", 45_306.0),
        ("2024-01-15", 45_306.0),
        ("2024-01-15T18:00Z", 45_306.75),
        (" 2024-01-15T06:00:00.5 ", 45_306.0 + 21_600.5 / 86_400.0),
        ("1900-01-01", 1.0),
        ("1900-02-28", 59.0),
        ("1900-03-01", 61.0),
        ("9999-12-31T23:59:59", 2_958_465.0 + 86_399.0 / 86_400.0),
        ("12:00", 0.5),
        ("06:00:00Z", 0.25),
        // Excel's day zero holds times without a date.
        ("1899-12-31", 0.0),
        ("1899-12-31T12:00:00", 0.5),
    ];
    let body: String = cells
        .iter()
        .enumerate()
        .map(|(row, (text, _))| {
            format!(
                "<row r='{0}'><c r='A{0}' t='d'><v>{text}</v></c></row>",
                row + 1
            )
        })
        .collect();
    let mut book = workbook(&format!("<sheetData>{body}</sheetData>"), None);
    for (row, (text, serial)) in cells.iter().enumerate() {
        assert_eq!(
            value(&book, &format!("A{}", row + 1)),
            CellValue::Number(*serial),
            "{text}"
        );
    }
    // Valid values outside the supported forms are unsupported; malformed
    // values make the workbook invalid. Either can be replaced.
    for (text, unsupported) in [
        ("1899-12-30", true),
        ("1899-12-30T23:59:59", true),
        ("2024-01-15T00:00:00+01:00", true),
        ("2024-01-15-05:00", true),
        ("2023-02-29", false),
        ("2024-13-01", false),
        ("2024-01-15T24:00", false),
        ("2024-01-15T00:60", false),
        ("2024-01-15T00:00:60", false),
        ("2024-01-15T00:00:00.", false),
        ("24-01-15", false),
        ("2024-1-15", false),
        ("noon", false),
        ("", false),
    ] {
        let mut book = workbook(&row(&format!("<c r='A1' t='d'><v>{text}</v></c>")), None);
        let error = book.get_cell("Data", "A1").unwrap_err();
        let expected = if unsupported {
            matches!(&error, Error::Unsupported(message) if message.starts_with("worksheet \"Data\": A1: ISO 8601 date cell value "))
        } else {
            matches!(&error, Error::InvalidWorkbook(message) if message == "worksheet \"Data\": A1: invalid ISO 8601 date cell value")
        };
        assert!(expected, "{text}: {error:?}");
        book.set_cell("Data", "A1", 1).unwrap();
        assert_eq!(value(&book, "A1"), CellValue::Number(1.0));
    }
    // Writing the serial a date cell reads as leaves it unchanged; another
    // number replaces the date.
    book.set_cell("Data", "A1", 45_306.0).unwrap();
    book.set_cell("Data", "A12", 0.5).unwrap();
    assert!(!book.has_changes());
    book.set_cell("Data", "A1", 45_307.0).unwrap();
    assert!(book.has_changes());
    assert!(sheet_xml(&book).contains("<c r='A1'><v>45307</v></c>"));
}

#[test]
fn edit_refusals_name_the_cell() {
    let mut book = workbook(
        &row(concat!(
            "<c r='A1' vm='1' t='e'><v>#VALUE!</v></c>",
            "<c r='B1' t='inlineStr'><is><t>x</t><foreign xmlns='urn:x'/></is></c>",
            "<c r='C1'><v>1</v><v>2</v></c>",
            "<c r='D1' t='inlineStr'><is unexpected='1'><t>x</t></is></c>",
            "<c r='E1'><v>1<x:y xmlns:x='urn:x'/></v></c>",
        )),
        None,
    );
    let error = book.set_cell("Data", "A1", 5).unwrap_err();
    assert_eq!(
        error.to_string(),
        "protected cell: worksheet \"Data\": A1 has value metadata (a linked or rich data type)"
    );
    for (address, message) in [
        ("B1", "B1: unfamiliar XML inside the cell value payload"),
        ("C1", "C1: cell has multiple value payloads"),
        (
            "D1",
            "D1: unfamiliar attributes inside the cell value payload",
        ),
        ("E1", "E1: elements inside a simple cell value payload"),
    ] {
        assert_eq!(
            unsupported_message(book.set_cell("Data", address, 5)),
            message
        );
    }
    let mut book = workbook(
        "<sheetData><row r='1' spans='x'><c r='A1'><v>1</v></c></row></sheetData>",
        None,
    );
    assert_eq!(
        unsupported_message(book.set_cell("Data", "B1", 5)),
        "row 1: invalid row spans"
    );
}
