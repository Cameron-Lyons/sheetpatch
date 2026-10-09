//! CLI reads exercise the independently exported application fixture.
use sheetpatch::{CellValue, Workbook};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
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

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sheetpatch-cli-read-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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
fn json_lists_names_and_part_paths_in_workbook_order() {
    assert_eq!(success(&["list", FIXTURE]), "Data\nNotes\n");
    assert_eq!(
        success(&["list", "--json", FIXTURE]),
        concat!(
            "[{\"name\":\"Data\",\"path\":\"xl/worksheets/sheet1.xml\"},",
            "{\"name\":\"Notes\",\"path\":\"xl/worksheets/sheet2.xml\"}]\n"
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
    let path = directory.0.join("edited.xlsx");
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
        vec!["list", "--json"],
    ] {
        let output = cli(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
    }
}
