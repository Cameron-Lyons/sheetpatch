//! Enumerating a worksheet's cells and its used range.
mod common;

use sheetpatch::{CellRange, CellRef, CellValue, Error, Workbook};

const MAIN_NAMESPACE: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const OFFICE_NAMESPACE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE_NAMESPACE: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

fn book(worksheet: &str) -> Workbook {
    let workbook = format!(
        "<workbook xmlns='{MAIN_NAMESPACE}' xmlns:r='{OFFICE_NAMESPACE}'><sheets><sheet name='Data' sheetId='1' r:id='one'/></sheets></workbook>"
    );
    let root = format!(
        "<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='office' Type='{OFFICE_NAMESPACE}/officeDocument' Target='xl/workbook.xml'/></Relationships>"
    );
    let relationships = format!(
        "<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='one' Type='{OFFICE_NAMESPACE}/worksheet' Target='worksheets/sheet1.xml'/></Relationships>"
    );
    let bytes = common::stored_archive(&[
        (
            "[Content_Types].xml",
            b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>",
        ),
        ("_rels/.rels", root.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", relationships.as_bytes()),
        ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
    ]);
    Workbook::from_bytes(bytes).unwrap()
}

fn addresses(book: &Workbook) -> Vec<String> {
    book.prepare_sheet("Data")
        .unwrap()
        .cells()
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
    let cells: Vec<String> = view.cells().map(|cell| cell.to_string()).collect();
    assert_eq!(cells, ["A1", "B1", "C1", "D1", "E1"]);
    assert_eq!(view.used_range(), Some("A1:E1".parse().unwrap()));
    let contents = view.get_cells_at(view.cells()).unwrap();
    assert_eq!(contents[1].value, CellValue::Number(12.5));
    assert_eq!(contents[3].formula.as_deref(), Some("B1*2"));
    assert_eq!(contents[3].value, CellValue::Number(25.0));

    let notes = book.prepare_sheet("Notes").unwrap();
    assert_eq!(
        notes.used_range(),
        Some(CellRange::from(CellRef::try_new(1, 1).unwrap()))
    );
}

#[test]
fn sheets_without_cells_have_no_used_range() {
    for body in [
        "<sheetData/>",
        "<sheetData><row r='5' ht='20' customHeight='1'/></sheetData>",
        "<dimension ref='A1:C3'/><sheetData><row r='1'/></sheetData>",
    ] {
        let book = book(&format!(
            "<worksheet xmlns='{MAIN_NAMESPACE}'>{body}</worksheet>"
        ));
        assert!(addresses(&book).is_empty(), "{body}");
        assert_eq!(used_range(&book), None, "{body}");
    }
}

#[test]
fn sparse_and_style_only_cells_define_the_used_range_rather_than_the_dimension() {
    let book = book(&format!(
        "<s:worksheet xmlns:s='{MAIN_NAMESPACE}'><s:dimension ref='A1'/><s:sheetData>\
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
        .get_cells_at(view.cells())
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
        "<worksheet xmlns='{MAIN_NAMESPACE}'><sheetData><row r='2'><c r='B2'><v>1</v></c></row></sheetData></worksheet>"
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

#[test]
fn reads_of_a_whole_grid_range_are_refused_before_allocating() {
    // One style-only cell in the last row and column stretches the used range
    // of a two-cell worksheet over all 17,179,869,184 cells of the grid.
    let book = book(&format!(
        "<worksheet xmlns='{MAIN_NAMESPACE}'><sheetData>\
         <row r='1'><c r='A1'><v>1</v></c></row>\
         <row r='1048576'><c r='XFD1048576' s='1'/></row>\
         </sheetData></worksheet>"
    ));
    let view = book.prepare_sheet("Data").unwrap();
    let range = view.used_range().unwrap();
    assert_eq!(range.to_string(), "A1:XFD1048576");
    let refused = |result: sheetpatch::Result<Vec<_>>| {
        assert!(
            matches!(&result, Err(Error::Unsupported(message)) if message.contains("too many cells")),
            "{:?}",
            result.map(|contents| contents.len())
        );
    };
    refused(view.get_cells_at(range));
    refused(book.get_cells_at("Data", range.cells()));
    // Seventeen whole columns are just over the limit of 2^24 cells.
    let columns: CellRange = "A1:Q1048576".parse().unwrap();
    refused(view.get_cells_at(columns));
    refused(book.get_cells_at("Data", columns));
    // An iterator without a size hint is cut off at the limit instead.
    refused(view.get_cells_at(columns.cells().filter(|_| true)));
    // The cells that exist can always be read.
    let contents = view.get_cells_at(view.cells()).unwrap();
    assert_eq!(contents.len(), 2);
    assert_eq!(contents[0].value, CellValue::Number(1.0));
    assert_eq!(contents[1].style_index, Some(1));
}

#[test]
fn indexed_contents_are_replayable_and_include_pending_and_style_only_cells() {
    let mut book =
        Workbook::from_bytes(include_bytes!("fixtures/libreoffice.xlsx").to_vec()).unwrap();
    book.set_cell("Data", "E1", CellValue::Blank).unwrap();
    book.set_cell("Data", "B3", "pending & \r\0").unwrap();
    let view = book.prepare_sheet("Data").unwrap();
    let expected: Vec<_> = view
        .cells()
        .map(|cell| (cell, view.get_cell_at(cell).unwrap()))
        .collect();
    assert_eq!(expected.last().unwrap().0.to_string(), "B3");
    assert!(
        expected
            .iter()
            .any(|(_, content)| content.formula.is_some())
    );
    assert!(expected.iter().any(|(_, content)| content.value.is_blank()));
    for range in [None, Some("A1:XFD1048576".parse().unwrap())] {
        for _ in 0..2 {
            let contents = view
                .cell_contents_in(range)
                .collect::<sheetpatch::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(contents, expected);
        }
    }
    let mut cursor = view.cell_contents();
    assert_eq!(cursor.next().unwrap().unwrap(), expected[0]);
    let checkpoint = cursor.clone();
    assert_eq!(
        cursor.collect::<sheetpatch::Result<Vec<_>>>().unwrap(),
        expected[1..]
    );
    assert_eq!(
        checkpoint.collect::<sheetpatch::Result<Vec<_>>>().unwrap(),
        expected[1..]
    );
    let selected = view
        .cell_contents_in(Some("B1:C3".parse().unwrap()))
        .collect::<sheetpatch::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        selected
            .iter()
            .map(|(cell, _)| cell.to_string())
            .collect::<Vec<_>>(),
        ["B1", "C1", "B3"]
    );
}

#[test]
fn indexed_contents_filter_before_decoding_and_keep_error_context() {
    let book = book(&format!(
        "<worksheet xmlns='{MAIN_NAMESPACE}'><sheetData>\
         <row r='1'><c r='A1'><v>7</v></c>\
         <c r='B1'><v>broken</v></c><c r='C1' t='s'><v>0</v></c>\
         <c r='D1' t='b'><v>true</v></c></row>\
         <row r='3'><c r='A3' s='2'/></row></sheetData></worksheet>"
    ));
    let view = book.prepare_sheet("Data").unwrap();
    let selected = view
        .cell_contents_in(Some("A1:A1048576".parse().unwrap()))
        .collect::<sheetpatch::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].1.value, CellValue::Number(7.0));
    assert_eq!(selected[1].1.style_index, Some(2));
    // An error is attributed exactly as an ordinary read, and does not
    // consume later entries. Starting a new iterator repeats those results.
    for _ in 0..2 {
        for (address, result) in view.cells().zip(view.cell_contents()) {
            let direct = view.get_cell_at(address);
            match (direct, result) {
                (Ok(expected), Ok((cell, content))) => {
                    assert_eq!(cell, address);
                    assert_eq!(content, expected);
                }
                (Err(expected), Err(error)) => {
                    assert_eq!(error.to_string(), expected.to_string());
                    assert!(error.to_string().contains("worksheet \"Data\""));
                    assert!(error.to_string().contains(&address.to_string()));
                }
                pair => panic!("mismatched reads for {address}: {pair:?}"),
            }
        }
    }
    assert_eq!(
        view.cell_contents_in(Some("Z1:Z9".parse().unwrap()))
            .count(),
        0
    );
}
