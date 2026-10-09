//! Patch validation and command help are exercised through the public CLI.
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output, Stdio},
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

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sheetpatch-cli-validation-{}-{}",
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
fn checks_complete_transactions_without_writing_the_workbook_or_other_files() {
    let directory = TempDir::new();
    let workbook = directory.0.join("input.xlsx");
    let patch = directory.0.join("edits.tsv");
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
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 2);
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
fn checks_use_actual_workbook_guards_and_leave_inputs_unchanged_on_failure() {
    let directory = TempDir::new();
    let workbook = directory.0.join("input.xlsx");
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
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }
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
    for command in ["list", "get", "set", "patch", "compact", "help"] {
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
