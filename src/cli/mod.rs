mod args;
mod error;
mod output;
mod patch;
mod streams;

use self::args::{Command, CommandLine, USAGE, ValueError};
use self::error::Failure;
use self::output::{CellOutput, Escape};
use self::patch::Patch;
use self::streams::{Destination, ObsoleteRecords, Source};
use sheetpatch::{CellEdit, CellRange};
use std::env;
use std::ffi::OsString;
use std::io::{self, BufWriter, Write};
use std::process::ExitCode;

/// The most cells one `get` reads, counting every cell of each range.
const MAX_GET_CELLS: u64 = 1_048_576;
/// The output `get` and `cells` buffer while validating.
const READ_BUFFER_BYTES: usize = 4 * 1024 * 1024;

fn run_help(args: &[OsString], output: &mut impl Write) -> Result<(), Failure> {
    let help = match &args[2..] {
        [] => USAGE,
        [topic] => match topic.to_str() {
            Some("--help" | "-h") => USAGE,
            Some(topic) => args::command_help(topic)
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

fn run_list(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let args = line.positionals()?;
    let workbook = Source::try_new(line.command, &args[0])?.open()?;
    let sheets = workbook.sheets();
    if line.flag {
        output::write_json_array(output, sheets, |output, sheet| {
            output.write_all(b"{\"name\":")?;
            output::write_json_string(output, sheet.name())?;
            output.write_all(b",\"path\":")?;
            output::write_json_string(output, sheet.path())?;
            let visibility = sheet.visibility().as_str();
            write!(output, ",\"visibility\":\"{visibility}\"}}")
        })?;
    } else {
        let escape = Escape::names(terminal);
        for sheet in sheets {
            let name = output::Escaped {
                value: sheet.name(),
                escape,
            };
            writeln!(output, "{name}")?;
        }
    }
    Ok(())
}

fn run_get(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let args = line.positionals()?;
    let sheet = line.text(1)?;
    let ranges: Vec<CellRange> = (2..args.len())
        .map(|index| args::parse_range("get", line.text(index)?))
        .collect::<Result<_, _>>()?;
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
    let workbook = Source::try_new(line.command, &args[0])?.open()?;
    let view = workbook.prepare_sheet(sheet)?;
    output::write_read_cells(
        &view,
        &cells,
        READ_BUFFER_BYTES,
        output,
        CellOutput {
            sheet,
            json: line.flag,
            records: false,
            escape: Escape::values(terminal),
        },
    )
}

fn run_cells(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let args = line.positionals()?;
    let sheet = line.text(1)?;
    let range = args
        .get(2)
        .map(|_| args::parse_range("cells", line.text(2)?))
        .transpose()?;
    let workbook = Source::try_new(line.command, &args[0])?.open()?;
    let view = workbook.prepare_sheet(sheet)?;
    // Iterate the existing indexed cells directly. Filtering precedes value
    // decoding, and a large range is never expanded into missing addresses.
    output::write_cells(
        view.cell_contents_in(range),
        READ_BUFFER_BYTES,
        output,
        CellOutput {
            sheet,
            json: line.flag,
            records: true,
            escape: Escape::values(terminal),
        },
    )
}

fn run_set(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let args = line.positionals()?;
    let sheet = line.text(2)?;
    let cell = line.text(3)?;
    let kind = line.text(4)?;
    let value = args.get(5).map(|_| line.text(5)).transpose()?;
    let cell = args::parse_cell(cell).map_err(|message| Failure::invalid("set", message))?;
    let value = args::parse_value(kind, value).map_err(|error| match error {
        ValueError::Arity(message) => Failure::Usage(message),
        ValueError::Invalid(message) => Failure::invalid("set", message),
    })?;
    let edit = CellEdit::try_new_at(sheet, cell, value)
        .map_err(|error| Failure::invalid("set", error.to_string()))?;
    let source = Source::try_new(line.command, &args[0])?;
    let destination = Destination::try_new(line.command, &args[1], terminal)?;
    let mut workbook = source.open()?;
    workbook.apply_edits([edit])?;
    destination.save(&workbook, ObsoleteRecords::Keep, output)
}

fn run_patch(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let (input, destination, patch) = if line.flag {
        let args = line.positionals()?;
        (&args[0], None, &args[1])
    } else {
        let args = line.positionals()?;
        (&args[0], Some(&args[1]), &args[2])
    };
    if input == "-" && patch == "-" {
        return Err(Failure::Usage(
            "INPUT and PATCH.tsv cannot both read standard input (-)".into(),
        ));
    }
    let source = Source::try_new(line.command, input)?;
    let destination = destination
        .map(|arg| Destination::try_new(line.command, arg, terminal))
        .transpose()?;
    let Patch {
        edits,
        lines,
        row_count: count,
    } = patch::read_patch_arg(patch)?;
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
        Some(destination) => destination.save(&workbook, ObsoleteRecords::Keep, output),
        None => {
            workbook.write_to(io::sink())?;
            let noun = if count == 1 { "row" } else { "rows" };
            writeln!(output, "Validated {count} patch {noun}.")?;
            Ok(())
        }
    }
}

fn run_compact(line: &CommandLine, output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let args = line.positionals()?;
    let source = Source::try_new(line.command, &args[0])?;
    let destination = Destination::try_new(line.command, &args[1], terminal)?;
    let workbook = source.open()?;
    destination.save(&workbook, ObsoleteRecords::Remove, output)
}

/// Run one command. `terminal` reports whether `output` is a terminal.
fn run(args: &[OsString], output: &mut impl Write, terminal: bool) -> Result<(), Failure> {
    let Some(name) = args.get(1) else {
        return Err(Failure::Usage("missing command".into()));
    };
    let command = match name.to_str() {
        Some("help") => return run_help(args, output),
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
        let kind = if args::is_option(name) {
            "option"
        } else {
            "command"
        };
        return Err(Failure::Usage(format!("unknown {kind} {name:?}")));
    };
    let line = CommandLine::parse(command, args)?;
    if line.help {
        if let Some(help) = args::command_help(command.name()) {
            writeln!(output, "{help}")?;
        }
        return Ok(());
    }
    match command {
        Command::List => run_list(&line, output, terminal),
        Command::Get => run_get(&line, output, terminal),
        Command::Cells => run_cells(&line, output, terminal),
        Command::Set => run_set(&line, output, terminal),
        Command::Patch => run_patch(&line, output, terminal),
        Command::Compact => run_compact(&line, output, terminal),
    }
}

pub(super) fn main() -> ExitCode {
    use std::io::IsTerminal as _;

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
            let _ = error::write_failure(&mut stderr.lock(), &failure, stderr.is_terminal());
            ExitCode::from(failure.exit_code())
        }
    }
}

// The integration tests' independent archive builder and temporary directories.
#[cfg(test)]
#[path = "../../tests/common/mod.rs"]
mod common;

#[cfg(test)]
mod tests;
