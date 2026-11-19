use super::index::{self, RowEntry, WorksheetIndex};
use super::parse::Element;
use super::read::{self, SharedStrings, StoredValue};
use super::{errors, protection, validation};
use crate::archive::MAX_PART_SIZE;
use crate::xml::{self, Attributes};
use crate::{CellRange, CellRef, CellValue, Error, Result};
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::{io, mem};

struct Edit {
    start: usize,
    end: usize,
    bytes: String,
}

// Newly generated markup is part of the final XML, so it cannot exceed the
// part limit even when every original byte will be removed.
fn check_part_size(length: usize) -> Result<()> {
    if length > MAX_PART_SIZE {
        return Err(errors::unsupported(format!(
            "patched worksheet exceeds the {} MiB part limit",
            MAX_PART_SIZE >> 20
        )));
    }
    Ok(())
}

/// Nonoverlapping changes against the original worksheet's absolute byte spans.
/// Existing cell/row/extension bytes are copied only when the final XML is emitted.
///
/// An edit's new cells, rows and values are written to one reused buffer while
/// the edit is open. The edit is closed when the next one needs that buffer,
/// or when planning ends, and then holds an allocation of exactly its size,
/// where a buffer of its own, grown by doubling, could take up to twice that.
/// Attribute edits are built at their exact size.
#[derive(Default)]
pub(crate) struct EditPlan<'xml> {
    xml: &'xml [u8],
    edits: Vec<Edit>,
    inserted_bytes: usize,
    // Whether the last edit's markup is still being written, usually in the
    // buffer that `scratch` otherwise holds.
    open: bool,
    scratch: String,
}

// The largest buffer kept for reuse when its edit closes: the markup is copied
// out of it, so the next edit writes without regrowing a buffer from empty. A
// larger buffer is shrunk to its markup and kept by the edit. Until planning
// ends, the kept buffer is held alongside the copies, so planning can briefly
// hold up to this much more than without reuse; the edits it leaves are
// exactly sized, so a batch that succeeds usually peaks lower.
const REUSE_LIMIT: usize = 4 * 1024;

impl<'xml> EditPlan<'xml> {
    pub(super) fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    fn count_inserted(&mut self, length: usize) -> Result<()> {
        let inserted_bytes = self
            .inserted_bytes
            .checked_add(length)
            .ok_or_else(|| errors::unsupported("patched XML exceeds addressable memory"))?;
        check_part_size(inserted_bytes)?;
        self.inserted_bytes = inserted_bytes;
        Ok(())
    }

    // Adds a finished edit. An open edit stays last, so later markup at its
    // position still extends it. Finalization sorts the edits and no two share a
    // span, so this order affects only the sort's cost; where a new edit
    // follows the open one, callers close it first to keep document order.
    fn push(&mut self, edit: Edit) -> Result<()> {
        self.count_inserted(edit.bytes.len())?;
        if self.open {
            let open = self.edits.len() - 1;
            self.edits.insert(open, edit);
        } else {
            self.edits.push(edit);
        }
        Ok(())
    }

    // Closes the open edit, if any, and adds `edit`, whose markup is usually
    // the buffer from `take_scratch`, as the open edit.
    fn push_open(&mut self, edit: Edit) -> Result<()> {
        self.close(true);
        self.push(edit)?;
        self.open = true;
        Ok(())
    }

    // Gives the open edit an allocation of exactly its markup's size. With
    // `reuse`, a buffer up to `REUSE_LIMIT` is kept for the next edit and the
    // markup copied out of it; otherwise the buffer is shrunk.
    fn close(&mut self, reuse: bool) {
        if !mem::take(&mut self.open) {
            return;
        }
        let edit = self
            .edits
            .last_mut()
            .expect("the open edit is the last one");
        if reuse && edit.bytes.capacity() <= REUSE_LIMIT {
            let exact = edit.bytes.as_str().to_owned();
            self.scratch = mem::replace(&mut edit.bytes, exact);
        } else {
            edit.bytes.shrink_to_fit();
        }
    }

    // Closes the open edit and returns the reused buffer, cleared, for
    // markup to pass to `push_open` or `expand`.
    fn take_scratch(&mut self) -> String {
        self.close(true);
        let mut scratch = mem::take(&mut self.scratch);
        scratch.clear();
        scratch
    }

    // Returns a buffer from `take_scratch` that no edit used.
    fn restore_scratch(&mut self, scratch: String) {
        self.scratch = scratch;
    }

    // Ends planning: the open edit's buffer is shrunk rather than copied,
    // since no later edit reuses it, and the reused buffer is freed.
    fn finish(&mut self) {
        self.close(false);
        self.scratch = String::new();
    }

    // Appends new markup at `position` with `write`. Consecutive insertions at
    // one position, such as new cells between two existing ones or new rows
    // after the last, extend one edit: the stable sort would emit them
    // in this order anyway, and each new cell then needs no buffer of its own.
    fn insert(
        &mut self,
        position: usize,
        write: impl FnOnce(&mut String) -> Result<()>,
    ) -> Result<()> {
        let extends_last = self
            .edits
            .last()
            .is_some_and(|edit| edit.start == position && edit.end == position);
        if !extends_last {
            let bytes = self.take_scratch();
            self.push_open(Edit {
                start: position,
                end: position,
                bytes,
            })?;
        }
        let edit = self
            .edits
            .last_mut()
            .expect("an insertion edit was just ensured");
        let before = edit.bytes.len();
        write(&mut edit.bytes)?;
        let added = edit.bytes.len() - before;
        self.count_inserted(added)
    }

    // Turns an empty element `<name/>` into `<name>contents</name>`.
    fn expand(&mut self, source: &str, element: &Element, mut contents: String) -> Result<()> {
        let open_end = element.open_end();
        self.push(Edit {
            start: open_end - 2,
            end: open_end - 1,
            bytes: String::new(),
        })?;
        write!(contents, "</{}>", element.name(source)).expect("writing to a String cannot fail");
        self.push_open(Edit {
            start: open_end,
            end: open_end,
            bytes: contents,
        })
    }

    /// Validate the edit spans and final size before allocating the output.
    pub(crate) fn into_patch(mut self) -> Result<CellPatch<'xml>> {
        // At one offset, a zero-width insertion sorts before the edit that
        // starts there. Edits never share a span, because `insert` merges
        // consecutive insertions at one gap into one edit.
        self.edits.sort_by_key(|edit| (edit.start, edit.end));
        debug_assert!(
            self.edits
                .windows(2)
                .all(|pair| (pair[0].start, pair[0].end) != (pair[1].start, pair[1].end)),
            "no two edits share a span"
        );
        let mut position = 0;
        let mut retained = self.xml.len();
        for edit in &self.edits {
            if edit.start < position || edit.end < edit.start || edit.end > self.xml.len() {
                return Err(errors::unsupported("overlapping XML edits"));
            }
            // The removed spans are disjoint and inside the original XML.
            retained -= edit.end - edit.start;
            position = edit.end;
        }
        let length = retained
            .checked_add(self.inserted_bytes)
            .ok_or_else(|| errors::unsupported("patched XML exceeds addressable memory"))?;
        check_part_size(length)?;
        Ok(CellPatch {
            xml: self.xml,
            edits: self.edits,
            length,
        })
    }
}

pub(crate) struct CellPatch<'xml> {
    xml: &'xml [u8],
    edits: Vec<Edit>,
    length: usize,
}

impl CellPatch<'_> {
    /// Write the validated edits into an exactly sized output buffer.
    pub(crate) fn apply(self) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        output
            .try_reserve_exact(self.length)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        let mut position = 0;
        // Each edit's markup is freed once copied, rather than all of it being
        // held until the whole worksheet is written.
        for edit in self.edits {
            output.extend_from_slice(&self.xml[position..edit.start]);
            output.extend_from_slice(edit.bytes.as_bytes());
            position = edit.end;
        }
        output.extend_from_slice(&self.xml[position..]);
        debug_assert_eq!(
            output.len(),
            self.length,
            "the retained XML and planned markup account for the whole output"
        );
        Ok(output)
    }
}

fn attribute_edit(
    element: &Element,
    attributes: &Attributes,
    name: &str,
    value: Option<&str>,
) -> Option<Edit> {
    if let Some(attribute) = element.attribute(attributes, name) {
        match value {
            Some(value) if value != attribute.value => Some(Edit {
                start: attribute.value_start(),
                end: attribute.value_end(),
                bytes: value.to_owned(),
            }),
            None => Some(Edit {
                start: attribute.start(),
                end: attribute.end(),
                bytes: String::new(),
            }),
            _ => None,
        }
    } else {
        // Before the tag's closing `>` or `/>`.
        let position = element.open_end() - if element.empty { 2 } else { 1 };
        value.map(|value| {
            let mut bytes = String::with_capacity(name.len() + value.len() + 4);
            write!(bytes, " {name}=\"{value}\"").expect("writing to a String cannot fail");
            Edit {
                start: position,
                end: position,
                bytes,
            }
        })
    }
}

#[cfg(test)]
pub(super) fn text_escaped(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    xml::append_cell_text(&mut output, text);
    output
}

fn value_type(value: &CellValue) -> Option<&'static str> {
    match value {
        CellValue::Text(_) => Some("inlineStr"),
        CellValue::Bool(_) => Some("b"),
        CellValue::Error(_) => Some("e"),
        _ => None,
    }
}

// A number as an edit writes it, formatted on the stack. The buffer holds the
// longest `f64` as `Display` writes it, a negative subnormal written out in
// full; a longer one would take the slower path that formats into the edit.
struct Digits {
    bytes: [u8; 336],
    len: usize,
}

impl fmt::Write for Digits {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        let slot = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        slot.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

// Appends the payload elements for `value`, named with `prefix` (see
// `Element::prefix`). When they start an edit, `ends` is how many bytes the
// caller writes after them: the payload and those are then reserved exactly,
// so unless escapes outgrow the text's share, the edit needs neither to
// double for its last few bytes nor to be shrunk when it closes. A number is
// measured for this only in a buffer not yet allocated. Shared buffers, and
// escapes beyond the reservation, grow as a `Vec` does.
fn append_payload(output: &mut String, prefix: &str, value: &CellValue, ends: Option<usize>) {
    // Reserves `payload` bytes and the end tags exactly when they start an edit.
    fn reserve(output: &mut String, ends: Option<usize>, payload: usize) {
        if let Some(ends) = ends {
            output.reserve_exact(payload + ends);
        }
    }
    let value_element = "<v></v>".len() + 2 * prefix.len();
    match value {
        CellValue::Text(text) => {
            reserve(
                output,
                ends,
                "<is><t xml:space=\"preserve\"></t></is>".len() + 4 * prefix.len() + text.len(),
            );
            write!(output, "<{prefix}is><{prefix}t xml:space=\"preserve\">")
                .expect("writing to a String cannot fail");
            xml::append_cell_text(output, text);
            write!(output, "</{prefix}t></{prefix}is>").expect("writing to a String cannot fail");
        }
        CellValue::Number(number) => {
            let number = stored_number(*number);
            // A number is measured first only when it starts an edit in a
            // buffer not yet allocated, as a plan's first does; a reused or
            // shared buffer takes it directly.
            if ends.is_some() && output.capacity() == 0 {
                let mut digits = Digits {
                    bytes: [0; 336],
                    len: 0,
                };
                if write!(digits, "{number}").is_ok() {
                    let digits =
                        str::from_utf8(&digits.bytes[..digits.len]).expect("a number is ASCII");
                    reserve(output, ends, value_element + digits.len());
                    write!(output, "<{prefix}v>{digits}</{prefix}v>")
                        .expect("writing to a String cannot fail");
                    return;
                }
            }
            write!(output, "<{prefix}v>{number}</{prefix}v>")
                .expect("writing to a String cannot fail");
        }
        CellValue::Bool(boolean) => {
            reserve(output, ends, value_element + 1);
            write!(output, "<{prefix}v>{}</{prefix}v>", u8::from(*boolean))
                .expect("writing to a String cannot fail");
        }
        CellValue::Error(error) => {
            reserve(output, ends, value_element + error.len());
            write!(output, "<{prefix}v>").expect("writing to a String cannot fail");
            xml::append_cell_text(output, error);
            write!(output, "</{prefix}v>").expect("writing to a String cannot fail");
        }
        CellValue::Blank => {}
    }
}

// Appends a new cell's markup, named with its row's `prefix`.
fn append_new_cell(
    output: &mut String,
    prefix: &str,
    cell: CellRef,
    style: Option<u32>,
    value: &CellValue,
) -> Result<()> {
    let starts_edit = output.is_empty();
    write!(output, "<{prefix}c r=\"{cell}\"").expect("writing to a String cannot fail");
    if let Some(style) = style {
        write!(output, " s=\"{style}\"").expect("writing to a String cannot fail");
    }
    if let Some(kind) = value_type(value) {
        write!(output, " t=\"{kind}\"").expect("writing to a String cannot fail");
    }
    output.push('>');
    let ends = starts_edit.then_some("</c>".len() + prefix.len());
    append_payload(output, prefix, value, ends);
    write!(output, "</{prefix}c>").expect("writing to a String cannot fail");
    check_part_size(output.len())
}

// The workbook-wide inputs that decide whether an edit rewrites a cell's
// current value: its shared strings, loaded only to compare text, and its
// date system.
struct CurrentValues<F> {
    shared_strings: F,
    date1904: bool,
}

// The number an edit writes. Spreadsheet numbers have no negative zero, and
// Excel never writes one, so -0.0 is stored as 0.
fn stored_number(number: f64) -> f64 {
    if number == 0.0 { 0.0 } else { number }
}

/// Whether writing `new` over a cell that reads as `old` leaves it unchanged:
/// the same value, where a number is the current one or the one written for
/// it. A stored -0 reads with its sign, so writing 0 replaces it, while
/// writing -0 leaves it.
pub(crate) fn rewrites_unchanged(old: &CellValue, new: &CellValue) -> bool {
    match (old, new) {
        (CellValue::Number(old), CellValue::Number(new)) => {
            old.to_bits() == new.to_bits() || old.to_bits() == stored_number(*new).to_bits()
        }
        _ => old == new,
    }
}

// Whether `value` is the cell's current payload, as it reads. Decoding the old
// value only detects no-op edits, so an undecodable old value counts as a
// change rather than blocking its replacement; `validate_payload` already
// refused unfamiliar markup that a replacement would discard.
fn unchanged_value<'s>(
    index: &WorksheetIndex,
    cell: &Element,
    payload: usize,
    value: &CellValue,
    current: &mut CurrentValues<impl FnMut() -> Option<&'s SharedStrings>>,
) -> bool {
    // An existing payload, even an empty `<v/>` that reads as blank, is a
    // representation that clearing removes.
    if matches!(value, CellValue::Blank) {
        return false;
    }
    // A different scalar type cannot compare equal. In particular, replacing
    // a large stored string with a number must not decode and allocate the old
    // text merely to discard it. Payload validation still precedes this check,
    // so unfamiliar markup and metadata retain their existing protections.
    let kind = cell
        .attribute(&index.attributes, "t")
        .map_or("n", |attribute| attribute.value);
    if !matches!(
        (value, kind),
        (CellValue::Text(_), "inlineStr" | "str" | "s")
            | (CellValue::Number(_), "n" | "d")
            | (CellValue::Bool(_), "b")
            | (CellValue::Error(_), "e")
    ) {
        return false;
    }
    match read::decode_payload(index, cell, payload, current.date1904) {
        Ok(StoredValue::Value(old)) => rewrites_unchanged(&old, value),
        // The table is loaded only to compare text. An entry that cannot be
        // decoded, like a table that cannot be loaded, leaves a change.
        Ok(StoredValue::SharedString(position)) => match value {
            CellValue::Text(text) => (current.shared_strings)()
                .and_then(|strings| strings.get(position)?.ok())
                .is_some_and(|old| old == text.as_str()),
            _ => false,
        },
        Err(_) => false,
    }
}

// Refuses a cell whose style index reads refuse.
fn check_style_index(index: &WorksheetIndex, cell: &Element, address: CellRef) -> Result<()> {
    read::style_index(index, cell)
        .map(drop)
        .map_err(|error| error.context(address))
}

// The error for a cell whose payload cannot be read. Reads report an
// unreadable style index first, and so does this.
#[cold]
fn refused(index: &WorksheetIndex, cell: &Element, address: CellRef, error: Error) -> Error {
    check_style_index(index, cell, address)
        .err()
        .unwrap_or_else(|| error.context(address))
}

fn patched_cell<'s>(
    index: &WorksheetIndex,
    address: CellRef,
    cell_index: usize,
    value: &CellValue,
    edits: &mut EditPlan,
    current: &mut CurrentValues<impl FnMut() -> Option<&'s SharedStrings>>,
) -> Result<bool> {
    let (source, elements, attributes, namespace) = (
        index.source(),
        &index.elements,
        &index.attributes,
        index.namespace,
    );
    let cell = &elements[cell_index];
    let value_child =
        read::value_payload(index, cell).map_err(|error| refused(index, cell, address, error))?;
    if let Some(child) = value_child {
        validation::validate_payload(source, elements, attributes, child, namespace)
            .map_err(|error| refused(index, cell, address, error))?;
    }
    let unchanged = if let Some(child) = value_child {
        unchanged_value(index, cell, child, value, current)
    } else {
        matches!(value, CellValue::Blank) && cell.attribute(attributes, "t").is_none()
    };
    if unchanged {
        return Ok(false);
    }
    // A change keeps the cell's style index, which reads refuse unless it is an
    // unsigned 32-bit integer, so such a cell is refused here too. An edit that
    // leaves the cell unchanged writes nothing and succeeds, as before.
    check_style_index(index, cell, address)?;
    // Value metadata can link to rich data in other package parts. Cell metadata
    // has per-type edit/clear flags in the metadata part (ECMA-376 18.9.10), so
    // even a cell-level link may not survive a value change. Without interpreting
    // those records, keep both links and their original payload intact.
    if cell.attribute(attributes, "vm").is_some() {
        return Err(errors::protected(address, protection::VALUE_METADATA));
    }
    if cell.attribute(attributes, "cm").is_some() {
        return Err(errors::protected(address, protection::CELL_METADATA));
    }
    // Taking the buffer closes the previous edit first, so the type edit
    // follows it in document order, which keeps the final sort cheap. A row's
    // spans edit and the dimension edit are still added after the edits that
    // follow them.
    let mut value_bytes = edits.take_scratch();
    if let Some(type_edit) = attribute_edit(cell, attributes, "t", value_type(value)) {
        edits.push(type_edit)?;
    }
    let prefix = cell.prefix(source);
    // An empty cell (`<c/>`) is expanded, which adds its end tag.
    let end_tag = if cell.empty {
        "</c>".len() + prefix.len()
    } else {
        0
    };
    append_payload(&mut value_bytes, prefix, value, Some(end_tag));
    if cell.empty {
        if value_bytes.is_empty() {
            edits.restore_scratch(value_bytes);
        } else {
            edits.expand(source, cell, value_bytes)?;
        }
        return Ok(true);
    }
    if let Some(child) = value_child {
        edits.push_open(Edit {
            start: elements[child].start(),
            end: elements[child].end(),
            bytes: value_bytes,
        })?;
    } else if value_bytes.is_empty() {
        edits.restore_scratch(value_bytes);
    } else {
        let position = cell
            .children(elements)
            .map(|child| &elements[child])
            .find(|child| child.is(source, namespace, "extLst"))
            .map_or(cell.close_start(), Element::start);
        edits.push_open(Edit {
            start: position,
            end: position,
            bytes: value_bytes,
        })?;
    }
    Ok(true)
}

// One `first:last` interval of a row's `spans` hint.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Span {
    first: u16,
    last: u16,
}

fn spans_edit(
    row: &Element,
    row_number: u32,
    attributes: &Attributes,
    columns: impl Iterator<Item = u16>,
) -> Result<Option<Edit>> {
    let Some(attribute) = row.attribute(attributes, "spans") else {
        return Ok(None);
    };
    let invalid = || errors::unsupported(format!("row {row_number}: invalid row spans"));
    let mut spans = Vec::new();
    let span_texts = attribute
        .value
        .split([' ', '\t', '\r', '\n'])
        .filter(|span| !span.is_empty());
    for span in span_texts {
        let Some((first, last)) = span.split_once(':') else {
            return Err(invalid());
        };
        let first: u16 = first.parse().map_err(|_| invalid())?;
        let last: u16 = last.parse().map_err(|_| invalid())?;
        if first == 0 || first > last || last > CellRef::MAX_COLUMN {
            return Err(invalid());
        }
        spans.push(Span { first, last });
    }
    let (Some(mut first), Some(mut last)) = (
        spans.iter().map(|span| span.first).min(),
        spans.iter().map(|span| span.last).max(),
    ) else {
        return Err(errors::unsupported(format!(
            "row {row_number}: empty row spans"
        )));
    };
    spans.sort_unstable();
    let mut spans = spans.into_iter().peekable();
    let mut changed = false;
    // Changed columns arrive in order. Sweep the sorted intervals once rather
    // than checking every span for every cell in a large fragmented row.
    for column in columns {
        while spans.peek().is_some_and(|span| span.last < column) {
            spans.next();
        }
        changed |= spans.peek().is_none_or(|span| column < span.first);
        first = first.min(column);
        last = last.max(column);
    }
    if !changed {
        return Ok(None);
    }
    // Spans are optional hints. Widen once for the whole batch, retaining every
    // old interval and covering all new columns with a conservative bound.
    Ok(attribute_edit(
        row,
        attributes,
        "spans",
        Some(&format!("{first}:{last}")),
    ))
}

#[derive(Clone, Copy)]
enum RowStyle {
    ColumnDefault,
    Custom(u32),
}

// A custom row format overrides column defaults, including style zero.
fn row_style(index: &WorksheetIndex, row: &Element, cell: CellRef) -> Result<RowStyle> {
    let attributes = &index.attributes;
    let custom_format = row
        .attribute(attributes, "customFormat")
        .map(|attribute| xml::trim_whitespace(attribute.value));
    if matches!(custom_format, None | Some("0" | "false")) {
        return Ok(RowStyle::ColumnDefault);
    }
    if !matches!(custom_format, Some("1" | "true")) {
        return Err(errors::unsupported(format!(
            "{cell}: invalid row format flag"
        )));
    }
    let Some(style) = row.attribute(attributes, "s") else {
        return Ok(RowStyle::Custom(0));
    };
    xml::trim_whitespace(style.value)
        .parse::<u32>()
        .map(RowStyle::Custom)
        .map_err(|_| errors::unsupported(format!("{cell}: invalid row style index")))
}

// One row's edits, in column order.
type RowChanges<'edits> = [(&'edits CellRef, &'edits CellValue)];

// Patches an existing row's cells and inserts its new ones in column order.
// Only a row with a changed cell gets wider spans or, when it was empty
// (`<row/>`), an expansion holding all of its new cells.
fn patch_existing_row<'s>(
    index: &WorksheetIndex,
    row: &RowEntry,
    changes: &RowChanges,
    edits: &mut EditPlan,
    changed_cells: &mut Vec<CellRef>,
    current: &mut CurrentValues<impl FnMut() -> Option<&'s SharedStrings>>,
) -> Result<()> {
    let (source, elements) = (index.source(), &index.elements);
    let element = &elements[row.element];
    // The prefix of new cells, found once one is needed.
    let mut prefix = None;
    let existing_cells = &index.cells[row.cells.clone()];
    let mut cell_position = 0;
    // The new cells of an empty row (`<row/>`). A first cell is written to the
    // reused buffer. A second moves the row to a buffer of exactly the first
    // cell's size and frees the reused one, so that a row of many cells grows
    // as a row built from whole cells does.
    let mut empty_row_cells: Option<String> = None;
    let mut cells_in_scratch = false;
    // Resolve only when a new nonblank cell needs a style. Existing cells and
    // blank edits must not start validating row defaults; later insertions can
    // reuse the first result without rescanning attributes or numeric text.
    let mut cached_row_style: Option<RowStyle> = None;
    // Trailing cells follow the last existing cell. Only an extension list
    // after that cell comes later; one placed before or between cells must not
    // move new cells ahead of existing ones.
    let mut trailing = element.children(elements);
    if let Some(last_cell) = existing_cells.last() {
        trailing.next = elements[last_cell.element].descendants().end;
    }
    let fallback = trailing
        .map(|child| &elements[child])
        .find(|child| child.is(source, index.namespace, "extLst"))
        .map_or(element.close_start(), Element::start);
    for &(&cell, value) in changes {
        while cell_position < existing_cells.len()
            && existing_cells[cell_position].address.column < cell.column
        {
            cell_position += 1;
        }
        let existing = existing_cells
            .get(cell_position)
            .filter(|existing| existing.address == cell);
        if let Some(existing) = existing {
            if !patched_cell(index, cell, existing.element, value, edits, current)? {
                continue;
            }
        } else if matches!(value, CellValue::Blank) {
            continue;
        } else {
            let default = match cached_row_style {
                Some(style) => style,
                None => {
                    let style = row_style(index, element, cell)?;
                    cached_row_style = Some(style);
                    style
                }
            };
            let style = match default {
                RowStyle::ColumnDefault => index.column_style(cell)?,
                RowStyle::Custom(style) => (style != 0).then_some(style),
            };
            let prefix = *prefix.get_or_insert_with(|| element.prefix(source));
            if element.empty {
                if let Some(markup) = &mut empty_row_cells {
                    if mem::take(&mut cells_in_scratch) {
                        *markup = markup.as_str().to_owned();
                    }
                    append_new_cell(markup, prefix, cell, style, value)?;
                } else {
                    let mut first = edits.take_scratch();
                    append_new_cell(&mut first, prefix, cell, style, value)?;
                    empty_row_cells = Some(first);
                    cells_in_scratch = true;
                }
            } else {
                let insertion = existing_cells
                    .get(cell_position)
                    .map_or(fallback, |next| elements[next.element].start());
                edits.insert(insertion, |markup| {
                    append_new_cell(markup, prefix, cell, style, value)
                })?;
            }
        }
        changed_cells.push(cell);
    }
    if changed_cells.is_empty() {
        return Ok(());
    }
    let spans = spans_edit(
        element,
        row.number,
        &index.attributes,
        changed_cells.iter().map(|cell| cell.column),
    )?;
    if let Some(spans) = spans {
        edits.push(spans)?;
    }
    if let Some(cells) = empty_row_cells {
        edits.expand(source, element, cells)?;
    }
    Ok(())
}

// Appends a new row holding a row number's values. Blank values need no
// cell, so the caller only adds a row for changes with a non-blank value.
fn append_new_row(
    output: &mut String,
    index: &WorksheetIndex,
    number: u32,
    changes: &RowChanges,
    changed_cells: &mut Vec<CellRef>,
) -> Result<()> {
    let prefix = index.elements[index.data_index].prefix(index.source());
    write!(output, "<{prefix}row r=\"{number}\">").expect("writing to a String cannot fail");
    for &(&cell, value) in changes {
        if !matches!(value, CellValue::Blank) {
            let style = index.column_style(cell)?;
            append_new_cell(output, prefix, cell, style, value)?;
            changed_cells.push(cell);
        }
    }
    write!(output, "</{prefix}row>").expect("writing to a String cannot fail");
    Ok(())
}

// Widens the worksheet's dimension to include every changed cell. A reference
// that already covers them keeps its exact bytes.
fn dimension_edit(index: &WorksheetIndex, changed: Option<CellRange>) -> Result<Option<Edit>> {
    let Some(changed) = changed else {
        return Ok(None);
    };
    let (source, elements) = (index.source(), &index.elements);
    let dimension = errors::unique(
        elements[0]
            .children(elements)
            .map(|child| &elements[child])
            .filter(|element| element.is(source, index.namespace, "dimension")),
        || errors::unsupported("multiple worksheet dimensions"),
    )?;
    let Some(dimension) = dimension else {
        return Ok(None);
    };
    let reference = dimension
        .attribute(&index.attributes, "ref")
        .ok_or_else(|| errors::unsupported("dimension without a reference"))?;
    // Reads ignore the dimension, so a malformed reference refuses only the
    // edits that may need to widen it, like a malformed `spans` hint.
    let old = index::reference_range(reference.value).map_err(|error| match error {
        Error::InvalidWorkbook(message) => errors::unsupported(message),
        error => error,
    })?;
    if old.contains(changed.first()) && old.contains(changed.last()) {
        return Ok(None);
    }
    let range = old.including(changed.first()).including(changed.last());
    Ok(attribute_edit(
        dimension,
        &index.attributes,
        "ref",
        Some(&range.to_string()),
    ))
}

pub(super) fn plan_cells<'xml, 's>(
    index: &WorksheetIndex<'xml>,
    cells: &BTreeMap<CellRef, CellValue>,
    shared_strings: impl FnMut() -> Option<&'s SharedStrings>,
    date1904: bool,
) -> Result<EditPlan<'xml>> {
    let mut edits = EditPlan {
        xml: index.source().as_bytes(),
        ..EditPlan::default()
    };
    if cells.is_empty() {
        return Ok(edits);
    }
    index.validate_edits(cells)?;
    // The table is needed only to recognize rewriting a shared string's own
    // text; a table that is absent or fails to load leaves such edits as
    // changes. The caller loads it at most once per call.
    let mut current = CurrentValues {
        shared_strings,
        date1904,
    };
    let data = &index.elements[index.data_index];
    let mut changed_bounds: Option<CellRange> = None;
    // Individual changed columns are needed only for the current row's spans.
    // Its first and last changed cells suffice to extend the sheet dimension.
    let mut changed_cells = Vec::new();
    // New rows for an empty `<sheetData/>` share one expansion, written to the
    // reused buffer once a first row needs it.
    let mut empty_data_rows: Option<String> = None;
    let mut pending = cells.iter().peekable();
    let mut row_position = 0;
    let mut changes: Vec<(&CellRef, &CellValue)> = Vec::new();
    while let Some(&(&address, _)) = pending.peek() {
        let number = address.row;
        // Rows and edits are both sorted, so one cursor finds each edited row
        // or the row a new one precedes.
        while index
            .rows
            .get(row_position)
            .is_some_and(|row| row.number < number)
        {
            row_position += 1;
        }
        changes.clear();
        changed_cells.clear();
        while let Some(change) = pending.next_if(|(cell, _)| cell.row == number) {
            changes.push(change);
        }
        let next_row = index.rows.get(row_position);
        if let Some(row) = next_row.filter(|row| row.number == number) {
            patch_existing_row(
                index,
                row,
                &changes,
                &mut edits,
                &mut changed_cells,
                &mut current,
            )?;
        } else if changes
            .iter()
            .any(|(_, value)| !matches!(value, CellValue::Blank))
        {
            if data.empty {
                append_new_row(
                    empty_data_rows.get_or_insert_with(|| edits.take_scratch()),
                    index,
                    number,
                    &changes,
                    &mut changed_cells,
                )?;
            } else {
                let insertion = next_row.map_or(data.close_start(), |row| {
                    index.elements[row.element].start()
                });
                edits.insert(insertion, |markup| {
                    append_new_row(markup, index, number, &changes, &mut changed_cells)
                })?;
            }
        }
        if let Some((&first, &last)) = changed_cells.first().zip(changed_cells.last()) {
            changed_bounds = Some(changed_bounds.map_or_else(
                || CellRange::new(first, last),
                |bounds| bounds.including(first).including(last),
            ));
        }
    }
    if let Some(rows) = empty_data_rows.filter(|rows| !rows.is_empty()) {
        edits.expand(index.source(), data, rows)?;
    }
    if let Some(edit) = dimension_edit(index, changed_bounds)? {
        edits.push(edit)?;
    }
    edits.finish();
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_are_allocated_at_exactly_their_size() {
        // New cells between and after existing ones, in an empty row and in new
        // rows, rewritten cells, and several of them at one position, whatever
        // their values need: XML and Excel escapes, long numbers, styles and a
        // prefix.
        for (namespace, prefix) in [("", ""), (":x", "x:")] {
            let xml = format!(
                "<{prefix}worksheet xmlns{namespace}='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><{prefix}sheetData><{prefix}row r='1'><{prefix}c r='A1'/><{prefix}c r='C1'><{prefix}v>1</{prefix}v></{prefix}c></{prefix}row><{prefix}row r='2' s='3' customFormat='1'/><{prefix}row r='4'><{prefix}c r='A4'/></{prefix}row><{prefix}row r='6'/></{prefix}sheetData></{prefix}worksheet>"
            );
            let index = WorksheetIndex::parse(xml.as_bytes()).unwrap();
            let text = |text: &str| CellValue::Text(text.to_owned());
            let cells = BTreeMap::from([
                ("A1".parse().unwrap(), text("rewritten & empty")),
                ("B1".parse().unwrap(), text("R&D <1>\r_x0041_\u{1}")),
                ("C1".parse().unwrap(), CellValue::Number(12_345.0 / 7.0)),
                ("D1".parse().unwrap(), CellValue::Number(1e21)),
                ("E1".parse().unwrap(), text(&"long ".repeat(40))),
                ("A2".parse().unwrap(), text("in an empty row")),
                ("B2".parse().unwrap(), CellValue::Bool(false)),
                ("A3".parse().unwrap(), CellValue::Bool(true)),
                ("A5".parse().unwrap(), CellValue::Error("#N/A".to_owned())),
                (
                    "XFD4".parse().unwrap(),
                    CellValue::Error("#DIV/0!".to_owned()),
                ),
            ]);
            let plan = plan_cells(&index, &cells, || None, false).unwrap();
            let markup: Vec<_> = plan
                .edits
                .iter()
                .filter(|edit| edit.bytes.starts_with('<'))
                .collect();
            // A1's payload with B1, which follows it; C1's payload; D1 with
            // E1; A2 and B2 with row 3; row 5; and XFD4.
            assert_eq!(markup.len(), 6, "{xml}");
            // Every edit, including attribute and value replacements.
            for edit in &plan.edits {
                assert_eq!(edit.bytes.capacity(), edit.bytes.len(), "{}", edit.bytes);
            }
            let patched = String::from_utf8(plan.into_patch().unwrap().apply().unwrap()).unwrap();
            for expected in [
                format!(
                    "<{prefix}c r='A1' t=\"inlineStr\"><{prefix}is><{prefix}t xml:space=\"preserve\">rewritten &amp; empty</{prefix}t></{prefix}is></{prefix}c>"
                ),
                format!(
                    "<{prefix}c r=\"B1\" t=\"inlineStr\"><{prefix}is><{prefix}t xml:space=\"preserve\">R&amp;D &lt;1&gt;&#13;_x005F_x0041__x0001_</{prefix}t></{prefix}is></{prefix}c>"
                ),
                format!(
                    "<{prefix}c r='C1'><{prefix}v>1763.5714285714287</{prefix}v></{prefix}c><{prefix}c r=\"D1\"><{prefix}v>1000000000000000000000</{prefix}v></{prefix}c><{prefix}c r=\"E1\""
                ),
                format!("customFormat='1'><{prefix}c r=\"A2\" s=\"3\" t=\"inlineStr\">"),
                format!(
                    "<{prefix}v>0</{prefix}v></{prefix}c></{prefix}row><{prefix}row r=\"3\"><{prefix}c r=\"A3\" t=\"b\"><{prefix}v>1</{prefix}v></{prefix}c></{prefix}row><{prefix}row r='4'>"
                ),
                format!(
                    "<{prefix}row r=\"5\"><{prefix}c r=\"A5\" t=\"e\"><{prefix}v>#N/A</{prefix}v></{prefix}c></{prefix}row>"
                ),
                format!(
                    "<{prefix}c r=\"XFD4\" t=\"e\"><{prefix}v>#DIV/0!</{prefix}v></{prefix}c></{prefix}row>"
                ),
            ] {
                assert!(patched.contains(&expected), "{patched}\n{expected}");
            }
        }
    }

    #[test]
    fn edits_closed_on_every_path_are_allocated_at_exactly_their_size() {
        // B1's long text grows the reused buffer first, so a smaller edit
        // that kept it would show a larger capacity than its length.
        //
        // - B1 and C1, which grows their buffer past B1's exact reservation,
        //   stay open while their row's spans edit is added, and are copied
        //   when A4 needs the buffer.
        // - A4 and B4 get values in cells without a value element.
        // - Row 5's three long cells, over `REUSE_LIMIT`, are shrunk rather
        //   than copied when the next row needs the buffer.
        // - Row 6's expansion is the last edit, still open while the
        //   dimension edit is added and shrunk when planning ends.
        let xml = "<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><dimension ref='A1'/><sheetData><row r='1' spans='1:1'><c r='A1'><v>1</v></c></row><row r='4'><c r='A4'></c><c r='B4'><extLst/></c></row><row r='6'/></sheetData></worksheet>";
        let index = WorksheetIndex::parse(xml.as_bytes()).unwrap();
        let long = "x".repeat(30_000);
        let mut cells = BTreeMap::from([
            ("B1".parse().unwrap(), CellValue::Text("long ".repeat(80))),
            ("C1".parse().unwrap(), CellValue::Number(2.5)),
            ("A4".parse().unwrap(), CellValue::Number(2.0)),
            ("B4".parse().unwrap(), CellValue::Error("#REF!".to_owned())),
            ("A6".parse().unwrap(), CellValue::Bool(true)),
        ]);
        for column in ["A", "B", "C"] {
            cells.insert(
                format!("{column}5").parse().unwrap(),
                CellValue::Text(long.clone()),
            );
        }
        let plan = plan_cells(&index, &cells, || None, false).unwrap();
        let markup: Vec<_> = plan
            .edits
            .iter()
            .filter(|edit| edit.bytes.starts_with('<'))
            .collect();
        assert_eq!(markup.len(), 5);
        assert!(markup.iter().any(|edit| edit.bytes.len() > REUSE_LIMIT));
        for edit in &plan.edits {
            assert_eq!(
                edit.bytes.capacity(),
                edit.bytes.len(),
                "{}",
                &edit.bytes[..edit.bytes.len().min(80)]
            );
        }
        let patched = String::from_utf8(plan.into_patch().unwrap().apply().unwrap()).unwrap();
        for expected in [
            "<dimension ref='A1:C6'/>",
            "<row r='1' spans='1:3'><c r='A1'><v>1</v></c><c r=\"B1\" t=\"inlineStr\">",
            "</c><c r=\"C1\"><v>2.5</v></c></row>",
            "<c r='A4'><v>2</v></c><c r='B4' t=\"e\"><v>#REF!</v><extLst/></c></row><row r=\"5\">",
            "</row><row r='6'><c r=\"A6\" t=\"b\"><v>1</v></c></row></sheetData>",
        ] {
            assert!(patched.contains(expected), "{expected}");
        }

        // New rows for an empty `<sheetData/>`, and a dimension edit.
        let xml = "<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><dimension ref='A1'/><sheetData/></worksheet>";
        let index = WorksheetIndex::parse(xml.as_bytes()).unwrap();
        let cells = BTreeMap::from([
            ("A1".parse().unwrap(), CellValue::Text("new row ".repeat(9))),
            ("B3".parse().unwrap(), CellValue::Number(3.0)),
        ]);
        let plan = plan_cells(&index, &cells, || None, false).unwrap();
        for edit in &plan.edits {
            assert_eq!(edit.bytes.capacity(), edit.bytes.len(), "{}", edit.bytes);
        }
        let patched = String::from_utf8(plan.into_patch().unwrap().apply().unwrap()).unwrap();
        assert!(
            patched.contains("<dimension ref='A1:B3'/><sheetData><row r=\"1\">"),
            "{patched}"
        );
    }

    #[test]
    fn type_edits_keep_a_plan_in_document_order() {
        // Rewrites that change a cell's type add a type edit before its value
        // edit, and an empty cell is expanded; the final sort then sees one run.
        let xml = "<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><sheetData><row r='1'><c r='A1'><v>1</v></c><c r='B1' t='s'><v>0</v></c><c r='C1' t='str'><v>x</v></c><c r='D1'/><c r='E1' t='b'><v>1</v></c></row><row r='2'><c r='A2'><v>2</v></c></row></sheetData></worksheet>";
        let index = WorksheetIndex::parse(xml.as_bytes()).unwrap();
        let cells = BTreeMap::from([
            ("A1".parse().unwrap(), CellValue::Bool(true)),
            ("B1".parse().unwrap(), CellValue::Number(3.0)),
            ("C1".parse().unwrap(), CellValue::Blank),
            ("D1".parse().unwrap(), CellValue::Error("#N/A".to_owned())),
            ("E1".parse().unwrap(), CellValue::Text("text".to_owned())),
            ("A2".parse().unwrap(), CellValue::Bool(false)),
        ]);
        let plan = plan_cells(&index, &cells, || None, false).unwrap();
        assert!(
            plan.edits.is_sorted_by_key(|edit| (edit.start, edit.end)),
            "{:?}",
            plan.edits
                .iter()
                .map(|edit| (edit.start, edit.end))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_number_fits_the_stack_buffer() {
        let mut values = vec![
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            5e-324,
            -5e-324,
            1.234_567_890_123_456_7e-308,
            -1e-323,
        ];
        // Every subnormal power of two, and its neighbours.
        let mut tiny = f64::MIN_POSITIVE;
        while tiny > 0.0 {
            values.extend([tiny, -tiny, tiny * 1.999_999_999_999_999_8]);
            tiny /= 2.0;
        }
        for value in values {
            let mut digits = Digits {
                bytes: [0; 336],
                len: 0,
            };
            assert!(write!(digits, "{value}").is_ok(), "{value:e}");
            assert_eq!(&digits.bytes[..digits.len], value.to_string().as_bytes());
        }
    }
}
