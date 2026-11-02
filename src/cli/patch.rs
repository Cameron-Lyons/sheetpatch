use super::args::{self, ValueError};
use super::error::Failure;
use super::streams;
use sheetpatch::{CellEdit, CellRef};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::str;

/// Includes a row's BOM and line ending. Excel's longest valid text needs at
/// most 98,301 UTF-8 bytes; 1 MiB leaves generous room for unusual sheet names
/// and other fields while bounding memory even for malformed input/comments.
const MAX_ROW_BYTES: usize = 1024 * 1024;

enum RowError {
    TooLong,
    Io(io::Error),
}

/// Read only enough input to accept one bounded row or reject it. Unlike
/// `read_line`, this never grows the row buffer past its budget, and checks
/// the budget before UTF-8 decoding so a split character cannot hide overflow.
fn read_row(reader: &mut impl BufRead, row: &mut Vec<u8>) -> Result<(), RowError> {
    row.clear();
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(RowError::Io(error)),
        };
        if available.is_empty() {
            return Ok(());
        }
        let remaining = MAX_ROW_BYTES - row.len();
        let examined = &available[..available.len().min(remaining + 1)];
        let newline = examined.iter().position(|&byte| byte == b'\n');
        let count = newline.map_or(examined.len(), |index| index + 1);
        if count > remaining {
            return Err(RowError::TooLong);
        }
        let needed = row.len() + count;
        if needed > row.capacity() {
            let capacity = row
                .capacity()
                .saturating_mul(2)
                .max(256)
                .max(needed)
                .min(MAX_ROW_BYTES);
            row.try_reserve_exact(capacity - row.len())
                .map_err(|error| RowError::Io(io::Error::new(io::ErrorKind::OutOfMemory, error)))?;
        }
        row.extend_from_slice(&examined[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(());
        }
    }
}

/// The first and last patch lines that target one edit's cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EditLines {
    pub(super) first: usize,
    pub(super) last: usize,
}

/// The patch lines of each unique edit, kept for diagnostics after the
/// edits move into the transaction.
#[derive(Debug, Default)]
pub(super) struct PatchLines {
    /// The lines of each edit, by edit index.
    pub(super) lines: Vec<EditLines>,
    /// The index of each edit by worksheet name and cell.
    targets: HashMap<String, HashMap<CellRef, usize>>,
}

impl PatchLines {
    /// The line to report with a refused transaction: the first line naming a
    /// missing worksheet, or the last line supplying a protected cell's value.
    pub(super) fn line_for(&self, error: &sheetpatch::Error) -> Option<usize> {
        match error {
            // Edits are indexed in first-occurrence order, so a worksheet's
            // lowest edit index began on the first line naming it.
            sheetpatch::Error::SheetNotFound(sheet) => {
                let first_edit = self.targets.get(sheet)?.values().min()?;
                Some(self.lines[*first_edit].first)
            }
            sheetpatch::Error::ProtectedCell { sheet, cell, .. } => {
                let index = self.targets.get(sheet)?.get(cell)?;
                Some(self.lines[*index].last)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Patch {
    // Retain unique targets in their first occurrence's order, with the last
    // value; `lines` holds both diagnostic line numbers.
    pub(super) edits: Vec<CellEdit>,
    pub(super) lines: PatchLines,
    pub(super) row_count: usize,
}

/// Validate every patch row, retaining only each target's final value.
/// `origin` names the source in I/O error messages.
pub(super) fn read_patch(mut reader: impl BufRead, origin: &str) -> Result<Patch, Failure> {
    let mut patch = Patch::default();
    let mut row = Vec::new();
    let mut line_number = 0usize;
    loop {
        line_number += 1;
        let invalid = |message: String| {
            Failure::invalid("patch", format!("Patch line {line_number}: {message}"))
        };
        match read_row(&mut reader, &mut row) {
            Ok(()) if row.is_empty() => break,
            Ok(()) => (),
            Err(RowError::TooLong) => {
                return Err(invalid(format!(
                    "row exceeds the {MAX_ROW_BYTES}-byte (1 MiB) limit, including any BOM and line ending"
                )));
            }
            Err(RowError::Io(error)) => {
                return Err(Failure::Context(
                    format!("cannot read patch {origin}"),
                    error.into(),
                ));
            }
        }
        let mut line = str::from_utf8(&row)
            .map_err(|error| invalid(format!("expected UTF-8 text: {error}")))?;
        let terminated = line.ends_with('\n');
        if terminated {
            line = &line[..line.len() - 1];
            line = line.strip_suffix('\r').unwrap_or(line);
        }
        let line = if line_number == 1 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
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
        let cell = args::parse_cell(cell).map_err(invalid)?;
        let value = args::parse_value(kind, fields.next()).map_err(|error| match error {
            ValueError::Arity(message) | ValueError::Invalid(message) => invalid(message),
        })?;
        let edit =
            CellEdit::try_new_at(sheet, cell, value).map_err(|error| invalid(error.to_string()))?;
        patch.row_count += 1;
        let lines = &mut patch.lines;
        if let Some(&index) = lines.targets.get(sheet).and_then(|cells| cells.get(&cell)) {
            lines.lines[index].last = line_number;
            patch.edits[index] = edit;
        } else {
            lines
                .targets
                .entry(sheet.to_owned())
                .or_default()
                .insert(cell, patch.edits.len());
            lines.lines.push(EditLines {
                first: line_number,
                last: line_number,
            });
            patch.edits.push(edit);
        }
    }
    Ok(patch)
}

pub(super) fn read_patch_arg(path: &OsStr) -> Result<Patch, Failure> {
    if path == "-" {
        return read_patch(io::stdin().lock(), "from standard input");
    }
    let origin = streams::quoted_path(path);
    let file = File::open(path)
        .map_err(|error| Failure::Context(format!("cannot read patch {origin}"), error.into()))?;
    read_patch(BufReader::new(file), &origin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn oversized_rows_stop_reading_at_the_budget() {
        // A small reader buffer exercises accumulation across many reads;
        // repeat/take supplies oversized input without allocating it first.
        let input = io::repeat(b'x').take((MAX_ROW_BYTES + 64) as u64);
        let mut reader = BufReader::with_capacity(17, input);
        let mut row = Vec::new();
        assert!(matches!(
            read_row(&mut reader, &mut row),
            Err(RowError::TooLong)
        ));
        assert!(row.len() <= MAX_ROW_BYTES);
        assert!(row.capacity() <= MAX_ROW_BYTES);
        assert!(
            reader.get_ref().limit() > 0,
            "the oversized suffix remains unread"
        );
    }

    #[test]
    fn row_limit_includes_bom_and_line_endings() {
        for ending in ["", "\n", "\r\n"] {
            let mut row = b"\xef\xbb\xbf#".to_vec();
            row.resize(MAX_ROW_BYTES - ending.len(), b'x');
            row.extend_from_slice(ending.as_bytes());
            let patch = read_patch(row.as_slice(), "test").unwrap();
            assert_eq!(patch.row_count, 0);
            row.insert(4, b'x');
            let error = read_patch(row.as_slice(), "test").unwrap_err().to_string();
            assert!(error.contains("Patch line 1: row exceeds"), "{error}");
        }
    }

    #[test]
    fn multibyte_text_and_crlf_can_cross_reader_boundaries() {
        let input = "\u{feff}Data\tA1\ttext\t雪😀\traw\r\nData\tA2\ttext\tlast\r";
        for capacity in 1..=8 {
            let reader = BufReader::with_capacity(capacity, input.as_bytes());
            let patch = read_patch(reader, "test").unwrap();
            assert_eq!(patch.row_count, 2);
            assert_eq!(patch.edits[0].value().as_text(), Some("雪😀\traw"));
            assert_eq!(patch.edits[1].value().as_text(), Some("last\r"));
        }
    }

    #[test]
    fn interrupted_reads_resume_without_losing_the_row() {
        struct InterruptedOnce<'a> {
            input: &'a [u8],
            interrupted: bool,
        }
        impl io::Read for InterruptedOnce<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.input.read(output)
            }
        }
        let reader = BufReader::new(InterruptedOnce {
            input: b"Data\tA1\ttext\tvalue\n",
            interrupted: false,
        });
        let patch = read_patch(reader, "test").unwrap();
        assert_eq!(patch.edits[0].value().as_text(), Some("value"));
    }
}
