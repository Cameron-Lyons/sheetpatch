//! Value conversions, accessors and formatting, grid limits, and cell ranges.
use sheetpatch::{
    CellContent, CellEdit, CellRange, CellRangeIter, CellRef, CellValue, Error, Workbook,
};
use std::{borrow::Cow, collections::HashSet};

const FIXTURE: &[u8] = include_bytes!("fixtures/libreoffice.xlsx");
const EXACT: u64 = 1 << 53;

fn cell(address: &str) -> CellRef {
    address.parse().unwrap()
}

fn range(text: &str) -> CellRange {
    text.parse().unwrap()
}

fn addresses(cells: impl IntoIterator<Item = CellRef>) -> Vec<String> {
    cells.into_iter().map(|cell| cell.to_string()).collect()
}

fn values(contents: Vec<CellContent>) -> Vec<CellValue> {
    contents.into_iter().map(|content| content.value).collect()
}

#[test]
fn literals_and_new_conversions_work_with_every_editing_api() {
    let mut book = Workbook::from_bytes(FIXTURE.to_vec()).unwrap();
    let name = String::from("Ada");
    let rows = ["first", "second", "third"];

    // Unsuffixed literals still infer i32 and f64 alongside the added impls.
    book.set_cell("Data", "A1", 42).unwrap();
    book.set_cell("Data", "B1", 1.5).unwrap();
    book.set_cells("Data", [("A3", 1), ("B3", 2)]).unwrap();
    book.set_cells("Data", [("A4", 1.25), ("B4", -2.5)])
        .unwrap();
    book.set_cell_at("Data", cell("C3"), 7).unwrap();
    book.set_cells_at("Data", [(cell("C4"), 8)]).unwrap();
    book.apply_edits([
        CellEdit::new("Data", "A5", 7).unwrap(),
        CellEdit::at("Data", cell("B5"), 0.25).unwrap(),
        CellEdit::new("Notes", "A2", Some(&name)).unwrap(),
    ])
    .unwrap();

    book.set_cell("Data", "D3", &name).unwrap();
    book.set_cell("Data", "D4", 7u32).unwrap();
    book.set_cell("Data", "D5", Some(-3i16)).unwrap();
    book.set_cell("Data", "E3", None::<&str>).unwrap();
    book.set_cell("Data", "E4", 'é').unwrap();
    book.set_cell("Data", "E5", Cow::Borrowed("borrowed"))
        .unwrap();
    book.set_cell("Data", "F3", CellValue::try_from(rows.len()).unwrap())
        .unwrap();
    book.set_cells("Data", [("F4", Some(1.5)), ("F5", None)])
        .unwrap();

    let book = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    let read =
        |sheet: &str, cells: &str| values(book.get_cells_at(sheet, range(cells).cells()).unwrap());
    assert_eq!(
        read("Data", "A1:B1"),
        [CellValue::Number(42.0), CellValue::Number(1.5)]
    );
    assert_eq!(
        read("Data", "A3:F5"),
        [
            CellValue::Number(1.0),
            CellValue::Number(2.0),
            CellValue::Number(7.0),
            CellValue::from("Ada"),
            CellValue::Blank,
            CellValue::Number(3.0),
            CellValue::Number(1.25),
            CellValue::Number(-2.5),
            CellValue::Number(8.0),
            CellValue::Number(7.0),
            CellValue::from("é"),
            CellValue::Number(1.5),
            CellValue::Number(7.0),
            CellValue::Number(0.25),
            CellValue::Blank,
            CellValue::Number(-3.0),
            CellValue::from("borrowed"),
            CellValue::Blank,
        ]
    );
    assert_eq!(read("Notes", "A2"), [CellValue::from("Ada")]);
}

#[test]
fn text_and_small_integer_conversions_are_exact() {
    let owned = String::from("owned");
    assert_eq!(CellValue::from(&owned), CellValue::Text(owned.clone()));
    assert_eq!(
        CellValue::from(Cow::<str>::Owned(owned.clone())),
        CellValue::Text(owned)
    );
    assert_eq!(CellValue::from(Cow::Borrowed("b")), CellValue::from("b"));
    assert_eq!(CellValue::from('😀'), CellValue::from("😀"));

    assert_eq!(CellValue::from(i8::MIN), CellValue::Number(-128.0));
    assert_eq!(CellValue::from(i16::MIN), CellValue::Number(-32_768.0));
    assert_eq!(
        CellValue::from(i32::MIN),
        CellValue::Number(-2_147_483_648.0)
    );
    assert_eq!(CellValue::from(u8::MAX), CellValue::Number(255.0));
    assert_eq!(CellValue::from(u16::MAX), CellValue::Number(65_535.0));
    assert_eq!(
        CellValue::from(u32::MAX),
        CellValue::Number(4_294_967_295.0)
    );

    assert_eq!(CellValue::from(Some(1.5)), CellValue::Number(1.5));
    assert_eq!(CellValue::from(Some("x")), CellValue::from("x"));
    assert_eq!(CellValue::from(Some(Some(true))), CellValue::Bool(true));
    assert_eq!(CellValue::from(Some(None::<i32>)), CellValue::Blank);
    assert_eq!(CellValue::from(None::<String>), CellValue::Blank);
    assert_eq!(CellValue::from(Some(CellValue::Blank)), CellValue::Blank);
}

#[test]
fn wide_integers_convert_only_when_f64_holds_them_exactly() {
    let exact = i64::try_from(EXACT).unwrap();
    for v in [0, 1, -1, exact - 1, exact, -exact] {
        assert_eq!(CellValue::try_from(v).unwrap(), CellValue::Number(v as f64));
        if let Ok(size) = isize::try_from(v) {
            assert_eq!(
                CellValue::try_from(size).unwrap(),
                CellValue::Number(v as f64)
            );
        }
    }
    for v in [0, 1, EXACT - 1, EXACT] {
        assert_eq!(CellValue::try_from(v).unwrap(), CellValue::Number(v as f64));
        if let Ok(size) = usize::try_from(v) {
            assert_eq!(
                CellValue::try_from(size).unwrap(),
                CellValue::Number(v as f64)
            );
        }
    }
    assert_eq!(
        CellValue::try_from(9_007_199_254_740_991i64).unwrap(),
        CellValue::Number(9_007_199_254_740_991.0)
    );

    let inexact = |result: sheetpatch::Result<CellValue>| match result {
        Err(Error::InvalidValue(message)) => message,
        other => panic!("expected InvalidValue, got {other:?}"),
    };
    for v in [exact + 1, -exact - 1, i64::MIN, i64::MAX] {
        let message = inexact(CellValue::try_from(v));
        assert!(message.contains(&v.to_string()), "{message}");
        assert!(message.contains("text"), "{message}");
    }
    for v in [EXACT + 1, u64::MAX] {
        assert!(inexact(CellValue::try_from(v)).contains(&v.to_string()));
    }
    // 9_007_199_254_740_993 as f64 silently rounds to ...992; TryFrom refuses.
    assert_eq!(9_007_199_254_740_993i64 as f64, 9_007_199_254_740_992.0);
    inexact(CellValue::try_from(9_007_199_254_740_993i64));

    if let Ok(v) = isize::try_from(exact + 1) {
        inexact(CellValue::try_from(v));
        inexact(CellValue::try_from(-v));
        inexact(CellValue::try_from(isize::MIN));
    }
    if let Ok(v) = usize::try_from(EXACT + 1) {
        inexact(CellValue::try_from(v));
        inexact(CellValue::try_from(usize::MAX));
    }
}

#[test]
fn default_display_and_accessors_follow_each_variant() {
    assert_eq!(CellValue::default(), CellValue::Blank);

    let text = CellValue::from("plain & <raw>");
    let number = CellValue::from(12.5);
    let boolean = CellValue::from(false);
    let error = CellValue::Error("#DIV/0!".into());
    let blank = CellValue::Blank;

    assert_eq!(text.to_string(), "plain & <raw>");
    assert_eq!(number.to_string(), "12.5");
    assert_eq!(boolean.to_string(), "false");
    assert_eq!(error.to_string(), "#DIV/0!");
    assert_eq!(blank.to_string(), "");
    for (n, shown) in [
        (42.0, "42"),
        (-0.0, "-0"),
        (1e21, "1000000000000000000000"),
        (1e-7, "0.0000001"),
        (0.1 + 0.2, "0.30000000000000004"),
    ] {
        assert_eq!(CellValue::Number(n).to_string(), shown);
    }
    assert_eq!(format!("{:>5}|", CellValue::from("ab")), "   ab|");
    assert_eq!(format!("{:<3}|", CellValue::Blank), "   |");
    assert_eq!(format!("{:^6}|", CellValue::from(true)), " true |");
    assert_eq!(format!("{:08.3}", CellValue::from(-1.5)), "-001.500");
    assert_eq!(format!("{:.3}", CellValue::from("abcdef")), "abc");

    assert_eq!(text.as_text(), Some("plain & <raw>"));
    assert_eq!(number.as_number(), Some(12.5));
    assert_eq!(boolean.as_bool(), Some(false));
    assert_eq!(error.as_error(), Some("#DIV/0!"));
    assert!(blank.is_blank());
    for value in [&number, &boolean, &error, &blank] {
        assert_eq!(value.as_text(), None);
    }
    for value in [&text, &boolean, &error, &blank] {
        assert_eq!(value.as_number(), None);
    }
    for value in [&text, &number, &error, &blank] {
        assert_eq!(value.as_bool(), None);
    }
    for value in [&text, &number, &boolean, &blank] {
        assert_eq!(value.as_error(), None);
    }
    for value in [&text, &number, &boolean, &error] {
        assert!(!value.is_blank());
    }
}

#[test]
fn edits_compare_by_sheet_cell_and_value() {
    let edit = CellEdit::new("Data", "b2", 7).unwrap();
    assert_eq!(edit, CellEdit::at("Data", cell("B2"), 7.0).unwrap());
    assert_ne!(edit, CellEdit::new("Data", "B2", "7").unwrap());
    assert_ne!(edit, CellEdit::new("Data", "B3", 7).unwrap());
    assert_ne!(edit, CellEdit::new("Notes", "B2", 7).unwrap());
}

#[test]
fn read_contents_destructure_with_a_rest_pattern() {
    let book = Workbook::from_bytes(FIXTURE.to_vec()).unwrap();
    let CellContent { value, formula, .. } = book.get_cell("Data", "D1").unwrap();
    assert_eq!(value.as_number(), Some(25.0));
    assert_eq!(formula.as_deref(), Some("B1*2"));
}

#[test]
fn unsupported_errors_do_not_claim_an_edit_was_attempted() {
    let error = Error::Unsupported("binary Excel workbooks (.xlsb) are not supported".into());
    assert_eq!(
        error.to_string(),
        "unsupported: binary Excel workbooks (.xlsb) are not supported"
    );
}

#[test]
fn grid_limits_match_address_validation() {
    assert_eq!(CellRef::MAX_ROW, 1_048_576);
    assert_eq!(CellRef::MAX_COLUMN, 16_384);
    let corner = CellRef::new(CellRef::MAX_ROW, CellRef::MAX_COLUMN).unwrap();
    assert_eq!(corner.to_string(), "XFD1048576");
    assert_eq!(cell("xfd1048576"), corner);
    assert!(CellRef::new(CellRef::MAX_ROW + 1, 1).is_err());
    assert!(CellRef::new(1, CellRef::MAX_COLUMN + 1).is_err());
    assert!("XFE1".parse::<CellRef>().is_err());
    assert!("A1048577".parse::<CellRef>().is_err());
}

#[test]
fn ranges_normalize_corners_and_round_trip_through_text() {
    let expected = CellRange::new(cell("A1"), cell("C3"));
    for text in ["A1:C3", "C3:A1", "A3:C1", "C1:A3", "a1:c3", "c3:A1"] {
        let parsed = range(text);
        assert_eq!(parsed, expected, "{text}");
        assert_eq!(parsed.first(), cell("A1"));
        assert_eq!(parsed.last(), cell("C3"));
        assert_eq!(parsed.to_string(), "A1:C3");
    }
    assert_eq!(CellRange::new(cell("C1"), cell("A3")), expected);
    assert_eq!(CellRange::new(cell("C3"), cell("A1")), expected);

    for text in [
        "A1",
        "Z9",
        "AA10",
        "XFD1048576",
        "A1:XFD1048576",
        "B2:C2",
        "B2:B9",
    ] {
        assert_eq!(range(text).to_string(), text);
    }

    let single = range("b2");
    assert_eq!(single, range("B2:B2"));
    assert_eq!(single, CellRange::from(cell("B2")));
    assert_eq!(single, CellRange::new(cell("B2"), cell("B2")));
    assert_eq!(single.to_string(), "B2");
    assert_eq!(single.first(), single.last());
    assert_eq!(
        (single.height(), single.width(), single.cell_count()),
        (1, 1, 1)
    );
    assert_eq!(addresses(single.cells()), ["B2"]);

    let set: HashSet<CellRange> = ["A1:C3", "C3:A1", "B2", "B2:B2"].map(range).into();
    assert_eq!(set.len(), 2);
}

#[test]
fn ranges_reject_malformed_text_with_the_whole_input() {
    for text in [
        "",
        ":",
        "A1:",
        ":B2",
        "A1:B2:C3",
        "A1::B2",
        "A0:B2",
        "A1:B0",
        "A01:B2",
        "$A$1:B2",
        "A1:$B$2",
        "A:C",
        "1:3",
        " A1:B2",
        "A1:B2 ",
        "A1 :B2",
        "A1-B2",
        "Data!A1:B2",
        "XFE1:A1",
        "A1:A1048577",
        "é1:B2",
    ] {
        match text.parse::<CellRange>() {
            Err(Error::InvalidCellReference(input)) => assert_eq!(input, text),
            other => panic!("{text:?} parsed as {other:?}"),
        }
    }
}

#[test]
fn ranges_measure_and_contain_cells_up_to_the_grid_corner() {
    let block = range("B2:D5");
    assert_eq!(
        (block.height(), block.width(), block.cell_count()),
        (4, 3, 12)
    );
    for inside in ["B2", "D2", "B5", "D5", "C3"] {
        assert!(block.contains(cell(inside)), "{inside}");
    }
    for outside in ["A1", "A2", "B1", "E2", "B6", "E6", "XFD1048576"] {
        assert!(!block.contains(cell(outside)), "{outside}");
    }

    let sheet = range("A1:XFD1048576");
    assert_eq!(sheet.first(), cell("A1"));
    assert_eq!(
        sheet.last(),
        CellRef::new(CellRef::MAX_ROW, CellRef::MAX_COLUMN).unwrap()
    );
    assert_eq!(sheet.height(), CellRef::MAX_ROW);
    assert_eq!(sheet.width(), CellRef::MAX_COLUMN);
    assert_eq!(sheet.cell_count(), 17_179_869_184);
    assert!(sheet.contains(cell("A1")));
    assert!(sheet.contains(cell("XFD1048576")));
    let mut cells = sheet.cells();
    match usize::try_from(sheet.cell_count()) {
        Ok(count) => assert_eq!(cells.size_hint(), (count, Some(count))),
        Err(_) => assert_eq!(cells.size_hint(), (usize::MAX, None)),
    }
    assert_eq!(addresses(cells.by_ref().take(3)), ["A1", "B1", "C1"]);
    assert_eq!(cells.nth(16_381), Some(cell("A2")));

    let column = range("XFD1048574:XFD1048576");
    assert_eq!((column.height(), column.width()), (3, 1));
    assert_eq!(
        addresses(column.cells()),
        ["XFD1048574", "XFD1048575", "XFD1048576"]
    );
    let row = range("XFB1048576:XFD1048576");
    assert_eq!((row.height(), row.width()), (1, 3));
    assert_eq!(
        addresses(row.cells()),
        ["XFB1048576", "XFC1048576", "XFD1048576"]
    );
    let corner = range("XFC1048575:XFD1048576");
    assert_eq!(
        addresses(corner.cells()),
        ["XFC1048575", "XFD1048575", "XFC1048576", "XFD1048576"]
    );
}

#[test]
fn range_cells_iterate_in_row_major_order_with_an_exact_length() {
    let block = range("D4:B2");
    let mut cells: CellRangeIter = block.cells();
    let restart = cells.clone();
    let mut seen = Vec::new();
    for remaining in (1..=9).rev() {
        assert_eq!(cells.size_hint(), (remaining, Some(remaining)));
        seen.push(cells.next().unwrap());
    }
    assert_eq!(cells.size_hint(), (0, Some(0)));
    assert_eq!(cells.next(), None);
    assert_eq!(cells.next(), None);
    assert_eq!(
        addresses(seen),
        ["B2", "C2", "D2", "B3", "C3", "D3", "B4", "C4", "D4"]
    );
    assert_eq!(restart.count() as u64, block.cell_count());

    let mut sorted: Vec<CellRef> = block.cells().collect();
    sorted.sort();
    assert_eq!(sorted, block.cells().collect::<Vec<_>>());
    assert!(block.cells().all(|cell| block.contains(cell)));
}

#[test]
fn ranges_drive_typed_block_reads_and_writes() {
    let mut book = Workbook::from_bytes(FIXTURE.to_vec()).unwrap();
    assert_eq!(
        values(book.get_cells_at("Data", range("A1:C1").cells()).unwrap()),
        [
            CellValue::from("Original text"),
            CellValue::Number(12.5),
            CellValue::Number(1.0),
        ]
    );

    let block = range("H10:G9");
    book.set_cells_at("Data", block.cells().zip([1, 2, 3, 4]))
        .unwrap();
    let saved = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    let read = saved.get_cells_at("Data", block.cells()).unwrap();
    assert_eq!(values(read), [1.0, 2.0, 3.0, 4.0].map(CellValue::Number));
    assert_eq!(
        saved.get_cell("Data", "H9").unwrap().value,
        CellValue::Number(2.0)
    );
    assert_eq!(
        saved.get_cell("Data", "G10").unwrap().value,
        CellValue::Number(3.0)
    );

    let view = saved.prepare_sheet("Data").unwrap();
    assert_eq!(
        values(view.get_cells_at(range("G10:H10").cells()).unwrap()),
        [CellValue::Number(3.0), CellValue::Number(4.0)]
    );
}
