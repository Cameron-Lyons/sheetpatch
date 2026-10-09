//! Typed access uses the same preservation and transaction rules as A1 access.
use sheetpatch::{CellRef, CellValue, Error, Workbook};

const FIXTURE: &[u8] = include_bytes!("fixtures/libreoffice.xlsx");

#[test]
fn typed_access_keeps_input_order_styles_cached_formulas_and_the_original_baseline() {
    let mut book = Workbook::from_bytes(FIXTURE.to_vec()).unwrap();
    let a1 = CellRef::new(1, 1).unwrap();
    let b1 = CellRef::new(1, 2).unwrap();
    let d1 = CellRef::new(1, 4).unwrap();
    let f3 = CellRef::new(3, 6).unwrap();
    let boundary = CellRef::new(1_048_576, 16_384).unwrap();
    let formula = book.get_cell_at("Data", d1).unwrap();
    assert_eq!(formula.formula.as_deref(), Some("B1*2"));
    assert_eq!(formula.value, CellValue::Number(25.0));

    book.set_cell_at("Data", a1, "typed text").unwrap();
    book.set_cells_at(
        "Data",
        [
            (f3, CellValue::from("first")),
            (b1, CellValue::Number(21.0)),
            (f3, CellValue::from("last wins")),
        ],
    )
    .unwrap();
    let addresses = [f3, a1, b1, boundary, d1, a1];
    let values = book.get_cells_at("Data", addresses).unwrap();
    assert_eq!(values[0].value, CellValue::from("last wins"));
    assert_eq!(values[1].value, CellValue::from("typed text"));
    assert_eq!(values[1].style_index, Some(1));
    assert_eq!(values[2].value, CellValue::Number(21.0));
    assert_eq!(values[3].value, CellValue::Blank);
    assert_eq!(values[4], formula);
    assert_eq!(values[5], values[1]);

    let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    assert_eq!(reopened.get_cells_at("Data", addresses).unwrap(), values);
    assert_eq!(
        reopened.get_cell("Notes", "A1").unwrap().value,
        CellValue::from("Keep this sheet")
    );
    book.reset_changes();
    assert_eq!(book.to_bytes().unwrap(), FIXTURE);
    assert_eq!(
        book.get_cell_at("Data", a1).unwrap().value,
        CellValue::from("Original text")
    );
}

#[test]
fn typed_batches_validate_overwritten_values_and_roll_back_on_formula_protection() {
    let mut book = Workbook::from_bytes(FIXTURE.to_vec()).unwrap();
    let a1 = CellRef::new(1, 1).unwrap();
    let b2 = CellRef::new(2, 2).unwrap();
    let d1 = CellRef::new(1, 4).unwrap();
    book.set_cell("Notes", "A1", "previous pending edit")
        .unwrap();
    let pending = book.to_bytes().unwrap();
    assert!(matches!(
        book.set_cells_at("Data", [(b2, 10), (d1, 11)]),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(book.to_bytes().unwrap(), pending);
    assert_eq!(
        book.get_cell_at("Data", b2).unwrap().value,
        CellValue::Blank
    );
    assert!(matches!(
        book.set_cells_at(
            "Data",
            [
                (a1, CellValue::Number(f64::NAN)),
                (a1, CellValue::from("valid"))
            ],
        ),
        Err(Error::InvalidValue(_))
    ));
    assert_eq!(book.to_bytes().unwrap(), pending);
    assert!(matches!(
        book.set_cell_at("Data", a1, "invalid\0text"),
        Err(Error::InvalidValue(_))
    ));
    assert_eq!(book.to_bytes().unwrap(), pending);
}

#[test]
fn empty_typed_batches_validate_sheet_names_without_staging_changes() {
    let mut book = Workbook::from_bytes(FIXTURE.to_vec()).unwrap();
    book.set_cells_at("Data", std::iter::empty::<(CellRef, i32)>())
        .unwrap();
    assert!(book.get_cells_at("Data", []).unwrap().is_empty());
    assert!(matches!(
        book.set_cells_at("data", std::iter::empty::<(CellRef, i32)>()),
        Err(Error::SheetNotFound(_))
    ));
    assert!(matches!(
        book.get_cells_at("data", []),
        Err(Error::SheetNotFound(_))
    ));
    assert!(!book.has_changes());
    assert_eq!(book.to_bytes().unwrap(), FIXTURE);
}
