use super::args::{ValueError, parse_cell, parse_value};
use super::error::write_failure;
use super::output::{Escaped, write_cell, write_json_string, write_read_cells};
use super::patch::{EditLines, read_patch};
use super::streams::quoted_path;
use super::*;
use sheetpatch::{CellRef, CellValue, Workbook};
use std::ffi::OsStr;
use std::fmt::Display;
use std::{fs, iter};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/libreoffice.xlsx"
);
const MAIN_NAMESPACE: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const OFFICE_NAMESPACE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE_NAMESPACE: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

fn args(values: &[&str]) -> Vec<OsString> {
    iter::once("sheetpatch")
        .chain(values.iter().copied())
        .map(OsString::from)
        .collect()
}

/// A workbook of empty worksheets with the given XML-escaped names.
fn named_sheets(names: &[&str]) -> Vec<u8> {
    let sheets: String = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            format!(
                "<sheet name='{name}' sheetId='{}' r:id='s{index}'/>",
                index + 1
            )
        })
        .collect();
    let relationships: String = (0..names.len())
        .map(|index| {
            format!(
                "<Relationship Id='s{index}' Type='{OFFICE_NAMESPACE}/worksheet' Target='worksheets/sheet{index}.xml'/>"
            )
        })
        .collect();
    let workbook = format!(
        "<workbook xmlns='{MAIN_NAMESPACE}' xmlns:r='{OFFICE_NAMESPACE}'><sheets>{sheets}</sheets></workbook>"
    );
    let root = format!(
        "<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='office' Type='{OFFICE_NAMESPACE}/officeDocument' Target='xl/workbook.xml'/></Relationships>"
    );
    let workbook_relationships =
        format!("<Relationships xmlns='{PACKAGE_NAMESPACE}'>{relationships}</Relationships>");
    let worksheet = format!("<worksheet xmlns='{MAIN_NAMESPACE}'><sheetData/></worksheet>");
    let paths: Vec<String> = (0..names.len())
        .map(|index| format!("xl/worksheets/sheet{index}.xml"))
        .collect();
    let mut entries: Vec<(&str, &[u8])> = vec![
        (
            "[Content_Types].xml",
            b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>",
        ),
        ("_rels/.rels", root.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", workbook_relationships.as_bytes()),
    ];
    entries.extend(
        paths
            .iter()
            .map(|path| (path.as_str(), worksheet.as_bytes())),
    );
    common::stored_archive(&entries)
}

fn patch_values(source: &str) -> Vec<CellValue> {
    read_patch(source.as_bytes(), "test")
        .unwrap()
        .edits
        .into_iter()
        .map(|edit| edit.value().clone())
        .collect()
}

fn patch_error(source: &[u8]) -> String {
    match read_patch(source, "test") {
        Err(Failure::Invalid { command, message }) => {
            assert_eq!(
                command, "patch",
                "a rejected row should point to the patch format's help"
            );
            message
        }
        other => panic!("unexpected patch result: {other:?}"),
    }
}

#[cfg(any(unix, windows))]
#[test]
fn non_unicode_text_arguments_are_rejected_before_opening_workbooks() {
    #[cfg(unix)]
    let invalid = {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xff])
    };
    #[cfg(windows)]
    let invalid = {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xd800])
    };
    for (args, text_indexes) in [
        (vec!["sheetpatch", "get", "", "Data", "A1"], 3..5),
        (
            vec!["sheetpatch", "get", "--json", "", "Data", "A1", "B2"],
            4..7,
        ),
        (vec!["sheetpatch", "get", "", "Data", "A1:B2", "C3"], 3..6),
        (
            vec!["sheetpatch", "set", "", "", "Data", "A1", "text", "value"],
            4..8,
        ),
        (vec!["sheetpatch", "cells", "", "Data", "A1:B2"], 3..5),
        (
            vec!["sheetpatch", "cells", "--json", "", "Data", "A1:B2"],
            4..6,
        ),
    ] {
        let args: Vec<_> = args.into_iter().map(OsString::from).collect();
        for index in text_indexes {
            let mut args = args.clone();
            args[index] = invalid.clone();
            match run(&args, &mut Vec::new(), /* terminal: */ false) {
                Err(Failure::Invalid { message, .. }) => {
                    assert!(message.starts_with(&format!("Argument {index} ")));
                }
                other => panic!("unexpected argument validation result: {other:?}"),
            }
        }
    }
}

#[test]
fn json_strings_escape_all_controls_and_keep_unicode() {
    let input = format!("Café 😀\"\\{}", (0..32).map(char::from).collect::<String>());
    let mut output = Vec::new();
    write_json_string(&mut output, &input).unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        concat!(
            "\"Café 😀\\\"\\\\",
            "\\u0000\\u0001\\u0002\\u0003\\u0004\\u0005\\u0006\\u0007",
            "\\b\\t\\n\\u000b\\f\\r\\u000e\\u000f",
            "\\u0010\\u0011\\u0012\\u0013\\u0014\\u0015\\u0016\\u0017",
            "\\u0018\\u0019\\u001a\\u001b\\u001c\\u001d\\u001e\\u001f\""
        )
    );
    // DEL and C1 controls are escaped too, so raw JSON on a terminal
    // cannot carry them; the characters around them stay literal.
    let input: String = ('\u{7e}'..='\u{a1}').collect();
    let mut output = Vec::new();
    write_json_string(&mut output, &input).unwrap();
    let escapes: String = (0x7f..=0x9f).map(|code| format!("\\u{code:04x}")).collect();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        format!("\"~{escapes}\u{a0}\u{a1}\"")
    );
}

#[test]
fn terminal_output_escapes_controls_and_other_output_is_exact() {
    let escaped = |value: &dyn Display, escape| Escaped { value, escape }.to_string();
    let text = "a\u{1b}]0;title\u{7}\u{1b}[2J\tb\nc\r\0\u{7f}\u{85}\u{9b}\u{a0}é😀";
    assert_eq!(escaped(&text, Escape::values(/* terminal: */ false)), text);
    assert_eq!(escaped(&text, Escape::names(/* terminal: */ false)), text);
    assert_eq!(
        escaped(&text, Escape::values(/* terminal: */ true)),
        "a\\u{1b}]0;title\\u{7}\\u{1b}[2J\tb\nc\\u{d}\\u{0}\\u{7f}\\u{85}\\u{9b}\u{a0}é😀"
    );
    // Names are listed one per line, so tabs and line feeds are escaped too.
    assert_eq!(
        escaped(&text, Escape::names(/* terminal: */ true)),
        "a\\u{1b}]0;title\\u{7}\\u{1b}[2J\\u{9}b\\u{a}c\\u{d}\\u{0}\\u{7f}\\u{85}\\u{9b}\u{a0}é😀"
    );
    // Values use their own display; only text and error tokens hold controls.
    for (value, expected) in [
        (CellValue::Number(-0.5), "-0.5"),
        (CellValue::Number(1e21), "1000000000000000000000"),
        (CellValue::Bool(true), "true"),
        (CellValue::Error("#N/A".into()), "#N/A"),
        (CellValue::Blank, ""),
        (CellValue::Text("x\ty\u{1b}".into()), "x\ty\\u{1b}"),
    ] {
        assert_eq!(
            escaped(&value, Escape::values(/* terminal: */ true)),
            expected
        );
        assert_eq!(
            escaped(&value, Escape::values(/* terminal: */ false)),
            value.to_string()
        );
    }
}

#[test]
fn terminal_diagnostics_escape_paths_and_workbook_metadata() {
    let directory = common::TempDir::new();
    let path = directory.join("invalid.xlsx");
    // The sheet name decodes to an ANSI clear-screen sequence. Omitting
    // its relationship id puts that name into a package-discovery error.
    let mut parts = common::payloads(&named_sheets(&["_x001B_[2Jspoofed"]));
    let workbook = parts.get_mut("xl/workbook.xml").unwrap();
    let xml = String::from_utf8(workbook.clone()).unwrap();
    *workbook = xml.replace("r:id='s0'", "").into_bytes();
    fs::write(
        &path,
        common::stored_archive(
            &parts
                .iter()
                .map(|(name, data)| (name.as_str(), data.as_slice()))
                .collect::<Vec<_>>(),
        ),
    )
    .unwrap();
    let mut output = Vec::new();
    let failure = run(
        &args(&["list", path.to_str().unwrap()]),
        &mut output,
        /* terminal: */ false,
    )
    .unwrap_err();
    assert!(output.is_empty());
    assert!(failure.to_string().contains("\u{1b}[2Jspoofed"));
    for failure in [
        failure,
        Failure::Context(
            format!(
                "cannot read workbook {}",
                quoted_path(OsStr::new("missing-\u{1b}]0;title\u{7}\r\u{9b}.xlsx"))
            ),
            io::Error::from(io::ErrorKind::NotFound).into(),
        ),
    ] {
        let mut plain = Vec::new();
        write_failure(&mut plain, &failure, /* terminal: */ false).unwrap();
        assert_eq!(
            String::from_utf8(plain).unwrap(),
            format!("sheetpatch: {failure}\n")
        );
        let mut terminal = Vec::new();
        write_failure(&mut terminal, &failure, /* terminal: */ true).unwrap();
        let terminal = String::from_utf8(terminal).unwrap();
        assert!(terminal.contains("\\u{1b}"), "{terminal:?}");
        assert!(
            terminal
                .chars()
                .all(|character| !character.is_control() || matches!(character, '\t' | '\n')),
            "{terminal:?}"
        );
    }
    let failure = Failure::Usage("expected INPUT\tOUTPUT".into());
    let mut terminal = Vec::new();
    write_failure(&mut terminal, &failure, /* terminal: */ true).unwrap();
    assert_eq!(
        String::from_utf8(terminal).unwrap(),
        format!("sheetpatch: {failure}\n")
    );
}

#[test]
fn terminal_reads_escape_stored_controls_but_json_stays_exact() {
    let directory = common::TempDir::new();
    let path = directory.join("controls.xlsx");
    let mut workbook = Workbook::open(FIXTURE).unwrap();
    workbook
        .set_cell("Notes", "A2", "x\r\u{9b}2J\u{7f}\ty")
        .unwrap();
    workbook.save(&path).unwrap();
    let path = path.to_str().unwrap();
    for (terminal, expected) in [
        (false, "x\r\u{9b}2J\u{7f}\ty\n"),
        (true, "x\\u{d}\\u{9b}2J\\u{7f}\ty\n"),
    ] {
        let mut output = Vec::new();
        run(&args(&["get", path, "Notes", "A2"]), &mut output, terminal).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), expected);
        let mut output = Vec::new();
        run(
            &args(&["cells", path, "Notes", "A2"]),
            &mut output,
            terminal,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("A2\ttext\t{expected}")
        );
    }
    for command in ["get", "cells"] {
        let mut json = Vec::new();
        run(
            &args(&[command, "--json", path, "Notes", "A2"]),
            &mut json,
            /* terminal: */ true,
        )
        .unwrap();
        assert!(
            String::from_utf8(json)
                .unwrap()
                .contains("\"x\\r\\u009b2J\\u007f\\ty\""),
            "{command}"
        );
    }
}

#[test]
fn read_output_does_not_depend_on_the_buffer_budget() {
    // Every scalar type, formulas with and without a cached value, a
    // formatting-only cell, a missing cell, controls, and text long enough
    // that the budget can end the buffered prefix before or after a cell.
    let long = "long ".repeat(12);
    let sheet = format!(
        "<worksheet xmlns='{MAIN_NAMESPACE}'><sheetData>\
         <row r='1'><c r='A1' t='s'><v>1</v></c><c r='B1'><v>12.5</v></c>\
         <c r='C1' s='1'/><c r='D1'><f>B1*2</f><v>25</v></c></row>\
         <row r='2'><c r='A2' t='inlineStr'><is><t>x&#13;&#x9b;2J&#x7f;&#9;y&#10;z</t></is></c>\
         <c r='B2' t='b'><v>1</v></c><c r='C2' t='e'><v>#N/A</v></c><c r='D2'><f>{long}</f></c></row>\
         <row r='3'><c r='A3' t='inlineStr'><is><t>{long}</t></is></c>\
         <c r='B3'><v>not-a-number</v></c></row></sheetData></worksheet>"
    );
    let mut parts = common::payloads(&fs::read(FIXTURE).unwrap());
    parts.insert("xl/worksheets/sheet1.xml".into(), sheet.into_bytes());
    let entries: Vec<_> = parts
        .iter()
        .map(|(name, data)| (name.as_str(), data.as_slice()))
        .collect();
    let workbook = Workbook::from_bytes(common::stored_archive(&entries)).unwrap();
    let view = workbook.prepare_sheet("Data").unwrap();
    let cell = |address: &str| address.parse::<CellRef>().unwrap();
    let mut cells: Vec<CellRef> = "A1:D3"
        .parse::<CellRange>()
        .unwrap()
        .cells()
        .filter(|&address| address != cell("B3"))
        .collect();
    cells.extend(["A3", "C1", "E9", "A2"].map(cell));
    for json in [false, true] {
        for records in [false, true] {
            for escape in [Escape::Exact, Escape::Controls] {
                let format = || CellOutput {
                    sheet: "Data",
                    json,
                    records,
                    escape,
                };
                // Formatting each cell in turn gives the unbuffered output.
                let mut expected = if json { b"[".to_vec() } else { Vec::new() };
                let mut first = true;
                for &cell in &cells {
                    let content = view.get_cell_at(cell).unwrap();
                    write_cell(&mut expected, cell, &content, &format(), &mut first).unwrap();
                }
                if json {
                    expected.extend_from_slice(b"]\n");
                }
                for budget in (0..=expected.len() + 1).chain([usize::MAX]) {
                    let mut output = Vec::new();
                    write_read_cells(&view, &cells, budget, &mut output, format()).unwrap();
                    assert_eq!(output, expected, "budget {budget}");
                }
                // A malformed value after any buffered prefix still
                // prevents all output.
                let mut invalid = cells.clone();
                invalid.push(cell("B3"));
                for budget in [0, expected.len() / 2, usize::MAX] {
                    let mut output = Vec::new();
                    let failure = write_read_cells(&view, &invalid, budget, &mut output, format())
                        .unwrap_err();
                    assert!(failure.to_string().contains("B3"), "{failure}");
                    assert!(output.is_empty(), "budget {budget}");
                }
            }
        }
    }
}

#[test]
fn buffered_read_output_stops_before_stored_text_over_the_budget() {
    // Records the length of each write. The buffered prefix is written
    // with one write_all, so it is the first write.
    struct Writes(Vec<usize>);
    impl Write for Writes {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.push(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let long = "x".repeat(1000);
    let sheet = format!(
        "<worksheet xmlns='{MAIN_NAMESPACE}'><sheetData><row r='1'><c r='A1'><v>1</v></c>\
         <c r='B1' t='inlineStr'><is><t>{long}</t></is></c>\
         <c r='C1'><f>{long}</f><v>2</v></c></row></sheetData></worksheet>"
    );
    let mut parts = common::payloads(&fs::read(FIXTURE).unwrap());
    parts.insert("xl/worksheets/sheet1.xml".into(), sheet.into_bytes());
    let entries: Vec<_> = parts
        .iter()
        .map(|(name, data)| (name.as_str(), data.as_slice()))
        .collect();
    let workbook = Workbook::from_bytes(common::stored_archive(&entries)).unwrap();
    let view = workbook.prepare_sheet("Data").unwrap();
    let cell = |address: &str| address.parse::<CellRef>().unwrap();
    // Long text in either format, and a long formula, which only JSON
    // output repeats. Plain output of C1 fits.
    for (cells, json, buffered) in [
        ([cell("A1"), cell("B1")], false, false),
        ([cell("A1"), cell("B1")], true, false),
        ([cell("A1"), cell("C1")], true, false),
        ([cell("A1"), cell("C1")], false, true),
    ] {
        for records in [false, true] {
            let format = CellOutput {
                sheet: "Data",
                json,
                records,
                escape: Escape::Exact,
            };
            let mut output = Writes(Vec::new());
            write_read_cells(&view, &cells, 200, &mut output, format).unwrap();
            let prefix = output.0[0];
            assert!(
                prefix <= 200,
                "{cells:?} json={json}: prefix of {prefix} bytes"
            );
            // Output that fits is the prefix, and any closing bracket.
            assert_eq!(output.0.len() == 1 + usize::from(json), buffered);
        }
    }
}

#[test]
fn indexed_output_replays_only_values_after_its_buffered_prefix() {
    use std::cell::Cell;

    let workbook = Workbook::open(FIXTURE).unwrap();
    let view = workbook.prepare_sheet("Data").unwrap();
    let visits: Vec<_> = view.cells().map(|_| Cell::new(0)).collect();
    let mut output = Vec::new();
    output::write_cells(
        view.cell_contents().enumerate().map(|(index, content)| {
            visits[index].set(visits[index].get() + 1);
            content
        }),
        20,
        &mut output,
        CellOutput {
            sheet: "Data",
            json: false,
            records: false,
            escape: Escape::Exact,
        },
    )
    .unwrap();
    assert_eq!(
        output,
        b"Original text\n12.5\n1\n25\nCaf\xc3\xa9 \xe2\x98\x95\n"
    );
    assert_eq!(
        visits.iter().map(Cell::get).collect::<Vec<_>>(),
        [1, 1, 1, 2, 2]
    );
}

#[test]
fn terminal_lists_escape_every_control_in_sheet_names() {
    let directory = common::TempDir::new();
    let path = directory.join("names.xlsx");
    fs::write(
        &path,
        named_sheets(&[
            "Two&#10;Lines",
            "Tab&#9;Name",
            "CR&#13;",
            "CSI&#x9b;2J",
            "Plain",
        ]),
    )
    .unwrap();
    let path = path.to_str().unwrap();
    for (terminal, expected) in [
        (false, "Two\nLines\nTab\tName\nCR\r\nCSI\u{9b}2J\nPlain\n"),
        (
            true,
            "Two\\u{a}Lines\nTab\\u{9}Name\nCR\\u{d}\nCSI\\u{9b}2J\nPlain\n",
        ),
    ] {
        let mut output = Vec::new();
        run(&args(&["list", path]), &mut output, terminal).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }
    let mut json = Vec::new();
    run(
        &args(&["list", "--json", path]),
        &mut json,
        /* terminal: */ true,
    )
    .unwrap();
    let json = String::from_utf8(json).unwrap();
    assert!(
        json.starts_with("[{\"name\":\"Two\\nLines\",\"path\":"),
        "{json}"
    );
    assert!(json.contains("{\"name\":\"Tab\\tName\","), "{json}");
}

#[test]
fn workbooks_are_not_written_to_a_terminal() {
    for command in [
        vec!["compact", FIXTURE, "-"],
        vec!["set", FIXTURE, "-", "Data", "A1", "text", "x"],
    ] {
        let mut output = Vec::new();
        match run(&args(&command), &mut output, /* terminal: */ true) {
            Err(Failure::Invalid { message, .. }) => {
                assert!(message.contains("terminal"), "{message}");
            }
            other => panic!("unexpected terminal result: {other:?}"),
        }
        assert!(output.is_empty());
    }
}

#[test]
fn output_failures_are_returned_without_panicking() {
    struct FailedWriter;
    impl Write for FailedWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed consumer"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    for command in [
        vec!["--help"],
        vec!["help", "cells"],
        vec!["--version"],
        vec!["get", FIXTURE, "Data", "A1"],
        vec!["get", FIXTURE, "Data", "A1:C3"],
        vec!["cells", FIXTURE, "Data"],
        vec!["cells", "--json", FIXTURE, "Data", "B1:B9"],
        vec!["list", FIXTURE],
        vec!["list", "--json", FIXTURE],
    ] {
        // `main` ends text output quietly when its reader closed the pipe.
        match run(
            &args(&command),
            &mut FailedWriter,
            /* terminal: */ false,
        ) {
            Err(Failure::Output(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
            }
            other => panic!("unexpected output result for {command:?}: {other:?}"),
        }
    }
    let directory = common::TempDir::new();
    let patch = directory.join("p.tsv");
    fs::write(&patch, "Data\tA1\ttext\tx\n").unwrap();
    for command in [
        vec!["compact", FIXTURE, "-"],
        vec!["set", FIXTURE, "-", "Data", "A1", "text", "x"],
        vec!["patch", FIXTURE, "-", patch.to_str().unwrap()],
    ] {
        // A truncated workbook is a failure, even behind a closed pipe.
        match run(
            &args(&command),
            &mut FailedWriter,
            /* terminal: */ false,
        ) {
            Err(failure @ Failure::Context(_, sheetpatch::Error::Io(_))) => {
                assert_eq!(failure.exit_code(), 1);
                assert_eq!(
                    failure.to_string(),
                    "cannot write workbook to standard output: closed consumer"
                );
            }
            other => panic!("unexpected output result for {command:?}: {other:?}"),
        }
    }
}

#[test]
fn refused_edits_name_the_last_patch_line_for_their_protected_cell() {
    // D1 of the fixture's Data worksheet holds a formula.
    let mut workbook =
        Workbook::from_bytes(include_bytes!("../../tests/fixtures/libreoffice.xlsx").to_vec())
            .unwrap();
    let protected = workbook.set_cell("Data", "D1", 1).unwrap_err();
    assert!(
        matches!(
            &protected,
            sheetpatch::Error::ProtectedCell { sheet, cell, .. }
                if sheet == "Data" && cell.to_string() == "D1"
        ),
        "{protected:?}"
    );
    let locations = |patch: &str| read_patch(patch.as_bytes(), "test").unwrap().lines;
    let line = |patch: &str| locations(patch).line_for(&protected);
    // The last line for the worksheet and cell supplied the applied value.
    assert_eq!(
        line(
            "Data\tD1\ttext\tfirst\nData\tD10\tnumber\t1\nData\td1\ttext\tlast\nOther\tD1\tblank\n"
        ),
        Some(3)
    );
    // D10 and the same cell on another worksheet are other targets.
    assert_eq!(line("Other\tD1\tblank\nData\tD10\tnumber\t1\n"), None);
    // Only protected-cell refusals have a line, even when another error's
    // message names a cell that a line targets.
    let rows = locations("Data\tA1\ttext\tx\n");
    for error in [
        sheetpatch::Error::Unsupported("worksheet \"Data\": A1 contains a formula".into()),
        sheetpatch::Error::Xml("worksheet \"Data\": A1: cell has multiple value payloads".into()),
        sheetpatch::Error::InvalidWorkbook("worksheet \"Data\": A1: invalid".into()),
        sheetpatch::Error::InvalidValue("worksheet \"Data\": A1 is invalid".into()),
    ] {
        assert_eq!(rows.line_for(&error), None, "{error}");
    }
    // A missing worksheet is reported at the first line naming it, even
    // when a later line repeats its first target.
    let rows = locations(
        "Data\tD1\tnumber\t1\nZ\tA1\ttext\tx\nA\tB2\tblank\nZ\ta1\tblank\nZ\tC3\tblank\n",
    );
    let missing = |name: &str| rows.line_for(&sheetpatch::Error::SheetNotFound(name.into()));
    assert_eq!(
        (missing("Z"), missing("A"), missing("Other")),
        (Some(2), Some(3), None)
    );
}

#[test]
fn patch_preserves_text_tabs_whitespace_and_empty_values() {
    let source =
        "\u{feff}# comment\r\n\r\nData & Notes\tB2\ttext\t  left\tright  \r\n#Data\tB3\ttext\t\r\n";
    let patch = read_patch(source.as_bytes(), "test").unwrap();
    assert_eq!(patch.row_count, 2);
    assert_eq!(patch.edits.len(), 2);
    let edit = &patch.edits[0];
    assert_eq!(patch.lines.lines[0], EditLines { first: 3, last: 3 });
    assert_eq!(edit.sheet(), "Data & Notes");
    assert_eq!(edit.cell().to_string(), "B2");
    assert_eq!(edit.value(), &CellValue::Text("  left\tright  ".into()));
    let edit = &patch.edits[1];
    assert_eq!(patch.lines.lines[1], EditLines { first: 4, last: 4 });
    assert_eq!(edit.sheet(), "#Data");
    assert_eq!(edit.value(), &CellValue::Text(String::new()));
}

#[test]
fn patch_reads_every_scalar_type() {
    let source = "Data\tA1\tblank\nData\tA2\tblank\t\nData\tA3\tnumber\t1.25e2\nData\tA4\tbool\tfalse\nData\tA5\terror\t#N/A\n";
    assert_eq!(
        patch_values(source),
        vec![
            CellValue::Blank,
            CellValue::Blank,
            CellValue::Number(125.0),
            CellValue::Bool(false),
            CellValue::Error("#N/A".into()),
        ]
    );
}

#[test]
fn patch_preserves_a_carriage_return_without_a_line_feed() {
    let edits = read_patch("Data\tA1\ttext\tvalue\r".as_bytes(), "test").unwrap();
    assert_eq!(edits.edits[0].value(), &CellValue::Text("value\r".into()));
    assert_eq!(
        patch_values("Data\tA1\ttext\tfirst\r\nData\tA2\ttext\tlast\r"),
        vec![
            CellValue::Text("first".into()),
            CellValue::Text("last\r".into())
        ]
    );
}

#[test]
fn patch_rejects_a_carriage_return_followed_by_more_data() {
    let expected = "carriage return without line feed; use LF or CRLF line endings";
    for (source, line) in [
        (
            "Data\tA2\ttext\tAlpha\rData\tB2\tnumber\t42\rData\tC2\ttext\tGamma\r".as_bytes(),
            1,
        ),
        (b"Data\tA1\ttext\tok\nData\tA2\ttext\ta\rb\n", 2),
        (b"# one\r# two\r", 1),
        (b"Data\tA1\ttext\tvalue\r\r", 1),
        // A line feed ends the line, so a carriage return left after
        // removing LF or CRLF is not the final byte of the input.
        (b"Data\tA1\ttext\tvalue\r\r\n", 1),
        (
            b"Data\tA1\ttext\tok\r\nData\tA2\ttext\tvalue\r\r\nData\tA3\tblank\n",
            2,
        ),
        (b"Data\tA1\ttext\tok\n# note\r\r\n", 2),
    ] {
        assert_eq!(
            patch_error(source),
            format!("Patch line {line}: {expected}")
        );
    }
}

#[test]
fn non_utf8_patch_reports_the_failing_line() {
    let source = b"Data\tA1\tnumber\t1\nData\tA2\ttext\t\xff\n";
    assert!(patch_error(source).starts_with("Patch line 2: expected UTF-8 text:"));
}

#[test]
fn malformed_patch_reports_the_line_before_any_edits_are_returned() {
    for row in [
        "Data",
        "Data\tA2",
        "Data\tA2\ttext",
        "Data\tA2\tblank\tunexpected",
        "Data\tA2\tbool\t1",
        "Data\tA2\tnumber\tNaN",
        "Data\tA0\ttext\tvalue",
        "Data\tA2\terror\tinvalid",
        "Data\tA2\tText\tvalue",
    ] {
        let source = format!("# comment\nData\tA1\tnumber\t42\n{row}\n");
        let message = patch_error(source.as_bytes());
        assert!(message.starts_with("Patch line 3:"), "{row:?}: {message}");
    }
}

#[test]
fn value_errors_quote_the_rejected_input() {
    let message = |kind, value| match parse_value(kind, value) {
        Err(ValueError::Arity(message) | ValueError::Invalid(message)) => message,
        Ok(value) => panic!("unexpected value {value:?}"),
    };
    assert_eq!(
        message("number", Some("42 ")),
        "Invalid finite number: \"42 \""
    );
    assert_eq!(
        message("Text", Some("x")),
        "unknown value type \"Text\"; expected text, number, bool, error or blank"
    );
    assert_eq!(
        message("bool", Some("TRUE")),
        "Boolean values must be true or false, not \"TRUE\""
    );
    assert_eq!(message("text", None), "text needs a VALUE");
    assert_eq!(
        message("blank", Some("x")),
        "blank takes no VALUE, but \"x\" was given"
    );
    assert_eq!(
        parse_cell("A1\u{feff}").unwrap_err(),
        "invalid cell reference \"A1\\u{feff}\""
    );
}
