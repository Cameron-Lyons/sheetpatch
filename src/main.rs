#![forbid(unsafe_code)]

use std::{
    collections::HashSet,
    env,
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io::{self, BufRead, BufReader, BufWriter, IsTerminal, Read, Write},
    path::Path,
    process::ExitCode,
};

use sheetpatch::{CellContent, CellEdit, CellRef, CellValue, Sheet, Workbook};

const USAGE: &str = "Usage:\n  sheetpatch list [--json] INPUT\n  sheetpatch get [--json] INPUT SHEET CELL [CELL ...]\n  sheetpatch set INPUT OUTPUT SHEET CELL text|number|bool|error VALUE\n  sheetpatch set INPUT OUTPUT SHEET CELL blank\n  sheetpatch patch INPUT OUTPUT PATCH.tsv\n  sheetpatch patch --check INPUT PATCH.tsv\n  sheetpatch compact INPUT OUTPUT\n  sheetpatch help [COMMAND]\n  sheetpatch --help\n  sheetpatch --version\n\nOptions go right after the command. Write a file whose name starts with - as ./NAME.\nUse - as INPUT to read the workbook from stdin, or as OUTPUT to write it to stdout when stdout is not a terminal.\nPatch rows: SHEET<TAB>CELL<TAB>TYPE<TAB>VALUE. Blank needs no value.\nUse - as PATCH.tsv to read stdin; INPUT and PATCH.tsv cannot both be -. Use LF or CRLF line endings.\nEmpty lines are ignored. Lines starting with # without tabs are comments.\n--check validates a patch against the workbook without saving.\nQuote sheet names and text values containing spaces. Formats: .xlsx, .xlsm, .xltx, .xltm.\nget prints each stored value in address order; formula results are cached and are not recalculated.\n--json returns an array: list includes names and paths; get includes types, values, formulas and styles.\nOn a terminal, plain list and get output shows control characters as \\u{..} escapes; --json is exact.";

fn command_help(command: &str) -> Option<&'static str> {
    match command {
        "help" => Some(USAGE),
        "list" => Some(
            "Usage: sheetpatch list [--json] INPUT\n\nList editable worksheets in workbook order. --json includes each worksheet's name and package path. Use - as INPUT to read the workbook from standard input. On a terminal, control characters in names are shown as \\u{..} escapes; --json output is exact.",
        ),
        "get" => Some(
            "Usage: sheetpatch get [--json] INPUT SHEET CELL [CELL ...]\n\nRead A1 addresses in input order with one worksheet parse. Names are case-sensitive; addresses accept lowercase letters. Formula results are stored caches, without recalculation. --json includes normalized addresses, scalar types, values, formulas and style indices. Blank values use null in JSON and an empty line in plain output. Use - as INPUT to read the workbook from standard input. On a terminal, plain output shows control characters as \\u{..} escapes; --json output is exact.",
        ),
        "set" => Some(
            "Usage:\n  sheetpatch set INPUT OUTPUT SHEET CELL text|number|bool|error VALUE\n  sheetpatch set INPUT OUTPUT SHEET CELL blank\n\nSet one scalar cell value and save atomically. Text stays text even when it starts with =. Numbers must be finite; booleans are true or false; error tokens include #N/A and #DIV/0!. Blank clears the value while retaining formatting. Formula cells and merged-range followers are protected. INPUT and OUTPUT may be the same path. Use - as INPUT to read the workbook from standard input, or as OUTPUT to write it to standard output when that is not a terminal.",
        ),
        "patch" => Some(
            "Usage:\n  sheetpatch patch INPUT OUTPUT PATCH.tsv\n  sheetpatch patch --check INPUT PATCH.tsv\n\nApply a transaction of tab-separated SHEET, CELL, TYPE and VALUE fields. Types: text, number, bool, error, blank. Blank may omit VALUE. Duplicate cells use the last value. Text retains extra tabs and trailing whitespace. UTF-8 BOM and CRLF files are accepted; a carriage return inside a row is rejected. Empty lines and tab-free lines starting with # are ignored. Use - as PATCH.tsv to read standard input. Use - as INPUT to read the workbook from standard input, or as OUTPUT to write it to standard output when that is not a terminal; INPUT and PATCH.tsv cannot both be -. --check validates the complete transaction and output generation without saving; success reports the number of patch rows. Check and apply both use the same edit protections and ZIP limits.",
        ),
        "compact" => Some(
            "Usage: sheetpatch compact INPUT OUTPUT\n\nRemove safely recognized obsolete ZIP records and save atomically. Unknown archive prefixes or gaps are protected. Compaction retains untouched active records, does not calculate formulas, and does not securely erase copies or filesystem history. INPUT and OUTPUT may be the same path. Use - as INPUT to read the workbook from standard input, or as OUTPUT to write it to standard output when that is not a terminal.",
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
    /// A workbook error, prefixed with the operation and path that failed.
    Context(String, sheetpatch::Error),
    /// A request names something the workbook does not contain.
    Rejected(String),
    /// Writing to standard output failed.
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
            Self::Workbook(_) | Self::Context(..) | Self::Rejected(_) | Self::Output(_) => 1,
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
            Self::Rejected(message) => f.write_str(message),
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
    Set,
    Patch,
    Compact,
}

impl Command {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "list" => Some(Self::List),
            "get" => Some(Self::Get),
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
    /// A cell address. A trailing `Cell` slot repeats.
    Cell,
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
                Some("--help" | "-h") => line.help = true,
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
                Some(Slot::Cell) if self.command.synopsis_for(arg).is_some() => {
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
            Self::Stdin => {
                let context = || "cannot read workbook from standard input".to_owned();
                let mut bytes = Vec::new();
                io::stdin()
                    .lock()
                    .read_to_end(&mut bytes)
                    .map_err(|error| Failure::Context(context(), error.into()))?;
                Workbook::from_bytes(bytes).map_err(|error| Failure::Context(context(), error))
            }
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
                written.map_err(|error| match error {
                    sheetpatch::Error::Io(error) => Failure::Output(error),
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

fn sheet_not_found(sheet: &str) -> String {
    format!("worksheet not found: {sheet:?}")
}

fn require_sheet(workbook: &Workbook, sheet: &str) -> Result<(), Failure> {
    if workbook
        .sheets()
        .iter()
        .any(|candidate| candidate.name() == sheet)
    {
        Ok(())
    } else {
        Err(Failure::Rejected(sheet_not_found(sheet)))
    }
}

/// Read patch rows with their one-based line numbers. `origin` names the
/// source in I/O error messages.
fn read_patch(mut reader: impl BufRead, origin: &str) -> Result<Vec<(usize, CellEdit)>, Failure> {
    let mut rows = Vec::new();
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
        // A carriage return ends a line only before a line feed. Followed by
        // more data, it would merge classic Mac lines into one text value. A
        // final carriage return without a line feed remains part of the value.
        if line.strip_suffix('\r').unwrap_or(line).contains('\r') {
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
        rows.push((line_number, edit));
    }
    Ok(rows)
}

fn read_patch_arg(path: &OsStr) -> Result<Vec<(usize, CellEdit)>, Failure> {
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

/// Write one line of plain output. On a terminal, C0 controls other than tab
/// and line feed, DEL, and C1 controls are shown as `\u{..}` escapes so text
/// from a workbook cannot move the cursor, clear the screen, or retitle the
/// window. Pipes and files receive the exact text.
fn write_plain_line(output: &mut impl Write, text: &str, terminal: bool) -> io::Result<()> {
    let mut start = 0;
    if terminal {
        for (index, ch) in text.char_indices() {
            if ch.is_control() && !matches!(ch, '\t' | '\n') {
                output.write_all(&text.as_bytes()[start..index])?;
                write!(output, "\\u{{{:x}}}", ch as u32)?;
                start = index + ch.len_utf8();
            }
        }
    }
    output.write_all(&text.as_bytes()[start..])?;
    output.write_all(b"\n")
}

fn write_scalar(output: &mut impl Write, value: &CellValue, terminal: bool) -> io::Result<()> {
    match value {
        CellValue::Text(value) | CellValue::Error(value) => {
            write_plain_line(output, value, terminal)
        }
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
            output.write_all(b"}")
        })?;
    } else {
        for sheet in sheets {
            write_plain_line(output, sheet.name(), terminal)?;
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
    let cells = (2..args.len())
        .map(|index| {
            parse_cell(line.text(index)?).map_err(|message| Failure::invalid("get", message))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let workbook = Source::new(line.command, &args[0])?.open()?;
    require_sheet(&workbook, sheet)?;
    let contents = workbook.get_cells_at(sheet, cells.iter().copied())?;
    if line.flag {
        write_json_array(
            output,
            cells.iter().zip(&contents),
            |output, (cell, content)| write_json_cell(output, sheet, *cell, content),
        )?;
    } else {
        for content in &contents {
            write_scalar(output, &content.value, terminal)?;
        }
    }
    Ok(())
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
    require_sheet(&workbook, sheet)?;
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
    let rows = read_patch_arg(patch)?;
    let mut workbook = source.open()?;
    let sheets: HashSet<&str> = workbook.sheets().iter().map(Sheet::name).collect();
    if let Some((line_number, edit)) = rows.iter().find(|(_, edit)| !sheets.contains(edit.sheet()))
    {
        return Err(Failure::Rejected(format!(
            "Patch line {line_number}: {}",
            sheet_not_found(edit.sheet())
        )));
    }
    let count = rows.len();
    workbook.apply_edits(rows.into_iter().map(|(_, edit)| edit))?;
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
        Command::Set => cmd_set(&line, output, terminal),
        Command::Patch => cmd_patch(&line, output, terminal),
        Command::Compact => cmd_compact(&line, output, terminal),
    }
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
            let _ = writeln!(io::stderr().lock(), "sheetpatch: {failure}");
            ExitCode::from(failure.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/libreoffice.xlsx"
    );

    fn args(values: &[&str]) -> Vec<OsString> {
        std::iter::once("sheetpatch")
            .chain(values.iter().copied())
            .map(OsString::from)
            .collect()
    }

    fn patch_values(source: &str) -> Vec<CellValue> {
        read_patch(source.as_bytes(), "test")
            .unwrap()
            .into_iter()
            .map(|(_, edit)| edit.value().clone())
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
            (
                vec!["sheetpatch", "set", "", "", "Data", "A1", "text", "value"],
                4..8,
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
    }

    #[test]
    fn terminal_output_escapes_controls_and_other_output_is_exact() {
        let text = "a\u{1b}]0;title\u{7}\u{1b}[2J\tb\nc\r\0\u{7f}\u{85}\u{9b}\u{a0}é😀";
        let mut exact = Vec::new();
        write_plain_line(&mut exact, text, false).unwrap();
        assert_eq!(exact, format!("{text}\n").as_bytes());
        let mut escaped = Vec::new();
        write_plain_line(&mut escaped, text, true).unwrap();
        assert_eq!(
            String::from_utf8(escaped).unwrap(),
            "a\\u{1b}]0;title\\u{7}\\u{1b}[2J\tb\nc\\u{d}\\u{0}\\u{7f}\\u{85}\\u{9b}\u{a0}é😀\n"
        );
    }

    #[test]
    fn terminal_reads_escape_stored_controls_but_json_stays_exact() {
        let directory = env::temp_dir().join(format!("sheetpatch-main-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
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
        }
        let mut json = Vec::new();
        run(
            &args(&["get", "--json", path, "Notes", "A2"]),
            &mut json,
            true,
        )
        .unwrap();
        assert!(
            String::from_utf8(json)
                .unwrap()
                .contains("\"x\\r\u{9b}2J\u{7f}\\ty\"")
        );
        std::fs::remove_dir_all(directory).unwrap();
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
            vec!["--version"],
            vec!["get", FIXTURE, "Data", "A1"],
            vec!["list", "--json", FIXTURE],
            vec!["compact", FIXTURE, "-"],
        ] {
            match run(&args(&command), &mut FailedWriter, false) {
                Err(Failure::Output(error)) => {
                    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                }
                other => panic!("unexpected output result for {command:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn patch_preserves_text_tabs_whitespace_and_empty_values() {
        let source = "\u{feff}# comment\r\n\r\nData & Notes\tB2\ttext\t  left\tright  \r\n#Data\tB3\ttext\t\r\n";
        let rows = read_patch(source.as_bytes(), "test").unwrap();
        assert_eq!(rows.len(), 2);
        let (line, edit) = &rows[0];
        assert_eq!(*line, 3);
        assert_eq!(edit.sheet(), "Data & Notes");
        assert_eq!(edit.cell().to_string(), "B2");
        assert_eq!(edit.value(), &CellValue::Text("  left\tright  ".into()));
        let (line, edit) = &rows[1];
        assert_eq!(*line, 4);
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
        assert_eq!(edits[0].1.value(), &CellValue::Text("value\r".into()));
        // A carriage return before a CRLF terminator is likewise the last byte.
        assert_eq!(
            patch_values("Data\tA1\ttext\tvalue\r\r\n"),
            vec![CellValue::Text("value\r".into())]
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
