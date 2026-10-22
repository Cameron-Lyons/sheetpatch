//! Regressions for worksheet text decoding, value validation, and new cells.
mod common;

use common::{payloads, stored_archive};
use sheetpatch::{CellEdit, CellValue, Error, Workbook};

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const SHEET_PATH: &str = "xl/worksheets/sheet1.xml";

/// A one-sheet workbook named `Data`, optionally with a shared-string table.
fn workbook(body: &str, shared_strings: Option<&str>) -> Workbook {
    workbook_in_namespace(body, shared_strings, MAIN_NS)
}

fn workbook_in_namespace(body: &str, shared_strings: Option<&str>, namespace: &str) -> Workbook {
    let sheet = format!("<worksheet xmlns='{namespace}'>{body}</worksheet>");
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
        .map(|items| format!("<sst xmlns='{namespace}'>{items}</sst>"))
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
fn xml_references_decode_consistently_across_cell_values_and_no_ops() {
    let escaped = "&amp;&lt;&gt;&apos;&quot;&#9;&#10;&#13;&#x1F600;&#xFEFF;&#95;x0041_";
    let expected_text = "&<>'\"\t\n\r😀\u{feff}A";
    let cells = format!(
        "<c r='A1' t='inlineStr'><is><t>{escaped}</t></is></c>\
         <c r='B1' t='s'><v>&#48;</v></c>\
         <c r='C1'><v>&#49;&#46;&#50;&#53;</v></c>\
         <c r='D1' t='b'><v>t&#114;ue</v></c>\
         <c r='E1' t='e'><v>#V&#65;LUE!</v></c>\
         <c r='F1' t='d'><v>2024-01-&#49;5T12:00:00</v></c>\
         <c r='G1'><f>IF(A1=&quot;&amp;&quot;,&#49;,0)</f><v>1</v></c>"
    );
    let strings = format!("<si><r><t>{escaped}</t></r></si>");
    let mut book = workbook(&row(&cells), Some(&strings));
    let original = book.to_bytes().unwrap();
    let expected = [
        ("A1", CellValue::from(expected_text)),
        ("B1", CellValue::from(expected_text)),
        ("C1", CellValue::Number(1.25)),
        ("D1", CellValue::Bool(true)),
        ("E1", CellValue::Error("#VALUE!".into())),
        ("F1", CellValue::Number(45_306.5)),
    ];
    for (address, expected) in &expected {
        assert_eq!(value(&book, address), *expected);
    }
    let formula = book.get_cell("Data", "G1").unwrap();
    assert_eq!(formula.value, CellValue::Number(1.0));
    assert_eq!(formula.formula.as_deref(), Some("IF(A1=\"&\",1,0)"));
    book.set_cells("Data", expected).unwrap();
    assert_eq!(book.to_bytes().unwrap(), original);
    book.set_cells(
        "Data",
        [
            ("A1", CellValue::from("&<>\r😀")),
            ("C1", CellValue::Number(-3.5)),
        ],
    )
    .unwrap();
    let reopened = reopened(&book);
    assert_eq!(value(&reopened, "A1"), CellValue::from("&<>\r😀"));
    assert_eq!(value(&reopened, "B1"), CellValue::from(expected_text));
    assert_eq!(value(&reopened, "C1"), CellValue::Number(-3.5));
}

#[test]
fn uppercase_x_in_stored_text_is_literal_for_reads_edits_and_no_ops() {
    for (cell, strings) in [
        ("<c r='A1' t='inlineStr'><is><t>_X0041_</t></is></c>", None),
        (
            "<c r='A1' t='inlineStr'><is><r><t>_X0041_</t></r></is></c>",
            None,
        ),
        ("<c r='A1' t='str'><v>_X0041_</v></c>", None),
        (
            "<c r='A1' t='s'><v>0</v></c>",
            Some("<si><t>_X0041_</t></si>"),
        ),
        (
            "<c r='A1' t='s'><v>0</v></c>",
            Some("<si><r><t>_X0041_</t></r></si>"),
        ),
    ] {
        let mut book = workbook(&row(cell), strings);
        let original = book.to_bytes().unwrap();
        assert_eq!(value(&book, "A1"), CellValue::from("_X0041_"));
        book.set_cell("Data", "A1", "_X0041_").unwrap();
        assert!(!book.has_changes());
        assert_eq!(book.to_bytes().unwrap(), original);

        book.set_cell("Data", "A1", "A").unwrap();
        assert!(book.has_changes());
        assert_eq!(value(&reopened(&book), "A1"), CellValue::from("A"));
        if strings.is_some() {
            assert_eq!(
                payloads(&book.to_bytes().unwrap())["xl/sharedStrings.xml"],
                payloads(&original)["xl/sharedStrings.xml"]
            );
        }

        book.set_cell("Data", "A1", "_X0041_").unwrap();
        assert!(sheet_xml(&book).contains(">_X0041_</t>"));
        assert_eq!(value(&reopened(&book), "A1"), CellValue::from("_X0041_"));
    }
}

#[test]
fn uppercase_x_in_sheet_names_is_literal() {
    let original = workbook(&row("<c r='A1'><v>1</v></c>"), None)
        .to_bytes()
        .unwrap();
    for (stored_name, decoded_name) in [
        ("_X0041_", "_X0041_"),
        ("_x0041_", "A"),
        ("_x005F_X0041_", "_X0041_"),
    ] {
        let mut parts = payloads(&original);
        let metadata = String::from_utf8(parts["xl/workbook.xml"].clone())
            .unwrap()
            .replace("name='Data'", &format!("name='{stored_name}'"));
        parts.insert("xl/workbook.xml".into(), metadata.as_bytes().to_vec());
        let entries: Vec<_> = parts
            .iter()
            .map(|(name, xml)| (name.as_str(), xml.as_slice()))
            .collect();
        let original = stored_archive(&entries);
        let mut book = Workbook::from_bytes(original.clone()).unwrap();
        assert_eq!(book.sheets()[0].name(), decoded_name);
        assert_eq!(
            book.get_cell(decoded_name, "A1").unwrap().value,
            CellValue::Number(1.0)
        );
        book.set_cell(decoded_name, "A1", 1).unwrap();
        assert!(!book.has_changes());
        assert_eq!(book.to_bytes().unwrap(), original);

        book.set_cell(decoded_name, "A1", 2).unwrap();
        let saved = book.to_bytes().unwrap();
        assert_eq!(payloads(&saved)["xl/workbook.xml"], metadata.as_bytes());
        let book = Workbook::from_bytes(saved).unwrap();
        assert_eq!(book.sheets()[0].name(), decoded_name);
        assert_eq!(
            book.get_cell(decoded_name, "A1").unwrap().value,
            CellValue::Number(2.0)
        );
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
        "<c r='E1' t='s'><v>0</v></c>",
    );
    let mut book = workbook(&row(cells), Some("<si><t>_xD83D_</t></si>"));
    // An unpaired surrogate is valid but has no Rust string.
    let unpaired = "unpaired UTF-16 surrogate in Excel text escape";
    for address in ["A1", "B1", "C1", "D1"] {
        assert!(matches!(
            book.get_cell("Data", address),
            Err(Error::Unsupported(message))
                if message == format!("worksheet \"Data\": {address}: {unpaired}")
        ));
    }
    assert!(matches!(
        book.get_cell("Data", "E1"),
        Err(Error::Unsupported(message))
            if message == format!("part \"xl/sharedStrings.xml\": string 0: {unpaired}")
    ));
    book.set_cell("Data", "A1", "fixed").unwrap();
    book.set_cell("Data", "B1", "also fixed").unwrap();
    book.set_cell("Data", "C1", CellValue::Error("#N/A".into()))
        .unwrap();
    book.set_cell("Data", "D1", CellValue::Error("#REF!".into()))
        .unwrap();
    book.set_cell("Data", "E1", "fixed too").unwrap();
    let book = reopened(&book);
    assert_eq!(value(&book, "A1"), CellValue::from("fixed"));
    assert_eq!(value(&book, "B1"), CellValue::from("also fixed"));
    assert_eq!(value(&book, "C1"), CellValue::Error("#N/A".into()));
    assert_eq!(value(&book, "D1"), CellValue::Error("#REF!".into()));
    assert_eq!(value(&book, "E1"), CellValue::from("fixed too"));

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
fn bulk_insertions_keep_each_rows_custom_or_column_style() {
    let padding = (0..512)
        .map(|index| format!(" v:a{index}='keep'"))
        .collect::<String>();
    let row_attributes = format!(
        "r='1' xmlns:v='urn:vendor'{padding} customFormat=' 1 ' s='{}7'",
        "0".repeat(32_768)
    );
    let body = format!(
        "<cols><col min='2' max='3' style='4'/><col min='4' max='5' style='5'/></cols>\
         <sheetData><row {row_attributes}><c r='A1' s='3'><v>1</v></c></row>\
         <row r='2' customFormat='true' s='0000'/>\
         <row r='3' customFormat='1'/>\
         <row r='4' customFormat='false' s='ignored'/></sheetData>"
    );
    let mut book = workbook(&body, None);
    let mut edits = (1..=128)
        .map(|column| (sheetpatch::CellRef::new(1, column).unwrap(), 2))
        .collect::<Vec<_>>();
    for row in 2..=4 {
        for column in [3, 4] {
            edits.push((sheetpatch::CellRef::new(row, column).unwrap(), 2));
        }
    }
    book.set_cells_at("Data", edits).unwrap();
    let book = reopened(&book);
    let view = book.prepare_sheet("Data").unwrap();
    assert_eq!(view.get_cell("A1").unwrap().style_index, Some(3));
    for column in 2..=128 {
        assert_eq!(
            view.get_cell_at(sheetpatch::CellRef::new(1, column).unwrap())
                .unwrap()
                .style_index,
            Some(7)
        );
    }
    for address in ["C2", "D2", "C3", "D3"] {
        assert_eq!(
            view.get_cell(address).unwrap().style_index,
            None,
            "{address}"
        );
    }
    assert_eq!(view.get_cell("C4").unwrap().style_index, Some(4));
    assert_eq!(view.get_cell("D4").unwrap().style_index, Some(5));
    assert!(sheet_xml(&book).contains(&format!("<row {row_attributes}>")));
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
    // Existing cells and clears of missing cells never need row defaults.
    book.set_cells(
        "Data",
        [
            ("A1", CellValue::Number(3.0)),
            ("B1", CellValue::Blank),
            ("A2", CellValue::Blank),
        ],
    )
    .unwrap();
    let pending = book.to_bytes().unwrap();
    for (edits, expected) in [
        (
            [
                ("A1", CellValue::Number(4.0)),
                ("B1", CellValue::Blank),
                ("C1", CellValue::Number(5.0)),
            ],
            "C1: invalid row style index",
        ),
        (
            [
                ("A1", CellValue::Number(4.0)),
                ("A2", CellValue::Blank),
                ("B2", CellValue::Number(5.0)),
            ],
            "B2: invalid row format flag",
        ),
    ] {
        assert_eq!(unsupported_message(book.set_cells("Data", edits)), expected);
        assert_eq!(book.to_bytes().unwrap(), pending);
    }
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
        // Months of 30 days.
        ("2024-04-30", 45_412.0),
        ("2024-06-15", 45_458.0),
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
        ("2024-04-31", false),
        ("2023-09-31", false),
        ("2024-06-00", false),
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
fn duplicate_text_in_one_rich_text_run_cannot_produce_false_no_ops() {
    let duplicate = "<r><t>first</t><t>second</t></r>";
    let mut book = workbook(
        &row(&format!(
            "<c r='A1' t='inlineStr'><is>{duplicate}</is></c><c r='B1'><v>9</v></c>"
        )),
        None,
    );
    let original = book.to_bytes().unwrap();
    assert!(matches!(
        book.get_cell("Data", "A1"),
        Err(Error::Unsupported(message))
            if message == "worksheet \"Data\": A1: rich text run has multiple text elements"
    ));
    for replacement in [
        CellValue::from("firstsecond"),
        CellValue::from("second"),
        CellValue::Number(1.0),
        CellValue::Blank,
    ] {
        assert!(matches!(
            book.set_cell("Data", "A1", replacement),
            Err(Error::Unsupported(message))
                if message == "worksheet \"Data\": A1: rich text run has multiple text elements"
        ));
        assert_eq!(book.to_bytes().unwrap(), original);
    }
    book.set_cell("Data", "B1", 8).unwrap();
    assert!(sheet_xml(&book).contains(duplicate));

    let mut book = workbook(
        &row("<c r='A1' t='s'><v>0</v></c>"),
        Some(&format!("<si>{duplicate}</si>")),
    );
    let original = book.to_bytes().unwrap();
    assert!(matches!(
        book.get_cell("Data", "A1"),
        Err(Error::Unsupported(message))
            if message
                == "part \"xl/sharedStrings.xml\": string 0: rich text run has multiple text elements"
    ));
    // Explicit replacement repairs an unreadable shared-string cell without
    // rewriting the shared table or mistaking the concatenation for a no-op.
    book.set_cell("Data", "A1", "firstsecond").unwrap();
    assert!(book.has_changes());
    assert_eq!(
        value(&reopened(&book), "A1"),
        CellValue::from("firstsecond")
    );
    assert_eq!(
        payloads(&book.to_bytes().unwrap())["xl/sharedStrings.xml"],
        payloads(&original)["xl/sharedStrings.xml"]
    );

    for content in [
        "<r><t>first</t></r><r><t>second</t></r>",
        "<t>first</t><t>second</t>",
        "<t>first</t><r><t>second</t></r>",
        "<r><t>first</t></r><t>second</t>",
        "<r/><t>firstsecond</t>",
    ] {
        let mut book = workbook(
            &row(&format!("<c r='A1' t='inlineStr'><is>{content}</is></c>")),
            None,
        );
        let original = book.to_bytes().unwrap();
        assert_eq!(value(&book, "A1"), CellValue::from("firstsecond"));
        book.set_cell("Data", "A1", "firstsecond").unwrap();
        assert_eq!(book.to_bytes().unwrap(), original);
    }
    let book = workbook(
        &row(
            "<c r='A1' t='inlineStr'><is><r><t>visible</t><x:t xmlns:x='urn:custom'>opaque</x:t></r></is></c>",
        ),
        None,
    );
    assert_eq!(value(&book, "A1"), CellValue::from("visible"));
}

#[test]
fn cell_metadata_with_edit_sensitive_semantics_cannot_survive_value_changes() {
    // Metadata operation flags apply to cell metadata as well as value metadata.
    // ECMA-376 18.9.10 requires disassociation on edit/clear when these are false.
    let metadata = format!(
        "<metadata xmlns='{MAIN_NS}'><metadataTypes count='1'><metadataType name='custom' minSupportedVersion='1' cellMeta='1' edit='0' delete='0' clearContents='0'/></metadataTypes><futureMetadata name='custom' count='1'><bk><extLst><ext uri='urn:custom'><stamp xmlns='urn:custom' originalValue='1'/></ext></extLst></bk></futureMetadata><cellMetadata count='2'><bk><rc t='1' v='0'/></bk><bk><rc t='1' v='0'/></bk></cellMetadata></metadata>"
    );
    for index in ["0", "1"] {
        let original = workbook(
            &row(&format!(
                "<c r='A1' cm='{index}'><v>1</v></c><c r='B1'><v>9</v></c>"
            )),
            None,
        )
        .to_bytes()
        .unwrap();
        let mut parts = payloads(&original);
        let relationships = String::from_utf8(parts["xl/_rels/workbook.xml.rels"].clone())
            .unwrap()
            .replace(
                "</Relationships>",
                &format!("<Relationship Id='metadata' Type='{OFFICE_NS}/sheetMetadata' Target='metadata.xml'/></Relationships>"),
            );
        parts.insert(
            "xl/_rels/workbook.xml.rels".into(),
            relationships.into_bytes(),
        );
        let types = String::from_utf8(parts["[Content_Types].xml"].clone())
            .unwrap()
            .replace(
                "</Types>",
                "<Override PartName='/xl/metadata.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.sheetMetadata+xml'/></Types>",
            );
        parts.insert("[Content_Types].xml".into(), types.into_bytes());
        parts.insert("xl/metadata.xml".into(), metadata.as_bytes().to_vec());
        let entries: Vec<_> = parts
            .iter()
            .map(|(name, xml)| (name.as_str(), xml.as_slice()))
            .collect();
        let original = stored_archive(&entries);
        let mut book = Workbook::from_bytes(original.clone()).unwrap();
        assert_eq!(value(&book, "A1"), CellValue::Number(1.0));
        book.set_cell("Data", "A1", 1).unwrap();
        assert!(!book.has_changes());
        assert_eq!(book.to_bytes().unwrap(), original);

        for value in [
            CellValue::Number(2.0),
            CellValue::from("replacement"),
            CellValue::Bool(true),
            CellValue::Error("#N/A".into()),
            CellValue::Blank,
        ] {
            let error = book
                .set_cells("Data", [("B1", CellValue::Number(8.0)), ("A1", value)])
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "protected cell: worksheet \"Data\": A1 has cell metadata with unsupported edit semantics"
            );
            assert!(!book.has_changes());
            assert_eq!(book.to_bytes().unwrap(), original);
        }
        book.set_cell("Data", "B1", 8).unwrap();
        let saved = book.to_bytes().unwrap();
        let after = payloads(&saved);
        for (name, bytes) in &parts {
            if name != SHEET_PATH {
                assert_eq!(&after[name], bytes);
            }
        }
        let reopened = Workbook::from_bytes(saved).unwrap();
        assert_eq!(value(&reopened, "A1"), CellValue::Number(1.0));
        assert_eq!(value(&reopened, "B1"), CellValue::Number(8.0));
    }
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
    // Two payloads leave the cell's value ambiguous, which makes the
    // workbook invalid rather than the edit unsupported.
    let error = book.set_cell("Data", "C1", 5).unwrap_err();
    assert!(
        matches!(&error, Error::InvalidWorkbook(message)
            if message == "worksheet \"Data\": C1: cell has multiple value payloads"),
        "{error:?}"
    );
    let mut book = workbook(
        "<sheetData><row r='1' spans='x'><c r='A1'><v>1</v></c></row></sheetData>",
        None,
    );
    assert_eq!(
        unsupported_message(book.set_cell("Data", "B1", 5)),
        "row 1: invalid row spans"
    );
}

#[test]
fn values_marked_to_preserve_whitespace_can_be_rewritten_and_replaced() {
    // SheetJS writes text with surrounding spaces or a line break this way.
    let mut book = workbook(
        &row(concat!(
            "<c r='A1' t='str'><v xml:space='preserve'>  spaced  </v></c>",
            "<c r='B1' t='str'><v xml:space='preserve'>multi\nline</v></c>",
            "<c r='C1'><v xml:space='default'>7</v></c>",
            "<c r='D1'><v xml:space='preserve' unexpected='1'>8</v></c>",
        )),
        None,
    );
    let original = book.to_bytes().unwrap();
    let current = [
        ("A1", CellValue::from("  spaced  ")),
        ("B1", CellValue::from("multi\nline")),
        ("C1", CellValue::Number(7.0)),
    ];
    for (address, expected) in &current {
        assert_eq!(&value(&book, address), expected);
    }
    book.set_cells("Data", current).unwrap();
    assert!(!book.has_changes());
    assert_eq!(book.to_bytes().unwrap(), original);
    assert_eq!(
        unsupported_message(book.set_cell("Data", "D1", 9)),
        "D1: unfamiliar attributes inside the cell value payload"
    );
    book.set_cells(
        "Data",
        [
            ("A1", CellValue::from(" new ")),
            ("B1", CellValue::Blank),
            ("C1", CellValue::Number(9.0)),
        ],
    )
    .unwrap();
    let saved = reopened(&book);
    assert_eq!(value(&saved, "A1"), CellValue::from(" new "));
    assert_eq!(value(&saved, "B1"), CellValue::Blank);
    assert_eq!(value(&saved, "C1"), CellValue::Number(9.0));
    assert_eq!(value(&saved, "D1"), CellValue::Number(8.0));
}

#[test]
fn namespace_prefix_caches_follow_shadowing_and_empty_element_scopes() {
    let escaped_main = MAIN_NS.replace("/main", "/ma&#105;n");
    let body = format!(
        "<sheetData xmlns:s='{MAIN_NS}' xmlns:t='{escaped_main}' xmlns:a='urn:first' xmlns:b='urn:second'><row r='1' xmlns:xml='http://www.w3.org/XML/1998/namespac&#101;'><s:c r='A1' a:flag='1' b:flag='2'><s:v>1</s:v></s:c><s:c xmlns:s='urn:vendor' r='Z1'/><marker xmlns=''><c r='Z2'/></marker><holder xmlns='urn:vendor' xmlns:s='urn:vendor'><s:c r='Z3'/></holder><t:c r='B1' a:flag='1' b:flag='2'><t:v>2</t:v></t:c><a:marker xmlns:a='urn:second' a:flag='x'/><c r='C1' a:flag='1' b:flag='2'><v>3</v></c></row></sheetData>"
    );
    let mut book = workbook(&body, None);
    let original = book.to_bytes().unwrap();
    for (address, expected) in [("A1", 1), ("B1", 2), ("C1", 3)] {
        assert_eq!(value(&book, address), CellValue::from(expected));
        book.set_cell("Data", address, expected).unwrap();
    }
    assert!(!book.has_changes());
    assert_eq!(book.to_bytes().unwrap(), original);
    book.set_cells("Data", [("A1", 4), ("D1", 5), ("C2", 6)])
        .unwrap();
    let saved = reopened(&book);
    for (address, expected) in [("A1", 4), ("B1", 2), ("C1", 3), ("D1", 5), ("C2", 6)] {
        assert_eq!(value(&saved, address), CellValue::from(expected));
    }
    for opaque in [
        "<s:c xmlns:s='urn:vendor' r='Z1'/>",
        "<marker xmlns=''><c r='Z2'/></marker>",
        "<holder xmlns='urn:vendor' xmlns:s='urn:vendor'><s:c r='Z3'/></holder>",
        "<a:marker xmlns:a='urn:second' a:flag='x'/>",
    ] {
        assert!(sheet_xml(&saved).contains(opaque));
    }

    // The two prefixes become aliases only on this final cell, after their
    // distinct outer bindings have already been used and cached.
    let duplicate = body.replace("<c r='C1'", "<c r='C1' xmlns:b='urn:first'");
    assert!(matches!(
        workbook(&duplicate, None).get_cell("Data", "A1"),
        Err(Error::Xml(message)) if message.contains("duplicate expanded attribute name")
    ));
    // A prefix declared on an empty element is unbound again afterward.
    for following in ["<x:c r='B1'/>", "<c r='B1' x:flag='unbound'/>"] {
        let body = row(&format!(
            "<c r='A1'><v>1</v></c><x:marker xmlns:x='urn:vendor' x:flag='cached'/>{following}"
        ));
        assert!(matches!(
            workbook(&body, None).get_cell("Data", "A1"),
            Err(Error::Xml(message)) if message.contains("unbound namespace prefix")
        ));
    }
}

#[test]
fn escaped_reserved_namespace_bindings_preserve_text_and_namespace_scopes() {
    let xml_namespace = "http://www.w3.org/XML/1998/namespac&#101;";
    let body = format!(
        "<sheetData xmlns:s='{MAIN_NS}'><row r='1' vendor='a\"b' xmlns:before='urn:first' xmlns:xml='{xml_namespace}' xmlns:after='urn:last' before:flag='keep' after:flag='keep'><s:c r='A1' t='inlineStr'><s:is><s:t xmlns:xml='{xml_namespace}' xml:space='preserve'> old </s:t></s:is></s:c><c r='B1' xmlns:s='urn:foreign' xmlns:xml='{xml_namespace}'/><s:c r='C1' t='s'><s:v>0</s:v></s:c></row></sheetData>"
    );
    let strings =
        format!("<si xmlns:xml='{xml_namespace}'><t xml:space='preserve'> shared </t></si>");
    let mut book = workbook(&body, Some(&strings));
    assert_eq!(value(&book, "A1"), CellValue::from(" old "));
    assert_eq!(value(&book, "B1"), CellValue::Blank);
    assert_eq!(value(&book, "C1"), CellValue::from(" shared "));

    // A no-op keeps even the namespace's escaped spelling byte for byte.
    let original = book.to_bytes().unwrap();
    book.set_cells("Data", [("A1", " old "), ("C1", " shared ")])
        .unwrap();
    assert_eq!(book.to_bytes().unwrap(), original);

    // A real edit uses original offsets despite declarations on the value,
    // and an empty element's declarations cannot leak into following cells.
    book.set_cells("Data", [("A1", " changed "), ("C1", "replacement")])
        .unwrap();
    let saved = reopened(&book);
    assert_eq!(value(&saved, "A1"), CellValue::from(" changed "));
    assert_eq!(value(&saved, "C1"), CellValue::from("replacement"));
    let xml = sheet_xml(&saved);
    assert!(xml.contains(&format!(
        "vendor='a\"b' xmlns:before='urn:first' xmlns:xml='{xml_namespace}' xmlns:after='urn:last' before:flag='keep' after:flag='keep'"
    )));
    assert!(xml.contains(&format!(
        "<c r='B1' xmlns:s='urn:foreign' xmlns:xml='{xml_namespace}'/>"
    )));
}

#[test]
fn escaped_reserved_namespace_bindings_cannot_bypass_xml_validation() {
    let xml_namespace = "http://www.w3.org/XML/1998/namespac&#101;";
    for attributes in [
        format!("xmlns:xml='{xml_namespace}' xmlns:xml='{xml_namespace}'"),
        format!("xmlns:xml='{xml_namespace}' xmlns:xml='urn:invalid'"),
        format!("xmlns:xml='{xml_namespace}' xmlns:other='{xml_namespace}'"),
        format!("xmlns:xml='{xml_namespace}' xmlns:other=''"),
        format!("xmlns:xml='{xml_namespace}' unbound:flag='x'"),
        format!(
            "xmlns:xml='{xml_namespace}' xmlns:a='urn:a' xmlns:b='urn:a' a:flag='x' b:flag='y'"
        ),
        "xmlns:xml='http://www.w3.org/XML/1998/namespace&#32;'".to_owned(),
    ] {
        let mut book = workbook(&format!("<sheetData {attributes}/>"), None);
        assert!(
            matches!(book.get_cell("Data", "A1"), Err(Error::Xml(_))),
            "{attributes}"
        );
        assert!(
            matches!(book.set_cell("Data", "A1", 1), Err(Error::Xml(_))),
            "{attributes}"
        );
        assert!(!book.has_changes(), "{attributes}");
    }
}

#[test]
fn hidden_rich_text_cannot_be_discarded_by_a_replacement() {
    for payload in [
        "<t>visible</t><rPr><t>hidden</t></rPr>",
        "<r><t>visible</t><rPr><t>hidden</t></rPr></r>",
        "<rPr><r><t>hidden</t></r></rPr>",
        "<rPh sb='0' eb='1'><r><t>hidden</t></r></rPh>",
    ] {
        let mut book = workbook(
            &row(&format!(
                "<c r='A1' t='inlineStr'><is>{payload}</is></c><c r='B1'><v>1</v></c>"
            )),
            None,
        );
        assert!(
            matches!(book.get_cell("Data", "A1"), Err(Error::Unsupported(_))),
            "{payload}"
        );
        let original = book.to_bytes().unwrap();
        for replacement in [CellValue::from("replacement"), 2.into(), CellValue::Blank] {
            assert!(
                matches!(
                    book.set_cell("Data", "A1", replacement),
                    Err(Error::Unsupported(_))
                ),
                "{payload}"
            );
            assert_eq!(book.to_bytes().unwrap(), original, "{payload}");
        }
        // Another cell remains editable without discarding the hidden text.
        book.set_cell("Data", "B1", 2).unwrap();
        assert!(sheet_xml(&book).contains(payload), "{payload}");
        assert_eq!(value(&book, "B1"), CellValue::Number(2.0));
    }
}

#[test]
fn mixed_spreadsheet_namespaces_cannot_hide_worksheet_semantics() {
    const STRICT: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";
    for (namespace, other) in [(MAIN_NS, STRICT), (STRICT, MAIN_NS)] {
        // Namespace comparison uses decoded URIs, including escaped spellings.
        let other = other.replace('/', "&#47;");
        for body in [
            row(&format!(
                "<c r='A1'><s:f xmlns:s='{other}'>1+1</s:f><v>2</v></c>"
            )),
            row(&format!("<c r='A1'><s:v xmlns:s='{other}'>2</s:v></c>")),
            row(&format!(
                "<c r='A1' t='inlineStr'><is><t>visible</t><s:t xmlns:s='{other}'>hidden</s:t></is></c>"
            )),
            format!(
                "<sheetData><s:row xmlns:s='{other}' r='1'><s:c r='A1'><s:v>2</s:v></s:c></s:row></sheetData>"
            ),
            format!(
                "<sheetData/><s:mergeCells xmlns:s='{other}'><s:mergeCell ref='A1:B2'/></s:mergeCells>"
            ),
            format!(
                "<s:cols xmlns:s='{other}'><s:col min='2' max='2' style='4'/></s:cols><sheetData/>"
            ),
        ] {
            let mut book = workbook_in_namespace(&body, None, namespace);
            assert!(
                matches!(
                    book.get_cell("Data", "A1"),
                    Err(Error::Unsupported(message))
                        if message == "worksheet \"Data\": worksheet mixes strict and transitional SpreadsheetML namespaces"
                ),
                "{body}"
            );
            let original = book.to_bytes().unwrap();
            assert!(
                matches!(book.set_cell("Data", "B2", 7), Err(Error::Unsupported(_))),
                "{body}"
            );
            assert_eq!(book.to_bytes().unwrap(), original, "{body}");
        }

        // Vendor extensions remain opaque even when they reuse core names.
        let body = row(&format!(
            "<c r='A1' xmlns:other='{other}'><v>1</v><s:f xmlns:s='urn:vendor'>extension</s:f></c>"
        ));
        let mut book = workbook_in_namespace(&body, None, namespace);
        book.set_cell("Data", "A1", 2).unwrap();
        assert_eq!(value(&book, "A1"), CellValue::Number(2.0));
        assert!(sheet_xml(&book).contains("<s:f xmlns:s='urn:vendor'>extension</s:f>"));
    }
}

#[test]
fn mixed_spreadsheet_namespaces_cannot_shift_or_truncate_shared_strings() {
    const STRICT: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";
    for (namespace, other) in [(MAIN_NS, STRICT), (STRICT, MAIN_NS)] {
        for strings in [
            format!("<s:si xmlns:s='{other}'><s:t>hidden entry</s:t></s:si><si><t>next</t></si>"),
            format!("<si><t>visible</t><s:t xmlns:s='{other}'>hidden text</s:t></si>"),
            format!("<si><s:r xmlns:s='{other}'><s:t>hidden run</s:t></s:r></si>"),
        ] {
            let mut book = workbook_in_namespace(
                &row("<c r='A1' t='s'><v>0</v></c>"),
                Some(&strings),
                namespace,
            );
            assert!(
                matches!(
                    book.get_cell("Data", "A1"),
                    Err(Error::Unsupported(message))
                        if message == "part \"xl/sharedStrings.xml\": shared-string table mixes strict and transitional SpreadsheetML namespaces"
                ),
                "{strings}"
            );
            // Like any unreadable table, this still permits an explicit
            // replacement of an ordinary cell without changing that table.
            let original_table =
                payloads(&book.to_bytes().unwrap())["xl/sharedStrings.xml"].clone();
            book.set_cell("Data", "A1", "replacement").unwrap();
            assert_eq!(value(&book, "A1"), CellValue::from("replacement"));
            assert_eq!(
                payloads(&book.to_bytes().unwrap())["xl/sharedStrings.xml"],
                original_table
            );
        }
        let book = workbook_in_namespace(
            &row("<c r='A1' t='s'><v>0</v></c>"),
            Some(
                "<vendor:si xmlns:vendor='urn:vendor'/><si><t>visible</t><vendor:t xmlns:vendor='urn:vendor'>extension</vendor:t></si>",
            ),
            namespace,
        );
        assert_eq!(value(&book, "A1"), CellValue::from("visible"));
    }
}
