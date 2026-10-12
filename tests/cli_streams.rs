//! Standard stream workbooks, output destinations, and I/O error context.
use sheetpatch::{CellValue, Workbook};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/libreoffice.xlsx"
);

fn command(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sheetpatch"));
    command.args(args);
    command
}

/// Run with `input` on standard input and capture standard output.
fn piped(args: &[&str], input: &[u8]) -> Output {
    let mut child = command(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // The child may exit before reading everything, for example on usage errors.
    let _ = stdin.write_all(input);
    drop(stdin);
    child.wait_with_output().unwrap()
}

fn cli(args: &[&str]) -> Output {
    command(args).stdin(Stdio::null()).output().unwrap()
}

fn success(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    output.stdout
}

fn cell(bytes: Vec<u8>, sheet: &str, address: &str) -> CellValue {
    Workbook::from_bytes(bytes)
        .unwrap()
        .get_cell(sheet, address)
        .unwrap()
        .value
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sheetpatch-cli-streams-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn count(&self) -> usize {
        fs::read_dir(&self.0).unwrap().count()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn every_command_reads_a_workbook_from_standard_input() {
    let workbook = fs::read(FIXTURE).unwrap();
    assert_eq!(success(piped(&["list", "-"], &workbook)), b"Data\nNotes\n");
    assert_eq!(
        success(piped(&["list", "--json", "-"], &workbook)),
        success(cli(&["list", "--json", FIXTURE]))
    );
    assert_eq!(
        success(piped(&["get", "-", "Data", "D1", "A1"], &workbook)),
        b"25\nOriginal text\n"
    );
    assert_eq!(
        success(piped(&["get", "--json", "-", "Data", "D1"], &workbook)),
        success(cli(&["get", "--json", FIXTURE, "Data", "D1"]))
    );

    let directory = TempDir::new();
    let output = directory.0.join("out.xlsx");
    let patch = directory.0.join("p.tsv");
    fs::write(&patch, "Data\tB1\tnumber\t7\n").unwrap();
    for (args, address, expected) in [
        (
            vec![
                "set",
                "-",
                text(&output),
                "Data",
                "A1",
                "text",
                "From stdin",
            ],
            "A1",
            CellValue::Text("From stdin".into()),
        ),
        (
            vec!["patch", "-", text(&output), text(&patch)],
            "B1",
            CellValue::Number(7.0),
        ),
        (
            vec!["compact", "-", text(&output)],
            "A1",
            CellValue::Text("Original text".into()),
        ),
    ] {
        assert!(success(piped(&args, &workbook)).is_empty());
        assert_eq!(cell(fs::read(&output).unwrap(), "Data", address), expected);
    }
    assert_eq!(
        success(piped(&["patch", "--check", "-", text(&patch)], &workbook)),
        b"Validated 1 patch row.\n"
    );
}

#[test]
fn workbooks_written_to_standard_output_match_saved_files() {
    let directory = TempDir::new();
    let saved = directory.0.join("saved.xlsx");
    let patch = directory.0.join("p.tsv");
    let rows = "Data\tA1\ttext\tPatched\nNotes\tB2\tbool\ttrue\n";
    fs::write(&patch, rows).unwrap();
    for args in [
        vec!["set", FIXTURE, "{}", "Data", "C3", "number", "-1.5"],
        vec!["patch", FIXTURE, "{}", text(&patch)],
        vec!["compact", FIXTURE, "{}"],
    ] {
        let to_file: Vec<_> = args
            .iter()
            .map(|arg| if *arg == "{}" { text(&saved) } else { arg })
            .collect();
        assert!(success(cli(&to_file)).is_empty());
        let to_stdout: Vec<_> = args
            .iter()
            .map(|arg| if *arg == "{}" { "-" } else { arg })
            .collect();
        assert_eq!(success(cli(&to_stdout)), fs::read(&saved).unwrap());
    }
    assert_eq!(directory.count(), 2);

    // The patch comes from standard input while the workbook goes to standard output.
    let streamed = success(piped(&["patch", FIXTURE, "-", "-"], rows.as_bytes()));
    assert_eq!(
        cell(streamed.clone(), "Data", "A1"),
        CellValue::Text("Patched".into())
    );
    assert_eq!(cell(streamed, "Notes", "B2"), CellValue::Bool(true));

    // Both ends of a pipeline can be workbooks.
    let workbook = fs::read(FIXTURE).unwrap();
    let edited = success(piped(
        &["set", "-", "-", "Notes", "A1", "text", "Piped"],
        &workbook,
    ));
    assert_eq!(cell(edited, "Notes", "A1"), CellValue::Text("Piped".into()));
    assert_eq!(directory.count(), 2);
}

#[test]
fn input_and_patch_cannot_both_read_standard_input() {
    let directory = TempDir::new();
    let destination = directory.0.join("out.xlsx");
    let workbook = fs::read(FIXTURE).unwrap();
    for args in [
        vec!["patch", "-", text(&destination), "-"],
        vec!["patch", "-", "-", "-"],
        vec!["patch", "--check", "-", "-"],
    ] {
        let output = piped(&args, &workbook);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        let message = String::from_utf8(output.stderr).unwrap();
        assert!(
            message.starts_with(
                "sheetpatch: INPUT and PATCH.tsv cannot both read standard input (-)\n"
            ),
            "{message}"
        );
        assert!(message.contains("Usage:"));
    }
    assert_eq!(directory.count(), 0);
}

#[test]
fn invalid_standard_input_workbooks_name_the_stream() {
    for input in [b"".as_slice(), b"not a workbook"] {
        let output = piped(&["list", "-"], input);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let message = String::from_utf8(output.stderr).unwrap();
        assert!(
            message.starts_with("sheetpatch: cannot read workbook from standard input: "),
            "{message}"
        );
    }
}

#[test]
fn io_errors_name_the_path_and_operation() {
    let directory = TempDir::new();
    let input = directory.0.join("in.xlsx");
    let patch = directory.0.join("p.tsv");
    let missing = directory.0.join("missing");
    fs::copy(FIXTURE, &input).unwrap();
    fs::write(&patch, "Data\tA1\ttext\tx\n").unwrap();
    let missing_workbook = missing.join("in.xlsx");
    let missing_patch = missing.join("p.tsv");
    let missing_output = missing.join("out.xlsx");
    for (args, prefix) in [
        (
            vec!["patch", text(&missing_workbook), text(&input), text(&patch)],
            format!("cannot read workbook '{}': ", missing_workbook.display()),
        ),
        (
            vec!["list", text(&directory.0)],
            format!("cannot read workbook '{}': ", directory.0.display()),
        ),
        (
            vec!["patch", text(&input), text(&input), text(&missing_patch)],
            format!("cannot read patch '{}': ", missing_patch.display()),
        ),
        (
            vec!["patch", "--check", text(&input), text(&missing_patch)],
            format!("cannot read patch '{}': ", missing_patch.display()),
        ),
        (
            vec![
                "set",
                text(&input),
                text(&missing_output),
                "Data",
                "A1",
                "text",
                "x",
            ],
            format!("cannot save '{}': ", missing_output.display()),
        ),
        (
            vec!["patch", text(&input), text(&missing_output), text(&patch)],
            format!("cannot save '{}': ", missing_output.display()),
        ),
        (
            vec!["compact", text(&input), text(&missing_output)],
            format!("cannot save '{}': ", missing_output.display()),
        ),
    ] {
        let output = cli(&args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(output.stdout.is_empty());
        let message = String::from_utf8(output.stderr).unwrap();
        assert!(
            message.starts_with(&format!("sheetpatch: {prefix}")),
            "{message}"
        );
        assert!(!message.contains("I/O error"), "{message}");
        assert!(!message.contains("Usage:"), "{message}");
    }
    assert_eq!(fs::read(&input).unwrap(), fs::read(FIXTURE).unwrap());
    assert_eq!(directory.count(), 2);
}

// Native byte paths are exercised on Linux; macOS rejects invalid UTF-8 names.
#[cfg(target_os = "linux")]
#[test]
fn io_errors_display_non_unicode_paths_lossily() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let directory = TempDir::new();
    let missing = directory
        .0
        .join(OsString::from_vec(b"missing-\xff.xlsx".to_vec()));
    let output = command(&["list"])
        .arg(&missing)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let message = String::from_utf8(output.stderr).unwrap();
    assert!(
        message.starts_with(&format!(
            "sheetpatch: cannot read workbook '{}': ",
            missing.display()
        )),
        "{message}"
    );
    assert!(message.contains("missing-\u{fffd}.xlsx"), "{message}");
}

#[cfg(target_os = "linux")]
#[test]
fn full_standard_output_reports_a_write_error() {
    for args in [
        vec!["get", FIXTURE, "Data", "A1"],
        vec!["list", "--json", FIXTURE],
        vec!["compact", FIXTURE, "-"],
    ] {
        let output = command(&args)
            .stdout(
                fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/full")
                    .unwrap(),
            )
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let message = String::from_utf8(output.stderr).unwrap();
        assert!(
            message.starts_with("sheetpatch: cannot write output: "),
            "{message}"
        );
    }
}

#[cfg(unix)]
#[test]
fn saving_to_a_symlink_replaces_the_link_and_keeps_its_target() {
    let directory = TempDir::new();
    let target = directory.0.join("target.xlsx");
    let link = directory.0.join("link.xlsx");
    fs::write(&target, b"target bytes stay unchanged").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    for args in [
        vec![
            "set",
            FIXTURE,
            text(&link),
            "Data",
            "A1",
            "text",
            "Replaced",
        ],
        vec!["compact", FIXTURE, text(&link)],
    ] {
        assert!(success(cli(&args)).is_empty());
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_file());
        assert_eq!(fs::read(&target).unwrap(), b"target bytes stay unchanged");
        assert!(Workbook::open(&link).is_ok());
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
    }
    assert_eq!(directory.count(), 2);
}
