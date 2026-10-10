use super::index::Range;
use super::parse::{Attribute, Element, NamespaceId};
use super::unsupported;
use crate::{CellRef, CellValue, Result};
use std::collections::BTreeMap;

// Known rich text is the old value and may be replaced. Unfamiliar markup
// within that value could carry other meaning: refuse to discard it.
pub(super) fn validate_payload(
    elements: &[Element],
    attributes: &[Attribute],
    index: usize,
    namespace: NamespaceId,
) -> Result<()> {
    let element = &elements[index];
    if element.namespace != namespace || element.opaque_markup {
        return Err(unsupported("unfamiliar XML inside the cell value payload"));
    }
    let allowed_attributes: &[&str] = match element.local_name() {
        "v" | "is" | "r" | "rPr" => &[],
        "t" => &["xml:space"],
        "rPh" => &["sb", "eb"],
        "phoneticPr" => &["fontId", "type", "alignment"],
        "color" => &["auto", "indexed", "rgb", "theme", "tint"],
        "rFont" | "charset" | "family" | "b" | "i" | "strike" | "outline" | "shadow"
        | "condense" | "extend" | "sz" | "u" | "vertAlign" | "scheme" => &["val"],
        _ => return Err(unsupported("unfamiliar XML inside the cell value payload")),
    };
    if attributes[element.attributes.clone()]
        .iter()
        .any(|attribute| {
            attribute.name != "xmlns"
                && !attribute.name.starts_with("xmlns:")
                && !allowed_attributes.contains(&attribute.name)
        })
    {
        return Err(unsupported(
            "unfamiliar attributes inside the cell value payload",
        ));
    }
    if matches!(element.local_name(), "v" | "t") && element.has_children() {
        return Err(unsupported("elements inside a simple cell value payload"));
    }
    for child in element.children(elements) {
        validate_payload(elements, attributes, child, namespace)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct GuardEvent {
    row: u32,
    first_column: u16,
    last_column: u16,
    delta: i32,
}

// Sorting integer row coordinates with a radix pass keeps large collections of
// formula/merged ranges linear in the worksheet size. A small Fenwick tree then
// tests each edited column against active ranges in at most 15 steps.
pub(super) fn sort_guard_events(events: &mut Vec<GuardEvent>) {
    if events.len() < 2 {
        return;
    }
    let mut scratch = events.clone();
    for shift in [0, 8, 16] {
        let mut counts = [0usize; 256];
        for event in events.iter() {
            counts[((event.row >> shift) & 255) as usize] += 1;
        }
        let mut total = 0;
        for count in &mut counts {
            let size = *count;
            *count = total;
            total += size;
        }
        for &event in events.iter() {
            let bucket = ((event.row >> shift) & 255) as usize;
            scratch[counts[bucket]] = event;
            counts[bucket] += 1;
        }
        std::mem::swap(events, &mut scratch);
    }
}

pub(super) fn validate_ranges(
    cells: &BTreeMap<CellRef, CellValue>,
    ranges: &[Range],
    merged: bool,
) -> Result<()> {
    if cells.is_empty() || ranges.is_empty() {
        return Ok(());
    }
    let violation = |cell: CellRef| {
        unsupported(if merged {
            format!("{cell} is not the anchor of its merged range")
        } else {
            format!("{cell} belongs to a formula range")
        })
    };
    // With either dimension small, direct membership checks cost less than
    // sorting events and allocating a column index. Large batches still use
    // the sweep so their work does not grow as cells times ranges.
    if cells.len() <= 8 || ranges.len() <= 8 || cells.len().saturating_mul(ranges.len()) <= 256 {
        for &cell in cells.keys() {
            if ranges
                .iter()
                .any(|range| range.contains(cell) && (!merged || cell != range.first))
            {
                return Err(violation(cell));
            }
        }
        return Ok(());
    }
    let first_row = cells.first_key_value().unwrap().0.row;
    let last_row = cells.last_key_value().unwrap().0.row;
    let (first_column, last_column) = cells.keys().fold((u16::MAX, 0), |(first, last), cell| {
        (first.min(cell.column), last.max(cell.column))
    });
    // A rectangle outside the edited envelope cannot contain a requested
    // cell. Count relevant ranges before reserving event storage, so unrelated
    // worksheet ranges add no memory overhead to a batch.
    let relevant_ranges = ranges.iter().filter(|range| {
        range.first.row <= last_row
            && range.last.row >= first_row
            && range.first.column <= last_column
            && range.last.column >= first_column
    });
    let relevant_count = relevant_ranges.clone().count();
    if relevant_count == 0 {
        return Ok(());
    }
    let mut events = Vec::with_capacity(relevant_count * if merged { 4 } else { 2 });
    let mut add_range = |first_row, last_row, first_column, last_column| {
        if first_row <= last_row && first_column <= last_column {
            events.push(GuardEvent {
                row: first_row,
                first_column,
                last_column,
                delta: 1,
            });
            events.push(GuardEvent {
                row: last_row + 1,
                first_column,
                last_column,
                delta: -1,
            });
        }
    };
    for range in relevant_ranges {
        if merged {
            // The anchor remains editable; the rest of its rectangle does not.
            add_range(
                range.first.row,
                range.first.row,
                range.first.column + 1,
                range.last.column,
            );
            add_range(
                range.first.row + 1,
                range.last.row,
                range.first.column,
                range.last.column,
            );
        } else {
            add_range(
                range.first.row,
                range.last.row,
                range.first.column,
                range.last.column,
            );
        }
    }
    if events.is_empty() {
        return Ok(());
    }
    sort_guard_events(&mut events);
    let mut active = vec![0i32; 16_386];
    let mut position = 0;
    for &cell in cells.keys() {
        while position < events.len() && events[position].row <= cell.row {
            let event = events[position];
            for (column, delta) in [
                (event.first_column as usize, event.delta),
                (event.last_column as usize + 1, -event.delta),
            ] {
                let mut index = column;
                while index < active.len() {
                    active[index] += delta;
                    index += index & index.wrapping_neg();
                }
            }
            position += 1;
        }
        let mut count = 0;
        let mut index = cell.column as usize;
        while index > 0 {
            count += active[index];
            index &= index - 1;
        }
        if count > 0 {
            return Err(violation(cell));
        }
    }
    Ok(())
}
