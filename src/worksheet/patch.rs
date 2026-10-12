use super::index::{WorksheetIndex, reference_range};
use super::parse::{Attribute, Element};
use super::read::{element_text, rich_text};
use super::validation::validate_payload;
use super::{at_cell, unique, unsupported};
use crate::xml::{excel_escaped, excel_text, trim_whitespace as trim_xml_whitespace};
use crate::{CellRef, CellValue, Result};
use std::{borrow::Cow, collections::BTreeMap};

struct Edit {
    start: usize,
    end: usize,
    bytes: Vec<u8>,
}

/// Nonoverlapping changes against the original worksheet's absolute byte spans.
/// Existing cell/row/extension bytes are copied only when the final XML is emitted.
#[derive(Default)]
struct EditPlan {
    edits: Vec<Edit>,
}

impl EditPlan {
    fn push(&mut self, edit: Edit) {
        self.edits.push(edit);
    }

    fn expand(&mut self, element: &Element, mut contents: Vec<u8>) {
        self.push(Edit {
            start: element.open_end - 2,
            end: element.open_end - 1,
            bytes: Vec::new(),
        });
        contents.extend_from_slice(format!("</{}>", element.name).as_bytes());
        self.push(Edit {
            start: element.open_end,
            end: element.open_end,
            bytes: contents,
        });
    }

    fn apply(mut self, xml: &[u8]) -> Result<Cow<'_, [u8]>> {
        if self.edits.is_empty() {
            return Ok(Cow::Borrowed(xml));
        }
        // Stable order is significant for several new cells/rows at one gap.
        self.edits.sort_by_key(|edit| (edit.start, edit.end));
        let mut position = 0;
        let mut length = 0usize;
        for edit in &self.edits {
            if edit.start < position || edit.end < edit.start || edit.end > xml.len() {
                return Err(unsupported("overlapping XML edits"));
            }
            length = length
                .checked_add(edit.start - position)
                .and_then(|length| length.checked_add(edit.bytes.len()))
                .ok_or_else(|| unsupported("patched XML exceeds addressable memory"))?;
            position = edit.end;
        }
        length = length
            .checked_add(xml.len() - position)
            .ok_or_else(|| unsupported("patched XML exceeds addressable memory"))?;
        let mut output = Vec::with_capacity(length);
        position = 0;
        for edit in self.edits {
            output.extend_from_slice(&xml[position..edit.start]);
            output.extend(edit.bytes);
            position = edit.end;
        }
        output.extend_from_slice(&xml[position..]);
        Ok(Cow::Owned(output))
    }
}

fn attribute_edit(
    element: &Element,
    attributes: &[Attribute],
    name: &str,
    value: Option<&str>,
) -> Option<Edit> {
    if let Some(attribute) = element.attr(attributes, name) {
        match value {
            Some(value) if value != attribute.value => Some(Edit {
                start: attribute.value_start,
                end: attribute.value_end,
                bytes: value.as_bytes().to_vec(),
            }),
            None => Some(Edit {
                start: attribute.start,
                end: attribute.end,
                bytes: Vec::new(),
            }),
            _ => None,
        }
    } else {
        value.map(|value| Edit {
            start: element.open_end - if element.empty { 2 } else { 1 },
            end: element.open_end - if element.empty { 2 } else { 1 },
            bytes: format!(" {name}=\"{value}\"").into_bytes(),
        })
    }
}

pub(super) fn text_escaped(text: &str) -> String {
    // Excel escapes come first: characters XML cannot carry are stored as
    // `_xHHHH_`, and literal tokens are escaped so Excel shows the exact user
    // string rather than interpreting, for example, _x0041_ as A.
    let text = excel_escaped(text);
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

fn payload(prefix: &Element, value: &CellValue) -> Vec<u8> {
    let v = prefix.qualified("v");
    match value {
        CellValue::Text(text) => {
            let is = prefix.qualified("is");
            let t = prefix.qualified("t");
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

fn new_cell(context: &Element, cell: CellRef, style: Option<u32>, value: &CellValue) -> Vec<u8> {
    let c = context.qualified("c");
    let style = style.map_or(String::new(), |style| format!(" s=\"{style}\""));
    let kind = value_type(value).map_or(String::new(), |kind| format!(" t=\"{kind}\""));
    let mut output = format!("<{c} r=\"{cell}\"{style}{kind}>").into_bytes();
    output.extend(payload(context, value));
    output.extend_from_slice(format!("</{c}>").as_bytes());
    output
}

// Whether `value` matches a cell's current payload. Decoding the old value only
// detects no-op edits, so an undecodable old value counts as a change rather
// than blocking its replacement; `validate_payload` already refused unfamiliar
// markup that a replacement would discard.
fn unchanged_value<'s>(
    xml: &[u8],
    index: &WorksheetIndex,
    kind: &str,
    payload: usize,
    value: &CellValue,
    shared_strings: &mut impl FnMut() -> Option<&'s [String]>,
) -> bool {
    let (elements, namespace) = (&index.elements, index.namespace);
    let old = &elements[payload];
    let old_text = || element_text(xml, old).ok();
    match value {
        CellValue::Number(number) if kind == "n" && old.is(namespace, "v") => old_text()
            .and_then(|text| trim_xml_whitespace(&text).parse::<f64>().ok())
            .is_some_and(|old| old == *number),
        CellValue::Bool(boolean) if kind == "b" && old.is(namespace, "v") => old_text()
            .is_some_and(|text| match trim_xml_whitespace(&text) {
                "1" | "true" => *boolean,
                "0" | "false" => !*boolean,
                _ => false,
            }),
        CellValue::Text(text) if kind == "inlineStr" && old.is(namespace, "is") => {
            rich_text(xml, elements, payload, namespace).is_ok_and(|old| old == *text)
        }
        CellValue::Text(text) if kind == "s" && old.is(namespace, "v") => old_text()
            .and_then(|position| trim_xml_whitespace(&position).parse::<usize>().ok())
            .and_then(|position| shared_strings()?.get(position))
            .is_some_and(|old| old == text),
        CellValue::Text(text) if kind == "str" && old.is(namespace, "v") => old_text()
            .and_then(|old| excel_text(&old).ok())
            .is_some_and(|old| old == *text),
        CellValue::Error(error) if kind == "e" && old.is(namespace, "v") => old_text()
            .and_then(|old| excel_text(&old).ok())
            .is_some_and(|old| old == *error),
        _ => false,
    }
}

fn patched_cell<'s>(
    xml: &[u8],
    index: &WorksheetIndex,
    address: CellRef,
    cell_index: usize,
    value: &CellValue,
    edits: &mut EditPlan,
    shared_strings: &mut impl FnMut() -> Option<&'s [String]>,
) -> Result<bool> {
    let (elements, attributes, namespace) = (&index.elements, &index.attributes, index.namespace);
    let cell = &elements[cell_index];
    let value_child = unique(
        cell.children(elements).filter(|&child| {
            elements[child].is(namespace, "v") || elements[child].is(namespace, "is")
        }),
        "cell has multiple value payloads",
    )
    .map_err(|error| at_cell(address, error))?;
    if let Some(child) = value_child {
        validate_payload(elements, attributes, child, namespace)
            .map_err(|error| at_cell(address, error))?;
    }
    let kind = cell
        .attr(attributes, "t")
        .map_or("n", |attribute| attribute.value.as_ref());
    let unchanged = if let Some(child) = value_child {
        unchanged_value(xml, index, kind, child, value, shared_strings)
    } else {
        matches!(value, CellValue::Blank) && cell.attr(attributes, "t").is_none()
    };
    if unchanged {
        return Ok(false);
    }
    // Unlike cell metadata (`cm`), value metadata describes the old value and
    // can link to rich data in other package parts. Retaining that link after
    // replacing a scalar payload would silently leave stale value semantics.
    if cell.attr(attributes, "vm").is_some() {
        return Err(unsupported(format!(
            "{address}: cell value has associated metadata"
        )));
    }
    let value_bytes = payload(cell, value);
    if let Some(kind) = attribute_edit(cell, attributes, "t", value_type(value)) {
        edits.push(kind);
    }
    if cell.empty {
        if !value_bytes.is_empty() {
            edits.expand(cell, value_bytes);
        }
        return Ok(true);
    }
    if let Some(child) = value_child {
        edits.push(Edit {
            start: elements[child].start,
            end: elements[child].end,
            bytes: value_bytes,
        });
    } else if !value_bytes.is_empty() {
        let position = cell
            .children(elements)
            .map(|child| &elements[child])
            .find(|child| child.is(namespace, "extLst"))
            .map_or(cell.close_start, |child| child.start);
        edits.push(Edit {
            start: position,
            end: position,
            bytes: value_bytes,
        });
    }
    Ok(true)
}

fn spans_edit(
    row: &Element,
    row_number: u32,
    attributes: &[Attribute],
    columns: impl Iterator<Item = u16>,
) -> Result<Option<Edit>> {
    let Some(attribute) = row.attr(attributes, "spans") else {
        return Ok(None);
    };
    let invalid = || unsupported(format!("row {row_number}: invalid row spans"));
    let mut spans = Vec::new();
    for span in attribute.value.split_whitespace() {
        let Some((first, last)) = span.split_once(':') else {
            return Err(invalid());
        };
        let first = first.parse::<u16>().map_err(|_| invalid())?;
        let last = last.parse::<u16>().map_err(|_| invalid())?;
        if first == 0 || first > last || last > 16_384 {
            return Err(invalid());
        }
        spans.push((first, last));
    }
    if spans.is_empty() {
        return Err(unsupported(format!("row {row_number}: empty row spans")));
    }
    let mut first = spans.iter().map(|span| span.0).min().unwrap();
    let mut last = spans.iter().map(|span| span.1).max().unwrap();
    let mut changed = false;
    for column in columns {
        changed |= !spans
            .iter()
            .any(|&(start, end)| (start..=end).contains(&column));
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

// Excel gives a new cell its row's style when the row has a custom format, and
// otherwise its column's default style. Style zero needs no attribute.
fn default_style(
    index: &WorksheetIndex,
    row: Option<&Element>,
    cell: CellRef,
) -> Result<Option<u32>> {
    if let Some(row) = row {
        let attributes = &index.attributes;
        let custom_format = row
            .attr(attributes, "customFormat")
            .map(|attribute| trim_xml_whitespace(&attribute.value));
        if !matches!(custom_format, None | Some("0" | "false")) {
            if !matches!(custom_format, Some("1" | "true")) {
                return Err(unsupported(format!("{cell}: invalid row format flag")));
            }
            let Some(style) = row.attr(attributes, "s") else {
                return Ok(None);
            };
            return trim_xml_whitespace(&style.value)
                .parse::<u32>()
                .map(|style| (style != 0).then_some(style))
                .map_err(|_| unsupported(format!("{cell}: invalid row style index")));
        }
    }
    index.column_style(cell)
}

pub(super) fn patch_cells<'a, 's>(
    xml: &'a [u8],
    index: &WorksheetIndex,
    cells: &BTreeMap<CellRef, CellValue>,
    mut shared_strings: impl FnMut() -> Result<&'s [String]>,
) -> Result<Cow<'a, [u8]>> {
    if cells.is_empty() {
        return Ok(Cow::Borrowed(xml));
    }
    index.validate_edits(cells)?;
    // The table is needed only to recognize rewriting a shared string's own
    // text. Load it at most once; a failure leaves such edits as changes.
    let mut loaded_strings = None;
    let mut strings = || *loaded_strings.get_or_insert_with(|| shared_strings().ok());
    let elements = &index.elements;
    let attributes = &index.attributes;
    let namespace = index.namespace;
    let data = &elements[index.data_index];
    let mut edits = EditPlan::default();
    let mut changed_cells = Vec::new();
    let mut pending = cells.iter().peekable();
    let mut row_position = 0;
    let mut empty_data_rows = Vec::new();
    while let Some(&(&address, _)) = pending.peek() {
        let row_number = address.row;
        while row_position < index.rows.len() && index.rows[row_position].0 < row_number {
            row_position += 1;
        }
        let existing_row = index
            .rows
            .get(row_position)
            .filter(|&&(number, _, _)| number == row_number);
        let mut row_changes = Vec::new();
        while pending
            .peek()
            .is_some_and(|(address, _)| address.row == row_number)
        {
            row_changes.push(pending.next().unwrap());
        }
        if let Some((_, row_index, existing_cells)) = existing_row {
            let row = &elements[*row_index];
            let existing_cells = &index.cells[existing_cells.clone()];
            let mut cell_position = 0;
            let mut changed_columns = Vec::new();
            let mut empty_row_cells = Vec::new();
            // Trailing cells follow the last existing cell. Only an extension
            // list after that cell comes later; one placed before or between
            // cells must not move new cells ahead of existing ones.
            let after_cells = existing_cells
                .last()
                .map_or(row.open_end, |&(_, index)| elements[index].end);
            let fallback = row
                .children(elements)
                .map(|index| &elements[index])
                .find(|child| child.start >= after_cells && child.is(namespace, "extLst"))
                .map_or(row.close_start, |child| child.start);
            for (&cell, value) in row_changes {
                while cell_position < existing_cells.len()
                    && existing_cells[cell_position].0.column < cell.column
                {
                    cell_position += 1;
                }
                let existing = existing_cells
                    .get(cell_position)
                    .filter(|&&(address, _)| address == cell);
                if let Some(&(_, cell_index)) = existing {
                    if !patched_cell(
                        xml,
                        index,
                        cell,
                        cell_index,
                        value,
                        &mut edits,
                        &mut strings,
                    )? {
                        continue;
                    }
                } else if matches!(value, CellValue::Blank) {
                    continue;
                } else {
                    let bytes = new_cell(row, cell, default_style(index, Some(row), cell)?, value);
                    if row.empty {
                        empty_row_cells.extend(bytes);
                    } else {
                        let insertion = existing_cells
                            .get(cell_position)
                            .map_or(fallback, |&(_, index)| elements[index].start);
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
            if !changed_columns.is_empty() {
                let span = spans_edit(row, row_number, attributes, changed_columns.into_iter())?;
                if let Some(span) = span {
                    edits.push(span);
                }
                if row.empty {
                    edits.expand(row, empty_row_cells);
                }
            }
        } else {
            let mut new_cells = Vec::new();
            for (&cell, value) in row_changes {
                if !matches!(value, CellValue::Blank) {
                    new_cells.extend(new_cell(
                        data,
                        cell,
                        default_style(index, None, cell)?,
                        value,
                    ));
                    changed_cells.push(cell);
                }
            }
            if !new_cells.is_empty() {
                let row_name = data.qualified("row");
                let mut bytes = format!("<{row_name} r=\"{row_number}\">").into_bytes();
                bytes.extend(new_cells);
                bytes.extend_from_slice(format!("</{row_name}>").as_bytes());
                if data.empty {
                    empty_data_rows.extend(bytes);
                } else {
                    let insertion = index
                        .rows
                        .get(row_position)
                        .map_or(data.close_start, |&(_, index, _)| elements[index].start);
                    edits.push(Edit {
                        start: insertion,
                        end: insertion,
                        bytes,
                    });
                }
            }
        }
    }
    if !empty_data_rows.is_empty() {
        edits.expand(data, empty_data_rows);
    }
    if !changed_cells.is_empty() {
        let dimension = unique(
            elements[0]
                .children(elements)
                .map(|index| &elements[index])
                .filter(|element| element.is(namespace, "dimension")),
            "multiple worksheet dimensions",
        )?;
        if let Some(dimension) = dimension {
            let reference = dimension
                .attr(attributes, "ref")
                .ok_or_else(|| unsupported("dimension without a reference"))?;
            let old = reference_range(&reference.value)?;
            let range = changed_cells
                .iter()
                .fold(old, |range, &cell| range.including(cell));
            if (!old.contains(range.first) || !old.contains(range.last))
                && let Some(edit) =
                    attribute_edit(dimension, attributes, "ref", Some(&range.reference()))
            {
                edits.push(edit);
            }
        }
    }
    edits.apply(xml)
}
