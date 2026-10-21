use super::parse::{Attributes, Element, NamespaceId, XmlDocument, parse};
use super::validation::validate_ranges;
use super::{invalid, protected, protection, unique, unsupported};
use crate::xml::trim_whitespace as trim_xml_whitespace;
use crate::{CellRange, CellRef, CellValue, Result};
use std::collections::{BTreeMap, HashMap};

// A worksheet reference such as a merge, formula group or dimension range.
// Unlike `CellRange`'s parser, this accepts `$` markers anywhere and refuses
// reversed corners instead of normalizing them. A third part is refused
// because a corner can never contain `:`. A malformed reference makes the
// worksheet invalid.
pub(super) fn reference_range(reference: &str) -> Result<CellRange> {
    let corner = |text: &str| -> Result<CellRef> {
        let cell = if text.contains('$') {
            text.replace('$', "").parse()
        } else {
            text.parse()
        };
        cell.map_err(|_| invalid(format!("invalid cell range {reference:?}")))
    };
    let (first, last) = match reference.split_once(':') {
        Some((first, last)) => (corner(first)?, corner(last)?),
        None => {
            let cell = corner(reference)?;
            (cell, cell)
        }
    };
    if first.row > last.row || first.column > last.column {
        return Err(invalid(format!("invalid cell range {reference:?}")));
    }
    Ok(CellRange::new(first, last))
}

// Column default styles as sorted, nonoverlapping `(min, max, style)` intervals.
// Malformed or ambiguous definitions are reported only when a new cell needs
// its column's style, so reads and edits of existing cells are unaffected.
type ColumnStyles = std::result::Result<Vec<(u32, u32, u32)>, &'static str>;

fn column_styles(
    source: &str,
    elements: &[Element],
    attributes: &Attributes,
    namespace: NamespaceId,
) -> ColumnStyles {
    let number = |column: &Element, name, default| {
        column
            .attr(attributes, name)
            .map_or(Ok(default), |attribute| {
                trim_xml_whitespace(attribute.value)
                    .parse::<u32>()
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

// Formula groups: the ranges they protect, and any group missing its range.
#[derive(Default)]
struct Formulas {
    ranges: Vec<CellRange>,
    // Each shared-formula index maps to its first cell and whether any
    // member of the group carries the group's reference range.
    shared: HashMap<u32, (CellRef, bool)>,
    incomplete_group: Option<CellRef>,
}

impl Formulas {
    fn add(&mut self, address: CellRef, formula: &Element, attributes: &Attributes) -> Result<()> {
        let range = formula
            .attr(attributes, "ref")
            .map(|attribute| reference_range(attribute.value))
            .transpose()
            .map_err(|error| error.context(address))?;
        if let Some(range) = range {
            self.ranges.push(range);
        }
        match formula
            .attr(attributes, "t")
            .map(|attribute| attribute.value)
        {
            Some("shared") => {
                let index_reference = formula.attr(attributes, "si").ok_or_else(|| {
                    invalid(format!("{address}: shared formula without an index"))
                })?;
                let index = trim_xml_whitespace(index_reference.value)
                    .parse::<u32>()
                    .map_err(|_| invalid(format!("{address}: invalid shared formula index")))?;
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
        return Err(invalid("worksheet has no OOXML namespace"));
    }
    if root.local_name(document.source) != "worksheet"
        || !document.is_spreadsheet_namespace(root.namespace)
    {
        return Err(invalid("expected an OOXML worksheet"));
    }
    Ok(root.namespace)
}

// Row numbers and cell references are optional in the schema, but this index
// needs them, so an absent one is unsupported. One that is malformed, out of
// order, or repeated leaves its cells' positions ambiguous: the workbook is
// invalid.
fn row_number(row: &Element, attributes: &Attributes, previous: u32) -> Result<u32> {
    let reference = row
        .attr(attributes, "r")
        .ok_or_else(|| unsupported("rows without explicit row numbers"))?;
    let number = trim_xml_whitespace(reference.value)
        .parse::<u32>()
        .map_err(|_| invalid("invalid row number"))?;
    if number <= previous || number > CellRef::MAX_ROW {
        return Err(invalid(format!(
            "row {number}: duplicate, unsorted, or invalid row numbers"
        )));
    }
    Ok(number)
}

fn cell_address(cell: &Element, attributes: &Attributes, row: u32) -> Result<CellRef> {
    cell.attr(attributes, "r")
        .ok_or_else(|| unsupported(format!("row {row}: cells without explicit references")))?
        .value
        .parse()
        .map_err(|_| invalid(format!("row {row}: invalid cell reference in worksheet")))
}

type RowIndex = (Vec<RowEntry>, Vec<(CellRef, usize)>);

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
                return Err(invalid(format!(
                    "{address}: duplicate, unsorted, or inconsistent cell references"
                )));
            }
            previous_column = address.column;
            cells.push((address, cell_index));
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
    Ok((rows, cells))
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
                .attr(attributes, "ref")
                .ok_or_else(|| invalid("merged cells without a reference"))?;
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
            return Err(unsupported(
                "worksheet mixes strict and transitional SpreadsheetML namespaces",
            ));
        }
        if element.namespace != namespace {
            continue;
        }
        match element.local_name(source) {
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
    /// The worksheet XML, which holds every element name and raw value.
    pub(super) source: &'a str,
    pub(super) elements: Vec<Element>,
    pub(super) attributes: Attributes<'a>,
    /// Every cell of `sheetData` in document order, with its element index.
    pub(super) cells: Vec<(CellRef, usize)>,
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

impl<'a> WorksheetIndex<'a> {
    pub(super) fn parse(xml: &'a [u8]) -> Result<Self> {
        let document = parse(xml)?;
        let namespace = worksheet_namespace(&document)?;
        let other_namespace = document.other_spreadsheet_namespace(namespace);
        let XmlDocument {
            source,
            elements,
            attributes,
            ..
        } = document;
        let one_sheet_data = || invalid("worksheet must contain exactly one sheetData element");
        let data_index = unique(
            elements[0]
                .children(&elements)
                .filter(|&index| elements[index].is(source, namespace, "sheetData")),
            one_sheet_data,
        )?
        .ok_or_else(one_sheet_data)?;
        let mut formulas = Formulas::default();
        let (rows, cells) = index_rows(
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
            return Err(unsupported(format!(
                "{cell} belongs to a formula group without a reference range"
            )));
        }
        for &cell in cells.keys() {
            if self.has_formula(cell) {
                return Err(protected(cell, protection::FORMULA));
            }
        }
        validate_ranges(cells, &self.formula_ranges, false)?;
        validate_ranges(cells, &self.merged_ranges, true)
    }
}
