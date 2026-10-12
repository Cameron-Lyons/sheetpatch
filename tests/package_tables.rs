//! Excel requires each table header cell's text to equal its table column's
//! name, so edits that would change a header cell are refused.
mod common;

use sheetpatch::{CellEdit, CellValue, Error, Workbook};
use std::collections::BTreeMap;

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const STRICT_MAIN_NS: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const STRICT_OFFICE_NS: &str = "http://purl.oclc.org/ooxml/officeDocument/relationships";
const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const TABLE_PATH: &str = "xl/tables/table1.xml";
const SHEET_RELS: &str = "xl/worksheets/_rels/sheet1.xml.rels";

const REFUSAL: &str =
    "is an Excel table header cell; editing it would desynchronize the table's column names";

/// A table on Data!A1:B3 with headers "Name" (a shared string) and "Amount"
/// (an inline string). Notes has no relationships.
fn parts(table_attributes: &str) -> BTreeMap<String, String> {
    let data = format!(
        "<worksheet xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheetData>\
         <row r='1'><c r='A1' t='s'><v>0</v></c><c r='B1' t='inlineStr'><is><t>Amount</t></is></c><c r='C1'><v>9</v></c></row>\
         <row r='2'><c r='A2' t='inlineStr'><is><t>x</t></is></c><c r='B2'><v>5</v></c></row>\
         <row r='3'><c r='A3' t='inlineStr'><is><t>y</t></is></c><c r='B3'><v>6</v></c></row>\
         </sheetData><tableParts count='1'><tablePart r:id='table'/></tableParts></worksheet>"
    );
    let notes = format!(
        "<worksheet xmlns='{MAIN_NS}'><sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData></worksheet>"
    );
    BTreeMap::from([
        (
            "[Content_Types].xml".into(),
            "<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>".into(),
        ),
        (
            "_rels/.rels".into(),
            format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='office' Type='{OFFICE_NS}/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ),
        (
            "xl/workbook.xml".into(),
            format!("<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheets><sheet name='Data' sheetId='1' r:id='one'/><sheet name='Notes' sheetId='2' r:id='two'/></sheets></workbook>"),
        ),
        (
            "xl/_rels/workbook.xml.rels".into(),
            format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='one' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet1.xml'/><Relationship Id='two' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet2.xml'/><Relationship Id='strings' Type='{OFFICE_NS}/sharedStrings' Target='sharedStrings.xml'/></Relationships>"),
        ),
        (
            "xl/sharedStrings.xml".into(),
            format!("<sst xmlns='{MAIN_NS}'><si><t>Name</t></si></sst>"),
        ),
        ("xl/worksheets/sheet1.xml".into(), data),
        ("xl/worksheets/sheet2.xml".into(), notes),
        (
            SHEET_RELS.into(),
            format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='link' Type='{OFFICE_NS}/hyperlink' Target='https://example.com/' TargetMode='External'/><Relationship Id='table' Type='{OFFICE_NS}/table' Target='../tables/table1.xml'/></Relationships>"),
        ),
        (
            TABLE_PATH.into(),
            format!("<table xmlns='{MAIN_NS}' id='1' name='Table1' displayName='Table1' {table_attributes}><autoFilter ref='A1:B3'/><tableColumns count='2'><tableColumn id='1' name='Name'/><tableColumn id='2' name='Amount'/></tableColumns></table>"),
        ),
    ])
}

fn archive(parts: &BTreeMap<String, String>) -> Vec<u8> {
    let entries: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(name, xml)| (name.as_str(), xml.as_bytes()))
        .collect();
    common::stored_archive(&entries)
}

fn book(parts: &BTreeMap<String, String>) -> Workbook {
    Workbook::from_bytes(archive(parts)).unwrap()
}

fn refusal(cell: &str) -> String {
    format!("worksheet \"Data\": {cell} {REFUSAL}")
}

fn assert_refused(result: sheetpatch::Result<()>, cell: &str) {
    match result {
        Err(Error::Unsupported(message)) => assert_eq!(message, refusal(cell)),
        other => panic!("expected {cell} to be refused, got {other:?}"),
    }
}

#[test]
fn table_body_and_cells_outside_tables_remain_editable() {
    let mut book = book(&parts("ref='A1:B3'"));
    book.set_cell("Data", "B2", 7).unwrap();
    book.set_cells("Data", [("A3", "z"), ("C1", "beside")])
        .unwrap();
    book.set_cell("Data", "A4", "below").unwrap();
    book.set_cell("Notes", "A1", "free").unwrap();
    let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    assert_eq!(
        reopened.get_cell("Data", "B2").unwrap().value,
        CellValue::Number(7.0)
    );
    assert_eq!(
        reopened.get_cell("Data", "C1").unwrap().value,
        CellValue::from("beside")
    );
}

#[test]
fn edits_that_change_a_header_cell_are_refused_without_changes() {
    for attributes in ["ref='A1:B3'", "ref=' A1:B3 ' headerRowCount=' 1 '"] {
        let original = archive(&parts(attributes));
        let mut book = Workbook::from_bytes(original.clone()).unwrap();
        assert_refused(book.set_cell("Data", "B1", "Total"), "B1");
        assert_refused(book.set_cell("Data", "A1", "name"), "A1");
        assert_refused(book.set_cell("Data", "A1", 1), "A1");
        assert_refused(book.set_cell("Data", "B1", CellValue::Blank), "B1");
        assert_refused(book.set_cells("Data", [("B2", "8"), ("B1", "x")]), "B1");
        assert_refused(
            book.apply_edits([
                CellEdit::new("Notes", "A1", 2).unwrap(),
                CellEdit::new("Data", "A1", "Label").unwrap(),
            ]),
            "A1",
        );
        assert!(!book.has_changes());
        assert_eq!(book.to_bytes().unwrap(), original);
    }
}

#[test]
fn edits_that_keep_a_header_value_are_no_ops() {
    let original = archive(&parts("ref='A1:B3'"));
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    // A1 is a shared string and B1 an inline string.
    book.set_cells("Data", [("A1", "Name"), ("B1", "Amount")])
        .unwrap();
    assert!(!book.has_changes());
    book.set_cells("Data", [("A1", "Name"), ("B2", "8")])
        .unwrap();
    assert_eq!(
        book.get_cell("Data", "B2").unwrap().value,
        CellValue::from("8")
    );
    book.set_cell("Data", "B2", 5).unwrap();
    assert_eq!(book.to_bytes().unwrap(), original);
}

#[test]
fn tables_without_a_header_row_leave_the_first_row_editable() {
    let mut book = book(&parts("ref='A1:B3' headerRowCount='0'"));
    book.set_cell("Data", "A1", "first").unwrap();
    book.set_cell("Data", "B1", 3).unwrap();
    // Several header rows protect each of them.
    let mut book = self::book(&parts("ref='A1:B3' headerRowCount='2'"));
    assert_refused(book.set_cell("Data", "B2", 4), "B2");
    book.set_cell("Data", "B3", 4).unwrap();
}

#[test]
fn strict_table_relationships_and_namespaces_are_recognized() {
    let mut parts = parts("ref='A1:B3'");
    let rels = parts[SHEET_RELS].replace(
        &format!("{OFFICE_NS}/table"),
        &format!("{STRICT_OFFICE_NS}/table"),
    );
    parts.insert(SHEET_RELS.into(), rels);
    let mut book = self::book(&parts);
    assert_refused(book.set_cell("Data", "B1", "Total"), "B1");

    let table = parts[TABLE_PATH].replace(MAIN_NS, STRICT_MAIN_NS);
    parts.insert(TABLE_PATH.into(), table);
    let mut book = self::book(&parts);
    assert_refused(book.set_cell("Data", "B1", "Total"), "B1");
    book.set_cell("Data", "B2", 1).unwrap();
}

#[test]
fn sheets_without_tables_are_unaffected() {
    let mut parts = parts("ref='A1:B3'");
    parts.remove(SHEET_RELS);
    let mut book = self::book(&parts);
    book.set_cell("Data", "B1", "Total").unwrap();

    // Relationships to other parts, and table relationships of other sheets,
    // do not protect anything.
    let mut parts = self::parts("ref='A1:B3'");
    let rels = parts.remove(SHEET_RELS).unwrap();
    parts.insert(
        SHEET_RELS.into(),
        rels.replace(
            &format!("{OFFICE_NS}/table"),
            &format!("{OFFICE_NS}/drawing"),
        ),
    );
    parts.insert(
        "xl/worksheets/_rels/sheet2.xml.rels".into(),
        rels.replace("../tables/table1.xml", "/xl/tables/table1.xml"),
    );
    let mut book = self::book(&parts);
    book.set_cell("Data", "B1", "Total").unwrap();
    assert_eq!(
        book.get_cell("Data", "B1").unwrap().value,
        CellValue::from("Total")
    );
    book.set_cell("Notes", "B5", "free").unwrap();
    assert!(matches!(
        book.set_cell("Notes", "A1", "Total"),
        Err(Error::Unsupported(message)) if message == format!("worksheet \"Notes\": A1 {REFUSAL}")
    ));
}

#[test]
fn malformed_tables_of_an_edited_sheet_are_invalid() {
    let invalid = |change: &dyn Fn(&mut BTreeMap<String, String>)| {
        let mut parts = parts("ref='A1:B3'");
        change(&mut parts);
        let mut book = book(&parts);
        // Reads and other sheets do not need the table.
        assert_eq!(
            book.get_cell("Data", "B2").unwrap().value,
            CellValue::Number(5.0)
        );
        book.set_cell("Notes", "A1", 2).unwrap();
        match book.set_cell("Data", "B2", 7) {
            Err(Error::InvalidWorkbook(message)) => {
                assert!(message.starts_with("worksheet \"Data\": "), "{message}");
                message
            }
            other => panic!("expected an invalid table, got {other:?}"),
        }
    };
    for attributes in [
        "",
        "ref='A1:'",
        "ref='$A$1:$B$3'",
        "ref='A1:B3' headerRowCount='x'",
        "ref='A1:B3' headerRowCount='-1'",
        "ref='A1:B3' headerRowCount='4'",
    ] {
        let attributes = attributes.to_owned();
        let message = invalid(&move |parts| {
            let table = parts[TABLE_PATH].replace("displayName='Table1' ref='A1:B3'", "");
            let table = table.replacen(" id='1'", &format!(" id='1' {attributes}"), 1);
            parts.insert(TABLE_PATH.into(), table);
        });
        assert!(message.contains(TABLE_PATH), "{message}");
    }
    let message = invalid(&|parts| {
        let table = parts[TABLE_PATH]
            .replace("<table ", "<other ")
            .replace("</table>", "</other>");
        parts.insert(TABLE_PATH.into(), table);
    });
    assert!(message.contains(TABLE_PATH), "{message}");
    let message = invalid(&|parts| {
        parts.remove(TABLE_PATH);
    });
    assert!(message.contains(TABLE_PATH), "{message}");
    invalid(&|parts| {
        let rels = parts[SHEET_RELS].replace(
            "Target='../tables/table1.xml'",
            "Target='../tables/table1.xml' TargetMode='External'",
        );
        parts.insert(SHEET_RELS.into(), rels);
    });
    invalid(&|parts| {
        let rels =
            parts[SHEET_RELS].replace("Target='../tables/table1.xml'", "Target='../../../x.xml'");
        parts.insert(SHEET_RELS.into(), rels);
    });
}
