#![forbid(unsafe_code)]

use std::{
    env,
    fs::File,
    io::{self, BufRead, BufReader},
    process::ExitCode,
};

use sheetpatch::{CellEdit, CellValue, Workbook};

const USAGE: &str = "Usage:\n  sheetpatch list INPUT\n  sheetpatch get INPUT SHEET CELL\n  sheetpatch set INPUT OUTPUT SHEET CELL text|number|bool|error VALUE\n  sheetpatch set INPUT OUTPUT SHEET CELL blank\n  sheetpatch patch INPUT OUTPUT PATCH.tsv\n  sheetpatch compact INPUT OUTPUT\n\nPatch rows: SHEET<TAB>CELL<TAB>TYPE<TAB>VALUE. Blank needs no value.\nUse - as PATCH.tsv to read stdin. Empty lines are ignored.\nLines starting with # without tabs are comments.\nQuote sheet names and text values containing spaces. Formats: .xlsx, .xlsm, .xltx, .xltm.\nget prints the stored value; formula results are cached and are not recalculated.";

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

fn read_patch(reader: impl BufRead) -> Result<Vec<CellEdit>, Failure> {
    let mut edits = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let line = if index == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(&line)
        } else {
            &line
        };
        if line.trim().is_empty() || (line.starts_with('#') && !line.contains('\t')) {
            continue;
        }
        let invalid = |message| Failure::Usage(format!("Patch line {}: {message}", index + 1));
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

fn run(args: &[String]) -> Result<(), Failure> {
    match args.get(1).map(String::as_str) {
        Some("list") if args.len() == 3 => {
            let workbook = Workbook::open(&args[2])?;
            for sheet in workbook.sheets() {
                println!("{}", sheet.name());
            }
            Ok(())
        }
        Some("get") if args.len() == 5 => {
            let workbook = Workbook::open(&args[2])?;
            match workbook.get_cell(&args[3], &args[4])?.value {
                CellValue::Text(value) | CellValue::Error(value) => println!("{value}"),
                CellValue::Number(value) => println!("{value}"),
                CellValue::Bool(value) => println!("{value}"),
                CellValue::Blank => println!(),
            }
            Ok(())
        }
        Some("set") if args.len() == 7 || args.len() == 8 => {
            let value =
                parse_value(&args[6], args.get(7).map(String::as_str)).map_err(Failure::Usage)?;
            let mut workbook = Workbook::open(&args[2])?;
            workbook.set_cell(&args[4], &args[5], value)?;
            workbook.save(&args[3])?;
            Ok(())
        }
        Some("patch") if args.len() == 5 => {
            let edits = if args[4] == "-" {
                read_patch(io::stdin().lock())?
            } else {
                read_patch(BufReader::new(File::open(&args[4])?))?
            };
            let mut workbook = Workbook::open(&args[2])?;
            workbook.apply_edits(edits)?;
            workbook.save(&args[3])?;
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
    let args: Vec<String> = env::args().collect();
    if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(message)) => {
            eprintln!("sheetpatch: {message}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Workbook(error)) => {
            eprintln!("sheetpatch: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
