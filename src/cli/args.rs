use super::error::{DiagnosticText, Failure};
use sheetpatch::{CellRange, CellRef, CellValue};
use std::ffi::{OsStr, OsString};
use std::path::Path;

pub(super) const USAGE: &str = "Usage:\n  sheetpatch list [--json] INPUT\n  sheetpatch get [--json] INPUT SHEET CELL [CELL ...]\n  sheetpatch cells [--json] INPUT SHEET [RANGE]\n  sheetpatch set INPUT OUTPUT SHEET CELL text|number|bool|error VALUE\n  sheetpatch set INPUT OUTPUT SHEET CELL blank\n  sheetpatch patch INPUT OUTPUT PATCH.tsv\n  sheetpatch patch --check INPUT PATCH.tsv\n  sheetpatch compact INPUT OUTPUT\n  sheetpatch help [COMMAND]\n  sheetpatch --help\n  sheetpatch --version\n\nOptions go right after the command. Write a file whose name starts with - as ./NAME.\nIn shell arguments, quote sheet names and values containing spaces.\nUse - as INPUT to read the workbook from stdin, or as OUTPUT to write it to stdout when stdout is not a terminal.\nPatch rows: SHEET<TAB>CELL<TAB>TYPE<TAB>VALUE. Blank needs no value. Fields are raw: quotes are stored as written. Each physical patch row is limited to 1 MiB, including its BOM and line ending.\nUse - as PATCH.tsv to read stdin; INPUT and PATCH.tsv cannot both be -. Use LF or CRLF line endings.\nEmpty lines are ignored. Lines starting with # without tabs are comments.\n--check validates a patch against the workbook without saving.\nFormats: .xlsx, .xlsm, .xltx, .xltm.\nget prints each stored value in CELL argument order; formula results are cached and are not recalculated.\nA get CELL may be a range such as A1:C3, read row by row; one get reads at most 1048576 cells.\ncells prints CELL<TAB>TYPE<TAB>VALUE for each cell holding a value or formula, optionally within RANGE.\n--json returns an array: list includes names, paths and visibility; get and cells include types, values, formulas and styles.\nOn a terminal, plain list output shows every control character in names as a \\u{..} escape, and plain get and cells output shows control characters other than tab and line feed that way; --json is exact.";

pub(super) fn command_help(command: &str) -> Option<&'static str> {
    match command {
        "help" => Some(USAGE),
        "list" => Some(
            "Usage: sheetpatch list [--json] INPUT\n\nList editable worksheets, including hidden ones, in workbook order. --json includes each worksheet's name, package path and visibility: visible, hidden or veryHidden. Use - as INPUT to read the workbook from standard input. On a terminal, control characters in names, including tabs and line feeds, are shown as \\u{..} escapes; --json output is exact.",
        ),
        "get" => Some(
            "Usage: sheetpatch get [--json] INPUT SHEET CELL [CELL ...]\n\nRead A1 addresses in input order with one worksheet parse. A CELL may also be a range such as A1:C3, which is read row by row from its top-left cell; one get reads at most 1048576 cells. Names are case-sensitive; addresses accept lowercase letters. Formula results are stored caches, without recalculation. --json includes normalized addresses, scalar types, values, formulas and style indices. Blank values use null in JSON and an empty line in plain output. Use - as INPUT to read the workbook from standard input. On a terminal, plain output shows control characters other than tab and line feed as \\u{..} escapes; --json output is exact.",
        ),
        "cells" => Some(
            "Usage: sheetpatch cells [--json] INPUT SHEET [RANGE]\n\nList the cells of a worksheet that hold a value or a formula, row by row, with one worksheet parse. A RANGE such as A1:C3 or B2 limits the list to the cells within it. Cells holding only formatting are skipped. Plain output has one CELL<TAB>TYPE<TAB>VALUE line per cell, with the types text, number, bool, error or blank, and values printed as get prints them. Prefixing each line with the sheet name and a tab gives patch rows for values without line breaks, with these exceptions. patch refuses rows for error values other than Excel's writable error codes and for text longer than 32,767 UTF-16 code units. Applied to the cells they were listed from, it also refuses rows for formula cells, for the other cells of a shared, array (including dynamic-array spill) or data-table formula range, for merged cells other than their range's top-left cell, and for cells whose stored value holds markup sheetpatch does not recognize, such as unfamiliar elements or attributes inside <v> or <is>, even though the value is unchanged. It refuses every row of a worksheet with a shared, array or data-table formula group that has no reference range, and of a digitally signed workbook. Formula results are stored caches, without recalculation. --json returns the same objects as get --json, including formulas and style indices, and is exact. Use - as INPUT to read the workbook from standard input. On a terminal, plain output shows control characters other than tab and line feed as \\u{..} escapes.",
        ),
        "set" => Some(
            "Usage:\n  sheetpatch set INPUT OUTPUT SHEET CELL text|number|bool|error VALUE\n  sheetpatch set INPUT OUTPUT SHEET CELL blank\n\nSet one scalar cell value and save atomically. Text stays text even when it starts with =. Numbers must be finite; booleans are true or false; error tokens include #N/A and #DIV/0!. Blank clears the value while retaining formatting. Protected cells are refused: formula cells and formula ranges, merged-range followers, Excel table headers, and cells with cell or value metadata. INPUT and OUTPUT may be the same path. Use - as INPUT to read the workbook from standard input, or as OUTPUT to write it to standard output when that is not a terminal.",
        ),
        "patch" => Some(
            "Usage:\n  sheetpatch patch INPUT OUTPUT PATCH.tsv\n  sheetpatch patch --check INPUT PATCH.tsv\n\nApply a transaction of tab-separated SHEET, CELL, TYPE and VALUE fields. Fields are raw: there is no quoting or escaping, so double quotes are stored as written and a value cannot span lines. Types: text, number, bool, error, blank. Blank may omit VALUE. Each physical row is limited to 1 MiB, including its BOM and line ending; oversized rows are rejected before allocation can grow beyond that budget. Duplicate cells use the last value. Text retains extra tabs and trailing whitespace. UTF-8 BOM and CRLF files are accepted; any other carriage return is rejected unless it is the last byte of the file, which stays in the value. Empty lines and tab-free lines starting with # are ignored. Use - as PATCH.tsv to read standard input. Use - as INPUT to read the workbook from standard input, or as OUTPUT to write it to standard output when that is not a terminal; INPUT and PATCH.tsv cannot both be -. --check validates the complete transaction and output generation without saving; success reports the number of patch rows. Check and apply both use the same edit protections and ZIP limits.",
        ),
        "compact" => Some(
            "Usage: sheetpatch compact INPUT OUTPUT\n\nRemove safely recognized obsolete ZIP records and save atomically. An archive with unrecognized prefixes or gaps is refused rather than having them removed. Compaction retains untouched active records, does not calculate formulas, and does not securely erase copies or filesystem history. INPUT and OUTPUT may be the same path. Use - as INPUT to read the workbook from standard input, or as OUTPUT to write it to standard output when that is not a terminal.",
        ),
        _ => None,
    }
}

#[derive(Clone, Copy)]
pub(super) enum Command {
    List,
    Get,
    Cells,
    Set,
    Patch,
    Compact,
}

impl Command {
    pub(super) fn parse(name: &str) -> Option<Self> {
        match name {
            "list" => Some(Self::List),
            "get" => Some(Self::Get),
            "cells" => Some(Self::Cells),
            "set" => Some(Self::Set),
            "patch" => Some(Self::Patch),
            "compact" => Some(Self::Compact),
            _ => None,
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Get => "get",
            Self::Cells => "cells",
            Self::Set => "set",
            Self::Patch => "patch",
            Self::Compact => "compact",
        }
    }

    /// The option accepted right after the command name besides help, with
    /// a synopsis that shows where it belongs.
    pub(super) fn option(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::List => Some(("--json", "sheetpatch list --json INPUT")),
            Self::Get => Some((
                "--json",
                "sheetpatch get --json INPUT SHEET CELL [CELL ...]",
            )),
            Self::Cells => Some(("--json", "sheetpatch cells --json INPUT SHEET [RANGE]")),
            Self::Patch => Some(("--check", "sheetpatch patch --check INPUT PATCH.tsv")),
            Self::Set | Self::Compact => None,
        }
    }

    /// The synopsis for `option` when this command accepts it.
    pub(super) fn synopsis_for(self, option: &OsStr) -> Option<String> {
        let name = self.name();
        match option.to_str()? {
            help @ ("--help" | "-h") => Some(format!("sheetpatch {name} {help}")),
            option => self
                .option()
                .filter(|(flag, _)| *flag == option)
                .map(|(_, synopsis)| synopsis.to_owned()),
        }
    }
}

/// The kind of value a positional argument holds.
#[derive(Clone, Copy)]
enum Slot {
    /// A file path, or `-` for a standard stream. Paths that look like options
    /// are rejected so a misplaced flag never becomes a file name.
    Path(&'static str),
    /// A cell address or range. A trailing `Cell` slot repeats.
    Cell,
    /// A single optional cell range, which unlike `Cell` does not repeat.
    Range,
    /// A sheet name, value type, or value, which may legitimately start with '-'.
    Text,
}

pub(super) fn is_option(arg: &OsStr) -> bool {
    arg.as_encoded_bytes().starts_with(b"-") && arg != "-"
}

/// A command's leading options and its positional arguments.
pub(super) struct CommandLine<'args> {
    pub(super) command: Command,
    /// Whether the command's option (`--json` or `--check`) was given.
    pub(super) flag: bool,
    pub(super) help: bool,
    args: &'args [OsString],
    /// The index of the first positional argument in `args`.
    start: usize,
}

impl<'args> CommandLine<'args> {
    /// Read options from right after the command name up to the first
    /// positional argument.
    pub(super) fn parse(command: Command, args: &'args [OsString]) -> Result<Self, Failure> {
        let mut line = Self {
            command,
            flag: false,
            help: false,
            args,
            start: 2,
        };
        while let Some(arg) = args.get(line.start).filter(|arg| is_option(arg)) {
            match arg.to_str() {
                Some(option @ ("--help" | "-h")) => {
                    if line.help {
                        return Err(Failure::Usage(format!("option {option:?} is repeated")));
                    }
                    line.help = true;
                }
                Some(option) if command.option().is_some_and(|(flag, _)| flag == option) => {
                    if line.flag {
                        return Err(Failure::Usage(format!("option {option:?} is repeated")));
                    }
                    line.flag = true;
                }
                _ => {
                    let accepted = match command.option() {
                        Some((flag, _)) => format!("{flag}, --help or -h"),
                        None => "--help or -h".into(),
                    };
                    return Err(Failure::Usage(format!(
                        "unknown option {arg:?} for {}; it accepts {accepted}",
                        command.name()
                    )));
                }
            }
            line.start += 1;
        }
        Ok(line)
    }

    /// Return the positional arguments after checking their number and
    /// rejecting options where paths or addresses belong. A trailing `Cell`
    /// slot repeats.
    pub(super) fn positionals(&self) -> Result<&'args [OsString], Failure> {
        let (slots, required, synopsis): (&[Slot], usize, &str) = match self.command {
            Command::List => (&[Slot::Path("INPUT")], 1, "sheetpatch list [--json] INPUT"),
            Command::Get => (
                &[Slot::Path("INPUT"), Slot::Text, Slot::Cell],
                3,
                "sheetpatch get [--json] INPUT SHEET CELL [CELL ...]",
            ),
            Command::Cells => (
                &[Slot::Path("INPUT"), Slot::Text, Slot::Range],
                2,
                "sheetpatch cells [--json] INPUT SHEET [RANGE]",
            ),
            Command::Set => (
                &[
                    Slot::Path("INPUT"),
                    Slot::Path("OUTPUT"),
                    Slot::Text,
                    Slot::Text,
                    Slot::Text,
                    Slot::Text,
                ],
                5,
                "sheetpatch set INPUT OUTPUT SHEET CELL TYPE [VALUE]",
            ),
            Command::Patch if self.flag => (
                &[Slot::Path("INPUT"), Slot::Path("PATCH.tsv")],
                2,
                "sheetpatch patch --check INPUT PATCH.tsv",
            ),
            Command::Patch => (
                &[
                    Slot::Path("INPUT"),
                    Slot::Path("OUTPUT"),
                    Slot::Path("PATCH.tsv"),
                ],
                3,
                "sheetpatch patch INPUT OUTPUT PATCH.tsv",
            ),
            Command::Compact => (
                &[Slot::Path("INPUT"), Slot::Path("OUTPUT")],
                2,
                "sheetpatch compact INPUT OUTPUT",
            ),
        };
        let positionals = &self.args[self.start..];
        let repeated = match slots.last() {
            Some(Slot::Cell) => slots.last(),
            _ => None,
        };
        for (index, arg) in positionals.iter().enumerate() {
            match slots.get(index).or(repeated) {
                Some(Slot::Path(slot)) if is_option(arg) => {
                    return Err(self.misplaced_option(arg, Some(slot)));
                }
                // An address never starts with '-'; name the misplaced option
                // instead of reporting an invalid address.
                Some(Slot::Cell | Slot::Range) if self.command.synopsis_for(arg).is_some() => {
                    return Err(self.misplaced_option(arg, None));
                }
                None if is_option(arg) => return Err(self.misplaced_option(arg, None)),
                _ => {}
            }
        }
        if positionals.len() < required || (repeated.is_none() && positionals.len() > slots.len()) {
            return Err(Failure::Usage(format!(
                "wrong number of arguments for {}; expected: {synopsis}",
                self.command.name()
            )));
        }
        Ok(positionals)
    }

    fn misplaced_option(&self, arg: &OsStr, slot: Option<&str>) -> Failure {
        use std::fmt::Write as _;

        let mut message = match slot {
            Some(slot) => format!(
                "{arg:?} is in the {slot} position, but options must come right after the command"
            ),
            None => {
                format!("option {arg:?} is out of place; options must come right after the command")
            }
        };
        if let Some(synopsis) = self.command.synopsis_for(arg) {
            message.push_str(": ");
            message.push_str(&synopsis);
        }
        if slot.is_some() {
            write!(
                message,
                "\nTo name a file that starts with '-', write \"./{}\".",
                Path::new(arg).display()
            )
            .expect("writing to a String cannot fail");
        }
        Failure::Usage(message)
    }

    /// Return positional argument `index` as text.
    pub(super) fn text(&self, index: usize) -> Result<&'args str, Failure> {
        text_arg(self.command.name(), self.args, self.start + index)
    }
}

pub(super) fn text_arg<'a>(
    command: &'static str,
    args: &'a [OsString],
    index: usize,
) -> Result<&'a str, Failure> {
    args[index].to_str().ok_or_else(|| {
        Failure::invalid(
            command,
            format!("Argument {index} must be valid Unicode text."),
        )
    })
}

/// Why a TYPE and VALUE pair was rejected.
pub(super) enum ValueError {
    /// VALUE is missing, or present for `blank`: an argument-count problem for `set`.
    Arity(String),
    /// The type or value is invalid.
    Invalid(String),
}

pub(super) fn parse_value(kind: &str, value: Option<&str>) -> Result<CellValue, ValueError> {
    // Reject oversized text before allocating an owned value. CellEdit still
    // validates every value centrally, including duplicate patch targets.
    const MAX_TEXT_UNITS: usize = 32_767;
    match (kind, value) {
        ("blank", None | Some("")) => Ok(CellValue::Blank),
        ("blank", Some(value)) => Err(ValueError::Arity(format!(
            "blank takes no VALUE, but {} was given",
            DiagnosticText(value)
        ))),
        ("text", Some(value))
            if value.len() > MAX_TEXT_UNITS
                && value.encode_utf16().take(MAX_TEXT_UNITS + 1).count() > MAX_TEXT_UNITS =>
        {
            Err(ValueError::Invalid(
                "invalid cell value: text exceeds Excel's 32,767 UTF-16 code unit limit".into(),
            ))
        }
        ("text", Some(value)) => Ok(CellValue::Text(value.to_owned())),
        ("error", Some(value)) => Ok(CellValue::Error(value.to_owned())),
        ("number", Some(value)) => value
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(CellValue::Number)
            .ok_or_else(|| {
                ValueError::Invalid(format!("Invalid finite number: {}", DiagnosticText(value)))
            }),
        ("bool", Some("true")) => Ok(CellValue::Bool(true)),
        ("bool", Some("false")) => Ok(CellValue::Bool(false)),
        ("bool", Some(value)) => Err(ValueError::Invalid(format!(
            "Boolean values must be true or false, not {}",
            DiagnosticText(value)
        ))),
        ("text" | "error" | "number" | "bool", None) => {
            Err(ValueError::Arity(format!("{kind} needs a VALUE")))
        }
        _ => Err(ValueError::Invalid(format!(
            "unknown value type {}; expected text, number, bool, error or blank",
            DiagnosticText(kind)
        ))),
    }
}

pub(super) fn parse_cell(address: &str) -> Result<CellRef, String> {
    address
        .parse()
        .map_err(|_| format!("invalid cell reference {}", DiagnosticText(address)))
}

/// Parse a range such as `A1:C3`, or a single address such as `B2`.
pub(super) fn parse_range(command: &'static str, range: &str) -> Result<CellRange, Failure> {
    range.parse().map_err(|_| {
        let kind = if range.contains(':') {
            "cell range"
        } else {
            "cell reference"
        };
        Failure::invalid(command, format!("invalid {kind} {}", DiagnosticText(range)))
    })
}
