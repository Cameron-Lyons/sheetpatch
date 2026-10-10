use super::index::{WorksheetIndex, reference_range};
use super::parse::{Attribute, Element, NamespaceId};
use super::read::{element_text, rich_text};
use super::validation::validate_payload;
use super::{unique, unsupported};
use crate::xml::{excel_text, trim_whitespace as trim_xml_whitespace};
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
    let mut output = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    for (position, character) in text.char_indices() {
        // Escape literal Excel control-escape tokens so Excel shows the exact
        // user string rather than interpreting, for example, _x0041_ as A.
        if character == '_'
            && position + 7 <= bytes.len()
            && matches!(bytes[position + 1], b'x' | b'X')
            && bytes[position + 2..position + 6]
                .iter()
                .all(u8::is_ascii_hexdigit)
            && bytes[position + 6] == b'_'
        {
            output.push_str("_x005F_");
            continue;
        }
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

fn new_cell(context: &Element, cell: CellRef, value: &CellValue) -> Vec<u8> {
    let c = context.qualified("c");
    let kind = value_type(value).map_or(String::new(), |kind| format!(" t=\"{kind}\""));
    let mut output = format!("<{c} r=\"{cell}\"{kind}>").into_bytes();
    output.extend(payload(context, value));
    output.extend_from_slice(format!("</{c}>").as_bytes());
    output
}

fn patched_cell(
    xml: &[u8],
    elements: &[Element],
    attributes: &[Attribute],
    index: usize,
    namespace: NamespaceId,
    value: &CellValue,
    edits: &mut EditPlan,
) -> Result<bool> {
    let cell = &elements[index];
    let value_child = unique(
        cell.children(elements).filter(|&child| {
            elements[child].is(namespace, "v") || elements[child].is(namespace, "is")
        }),
        "cell has multiple value payloads",
    )?;
    if let Some(child) = value_child {
        validate_payload(elements, attributes, child, namespace)?;
    }
    let kind = cell
        .attr(attributes, "t")
        .map_or("n", |attribute| attribute.value.as_ref());
    let unchanged = if let Some(child) = value_child {
        let old = &elements[child];
        match value {
            CellValue::Number(number) if kind == "n" && old.is(namespace, "v") => {
                trim_xml_whitespace(&element_text(xml, old)?)
                    .parse::<f64>()
                    .ok()
                    == Some(*number)
            }
            CellValue::Bool(boolean) if kind == "b" && old.is(namespace, "v") => {
                match trim_xml_whitespace(&element_text(xml, old)?) {
                    "1" | "true" => *boolean,
                    "0" | "false" => !*boolean,
                    _ => false,
                }
            }
            CellValue::Text(text) if kind == "inlineStr" && old.is(namespace, "is") => {
                rich_text(xml, elements, child, namespace)? == *text
            }
            CellValue::Text(text) if kind == "str" && old.is(namespace, "v") => {
                excel_text(&element_text(xml, old)?)? == *text
            }
            CellValue::Error(error) if kind == "e" && old.is(namespace, "v") => {
                excel_text(&element_text(xml, old)?)? == *error
            }
            _ => false,
        }
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
        return Err(unsupported("cell value has associated metadata"));
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
    attributes: &[Attribute],
    columns: impl Iterator<Item = u16>,
) -> Result<Option<Edit>> {
    let Some(attribute) = row.attr(attributes, "spans") else {
        return Ok(None);
    };
    let mut spans = Vec::new();
    for span in attribute.value.split_whitespace() {
        let Some((first, last)) = span.split_once(':') else {
            return Err(unsupported("invalid row spans"));
        };
        let first = first
            .parse::<u16>()
            .map_err(|_| unsupported("invalid row spans"))?;
        let last = last
            .parse::<u16>()
            .map_err(|_| unsupported("invalid row spans"))?;
        if first == 0 || first > last || last > 16_384 {
            return Err(unsupported("invalid row spans"));
        }
        spans.push((first, last));
    }
    if spans.is_empty() {
        return Err(unsupported("empty row spans"));
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

pub(super) fn patch_cells<'a>(
    xml: &'a [u8],
    index: &WorksheetIndex,
    cells: &BTreeMap<CellRef, CellValue>,
) -> Result<Cow<'a, [u8]>> {
    if cells.is_empty() {
        return Ok(Cow::Borrowed(xml));
    }
    index.validate_edits(cells)?;
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
            let fallback = row
                .children(elements)
                .map(|index| &elements[index])
                .find(|child| child.is(namespace, "extLst"))
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
                        xml, elements, attributes, cell_index, namespace, value, &mut edits,
                    )? {
                        continue;
                    }
                } else if matches!(value, CellValue::Blank) {
                    continue;
                } else if row.empty {
                    empty_row_cells.extend(new_cell(row, cell, value));
                } else {
                    let insertion = existing_cells
                        .get(cell_position)
                        .map_or(fallback, |&(_, index)| elements[index].start);
                    edits.push(Edit {
                        start: insertion,
                        end: insertion,
                        bytes: new_cell(row, cell, value),
                    });
                }
                changed_cells.push(cell);
                changed_columns.push(cell.column);
            }
            if !changed_columns.is_empty() {
                let span = spans_edit(row, attributes, changed_columns.into_iter())?;
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
                    new_cells.extend(new_cell(data, cell, value));
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
