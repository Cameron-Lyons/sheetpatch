#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    env,
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io::{self, BufRead, BufReader, BufWriter, IsTerminal, Write},
    path::Path,
    process::ExitCode,
};

use sheetpatch::{CellContent, CellEdit, CellRange, CellRef, CellValue, Workbook, WorksheetView};

const USAGE: &str = "Usage:\n  sheetpatch list [--json] INPUT\n  sheetpatch get [--json] INPUT SHEET CELL [CELL ...]\n  sheetpatch cells [--json] INPUT SHEET [RANGE]\n  sheetpatch set INPUT OUTPUT SHEET CELL text|number|bool|error VALUE\n  sheetpatch set INPUT OUTPUT SHEET CELL blank\n  sheetpatch patch INPUT OUTPUT PATCH.tsv\n  sheetpatch patch --check INPUT PATCH.tsv\n  sheetpatch compact INPUT OUTPUT\n  sheetpatch help [COMMAND]\n  sheetpatch --help\n  sheetpatch --version\n\nOptions go right after the command. Write a file whose name starts with - as ./NAME.\nUse - as INPUT to read the workbook from stdin, or as OUTPUT to write it to stdout when stdout is not a terminal.\nPatch rows: SHEET<TAB>CELL<TAB>TYPE<TAB>VALUE. Blank needs no value.\nUse - as PATCH.tsv to read stdin; INPUT and PATCH.tsv cannot both be -. Use LF or CRLF line endings.\nEmpty lines are ignored. Lines starting with # without tabs are comments.\n--check validates a patch against the workbook without saving.\nQuote sheet names and text values containing spaces. Formats: .xlsx, .xlsm, .xltx, .xltm.\nget prints each stored value in CELL argument order; formula results are cached and are not recalculated.\nA get CELL may be a range such as A1:C3, read row by row; one get reads at most 1048576 cells.\ncells prints CELL<TAB>TYPE<TAB>VALUE for each cell holding a value or formula, optionally within RANGE.\n--json returns an array: list includes names, paths and visibility; get and cells include types, values, formulas and styles.\nOn a terminal, plain list output shows every control character in names as a \\u{..} escape, and plain get and cells output shows control characters other than tab and line feed that way; --json is exact.";

/// The most cells one `get` reads, counting every cell of each range.
const MAX_GET_CELLS: u64 = 1_048_576;
/// Keep common queries to one read without retaining unbounded decoded output.
const READ_CACHE_CELLS: usize = 128;
const READ_CACHE_BYTES: usize = 4 * 1024 * 1024;

fn command_help(command: &str) -> Option<&'static str> {
    match command {
        "help" => Some(USAGE),
        "list" => Some(
            "Usage: sheetpatch list [--json] INPUT\n\nList editable worksheets, including hidden ones, in workbook order. --json includes each worksheet's name, package path and visibility: visible, hidden or veryHidden. Use - as INPUT to read the workbook from standard input. On a terminal, control characters in names, including tabs and line feeds, are shown as \\u{..} escapes; --json output is exact.",
        ),
        "get" => Some(
            "Usage: sheetpatch get [--json] INPUT SHEET CELL [CELL ...]\n\nRead A1 addresses in input order with one worksheet parse. A CELL may also be a range such as A1:C3, which is read row by row from its top-left cell; one get reads at most 1048576 cells. Names are case-sensitive; addresses accept lowercase letters. Formula results are stored caches, without recalculation. --json includes normalized addresses, scalar types, values, formulas and style indices. Blank values use null in JSON and an empty line in plain output. Use - as INPUT to read the workbook from standard input. On a terminal, plain output shows control characters other than tab and line feed as \\u{..} escapes; --json output is exact.",
        ),
        "cells" => Some(
            "Usage: sheetpatch cells [--json] INPUT SHEET [RANGE]\n\nList the cells of a worksheet that hold a value or a formula, row by row, with one worksheet parse. A RANGE such as A1:C3 or B2 limits the list to the cells within it. Cells holding only formatting are skipped. Plain output has one CELL<TAB>TYPE<TAB>VALUE line per cell, with the types text, number, bool, error or blank, and values printed as get prints them. Prefixing each line with the sheet name and a tab gives patch rows for values without line breaks, except that rows for formula cells cannot be applied: patch refuses edits to formula cells. Formula results are stored caches, without recalculation. --json returns the same objects as get --json, including formulas and style indices, and is exact. Use - as INPUT to read the workbook from standard input. On a terminal, plain output shows control characters other than tab and line feed as \\u{..} escapes.",
        ),
        "set" => Some(
            "Usage:\n  sheetpatch set INPUT OUTPUT SHEET CELL text|number|bool|error VALUE\n  sheetpatch set INPUT OUTPUT SHEET CELL blank\n\nSet one scalar cell value and save atomically. Text stays text even when it starts with =. Numbers must be finite; booleans are true or false; error tokens include #N/A and #DIV/0!. Blank clears the value while retaining formatting. Protected cells are refused: formula cells and formula ranges, merged-range followers, Excel table headers, and cells with cell or value metadata. INPUT and OUTPUT may be the same path. Use - as INPUT to read the workbook from standard input, or as OUTPUT to write it to standard output when that is not a terminal.",
        ),
        "patch" => Some(
            "Usage:\n  sheetpatch patch INPUT OUTPUT PATCH.tsv\n  sheetpatch patch --check INPUT PATCH.tsv\n\nApply a transaction of tab-separated SHEET, CELL, TYPE and VALUE fields. Types: text, number, bool, error, blank. Blank may omit VALUE. Duplicate cells use the last value. Text retains extra tabs and trailing whitespace. UTF-8 BOM and CRLF files are accepted; any other carriage return is rejected unless it is the last byte of the file, which stays in the value. Empty lines and tab-free lines starting with # are ignored. Use - as PATCH.tsv to read standard input. Use - as INPUT to read the workbook from standard input, or as OUTPUT to write it to standard output when that is not a terminal; INPUT and PATCH.tsv cannot both be -. --check validates the complete transaction and output generation without saving; success reports the number of patch rows. Check and apply both use the same edit protections and ZIP limits.",
        ),
        "compact" => Some(
            "Usage: sheetpatch compact INPUT OUTPUT\n\nRemove safely recognized obsolete ZIP records and save atomically. An archive with unrecognized prefixes or gaps is refused rather than having them removed. Compaction retains untouched active records, does not calculate formulas, and does not securely erase copies or filesystem history. INPUT and OUTPUT may be the same path. Use - as INPUT to read the workbook from standard input, or as OUTPUT to write it to standard output when that is not a terminal.",
        ),
        _ => None,
    }
}

#[derive(Debug)]
enum Failure {
    /// Invalid command syntax; the full usage follows the message.
    Usage(String),
    /// An invalid argument value or patch row; a pointer to the command's help
    /// follows the message.
    Invalid {
        command: &'static str,
        message: String,
    },
    /// A workbook could not be read, edited, or written.
    Workbook(sheetpatch::Error),
    /// A workbook error, prefixed with what failed: an operation and its path
    /// or stream, or the patch line whose edit was refused.
    Context(String, sheetpatch::Error),
    /// Writing text to standard output failed. A workbook that cannot be
    /// written to standard output is a `Context` failure instead, so a closed
    /// pipe is never mistaken for a complete archive.
    Output(io::Error),
}

impl Failure {
    fn invalid(command: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            command,
            message: message.into(),
        }
    }

    fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) | Self::Invalid { .. } => 2,
            Self::Workbook(_) | Self::Context(..) | Self::Output(_) => 1,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(f, "{message}\n\n{USAGE}"),
            Self::Invalid { command, message } => {
                let topic = if *command == "patch" {
                    "the format"
                } else {
                    "details"
                };
                write!(f, "{message}\nRun 'sheetpatch help {command}' for {topic}.")
            }
            Self::Workbook(error) => write!(f, "{error}"),
            // The context already says what failed; print the bare OS message.
            Self::Context(context, sheetpatch::Error::Io(error)) => write!(f, "{context}: {error}"),
            Self::Context(context, error) => write!(f, "{context}: {error}"),
            Self::Output(error) => write!(f, "cannot write output: {error}"),
        }
    }
}

impl From<sheetpatch::Error> for Failure {
    fn from(error: sheetpatch::Error) -> Self {
        Self::Workbook(error)
    }
}

/// Writes to standard output. Other I/O errors are converted with context.
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Output(error)
    }
}

#[derive(Clone, Copy)]
enum Command {
    List,
    Get,
    Cells,
    Set,
    Patch,
    Compact,
}

impl Command {
    fn parse(name: &str) -> Option<Self> {
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

    fn name(self) -> &'static str {
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
    fn option(self) -> Option<(&'static str, &'static str)> {
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
    fn synopsis_for(self, option: &OsStr) -> Option<String> {
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

fn is_option(arg: &OsStr) -> bool {
    arg.as_encoded_bytes().starts_with(b"-") && arg != "-"
}

/// A command's leading options and its positional arguments.
struct CommandLine<'a> {
    command: Command,
    /// Whether the command's option (`--json` or `--check`) was given.
    flag: bool,
    help: bool,
    args: &'a [OsString],
    /// The index of the first positional argument in `args`.
    start: usize,
}

impl<'a> CommandLine<'a> {
    /// Read options from right after the command name up to the first
    /// positional argument.
    fn parse(command: Command, args: &'a [OsString]) -> Result<Self, Failure> {
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
    fn positionals(
        &self,
        slots: &[Slot],
        required: usize,
        synopsis: &str,
    ) -> Result<&'a [OsString], Failure> {
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
            message.push_str(&format!(
                "\nTo name a file that starts with '-', write \"./{}\".",
                Path::new(arg).display()
            ));
        }
        Failure::Usage(message)
    }

    /// Return positional argument `index` as text.
    fn text(&self, index: usize) -> Result<&'a str, Failure> {
        text_arg(self.command.name(), self.args, self.start + index)
    }
}

fn text_arg<'a>(
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

fn quoted_path(path: &OsStr) -> String {
    format!("'{}'", Path::new(path).display())
}

/// Where a command reads its workbook from.
enum Source<'a> {
    Stdin,
    File(&'a OsStr),
}

impl<'a> Source<'a> {
    fn new(command: Command, arg: &'a OsStr) -> Result<Self, Failure> {
        if arg != "-" {
            return Ok(Self::File(arg));
        }
        if io::stdin().is_terminal() {
            return Err(Failure::invalid(
                command.name(),
                "refusing to read a workbook from a terminal; redirect standard input or name an INPUT file",
            ));
        }
        Ok(Self::Stdin)
    }

    fn open(self) -> Result<Workbook, Failure> {
        match self {
            Self::File(path) => Workbook::open(path).map_err(|error| {
                Failure::Context(format!("cannot read workbook {}", quoted_path(path)), error)
            }),
            // Reading stops once the input exceeds the largest ZIP32 archive.
            Self::Stdin => Workbook::from_reader(io::stdin().lock()).map_err(|error| {
                Failure::Context("cannot read workbook from standard input".into(), error)
            }),
        }
    }
}

/// Where a command writes its workbook.
enum Destination<'a> {
    Stdout,
    File(&'a OsStr),
}

impl<'a> Destination<'a> {
    fn new(command: Command, arg: &'a OsStr, terminal: bool) -> Result<Self, Failure> {
        if arg != "-" {
            return Ok(Self::File(arg));
        }
        if terminal {
            return Err(Failure::invalid(
                command.name(),
                "refusing to write a workbook to a terminal; redirect standard output or name an OUTPUT file",
            ));
        }
        Ok(Self::Stdout)
    }

    fn save(
        self,
        workbook: &Workbook,
        compact: bool,
        output: &mut impl Write,
    ) -> Result<(), Failure> {
        match self {
            Self::File(path) => {
                let saved = if compact {
                    workbook.save_compact(path)
                } else {
                    workbook.save(path)
                };
                // Generation errors, such as ZIP limits, match `patch --check`
                // output; only file system failures name the destination.
                saved.map_err(|error| match error {
                    sheetpatch::Error::Io(_) => {
                        Failure::Context(format!("cannot save {}", quoted_path(path)), error)
                    }
                    error => Failure::Workbook(error),
                })
            }
            Self::Stdout => {
                let written = if compact {
                    workbook.write_compact_to(&mut *output)
                } else {
                    workbook.write_to(&mut *output)
                };
                // Archive generation reports I/O errors only from its writer.
                // Unlike text, a truncated archive is unusable, so even a pipe
                // closed by its reader is a failure.
                written.map_err(|error| match error {
                    sheetpatch::Error::Io(_) => {
                        Failure::Context("cannot write workbook to standard output".into(), error)
                    }
                    error => Failure::Workbook(error),
                })
            }
        }
    }
}

/// Why a TYPE and VALUE pair was rejected.
enum ValueError {
    /// VALUE is missing, or present for `blank`: an argument-count problem for `set`.
    Arity(String),
    /// The type or value is invalid.
    Invalid(String),
}

fn parse_value(kind: &str, value: Option<&str>) -> Result<CellValue, ValueError> {
    match (kind, value) {
        ("blank", None | Some("")) => Ok(CellValue::Blank),
        ("blank", Some(value)) => Err(ValueError::Arity(format!(
            "blank takes no VALUE, but {value:?} was given"
        ))),
        ("text", Some(value)) => Ok(CellValue::Text(value.to_owned())),
        ("error", Some(value)) => Ok(CellValue::Error(value.to_owned())),
        ("number", Some(value)) => value
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(CellValue::Number)
            .ok_or_else(|| ValueError::Invalid(format!("Invalid finite number: {value:?}"))),
        ("bool", Some("true")) => Ok(CellValue::Bool(true)),
        ("bool", Some("false")) => Ok(CellValue::Bool(false)),
        ("bool", Some(value)) => Err(ValueError::Invalid(format!(
            "Boolean values must be true or false, not {value:?}"
        ))),
        ("text" | "error" | "number" | "bool", None) => {
            Err(ValueError::Arity(format!("{kind} needs a VALUE")))
        }
        _ => Err(ValueError::Invalid(format!(
            "unknown value type {kind:?}; expected text, number, bool, error or blank"
        ))),
    }
}

fn parse_cell(address: &str) -> Result<CellRef, String> {
    address
        .parse()
        .map_err(|_| format!("invalid cell reference {address:?}"))
}

/// Parse a range such as `A1:C3`, or a single address such as `B2`.
fn parse_range(command: &'static str, range: &str) -> Result<CellRange, Failure> {
    range.parse().map_err(|_| {
        let kind = if range.contains(':') {
            "cell range"
        } else {
            "cell reference"
        };
        Failure::invalid(command, format!("invalid {kind} {range:?}"))
    })
}

/// The patch lines of each unique edit, kept for diagnostics after the
/// edits move into the transaction.
#[derive(Debug, Default)]
struct PatchLines {
    /// The first and last line of each edit, by edit index.
    lines: Vec<(usize, usize)>,
    /// The index of each edit by worksheet name and cell.
    targets: HashMap<String, HashMap<CellRef, usize>>,
}

impl PatchLines {
    /// The line to report with a refused transaction: the first line naming a
    /// missing worksheet, or the last line supplying a protected cell's value.
    fn line_for(&self, error: &sheetpatch::Error) -> Option<usize> {
        match error {
            // Edits are indexed in first-occurrence order, so a worksheet's
            // lowest edit index began on the first line naming it.
            sheetpatch::Error::SheetNotFound(sheet) => {
                let first = self.targets.get(sheet)?.values().min()?;
                Some(self.lines[*first].0)
            }
            sheetpatch::Error::ProtectedCell { sheet, cell, .. } => {
                let index = self.targets.get(sheet)?.get(cell)?;
                Some(self.lines[*index].1)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
struct Patch {
    // Retain unique targets in their first occurrence's order, with the last
    // value; `lines` holds both diagnostic line numbers.
    edits: Vec<CellEdit>,
    lines: PatchLines,
    row_count: usize,
}

/// Validate every patch row, retaining only each target's final value.
/// `origin` names the source in I/O error messages.
fn read_patch(mut reader: impl BufRead, origin: &str) -> Result<Patch, Failure> {
    let mut patch = Patch::default();
    let mut line = String::new();
    let mut line_number = 0usize;
    loop {
        line.clear();
        line_number += 1;
        let invalid = |message: String| {
            Failure::invalid("patch", format!("Patch line {line_number}: {message}"))
        };
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return Err(invalid(format!("expected UTF-8 text: {error}")));
            }
            Err(error) => {
                return Err(Failure::Context(
                    format!("cannot read patch {origin}"),
                    error.into(),
                ));
            }
        }
        let terminated = line.ends_with('\n');
        if terminated {
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
        // A carriage return ends a line only before a line feed. Followed by
        // more data, it would merge classic Mac lines into one text value, and
        // before a CRLF it would leave a stray carriage return in the value.
        // Only a final line without a line feed may end with one, which then
        // remains part of the value.
        let body = if terminated {
            line
        } else {
            line.strip_suffix('\r').unwrap_or(line)
        };
        if body.contains('\r') {
            return Err(invalid(
                "carriage return without line feed; use LF or CRLF line endings".into(),
            ));
        }
        if line.trim().is_empty() || (line.starts_with('#') && !line.contains('\t')) {
            continue;
        }
        let mut fields = line.splitn(4, '\t');
        let sheet = fields.next().unwrap_or_default();
        let (Some(cell), Some(kind)) = (fields.next(), fields.next()) else {
            return Err(invalid("expected SHEET<TAB>CELL<TAB>TYPE<TAB>VALUE".into()));
        };
        let cell = parse_cell(cell).map_err(invalid)?;
        let value = parse_value(kind, fields.next()).map_err(|error| match error {
            ValueError::Arity(message) | ValueError::Invalid(message) => invalid(message),
        })?;
        let edit = CellEdit::at(sheet, cell, value).map_err(|error| invalid(error.to_string()))?;
        patch.row_count += 1;
        let lines = &mut patch.lines;
        if let Some(&index) = lines.targets.get(sheet).and_then(|cells| cells.get(&cell)) {
            lines.lines[index].1 = line_number;
            patch.edits[index] = edit;
        } else {
            lines
                .targets
                .entry(sheet.to_owned())
                .or_default()
                .insert(cell, patch.edits.len());
            lines.lines.push((line_number, line_number));
            patch.edits.push(edit);
        }
    }
    Ok(patch)
}

fn read_patch_arg(path: &OsStr) -> Result<Patch, Failure> {
    if path == "-" {
        return read_patch(io::stdin().lock(), "from standard input");
    }
    let origin = quoted_path(path);
    let file = File::open(path)
        .map_err(|error| Failure::Context(format!("cannot read patch {origin}"), error.into()))?;
    read_patch(BufReader::new(file), &origin)
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
            // JSON requires escapes only for C0 controls. Escaping DEL and C1
            // controls too keeps them out of terminals showing the output.
            '\u{00}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => "",
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

/// Write `items` as one JSON array followed by a newline.
fn write_json_array<W: Write, T>(
    output: &mut W,
    items: impl IntoIterator<Item = T>,
    mut write_item: impl FnMut(&mut W, T) -> io::Result<()>,
) -> io::Result<()> {
    output.write_all(b"[")?;
    for (index, item) in items.into_iter().enumerate() {
        if index != 0 {
            output.write_all(b",")?;
        }
        write_item(output, item)?;
    }
    output.write_all(b"]\n")
}

/// The characters that plain output shows as `\u{..}` escapes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    /// Pipes and files receive the exact text.
    Exact,
    /// Cell values on a terminal: C0 controls other than tab and line feed,
    /// DEL, and C1 controls, so text from a workbook cannot move the cursor,
    /// clear the screen, or retitle the window.
    Controls,
    /// Names on a terminal, one per line: every control, including tab and
    /// line feed, so one crafted name cannot look like two.
    AllControls,
}

impl Escape {
    fn values(terminal: bool) -> Self {
        if terminal {
            Self::Controls
        } else {
            Self::Exact
        }
    }

    fn names(terminal: bool) -> Self {
        if terminal {
            Self::AllControls
        } else {
            Self::Exact
        }
    }

    fn applies(self, ch: char) -> bool {
        match self {
            Self::Exact => false,
            Self::Controls => ch.is_control() && !matches!(ch, '\t' | '\n'),
            Self::AllControls => ch.is_control(),
        }
    }
}

/// Displays a value for plain output with the characters its `Escape` selects
/// shown as `\u{..}` escapes.
struct Escaped<T>(T, Escape);

impl<T: fmt::Display> fmt::Display for Escaped<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Escaping<'a, 'b>(&'a mut fmt::Formatter<'b>, Escape);

        impl fmt::Write for Escaping<'_, '_> {
            fn write_str(&mut self, text: &str) -> fmt::Result {
                let mut start = 0;
                for (index, ch) in text.char_indices() {
                    if self.1.applies(ch) {
                        self.0.write_str(&text[start..index])?;
                        write!(self.0, "\\u{{{:x}}}", u32::from(ch))?;
                        start = index + ch.len_utf8();
                    }
                }
                self.0.write_str(&text[start..])
            }
        }

        match self.1 {
            Escape::Exact => fmt::Display::fmt(&self.0, f),
            escape => fmt::Write::write_fmt(&mut Escaping(f, escape), format_args!("{}", self.0)),
        }
    }
}

/// The `TYPE` of a value, as patch rows and JSON output spell it.
fn value_type(value: &CellValue) -> &'static str {
    match value {
        CellValue::Text(_) => "text",
        CellValue::Number(_) => "number",
        CellValue::Bool(_) => "bool",
        CellValue::Error(_) => "error",
        CellValue::Blank => "blank",
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
    let kind = value_type(&content.value);
    write!(output, ",\"type\":\"{kind}\",\"value\":")?;
    match &content.value {
        CellValue::Text(text) | CellValue::Error(text) => write_json_string(output, text)?,
        CellValue::Blank => output.write_all(b"null")?,
        // Numbers are finite and displayed without exponents, and booleans as
        // true or false, so both display as JSON literals.
        value => write!(output, "{value}")?,
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

struct CellOutput<'a> {
    sheet: &'a str,
    json: bool,
    /// `cells` includes addresses and types, and omits formatting-only cells.
    records: bool,
    escape: Escape,
}

/// Validate every requested value before emitting any output. Cache a bounded
/// prefix, then reread remaining values individually from the prepared sheet.
/// A cell-count limit alone would let oversized stored strings multiply memory
/// use; the byte budget covers both scalar text and stored formulas.
fn write_read_cells(
    view: &WorksheetView<'_>,
    cells: &[CellRef],
    output: &mut impl Write,
    format: CellOutput<'_>,
) -> Result<(), Failure> {
    let mut cached = Vec::with_capacity(cells.len().min(READ_CACHE_CELLS));
    let mut cached_bytes = 0usize;
    let mut caching = true;
    for &cell in cells {
        let content = view.get_cell_at(cell)?;
        if caching {
            let value_bytes = match &content.value {
                CellValue::Text(text) | CellValue::Error(text) => text.capacity(),
                _ => 0,
            };
            let bytes =
                value_bytes.saturating_add(content.formula.as_ref().map_or(0, String::capacity));
            if bytes <= READ_CACHE_BYTES - cached_bytes {
                cached_bytes += bytes;
                cached.push(content);
                caching = cached.len() < READ_CACHE_CELLS;
            } else {
                caching = false;
            }
        }
    }
    if format.json {
        output.write_all(b"[")?;
    }
    let mut first = true;
    let mut cached = cached.into_iter();
    for &cell in cells {
        let content = match cached.next() {
            Some(content) => content,
            None => view.get_cell_at(cell)?,
        };
        if format.records && content.value.is_blank() && content.formula.is_none() {
            continue;
        }
        if format.json {
            if !first {
                output.write_all(b",")?;
            }
            write_json_cell(output, format.sheet, cell, &content)?;
            first = false;
        } else if format.records {
            let kind = value_type(&content.value);
            writeln!(
                output,
                "{cell}\t{kind}\t{}",
                Escaped(&content.value, format.escape)
            )?;
        } else {
            writeln!(output, "{}", Escaped(&content.value, format.escape))?;
        }
    }
    if format.json {
        output.write_all(b"]\n")?;
    }
    Ok(())
}

fn cmd_help(args: &[OsString], output: &mut impl Write) -> Result<(), Failure> {
    let help = match &args[2..] {
        [] => USAGE,
        [topic] => match topic.to_str() {
            Some("--help" | "-h") => USAGE,
            Some(topic) => command_help(topic)
                .ok_or_else(|| Failure::Usage(format!("unknown command {topic:?} for help")))?,
            None => {
                return Err(Failure::Usage(format!(
                    "unknown command {topic:?} for help"
                )));
            }
        },
        _ => return Err(Failure::Usage("help accepts at most one COMMAND".into())),
    };
    writeln!(output, "{help}")?;
    Ok(())
}

fn cmd_list(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let args = line.positionals(&[Slot::Path("INPUT")], 1, "sheetpatch list [--json] INPUT")?;
    let workbook = Source::new(line.command, &args[0])?.open()?;
    let sheets = workbook.sheets();
    if line.flag {
        write_json_array(output, sheets, |output, sheet| {
            output.write_all(b"{\"name\":")?;
            write_json_string(output, sheet.name())?;
            output.write_all(b",\"path\":")?;
            write_json_string(output, sheet.path())?;
            let visibility = sheet.visibility().as_str();
            write!(output, ",\"visibility\":\"{visibility}\"}}")
        })?;
    } else {
        let escape = Escape::names(terminal);
        for sheet in sheets {
            writeln!(output, "{}", Escaped(sheet.name(), escape))?;
        }
    }
    Ok(())
}

fn cmd_get(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let args = line.positionals(
        &[Slot::Path("INPUT"), Slot::Text, Slot::Cell],
        3,
        "sheetpatch get [--json] INPUT SHEET CELL [CELL ...]",
    )?;
    let sheet = line.text(1)?;
    let ranges = (2..args.len())
        .map(|index| parse_range("get", line.text(index)?))
        .collect::<Result<Vec<_>, _>>()?;
    // Count before expanding, so that a whole-sheet range fails without
    // allocating an address for every cell.
    let count = ranges.iter().fold(0u64, |count, range| {
        count.saturating_add(range.cell_count())
    });
    if count > MAX_GET_CELLS {
        return Err(Failure::invalid(
            "get",
            format!(
                "too many cells: the CELL arguments name {count} cells, but one get reads at most {MAX_GET_CELLS}"
            ),
        ));
    }
    let mut cells = Vec::with_capacity(count as usize);
    cells.extend(ranges.into_iter().flat_map(CellRange::cells));
    let workbook = Source::new(line.command, &args[0])?.open()?;
    let view = workbook.prepare_sheet(sheet)?;
    write_read_cells(
        &view,
        &cells,
        output,
        CellOutput {
            sheet,
            json: line.flag,
            records: false,
            escape: Escape::values(terminal),
        },
    )
}

fn cmd_cells(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let args = line.positionals(
        &[Slot::Path("INPUT"), Slot::Text, Slot::Range],
        2,
        "sheetpatch cells [--json] INPUT SHEET [RANGE]",
    )?;
    let sheet = line.text(1)?;
    let range = args
        .get(2)
        .map(|_| parse_range("cells", line.text(2)?))
        .transpose()?;
    let workbook = Source::new(line.command, &args[0])?.open()?;
    let view = workbook.prepare_sheet(sheet)?;
    // Filter the worksheet's own cells; a large range is never expanded.
    let cells: Vec<CellRef> = view
        .cells()
        .filter(|&cell| range.is_none_or(|range| range.contains(cell)))
        .collect();
    write_read_cells(
        &view,
        &cells,
        output,
        CellOutput {
            sheet,
            json: line.flag,
            records: true,
            escape: Escape::values(terminal),
        },
    )
}

fn cmd_set(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let args = line.positionals(
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
    )?;
    let sheet = line.text(2)?;
    let cell = line.text(3)?;
    let kind = line.text(4)?;
    let value = args.get(5).map(|_| line.text(5)).transpose()?;
    let cell = parse_cell(cell).map_err(|message| Failure::invalid("set", message))?;
    let value = parse_value(kind, value).map_err(|error| match error {
        ValueError::Arity(message) => Failure::Usage(message),
        ValueError::Invalid(message) => Failure::invalid("set", message),
    })?;
    let edit = CellEdit::at(sheet, cell, value)
        .map_err(|error| Failure::invalid("set", error.to_string()))?;
    let source = Source::new(line.command, &args[0])?;
    let destination = Destination::new(line.command, &args[1], terminal)?;
    let mut workbook = source.open()?;
    workbook.apply_edits([edit])?;
    destination.save(&workbook, false, output)
}

fn cmd_patch(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let (input, destination, patch) = if line.flag {
        let args = line.positionals(
            &[Slot::Path("INPUT"), Slot::Path("PATCH.tsv")],
            2,
            "sheetpatch patch --check INPUT PATCH.tsv",
        )?;
        (&args[0], None, &args[1])
    } else {
        let args = line.positionals(
            &[
                Slot::Path("INPUT"),
                Slot::Path("OUTPUT"),
                Slot::Path("PATCH.tsv"),
            ],
            3,
            "sheetpatch patch INPUT OUTPUT PATCH.tsv",
        )?;
        (&args[0], Some(&args[1]), &args[2])
    };
    if input == "-" && patch == "-" {
        return Err(Failure::Usage(
            "INPUT and PATCH.tsv cannot both read standard input (-)".into(),
        ));
    }
    let source = Source::new(line.command, input)?;
    let destination = destination
        .map(|arg| Destination::new(line.command, arg, terminal))
        .transpose()?;
    let Patch {
        edits,
        lines,
        row_count: count,
    } = read_patch_arg(patch)?;
    let mut workbook = source.open()?;
    // Move the values into the transaction instead of keeping a second copy
    // of every text value in a large patch; only targets and lines remain.
    workbook
        .apply_edits(edits)
        .map_err(|error| match lines.line_for(&error) {
            Some(line_number) => Failure::Context(format!("Patch line {line_number}"), error),
            None => Failure::Workbook(error),
        })?;
    match destination {
        Some(destination) => destination.save(&workbook, false, output),
        None => {
            workbook.write_to(io::sink())?;
            let noun = if count == 1 { "row" } else { "rows" };
            writeln!(output, "Validated {count} patch {noun}.")?;
            Ok(())
        }
    }
}

fn cmd_compact(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let args = line.positionals(
        &[Slot::Path("INPUT"), Slot::Path("OUTPUT")],
        2,
        "sheetpatch compact INPUT OUTPUT",
    )?;
    let source = Source::new(line.command, &args[0])?;
    let destination = Destination::new(line.command, &args[1], terminal)?;
    let workbook = source.open()?;
    destination.save(&workbook, true, output)
}

/// Run one command. `terminal` reports whether `output` is a terminal.
fn run(args: &[OsString], output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let Some(name) = args.get(1) else {
        return Err(Failure::Usage("missing command".into()));
    };
    let command = match name.to_str() {
        Some("help") => return cmd_help(args, output),
        Some(flag @ ("--help" | "-h" | "--version" | "-V")) => {
            if let Some(extra) = args.get(2) {
                return Err(Failure::Usage(format!(
                    "unexpected argument {extra:?} after {flag}"
                )));
            }
            if matches!(flag, "--help" | "-h") {
                writeln!(output, "{USAGE}")?;
            } else {
                writeln!(output, "sheetpatch {}", env!("CARGO_PKG_VERSION"))?;
            }
            return Ok(());
        }
        Some(name) => Command::parse(name),
        None => None,
    };
    let Some(command) = command else {
        let kind = if is_option(name) { "option" } else { "command" };
        return Err(Failure::Usage(format!("unknown {kind} {name:?}")));
    };
    let line = CommandLine::parse(command, args)?;
    if line.help {
        if let Some(help) = command_help(command.name()) {
            writeln!(output, "{help}")?;
        }
        return Ok(());
    }
    match command {
        Command::List => cmd_list(&line, output, terminal),
        Command::Get => cmd_get(&line, output, terminal),
        Command::Cells => cmd_cells(&line, output, terminal),
        Command::Set => cmd_set(&line, output, terminal),
        Command::Patch => cmd_patch(&line, output, terminal),
        Command::Compact => cmd_compact(&line, output, terminal),
    }
}

fn write_failure(output: &mut impl Write, failure: &Failure, terminal: bool) -> io::Result<()> {
    // Diagnostics can contain paths and workbook metadata. Keep their control
    // characters from reaching a terminal, while retaining help line breaks.
    writeln!(
        output,
        "sheetpatch: {}",
        Escaped(failure, Escape::values(terminal))
    )
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().collect();
    let stdout = io::stdout();
    let terminal = stdout.is_terminal();
    let mut output = BufWriter::new(stdout.lock());
    let result =
        run(&args, &mut output, terminal).and_then(|()| output.flush().map_err(Failure::Output));
    // Discard anything still buffered after a failure instead of flushing a
    // partial result behind the error message.
    let _ = output.into_parts();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // A reader such as `head` may close the pipe once it has enough output.
        Err(Failure::Output(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
            ExitCode::SUCCESS
        }
        Err(failure) => {
            let stderr = io::stderr();
            let _ = write_failure(&mut stderr.lock(), &failure, stderr.is_terminal());
            ExitCode::from(failure.exit_code())
        }
    }
}

// The integration tests' archive builder and temporary directories.
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod common;

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/libreoffice.xlsx"
    );
    const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

    fn args(values: &[&str]) -> Vec<OsString> {
        std::iter::once("sheetpatch")
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
                    "<Relationship Id='s{index}' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet{index}.xml'/>"
                )
            })
            .collect();
        let workbook = format!(
            "<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheets>{sheets}</sheets></workbook>"
        );
        let root = format!(
            "<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='office' Type='{OFFICE_NS}/officeDocument' Target='xl/workbook.xml'/></Relationships>"
        );
        let rels = format!("<Relationships xmlns='{PACKAGE_NS}'>{relationships}</Relationships>");
        let worksheet = format!("<worksheet xmlns='{MAIN_NS}'><sheetData/></worksheet>");
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
            ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
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
                assert_eq!(command, "patch");
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
                match run(&args, &mut Vec::new(), false) {
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
        let text = "a\u{1b}]0;title\u{7}\u{1b}[2J\tb\nc\r\0\u{7f}\u{85}\u{9b}\u{a0}é😀";
        assert_eq!(Escaped(text, Escape::values(false)).to_string(), text);
        assert_eq!(Escaped(text, Escape::names(false)).to_string(), text);
        assert_eq!(
            Escaped(text, Escape::values(true)).to_string(),
            "a\\u{1b}]0;title\\u{7}\\u{1b}[2J\tb\nc\\u{d}\\u{0}\\u{7f}\\u{85}\\u{9b}\u{a0}é😀"
        );
        // Names are listed one per line, so tabs and line feeds are escaped too.
        assert_eq!(
            Escaped(text, Escape::names(true)).to_string(),
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
            assert_eq!(Escaped(&value, Escape::values(true)).to_string(), expected);
            assert_eq!(
                Escaped(&value, Escape::values(false)).to_string(),
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
        std::fs::write(
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
        let failure =
            run(&args(&["list", path.to_str().unwrap()]), &mut output, false).unwrap_err();
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
            write_failure(&mut plain, &failure, false).unwrap();
            assert_eq!(
                String::from_utf8(plain).unwrap(),
                format!("sheetpatch: {failure}\n")
            );
            let mut terminal = Vec::new();
            write_failure(&mut terminal, &failure, true).unwrap();
            let terminal = String::from_utf8(terminal).unwrap();
            assert!(terminal.contains("\\u{1b}"), "{terminal:?}");
            assert!(
                terminal
                    .chars()
                    .all(|ch| !ch.is_control() || matches!(ch, '\t' | '\n')),
                "{terminal:?}"
            );
        }
        let failure = Failure::Usage("expected INPUT\tOUTPUT".into());
        let mut terminal = Vec::new();
        write_failure(&mut terminal, &failure, true).unwrap();
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
                true,
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
    fn terminal_lists_escape_every_control_in_sheet_names() {
        let directory = common::TempDir::new();
        let path = directory.join("names.xlsx");
        std::fs::write(
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
        run(&args(&["list", "--json", path]), &mut json, true).unwrap();
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
            match run(&args(&command), &mut output, true) {
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
            match run(&args(&command), &mut FailedWriter, false) {
                Err(Failure::Output(error)) => {
                    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                }
                other => panic!("unexpected output result for {command:?}: {other:?}"),
            }
        }
        let directory = common::TempDir::new();
        let patch = directory.join("p.tsv");
        std::fs::write(&patch, "Data\tA1\ttext\tx\n").unwrap();
        for command in [
            vec!["compact", FIXTURE, "-"],
            vec!["set", FIXTURE, "-", "Data", "A1", "text", "x"],
            vec!["patch", FIXTURE, "-", patch.to_str().unwrap()],
        ] {
            // A truncated workbook is a failure, even behind a closed pipe.
            match run(&args(&command), &mut FailedWriter, false) {
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
            Workbook::from_bytes(include_bytes!("../tests/fixtures/libreoffice.xlsx").to_vec())
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
            sheetpatch::Error::Xml(
                "worksheet \"Data\": A1: cell has multiple value payloads".into(),
            ),
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
        let source = "\u{feff}# comment\r\n\r\nData & Notes\tB2\ttext\t  left\tright  \r\n#Data\tB3\ttext\t\r\n";
        let patch = read_patch(source.as_bytes(), "test").unwrap();
        assert_eq!(patch.row_count, 2);
        assert_eq!(patch.edits.len(), 2);
        let edit = &patch.edits[0];
        assert_eq!(patch.lines.lines[0], (3, 3));
        assert_eq!(edit.sheet(), "Data & Notes");
        assert_eq!(edit.cell().to_string(), "B2");
        assert_eq!(edit.value(), &CellValue::Text("  left\tright  ".into()));
        let edit = &patch.edits[1];
        assert_eq!(patch.lines.lines[1], (4, 4));
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
}
