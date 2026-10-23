use super::parse::{self, Element, XmlDocument};
use super::validation::{self, RangeKind};
use super::{errors, protection};
use crate::xml::{self, Attributes, NamespaceId};
use crate::{CellRange, CellRef, CellValue, Result};
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::result::Result as StdResult;

// A worksheet reference such as a merge, formula group or dimension range.
// Unlike `CellRange`'s parser, this accepts `$` markers anywhere and refuses
// reversed corners instead of normalizing them. A third part is refused
// because a corner can never contain `:`. A malformed reference makes the
// worksheet invalid.
pub(super) fn reference_range(reference: &str) -> Result<CellRange> {
    let corner = |text: &str| -> Result<CellRef> {
        let cell: Result<CellRef> = if text.contains('$') {
            text.replace('$', "").parse()
        } else {
            text.parse()
        };
        cell.map_err(|_| errors::invalid(format!("invalid cell range {reference:?}")))
    };
    let (first, last) = match reference.split_once(':') {
        Some((first, last)) => (corner(first)?, corner(last)?),
        None => {
            let cell = corner(reference)?;
            (cell, cell)
        }
    };
    if first.row > last.row || first.column > last.column {
        return Err(errors::invalid(format!("invalid cell range {reference:?}")));
    }
    Ok(CellRange::new(first, last))
}

// A column definition: the default style of the columns `first..=last`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ColumnStyle {
    first: u32,
    last: u32,
    style: u32,
}

// Column default styles as sorted, nonoverlapping intervals. Malformed or
// ambiguous definitions are reported only when a new cell needs its column's
// style, so reads and edits of existing cells are unaffected.
type ColumnStyles = StdResult<Vec<ColumnStyle>, &'static str>;

fn column_styles(
    source: &str,
    elements: &[Element],
    attributes: &Attributes,
    namespace: NamespaceId,
) -> ColumnStyles {
    let number = |column: &Element, name: &str, default: u32| {
        column
            .attribute(attributes, name)
            .map_or(Ok(default), |attribute| {
                xml::trim_whitespace(attribute.value)
                    .parse()
                    .map_err(|_| "invalid column definition")
            })
    };
    let mut columns = Vec::new();
    for index in elements[0].children(elements) {
        let definitions = &elements[index];
        if !definitions.is(source, namespace, "cols") {
            continue;
        }
        for index in definitions.children(elements) {
            let column = &elements[index];
            if !column.is(source, namespace, "col") {
                continue;
            }
            let first = number(column, "min", 0)?;
            let last = number(column, "max", 0)?;
            if first == 0 || first > last {
                return Err("invalid column definition");
            }
            columns.push(ColumnStyle {
                first,
                last,
                style: number(column, "style", 0)?,
            });
        }
    }
    columns.sort_unstable();
    if columns.windows(2).any(|pair| pair[0].last >= pair[1].first) {
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
    pub(super) cells: Range<usize>,
}

/// One `c` of a `sheetData` row, located in the index arenas.
#[derive(Clone, Copy)]
pub(super) struct CellEntry {
    pub(super) address: CellRef,
    /// The cell's index in `WorksheetIndex::elements`.
    pub(super) element: usize,
}

// A shared-formula group: its first cell, and whether any member of the
// group carries the group's reference range.
struct SharedFormula {
    first: CellRef,
    has_range: bool,
}

// Formula groups: the ranges they protect, and any group missing its range.
#[derive(Default)]
struct Formulas {
    ranges: Vec<CellRange>,
    // Each shared-formula index maps to its group.
    shared: HashMap<u32, SharedFormula>,
    incomplete_group: Option<CellRef>,
}

impl Formulas {
    fn add(&mut self, address: CellRef, formula: &Element, attributes: &Attributes) -> Result<()> {
        let range = formula
            .attribute(attributes, "ref")
            .map(|attribute| reference_range(attribute.value))
            .transpose()
            .map_err(|error| error.context(address))?;
        if let Some(range) = range {
            self.ranges.push(range);
        }
        match formula
            .attribute(attributes, "t")
            .map(|attribute| attribute.value)
        {
            Some("shared") => {
                let index_reference = formula.attribute(attributes, "si").ok_or_else(|| {
                    errors::invalid(format!("{address}: shared formula without an index"))
                })?;
                let index: u32 = xml::trim_whitespace(index_reference.value)
                    .parse()
                    .map_err(|_| {
                        errors::invalid(format!("{address}: invalid shared formula index"))
                    })?;
                let group = self.shared.entry(index).or_insert(SharedFormula {
                    first: address,
                    has_range: false,
                });
                group.has_range |= range.is_some();
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
            .filter(|group| !group.has_range)
            .map(|group| group.first)
            .chain(self.incomplete_group)
            .min()
    }
}

fn worksheet_namespace(document: &XmlDocument) -> Result<NamespaceId> {
    let root = &document.elements[0];
    if root.namespace.is_unbound() {
        return Err(errors::invalid("worksheet has no OOXML namespace"));
    }
    if root.local_name(document.source) != "worksheet"
        || !document.is_spreadsheet_namespace(root.namespace)
    {
        return Err(errors::invalid("expected an OOXML worksheet"));
    }
    Ok(root.namespace)
}

// Row numbers and cell references are optional in the schema, but this index
// needs them, so an absent one is unsupported. One that is malformed, out of
// order, or repeated leaves its cells' positions ambiguous: the workbook is
// invalid.
fn row_number(row: &Element, attributes: &Attributes, previous: u32) -> Result<u32> {
    let reference = row
        .attribute(attributes, "r")
        .ok_or_else(|| errors::unsupported("rows without explicit row numbers"))?;
    let number: u32 = xml::trim_whitespace(reference.value)
        .parse()
        .map_err(|_| errors::invalid("invalid row number"))?;
    if number <= previous || number > CellRef::MAX_ROW {
        return Err(errors::invalid(format!(
            "row {number}: duplicate, unsorted, or invalid row numbers"
        )));
    }
    Ok(number)
}

fn cell_address(cell: &Element, attributes: &Attributes, row: u32) -> Result<CellRef> {
    cell.attribute(attributes, "r")
        .ok_or_else(|| {
            errors::unsupported(format!("row {row}: cells without explicit references"))
        })?
        .value
        .parse()
        .map_err(|_| errors::invalid(format!("row {row}: invalid cell reference in worksheet")))
}

// The rows of `sheetData`, and the cells of every row in document order.
struct RowIndex {
    rows: Vec<RowEntry>,
    cells: Vec<CellEntry>,
}

// Rows of `sheetData` and their cells need explicit, strictly increasing
// addresses, so every lookup and insertion point is a binary search away.
fn index_rows(
    source: &str,
    elements: &[Element],
    attributes: &Attributes,
    namespace: NamespaceId,
    data_index: usize,
    formulas: &mut Formulas,
) -> Result<RowIndex> {
    let mut rows = Vec::new();
    let mut cells = Vec::new();
    let mut previous_row = 0;
    for row_index in elements[data_index].children(elements) {
        let row = &elements[row_index];
        if !row.is(source, namespace, "row") {
            continue;
        }
        let number = row_number(row, attributes, previous_row)?;
        previous_row = number;
        let cells_start = cells.len();
        let mut previous_column = 0;
        for cell_index in row.children(elements) {
            let cell = &elements[cell_index];
            if !cell.is(source, namespace, "c") {
                continue;
            }
            let address = cell_address(cell, attributes, number)?;
            if address.row != number || address.column <= previous_column {
                return Err(errors::invalid(format!(
                    "{address}: duplicate, unsorted, or inconsistent cell references"
                )));
            }
            previous_column = address.column;
            cells.push(CellEntry {
                address,
                element: cell_index,
            });
            for formula in cell.children(elements) {
                let formula = &elements[formula];
                if formula.is(source, namespace, "f") {
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
    Ok(RowIndex { rows, cells })
}

fn merged_ranges(
    source: &str,
    elements: &[Element],
    attributes: &Attributes,
    namespace: NamespaceId,
) -> Result<Vec<CellRange>> {
    let mut ranges = Vec::new();
    for index in elements[0].children(elements) {
        let merges = &elements[index];
        if !merges.is(source, namespace, "mergeCells") {
            continue;
        }
        for index in merges.children(elements) {
            let merge = &elements[index];
            if !merge.is(source, namespace, "mergeCell") {
                continue;
            }
            let reference = merge
                .attribute(attributes, "ref")
                .ok_or_else(|| errors::invalid("merged cells without a reference"))?;
            ranges.push(reference_range(reference.value)?);
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
    source: &str,
    elements: &[Element],
    namespace: NamespaceId,
    other_namespace: Option<NamespaceId>,
    data_index: usize,
) -> Result<bool> {
    let parent_named = |element: &Element, name| {
        element
            .parent()
            .map(|parent| &elements[parent])
            .filter(|parent| parent.is(source, namespace, name))
    };
    let mut hidden_columns = false;
    for element in elements {
        // Spreadsheet consumers can recognize both dialects within one part.
        // Treating the other dialect as a vendor extension would hide values,
        // formulas, merged ranges, or column defaults from this index.
        if Some(element.namespace) == other_namespace {
            return Err(errors::unsupported(
                "worksheet mixes strict and transitional SpreadsheetML namespaces",
            ));
        }
        if element.namespace != namespace {
            continue;
        }
        match element.local_name(source) {
            "row" if element.parent() != Some(data_index) => {
                return Err(errors::unsupported("row outside sheetData"));
            }
            "c" if parent_named(element, "row")
                .is_none_or(|row| row.parent() != Some(data_index)) =>
            {
                return Err(errors::unsupported("cell outside a worksheet row"));
            }
            "f" if parent_named(element, "c").is_none() => {
                return Err(errors::unsupported("formula outside a worksheet cell"));
            }
            "v" | "is" if parent_named(element, "c").is_none() => {
                return Err(errors::unsupported(
                    "value payload outside a worksheet cell",
                ));
            }
            "mergeCells" if element.parent() != Some(0) => {
                return Err(errors::unsupported(
                    "merged ranges outside the worksheet root",
                ));
            }
            "mergeCell"
                if parent_named(element, "mergeCells")
                    .is_none_or(|merges| merges.parent() != Some(0)) =>
            {
                return Err(errors::unsupported("merged range outside mergeCells"));
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
pub(super) struct WorksheetIndex<'xml> {
    /// The worksheet XML, which holds every element name and raw value.
    pub(super) source: &'xml str,
    pub(super) elements: Vec<Element>,
    pub(super) attributes: Attributes<'xml>,
    /// Every cell of `sheetData` in document order.
    pub(super) cells: Vec<CellEntry>,
    pub(super) namespace: NamespaceId,
    pub(super) data_index: usize,
    pub(super) rows: Vec<RowEntry>,
    pub(super) formula_ranges: Vec<CellRange>,
    pub(super) merged_ranges: Vec<CellRange>,
    /// The first formula cell whose shared, array, or data-table group lacks a
    /// reference range, leaving the protected cells unknown.
    pub(super) incomplete_formula_group: Option<CellRef>,
    column_styles: ColumnStyles,
}

impl<'xml> WorksheetIndex<'xml> {
    pub(super) fn parse(xml: &'xml [u8]) -> Result<Self> {
        let document = parse::parse(xml)?;
        let namespace = worksheet_namespace(&document)?;
        let other_namespace = document.other_spreadsheet_namespace(namespace);
        let XmlDocument {
            source,
            elements,
            attributes,
            ..
        } = document;
        let one_sheet_data =
            || errors::invalid("worksheet must contain exactly one sheetData element");
        let data_index = match errors::unique(
            elements[0]
                .children(&elements)
                .filter(|&index| elements[index].is(source, namespace, "sheetData")),
            one_sheet_data,
        )? {
            Some(index) => index,
            // Like a row, a sheetData in a markup-compatibility branch can
            // hold the cells of whichever branch a reader selects.
            None if elements
                .iter()
                .any(|element| element.is(source, namespace, "sheetData")) =>
            {
                return Err(errors::unsupported(
                    "sheetData inside an unsupported XML container",
                ));
            }
            None => return Err(one_sheet_data()),
        };
        let mut formulas = Formulas::default();
        let RowIndex { rows, cells } = index_rows(
            source,
            &elements,
            &attributes,
            namespace,
            data_index,
            &mut formulas,
        )?;
        let mut column_styles = column_styles(source, &elements, &attributes, namespace);
        let merged_ranges = merged_ranges(source, &elements, &attributes, namespace)?;
        if hidden_containers(source, &elements, namespace, other_namespace, data_index)? {
            column_styles = Err("column definitions inside an unsupported XML container");
        }
        let incomplete_formula_group = formulas.first_incomplete_group();
        Ok(Self {
            source,
            elements,
            attributes,
            cells,
            namespace,
            data_index,
            rows,
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
            .map_err(|&message| errors::unsupported(format!("{cell}: {message}")))?;
        let column = u32::from(cell.column);
        let position = columns.partition_point(|definition| definition.first <= column);
        Ok(position
            .checked_sub(1)
            .map(|position| columns[position])
            .filter(|definition| column <= definition.last && definition.style != 0)
            .map(|definition| definition.style))
    }

    pub(super) fn cell(&self, address: CellRef) -> Option<usize> {
        let row = self
            .rows
            .binary_search_by_key(&address.row, |row| row.number)
            .ok()?;
        let cells = &self.cells[self.rows[row].cells.clone()];
        let cell = cells
            .binary_search_by_key(&address.column, |cell| cell.address.column)
            .ok()?;
        Some(cells[cell].element)
    }

    // Whether an existing cell has a main-namespace formula child, the same
    // test that reads use. A new cell has none.
    fn has_formula(&self, address: CellRef) -> bool {
        self.cell(address).is_some_and(|cell| {
            self.elements[cell]
                .children(&self.elements)
                .any(|child| self.elements[child].is(self.source, self.namespace, "f"))
        })
    }

    pub(super) fn validate_edits(&self, cells: &BTreeMap<CellRef, CellValue>) -> Result<()> {
        // Without the group's range, any edited cell could be a hidden member.
        if let Some(cell) = self.incomplete_formula_group {
            return Err(errors::unsupported(format!(
                "{cell} belongs to a formula group without a reference range"
            )));
        }
        for &cell in cells.keys() {
            if self.has_formula(cell) {
                return Err(errors::protected(cell, protection::FORMULA));
            }
        }
        validation::validate_ranges(cells, &self.formula_ranges, RangeKind::Formula)?;
        validation::validate_ranges(cells, &self.merged_ranges, RangeKind::Merged)
    }
}
