//! CLI reads exercise the independently exported application fixture.
mod common;

use common::TempDir;
use sheetpatch::{CellValue, Workbook};
use std::{
    fs,
    process::{Command, Output, Stdio},
};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/libreoffice.xlsx"
);

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
        .args(args)
        .output()
        .unwrap()
}

fn success(args: &[&str]) -> String {
    let output = cli(args);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn batch_reads_preserve_order_duplicates_and_single_value_behavior() {
    assert_eq!(success(&["get", FIXTURE, "Data", "D1"]), "25\n");
    assert_eq!(
        success(&["get", FIXTURE, "Data", "D1", "a1", "E1", "F3", "D1"]),
        "25\nOriginal text\nCafé ☕\n\n25\n"
    );
}

#[test]
fn ranges_expand_row_by_row_in_argument_order() {
    assert_eq!(
        success(&["get", FIXTURE, "Data", "A1:C2", "D1", "e1:d1", "B1:B1"]),
        "Original text\n12.5\n1\n\n\n\n25\n25\nCafé ☕\n12.5\n"
    );
    // Corners can be given in any order; each cell prints as a single address would.
    let range = success(&["get", "--json", FIXTURE, "Data", "B2:a1"]);
    let cells = success(&["get", "--json", FIXTURE, "Data", "A1", "B1", "A2", "B2"]);
    assert_eq!(range, cells);
    assert!(
        range.starts_with("[{\"sheet\":\"Data\",\"cell\":\"A1\",\"type\":\"text\""),
        "{range}"
    );
}

#[test]
fn reads_up_to_the_cell_limit_succeed() {
    // 16,384 columns by 64 rows is exactly the 1,048,576-cell limit.
    let output = success(&["get", FIXTURE, "Data", "A1:XFD64"]);
    assert_eq!(output.lines().count(), 1_048_576);
    assert_eq!(
        output.len(),
        1_048_576 + "Original text12.5125Café ☕".len()
    );
    assert!(output.starts_with("Original text\n12.5\n1\n25\nCafé ☕\n\n"));
    // Single cells count toward the limit like ranges: 1,032,192 + 16,383 + 1.
    let output = success(&["get", FIXTURE, "Data", "A1:XFD63", "B2:XFD2", "E1"]);
    assert_eq!(output.lines().count(), 1_048_576);
    assert!(output.ends_with("\n\nCafé ☕\n"));
}

#[test]
fn json_lists_names_part_paths_and_visibility_in_workbook_order() {
    assert_eq!(success(&["list", FIXTURE]), "Data\nNotes\n");
    assert_eq!(
        success(&["list", "--json", FIXTURE]),
        concat!(
            "[{\"name\":\"Data\",\"path\":\"xl/worksheets/sheet1.xml\",\"visibility\":\"visible\"},",
            "{\"name\":\"Notes\",\"path\":\"xl/worksheets/sheet2.xml\",\"visibility\":\"visible\"}]\n"
        )
    );
}

#[test]
fn json_reads_report_cached_formulas_styles_blanks_and_normalized_addresses() {
    assert_eq!(
        success(&["get", "--json", FIXTURE, "Data", "a1", "D1", "F3", "a1"]),
        concat!(
            "[{\"sheet\":\"Data\",\"cell\":\"A1\",\"type\":\"text\",",
            "\"value\":\"Original text\",\"formula\":null,\"style_index\":1},",
            "{\"sheet\":\"Data\",\"cell\":\"D1\",\"type\":\"number\",",
            "\"value\":25,\"formula\":\"B1*2\",\"style_index\":0},",
            "{\"sheet\":\"Data\",\"cell\":\"F3\",\"type\":\"blank\",",
            "\"value\":null,\"formula\":null,\"style_index\":null},",
            "{\"sheet\":\"Data\",\"cell\":\"A1\",\"type\":\"text\",",
            "\"value\":\"Original text\",\"formula\":null,\"style_index\":1}]\n"
        )
    );
}

#[test]
fn json_preserves_multiline_text_and_distinguishes_boolean_error_and_blank_values() {
    let directory = TempDir::new();
    let path = directory.join("edited.xlsx");
    let mut workbook = Workbook::open(FIXTURE).unwrap();
    workbook
        .set_cells(
            "Notes",
            [
                (
                    "A2",
                    CellValue::from("Café 😀 \"quoted\" \\path\tline\nnext\rend"),
                ),
                ("B2", CellValue::Bool(false)),
                ("C2", CellValue::Error("#N/A".into())),
                ("D2", CellValue::Number(-1.25)),
            ],
        )
        .unwrap();
    workbook.save(&path).unwrap();
    let before = fs::read(&path).unwrap();
    assert_eq!(
        success(&[
            "get",
            "--json",
            path.to_str().unwrap(),
            "Notes",
            "A2",
            "B2",
            "C2",
            "D2",
            "E2",
        ]),
        concat!(
            "[{\"sheet\":\"Notes\",\"cell\":\"A2\",\"type\":\"text\",",
            "\"value\":\"Café 😀 \\\"quoted\\\" \\\\path\\tline\\nnext\\rend\",",
            "\"formula\":null,\"style_index\":null},",
            "{\"sheet\":\"Notes\",\"cell\":\"B2\",\"type\":\"bool\",",
            "\"value\":false,\"formula\":null,\"style_index\":null},",
            "{\"sheet\":\"Notes\",\"cell\":\"C2\",\"type\":\"error\",",
            "\"value\":\"#N/A\",\"formula\":null,\"style_index\":null},",
            "{\"sheet\":\"Notes\",\"cell\":\"D2\",\"type\":\"number\",",
            "\"value\":-1.25,\"formula\":null,\"style_index\":null},",
            "{\"sheet\":\"Notes\",\"cell\":\"E2\",\"type\":\"blank\",",
            "\"value\":null,\"formula\":null,\"style_index\":null}]\n"
        )
    );
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn failed_batch_reads_emit_no_partial_results() {
    for json in [false, true] {
        let mut args = vec!["get"];
        if json {
            args.push("--json");
        }
        args.extend([FIXTURE, "Data", "A1", "A0"]);
        let invalid = cli(&args);
        assert_eq!(invalid.status.code(), Some(2));
        assert!(invalid.stdout.is_empty());
        assert!(String::from_utf8(invalid.stderr).unwrap().contains("A0"));
        let sheet_index = if json { 3 } else { 2 };
        args[sheet_index] = "Missing";
        args.pop();
        let missing_sheet = cli(&args);
        assert_eq!(missing_sheet.status.code(), Some(1));
        assert!(missing_sheet.stdout.is_empty());
    }
}

#[test]
fn invalid_read_and_set_arguments_fail_before_opening_the_input() {
    for args in [
        vec!["get", "", "Data", "A1", "A0"],
        vec!["get", "--json", "", "Data", "A0"],
        vec!["set", "", "", "Data", "A0", "number", "3"],
        vec!["set", "", "", "Data", "A1", "error", "invalid"],
        vec!["get", "--json"],
        vec!["get", "--json", FIXTURE, "Data"],
        vec!["get", "", "Data", "A1", "B2:"],
        vec!["get", "--json", "", "Data", "A1:XFD1048576"],
        vec!["list", "--json"],
        vec!["cells", "--json"],
        vec!["cells", ""],
        vec!["cells", "", "Data", "A0"],
        vec!["cells", "--json", "", "Data", "A1:"],
    ] {
        let output = cli(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn large_reads_are_complete_and_in_order() {
    let mut args = vec!["get", FIXTURE, "Data"];
    let mut expected = String::new();
    for _ in 0..3000 {
        args.extend(["A1", "D1", "F3"]);
        expected.push_str("Original text\n25\n\n");
    }
    assert_eq!(success(&args), expected);
}

#[test]
fn a_reader_that_closes_the_pipe_early_ends_output_quietly() {
    // 8,000 values of about 14 bytes exceed any default pipe buffer, so the
    // child must see the closed pipe. The arguments stay within Windows
    // command-line limits. A range of 131,072 cells likewise outgrows the
    // buffer, even as blank lines.
    let addresses = vec!["A1"; 8000];
    let cases: [(bool, &[&str]); 4] = [
        (false, &addresses),
        (true, &addresses),
        (false, &["A1:XFD8"]),
        (true, &["A1:XFD8"]),
    ];
    for (json, cells) in cases {
        let mut args = vec!["get"];
        if json {
            args.push("--json");
        }
        args.extend([FIXTURE, "Data"]);
        args.extend(cells);
        let mut child = Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdout.take());
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    }
}

#[test]
fn cells_list_the_fixture_values_and_formulas() {
    assert_eq!(
        success(&["cells", FIXTURE, "Data"]),
        "A1\ttext\tOriginal text\nB1\tnumber\t12.5\nC1\tnumber\t1\nD1\tnumber\t25\nE1\ttext\tCafé ☕\n"
    );
    assert_eq!(
        success(&["cells", FIXTURE, "Notes"]),
        "A1\ttext\tKeep this sheet\n"
    );
    // JSON objects match get --json for the same cells, including formulas.
    assert_eq!(
        success(&["cells", "--json", FIXTURE, "Data"]),
        success(&["get", "--json", FIXTURE, "Data", "A1:E1"])
    );
    assert_eq!(
        success(&["cells", "--json", FIXTURE, "Data", "d1:XFD1048576"]),
        concat!(
            "[{\"sheet\":\"Data\",\"cell\":\"D1\",\"type\":\"number\",",
            "\"value\":25,\"formula\":\"B1*2\",\"style_index\":0},",
            "{\"sheet\":\"Data\",\"cell\":\"E1\",\"type\":\"text\",",
            "\"value\":\"Café ☕\",\"formula\":null,\"style_index\":0}]\n"
        )
    );
}
