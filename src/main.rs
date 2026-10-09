#![forbid(unsafe_code)]

use std::{
    env,
    ffi::OsString,
    fs::File,
    io::{self, BufRead, BufReader, Write},
    process::ExitCode,
};

use sheetpatch::{CellContent, CellEdit, CellRef, CellValue, Workbook};

const USAGE: &str = "Usage:\n  sheetpatch list [--json] INPUT\n  sheetpatch get [--json] INPUT SHEET CELL [CELL ...]\n  sheetpatch set INPUT OUTPUT SHEET CELL text|number|bool|error VALUE\n  sheetpatch set INPUT OUTPUT SHEET CELL blank\n  sheetpatch patch INPUT OUTPUT PATCH.tsv\n  sheetpatch patch --check INPUT PATCH.tsv\n  sheetpatch compact INPUT OUTPUT\n  sheetpatch help [COMMAND]\n  sheetpatch --help\n  sheetpatch --version\n\nPatch rows: SHEET<TAB>CELL<TAB>TYPE<TAB>VALUE. Blank needs no value.\nUse - as PATCH.tsv to read stdin. Empty lines are ignored.\nLines starting with # without tabs are comments.\n--check validates a patch against the workbook without saving.\nQuote sheet names and text values containing spaces. Formats: .xlsx, .xlsm, .xltx, .xltm.\nget prints each stored value in address order; formula results are cached and are not recalculated.\n--json returns an array: list includes names and paths; get includes types, values, formulas and styles.";

fn command_help(command: &str) -> Option<&'static str> {
    match command {
        "help" => Some(USAGE),
        "list" => Some(
            "Usage: sheetpatch list [--json] INPUT\n\nList editable worksheets in workbook order. --json includes each worksheet's name and package path.",
        ),
        "get" => Some(
            "Usage: sheetpatch get [--json] INPUT SHEET CELL [CELL ...]\n\nRead A1 addresses in input order with one worksheet parse. Names are case-sensitive; addresses accept lowercase letters. Formula results are stored caches, without recalculation. --json includes normalized addresses, scalar types, values, formulas and style indices. Blank values use null in JSON and an empty line in plain output.",
        ),
        "set" => Some(
            "Usage:\n  sheetpatch set INPUT OUTPUT SHEET CELL text|number|bool|error VALUE\n  sheetpatch set INPUT OUTPUT SHEET CELL blank\n\nSet one scalar cell value and save atomically. Text stays text even when it starts with =. Numbers must be finite; booleans are true or false; error tokens include #N/A and #DIV/0!. Blank clears the value while retaining formatting. Formula cells and merged-range followers are protected. INPUT and OUTPUT may be the same path.",
        ),
        "patch" => Some(
            "Usage:\n  sheetpatch patch INPUT OUTPUT PATCH.tsv\n  sheetpatch patch --check INPUT PATCH.tsv\n\nApply a transaction of tab-separated SHEET, CELL, TYPE and VALUE fields. Types: text, number, bool, error, blank. Blank may omit VALUE. Duplicate cells use the last value. Text retains extra tabs and trailing whitespace. UTF-8 BOM and CRLF files are accepted; empty lines and tab-free lines starting with # are ignored. Use - as PATCH.tsv to read standard input. --check validates the complete transaction and output generation without saving; success reports the number of patch rows. Check and apply both use the same edit protections and ZIP limits.",
        ),
        "compact" => Some(
            "Usage: sheetpatch compact INPUT OUTPUT\n\nRemove safely recognized obsolete ZIP records and save atomically. Unknown archive prefixes or gaps are protected. Compaction retains untouched active records, does not calculate formulas, and does not securely erase copies or filesystem history. INPUT and OUTPUT may be the same path.",
        ),
        _ => None,
    }
}

#[derive(Debug)]
enum Failure {
    Usage(String),
    Workbook(sheetpatch::Error),
}

impl From<sheetpatch::Error> for Failure {
    fn from(error: sheetpatch::Error) -> Self {
        Self::Workbook(error)
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Workbook(error.into())
    }
}

fn parse_value(kind: &str, value: Option<&str>) -> Result<CellValue, String> {
    match (kind, value) {
        ("blank", None | Some("")) => Ok(CellValue::Blank),
        ("text", Some(value)) => Ok(CellValue::Text(value.to_owned())),
        ("error", Some(value)) => Ok(CellValue::Error(value.to_owned())),
        ("number", Some(value)) => value
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(CellValue::Number)
            .ok_or_else(|| format!("Invalid finite number: {value}")),
        ("bool", Some("true")) => Ok(CellValue::Bool(true)),
        ("bool", Some("false")) => Ok(CellValue::Bool(false)),
        ("bool", Some(_)) => Err("Boolean values must be true or false.".into()),
        _ => Err("Invalid value type or argument count.".into()),
    }
}

fn read_patch(mut reader: impl BufRead) -> Result<Vec<CellEdit>, Failure> {
    let mut edits = Vec::new();
    let mut line = String::new();
    let mut line_number = 0usize;
    loop {
        line.clear();
        line_number += 1;
        let invalid = |message| Failure::Usage(format!("Patch line {line_number}: {message}"));
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return Err(invalid(format!("expected UTF-8 text: {error}")));
            }
            Err(error) => return Err(error.into()),
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        let line = if line_number == 1 {
            line.strip_prefix('\u{feff}').unwrap_or(&line)
        } else {
            &line
        };
        if line.trim().is_empty() || (line.starts_with('#') && !line.contains('\t')) {
            continue;
        }
        let mut fields = line.splitn(4, '\t');
        let sheet = fields.next().unwrap();
        let cell = fields
            .next()
            .ok_or_else(|| invalid("expected SHEET<TAB>CELL<TAB>TYPE<TAB>VALUE".into()))?;
        let kind = fields
            .next()
            .ok_or_else(|| invalid("expected SHEET<TAB>CELL<TAB>TYPE<TAB>VALUE".into()))?;
        let value = parse_value(kind, fields.next()).map_err(invalid)?;
        let edit = CellEdit::new(sheet, cell, value).map_err(|error| invalid(error.to_string()))?;
        edits.push(edit);
    }
    Ok(edits)
}

fn text_arg(args: &[OsString], index: usize) -> Result<&str, Failure> {
    args[index]
        .to_str()
        .ok_or_else(|| Failure::Usage(format!("Argument {index} must be valid Unicode text.")))
}

fn patch_arg(path: &OsString) -> Result<Vec<CellEdit>, Failure> {
    if path == "-" {
        read_patch(io::stdin().lock())
    } else {
        read_patch(BufReader::new(File::open(path)?))
    }
}

fn write_json_string(output: &mut impl Write, value: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    let mut start = 0;
    for (index, ch) in value.char_indices() {
        let escape = match ch {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            '\u{08}' => "\\b",
            '\u{0c}' => "\\f",
            '\u{00}'..='\u{1f}' => "",
            _ => continue,
        };
        output.write_all(&value.as_bytes()[start..index])?;
        if escape.is_empty() {
            write!(output, "\\u{:04x}", ch as u32)?;
        } else {
            output.write_all(escape.as_bytes())?;
        }
        start = index + ch.len_utf8();
    }
    output.write_all(&value.as_bytes()[start..])?;
    output.write_all(b"\"")
}

fn write_scalar(output: &mut impl Write, value: &CellValue) -> io::Result<()> {
    match value {
        CellValue::Text(value) | CellValue::Error(value) => writeln!(output, "{value}"),
        CellValue::Number(value) => writeln!(output, "{value}"),
        CellValue::Bool(value) => writeln!(output, "{value}"),
        CellValue::Blank => writeln!(output),
    }
}

fn write_json_cell(
    output: &mut impl Write,
    sheet: &str,
    cell: CellRef,
    content: &CellContent,
) -> io::Result<()> {
    output.write_all(b"{\"sheet\":")?;
    write_json_string(output, sheet)?;
    write!(output, ",\"cell\":\"{cell}\"")?;
    let kind = match content.value {
        CellValue::Text(_) => "text",
        CellValue::Number(_) => "number",
        CellValue::Bool(_) => "bool",
        CellValue::Error(_) => "error",
        CellValue::Blank => "blank",
    };
    write!(output, ",\"type\":\"{kind}\",\"value\":")?;
    match &content.value {
        CellValue::Text(value) | CellValue::Error(value) => write_json_string(output, value)?,
        CellValue::Number(value) => write!(output, "{value}")?,
        CellValue::Bool(value) => write!(output, "{value}")?,
        CellValue::Blank => output.write_all(b"null")?,
    }
    output.write_all(b",\"formula\":")?;
    match &content.formula {
        Some(formula) => write_json_string(output, formula)?,
        None => output.write_all(b"null")?,
    }
    output.write_all(b",\"style_index\":")?;
    match content.style_index {
        Some(style) => write!(output, "{style}")?,
        None => output.write_all(b"null")?,
    }
    output.write_all(b"}")
}

fn run(args: &[OsString], mut output: impl Write) -> Result<(), Failure> {
    if args.len() == 3
        && matches!(args[2].to_str(), Some("--help" | "-h"))
        && let Some(help) = args[1].to_str().and_then(command_help)
    {
        writeln!(output, "{help}")?;
        return Ok(());
    }
    match args.get(1).and_then(|arg| arg.to_str()) {
        Some("--help" | "-h") if args.len() == 2 => {
            writeln!(output, "{USAGE}")?;
            Ok(())
        }
        Some("--version" | "-V") if args.len() == 2 => {
            writeln!(output, "sheetpatch {}", env!("CARGO_PKG_VERSION"))?;
            Ok(())
        }
        Some("help") if args.len() == 2 || args.len() == 3 => {
            let help = if args.len() == 2 {
                USAGE
            } else {
                command_help(text_arg(args, 2)?)
                    .ok_or_else(|| Failure::Usage("Unknown command for help.".into()))?
            };
            writeln!(output, "{help}")?;
            Ok(())
        }
        Some("list") => {
            let json = args.get(2).is_some_and(|arg| arg == "--json");
            let input_index = if json { 3 } else { 2 };
            if args.len() != input_index + 1 {
                return Err(Failure::Usage("list requires one input workbook.".into()));
            }
            let workbook = Workbook::open(&args[input_index])?;
            if json {
                output.write_all(b"[")?;
            }
            for (index, sheet) in workbook.sheets().iter().enumerate() {
                if json {
                    if index != 0 {
                        output.write_all(b",")?;
                    }
                    output.write_all(b"{\"name\":")?;
                    write_json_string(&mut output, sheet.name())?;
                    output.write_all(b",\"path\":")?;
                    write_json_string(&mut output, sheet.path())?;
                    output.write_all(b"}")?;
                } else {
                    writeln!(output, "{}", sheet.name())?;
                }
            }
            if json {
                output.write_all(b"]\n")?;
            }
            Ok(())
        }
        Some("get") => {
            let json = args.get(2).is_some_and(|arg| arg == "--json");
            let input_index = if json { 3 } else { 2 };
            if args.len() < input_index + 3 {
                return Err(Failure::Usage(
                    "get requires an input workbook, a sheet, and at least one cell.".into(),
                ));
            }
            let sheet = text_arg(args, input_index + 1)?;
            let cells = (input_index + 2..args.len())
                .map(|index| {
                    text_arg(args, index)?
                        .parse::<CellRef>()
                        .map_err(|error| Failure::Usage(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let workbook = Workbook::open(&args[input_index])?;
            let contents = workbook.get_cells_at(sheet, cells.iter().copied())?;
            if json {
                output.write_all(b"[")?;
            }
            for (index, (cell, content)) in cells.iter().zip(&contents).enumerate() {
                if json {
                    if index != 0 {
                        output.write_all(b",")?;
                    }
                    write_json_cell(&mut output, sheet, *cell, content)?;
                } else {
                    write_scalar(&mut output, &content.value)?;
                }
            }
            if json {
                output.write_all(b"]\n")?;
            }
            Ok(())
        }
        Some("set") if args.len() == 7 || args.len() == 8 => {
            let value = parse_value(
                text_arg(args, 6)?,
                args.get(7).map(|_| text_arg(args, 7)).transpose()?,
            )
            .map_err(Failure::Usage)?;
            let sheet = text_arg(args, 4)?;
            let cell = text_arg(args, 5)?;
            let edit = CellEdit::new(sheet, cell, value)
                .map_err(|error| Failure::Usage(error.to_string()))?;
            let mut workbook = Workbook::open(&args[2])?;
            workbook.apply_edits([edit])?;
            workbook.save(&args[3])?;
            Ok(())
        }
        Some("patch") if args.len() == 5 => {
            let check = args[2] == "--check";
            let edits = patch_arg(&args[4])?;
            let count = edits.len();
            let mut workbook = Workbook::open(&args[if check { 3 } else { 2 }])?;
            workbook.apply_edits(edits)?;
            if check {
                workbook.write_to(io::sink())?;
                let noun = if count == 1 { "row" } else { "rows" };
                writeln!(output, "Validated {count} patch {noun}.")?;
            } else {
                workbook.save(&args[3])?;
            }
            Ok(())
        }
        Some("compact") if args.len() == 4 => {
            Workbook::open(&args[2])?.save_compact(&args[3])?;
            Ok(())
        }
        _ => Err(Failure::Usage("Invalid command or argument count.".into())),
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    match run(&args, io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(message)) => {
            let _ = writeln!(io::stderr().lock(), "sheetpatch: {message}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Workbook(sheetpatch::Error::Io(error)))
            if error.kind() == io::ErrorKind::BrokenPipe =>
        {
            ExitCode::SUCCESS
        }
        Err(Failure::Workbook(error)) => {
            let _ = writeln!(io::stderr().lock(), "sheetpatch: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            (
                vec!["sheetpatch", "set", "", "", "Data", "A1", "text", "value"],
                4..8,
            ),
        ] {
            let args: Vec<_> = args.into_iter().map(OsString::from).collect();
            for index in text_indexes {
                let mut args = args.clone();
                args[index] = invalid.clone();
                match run(&args, Vec::new()) {
                    Err(Failure::Usage(message)) => {
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
        for flag in ["--help", "--version"] {
            let args = [OsString::from("sheetpatch"), OsString::from(flag)];
            match run(&args, FailedWriter) {
                Err(Failure::Workbook(sheetpatch::Error::Io(error))) => {
                    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                }
                other => panic!("unexpected output result: {other:?}"),
            }
        }
    }

    #[test]
    fn patch_preserves_text_tabs_whitespace_and_empty_values() {
        let source = "\u{feff}# comment\r\n\r\nData & Notes\tB2\ttext\t  left\tright  \r\n#Data\tB3\ttext\t\r\n";
        let edits = read_patch(source.as_bytes()).unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].sheet(), "Data & Notes");
        assert_eq!(edits[0].cell().to_string(), "B2");
        assert_eq!(edits[0].value(), &CellValue::Text("  left\tright  ".into()));
        assert_eq!(edits[1].sheet(), "#Data");
        assert_eq!(edits[1].value(), &CellValue::Text(String::new()));
    }

    #[test]
    fn patch_reads_every_scalar_type() {
        let source = "Data\tA1\tblank\nData\tA2\tblank\t\nData\tA3\tnumber\t1.25e2\nData\tA4\tbool\tfalse\nData\tA5\terror\t#N/A\n";
        let edits = read_patch(source.as_bytes()).unwrap();
        let values: Vec<_> = edits.iter().map(CellEdit::value).collect();
        assert_eq!(
            values,
            vec![
                &CellValue::Blank,
                &CellValue::Blank,
                &CellValue::Number(125.0),
                &CellValue::Bool(false),
                &CellValue::Error("#N/A".into()),
            ]
        );
    }

    #[test]
    fn patch_preserves_a_carriage_return_without_a_line_feed() {
        let edits = read_patch("Data\tA1\ttext\tvalue\r".as_bytes()).unwrap();
        assert_eq!(edits[0].value(), &CellValue::Text("value\r".into()));
    }

    #[test]
    fn non_utf8_patch_reports_the_failing_line() {
        let source = b"Data\tA1\tnumber\t1\nData\tA2\ttext\t\xff\n";
        match read_patch(source.as_slice()) {
            Err(Failure::Usage(message)) => {
                assert!(message.starts_with("Patch line 2: expected UTF-8 text:"));
            }
            other => panic!("unexpected UTF-8 validation result: {other:?}"),
        }
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
        ] {
            let source = format!("# comment\nData\tA1\tnumber\t42\n{row}\n");
            match read_patch(source.as_bytes()) {
                Err(Failure::Usage(message)) => assert!(message.starts_with("Patch line 3:")),
                other => panic!("unexpected result for {row:?}: {other:?}"),
            }
        }
    }
}
