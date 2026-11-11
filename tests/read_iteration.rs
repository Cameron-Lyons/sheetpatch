//! Read order and sparse boundaries through prepared index traversal.
mod common;

use sheetpatch::{CellRange, CellRef, CellValue, Workbook};
use std::fmt::Write as _;

fn book(rows: &str) -> Workbook {
    let mut parts = common::payloads(include_bytes!("fixtures/libreoffice.xlsx"));
    parts.insert(
        "xl/worksheets/sheet1.xml".into(),
        format!(
            "<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><sheetData>{rows}</sheetData></worksheet>"
        )
        .into_bytes(),
    );
    let entries: Vec<_> = parts
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    Workbook::from_bytes(common::stored_archive(&entries)).unwrap()
}

#[test]
fn ordered_batches_retain_duplicates_and_blanks_across_sparse_rows() {
    let book = book(
        "<row r='1'><c r='B1'><v>2</v></c><c r='D1'><v>4</v></c></row>\
         <row r='3'/><row r='5'><c r='A5' s='7'/><c r='Z5'><v>26</v></c></row>\
         <row r='1048576'><c r='XFD1048576'><v>99</v></c></row>",
    );
    let view = book.prepare_sheet("Data").unwrap();
    let mut cells: Vec<CellRef> = [
        "A1",
        "A1",
        "B1",
        "B1",
        "C1",
        "D1",
        "XFD1",
        "A2",
        "A3",
        "A4",
        "A5",
        "B5",
        "Z5",
        "Z5",
        "XFD5",
        "A1048576",
        "XFD1048576",
        "XFD1048576",
    ]
    .map(|address| address.parse().unwrap())
    .to_vec();
    // Each order includes repeated existing and missing cells. The descending
    // and mixed cases must keep the ordinary lookup behavior.
    for order in 0..3 {
        if order == 1 {
            cells.reverse();
        } else if order == 2 {
            cells.rotate_left(5);
        }
        let expected: Vec<_> = cells
            .iter()
            .map(|&cell| view.get_cell_at(cell).unwrap())
            .collect();
        assert_eq!(view.get_cells_at(cells.iter().copied()).unwrap(), expected);
        assert_eq!(
            book.get_cells_at("Data", cells.iter().copied()).unwrap(),
            expected
        );
    }
}

#[test]
fn batches_report_the_first_requested_bad_value_in_every_order() {
    let book = book(
        "<row r='1'><c r='B1'><v>bad number</v></c><c r='D1' s='bad style'/></row>\
         <row r='3'><c r='B3' t='s'><v>999999</v></c></row>",
    );
    let view = book.prepare_sheet("Data").unwrap();
    for addresses in [
        ["A1", "A1", "B1", "D1", "B3"],
        ["C1", "C1", "D1", "B3", "XFD1048576"],
        ["B3", "B1", "D1", "A1", "A1"],
    ] {
        let cells = addresses.map(|address| address.parse::<CellRef>().unwrap());
        let expected = cells
            .iter()
            .find_map(|&cell| view.get_cell_at(cell).err())
            .unwrap();
        assert_eq!(
            view.get_cells_at(cells).unwrap_err().to_string(),
            expected.to_string()
        );
        assert_eq!(
            book.get_cells_at("Data", cells).unwrap_err().to_string(),
            expected.to_string()
        );
    }
}

#[test]
fn narrow_ranges_skip_bad_columns_and_resume_across_empty_rows() {
    let mut rows = String::new();
    for row in [1, 3, 5, 8] {
        write!(rows, "<row r='{row}'>").unwrap();
        if row != 3 {
            for column in 1..=64 {
                let cell = CellRef::try_new(row, column).unwrap();
                if column == 1 || column == 64 {
                    write!(rows, "<c r='{cell}'><v>bad number</v></c>").unwrap();
                } else {
                    write!(rows, "<c r='{cell}'><v>{column}</v></c>").unwrap();
                }
            }
        }
        rows.push_str("</row>");
    }
    let book = book(&rows);
    let view = book.prepare_sheet("Data").unwrap();
    for range in ["AF2:AG8", "AG8:AF2", "AF3:AG4", "AF9:AG10"] {
        let range: CellRange = range.parse().unwrap();
        let expected: Vec<_> = [1, 5, 8]
            .into_iter()
            .flat_map(|row| (32..=33).map(move |column| CellRef::try_new(row, column).unwrap()))
            .filter(|&cell| range.contains(cell))
            .map(|cell| (cell, CellValue::from(cell.column())))
            .collect();
        for split in 0..=expected.len() {
            let mut cells = view.cell_contents_in(Some(range));
            for (cell, value) in &expected[..split] {
                let (actual, content) = cells.next().unwrap().unwrap();
                assert_eq!((actual, content.value), (*cell, value.clone()));
            }
            for tail in [cells.clone(), cells] {
                let folded = tail.clone().fold(Vec::new(), |mut values, result| {
                    let (cell, content) = result.unwrap();
                    values.push((cell, content.value));
                    values
                });
                assert_eq!(folded, expected[split..]);
                let actual: Vec<_> = tail
                    .map(|result| {
                        let (cell, content) = result.unwrap();
                        (cell, content.value)
                    })
                    .collect();
                assert_eq!(actual, expected[split..]);
            }
        }
    }
}

#[test]
fn ranges_over_empty_rows_visit_only_matching_stored_cells() {
    for populated in [false, true] {
        let mut rows = String::new();
        for row in 1..=2_000 {
            write!(rows, "<row r='{row}' s='1' customFormat='1'>").unwrap();
            if populated && row % 1_000 == 0 {
                write!(
                    rows,
                    "<c r='A{row}'><v>bad number</v></c><c r='B{row}'><v>{row}</v></c>"
                )
                .unwrap();
            }
            rows.push_str("</row>");
        }
        let book = book(&rows);
        let view = book.prepare_sheet("Data").unwrap();
        let values: Vec<_> = view
            .cell_contents_in(Some("B1:B2000".parse().unwrap()))
            .map(|cell| cell.unwrap().1.value)
            .collect();
        assert_eq!(
            values,
            if populated {
                vec![CellValue::from(1_000), CellValue::from(2_000)]
            } else {
                Vec::new()
            }
        );
        assert!(
            view.cell_contents_in(Some("B1001:B1999".parse().unwrap()))
                .next()
                .is_none()
        );
    }
}
