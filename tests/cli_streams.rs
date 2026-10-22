//! Standard stream workbooks, output destinations, and I/O error context.
mod common;

use common::TempDir;
use sheetpatch::{CellValue, Workbook};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

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

/// The number of files in `directory`.
fn count(directory: &TempDir) -> usize {
    fs::read_dir(directory.path()).unwrap().count()
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
    assert_eq!(
        success(piped(&["get", "-", "Data", "C1:D1"], &workbook)),
        b"1\n25\n"
    );
    assert_eq!(
        success(piped(&["cells", "-", "Notes"], &workbook)),
        b"A1\ttext\tKeep this sheet\n"
    );
    assert_eq!(
        success(piped(&["cells", "--json", "-", "Data", "B1:C9"], &workbook)),
        success(cli(&["cells", "--json", FIXTURE, "Data", "B1:C9"]))
    );

    let directory = TempDir::new();
    let output = directory.join("out.xlsx");
    let patch = directory.join("p.tsv");
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
    let saved = directory.join("saved.xlsx");
    let patch = directory.join("p.tsv");
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
    assert_eq!(count(&directory), 2);

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
    assert_eq!(count(&directory), 2);
}

#[test]
fn input_and_patch_cannot_both_read_standard_input() {
    let directory = TempDir::new();
    let destination = directory.join("out.xlsx");
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
    assert_eq!(count(&directory), 0);
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
    let input = directory.join("in.xlsx");
    let patch = directory.join("p.tsv");
    let missing = directory.join("missing");
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
            vec!["list", text(directory.path())],
            format!("cannot read workbook '{}': ", directory.path().display()),
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
    assert_eq!(count(&directory), 2);
}

// Native byte paths are exercised on Linux; macOS rejects invalid UTF-8 names.
#[cfg(target_os = "linux")]
#[test]
fn io_errors_display_non_unicode_paths_lossily() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let directory = TempDir::new();
    let missing = directory.join(OsString::from_vec(b"missing-\xff.xlsx".to_vec()));
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
    let text_error = "sheetpatch: cannot write output: ";
    let workbook_error = "sheetpatch: cannot write workbook to standard output: ";
    for (args, prefix) in [
        (vec!["get", FIXTURE, "Data", "A1"], text_error),
        (vec!["list", "--json", FIXTURE], text_error),
        (vec!["cells", FIXTURE, "Data"], text_error),
        (vec!["compact", FIXTURE, "-"], workbook_error),
        (
            vec!["set", FIXTURE, "-", "Data", "A1", "text", "x"],
            workbook_error,
        ),
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
        assert!(message.starts_with(prefix), "{message}");
        assert!(!message.contains("I/O error"), "{message}");
    }
}

/// A workbook whose 1 MiB of untouched package data outgrows any default
/// pipe buffer.
fn large_workbook() -> Vec<u8> {
    const MAIN_NAMESPACE: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const OFFICE_NAMESPACE: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PACKAGE_NAMESPACE: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    let root = format!(
        "<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='office' Type='{OFFICE_NAMESPACE}/officeDocument' Target='xl/workbook.xml'/></Relationships>"
    );
    let workbook = format!(
        "<workbook xmlns='{MAIN_NAMESPACE}' xmlns:r='{OFFICE_NAMESPACE}'><sheets><sheet name='Data' sheetId='1' r:id='one'/></sheets></workbook>"
    );
    let rels = format!(
        "<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='one' Type='{OFFICE_NAMESPACE}/worksheet' Target='worksheets/sheet1.xml'/></Relationships>"
    );
    let worksheet = format!("<worksheet xmlns='{MAIN_NAMESPACE}'><sheetData/></worksheet>");
    let padding = vec![b'x'; 1 << 20];
    common::stored_archive(&[
        (
            "[Content_Types].xml",
            b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>",
        ),
        ("_rels/.rels", root.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
        ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
        ("xl/media/padding.bin", &padding),
    ])
}

#[test]
fn a_reader_that_closes_the_pipe_early_fails_a_workbook_write() {
    // A truncated archive is unusable, so unlike text output, a closed pipe
    // is reported and the exit status is 1.
    let directory = TempDir::new();
    let input = directory.join("large.xlsx");
    let patch = directory.join("p.tsv");
    fs::write(&input, large_workbook()).unwrap();
    fs::write(&patch, "Data\tA1\tnumber\t1\n").unwrap();
    for args in [
        vec!["compact", text(&input), "-"],
        vec!["set", text(&input), "-", "Data", "A1", "bool", "true"],
        vec!["patch", text(&input), "-", text(&patch)],
    ] {
        let mut child = command(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdout.take());
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let message = String::from_utf8(output.stderr).unwrap();
        assert!(
            message.starts_with("sheetpatch: cannot write workbook to standard output: "),
            "{message}"
        );
        assert_eq!(message.lines().count(), 1, "{message}");
    }
    // The same workbook streams completely to a reader that keeps the pipe open.
    let streamed = success(cli(&[
        "set",
        text(&input),
        "-",
        "Data",
        "A1",
        "bool",
        "true",
    ]));
    assert!(streamed.len() > 1 << 20);
    assert_eq!(cell(streamed, "Data", "A1"), CellValue::Bool(true));
    assert_eq!(count(&directory), 2);
}

#[cfg(unix)]
#[test]
fn saving_to_a_symlink_replaces_the_link_and_keeps_its_target() {
    let directory = TempDir::new();
    let target = directory.join("target.xlsx");
    let link = directory.join("link.xlsx");
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
    assert_eq!(count(&directory), 2);
}
