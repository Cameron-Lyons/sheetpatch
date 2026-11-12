use super::error::Failure;
use sheetpatch::{CellContent, CellRef, CellValue, WorksheetView};
use std::fmt::{self, Display, Formatter};
use std::io::{self, Write};

pub(super) fn write_json_string(output: &mut impl Write, value: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    let mut start = 0;
    for (index, character) in value.char_indices() {
        let escape = match character {
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
            write!(output, "\\u{:04x}", character as u32)?;
        } else {
            output.write_all(escape.as_bytes())?;
        }
        start = index + character.len_utf8();
    }
    output.write_all(&value.as_bytes()[start..])?;
    output.write_all(b"\"")
}

/// Write `items` as one JSON array followed by a newline.
pub(super) fn write_json_array<W: Write, T>(
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
pub(super) enum Escape {
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
    pub(super) fn values(terminal: bool) -> Self {
        if terminal {
            Self::Controls
        } else {
            Self::Exact
        }
    }

    pub(super) fn names(terminal: bool) -> Self {
        if terminal {
            Self::AllControls
        } else {
            Self::Exact
        }
    }

    fn applies(self, character: char) -> bool {
        match self {
            Self::Exact => false,
            Self::Controls => character.is_control() && !matches!(character, '\t' | '\n'),
            Self::AllControls => character.is_control(),
        }
    }
}

/// Displays a value for plain output with the characters its `escape` selects
/// shown as `\u{..}` escapes.
pub(super) struct Escaped<T> {
    pub(super) value: T,
    pub(super) escape: Escape,
}

impl<T: Display> Display for Escaped<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        struct Escaping<'formatter, 'output> {
            formatter: &'formatter mut Formatter<'output>,
            escape: Escape,
        }

        impl fmt::Write for Escaping<'_, '_> {
            fn write_str(&mut self, text: &str) -> fmt::Result {
                let mut start = 0;
                for (index, character) in text.char_indices() {
                    if self.escape.applies(character) {
                        self.formatter.write_str(&text[start..index])?;
                        write!(self.formatter, "\\u{{{:x}}}", u32::from(character))?;
                        start = index + character.len_utf8();
                    }
                }
                self.formatter.write_str(&text[start..])
            }
        }

        match self.escape {
            Escape::Exact => Display::fmt(&self.value, f),
            escape => fmt::Write::write_fmt(
                &mut Escaping {
                    formatter: f,
                    escape,
                },
                format_args!("{}", self.value),
            ),
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

pub(super) struct CellOutput<'sheet> {
    pub(super) sheet: &'sheet str,
    pub(super) json: bool,
    /// `cells` includes addresses and types, and omits formatting-only cells.
    pub(super) records: bool,
    pub(super) escape: Escape,
}

/// Write one cell's output. `cells` output skips formatting-only cells, and
/// JSON output separates objects with commas after the first.
pub(super) fn write_cell(
    output: &mut impl Write,
    cell: CellRef,
    content: &CellContent,
    format: &CellOutput<'_>,
    first: &mut bool,
) -> io::Result<()> {
    if format.records && content.value.is_blank() && content.formula.is_none() {
        return Ok(());
    }
    if format.json {
        if !*first {
            output.write_all(b",")?;
        }
        *first = false;
        write_json_cell(output, format.sheet, cell, content)
    } else {
        let value = Escaped {
            value: &content.value,
            escape: format.escape,
        };
        if format.records {
            let kind = value_type(&content.value);
            writeln!(output, "{cell}\t{kind}\t{value}")
        } else {
            writeln!(output, "{value}")
        }
    }
}

/// The length of the stored text that a cell's output repeats: its value and,
/// in JSON, its formula. Escaping only lengthens it, so this is a lower bound.
fn stored_len(content: &CellContent, json: bool) -> usize {
    let value = match &content.value {
        CellValue::Text(text) | CellValue::Error(text) => text.len(),
        _ => 0,
    };
    let formula = match &content.formula {
        Some(formula) if json => formula.len(),
        _ => 0,
    };
    value.saturating_add(formula)
}

/// Bounds both formatted bytes and buffer growth. A cell that exceeds the
/// remaining budget is rolled back and streamed after validation instead.
struct PrefixBuffer<'buffer> {
    bytes: &'buffer mut Vec<u8>,
    budget: usize,
    full: bool,
}

impl Write for PrefixBuffer<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.budget - self.bytes.len() {
            self.full = true;
            return Err(io::ErrorKind::WriteZero.into());
        }
        let needed = self.bytes.len() + bytes.len();
        if needed > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(256)
                .max(needed)
                .min(self.budget);
            self.bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn buffer_cell(
    buffer: &mut Vec<u8>,
    budget: usize,
    cell: CellRef,
    content: &CellContent,
    format: &CellOutput<'_>,
    first: &mut bool,
) -> io::Result<bool> {
    if stored_len(content, format.json) > budget - buffer.len() {
        return Ok(false);
    }
    let start = buffer.len();
    let was_first = *first;
    let mut writer = PrefixBuffer {
        bytes: buffer,
        budget,
        full: false,
    };
    let result = write_cell(&mut writer, cell, content, format, first);
    if writer.full {
        writer.bytes.truncate(start);
        *first = was_first;
        return Ok(false);
    }
    result.map(|()| true)
}

/// Validate every requested value before emitting any output. While
/// validating, buffer complete cells up to `budget` formatted bytes;
/// only the cells after that prefix are read a second time. A cell whose
/// stored text would exceed the budget ends the prefix before it is formatted,
/// and formatting or escaping that exceeds it rolls back that cell's output.
pub(super) fn write_read_cells(
    view: &WorksheetView<'_>,
    cells: &[CellRef],
    budget: usize,
    output: &mut impl Write,
    format: CellOutput<'_>,
) -> Result<(), Failure> {
    write_cells(
        cells
            .iter()
            .map(|&cell| view.get_cell_at(cell).map(|content| (cell, content))),
        budget,
        output,
        format,
    )
}

/// Save the iterator's position immediately after the buffered prefix. The
/// output pass resumes there, without storing addresses or decoding the
/// prefix again to skip it. Clones of these iterators copy only their cursor
/// and references to immutable worksheet data.
pub(super) fn write_cells<I>(
    mut cells: I,
    budget: usize,
    output: &mut impl Write,
    format: CellOutput<'_>,
) -> Result<(), Failure>
where
    I: Iterator<Item = sheetpatch::Result<(CellRef, CellContent)>> + Clone,
{
    let mut buffer = Vec::new();
    if format.json && budget > 0 {
        PrefixBuffer {
            bytes: &mut buffer,
            budget,
            full: false,
        }
        .write_all(b"[")?;
    }
    let mut first = true;
    let mut remaining = cells.clone();
    let mut buffering = budget > 0;
    while let Some(cell) = cells.next() {
        let (cell, content) = cell?;
        if buffering {
            buffering = buffer_cell(&mut buffer, budget, cell, &content, &format, &mut first)?;
            if buffering {
                remaining = cells.clone();
            }
        }
    }
    output.write_all(&buffer)?;
    if format.json && budget == 0 {
        output.write_all(b"[")?;
    }
    // Release the buffer before the remaining cells are read again.
    drop(buffer);
    for cell in remaining {
        let (cell, content) = cell?;
        write_cell(output, cell, &content, &format, &mut first)?;
    }
    if format.json {
        output.write_all(b"]\n")?;
    }
    Ok(())
}
