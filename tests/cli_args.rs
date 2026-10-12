//! Option placement, argument counts, and error message formats of the CLI.
use sheetpatch::{CellValue, Workbook};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/libreoffice.xlsx"
);

fn cli_in(directory: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

struct TempDir(PathBuf);

impl TempDir {
    /// A directory holding `in.xlsx` and a one-row `p.tsv`.
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sheetpatch-cli-args-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::copy(FIXTURE, path.join("in.xlsx")).unwrap();
        fs::write(path.join("p.tsv"), "Data\tA1\ttext\tPatched\n").unwrap();
        Self(path)
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn misplaced_options_are_usage_errors_and_never_become_files() {
    let directory = TempDir::new();
    let original = fs::read(directory.0.join("in.xlsx")).unwrap();
    for (args, slot, synopsis) in [
        (
            vec!["patch", "in.xlsx", "--check", "p.tsv"],
            Some(("--check", "OUTPUT")),
            Some("sheetpatch patch --check INPUT PATCH.tsv"),
        ),
        (
            vec!["patch", "in.xlsx", "out.xlsx", "--check"],
            Some(("--check", "PATCH.tsv")),
            Some("sheetpatch patch --check INPUT PATCH.tsv"),
        ),
        (
            vec!["patch", "--check", "in.xlsx", "--json"],
            Some(("--json", "PATCH.tsv")),
            None,
        ),
        (
            vec!["compact", "in.xlsx", "--help"],
            Some(("--help", "OUTPUT")),
            Some("sheetpatch compact --help"),
        ),
        (
            vec!["compact", "in.xlsx", "-h"],
            Some(("-h", "OUTPUT")),
            Some("sheetpatch compact -h"),
        ),
        (vec!["compact", "--", "in.xlsx"], None, None),
        (
            vec!["set", "in.xlsx", "-o", "Data", "A1", "text", "x"],
            Some(("-o", "OUTPUT")),
            None,
        ),
        (vec!["list", "-in.xlsx"], None, None),
        (
            vec!["list", "in.xlsx", "--json"],
            None,
            Some("sheetpatch list --json INPUT"),
        ),
        (
            vec!["get", "in.xlsx", "Data", "A1", "--json"],
            None,
            Some("sheetpatch get --json INPUT SHEET CELL [CELL ...]"),
        ),
        (
            vec!["patch", "in.xlsx", "out.xlsx", "p.tsv", "--check"],
            None,
            Some("sheetpatch patch --check INPUT PATCH.tsv"),
        ),
        (
            vec![
                "set", "in.xlsx", "out.xlsx", "Data", "A1", "text", "x", "-v",
            ],
            None,
            None,
        ),
    ] {
        let output = cli_in(&directory.0, &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let message = stderr(&output);
        assert!(message.contains("Usage:"), "{message}");
        if let Some((option, slot)) = slot {
            assert!(
                message.starts_with(&format!(
                    "sheetpatch: \"{option}\" is in the {slot} position, but options must come right after the command"
                )),
                "{message}"
            );
            assert!(
                message.contains(&format!("write \"./{option}\"")),
                "{message}"
            );
        }
        if let Some(synopsis) = synopsis {
            assert!(message.contains(synopsis), "{message}");
        }
        assert_eq!(directory.names(), ["in.xlsx", "p.tsv"], "{args:?}");
        assert_eq!(fs::read(directory.0.join("in.xlsx")).unwrap(), original);
    }
}

#[test]
fn unknown_and_repeated_options_are_usage_errors() {
    let directory = TempDir::new();
    for (args, expected) in [
        (
            vec!["get", "--bogus", "in.xlsx", "Data", "A1"],
            "unknown option \"--bogus\" for get; it accepts --json, --help or -h",
        ),
        (
            vec!["list", "-j", "in.xlsx"],
            "unknown option \"-j\" for list",
        ),
        (
            vec!["list", "--JSON", "in.xlsx"],
            "unknown option \"--JSON\" for list",
        ),
        (
            vec!["list", "--json", "--json", "in.xlsx"],
            "option \"--json\" is repeated",
        ),
        (
            vec!["get", "--json", "--json", "in.xlsx", "Data", "A1"],
            "option \"--json\" is repeated",
        ),
        (
            vec!["patch", "--check", "--check", "in.xlsx", "p.tsv"],
            "option \"--check\" is repeated",
        ),
        (
            vec![
                "set", "--json", "in.xlsx", "out.xlsx", "Data", "A1", "blank",
            ],
            "unknown option \"--json\" for set; it accepts --help or -h",
        ),
        (
            vec!["compact", "--check", "in.xlsx", "out.xlsx"],
            "unknown option \"--check\" for compact",
        ),
        (vec!["--bogus"], "unknown option \"--bogus\""),
        (vec!["bogus"], "unknown command \"bogus\""),
        (
            vec!["--help", "list"],
            "unexpected argument \"list\" after --help",
        ),
    ] {
        let output = cli_in(&directory.0, &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        let message = stderr(&output);
        assert!(
            message.starts_with(&format!("sheetpatch: {expected}")),
            "{message}"
        );
        assert!(message.contains("\n\nUsage:\n"), "{message}");
        assert_eq!(directory.names(), ["in.xlsx", "p.tsv"]);
    }
}

#[test]
fn wrong_argument_counts_print_the_expected_form_and_full_usage() {
    let directory = TempDir::new();
    for (args, expected) in [
        (vec![], "missing command"),
        (vec!["list"], "sheetpatch list [--json] INPUT"),
        (
            vec!["list", "in.xlsx", "extra"],
            "sheetpatch list [--json] INPUT",
        ),
        (
            vec!["get", "in.xlsx", "Data"],
            "sheetpatch get [--json] INPUT SHEET CELL [CELL ...]",
        ),
        (
            vec!["set", "in.xlsx", "out.xlsx", "Data", "A1"],
            "sheetpatch set INPUT OUTPUT SHEET CELL TYPE [VALUE]",
        ),
        (
            vec!["patch", "in.xlsx", "p.tsv"],
            "sheetpatch patch INPUT OUTPUT PATCH.tsv",
        ),
        (
            vec!["patch", "--check", "in.xlsx", "p.tsv", "out.xlsx"],
            "sheetpatch patch --check INPUT PATCH.tsv",
        ),
        (
            vec!["compact", "in.xlsx"],
            "sheetpatch compact INPUT OUTPUT",
        ),
        (
            vec!["set", "in.xlsx", "out.xlsx", "Data", "A1", "text"],
            "text needs a VALUE",
        ),
        (
            vec!["set", "in.xlsx", "out.xlsx", "Data", "A1", "blank", "x"],
            "blank takes no VALUE, but \"x\" was given",
        ),
        (
            vec!["help", "list", "get"],
            "help accepts at most one COMMAND",
        ),
        (vec!["help", "Text"], "unknown command \"Text\" for help"),
    ] {
        let output = cli_in(&directory.0, &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        let message = stderr(&output);
        assert!(message.contains(expected), "{message}");
        assert!(message.contains("\n\nUsage:\n"), "{message}");
        assert_eq!(directory.names(), ["in.xlsx", "p.tsv"]);
    }
}

#[test]
fn data_errors_print_one_quoted_line_and_a_help_hint() {
    let directory = TempDir::new();
    for (args, expected) in [
        (
            vec!["set", "in.xlsx", "out.xlsx", "Data", "A1", "number", "42 "],
            "Invalid finite number: \"42 \"\nRun 'sheetpatch help set' for details.",
        ),
        (
            vec!["set", "in.xlsx", "out.xlsx", "Data", "A1", "Text", "x"],
            "unknown value type \"Text\"; expected text, number, bool, error or blank\nRun 'sheetpatch help set' for details.",
        ),
        (
            vec!["set", "in.xlsx", "out.xlsx", "Data", "A1", "bool", "TRUE"],
            "Boolean values must be true or false, not \"TRUE\"\nRun 'sheetpatch help set' for details.",
        ),
        (
            vec!["set", "in.xlsx", "out.xlsx", "Data", "A0", "text", "x"],
            "invalid cell reference \"A0\"\nRun 'sheetpatch help set' for details.",
        ),
        (
            vec!["get", "in.xlsx", "Data", "A1", "B2\r"],
            "invalid cell reference \"B2\\r\"\nRun 'sheetpatch help get' for details.",
        ),
        (
            vec!["get", "--json", "in.xlsx", "Data", "-5"],
            "invalid cell reference \"-5\"\nRun 'sheetpatch help get' for details.",
        ),
    ] {
        let output = cli_in(&directory.0, &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(stderr(&output), format!("sheetpatch: {expected}\n"));
        assert_eq!(directory.names(), ["in.xlsx", "p.tsv"]);
    }
}

#[test]
fn missing_sheets_are_quoted_so_invisible_characters_show() {
    let directory = TempDir::new();
    for args in [
        vec!["get", "in.xlsx", "Data ", "A1"],
        vec!["get", "--json", "in.xlsx", "Data ", "A1"],
        vec!["set", "in.xlsx", "out.xlsx", "Data ", "A1", "text", "x"],
    ] {
        let output = cli_in(&directory.0, &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            stderr(&output),
            "sheetpatch: worksheet not found: \"Data \"\n"
        );
        assert_eq!(directory.names(), ["in.xlsx", "p.tsv"]);
    }
}

#[test]
fn values_and_sheet_names_may_start_with_a_dash() {
    let directory = TempDir::new();
    for (kind, value, expected) in [
        ("number", "-5", CellValue::Number(-5.0)),
        ("text", "--json", CellValue::Text("--json".into())),
        ("text", "-", CellValue::Text("-".into())),
    ] {
        let output = cli_in(
            &directory.0,
            &["set", "in.xlsx", "out.xlsx", "Data", "A1", kind, value],
        );
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(
            Workbook::open(directory.0.join("out.xlsx"))
                .unwrap()
                .get_cell("Data", "A1")
                .unwrap()
                .value,
            expected
        );
    }
    // A sheet name is never taken for an option; this one does not exist.
    let output = cli_in(&directory.0, &["get", "in.xlsx", "--json", "A1"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "sheetpatch: worksheet not found: \"--json\"\n"
    );
}

#[test]
fn files_named_like_options_are_reached_with_a_relative_prefix() {
    let directory = TempDir::new();
    fs::copy(FIXTURE, directory.0.join("-in.xlsx")).unwrap();
    let listed = cli_in(&directory.0, &["list", "./-in.xlsx"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert_eq!(listed.stdout, b"Data\nNotes\n");
    for args in [
        vec!["compact", "./-in.xlsx", "./--help"],
        vec!["patch", "in.xlsx", "./--check", "p.tsv"],
        vec!["compact", "in.xlsx", "./-"],
    ] {
        let output = cli_in(&directory.0, &args);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(output.stdout.is_empty());
    }
    assert_eq!(
        directory.names(),
        ["-", "--check", "--help", "-in.xlsx", "in.xlsx", "p.tsv"]
    );
    assert_eq!(
        Workbook::open(directory.0.join("--check"))
            .unwrap()
            .get_cell("Data", "A1")
            .unwrap()
            .value,
        CellValue::Text("Patched".into())
    );
}

#[test]
fn help_options_right_after_a_command_show_its_help() {
    let directory = TempDir::new();
    for (args, command) in [
        (vec!["get", "--json", "--help"], "get"),
        (vec!["patch", "--check", "-h", "in.xlsx", "p.tsv"], "patch"),
        (vec!["compact", "--help", "in.xlsx", "out.xlsx"], "compact"),
    ] {
        let output = cli_in(&directory.0, &args);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(output.stderr.is_empty());
        assert_eq!(
            output.stdout,
            cli_in(&directory.0, &["help", command]).stdout
        );
        assert_eq!(directory.names(), ["in.xlsx", "p.tsv"]);
    }
}
