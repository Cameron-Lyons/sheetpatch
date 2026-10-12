use super::parse::{Attribute, Element, NamespaceId, XmlDocument, parse};
use super::validation::validate_ranges;
use super::{at_cell, protected, protection, unique, unsupported};
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

// Column default styles as sorted, nonoverlapping `(min, max, style)` intervals.
// Malformed or ambiguous definitions are reported only when a new cell needs
// its column's style, so reads and edits of existing cells are unaffected.
type ColumnStyles = std::result::Result<Vec<(u32, u32, u32)>, &'static str>;

fn column_styles<'a>(
    elements: &[Element<'a>],
    attributes: &[Attribute<'a>],
    namespace: NamespaceId,
) -> ColumnStyles {
    let number = |column: &Element, name, default| {
        column
            .attr(attributes, name)
            .map_or(Ok(default), |attribute| {
                trim_xml_whitespace(&attribute.value)
                    .parse::<u32>()
                    .map_err(|_| "invalid column definition")
            })
    };
    let mut columns = Vec::new();
    for index in elements[0].children(elements) {
        let definitions = &elements[index];
        if !definitions.is(namespace, "cols") {
            continue;
        }
        for index in definitions.children(elements) {
            let column = &elements[index];
            if !column.is(namespace, "col") {
                continue;
            }
            let first = number(column, "min", 0)?;
            let last = number(column, "max", 0)?;
            if first == 0 || first > last {
                return Err("invalid column definition");
            }
            columns.push((first, last, number(column, "style", 0)?));
        }
    }
    columns.sort_unstable();
    if columns.windows(2).any(|pair| pair[0].1 >= pair[1].0) {
        return Err("overlapping column definitions");
    }
    Ok(columns)
}

/// One `row` of `sheetData`, located in the index arenas.
pub(super) struct RowEntry {
    pub(super) number: u32,
    /// The row's index in `WorksheetIndex::elements`.
    pub(super) element: usize,
    /// The row's cells, as a range of `WorksheetIndex::cells`.
    pub(super) cells: std::ops::Range<usize>,
}

// Formula cells and the ranges their groups protect.
#[derive(Default)]
struct Formulas {
    cells: HashSet<CellRef>,
    ranges: Vec<Range>,
    // Each shared-formula index maps to its first cell and whether any
    // member of the group carries the group's reference range.
    shared: HashMap<u32, (CellRef, bool)>,
    incomplete_group: Option<CellRef>,
}

impl Formulas {
    fn add(&mut self, address: CellRef, formula: &Element, attributes: &[Attribute]) -> Result<()> {
        self.cells.insert(address);
        let range = formula
            .attr(attributes, "ref")
            .map(|attribute| reference_range(&attribute.value))
            .transpose()
            .map_err(|error| at_cell(address, error))?;
        if let Some(range) = range {
            self.ranges.push(range);
        }
        match formula
            .attr(attributes, "t")
            .map(|attribute| attribute.value.as_ref())
        {
            Some("shared") => {
                let index_reference = formula.attr(attributes, "si").ok_or_else(|| {
                    unsupported(format!("{address}: shared formula without an index"))
                })?;
                let index = trim_xml_whitespace(&index_reference.value)
                    .parse::<u32>()
                    .map_err(|_| unsupported(format!("{address}: invalid shared formula index")))?;
                let group = self.shared.entry(index).or_insert((address, false));
                group.1 |= range.is_some();
            }
            Some("array" | "dataTable") if range.is_none() => {
                self.incomplete_group.get_or_insert(address);
            }
            _ => {}
        }
        Ok(())
    }

    // The first formula cell of a group without a reference range.
    fn first_incomplete_group(&self) -> Option<CellRef> {
        self.shared
            .values()
            .filter(|&&(_, has_range)| !has_range)
            .map(|&(first, _)| first)
            .chain(self.incomplete_group)
            .min()
    }
}

fn worksheet_namespace(document: &XmlDocument) -> Result<NamespaceId> {
    let root = &document.elements[0];
    if root.namespace.is_unbound() {
        return Err(unsupported("worksheet has no OOXML namespace"));
    }
    if root.local_name() != "worksheet" || !document.is_spreadsheet_namespace(root.namespace) {
        return Err(unsupported("expected an OOXML worksheet"));
    }
    Ok(root.namespace)
}

fn row_number(row: &Element, attributes: &[Attribute], previous: u32) -> Result<u32> {
    let reference = row
        .attr(attributes, "r")
        .ok_or_else(|| unsupported("rows without explicit row numbers"))?;
    let number = trim_xml_whitespace(&reference.value)
        .parse::<u32>()
        .map_err(|_| unsupported("invalid row number"))?;
    if number <= previous || number > CellRef::MAX_ROW {
        return Err(unsupported(format!(
            "row {number}: duplicate, unsorted, or invalid row numbers"
        )));
    }
    Ok(number)
}

fn cell_address(cell: &Element, attributes: &[Attribute], row: u32) -> Result<CellRef> {
    cell.attr(attributes, "r")
        .ok_or_else(|| unsupported(format!("row {row}: cells without explicit references")))?
        .value
        .parse()
        .map_err(|_| unsupported(format!("row {row}: invalid cell reference in worksheet")))
}

type RowIndex = (Vec<RowEntry>, Vec<(CellRef, usize)>);

// Rows of `sheetData` and their cells need explicit, strictly increasing
// addresses, so every lookup and insertion point is a binary search away.
fn index_rows(
    elements: &[Element],
    attributes: &[Attribute],
    namespace: NamespaceId,
    data_index: usize,
    formulas: &mut Formulas,
) -> Result<RowIndex> {
    let mut rows = Vec::new();
    let mut cells = Vec::new();
    let mut previous_row = 0;
    for row_index in elements[data_index].children(elements) {
        let row = &elements[row_index];
        if !row.is(namespace, "row") {
            continue;
        }
        let number = row_number(row, attributes, previous_row)?;
        previous_row = number;
        let cells_start = cells.len();
        let mut previous_column = 0;
        for cell_index in row.children(elements) {
            let cell = &elements[cell_index];
            if !cell.is(namespace, "c") {
                continue;
            }
            let address = cell_address(cell, attributes, number)?;
            if address.row != number || address.column <= previous_column {
                return Err(unsupported(format!(
                    "{address}: duplicate, unsorted, or inconsistent cell references"
                )));
            }
            previous_column = address.column;
            cells.push((address, cell_index));
            for formula in cell.children(elements) {
                let formula = &elements[formula];
                if formula.is(namespace, "f") {
                    formulas.add(address, formula, attributes)?;
                }
            }
        }
        rows.push(RowEntry {
            number,
            element: row_index,
            cells: cells_start..cells.len(),
        });
    }
    Ok((rows, cells))
}

fn merged_ranges(
    elements: &[Element],
    attributes: &[Attribute],
    namespace: NamespaceId,
) -> Result<Vec<Range>> {
    let mut ranges = Vec::new();
    for index in elements[0].children(elements) {
        let merges = &elements[index];
        if !merges.is(namespace, "mergeCells") {
            continue;
        }
        for index in merges.children(elements) {
            let merge = &elements[index];
            if !merge.is(namespace, "mergeCell") {
                continue;
            }
            let reference = merge
                .attr(attributes, "ref")
                .ok_or_else(|| unsupported("merged cells without a reference"))?;
            ranges.push(reference_range(&reference.value)?);
        }
    }
    Ok(ranges)
}

// Main-namespace rows, cells, formulas, values and merged ranges hidden in
// another container (such as AlternateContent) would make the visible cells
// ambiguous. Each rule concerns one local name, so one match per element
// suffices. Returns whether column definitions are hidden the same way, which
// leaves a new cell's default style unknown.
fn hidden_containers(
    elements: &[Element],
    namespace: NamespaceId,
    data_index: usize,
) -> Result<bool> {
    let parent_named = |element: &Element, name| {
        element
            .parent()
            .map(|parent| &elements[parent])
            .filter(|parent| parent.is(namespace, name))
    };
    let mut hidden_columns = false;
    for element in elements {
        if element.namespace != namespace {
            continue;
        }
        match element.local_name() {
            "row" if element.parent() != Some(data_index) => {
                return Err(unsupported("row outside sheetData"));
            }
            "c" if parent_named(element, "row")
                .is_none_or(|row| row.parent() != Some(data_index)) =>
            {
                return Err(unsupported("cell outside a worksheet row"));
            }
            "f" if parent_named(element, "c").is_none() => {
                return Err(unsupported("formula outside a worksheet cell"));
            }
            "v" | "is" if parent_named(element, "c").is_none() => {
                return Err(unsupported("value payload outside a worksheet cell"));
            }
            "mergeCells" if element.parent() != Some(0) => {
                return Err(unsupported("merged ranges outside the worksheet root"));
            }
            "mergeCell"
                if parent_named(element, "mergeCells")
                    .is_none_or(|merges| merges.parent() != Some(0)) =>
            {
                return Err(unsupported("merged range outside mergeCells"));
            }
            "cols" if element.parent() != Some(0) => hidden_columns = true,
            "col"
                if parent_named(element, "cols")
                    .is_none_or(|columns| columns.parent() != Some(0)) =>
            {
                hidden_columns = true;
            }
            _ => {}
        }
    }
    Ok(hidden_columns)
}

// A worksheet's structure is indexed once. Byte spans remain relative to the
// original document, so unrelated whitespace, prefixes and markup never move.
pub(super) struct WorksheetIndex<'a> {
    pub(super) elements: Vec<Element<'a>>,
    pub(super) attributes: Vec<Attribute<'a>>,
    /// Every cell of `sheetData` in document order, with its element index.
    pub(super) cells: Vec<(CellRef, usize)>,
    pub(super) namespace: NamespaceId,
    pub(super) data_index: usize,
    pub(super) rows: Vec<RowEntry>,
    pub(super) formula_cells: HashSet<CellRef>,
    pub(super) formula_ranges: Vec<Range>,
    pub(super) merged_ranges: Vec<Range>,
    /// The first formula cell whose shared, array, or data-table group lacks a
    /// reference range, leaving the protected cells unknown.
    pub(super) incomplete_formula_group: Option<CellRef>,
    column_styles: ColumnStyles,
}

impl<'a> WorksheetIndex<'a> {
    pub(super) fn parse(xml: &'a [u8]) -> Result<Self> {
        let document = parse(xml)?;
        let namespace = worksheet_namespace(&document)?;
        let XmlDocument {
            elements,
            attributes,
            ..
        } = document;
        let data_index = unique(
            elements[0]
                .children(&elements)
                .filter(|&index| elements[index].is(namespace, "sheetData")),
            "worksheet must contain exactly one sheetData element",
        )?
        .ok_or_else(|| unsupported("worksheet must contain exactly one sheetData element"))?;
        let mut formulas = Formulas::default();
        let (rows, cells) =
            index_rows(&elements, &attributes, namespace, data_index, &mut formulas)?;
        let mut column_styles = column_styles(&elements, &attributes, namespace);
        let merged_ranges = merged_ranges(&elements, &attributes, namespace)?;
        if hidden_containers(&elements, namespace, data_index)? {
            column_styles = Err("column definitions inside an unsupported XML container");
        }
        let incomplete_formula_group = formulas.first_incomplete_group();
        Ok(Self {
            elements,
            attributes,
            cells,
            namespace,
            data_index,
            rows,
            formula_cells: formulas.cells,
            formula_ranges: formulas.ranges,
            merged_ranges,
            incomplete_formula_group,
            column_styles,
        })
    }

    // The column's default style for a new cell, or `None` for style zero.
    pub(super) fn column_style(&self, cell: CellRef) -> Result<Option<u32>> {
        let columns = self
            .column_styles
            .as_ref()
            .map_err(|&message| unsupported(format!("{cell}: {message}")))?;
        let column = u32::from(cell.column);
        let position = columns.partition_point(|&(first, _, _)| first <= column);
        Ok(position
            .checked_sub(1)
            .map(|position| columns[position])
            .filter(|&(_, last, style)| column <= last && style != 0)
            .map(|(_, _, style)| style))
    }

    pub(super) fn cell(&self, address: CellRef) -> Option<usize> {
        let row = self
            .rows
            .binary_search_by_key(&address.row, |row| row.number)
            .ok()?;
        let cells = &self.cells[self.rows[row].cells.clone()];
        let cell = cells
            .binary_search_by_key(&address.column, |&(cell, _)| cell.column)
            .ok()?;
        Some(cells[cell].1)
    }

    pub(super) fn validate_edits(&self, cells: &BTreeMap<CellRef, CellValue>) -> Result<()> {
        // Without the group's range, any edited cell could be a hidden member.
        if let Some(cell) = self.incomplete_formula_group {
            return Err(unsupported(format!(
                "{cell} belongs to a formula group without a reference range"
            )));
        }
        for &cell in cells.keys() {
            if self.formula_cells.contains(&cell) {
                return Err(protected(cell, protection::FORMULA));
            }
        }
        validate_ranges(cells, &self.formula_ranges, false)?;
        validate_ranges(cells, &self.merged_ranges, true)
    }
}
