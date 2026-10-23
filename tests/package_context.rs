//! Worksheet-level errors name their worksheet, errors in other parts read for
//! a worksheet name those parts instead, and every entry point reports the
//! same mistake with the same error.
mod common;

use sheetpatch::{CellEdit, CellRef, CellValue, Error, Workbook};
use std::collections::BTreeMap;
use std::mem;

const MAIN_NAMESPACE: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const OFFICE_NAMESPACE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE_NAMESPACE: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const CONTENT_TYPES: &str = "<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='rels' ContentType='application/vnd.openxmlformats-package.relationships+xml'/><Default Extension='xml' ContentType='application/xml'/></Types>";
const CELL: &str = "<sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData>";

fn worksheet(body: &str) -> Vec<u8> {
    format!("<worksheet xmlns='{MAIN_NAMESPACE}'>{body}</worksheet>").into_bytes()
}

/// The parts of a workbook whose worksheets have the given names and XML.
fn parts(sheets: &[(&str, Vec<u8>)]) -> BTreeMap<String, Vec<u8>> {
    use std::fmt::Write as _;

    let mut entries = String::new();
    let mut relationships = String::new();
    let mut parts = BTreeMap::new();
    for (number, (name, xml)) in sheets.iter().enumerate() {
        let number = number + 1;
        let name = name.replace('&', "&amp;").replace('"', "&quot;");
        write!(
            entries,
            "<sheet name=\"{name}\" sheetId='{number}' r:id='s{number}'/>"
        )
        .unwrap();
        write!(
            relationships,
            "<Relationship Id='s{number}' Type='{OFFICE_NAMESPACE}/worksheet' Target='worksheets/sheet{number}.xml'/>"
        )
        .unwrap();
        parts.insert(format!("xl/worksheets/sheet{number}.xml"), xml.clone());
    }
    parts.insert("[Content_Types].xml".into(), CONTENT_TYPES.into());
    parts.insert(
        "_rels/.rels".into(),
        format!("<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='office' Type='{OFFICE_NAMESPACE}/officeDocument' Target='xl/workbook.xml'/></Relationships>").into_bytes(),
    );
    parts.insert(
        "xl/workbook.xml".into(),
        format!("<workbook xmlns='{MAIN_NAMESPACE}' xmlns:r='{OFFICE_NAMESPACE}'><sheets>{entries}</sheets></workbook>").into_bytes(),
    );
    parts.insert(
        "xl/_rels/workbook.xml.rels".into(),
        format!("<Relationships xmlns='{PACKAGE_NAMESPACE}'>{relationships}</Relationships>")
            .into_bytes(),
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

/// The worksheet, cell, and reason of a protected-cell refusal.
#[derive(Debug, PartialEq)]
struct Refusal<'error> {
    sheet: &'error str,
    cell: String,
    reason: &'error str,
}

/// The refusal that `error`, which must be a protected-cell error, reports.
fn protected(error: &Error) -> Refusal<'_> {
    match error {
        Error::ProtectedCell {
            sheet,
            cell,
            reason,
            ..
        } => Refusal {
            sheet,
            cell: cell.to_string(),
            reason,
        },
        other => panic!("expected a protected-cell refusal, got {other:?}"),
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
            CellEdit::try_new("Alpha", "B1", 1).unwrap(),
            CellEdit::try_new("Beta", "A1", 5).unwrap(),
        ])
        .unwrap_err();
    assert_eq!(
        protected(&error),
        Refusal {
            sheet: "Beta",
            cell: "A1".into(),
            reason: "contains a formula",
        }
    );
    assert_eq!(
        error.to_string(),
        "protected cell: worksheet \"Beta\": A1 contains a formula"
    );
    assert!(!book.has_changes());
    assert_eq!(book.to_bytes().unwrap(), original);

    for result in [
        book.set_cell("Beta", "B2", 3),
        book.set_cell_at("Beta", CellRef::try_new(2, 2).unwrap(), 3),
        book.set_cells("Beta", [("B2", 3)]),
        book.set_cells_at("Beta", [(CellRef::try_new(2, 2).unwrap(), 3)]),
    ] {
        let error = result.unwrap_err();
        assert_eq!(
            protected(&error),
            Refusal {
                sheet: "Beta",
                cell: "B2".into(),
                reason: "has value metadata (a linked or rich data type)",
            }
        );
    }
    // Workbook-level refusals and value errors carry no worksheet.
    assert!(matches!(
        book.set_cell("Alpha", "A1", f64::NAN),
        Err(Error::InvalidValue(message)) if !message.contains("worksheet")
    ));
    // A single-cell edit's caller holds the cell, but a batch's value error
    // names the cell it concerns.
    let (a1, b7) = (
        CellRef::try_new(1, 1).unwrap(),
        CellRef::try_new(7, 2).unwrap(),
    );
    for result in [
        book.set_cell("Alpha", "B7", f64::NAN),
        book.set_cell_at("Alpha", b7, f64::NAN),
    ] {
        assert!(
            matches!(&result, Err(Error::InvalidValue(message)) if message == "numbers must be finite"),
            "{result:?}"
        );
    }
    for result in [
        book.set_cells("Alpha", [("A1", 1.0), ("B7", f64::NAN)]),
        book.set_cells_at("Alpha", [(a1, 1.0), (b7, f64::NAN)]),
    ] {
        let error = result.unwrap_err();
        let Error::InvalidValue(message) = &error else {
            panic!("expected a value error, got {error:?}");
        };
        assert!(
            message.starts_with("B7: ") && !message.contains("worksheet"),
            "{message}"
        );
        assert_eq!(
            error.to_string(),
            "invalid cell value: B7: numbers must be finite"
        );
    }
    assert!(!book.has_changes());
    book.set_cell("Alpha", "B1", 1).unwrap();
}

#[test]
fn cross_sheet_repairs_keep_unreadable_shared_string_errors_and_atomicity() {
    for shared_strings in [
        None,
        Some("<sst xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'>"),
    ] {
        let sheet = worksheet(
            "<sheetData><row r='1'><c r='A1' t='s'><v>0</v></c><c r='B1'><f>1+1</f><v>2</v></c></row></sheetData>",
        );
        let mut parts = parts(&[
            ("Alpha", sheet.clone()),
            ("Beta", sheet.clone()),
            ("Gamma", sheet),
        ]);
        let relationships = str::from_utf8(&parts["xl/_rels/workbook.xml.rels"])
            .unwrap()
            .replace(
                "</Relationships>",
                &format!(
                    "<Relationship Id='strings' Type='{OFFICE_NAMESPACE}/sharedStrings' Target='sharedStrings.xml'/></Relationships>"
                ),
            );
        parts.insert(
            "xl/_rels/workbook.xml.rels".into(),
            relationships.into_bytes(),
        );
        if let Some(xml) = shared_strings {
            parts.insert("xl/sharedStrings.xml".into(), xml.as_bytes().to_vec());
        }
        let original = archive(&parts);
        let mut book = Workbook::from_bytes(original.clone()).unwrap();
        let expected = book.get_cell("Alpha", "A1").unwrap_err();
        assert!(message(&expected).starts_with("part \"xl/sharedStrings.xml\": "));

        // Earlier repairable cells are staged, but a later refusal still
        // leaves every worksheet and the unreadable shared-string part intact.
        let error = book
            .apply_edits([
                CellEdit::try_new("Alpha", "A1", "first").unwrap(),
                CellEdit::try_new("Beta", "A1", "second").unwrap(),
                CellEdit::try_new("Gamma", "B1", 7).unwrap(),
            ])
            .unwrap_err();
        assert_eq!(
            protected(&error),
            Refusal {
                sheet: "Gamma",
                cell: "B1".into(),
                reason: "contains a formula",
            }
        );
        assert_eq!(book.to_bytes().unwrap(), original);
        assert_eq!(
            book.get_cell("Beta", "A1").unwrap_err().to_string(),
            expected.to_string()
        );

        book.apply_edits([
            CellEdit::try_new("Alpha", "A1", "first").unwrap(),
            CellEdit::try_new("Beta", "A1", "second").unwrap(),
        ])
        .unwrap();
        let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
        assert_eq!(
            reopened.get_cell("Alpha", "A1").unwrap().value,
            "first".into()
        );
        assert_eq!(
            reopened.get_cell("Beta", "A1").unwrap().value,
            "second".into()
        );
        // Memoizing an optional edit lookup must not turn later failed reads
        // into a missing-table or missing-index error, in either workbook.
        for workbook in [&book, &reopened] {
            let actual = workbook.get_cell("Gamma", "A1").unwrap_err();
            assert_eq!(mem::discriminant(&actual), mem::discriminant(&expected));
            assert_eq!(actual.to_string(), expected.to_string());
        }
    }
}

#[test]
fn worksheet_names_are_debug_quoted() {
    let mut book = book(&[(
        "Q\"1 & \\",
        worksheet("<sheetData><row r='1'><c r='A1'><f>1</f><v>1</v></c></row></sheetData>"),
    )]);
    let error = book.set_cell("Q\"1 & \\", "A1", 2).unwrap_err();
    assert_eq!(protected(&error).sheet, "Q\"1 & \\");
    assert_eq!(
        error.to_string(),
        "protected cell: worksheet \"Q\\\"1 & \\\\\": A1 contains a formula"
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
    let cases: [(&str, Vec<u8>, Error); 6] = [
        (
            "unsorted rows",
            worksheet("<sheetData><row r='2'/><row r='1'/></sheetData>"),
            Error::InvalidWorkbook(String::new()),
        ),
        (
            "rows without numbers",
            worksheet("<sheetData><row/></sheetData>"),
            Error::Unsupported(String::new()),
        ),
        (
            "malformed XML",
            format!("<worksheet xmlns='{MAIN_NAMESPACE}'><sheetData></worksheet>").into_bytes(),
            Error::Xml(String::new()),
        ),
        (
            "unreadable value",
            worksheet("<sheetData><row r='1'><c r='A1' t='b'><v>2</v></c></row></sheetData>"),
            Error::InvalidWorkbook(String::new()),
        ),
        (
            "not a worksheet",
            format!("<chartsheet xmlns='{MAIN_NAMESPACE}'/>").into_bytes(),
            Error::InvalidWorkbook(String::new()),
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
            mem::discriminant(&direct),
            mem::discriminant(&expected),
            "{case}: {direct}"
        );
        assert_eq!(prepared.to_string(), direct.to_string(), "{case}");
        assert!(
            message(&direct).starts_with("worksheet \"Beta\": "),
            "{case}: {direct}"
        );
        for error in [
            book.get_cells("Beta", ["A1", "B2"]).unwrap_err(),
            book.get_cell_at("Beta", CellRef::try_new(1, 1).unwrap())
                .unwrap_err(),
        ] {
            assert_eq!(error.to_string(), direct.to_string(), "{case}");
        }
        // Edits fail with the same context, other than for a value that only
        // a read rejects.
        let edit = book.set_cell("Beta", "C3", 1);
        if case != "unreadable value" {
            let edit = edit.unwrap_err();
            assert_eq!(
                mem::discriminant(&edit),
                mem::discriminant(&expected),
                "{case}"
            );
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
    let a1 = CellRef::try_new(1, 1).unwrap();
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
    let sheet = format!(
        "<worksheet xmlns='{MAIN_NAMESPACE}' xmlns:x='urn:x'><!--note-->{CELL}</worksheet>"
    );
    for (case, damage, expected) in &cases {
        // The worksheet is read lazily, so the error appears on first access.
        let book = book(&[("Data", damage(&sheet))]);
        let worksheet_error = book.get_cell("Data", "A1").unwrap_err();
        assert_eq!(
            mem::discriminant(&worksheet_error),
            mem::discriminant(expected),
            "worksheet, {case}: {worksheet_error}"
        );

        let base = parts(&[("Data", worksheet(CELL))]);
        for part in [
            "[Content_Types].xml",
            "_rels/.rels",
            "xl/workbook.xml",
            "xl/_rels/workbook.xml.rels",
        ] {
            let xml = str::from_utf8(&base[part]).unwrap();
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
                mem::discriminant(&error),
                mem::discriminant(expected),
                "{part}, {case}: {error}"
            );
        }
    }
}

#[test]
fn discovery_names_duplicate_targets_and_misplaced_sheets_as_invalid() {
    let mut parts = parts(&[("Data", worksheet(CELL)), ("Alias", worksheet(CELL))]);
    let relationships = str::from_utf8(&parts["xl/_rels/workbook.xml.rels"])
        .unwrap()
        .replace("worksheets/sheet2.xml", "worksheets/sheet1.xml");
    parts.insert(
        "xl/_rels/workbook.xml.rels".into(),
        relationships.into_bytes(),
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
            format!("<workbook xmlns='{MAIN_NAMESPACE}' xmlns:r='{OFFICE_NAMESPACE}'>{sheets}</workbook>")
                .into_bytes(),
        );
        let error = Workbook::from_bytes(archive(&parts)).err().unwrap();
        assert!(
            matches!(&error, Error::InvalidWorkbook(message) if message.contains("sheet")),
            "{sheets}: {error:?}"
        );
    }
}

/// A worksheet holding one cell of each protected kind, with its relationships
/// to an Excel table on J1:K3 whose header row is J1:K1.
fn guarded_parts() -> BTreeMap<String, Vec<u8>> {
    let guarded = format!(
        "<worksheet xmlns='{MAIN_NAMESPACE}' xmlns:r='{OFFICE_NAMESPACE}'><sheetData><row r='1'>\
         <c r='A1'><f>1+1</f><v>2</v></c>\
         <c r='B1'><f t='shared' si='0' ref='B1:C2'>1</f><v>1</v></c><c r='C1'><f t='shared' si='0'/><v>1</v></c>\
         <c r='D1'><f t='array' ref='D1:D3'>1</f><v>1</v></c>\
         <c r='E1'><f t='dataTable' ref='E1:E3' dt2D='0' dtr='0' r1='A1'/><v>1</v></c>\
         <c r='F1'><v>1</v></c>\
         <c r='H1' vm='1'><v>1</v></c>\
         <c r='J1' t='inlineStr'><is><t>Name</t></is></c><c r='K1' t='inlineStr'><is><t>Size</t></is></c>\
         </row></sheetData><mergeCells count='1'><mergeCell ref='F1:G2'/></mergeCells>\
         <tableParts count='1'><tablePart r:id='t1'/></tableParts></worksheet>"
    );
    let mut parts = parts(&[("Alpha", worksheet(CELL)), ("Beta", guarded.into_bytes())]);
    parts.insert(
        "xl/worksheets/_rels/sheet2.xml.rels".into(),
        format!("<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='t1' Type='{OFFICE_NAMESPACE}/table' Target='../tables/table1.xml'/></Relationships>").into_bytes(),
    );
    parts.insert(
        "xl/tables/table1.xml".into(),
        format!("<table xmlns='{MAIN_NAMESPACE}' id='1' name='T' displayName='T' ref='J1:K3'><tableColumns count='2'><tableColumn id='1' name='Name'/><tableColumn id='2' name='Size'/></tableColumns></table>").into_bytes(),
    );
    parts
}

#[test]
fn every_protected_cell_names_its_worksheet_through_every_edit_method() {
    let mut book = Workbook::from_bytes(archive(&guarded_parts())).unwrap();
    for (address, value, reason) in [
        ("A1", CellValue::from(2), "contains a formula"),
        ("C1", CellValue::Blank, "contains a formula"),
        ("C2", CellValue::from("x"), "belongs to a formula range"),
        ("D3", CellValue::from(true), "belongs to a formula range"),
        ("E2", CellValue::from(1), "belongs to a formula range"),
        (
            "G2",
            CellValue::from(1),
            "is merged into another cell's range",
        ),
        (
            "J1",
            CellValue::from("Label"),
            "is an Excel table header cell; editing it would desynchronize the table's column names",
        ),
        (
            "K1",
            CellValue::Blank,
            "is an Excel table header cell; editing it would desynchronize the table's column names",
        ),
        (
            "H1",
            CellValue::from(5),
            "has value metadata (a linked or rich data type)",
        ),
    ] {
        let cell: CellRef = address.parse().unwrap();
        let alpha = CellEdit::try_new("Alpha", "B1", 1).unwrap();
        let edit = CellEdit::try_new_at("Beta", cell, value.clone()).unwrap();
        for (method, result) in [
            ("set_cell", book.set_cell("Beta", address, value.clone())),
            ("set_cell_at", book.set_cell_at("Beta", cell, value.clone())),
            (
                "set_cells",
                book.set_cells(
                    "Beta",
                    [("F1", CellValue::from(3)), (address, value.clone())],
                ),
            ),
            (
                "set_cells_at",
                book.set_cells_at("Beta", [(cell, value.clone())]),
            ),
            ("apply_edits", book.apply_edits([alpha, edit])),
        ] {
            let error = result.unwrap_err();
            assert_eq!(
                protected(&error),
                Refusal {
                    sheet: "Beta",
                    cell: address.to_owned(),
                    reason,
                },
                "{method} {address}"
            );
            // The worksheet is named once, by the variant's own message.
            assert_eq!(
                error.to_string(),
                format!("protected cell: worksheet \"Beta\": {address} {reason}"),
                "{method} {address}"
            );
            assert!(!book.has_changes(), "{method} {address}");
        }
    }
    // Rewriting a header's or a value-metadata cell's current value is allowed,
    // but H1 holds the number 1, not the text "1"; rewriting a formula's cached
    // value is never allowed.
    let error = book
        .set_cells("Beta", [("J1", "Name"), ("H1", "1")])
        .unwrap_err();
    assert_eq!(protected(&error).cell, "H1");
    book.set_cells(
        "Beta",
        [("J1", CellValue::from("Name")), ("H1", CellValue::from(1))],
    )
    .unwrap();
    assert!(!book.has_changes());
    assert_eq!(
        protected(&book.set_cell("Beta", "A1", 2).unwrap_err()).reason,
        "contains a formula"
    );
    // Worksheet-level refusals stay unsupported and name the worksheet.
    let incomplete = archive(&parts(&[(
        "Beta",
        worksheet(
            "<sheetData><row r='1'><c r='A1'><f t='array'>1</f><v>1</v></c></row></sheetData>",
        ),
    )]));
    let mut book = Workbook::from_bytes(incomplete).unwrap();
    let error = book.set_cell("Beta", "Z9", 1).unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    assert_eq!(
        message(&error),
        "worksheet \"Beta\": A1 belongs to a formula group without a reference range"
    );
}

/// Adds a shared-string table to the workbook's parts.
fn add_shared_strings(parts: &mut BTreeMap<String, Vec<u8>>, table: &[u8]) {
    let relationships = str::from_utf8(&parts["xl/_rels/workbook.xml.rels"])
        .unwrap()
        .replace(
            "</Relationships>",
            &format!("<Relationship Id='strings' Type='{OFFICE_NAMESPACE}/sharedStrings' Target='sharedStrings.xml'/></Relationships>"),
        );
    parts.insert(
        "xl/_rels/workbook.xml.rels".into(),
        relationships.into_bytes(),
    );
    parts.insert("xl/sharedStrings.xml".into(), table.to_vec());
}

/// A Data worksheet whose A1 is the shared string at index 0 and B1 a number,
/// with the given shared-string part, or none.
fn shared_string_book(table: Option<&[u8]>) -> Workbook {
    let data = worksheet(
        "<sheetData><row r='1'><c r='A1' t='s'><v>0</v></c><c r='B1'><v>2</v></c></row></sheetData>",
    );
    let mut parts = parts(&[("Data", data)]);
    if let Some(table) = table {
        add_shared_strings(&mut parts, table);
    }
    Workbook::from_bytes(archive(&parts)).unwrap()
}

#[test]
fn shared_string_table_errors_name_their_part_rather_than_the_worksheet() {
    let table = |body: &str| format!("<sst xmlns='{MAIN_NAMESPACE}'>{body}</sst>").into_bytes();
    let cases: [(&str, Vec<u8>, Error); 4] = [
        (
            "truncated",
            format!("<sst xmlns='{MAIN_NAMESPACE}'><si><t>a</t></si>").into_bytes(),
            Error::Xml(String::new()),
        ),
        (
            "undeclared entity",
            table("<si><t>a&bogus;</t></si>"),
            Error::Xml(String::new()),
        ),
        (
            "string outside the table",
            table("<x><si><t>a</t></si></x>"),
            Error::Unsupported(String::new()),
        ),
        (
            "wrong root",
            format!("<worksheet xmlns='{MAIN_NAMESPACE}'/>").into_bytes(),
            Error::InvalidWorkbook(String::new()),
        ),
    ];
    for (case, xml, expected) in cases {
        let mut book = shared_string_book(Some(&xml));
        let view = book.prepare_sheet("Data").unwrap();
        let a1 = CellRef::try_new(1, 1).unwrap();
        let errors = [
            book.get_cell("Data", "A1").unwrap_err(),
            book.get_cell_at("Data", a1).unwrap_err(),
            book.get_cells("Data", ["B1", "A1"]).unwrap_err(),
            book.get_cells_at("Data", [a1]).unwrap_err(),
            view.get_cell("A1").unwrap_err(),
            view.get_cells_at([a1]).unwrap_err(),
        ];
        for error in &errors {
            assert_eq!(
                mem::discriminant(error),
                mem::discriminant(&expected),
                "{case}: {error}"
            );
            let message = message(error);
            assert!(
                message.starts_with("part \"xl/sharedStrings.xml\": "),
                "{case}: {message}"
            );
            assert!(!message.contains("worksheet"), "{case}: {message}");
            assert_eq!(error.to_string(), errors[0].to_string(), "{case}");
        }
        // Cells that need no shared string stay readable.
        assert_eq!(view.get_cell("B1").unwrap().value, CellValue::Number(2.0));
        drop(view);
        // An edit only compares against the table, so a damaged table leaves
        // rewriting the cell a change rather than an error.
        book.set_cell("Data", "A1", "a").unwrap();
        assert!(book.has_changes(), "{case}");
    }
}

#[test]
fn table_headers_read_through_a_damaged_shared_string_table_name_that_part() {
    let mut parts = guarded_parts();
    let beta = str::from_utf8(&parts["xl/worksheets/sheet2.xml"])
        .unwrap()
        .replace(
            "<c r='J1' t='inlineStr'><is><t>Name</t></is></c>",
            "<c r='J1' t='s'><v>0</v></c>",
        );
    parts.insert("xl/worksheets/sheet2.xml".into(), beta.into_bytes());
    add_shared_strings(
        &mut parts,
        format!("<sst xmlns='{MAIN_NAMESPACE}'><si><t>Name</t></si>").as_bytes(),
    );
    let mut book = Workbook::from_bytes(archive(&parts)).unwrap();
    let error = book.set_cell("Beta", "J1", "Other").unwrap_err();
    assert!(
        matches!(&error, Error::Xml(message) if message.starts_with("part \"xl/sharedStrings.xml\": ")),
        "{error:?}"
    );
    assert!(!book.has_changes());
}

#[test]
fn shared_string_cells_without_a_table_are_invalid_cells_of_their_worksheet() {
    let book = shared_string_book(None);
    let expected =
        "worksheet \"Data\": A1: shared-string cell, but the workbook has no shared-string table";
    for error in [
        book.get_cell("Data", "A1").unwrap_err(),
        book.prepare_sheet("Data")
            .unwrap()
            .get_cell("A1")
            .unwrap_err(),
    ] {
        assert!(
            matches!(&error, Error::InvalidWorkbook(message) if message == expected),
            "{error:?}"
        );
    }
    assert_eq!(
        book.get_cell("Data", "B1").unwrap().value,
        CellValue::Number(2.0)
    );
}

#[test]
fn an_undecodable_shared_string_refuses_only_the_cells_that_use_it() {
    // An Excel table on A1:B2 whose headers are shared string 0, "Name", and
    // string 1, an unpaired surrogate escape: valid, but not a Rust string.
    let data = format!(
        "<worksheet xmlns='{MAIN_NAMESPACE}' xmlns:r='{OFFICE_NAMESPACE}'><sheetData>\
         <row r='1'><c r='A1' t='s'><v>0</v></c><c r='B1' t='s'><v>1</v></c></row>\
         <row r='2'><c r='A2' t='s'><v>1</v></c><c r='B2'><v>2</v></c></row>\
         </sheetData><tableParts count='1'><tablePart r:id='t1'/></tableParts></worksheet>"
    );
    let mut parts = parts(&[("Data", data.into_bytes())]);
    add_shared_strings(
        &mut parts,
        format!("<sst xmlns='{MAIN_NAMESPACE}'><si><t>Name</t></si><si><t>_xD83D_</t></si></sst>")
            .as_bytes(),
    );
    parts.insert(
        "xl/worksheets/_rels/sheet1.xml.rels".into(),
        format!("<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='t1' Type='{OFFICE_NAMESPACE}/table' Target='../tables/table1.xml'/></Relationships>").into_bytes(),
    );
    parts.insert(
        "xl/tables/table1.xml".into(),
        format!("<table xmlns='{MAIN_NAMESPACE}' id='1' name='T' displayName='T' ref='A1:B2'><tableColumns count='2'><tableColumn id='1' name='Name'/><tableColumn id='2' name='Other'/></tableColumns></table>").into_bytes(),
    );
    let original = archive(&parts);
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    let expected =
        "part \"xl/sharedStrings.xml\": string 1: unpaired UTF-16 surrogate in Excel text escape";
    let view = book.prepare_sheet("Data").unwrap();
    let a2 = CellRef::try_new(2, 1).unwrap();
    for error in [
        book.get_cell("Data", "A2").unwrap_err(),
        book.get_cell_at("Data", a2).unwrap_err(),
        book.get_cells("Data", ["A1", "A2"]).unwrap_err(),
        book.get_cells_at("Data", [a2]).unwrap_err(),
        view.get_cell("B1").unwrap_err(),
        view.get_cells(["A1", "B1"]).unwrap_err(),
    ] {
        assert!(
            matches!(&error, Error::Unsupported(message) if message == expected),
            "{error:?}"
        );
    }
    // Every other cell reads normally, including the other shared string.
    let values: Vec<CellValue> = view
        .get_cells(["A1", "B2"])
        .unwrap()
        .into_iter()
        .map(|content| content.value)
        .collect();
    assert_eq!(values, [CellValue::from("Name"), CellValue::Number(2.0)]);
    drop(view);

    // Rewriting the header that is string 0 with its value is a no-op, and
    // changing it is still refused as a header change.
    book.set_cell("Data", "A1", "Name").unwrap();
    book.set_cells("Data", [("A1", "Name")]).unwrap();
    assert!(!book.has_changes());
    let error = book.set_cell("Data", "A1", "Renamed").unwrap_err();
    assert_eq!(protected(&error).cell, "A1");
    // The header that is string 1 cannot be compared with any value, so every
    // edit of it is refused, even one that would match its empty placeholder.
    for value in ["", "Other"] {
        let error = book.set_cell("Data", "B1", value).unwrap_err();
        assert!(
            matches!(&error, Error::Unsupported(message) if message == expected),
            "{error:?}"
        );
    }
    assert_eq!(book.to_bytes().unwrap(), original);

    // A body cell that is string 1 can be replaced, and rewriting it counts
    // as a change.
    book.set_cells("Data", [("A1", "Name"), ("A2", "")])
        .unwrap();
    assert!(book.has_changes());
    let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    assert_eq!(
        reopened.get_cell("Data", "A2").unwrap().value,
        CellValue::from("")
    );
    assert_eq!(
        reopened.get_cell("Data", "A1").unwrap().value,
        CellValue::from("Name")
    );
}

#[test]
fn every_part_without_a_root_element_is_incomplete_xml() {
    for part in [
        "[Content_Types].xml",
        "_rels/.rels",
        "xl/workbook.xml",
        "xl/_rels/workbook.xml.rels",
        "xl/worksheets/sheet1.xml",
        "xl/sharedStrings.xml",
        "xl/worksheets/_rels/sheet1.xml.rels",
        "xl/tables/table1.xml",
    ] {
        for rootless in ["", "<!--comment-->", "<?xml version='1.0'?>"] {
            let mut parts = every_part();
            assert!(parts.insert(part.into(), rootless.into()).is_some());
            // Opening the workbook, reading its cells, and editing the table
            // body parse every part.
            let result = Workbook::from_bytes(archive(&parts)).and_then(|mut book| {
                book.get_cells("Data", ["A1", "A2"])?;
                book.set_cell("Data", "A2", 5)
            });
            assert!(
                matches!(&result, Err(Error::Xml(message)) if message.ends_with("incomplete XML document")),
                "{part} {rootless:?}: {result:?}"
            );
        }
    }
}

/// A Data worksheet whose A1 is the shared string at index 0 and A2 a number,
/// in an Excel table on A1:A2 whose header row is A1.
fn every_part() -> BTreeMap<String, Vec<u8>> {
    let data = format!(
        "<worksheet xmlns='{MAIN_NAMESPACE}' xmlns:r='{OFFICE_NAMESPACE}'><sheetData>\
         <row r='1'><c r='A1' t='s'><v>0</v></c></row><row r='2'><c r='A2'><v>1</v></c></row>\
         </sheetData><tableParts count='1'><tablePart r:id='t1'/></tableParts></worksheet>"
    );
    let mut parts = parts(&[("Data", data.into_bytes())]);
    add_shared_strings(
        &mut parts,
        format!("<sst xmlns='{MAIN_NAMESPACE}'><si><t>H</t></si></sst>").as_bytes(),
    );
    parts.insert(
        "xl/worksheets/_rels/sheet1.xml.rels".into(),
        format!("<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='t1' Type='{OFFICE_NAMESPACE}/table' Target='../tables/table1.xml'/></Relationships>").into_bytes(),
    );
    parts.insert(
        "xl/tables/table1.xml".into(),
        format!("<table xmlns='{MAIN_NAMESPACE}' id='1' name='T' displayName='T' ref='A1:A2'><tableColumns count='1'><tableColumn id='1' name='H'/></tableColumns></table>").into_bytes(),
    );
    parts
}

#[test]
fn each_kind_of_defect_has_one_variant_in_every_part() {
    const WORKBOOK: &str = "xl/workbook.xml";
    const SHEET: &str = "xl/worksheets/sheet1.xml";
    const TABLE: &str = "xl/tables/table1.xml";
    const STRINGS: &str = "xl/sharedStrings.xml";
    let strict = "xmlns='http://purl.oclc.org/ooxml/spreadsheetml/main'";
    let branch = |contents: &str| {
        format!(
            "<mc:AlternateContent xmlns:mc='http://schemas.openxmlformats.org/markup-compatibility/2006'><mc:Choice Requires='x'>{contents}</mc:Choice></mc:AlternateContent>"
        )
    };
    // The same contents in both branches, so that either one is valid.
    let both = |contents: &str| {
        format!(
            "<mc:AlternateContent xmlns:mc='http://schemas.openxmlformats.org/markup-compatibility/2006'><mc:Choice Requires='x'>{contents}</mc:Choice><mc:Fallback>{contents}</mc:Fallback></mc:AlternateContent>"
        )
    };
    let sheets = "<sheets><sheet name=\"Data\" sheetId='1' r:id='s1'/></sheets>";
    let sheet_data = "<sheetData><row r='1'><c r='A1' t='s'><v>0</v></c></row><row r='2'><c r='A2'><v>1</v></c></row></sheetData>";
    // Each defect replaces text in one part.
    let malformed: [(&str, &str, &str, String); 21] = [
        ("wrong root", WORKBOOK, "workbook", "document".into()),
        ("wrong root", SHEET, "worksheet", "document".into()),
        ("wrong root", TABLE, "table", "document".into()),
        ("wrong root", STRINGS, "sst", "document".into()),
        (
            "repeated container",
            WORKBOOK,
            "</sheets>",
            "</sheets><sheets/>".into(),
        ),
        (
            "repeated container",
            SHEET,
            "</sheetData>",
            "</sheetData><sheetData/>".into(),
        ),
        ("invalid range", TABLE, "A1:A2", "nope".into()),
        (
            "invalid range",
            SHEET,
            "</sheetData>",
            "</sheetData><mergeCells><mergeCell ref='nope'/></mergeCells>".into(),
        ),
        (
            "invalid range",
            SHEET,
            "<v>1</v>",
            "<f t='array' ref='nope'>1</f><v>1</v>".into(),
        ),
        ("missing range", TABLE, " ref='A1:A2'", String::new()),
        (
            "missing range",
            SHEET,
            "</sheetData>",
            "</sheetData><mergeCells><mergeCell/></mergeCells>".into(),
        ),
        ("invalid cell reference", SHEET, "r='A2'", "r='nope'".into()),
        (
            "ill-typed attribute",
            WORKBOOK,
            "sheetId='1'",
            "sheetId='1' state='x'".into(),
        ),
        (
            "ill-typed attribute",
            TABLE,
            "ref='A1:A2'",
            "ref='A1:A2' headerRowCount='x'".into(),
        ),
        (
            "ill-typed attribute",
            SHEET,
            "<row r='2'>",
            "<row r='x'>".into(),
        ),
        (
            "ill-typed attribute",
            SHEET,
            "r='A2'",
            "r='A2' s='x'".into(),
        ),
        (
            "ill-typed attribute",
            SHEET,
            "<v>1</v>",
            "<f t='shared' si='x' ref='A2'>1</f><v>1</v>".into(),
        ),
        (
            "duplicate",
            WORKBOOK,
            "</sheets>",
            "</sheets><workbookPr/><workbookPr/>".into(),
        ),
        (
            "duplicate",
            SHEET,
            "<row r='2'>",
            "<row r='1'/><row r='2'>".into(),
        ),
        ("duplicate", SHEET, "<v>1</v>", "<v>1</v><v>2</v>".into()),
        (
            "payload of the wrong type",
            SHEET,
            "r='A2'",
            "r='A2' t='inlineStr'".into(),
        ),
    ];
    let unhandled: [(&str, &str, &str, String); 10] = [
        (
            "mixed dialects",
            WORKBOOK,
            "</sheets>",
            format!("</sheets><workbookPr {strict}/>"),
        ),
        (
            "mixed dialects",
            SHEET,
            "</sheetData>",
            format!("</sheetData><extLst {strict}/>"),
        ),
        (
            "mixed dialects",
            STRINGS,
            "</si>",
            format!("</si><extLst {strict}/>"),
        ),
        (
            "hidden content",
            WORKBOOK,
            "</sheets>",
            format!("</sheets>{}", branch("<workbookPr date1904='1'/>")),
        ),
        (
            "hidden content",
            WORKBOOK,
            "</sheets>",
            format!(
                "{}</sheets>",
                both("<sheet name='Hidden' sheetId='2' r:id='s1'/>")
            ),
        ),
        ("hidden content", WORKBOOK, sheets, both(sheets)),
        (
            "hidden content",
            SHEET,
            "</sheetData>",
            format!("{}</sheetData>", branch("<row r='3'/>")),
        ),
        ("hidden content", SHEET, sheet_data, both(sheet_data)),
        (
            "hidden content",
            STRINGS,
            "</sst>",
            format!("{}</sst>", branch("<si><t>hidden</t></si>")),
        ),
        (
            "missing optional reference",
            SHEET,
            "<row r='2'>",
            "<row>".into(),
        ),
    ];
    // The first error from opening the workbook, reading its cells, or
    // editing the table body, which reads the table part.
    let first_error = |parts: &BTreeMap<String, Vec<u8>>| {
        let mut book = Workbook::from_bytes(archive(parts))?;
        book.get_cells("Data", ["A1", "A2"])?;
        book.set_cell("Data", "A2", 5)
    };
    assert!(first_error(&every_part()).is_ok());
    // Malformed or ambiguous content makes the workbook invalid; valid
    // content outside the supported scope is unsupported.
    for (expected, cases) in [
        (Error::InvalidWorkbook(String::new()), &malformed[..]),
        (Error::Unsupported(String::new()), &unhandled[..]),
    ] {
        for (defect, part, from, to) in cases {
            let mut parts = every_part();
            let xml = str::from_utf8(&parts[*part]).unwrap();
            let damaged = xml.replace(from, to);
            assert_ne!(damaged, xml, "{defect} in {part}");
            parts.insert(part.to_string(), damaged.into_bytes());
            let error = first_error(&parts).unwrap_err();
            assert_eq!(
                mem::discriminant(&error),
                mem::discriminant(&expected),
                "{defect} in {part}: {error}"
            );
        }
    }
}
