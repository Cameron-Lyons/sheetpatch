use super::index::{self, RowEntry, WorksheetIndex};
use super::parse::Element;
use super::read::{self, SharedStrings, StoredValue};
use super::{errors, protection, validation};
use crate::xml::{self, Attributes};
use crate::{CellRef, CellValue, Error, Result};
use std::collections::BTreeMap;

struct Edit {
    start: usize,
    end: usize,
    bytes: Vec<u8>,
}

/// Nonoverlapping changes against the original worksheet's absolute byte spans.
/// Existing cell/row/extension bytes are copied only when the final XML is emitted.
#[derive(Default)]
pub(super) struct EditPlan {
    edits: Vec<Edit>,
}

impl EditPlan {
    pub(super) fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    fn push(&mut self, edit: Edit) {
        self.edits.push(edit);
    }

    // Turns an empty element `<name/>` into `<name>contents</name>`.
    fn expand(&mut self, source: &str, element: &Element, mut contents: Vec<u8>) {
        let open_end = element.open_end();
        self.push(Edit {
            start: open_end - 2,
            end: open_end - 1,
            bytes: Vec::new(),
        });
        contents.extend_from_slice(format!("</{}>", element.name(source)).as_bytes());
        self.push(Edit {
            start: open_end,
            end: open_end,
            bytes: contents,
        });
    }

    /// The original XML with every planned edit applied.
    pub(super) fn apply(mut self, xml: &[u8]) -> Result<Vec<u8>> {
        // Stable order is significant for several new cells/rows at one gap.
        self.edits.sort_by_key(|edit| (edit.start, edit.end));
        let mut position = 0;
        let mut length = 0usize;
        for edit in &self.edits {
            if edit.start < position || edit.end < edit.start || edit.end > xml.len() {
                return Err(errors::unsupported("overlapping XML edits"));
            }
            length = length
                .checked_add(edit.start - position)
                .and_then(|length| length.checked_add(edit.bytes.len()))
                .ok_or_else(|| errors::unsupported("patched XML exceeds addressable memory"))?;
            position = edit.end;
        }
        length = length
            .checked_add(xml.len() - position)
            .ok_or_else(|| errors::unsupported("patched XML exceeds addressable memory"))?;
        let mut output = Vec::with_capacity(length);
        position = 0;
        for edit in self.edits {
            output.extend_from_slice(&xml[position..edit.start]);
            output.extend(edit.bytes);
            position = edit.end;
        }
        output.extend_from_slice(&xml[position..]);
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
                bytes: value.as_bytes().to_vec(),
            }),
            None => Some(Edit {
                start: attribute.start(),
                end: attribute.end(),
                bytes: Vec::new(),
            }),
            _ => None,
        }
    } else {
        // Before the tag's closing `>` or `/>`.
        let position = element.open_end() - if element.empty { 2 } else { 1 };
        value.map(|value| Edit {
            start: position,
            end: position,
            bytes: format!(" {name}=\"{value}\"").into_bytes(),
        })
    }
}

pub(super) fn text_escaped(text: &str) -> String {
    // Excel escapes come first: characters XML cannot carry are stored as
    // `_xHHHH_`, and literal tokens are escaped so Excel shows the exact user
    // string rather than interpreting, for example, _x0041_ as A.
    let text = xml::excel_escaped(text);
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '\r' => output.push_str("&#13;"),
            _ => output.push(character),
        }
    }
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

fn payload(source: &str, prefix: &Element, value: &CellValue) -> Vec<u8> {
    let v = prefix.qualified(source, "v");
    match value {
        CellValue::Text(text) => {
            let is = prefix.qualified(source, "is");
            let t = prefix.qualified(source, "t");
            format!(
                "<{is}><{t} xml:space=\"preserve\">{}</{t}></{is}>",
                text_escaped(text)
            )
            .into_bytes()
        }
        CellValue::Number(number) => format!("<{v}>{number}</{v}>").into_bytes(),
        CellValue::Bool(boolean) => format!("<{v}>{}</{v}>", u8::from(*boolean)).into_bytes(),
        CellValue::Error(error) => format!("<{v}>{}</{v}>", text_escaped(error)).into_bytes(),
        CellValue::Blank => Vec::new(),
    }
}

fn new_cell(
    source: &str,
    context: &Element,
    cell: CellRef,
    style: Option<u32>,
    value: &CellValue,
) -> Vec<u8> {
    let c = context.qualified(source, "c");
    let style = style.map_or(String::new(), |style| format!(" s=\"{style}\""));
    let kind = value_type(value).map_or(String::new(), |kind| format!(" t=\"{kind}\""));
    let mut output = format!("<{c} r=\"{cell}\"{style}{kind}>").into_bytes();
    output.extend(payload(source, context, value));
    output.extend_from_slice(format!("</{c}>").as_bytes());
    output
}

// The workbook-wide inputs that decide whether an edit rewrites a cell's
// current value: its shared strings, loaded only to compare text, and its
// date system.
struct CurrentValues<F> {
    shared_strings: F,
    date1904: bool,
}

// Whether `value` is the cell's current payload, as it reads. Decoding the old
// value only detects no-op edits, so an undecodable old value counts as a
// change rather than blocking its replacement; `validate_payload` already
// refused unfamiliar markup that a replacement would discard.
fn unchanged_value<'s>(
    xml: &[u8],
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
    match read::decode_payload(xml, index, cell, payload, current.date1904) {
        Ok(StoredValue::Value(old)) => old == *value,
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

fn patched_cell<'s>(
    xml: &[u8],
    index: &WorksheetIndex,
    address: CellRef,
    cell_index: usize,
    value: &CellValue,
    edits: &mut EditPlan,
    current: &mut CurrentValues<impl FnMut() -> Option<&'s SharedStrings>>,
) -> Result<bool> {
    let (source, elements, attributes, namespace) = (
        index.source,
        &index.elements,
        &index.attributes,
        index.namespace,
    );
    let cell = &elements[cell_index];
    let value_child = read::value_payload(index, cell).map_err(|error| error.context(address))?;
    if let Some(child) = value_child {
        validation::validate_payload(source, elements, attributes, child, namespace)
            .map_err(|error| error.context(address))?;
    }
    let unchanged = if let Some(child) = value_child {
        unchanged_value(xml, index, cell, child, value, current)
    } else {
        matches!(value, CellValue::Blank) && cell.attribute(attributes, "t").is_none()
    };
    if unchanged {
        return Ok(false);
    }
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
    let value_bytes = payload(source, cell, value);
    if let Some(type_edit) = attribute_edit(cell, attributes, "t", value_type(value)) {
        edits.push(type_edit);
    }
    if cell.empty {
        if !value_bytes.is_empty() {
            edits.expand(source, cell, value_bytes);
        }
        return Ok(true);
    }
    if let Some(child) = value_child {
        edits.push(Edit {
            start: elements[child].start(),
            end: elements[child].end(),
            bytes: value_bytes,
        });
    } else if !value_bytes.is_empty() {
        let position = cell
            .children(elements)
            .map(|child| &elements[child])
            .find(|child| child.is(source, namespace, "extLst"))
            .map_or(cell.close_start(), Element::start);
        edits.push(Edit {
            start: position,
            end: position,
            bytes: value_bytes,
        });
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
    xml: &[u8],
    index: &WorksheetIndex,
    row: &RowEntry,
    changes: &RowChanges,
    edits: &mut EditPlan,
    changed_cells: &mut Vec<CellRef>,
    current: &mut CurrentValues<impl FnMut() -> Option<&'s SharedStrings>>,
) -> Result<()> {
    let (source, elements) = (index.source, &index.elements);
    let element = &elements[row.element];
    let existing_cells = &index.cells[row.cells.clone()];
    let mut cell_position = 0;
    let mut changed_columns = Vec::new();
    let mut empty_row_cells = Vec::new();
    // Resolve only when a new nonblank cell needs a style. Existing cells and
    // blank edits must not start validating row defaults; later insertions can
    // reuse the first result without rescanning attributes or numeric text.
    let mut cached_row_style: Option<RowStyle> = None;
    // Trailing cells follow the last existing cell. Only an extension list
    // after that cell comes later; one placed before or between cells must not
    // move new cells ahead of existing ones.
    let after_cells = existing_cells
        .last()
        .map_or(element.open_end(), |cell| elements[cell.element].end());
    let fallback = element
        .children(elements)
        .map(|child| &elements[child])
        .find(|child| child.start() >= after_cells && child.is(source, index.namespace, "extLst"))
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
            if !patched_cell(xml, index, cell, existing.element, value, edits, current)? {
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
            let bytes = new_cell(source, element, cell, style, value);
            if element.empty {
                empty_row_cells.extend(bytes);
            } else {
                let insertion = existing_cells
                    .get(cell_position)
                    .map_or(fallback, |next| elements[next.element].start());
                edits.push(Edit {
                    start: insertion,
                    end: insertion,
                    bytes,
                });
            }
        }
        changed_cells.push(cell);
        changed_columns.push(cell.column);
    }
    if changed_columns.is_empty() {
        return Ok(());
    }
    let spans = spans_edit(
        element,
        row.number,
        &index.attributes,
        changed_columns.into_iter(),
    )?;
    if let Some(spans) = spans {
        edits.push(spans);
    }
    if element.empty {
        edits.expand(source, element, empty_row_cells);
    }
    Ok(())
}

// The markup of a new row holding a row number's non-blank values, or `None`
// when every value is blank and the row is not needed.
fn new_row(
    index: &WorksheetIndex,
    number: u32,
    changes: &RowChanges,
    changed_cells: &mut Vec<CellRef>,
) -> Result<Option<Vec<u8>>> {
    let data = &index.elements[index.data_index];
    let mut cells = Vec::new();
    for &(&cell, value) in changes {
        if !matches!(value, CellValue::Blank) {
            let style = index.column_style(cell)?;
            cells.extend(new_cell(index.source, data, cell, style, value));
            changed_cells.push(cell);
        }
    }
    if cells.is_empty() {
        return Ok(None);
    }
    let row = data.qualified(index.source, "row");
    let mut bytes = format!("<{row} r=\"{number}\">").into_bytes();
    bytes.extend(cells);
    bytes.extend_from_slice(format!("</{row}>").as_bytes());
    Ok(Some(bytes))
}

// Widens the worksheet's dimension to include every changed cell. A reference
// that already covers them keeps its exact bytes.
fn dimension_edit(index: &WorksheetIndex, changed_cells: &[CellRef]) -> Result<Option<Edit>> {
    if changed_cells.is_empty() {
        return Ok(None);
    }
    let (source, elements) = (index.source, &index.elements);
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
    let range = changed_cells
        .iter()
        .fold(old, |range, &cell| range.including(cell));
    if old.contains(range.first()) && old.contains(range.last()) {
        return Ok(None);
    }
    Ok(attribute_edit(
        dimension,
        &index.attributes,
        "ref",
        Some(&range.to_string()),
    ))
}

pub(super) fn plan_cells<'s>(
    xml: &[u8],
    index: &WorksheetIndex,
    cells: &BTreeMap<CellRef, CellValue>,
    shared_strings: impl FnMut() -> Option<&'s SharedStrings>,
    date1904: bool,
) -> Result<EditPlan> {
    if cells.is_empty() {
        return Ok(EditPlan::default());
    }
    index.validate_edits(cells)?;
    // The table is needed only to recognize rewriting a shared string's own
    // text; a table that is absent or fails to load leaves such edits as
    // changes. The workbook loads it once and keeps it.
    let mut current = CurrentValues {
        shared_strings,
        date1904,
    };
    let data = &index.elements[index.data_index];
    let mut edits = EditPlan::default();
    let mut changed_cells = Vec::new();
    // New rows for an empty `<sheetData/>` share one expansion.
    let mut empty_data_rows = Vec::new();
    let mut pending = cells.iter().peekable();
    let mut row_position = 0;
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
        let mut changes: Vec<(&CellRef, &CellValue)> = Vec::new();
        while let Some(change) = pending.next_if(|(cell, _)| cell.row == number) {
            changes.push(change);
        }
        let next_row = index.rows.get(row_position);
        if let Some(row) = next_row.filter(|row| row.number == number) {
            patch_existing_row(
                xml,
                index,
                row,
                &changes,
                &mut edits,
                &mut changed_cells,
                &mut current,
            )?;
        } else if let Some(bytes) = new_row(index, number, &changes, &mut changed_cells)? {
            if data.empty {
                empty_data_rows.extend(bytes);
            } else {
                let insertion = next_row.map_or(data.close_start(), |row| {
                    index.elements[row.element].start()
                });
                edits.push(Edit {
                    start: insertion,
                    end: insertion,
                    bytes,
                });
            }
        }
    }
    if !empty_data_rows.is_empty() {
        edits.expand(index.source, data, empty_data_rows);
    }
    if let Some(edit) = dimension_edit(index, &changed_cells)? {
        edits.push(edit);
    }
    Ok(edits)
}
