//! Enumerating a worksheet's cells and its used range.
mod common;

use sheetpatch::{CellRange, CellRef, CellValue, Workbook};

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

fn book(worksheet: &str) -> Workbook {
    let workbook = format!(
        "<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheets><sheet name='Data' sheetId='1' r:id='one'/></sheets></workbook>"
    );
    let root = format!(
        "<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='office' Type='{OFFICE_NS}/officeDocument' Target='xl/workbook.xml'/></Relationships>"
    );
    let rels = format!(
        "<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='one' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet1.xml'/></Relationships>"
    );
    let bytes = common::stored_archive(&[
        (
            "[Content_Types].xml",
            b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>",
        ),
        ("_rels/.rels", root.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
    ]);
    Workbook::from_bytes(bytes).unwrap()
}

fn addresses(book: &Workbook) -> Vec<String> {
    book.prepare_sheet("Data")
        .unwrap()
        .cell_refs()
        .map(|cell| cell.to_string())
        .collect()
}

fn used_range(book: &Workbook) -> Option<String> {
    book.prepare_sheet("Data")
        .unwrap()
        .used_range()
        .map(|range| range.to_string())
}

#[test]
fn fixture_cells_are_enumerated_with_their_contents() {
    let book = Workbook::from_bytes(include_bytes!("fixtures/libreoffice.xlsx").to_vec()).unwrap();
    let view = book.prepare_sheet("Data").unwrap();
    let cells: Vec<String> = view.cell_refs().map(|cell| cell.to_string()).collect();
    assert_eq!(cells, ["A1", "B1", "C1", "D1", "E1"]);
    assert_eq!(view.used_range(), Some("A1:E1".parse().unwrap()));
    let contents = view.get_cells_at(view.cell_refs()).unwrap();
    assert_eq!(contents[1].value, CellValue::Number(12.5));
    assert_eq!(contents[3].formula.as_deref(), Some("B1*2"));
    assert_eq!(contents[3].value, CellValue::Number(25.0));

    let notes = book.prepare_sheet("Notes").unwrap();
    assert_eq!(
        notes.used_range(),
        Some(CellRange::from(CellRef::new(1, 1).unwrap()))
    );
}

#[test]
fn sheets_without_cells_have_no_used_range() {
    for body in [
        "<sheetData/>",
        "<sheetData><row r='5' ht='20' customHeight='1'/></sheetData>",
        "<dimension ref='A1:C3'/><sheetData><row r='1'/></sheetData>",
    ] {
        let book = book(&format!("<worksheet xmlns='{MAIN_NS}'>{body}</worksheet>"));
        assert!(addresses(&book).is_empty(), "{body}");
        assert_eq!(used_range(&book), None, "{body}");
    }
}

#[test]
fn sparse_and_style_only_cells_define_the_used_range_rather_than_the_dimension() {
    let book = book(&format!(
        "<s:worksheet xmlns:s='{MAIN_NS}'><s:dimension ref='A1'/><s:sheetData>\
         <s:row r='3'><s:c r='D3' s='2'/></s:row>\
         <s:row r='7'><s:c r='B7'><s:v>1</s:v></s:c><s:c r='F7' s='1'/></s:row>\
         <s:row r='8'/>\
         <s:row r='9'><s:c r='C9' t='inlineStr'><s:is><s:t>x</s:t></s:is></s:c></s:row>\
         </s:sheetData></s:worksheet>"
    ));
    assert_eq!(addresses(&book), ["D3", "B7", "F7", "C9"]);
    assert_eq!(used_range(&book).as_deref(), Some("B3:F9"));
    let view = book.prepare_sheet("Data").unwrap();
    let values: Vec<CellValue> = view
        .get_cells_at(view.cell_refs())
        .unwrap()
        .into_iter()
        .map(|content| content.value)
        .collect();
    assert_eq!(
        values,
        [
            CellValue::Blank,
            CellValue::Number(1.0),
            CellValue::Blank,
            CellValue::from("x")
        ]
    );
}

#[test]
fn pending_edits_are_enumerated() {
    let mut book = book(&format!(
        "<worksheet xmlns='{MAIN_NS}'><sheetData><row r='2'><c r='B2'><v>1</v></c></row></sheetData></worksheet>"
    ));
    assert_eq!(used_range(&book).as_deref(), Some("B2"));
    // A blank value for a missing cell adds nothing; clearing a cell keeps it.
    book.set_cells(
        "Data",
        [
            ("Z100", CellValue::from("far")),
            ("A1", CellValue::Number(3.0)),
            ("C3", CellValue::Blank),
            ("B2", CellValue::Blank),
        ],
    )
    .unwrap();
    assert_eq!(addresses(&book), ["A1", "B2", "Z100"]);
    assert_eq!(used_range(&book).as_deref(), Some("A1:Z100"));
    book.reset_changes();
    assert_eq!(addresses(&book), ["B2"]);
}
