use super::parse::{Attribute, Element, NamespaceId, parse};
use super::validation::validate_ranges;
use super::{RowCells, unique, unsupported};
use crate::xml::trim_whitespace as trim_xml_whitespace;
use crate::{CellRef, CellValue, Result};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Copy)]
pub(super) struct Range {
    pub(super) first: CellRef,
    pub(super) last: CellRef,
}
impl Range {
    pub(super) fn contains(self, cell: CellRef) -> bool {
        cell.row >= self.first.row
            && cell.row <= self.last.row
            && cell.column >= self.first.column
            && cell.column <= self.last.column
    }
    pub(super) fn including(self, cell: CellRef) -> Self {
        Self {
            first: CellRef {
                row: self.first.row.min(cell.row),
                column: self.first.column.min(cell.column),
            },
            last: CellRef {
                row: self.last.row.max(cell.row),
                column: self.last.column.max(cell.column),
            },
        }
    }
    pub(super) fn reference(self) -> String {
        if self.first == self.last {
            self.first.to_string()
        } else {
            format!("{}:{}", self.first, self.last)
        }
    }
}

pub(super) fn reference_range(reference: &str) -> Result<Range> {
    let mut parts = reference.split(':');
    let first: CellRef = parts
        .next()
        .unwrap_or_default()
        .replace('$', "")
        .parse()
        .map_err(|_| unsupported(format!("invalid cell range {reference:?}")))?;
    let last = if let Some(last) = parts.next() {
        last.replace('$', "")
            .parse()
            .map_err(|_| unsupported(format!("invalid cell range {reference:?}")))?
    } else {
        first
    };
    if parts.next().is_some() || first.row > last.row || first.column > last.column {
        return Err(unsupported(format!("invalid cell range {reference:?}")));
    }
    Ok(Range { first, last })
}

// A worksheet's structure is indexed once. Byte spans remain relative to the
// original document, so unrelated whitespace, prefixes and markup never move.
pub(super) struct WorksheetIndex<'a> {
    pub(super) elements: Vec<Element<'a>>,
    pub(super) attributes: Vec<Attribute<'a>>,
    pub(super) cells: Vec<(CellRef, usize)>,
    pub(super) namespace: NamespaceId,
    pub(super) data_index: usize,
    pub(super) rows: Vec<RowCells>,
    pub(super) formula_cells: HashSet<CellRef>,
    pub(super) formula_ranges: Vec<Range>,
    pub(super) merged_ranges: Vec<Range>,
    pub(super) incomplete_formula_group: bool,
}

impl<'a> WorksheetIndex<'a> {
    pub(super) fn parse(xml: &'a [u8]) -> Result<Self> {
        let document = parse(xml)?;
        let root = &document.elements[0];
        let namespace = root.namespace;
        if namespace.is_unbound() {
            return Err(unsupported("worksheet has no OOXML namespace"));
        }
        if root.local_name() != "worksheet" || !document.is_spreadsheet_namespace(namespace) {
            return Err(unsupported("expected an OOXML worksheet"));
        }
        let super::parse::XmlDocument {
            elements,
            attributes,
            ..
        } = document;
        let root = &elements[0];
        let data_index = unique(
            root.children(&elements)
                .filter(|&index| elements[index].is(namespace, "sheetData")),
            "worksheet must contain exactly one sheetData element",
        )?
        .ok_or_else(|| unsupported("worksheet must contain exactly one sheetData element"))?;
        let mut rows = Vec::new();
        let mut cells = Vec::new();
        let mut previous_row = 0;
        let mut shared: HashMap<u32, bool> = HashMap::new();
        let mut formula_cells = HashSet::new();
        let mut formula_ranges = Vec::new();
        let mut incomplete_formula_group = false;
        for index in elements[data_index].children(&elements) {
            let row = &elements[index];
            if !row.is(namespace, "row") {
                continue;
            }
            let row_reference = row
                .attr(&attributes, "r")
                .ok_or_else(|| unsupported("rows without explicit row numbers"))?;
            let row_number = trim_xml_whitespace(&row_reference.value)
                .parse::<u32>()
                .map_err(|_| unsupported("invalid row number"))?;
            if row_number <= previous_row || row_number > 1_048_576 {
                return Err(unsupported("duplicate, unsorted, or invalid row numbers"));
            }
            previous_row = row_number;
            let cells_start = cells.len();
            let mut previous_column = 0;
            for index in row.children(&elements) {
                let existing = &elements[index];
                if !existing.is(namespace, "c") {
                    continue;
                }
                let address: CellRef = existing
                    .attr(&attributes, "r")
                    .ok_or_else(|| unsupported("cells without explicit references"))?
                    .value
                    .parse()
                    .map_err(|_| unsupported("invalid cell reference in worksheet"))?;
                if address.row != row_number || address.column <= previous_column {
                    return Err(unsupported(
                        "duplicate, unsorted, or inconsistent cell references",
                    ));
                }
                previous_column = address.column;
                cells.push((address, index));
                for formula in existing.children(&elements) {
                    let formula = &elements[formula];
                    if !formula.is(namespace, "f") {
                        continue;
                    }
                    formula_cells.insert(address);
                    let range = formula
                        .attr(&attributes, "ref")
                        .map(|attribute| reference_range(&attribute.value))
                        .transpose()?;
                    if let Some(range) = range {
                        formula_ranges.push(range);
                    }
                    match formula
                        .attr(&attributes, "t")
                        .map(|attribute| attribute.value.as_ref())
                    {
                        Some("shared") => {
                            let index_reference = formula
                                .attr(&attributes, "si")
                                .ok_or_else(|| unsupported("shared formula without an index"))?;
                            let index = trim_xml_whitespace(&index_reference.value)
                                .parse::<u32>()
                                .map_err(|_| unsupported("invalid shared formula index"))?;
                            let has_range = shared.entry(index).or_insert(false);
                            *has_range |= range.is_some();
                        }
                        Some("array" | "dataTable") if range.is_none() => {
                            incomplete_formula_group = true;
                        }
                        _ => {}
                    }
                }
            }
            rows.push((row_number, index, cells_start..cells.len()));
        }
        incomplete_formula_group |= shared.values().any(|has_range| !has_range);
        let mut merged_ranges = Vec::new();
        for index in root.children(&elements) {
            let merges = &elements[index];
            if !merges.is(namespace, "mergeCells") {
                continue;
            }
            for index in merges.children(&elements) {
                let merge = &elements[index];
                if !merge.is(namespace, "mergeCell") {
                    continue;
                }
                let reference = merge
                    .attr(&attributes, "ref")
                    .ok_or_else(|| unsupported("merged cells without a reference"))?;
                merged_ranges.push(reference_range(&reference.value)?);
            }
        }
        // Main-namespace rows/cells hidden in another container (such as
        // AlternateContent) would make the visible cell addresses ambiguous.
        for element in &elements {
            if element.is(namespace, "row") && element.parent != Some(data_index) {
                return Err(unsupported("row outside sheetData"));
            }
            if element.is(namespace, "c")
                && !element.parent.is_some_and(|parent| {
                    elements[parent].is(namespace, "row")
                        && elements[parent].parent == Some(data_index)
                })
            {
                return Err(unsupported("cell outside a worksheet row"));
            }
            if element.is(namespace, "f")
                && !element
                    .parent
                    .is_some_and(|parent| elements[parent].is(namespace, "c"))
            {
                return Err(unsupported("formula outside a worksheet cell"));
            }
            if (element.is(namespace, "v") || element.is(namespace, "is"))
                && !element
                    .parent
                    .is_some_and(|parent| elements[parent].is(namespace, "c"))
            {
                return Err(unsupported("value payload outside a worksheet cell"));
            }
            if element.is(namespace, "mergeCells") && element.parent != Some(0) {
                return Err(unsupported("merged ranges outside the worksheet root"));
            }
            if element.is(namespace, "mergeCell")
                && !element.parent.is_some_and(|parent| {
                    elements[parent].is(namespace, "mergeCells")
                        && elements[parent].parent == Some(0)
                })
            {
                return Err(unsupported("merged range outside mergeCells"));
            }
        }
        Ok(Self {
            elements,
            attributes,
            cells,
            namespace,
            data_index,
            rows,
            formula_cells,
            formula_ranges,
            merged_ranges,
            incomplete_formula_group,
        })
    }

    pub(super) fn cell(&self, address: CellRef) -> Option<usize> {
        let row = self
            .rows
            .binary_search_by_key(&address.row, |&(row, _, _)| row)
            .ok()?;
        let cells = &self.cells[self.rows[row].2.clone()];
        let cell = cells
            .binary_search_by_key(&address.column, |&(cell, _)| cell.column)
            .ok()?;
        Some(cells[cell].1)
    }

    pub(super) fn validate_edits(&self, cells: &BTreeMap<CellRef, CellValue>) -> Result<()> {
        if self.incomplete_formula_group {
            return Err(unsupported("formula group without a reference range"));
        }
        for &cell in cells.keys() {
            if self.formula_cells.contains(&cell) {
                return Err(unsupported(format!("{cell} contains a formula")));
            }
        }
        validate_ranges(cells, &self.formula_ranges, false)?;
        validate_ranges(cells, &self.merged_ranges, true)
    }
}
