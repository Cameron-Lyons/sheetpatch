//! Patch validation and command help are exercised through the public CLI.
mod common;

use common::TempDir;
use std::{
    fs,
    io::Write,
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

fn check_stdin(input: &str, patch: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
        .args(["patch", "--check", input, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(patch).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn checks_complete_transactions_without_writing_the_workbook_or_other_files() {
    let directory = TempDir::new();
    let workbook = directory.join("input.xlsx");
    let patch = directory.join("edits.tsv");
    fs::copy(FIXTURE, &workbook).unwrap();
    let original = fs::read(&workbook).unwrap();
    let contents = b"# comment\nData\tA1\ttext\tFirst\nData\tA1\ttext\tLast\nNotes\tB3\tnumber\t42\nData\tF3\tblank\n";
    fs::write(&patch, contents).unwrap();
    let output = cli(&[
        "patch",
        "--check",
        workbook.to_str().unwrap(),
        patch.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"Validated 4 patch rows.\n");
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(&workbook).unwrap(), original);
    assert_eq!(fs::read(&patch).unwrap(), contents);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn stdin_checks_accept_empty_patches_and_report_raw_edit_counts() {
    for (patch, expected) in [
        ("# comment\r\n\r\n", "Validated 0 patch rows.\n"),
        (
            "\u{feff}Notes\tB2\ttext\tCafé ☕\r\n",
            "Validated 1 patch row.\n",
        ),
    ] {
        let output = check_stdin(FIXTURE, patch.as_bytes());
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(output.stdout, expected.as_bytes());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn duplicate_patch_targets_keep_the_last_value_and_count_every_row() {
    let directory = TempDir::new();
    let patch = directory.join("duplicates.tsv");
    let output = directory.join("output.xlsx");
    let rows = b"Data\tA1\ttext\tfirst\nNotes\tA1\ttext\tother sheet\n\
        # comment\n\nData\ta1\tnumber\t7\nData\tA1\ttext\tlast\nNotes\ta1\tbool\tfalse\n";
    fs::write(&patch, rows).unwrap();
    let checked = check_stdin(FIXTURE, rows);
    assert!(checked.status.success(), "{:?}", checked.stderr);
    assert_eq!(checked.stdout, b"Validated 5 patch rows.\n");
    assert!(checked.stderr.is_empty());
    let applied = cli(&[
        "patch",
        FIXTURE,
        output.to_str().unwrap(),
        patch.to_str().unwrap(),
    ]);
    assert!(applied.status.success(), "{:?}", applied.stderr);
    assert!(applied.stdout.is_empty());
    assert!(applied.stderr.is_empty());
    let workbook = sheetpatch::Workbook::open(&output).unwrap();
    assert_eq!(
        workbook.get_cell("Data", "A1").unwrap().value,
        sheetpatch::CellValue::from("last")
    );
    assert_eq!(
        workbook.get_cell("Notes", "A1").unwrap().value,
        sheetpatch::CellValue::Bool(false)
    );
}

#[test]
fn duplicate_rows_preserve_missing_sheet_and_invalid_value_error_precedence() {
    // The missing sheet is reported at its first occurrence even when its
    // target is repeated, ahead of both a later missing sheet and an earlier
    // protected-cell edit. Names deliberately oppose alphabetical order.
    let missing = check_stdin(
        FIXTURE,
        b"Data\tD1\tnumber\t1\nZMissing\tA1\ttext\tfirst\n\
          AMissing\tA1\ttext\tother\nZMissing\ta1\ttext\tlast\n",
    );
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert_eq!(
        missing.stderr,
        b"sheetpatch: Patch line 2: worksheet not found: \"ZMissing\"\n"
    );
    // A later valid value cannot hide an invalid overwritten value. Patch
    // syntax still takes precedence over opening the missing workbook.
    for (rows, line) in [
        ("Data\tA1\tnumber\tNaN\nData\ta1\ttext\tvalid\n", 1),
        (
            "Data\tA1\ttext\tfirst\nData\ta1\tnumber\tNaN\nData\tA1\ttext\tlast\n",
            2,
        ),
    ] {
        let invalid = check_stdin("", rows.as_bytes());
        assert_eq!(invalid.status.code(), Some(2));
        assert!(invalid.stdout.is_empty());
        let message = String::from_utf8(invalid.stderr).unwrap();
        assert!(
            message.starts_with(&format!(
                "sheetpatch: Patch line {line}: Invalid finite number:"
            )),
            "{message}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn repeated_patch_values_do_not_accumulate_in_memory() {
    let row = format!("Data\tA1\ttext\t{}\n", "x".repeat(32_767));
    for count in [1, 8192] {
        let mut child = Command::new("sh")
            .args([
                "-c",
                "ulimit -c 0 && ulimit -v 262144 && exec \"$@\"",
                "sheetpatch-patch-memory-test",
                env!("CARGO_BIN_EXE_sheetpatch"),
                "patch",
                "--check",
                FIXTURE,
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        // Stream over 256 MiB of valid repeated edits without retaining the
        // patch in this test process. Only one final cell value is needed.
        for _ in 0..count {
            if stdin.write_all(row.as_bytes()).is_err() {
                break;
            }
        }
        drop(stdin);
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{count} rows: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let noun = if count == 1 { "row" } else { "rows" };
        assert_eq!(
            output.stdout,
            format!("Validated {count} patch {noun}.\n").as_bytes()
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn checks_use_actual_workbook_guards_and_leave_inputs_unchanged_on_failure() {
    let directory = TempDir::new();
    let workbook = directory.join("input.xlsx");
    fs::copy(FIXTURE, &workbook).unwrap();
    let original = fs::read(&workbook).unwrap();
    for patch in [
        "Data\tA1\ttext\tAllowed\nData\tD1\tnumber\t99\n",
        "Data\tA1\ttext\tAllowed\nMissing\tA1\ttext\tUnknown sheet\n",
    ] {
        let output = check_stdin(workbook.to_str().unwrap(), patch.as_bytes());
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
        assert_eq!(fs::read(&workbook).unwrap(), original);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}

#[test]
fn refused_edits_report_the_last_patch_line_for_their_cell() {
    let directory = TempDir::new();
    let workbook = directory.join("input.xlsx");
    let output = directory.join("output.xlsx");
    fs::copy(FIXTURE, &workbook).unwrap();
    // D1 holds the formula B1*2. Duplicate cells apply the last line's value,
    // so that line is the one reported.
    for (patch, line) in [
        ("Data\tA1\ttext\tAllowed\nData\tD1\tnumber\t99\n", 2),
        (
            "Data\tD1\tnumber\t1\nData\tA1\ttext\tx\n# comment\n\nData\td1\tblank\nNotes\tD1\ttext\tother sheet\n",
            5,
        ),
    ] {
        for check in [true, false] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_sheetpatch"));
            command.arg("patch");
            if check {
                command.args(["--check", workbook.to_str().unwrap(), "-"]);
            } else {
                command.args([workbook.to_str().unwrap(), output.to_str().unwrap(), "-"]);
            }
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(patch.as_bytes())
                .unwrap();
            let result = child.wait_with_output().unwrap();
            assert_eq!(result.status.code(), Some(1));
            assert!(result.stdout.is_empty());
            assert_eq!(
                String::from_utf8(result.stderr).unwrap(),
                format!(
                    "sheetpatch: Patch line {line}: protected cell: worksheet \"Data\": D1 contains a formula\n"
                )
            );
        }
    }
    assert!(!output.exists());
    assert_eq!(fs::read(&workbook).unwrap(), fs::read(FIXTURE).unwrap());
    // A single set names its cell without a line.
    let result = cli(&[
        "set",
        workbook.to_str().unwrap(),
        output.to_str().unwrap(),
        "Data",
        "D1",
        "number",
        "1",
    ]);
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(result.stderr).unwrap(),
        "sheetpatch: protected cell: worksheet \"Data\": D1 contains a formula\n"
    );
    assert!(!output.exists());
}

#[test]
fn invalid_checks_report_patch_line_before_opening_the_workbook() {
    for patch in [
        b"Data\tA1\ttext\tValid\nData\tA0\ttext\tInvalid address\n".as_slice(),
        b"Data\tA1\ttext\tValid\nData\tB2\terror\tinvalid\n".as_slice(),
        b"Data\tA1\ttext\tValid\nData\tB2\ttext\t\xff\n".as_slice(),
    ] {
        let output = check_stdin("", patch);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("Patch line 2:"), "{error}");
    }
    for args in [
        vec!["patch", "--check"],
        vec!["patch", "--check", FIXTURE],
        vec!["patch", "--check", FIXTURE, "-", "unexpected"],
    ] {
        let output = cli(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn command_help_is_available_without_opening_an_input() {
    assert_eq!(cli(&["help"]).stdout, cli(&["--help"]).stdout);
    let usage = String::from_utf8(cli(&["help"]).stdout).unwrap();
    for command in ["list", "get", "cells", "set", "patch", "compact", "help"] {
        assert!(
            usage.contains(&format!("\n  sheetpatch {command} ")),
            "{command}"
        );
        let named = cli(&["help", command]);
        assert!(named.status.success());
        assert!(named.stderr.is_empty());
        let help = String::from_utf8(named.stdout.clone()).unwrap();
        assert!(help.contains(&format!("sheetpatch {command} ")));
        for flag in ["--help", "-h"] {
            let output = cli(&[command, flag]);
            assert!(output.status.success());
            assert_eq!(output.stdout, named.stdout);
            assert!(output.stderr.is_empty());
        }
    }
    let unknown = cli(&["help", "unknown"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(unknown.stdout.is_empty());
}

#[test]
fn carriage_returns_inside_rows_are_rejected_with_their_line() {
    let directory = TempDir::new();
    let workbook = directory.join("input.xlsx");
    let output = directory.join("output.xlsx");
    fs::copy(FIXTURE, &workbook).unwrap();
    for (patch, line) in [
        (
            "Data\tA2\ttext\tAlpha\rData\tB2\tnumber\t42\rData\tC2\ttext\tGamma\r",
            1,
        ),
        ("Data\tA1\ttext\tok\r\nData\tA2\ttext\tsplit\rvalue\r\n", 2),
    ] {
        let path = directory.join("edits.tsv");
        fs::write(&path, patch).unwrap();
        for args in [
            vec!["patch", "--check", workbook.to_str().unwrap(), "-"],
            vec![
                "patch",
                workbook.to_str().unwrap(),
                output.to_str().unwrap(),
                path.to_str().unwrap(),
            ],
        ] {
            // Only a `-` PATCH reads standard input; piping input to a command
            // that exits without reading it would race with a broken pipe.
            let reads_stdin = args.last() == Some(&"-");
            let mut child = Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
                .args(&args)
                .stdin(if reads_stdin {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            if reads_stdin {
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(patch.as_bytes())
                    .unwrap();
            }
            let result = child.wait_with_output().unwrap();
            assert_eq!(result.status.code(), Some(2));
            assert!(result.stdout.is_empty());
            assert_eq!(
                String::from_utf8(result.stderr).unwrap(),
                format!(
                    "sheetpatch: Patch line {line}: carriage return without line feed; use LF or CRLF line endings\nRun 'sheetpatch help patch' for the format.\n"
                )
            );
        }
    }
    assert!(!output.exists());
    // A final carriage return without a line feed stays in the text value.
    let result = check_stdin(FIXTURE, b"Data\tA1\ttext\tvalue\r");
    assert_eq!(result.stdout, b"Validated 1 patch row.\n");
}

#[test]
fn missing_sheets_report_the_patch_line_and_quoted_name() {
    let directory = TempDir::new();
    let workbook = directory.join("input.xlsx");
    let output = directory.join("output.xlsx");
    fs::copy(FIXTURE, &workbook).unwrap();
    for (patch, expected) in [
        (
            "Data\tA1\ttext\tok\n\n# comment\nNotes\tB2\tnumber\t1\nMissing \tA1\ttext\tx\nOther\tA1\ttext\tx\n",
            "Patch line 5: worksheet not found: \"Missing \"",
        ),
        // Concatenated BOM files: only the first BOM is a byte-order mark.
        (
            "\u{feff}Data\tA1\ttext\tx\n\u{feff}Data\tA2\ttext\ty\n",
            "Patch line 2: worksheet not found: \"\\u{feff}Data\"",
        ),
        ("\tA1\ttext\tx\n", "Patch line 1: worksheet not found: \"\""),
    ] {
        for check in [true, false] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_sheetpatch"));
            command.arg("patch");
            if check {
                command.args(["--check", workbook.to_str().unwrap(), "-"]);
            } else {
                command.args([workbook.to_str().unwrap(), output.to_str().unwrap(), "-"]);
            }
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(patch.as_bytes())
                .unwrap();
            let result = child.wait_with_output().unwrap();
            assert_eq!(result.status.code(), Some(1));
            assert!(result.stdout.is_empty());
            assert_eq!(
                String::from_utf8(result.stderr).unwrap(),
                format!("sheetpatch: {expected}\n")
            );
        }
    }
    assert!(!output.exists());
    assert_eq!(fs::read(&workbook).unwrap(), fs::read(FIXTURE).unwrap());
}

#[test]
fn patch_data_errors_print_one_line_and_a_format_hint() {
    let output = check_stdin(FIXTURE, b"Data\tA1\ttext\tok\nData\tA2\tnumber\t42 \n");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "sheetpatch: Patch line 2: Invalid finite number: \"42 \"\nRun 'sheetpatch help patch' for the format.\n"
    );
}
