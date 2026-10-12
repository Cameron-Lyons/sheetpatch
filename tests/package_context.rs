//! Worksheet-level errors name their worksheet, and every entry point reports
//! the same mistake with the same error.
mod common;

use sheetpatch::{CellEdit, CellRef, CellValue, Error, Workbook};
use std::{collections::BTreeMap, mem::discriminant};

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const CONTENT_TYPES: &str = "<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='rels' ContentType='application/vnd.openxmlformats-package.relationships+xml'/><Default Extension='xml' ContentType='application/xml'/></Types>";
const CELL: &str = "<sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData>";

fn worksheet(body: &str) -> Vec<u8> {
    format!("<worksheet xmlns='{MAIN_NS}'>{body}</worksheet>").into_bytes()
}

/// The parts of a workbook whose worksheets have the given names and XML.
fn parts(sheets: &[(&str, Vec<u8>)]) -> BTreeMap<String, Vec<u8>> {
    let mut entries = String::new();
    let mut relationships = String::new();
    let mut parts = BTreeMap::new();
    for (number, (name, xml)) in sheets.iter().enumerate() {
        let number = number + 1;
        let name = name.replace('&', "&amp;").replace('"', "&quot;");
        entries.push_str(&format!(
            "<sheet name=\"{name}\" sheetId='{number}' r:id='s{number}'/>"
        ));
        relationships.push_str(&format!(
            "<Relationship Id='s{number}' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet{number}.xml'/>"
        ));
        parts.insert(format!("xl/worksheets/sheet{number}.xml"), xml.clone());
    }
    parts.insert("[Content_Types].xml".into(), CONTENT_TYPES.into());
    parts.insert(
        "_rels/.rels".into(),
        format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='office' Type='{OFFICE_NS}/officeDocument' Target='xl/workbook.xml'/></Relationships>").into_bytes(),
    );
    parts.insert(
        "xl/workbook.xml".into(),
        format!("<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheets>{entries}</sheets></workbook>").into_bytes(),
    );
    parts.insert(
        "xl/_rels/workbook.xml.rels".into(),
        format!("<Relationships xmlns='{PACKAGE_NS}'>{relationships}</Relationships>").into_bytes(),
    );
    parts
}

fn archive(parts: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let entries: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    common::stored_archive(&entries)
}

fn book(sheets: &[(&str, Vec<u8>)]) -> Workbook {
    Workbook::from_bytes(archive(&parts(sheets))).unwrap()
}

fn message(error: &Error) -> &str {
    match error {
        Error::Xml(message) | Error::InvalidWorkbook(message) | Error::Unsupported(message) => {
            message
        }
        other => panic!("expected a message-carrying error, got {other:?}"),
    }
}

#[test]
fn edit_refusals_name_the_worksheet_in_cross_sheet_transactions() {
    let mut book = book(&[
        ("Alpha", worksheet(CELL)),
        (
            "Beta",
            worksheet(
                "<sheetData><row r='1'><c r='A1'><f>1+1</f><v>2</v></c></row><row r='2'><c r='B2' vm='1'><v>1</v></c></row></sheetData>",
            ),
        ),
    ]);
    let original = book.to_bytes().unwrap();
    let error = book
        .apply_edits([
            CellEdit::new("Alpha", "B1", 1).unwrap(),
            CellEdit::new("Beta", "A1", 5).unwrap(),
        ])
        .unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    assert_eq!(message(&error), "worksheet \"Beta\": A1 contains a formula");
    assert_eq!(
        error.to_string(),
        "unsupported: worksheet \"Beta\": A1 contains a formula"
    );
    assert!(!book.has_changes());
    assert_eq!(book.to_bytes().unwrap(), original);

    for result in [
        book.set_cell("Beta", "B2", 3),
        book.set_cell_at("Beta", CellRef::new(2, 2).unwrap(), 3),
        book.set_cells("Beta", [("B2", 3)]),
        book.set_cells_at("Beta", [(CellRef::new(2, 2).unwrap(), 3)]),
    ] {
        let error = result.unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
        assert_eq!(
            message(&error),
            "worksheet \"Beta\": B2: cell value has associated metadata"
        );
    }
    // Workbook-level refusals and value errors carry no worksheet.
    assert!(matches!(
        book.set_cell("Alpha", "A1", f64::NAN),
        Err(Error::InvalidValue(message)) if !message.contains("worksheet")
    ));
    book.set_cell("Alpha", "B1", 1).unwrap();
}

#[test]
fn worksheet_names_are_debug_quoted() {
    let mut book = book(&[(
        "Q\"1 & \\",
        worksheet("<sheetData><row r='1'><c r='A1'><f>1</f><v>1</v></c></row></sheetData>"),
    )]);
    let error = book.set_cell("Q\"1 & \\", "A1", 2).unwrap_err();
    assert_eq!(
        message(&error),
        "worksheet \"Q\\\"1 & \\\\\": A1 contains a formula"
    );
}

#[test]
fn read_and_parse_errors_name_the_worksheet_and_match_prepared_views() {
    let corrupt = |bytes: &mut Vec<u8>| {
        // Change the worksheet's stored bytes without updating its CRC-32.
        let at = bytes
            .windows(8)
            .rposition(|window| window == b"<v>1</v>")
            .unwrap();
        bytes[at + 3] = b'2';
    };
    let cases: [(&str, Vec<u8>, Error); 5] = [
        (
            "unsorted rows",
            worksheet("<sheetData><row r='2'/><row r='1'/></sheetData>"),
            Error::Unsupported(String::new()),
        ),
        (
            "malformed XML",
            format!("<worksheet xmlns='{MAIN_NS}'><sheetData></worksheet>").into_bytes(),
            Error::Xml(String::new()),
        ),
        (
            "unreadable value",
            worksheet("<sheetData><row r='1'><c r='A1' t='b'><v>2</v></c></row></sheetData>"),
            Error::Unsupported(String::new()),
        ),
        (
            "not a worksheet",
            format!("<chartsheet xmlns='{MAIN_NS}'/>").into_bytes(),
            Error::Unsupported(String::new()),
        ),
        (
            "checksum mismatch",
            worksheet(CELL),
            Error::InvalidWorkbook(String::new()),
        ),
    ];
    for (case, xml, expected) in cases {
        let mut bytes = archive(&parts(&[("Alpha", worksheet(CELL)), ("Beta", xml)]));
        if case == "checksum mismatch" {
            corrupt(&mut bytes);
        }
        let mut book = Workbook::from_bytes(bytes).unwrap();
        let direct = book.get_cell("Beta", "A1").unwrap_err();
        let prepared = match book.prepare_sheet("Beta") {
            Ok(view) => view.get_cell("A1").unwrap_err(),
            Err(error) => error,
        };
        assert_eq!(
            discriminant(&direct),
            discriminant(&expected),
            "{case}: {direct}"
        );
        assert_eq!(prepared.to_string(), direct.to_string(), "{case}");
        assert!(
            message(&direct).starts_with("worksheet \"Beta\": "),
            "{case}: {direct}"
        );
        for error in [
            book.get_cells("Beta", ["A1", "B2"]).unwrap_err(),
            book.get_cell_at("Beta", CellRef::new(1, 1).unwrap())
                .unwrap_err(),
        ] {
            assert_eq!(error.to_string(), direct.to_string(), "{case}");
        }
        // Edits fail with the same context, other than for a value that only
        // a read rejects.
        let edit = book.set_cell("Beta", "C3", 1);
        if case != "unreadable value" {
            let edit = edit.unwrap_err();
            assert_eq!(discriminant(&edit), discriminant(&expected), "{case}");
            assert!(
                message(&edit).starts_with("worksheet \"Beta\": "),
                "{case}: {edit}"
            );
        }
        assert_eq!(
            book.get_cell("Alpha", "A1").unwrap().value,
            CellValue::Number(1.0)
        );
    }
}

#[test]
fn single_cell_setters_resolve_the_sheet_before_the_address_and_value() {
    let mut book = book(&[("Data", worksheet(CELL))]);
    let a1 = CellRef::new(1, 1).unwrap();
    for error in [
        book.set_cell("Missing", "A0", 1).unwrap_err(),
        book.set_cell_at("Missing", a1, f64::NAN).unwrap_err(),
        book.set_cells("Missing", [("A0", 1)]).unwrap_err(),
        book.set_cells_at("Missing", [(a1, f64::NAN)]).unwrap_err(),
        book.get_cell("Missing", "A0").unwrap_err(),
        book.get_cells("Missing", ["A0"]).unwrap_err(),
    ] {
        assert!(
            matches!(&error, Error::SheetNotFound(name) if name == "Missing"),
            "{error:?}"
        );
    }
    assert!(matches!(
        book.set_cell("Data", "A0", 1),
        Err(Error::InvalidCellReference(_))
    ));
    assert!(matches!(
        book.set_cell_at("Data", a1, f64::NAN),
        Err(Error::InvalidValue(_))
    ));
    assert!(matches!(
        book.get_cells("Data", ["A1", "A0"]),
        Err(Error::InvalidCellReference(address)) if address == "A0"
    ));
    assert!(matches!(
        book.prepare_sheet("Data").unwrap().get_cells(["A1", "A0"]),
        Err(Error::InvalidCellReference(address)) if address == "A0"
    ));
    assert!(!book.has_changes());
}

#[test]
fn content_types_are_found_without_ascii_case_and_ambiguity_is_rejected() {
    let mut parts = parts(&[("Data", worksheet(CELL))]);
    let content_types = parts.remove("[Content_Types].xml").unwrap();
    parts.insert("[CONTENT_TYPES].XML".into(), content_types.clone());
    let book = Workbook::from_bytes(archive(&parts)).unwrap();
    assert_eq!(
        book.get_cell("Data", "A1").unwrap().value,
        CellValue::Number(1.0)
    );

    parts.insert("[Content_Types].xml".into(), content_types);
    let error = Workbook::from_bytes(archive(&parts)).err().unwrap();
    assert!(
        matches!(&error, Error::InvalidWorkbook(message) if message.contains("ambiguous")),
        "{error:?}"
    );

    parts.remove("[Content_Types].xml");
    parts.remove("[CONTENT_TYPES].XML");
    let error = Workbook::from_bytes(archive(&parts)).err().unwrap();
    assert!(
        matches!(&error, Error::InvalidWorkbook(message) if message.contains("[Content_Types].xml")),
        "{error:?}"
    );
}

/// Damages a part's XML, given with a `<!--note-->` inside its root.
type Damage = Box<dyn Fn(&str) -> Vec<u8>>;

#[test]
fn package_parts_and_worksheets_report_malformed_xml_with_the_same_variant() {
    let deep = format!("{}{}", "<x:a>".repeat(300), "</x:a>".repeat(300));
    let cases: [(&str, Damage, Error); 5] = [
        (
            "invalid UTF-8",
            Box::new(|root: &str| {
                let mut bytes = root.replacen("<!--note-->", "<!--\u{1}-->", 1).into_bytes();
                let at = bytes.iter().position(|&byte| byte == 1).unwrap();
                bytes[at] = 0xff;
                bytes
            }),
            Error::Xml(String::new()),
        ),
        (
            "two byte order marks",
            Box::new(|root: &str| format!("\u{feff}\u{feff}{root}").into_bytes()),
            Error::Xml(String::new()),
        ),
        (
            "an encoding other than UTF-8",
            Box::new(|root: &str| {
                format!("<?xml version='1.0' encoding='ISO-8859-1'?>{root}").into_bytes()
            }),
            Error::Unsupported(String::new()),
        ),
        (
            "nesting beyond the depth limit",
            Box::new(move |root: &str| root.replacen("<!--note-->", &deep, 1).into_bytes()),
            Error::Unsupported(String::new()),
        ),
        (
            "a document type declaration",
            Box::new(|root: &str| format!("<!DOCTYPE root>{root}").into_bytes()),
            Error::Unsupported(String::new()),
        ),
    ];
    let sheet =
        format!("<worksheet xmlns='{MAIN_NS}' xmlns:x='urn:x'><!--note-->{CELL}</worksheet>");
    for (case, damage, expected) in &cases {
        // The worksheet is read lazily, so the error appears on first access.
        let book = book(&[("Data", damage(&sheet))]);
        let worksheet_error = book.get_cell("Data", "A1").unwrap_err();
        assert_eq!(
            discriminant(&worksheet_error),
            discriminant(expected),
            "worksheet, {case}: {worksheet_error}"
        );

        let base = parts(&[("Data", worksheet(CELL))]);
        for part in [
            "[Content_Types].xml",
            "_rels/.rels",
            "xl/workbook.xml",
            "xl/_rels/workbook.xml.rels",
        ] {
            let xml = String::from_utf8(base[part].clone()).unwrap();
            let close = xml.rfind("</").unwrap();
            let root = format!("{}<!--note-->{}", &xml[..close], &xml[close..]).replacen(
                '>',
                " xmlns:x='urn:x'>",
                1,
            );
            let mut damaged = base.clone();
            damaged.insert(part.into(), damage(&root));
            let error = Workbook::from_bytes(archive(&damaged)).err().unwrap();
            assert_eq!(
                discriminant(&error),
                discriminant(expected),
                "{part}, {case}: {error}"
            );
        }
    }
}

#[test]
fn discovery_names_duplicate_targets_and_misplaced_sheets_as_invalid() {
    let mut parts = parts(&[("Data", worksheet(CELL)), ("Alias", worksheet(CELL))]);
    let rels = String::from_utf8(parts["xl/_rels/workbook.xml.rels"].clone()).unwrap();
    parts.insert(
        "xl/_rels/workbook.xml.rels".into(),
        rels.replace("worksheets/sheet2.xml", "worksheets/sheet1.xml")
            .into_bytes(),
    );
    let error = Workbook::from_bytes(archive(&parts)).err().unwrap();
    assert!(
        matches!(&error, Error::InvalidWorkbook(message) if message == "duplicate worksheet target xl/worksheets/sheet1.xml"),
        "{error:?}"
    );

    for sheets in [
        "<sheets/><extension><sheet name='Data' sheetId='1' r:id='s1'/></extension>",
        "<sheets><group><sheet name='Data' sheetId='1' r:id='s1'/></group></sheets>",
    ] {
        let mut parts = parts.clone();
        parts.insert(
            "xl/workbook.xml".into(),
            format!("<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'>{sheets}</workbook>")
                .into_bytes(),
        );
        let error = Workbook::from_bytes(archive(&parts)).err().unwrap();
        assert!(
            matches!(&error, Error::InvalidWorkbook(message) if message.contains("sheet")),
            "{sheets}: {error:?}"
        );
    }
}
