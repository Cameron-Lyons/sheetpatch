//! A workbook exported by LibreOffice, independent of our ZIP/XML test builders.
use sheetpatch::{CellValue, Workbook};

const LIBREOFFICE: &[u8] = include_bytes!("fixtures/libreoffice.xlsx");

#[test]
fn reads_and_edits_a_libreoffice_workbook_with_styles_and_formulas() {
    let mut workbook = Workbook::from_bytes(LIBREOFFICE.to_vec()).unwrap();
    assert_eq!(
        workbook
            .sheets()
            .iter()
            .map(|sheet| sheet.name())
            .collect::<Vec<_>>(),
        ["Data", "Notes"]
    );
    let original = workbook.get_cell("Data", "A1").unwrap();
    assert_eq!(original.value, CellValue::from("Original text"));
    assert_eq!(original.style_index, Some(1));
    assert_eq!(
        workbook.get_cell("Data", "E1").unwrap().value,
        CellValue::from("Café ☕")
    );
    let formula = workbook.get_cell("Data", "D1").unwrap();
    assert_eq!(formula.formula.as_deref(), Some("B1*2"));
    assert_eq!(formula.value, CellValue::Number(25.0));
    assert_eq!(workbook.to_bytes().unwrap(), LIBREOFFICE);

    workbook
        .set_cells(
            "Data",
            [
                ("A1", CellValue::from("=literal text")),
                ("B1", CellValue::Number(21.0)),
                ("C1", CellValue::Bool(false)),
                ("E1", CellValue::from("Café ☕ _x0041_\nnext line")),
                ("F3", CellValue::from("Inserted")),
            ],
        )
        .unwrap();
    for bytes in [
        workbook.to_bytes().unwrap(),
        workbook.to_bytes_compact().unwrap(),
    ] {
        let reopened = Workbook::from_bytes(bytes).unwrap();
        assert_eq!(
            reopened.get_cell("Data", "A1").unwrap().value,
            CellValue::from("=literal text")
        );
        assert_eq!(
            reopened.get_cell("Data", "A1").unwrap().style_index,
            Some(1)
        );
        assert_eq!(
            reopened.get_cell("Data", "B1").unwrap().value,
            CellValue::Number(21.0)
        );
        assert_eq!(
            reopened.get_cell("Data", "C1").unwrap().value,
            CellValue::Bool(false)
        );
        assert_eq!(
            reopened.get_cell("Data", "E1").unwrap().value,
            CellValue::from("Café ☕ _x0041_\nnext line")
        );
        assert_eq!(
            reopened.get_cell("Data", "F3").unwrap().value,
            CellValue::from("Inserted")
        );
        assert_eq!(reopened.get_cell("Data", "D1").unwrap(), formula);
        assert_eq!(
            reopened.get_cell("Notes", "A1").unwrap().value,
            CellValue::from("Keep this sheet")
        );
    }
}
