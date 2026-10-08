use std::{env, process::ExitCode};

use sheetpatch::{CellValue, Workbook};

const USAGE: &str = "Usage:\n  sheetpatch list INPUT\n  sheetpatch set INPUT OUTPUT SHEET CELL text|number|bool VALUE\n  sheetpatch set INPUT OUTPUT SHEET CELL blank\n\nQuote sheet names and text values containing spaces. Formats: .xlsx, .xlsm, .xltx, .xltm.";

enum Failure {
    Usage(String),
    Workbook(sheetpatch::Error),
}

impl From<sheetpatch::Error> for Failure {
    fn from(error: sheetpatch::Error) -> Self {
        Self::Workbook(error)
    }
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
        Some("set") if args.len() == 7 || args.len() == 8 => {
            let kind = args[6].as_str();
            let value = match (kind, args.get(7)) {
                ("blank", None) => CellValue::Blank,
                ("text", Some(value)) => CellValue::Text(value.clone()),
                ("number", Some(value)) => CellValue::Number(
                    value
                        .parse()
                        .map_err(|_| Failure::Usage(format!("Invalid number: {value}")))?,
                ),
                ("bool", Some(value)) => CellValue::Bool(match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(Failure::Usage(
                            "Boolean values must be true or false.".into(),
                        ));
                    }
                }),
                _ => {
                    return Err(Failure::Usage(
                        "Invalid value type or argument count.".into(),
                    ));
                }
            };
            let mut workbook = Workbook::open(&args[2])?;
            workbook.set_cell(&args[4], &args[5], value)?;
            workbook.save(&args[3])?;
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
