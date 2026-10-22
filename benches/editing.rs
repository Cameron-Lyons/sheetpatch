//! Reproducible benchmarks without a benchmarking dependency.
//! Run `SHEETPATCH_BENCH_SAMPLES=3 cargo bench --bench editing`.
//! Cold reads and edits include workbook loading; warm reads and saves reuse a book.
//! Prepared lookups exclude view construction; `prepare_and_read` includes it.
//! All timings exclude fixture construction.
//! Filter comma-separated case names with `SHEETPATCH_BENCH_CASES` and operation
//! names with `SHEETPATCH_BENCH_OPERATIONS`; unset filters run every workload.
//! Example: `SHEETPATCH_BENCH_CASES=wide SHEETPATCH_BENCH_OPERATIONS=wide_bulk_at`.
//! Run one filtered operation under `/usr/bin/time -v` to compare process peak RSS;
//! this includes fixture construction and correctness checks as well as samples.

// Shared with the integration tests: an independent ZIP writer and reader.
#[path = "../tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::hint::black_box;
use std::io;
use std::time::{Duration, Instant};

use common::{ZipEntry, ZipLayout, payloads, write_zip, zip_entries};
use sheetpatch::{CellEdit, CellRef, CellValue, Workbook};

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const SHEET_PATH: &str = "xl/worksheets/sheet1.xml";
// Default Deflate level, without optional ZIP metadata or comments.
const LAYOUT: ZipLayout<'static> = ZipLayout::deflated(6);
const CASES: &[(usize, usize)] = &[(10_000, 100), (50_000, 500)];
const CASE_NAMES: &[&str] = &[
    "narrow_10000",
    "narrow_50000",
    "guarded",
    "wide",
    "opaque",
    "shared_strings",
    "insertion",
    "multisheet",
    "tables",
    "control_text",
    "row_style",
];
const OPERATIONS: &[&str] = &[
    "sequential",
    "bulk",
    "get_cells",
    "bulk_at",
    "get_cells_at",
    "unchanged_bulk",
    "to_bytes",
    "write_to_sink",
    "compact_to_bytes",
    "compact_to_sink",
    "guarded_bulk_at",
    "cold_single_read",
    "warm_single_reads",
    "prepared_single_reads",
    "prepare_and_read",
    "wide_bulk_at",
    "opaque_bulk_at",
    "opaque_unchanged",
    "shared_cold_reads",
    "shared_warm_reads",
    "shared_prepared_reads",
    "insert_bulk_at",
    "multisheet_apply",
    "table_warm_bulk_at",
    "control_text_bulk_at",
    "row_style_bulk_at",
];

fn selected(variable: &str, name: &str) -> bool {
    std::env::var(variable).is_ok_and(|filter| filter.split(',').any(|item| item.trim() == name))
        || std::env::var_os(variable).is_none()
}

fn validate_filter(variable: &str, names: &[&str]) {
    if let Ok(filter) = std::env::var(variable) {
        for item in filter.split(',') {
            assert!(
                names.contains(&item.trim()),
                "unknown {variable} value {item:?}; choose from {names:?}"
            );
        }
    }
}

fn updates(cells: usize, count: usize) -> Vec<(String, CellValue)> {
    (0..count)
        .map(|index| {
            (
                format!("A{}", 1 + index * cells / count),
                CellValue::Number(-((index + 1) as f64)),
            )
        })
        .collect()
}

fn worksheet(cells: usize, values: &BTreeMap<usize, i32>) -> Vec<u8> {
    let mut xml = format!(
        "<worksheet xmlns=\"{MAIN_NS}\" xmlns:v=\"urn:vendor\"><dimension ref=\"A1:A{cells}\"/><sheetData>"
    );
    for row in 1..=cells {
        let value = values.get(&row).copied().unwrap_or(row as i32);
        write!(
            xml,
            "<row r=\"{row}\"><c r=\"A{row}\" s=\"0\" v:tag=\"keep\"><v>{value}</v></c></row>"
        )
        .unwrap();
    }
    xml.push_str("</sheetData><v:opaque flag='retained'>future XML</v:opaque></worksheet>");
    xml.into_bytes()
}

fn fixture(cells: usize) -> Vec<u8> {
    let sheet = worksheet(cells, &BTreeMap::new());
    fixture_with_sheet(&sheet)
}

fn fixture_with_sheet(sheet: &[u8]) -> Vec<u8> {
    let workbook = format!(
        "<workbook xmlns=\"{MAIN_NS}\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet name=\"Data\" sheetId=\"1\" r:id=\"r1\"/></sheets></workbook>"
    );
    // An incompressible opaque part makes serialization exercise retained data,
    // instead of benchmarking only an unusually small, compressible worksheet.
    let mut payload = Vec::with_capacity(1 << 20);
    let mut state = 0x1234_5678u32;
    for _ in 0..(1 << 20) {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        payload.push(state as u8);
    }
    zip(&[
        ("[Content_Types].xml", b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='rels' ContentType='application/vnd.openxmlformats-package.relationships+xml'/><Default Extension='xml' ContentType='application/xml'/><Default Extension='bin' ContentType='application/octet-stream'/><Override PartName='/xl/workbook.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml'/><Override PartName='/xl/worksheets/sheet1.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml'/><Override PartName='/xl/styles.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml'/></Types>"),
        ("_rels/.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='root' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='r1' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet' Target='worksheets/sheet1.xml'/><Relationship Id='s1' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles' Target='styles.xml'/></Relationships>"),
        (SHEET_PATH, sheet),
        ("xl/styles.xml", b"<styleSheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><fonts count='1'><font/></fonts><fills count='2'><fill><patternFill patternType='none'/></fill><fill><patternFill patternType='gray125'/></fill></fills><borders count='1'><border/></borders><cellStyleXfs count='1'><xf/></cellStyleXfs><cellXfs count='1'><xf xfId='0'/></cellXfs></styleSheet>"),
        ("customXml/item1.xml", b"<opaque xmlns='urn:vendor' flag='keep'>unfamiliar XML</opaque>"),
        ("vendor/opaque.bin", &payload),
    ])
}

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    write_zip(entries, &LAYOUT)
}

fn entries(bytes: &[u8]) -> BTreeMap<String, ZipEntry<'_>> {
    zip_entries(bytes)
        .into_iter()
        .map(|entry| (entry.name.clone(), entry))
        .collect()
}

fn verify_output(input: &[u8], output: &[u8], expected: &[u8]) {
    verify_parts(input, output, &[(SHEET_PATH, expected)]);
}

fn verify_parts(input: &[u8], output: &[u8], expected: &[(&str, &[u8])]) {
    let originals = entries(input);
    let replacements = entries(output);
    assert_eq!(originals.len(), replacements.len());
    for &(path, xml) in expected {
        assert_eq!(
            replacements[path].payload(),
            xml,
            "effective worksheet XML must match: {path}"
        );
    }
    for (name, original) in originals {
        if !expected.iter().any(|&(path, _)| path == name) {
            let replacement = &replacements[&name];
            assert_eq!(original.local, replacement.local, "{name}");
            assert_eq!(original.compressed, replacement.compressed, "{name}");
            assert_eq!(
                original.central_before_offset(),
                replacement.central_before_offset(),
                "{name}"
            );
            assert_eq!(
                original.central_after_offset(),
                replacement.central_after_offset(),
                "{name}"
            );
        }
    }
}

fn expected(cells: usize, count: usize) -> Vec<u8> {
    worksheet(
        cells,
        &(0..count)
            .map(|index| (1 + index * cells / count, -((index + 1) as i32)))
            .collect(),
    )
}

fn sequential(input: &[u8], edits: &[(String, CellValue)]) -> Workbook {
    let mut book = Workbook::from_bytes(input.to_vec()).unwrap();
    for (address, value) in edits {
        book.set_cell("Data", address, value.clone()).unwrap();
    }
    book
}

fn measure<T>(samples: usize, label: &str, mut operation: impl FnMut() -> T) -> Option<Duration> {
    if !selected("SHEETPATCH_BENCH_OPERATIONS", label) {
        return None;
    }
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        let result = black_box(operation());
        times.push(started.elapsed());
        drop(result);
    }
    times.sort_unstable();
    Some(times[times.len() / 2])
}

fn samples() -> usize {
    std::env::var("SHEETPATCH_BENCH_SAMPLES")
        .map(|value| {
            value
                .parse::<usize>()
                .ok()
                .filter(|&samples| samples > 0)
                .expect("SHEETPATCH_BENCH_SAMPLES must be a positive integer")
        })
        .unwrap_or(3)
}

fn report(label: &str, cells: usize, count: usize, samples: usize, elapsed: Option<Duration>) {
    if let Some(elapsed) = elapsed {
        println!("{label},{cells},{count},{samples},{}", elapsed.as_micros());
    }
}

// New APIs begin here; the helpers above (with tests/common/mod.rs) also run
// against the original baseline.
fn bulk(input: &[u8], edits: &[(String, CellValue)]) -> Workbook {
    let mut book = Workbook::from_bytes(input.to_vec()).unwrap();
    book.set_cells(
        "Data",
        edits
            .iter()
            .map(|(address, value)| (address.as_str(), value.clone())),
    )
    .unwrap();
    book
}

fn bulk_at(input: &[u8], edits: &[(CellRef, CellValue)]) -> Workbook {
    let mut book = Workbook::from_bytes(input.to_vec()).unwrap();
    book.set_cells_at(
        "Data",
        edits.iter().map(|(cell, value)| (*cell, value.clone())),
    )
    .unwrap();
    book
}

fn guarded_worksheet(cells: usize, values: &BTreeMap<usize, i32>) -> Vec<u8> {
    let mut merges = format!("<mergeCells count=\"{cells}\">");
    for row in 1..=cells {
        write!(merges, "<mergeCell ref=\"A{row}:D{row}\"/>").unwrap();
    }
    merges.push_str("</mergeCells>");
    String::from_utf8(worksheet(cells, values))
        .unwrap()
        .replace("r=\"A", "r=\"E")
        .replace("ref=\"A1:A", "ref=\"E1:E")
        .replace("</sheetData>", &format!("</sheetData>{merges}"))
        .into_bytes()
}

fn guarded_batches(samples: usize) {
    let cells = 20_000;
    let input = fixture_with_sheet(&guarded_worksheet(cells, &BTreeMap::new()));
    for count in [1, 500] {
        let rows: Vec<_> = if count == 1 {
            vec![10_000]
        } else {
            (0..count).map(|index| 1 + index * cells / count).collect()
        };
        let edits: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(index, &row)| {
                (
                    CellRef::new(row as u32, 5).unwrap(),
                    CellValue::Number(-((index + 1) as f64)),
                )
            })
            .collect();
        let values = rows
            .iter()
            .enumerate()
            .map(|(index, &row)| (row, -((index + 1) as i32)))
            .collect();
        let mut edited = bulk_at(&input, &edits);
        let output = edited.to_bytes().unwrap();
        verify_output(&input, &output, &guarded_worksheet(cells, &values));
        assert!(
            edited
                .set_cell_at("Data", CellRef::new(10_000, 3).unwrap(), 42)
                .is_err()
        );
        assert_eq!(edited.to_bytes().unwrap(), output);
        report(
            "guarded_bulk_at",
            cells,
            count,
            samples,
            measure(samples, "guarded_bulk_at", || bulk_at(&input, &edits)),
        );
    }
}

fn table_batches(samples: usize) {
    let columns = 5_000;
    let office = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    let mut table_parts = format!("<tableParts count='{columns}'>");
    let mut relationships = String::from(
        "<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'>",
    );
    let mut parts = payloads(&fixture_with_sheet(&wide_worksheet(
        2,
        columns,
        &BTreeMap::new(),
    )));
    for column in 1..=columns {
        let first = CellRef::new(1, column).unwrap();
        let last = CellRef::new(2, column).unwrap();
        write!(table_parts, "<tablePart r:id='t{column}'/>").unwrap();
        write!(
            relationships,
            "<Relationship Id='t{column}' Type='{office}/table' Target='../tables/table{column}.xml'/>"
        )
        .unwrap();
        parts.insert(
            format!("xl/tables/table{column}.xml"),
            format!(
                "<table xmlns='{MAIN_NS}' id='{column}' name='Table{column}' displayName='Table{column}' ref='{first}:{last}'><tableColumns count='1'><tableColumn id='1' name='{column}'/></tableColumns></table>"
            )
            .into_bytes(),
        );
    }
    table_parts.push_str("</tableParts>");
    relationships.push_str("</Relationships>");
    let worksheet = |values: &BTreeMap<CellRef, i32>| {
        String::from_utf8(wide_worksheet(2, columns, values))
            .unwrap()
            .replace("<worksheet ", &format!("<worksheet xmlns:r='{office}' "))
            .replace("</worksheet>", &format!("{table_parts}</worksheet>"))
            .into_bytes()
    };
    parts.insert(SHEET_PATH.into(), worksheet(&BTreeMap::new()));
    parts.insert(
        "xl/worksheets/_rels/sheet1.xml.rels".into(),
        relationships.into_bytes(),
    );
    let borrowed: Vec<_> = parts
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    let input = zip(&borrowed);
    let edits: Vec<_> = (1..=columns)
        .map(|column| {
            (
                CellRef::new(2, column).unwrap(),
                CellValue::from(-i32::from(column)),
            )
        })
        .collect();
    let values = edits
        .iter()
        .map(|(cell, _)| (*cell, -i32::from(cell.column())))
        .collect();
    let mut book = bulk_at(&input, &edits);
    verify_output(&input, &book.to_bytes().unwrap(), &worksheet(&values));
    assert!(book.set_cell("Data", "A1", "changed").is_err());
    // Reuse discovered table headers to isolate bulk-edit scaling from the
    // cost of opening thousands of table parts. Reset keeps those caches.
    report(
        "table_warm_bulk_at",
        2 * usize::from(columns),
        edits.len(),
        samples,
        measure(samples, "table_warm_bulk_at", || {
            book.reset_changes();
            book.set_cells_at(
                "Data",
                edits.iter().map(|(cell, value)| (*cell, value.clone())),
            )
            .unwrap();
        }),
    );
}

fn expanded_fixture(sheets: &[(&str, &[u8])], shared_strings: Option<&[u8]>) -> Vec<u8> {
    let base = fixture_with_sheet(sheets[0].1);
    let mut parts = payloads(&base);
    let office = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    let mut workbook = format!("<workbook xmlns='{MAIN_NS}' xmlns:r='{office}'><sheets>");
    let mut relationships = String::from(
        "<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'>",
    );
    let mut additional_types = String::new();
    for (index, &(name, xml)) in sheets.iter().enumerate() {
        let number = index + 1;
        write!(
            workbook,
            "<sheet name='{name}' sheetId='{number}' r:id='r{number}'/>"
        )
        .unwrap();
        write!(relationships, "<Relationship Id='r{number}' Type='{office}/worksheet' Target='worksheets/sheet{number}.xml'/>").unwrap();
        if index > 0 {
            write!(additional_types, "<Override PartName='/xl/worksheets/sheet{number}.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml'/>").unwrap();
        }
        parts.insert(format!("xl/worksheets/sheet{number}.xml"), xml.to_vec());
    }
    if let Some(strings) = shared_strings {
        write!(
            relationships,
            "<Relationship Id='strings' Type='{office}/sharedStrings' Target='sharedStrings.xml'/>"
        )
        .unwrap();
        additional_types.push_str("<Override PartName='/xl/sharedStrings.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml'/>");
        parts.insert("xl/sharedStrings.xml".into(), strings.to_vec());
    }
    write!(
        relationships,
        "<Relationship Id='styles' Type='{office}/styles' Target='styles.xml'/></Relationships>"
    )
    .unwrap();
    workbook.push_str("</sheets></workbook>");
    parts.insert("xl/workbook.xml".into(), workbook.into_bytes());
    parts.insert(
        "xl/_rels/workbook.xml.rels".into(),
        relationships.into_bytes(),
    );
    let types = String::from_utf8(parts.remove("[Content_Types].xml").unwrap()).unwrap();
    parts.insert(
        "[Content_Types].xml".into(),
        types
            .replace("</Types>", &format!("{additional_types}</Types>"))
            .into_bytes(),
    );
    let borrowed: Vec<_> = parts
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    zip(&borrowed)
}

fn wide_worksheet(rows: u32, columns: u16, values: &BTreeMap<CellRef, i32>) -> Vec<u8> {
    let last = CellRef::new(rows, columns).unwrap();
    let mut xml = format!("<worksheet xmlns='{MAIN_NS}'><dimension ref='A1:{last}'/><sheetData>");
    for row in 1..=rows {
        write!(xml, "<row r='{row}'>").unwrap();
        for column in 1..=columns {
            let cell = CellRef::new(row, column).unwrap();
            let value = values
                .get(&cell)
                .copied()
                .unwrap_or(((row - 1) * u32::from(columns) + u32::from(column)) as i32);
            write!(xml, "<c r='{cell}' s='0'><v>{value}</v></c>").unwrap();
        }
        xml.push_str("</row>");
    }
    xml.push_str("</sheetData></worksheet>");
    xml.into_bytes()
}

fn edit_case(
    samples: usize,
    label: &str,
    sheet: &[u8],
    expected: &[u8],
    cells: usize,
    edits: &[(CellRef, CellValue)],
) -> Workbook {
    let input = fixture_with_sheet(sheet);
    let book = bulk_at(&input, edits);
    verify_output(&input, &book.to_bytes().unwrap(), expected);
    let reads = book
        .get_cells_at("Data", edits.iter().map(|(cell, _)| *cell))
        .unwrap();
    for (read, (_, value)) in reads.iter().zip(edits) {
        assert_eq!(&read.value, value);
    }
    report(
        label,
        cells,
        edits.len(),
        samples,
        measure(samples, label, || bulk_at(&input, edits)),
    );
    book
}

fn repeated_reads(
    samples: usize,
    input: &[u8],
    book: &Workbook,
    edits: &[(CellRef, CellValue)],
    cells: usize,
) {
    let queries: Vec<_> = edits.iter().take(16).map(|(cell, _)| *cell).collect();
    let expected = book.get_cells_at("Data", queries.iter().copied()).unwrap();
    let read_one_at_a_time = || {
        queries
            .iter()
            .map(|&cell| book.get_cell_at("Data", cell).unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(read_one_at_a_time(), expected);
    report(
        "cold_single_read",
        cells,
        1,
        samples,
        measure(samples, "cold_single_read", || {
            Workbook::from_bytes(input.to_vec())
                .unwrap()
                .get_cell_at("Data", queries[0])
                .unwrap()
        }),
    );
    report(
        "warm_single_reads",
        cells,
        queries.len(),
        samples,
        measure(samples, "warm_single_reads", read_one_at_a_time),
    );
    prepared_reads(samples, book, &queries, cells);
}

// Keep the new API benchmarks together so the common workloads can also run
// against an older checkout with a small view adapter for baseline comparison.
fn prepared_reads(samples: usize, book: &Workbook, queries: &[CellRef], cells: usize) {
    if selected("SHEETPATCH_BENCH_OPERATIONS", "prepared_single_reads") {
        let view = book.prepare_sheet("Data").unwrap();
        let expected = book.get_cells_at("Data", queries.iter().copied()).unwrap();
        let read = || {
            queries
                .iter()
                .map(|&cell| view.get_cell_at(cell).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(read(), expected);
        report(
            "prepared_single_reads",
            cells,
            queries.len(),
            samples,
            measure(samples, "prepared_single_reads", read),
        );
    }
    report(
        "prepare_and_read",
        cells,
        1,
        samples,
        measure(samples, "prepare_and_read", || {
            book.prepare_sheet("Data")
                .unwrap()
                .get_cell_at(queries[0])
                .unwrap()
        }),
    );
}

fn shared_string_reads(samples: usize) {
    let cells = 2_000;
    let count = 64;
    let mut sheet = format!("<worksheet xmlns='{MAIN_NS}'><sheetData>");
    let mut strings = format!("<sst xmlns='{MAIN_NS}'>");
    for index in 0..128 {
        write!(
            strings,
            "<si><r><t>Shared {index} </t></r><r><t>☕ _x005F_x0041_</t></r></si>"
        )
        .unwrap();
    }
    strings.push_str("</sst>");
    for row in 1..=cells {
        write!(
            sheet,
            "<row r='{row}'><c r='A{row}' t='s'><v>{}</v></c></row>",
            (row - 1) % 128
        )
        .unwrap();
    }
    sheet.push_str("</sheetData></worksheet>");
    let input = expanded_fixture(&[("Data", sheet.as_bytes())], Some(strings.as_bytes()));
    let queries: Vec<_> = (0..count)
        .map(|index| CellRef::new(1 + (index * cells / count) as u32, 1).unwrap())
        .collect();
    let book = Workbook::from_bytes(input.clone()).unwrap();
    let expected = book.get_cells_at("Data", queries.iter().copied()).unwrap();
    for (content, cell) in expected.iter().zip(&queries) {
        assert_eq!(
            content.value,
            CellValue::from(format!("Shared {} ☕ _x0041_", (cell.row() - 1) % 128))
        );
    }
    assert_eq!(book.to_bytes().unwrap(), input);
    report(
        "shared_cold_reads",
        cells,
        count,
        samples,
        measure(samples, "shared_cold_reads", || {
            Workbook::from_bytes(input.clone())
                .unwrap()
                .get_cells_at("Data", queries.iter().copied())
                .unwrap()
        }),
    );
    report(
        "shared_warm_reads",
        cells,
        count,
        samples,
        measure(samples, "shared_warm_reads", || {
            book.get_cells_at("Data", queries.iter().copied()).unwrap()
        }),
    );
    if selected("SHEETPATCH_BENCH_OPERATIONS", "shared_prepared_reads") {
        let view = book.prepare_sheet("Data").unwrap();
        assert_eq!(
            view.get_cells_at(queries.iter().copied()).unwrap(),
            expected
        );
        report(
            "shared_prepared_reads",
            cells,
            count,
            samples,
            measure(samples, "shared_prepared_reads", || {
                view.get_cells_at(queries.iter().copied()).unwrap()
            }),
        );
    }
}

fn extra_workloads(samples: usize) {
    if selected("SHEETPATCH_BENCH_CASES", "wide") {
        let rows = 200;
        let columns = 100;
        let values: BTreeMap<_, _> = (0..100)
            .map(|index| {
                (
                    CellRef::new(
                        1 + index * rows / 100,
                        1 + (index * 37 % u32::from(columns)) as u16,
                    )
                    .unwrap(),
                    -((index + 1) as i32),
                )
            })
            .collect();
        let edits: Vec<_> = values
            .iter()
            .map(|(&cell, &value)| (cell, CellValue::from(value)))
            .collect();
        edit_case(
            samples,
            "wide_bulk_at",
            &wide_worksheet(rows, columns, &BTreeMap::new()),
            &wide_worksheet(rows, columns, &values),
            rows as usize * usize::from(columns),
            &edits,
        );
    }
    if selected("SHEETPATCH_BENCH_CASES", "opaque") {
        let cells = 1_000;
        let count = 20;
        let mut extension = String::from("<extLst><ext uri='urn:bench'><v:tree>");
        for index in 0..32 {
            write!(extension, "<v:branch n='{index}'><v:leaf a='opaque'>unfamiliar cell extension payload</v:leaf></v:branch>").unwrap();
        }
        extension.push_str("</v:tree></ext></extLst>");
        let with_extensions = |values| {
            String::from_utf8(worksheet(cells, values))
                .unwrap()
                .replace("</c>", &format!("{extension}</c>"))
                .into_bytes()
        };
        let edits: Vec<_> = updates(cells, count)
            .into_iter()
            .map(|(cell, value)| (cell.parse().unwrap(), value))
            .collect();
        let values = (0..count)
            .map(|index| (1 + index * cells / count, -((index + 1) as i32)))
            .collect();
        let mut book = edit_case(
            samples,
            "opaque_bulk_at",
            &with_extensions(&BTreeMap::new()),
            &with_extensions(&values),
            cells,
            &edits,
        );
        let before = book.to_bytes().unwrap();
        report(
            "opaque_unchanged",
            cells,
            count,
            samples,
            measure(samples, "opaque_unchanged", || {
                book.set_cells_at("Data", edits.iter().cloned()).unwrap()
            }),
        );
        assert_eq!(book.to_bytes().unwrap(), before);
    }
    if selected("SHEETPATCH_BENCH_CASES", "shared_strings") {
        shared_string_reads(samples);
    }
    if selected("SHEETPATCH_BENCH_CASES", "insertion") {
        let sheet = format!("<worksheet xmlns='{MAIN_NS}'><sheetData/></worksheet>");
        let mut rows = String::from("<sheetData>");
        let mut edits = Vec::new();
        for row in 1..=500 {
            write!(rows, "<row r=\"{row}\">").unwrap();
            for column in 1..=2 {
                let cell = CellRef::new(row, column).unwrap();
                let value = row as i32 * i32::from(column);
                write!(rows, "<c r=\"{cell}\"><v>{value}</v></c>").unwrap();
                edits.push((cell, CellValue::from(value)));
            }
            rows.push_str("</row>");
        }
        rows.push_str("</sheetData>");
        edit_case(
            samples,
            "insert_bulk_at",
            sheet.as_bytes(),
            sheet.replace("<sheetData/>", &rows).as_bytes(),
            0,
            &edits,
        );
    }
    if selected("SHEETPATCH_BENCH_CASES", "row_style") {
        // A valid style index with a long lexical spelling should be parsed
        // once when many inserted cells inherit the same row's custom format.
        // Style zero is present in the fixture and needs no cell attribute.
        let style = "0".repeat(64 * 1024);
        let prefix = format!(
            "<worksheet xmlns='{MAIN_NS}'><sheetData><row r='1' customFormat='1' s='{style}'>"
        );
        let suffix = "</row></sheetData></worksheet>";
        let sheet = format!("{prefix}{suffix}");
        let mut expected = prefix;
        let mut edits = Vec::new();
        for column in 1..=1_024 {
            let cell = CellRef::new(1, column).unwrap();
            write!(expected, "<c r=\"{cell}\"><v>{column}</v></c>").unwrap();
            edits.push((cell, CellValue::from(i32::from(column))));
        }
        expected.push_str(suffix);
        edit_case(
            samples,
            "row_style_bulk_at",
            sheet.as_bytes(),
            expected.as_bytes(),
            0,
            &edits,
        );
    }
    if selected("SHEETPATCH_BENCH_CASES", "multisheet") {
        let cells = 2_000;
        let count = 32;
        let sheet = worksheet(cells, &BTreeMap::new());
        let names = ["Data", "Other1", "Other2", "Other3"];
        let sheets: Vec<_> = names.iter().map(|&name| (name, sheet.as_slice())).collect();
        let input = expanded_fixture(&sheets, None);
        let edits: Vec<_> = names
            .iter()
            .flat_map(|&name| {
                updates(cells, count)
                    .into_iter()
                    .map(move |(cell, value)| CellEdit::new(name, &cell, value).unwrap())
            })
            .collect();
        let run = || {
            let mut book = Workbook::from_bytes(input.clone()).unwrap();
            book.apply_edits(edits.clone()).unwrap();
            book
        };
        let book = run();
        let expected = expected(cells, count);
        let paths: Vec<_> = (1..=names.len())
            .map(|number| format!("xl/worksheets/sheet{number}.xml"))
            .collect();
        let parts: Vec<_> = paths
            .iter()
            .map(|path| (path.as_str(), expected.as_slice()))
            .collect();
        verify_parts(&input, &book.to_bytes().unwrap(), &parts);
        for name in names {
            assert_eq!(
                book.get_cell(name, "A1").unwrap().value,
                CellValue::Number(-1.0)
            );
        }
        report(
            "multisheet_apply",
            cells * names.len(),
            edits.len(),
            samples,
            measure(samples, "multisheet_apply", run),
        );
    }
    if selected("SHEETPATCH_BENCH_CASES", "control_text") {
        // Each cell holds exactly Excel's 32,767 UTF-16 units. Its XML spells
        // every character as an escape, exercising output growth and formatting.
        let text = "\0\u{1}\u{1f}\u{fffe}\u{ffff}".repeat(6_553) + "\0\u{1}";
        let escaped = "_x0000__x0001__x001F__xFFFE__xFFFF_".repeat(6_553) + "_x0000__x0001_";
        let sheet = format!("<worksheet xmlns='{MAIN_NS}'><sheetData/></worksheet>");
        let mut rows = String::from("<sheetData>");
        let mut edits = Vec::new();
        for row in 1..=64 {
            let cell = CellRef::new(row, 1).unwrap();
            write!(
                rows,
                "<row r=\"{row}\"><c r=\"{cell}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">{escaped}</t></is></c></row>"
            )
            .unwrap();
            edits.push((cell, CellValue::Text(text.clone())));
        }
        rows.push_str("</sheetData>");
        edit_case(
            samples,
            "control_text_bulk_at",
            sheet.as_bytes(),
            sheet.replace("<sheetData/>", &rows).as_bytes(),
            0,
            &edits,
        );
    }
}

fn main() {
    if cfg!(debug_assertions) {
        println!("Run cargo bench --bench editing for release-profile measurements.");
        return;
    }
    validate_filter("SHEETPATCH_BENCH_CASES", CASE_NAMES);
    validate_filter("SHEETPATCH_BENCH_OPERATIONS", OPERATIONS);
    let samples = samples();
    println!("operation,cells,edits,samples,median_us");
    for &(cells, count) in CASES {
        if !selected("SHEETPATCH_BENCH_CASES", &format!("narrow_{cells}")) {
            continue;
        }
        let input = fixture(cells);
        let edits = updates(cells, count);
        let typed_edits: Vec<_> = edits
            .iter()
            .map(|(address, value)| (address.parse::<CellRef>().unwrap(), value.clone()))
            .collect();
        let expected = expected(cells, count);
        let batched = bulk(&input, &edits);
        let output = batched.to_bytes().unwrap();
        let typed = bulk_at(&input, &typed_edits);
        verify_output(&input, &typed.to_bytes().unwrap(), &expected);
        assert_eq!(typed.to_bytes().unwrap(), output);
        if selected("SHEETPATCH_BENCH_OPERATIONS", "sequential") {
            verify_output(
                &input,
                &sequential(&input, &edits).to_bytes().unwrap(),
                &expected,
            );
        }
        verify_output(&input, &output, &expected);
        let mut streamed = Vec::new();
        batched.write_to(&mut streamed).unwrap();
        assert_eq!(streamed, output);
        let compact = batched.to_bytes_compact().unwrap();
        verify_output(&input, &compact, &expected);
        let addresses: Vec<_> = edits.iter().map(|(address, _)| address.as_str()).collect();
        let values = batched.get_cells("Data", &addresses).unwrap();
        let typed_values = typed
            .get_cells_at("Data", typed_edits.iter().map(|(cell, _)| *cell))
            .unwrap();
        assert_eq!(typed_values, values);
        for (cell, (_, expected_value)) in values.iter().zip(&edits) {
            assert_eq!(&cell.value, expected_value);
        }
        repeated_reads(samples, &input, &batched, &typed_edits, cells);
        let mut unchanged = bulk(&input, &edits);
        unchanged
            .set_cells(
                "Data",
                edits
                    .iter()
                    .map(|(address, value)| (address.as_str(), value.clone())),
            )
            .unwrap();
        assert_eq!(unchanged.to_bytes().unwrap(), output);

        report(
            "sequential",
            cells,
            count,
            samples,
            measure(samples, "sequential", || self::sequential(&input, &edits)),
        );
        report(
            "bulk",
            cells,
            count,
            samples,
            measure(samples, "bulk", || bulk(&input, &edits)),
        );
        report(
            "get_cells",
            cells,
            count,
            samples,
            measure(samples, "get_cells", || {
                batched.get_cells("Data", &addresses).unwrap()
            }),
        );
        report(
            "bulk_at",
            cells,
            count,
            samples,
            measure(samples, "bulk_at", || bulk_at(&input, &typed_edits)),
        );
        report(
            "get_cells_at",
            cells,
            count,
            samples,
            measure(samples, "get_cells_at", || {
                typed
                    .get_cells_at("Data", typed_edits.iter().map(|(cell, _)| *cell))
                    .unwrap()
            }),
        );
        report(
            "unchanged_bulk",
            cells,
            count,
            samples,
            measure(samples, "unchanged_bulk", || {
                unchanged
                    .set_cells(
                        "Data",
                        edits
                            .iter()
                            .map(|(address, value)| (address.as_str(), value.clone())),
                    )
                    .unwrap()
            }),
        );
        assert_eq!(unchanged.to_bytes().unwrap(), output);
        report(
            "to_bytes",
            cells,
            count,
            samples,
            measure(samples, "to_bytes", || batched.to_bytes().unwrap()),
        );
        report(
            "write_to_sink",
            cells,
            count,
            samples,
            measure(samples, "write_to_sink", || {
                batched.write_to(io::sink()).unwrap()
            }),
        );
        report(
            "compact_to_bytes",
            cells,
            count,
            samples,
            measure(samples, "compact_to_bytes", || {
                batched.to_bytes_compact().unwrap()
            }),
        );
        report(
            "compact_to_sink",
            cells,
            count,
            samples,
            measure(samples, "compact_to_sink", || {
                batched.write_compact_to(io::sink()).unwrap()
            }),
        );
        println!(
            "sizes,{cells},{count},input={},edited={},compact={}",
            input.len(),
            output.len(),
            compact.len()
        );

        let mut repeated = input.clone();
        let mut values = BTreeMap::new();
        for value in 1..=5 {
            let mut book = Workbook::from_bytes(repeated).unwrap();
            book.set_cell("Data", "A1", -(value as f64)).unwrap();
            repeated = book.to_bytes().unwrap();
            values.insert(1, -value);
        }
        let compacted = Workbook::from_bytes(repeated.clone())
            .unwrap()
            .to_bytes_compact()
            .unwrap();
        verify_output(&input, &compacted, &worksheet(cells, &values));
        assert_eq!(compacted, repeated);
        println!(
            "growth,{cells},saves=5,normal={},compact={}",
            repeated.len(),
            compacted.len()
        );
    }
    if selected("SHEETPATCH_BENCH_CASES", "guarded") {
        guarded_batches(samples);
    }
    if selected("SHEETPATCH_BENCH_CASES", "tables") {
        table_batches(samples);
    }
    extra_workloads(samples);
}
