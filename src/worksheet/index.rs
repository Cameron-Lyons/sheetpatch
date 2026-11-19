use super::parse::{self, Element, XmlDocument};
use super::validation::{self, RangeKind};
use super::{errors, protection};
use crate::xml::{self, Attributes, NamespaceId};
use crate::{CellRange, CellRef, CellValue, Result};
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::result::Result as StdResult;
use std::slice;

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
    element: u32,
    /// Cell and element positions fit in the parsed XML's 32-bit offset space.
    cells: Range<u32>,
}

impl RowEntry {
    pub(super) fn element(&self) -> usize {
        self.element as usize
    }

    pub(super) fn cells(&self) -> Range<usize> {
        self.cells.start as usize..self.cells.end as usize
    }
}

/// One `c` of a `sheetData` row, located in the index arenas.
#[derive(Clone, Copy)]
pub(super) struct CellEntry {
    pub(super) address: CellRef,
    /// The cell's index in `WorksheetIndex::elements`.
    element: u32,
}

impl CellEntry {
    pub(super) fn element(&self) -> usize {
        self.element as usize
    }
}

/// The most cells per requested row number that a range read filters by
/// column, rather than seeking the columns in each row, on a worksheet with no
/// more cells than rows. Seeking there also visits the empty rows, so it pays
/// only for rows so wide that searching them beats scanning them, whether or
/// not the requested columns hold cells. Other worksheets seek rows averaging
/// more than one cell: scanning costs less only when the requested columns
/// are full, which the index cannot tell.
const FILTERED_CELLS_PER_ROW: usize = 128;

/// The most row numbers missing between a worksheet's first and last rows for
/// which row searches confine themselves to a number's window, which holds no
/// more records than there are missing numbers. Each window starts at its own
/// offset, so searching wide ones would not share the first probes, and their
/// cache lines, with other searches of all records.
const NARROW_ROW_WINDOW: usize = 64;

/// The fewest row records per missing row number for which row searches use
/// windows. A window holds about as many records as there are missing
/// numbers, and finding it costs about as much as four steps of a search of
/// all records, so it pays only among many more records than that.
const RECORDS_PER_MISSING_ROW: usize = 16;

/// How row records are found by number, chosen from the row numbers missing
/// between the first row and the last.
// `Full` comes first, as the variant that costs least to test for, since its
// searches gain the least from the others' shortcuts.
#[derive(Clone, Copy)]
enum RowSearch {
    /// Every record is searched, among rows missing many numbers or too few
    /// records for a window to pay, or at most one row.
    Full,
    /// At most `NARROW_ROW_WINDOW` are missing, and at most one per
    /// `RECORDS_PER_MISSING_ROW` records: a number's window is searched.
    Windowed,
    /// None are missing, so a number's record, if any, is at its offset from
    /// `first`, the first row's number.
    Gapless { first: u32 },
}

/// Existing cells in a rectangle. On a worksheet with no more cells than rows,
/// the cells between the rectangle's corners form one slice, filtered by
/// column unless it averages more than `FILTERED_CELLS_PER_ROW` cells per
/// requested row number. Otherwise the requested rows' cells form one slice,
/// walked directly for whole rows and filtered by column when the rows average
/// at most one cell. In the remaining cases each row's columns are sought
/// separately.
#[derive(Clone)]
pub(super) struct CellEntries<'index> {
    cells: &'index [CellEntry],
    remaining: slice::Iter<'index, CellEntry>,
    rows: slice::Iter<'index, RowEntry>,
    first_column: u16,
    last_column: u16,
    filter_remaining: bool,
}

fn cell_columns(cells: &[CellEntry], first: u16, last: u16) -> &[CellEntry] {
    let start = cells.partition_point(|cell| cell.address.column < first);
    let cells = &cells[start..];
    let end = cells.partition_point(|cell| cell.address.column <= last);
    &cells[..end]
}

impl<'index> Iterator for CellEntries<'index> {
    type Item = &'index CellEntry;

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining.len();
        if self.filter_remaining {
            return (0, Some(remaining));
        }
        // The current slice already matches the columns. Future rows may
        // contain unrelated columns, but their cell span bounds the rest.
        // With no future rows, including whole-sheet and full-width reads,
        // the length is exact and collectors can reserve once.
        let rows = self.rows.as_slice();
        let future = rows
            .first()
            .zip(rows.last())
            .map_or(0, |(first, last)| last.cells().end - first.cells().start);
        (remaining, Some(remaining + future))
    }

    fn next(&mut self) -> Option<Self::Item> {
        // Filtering a span of narrow rows avoids seeking in each row, and
        // never visits the empty or styled rows between its cells.
        if self.filter_remaining {
            let (first, last) = (self.first_column, self.last_column);
            return self
                .remaining
                .find(|cell| first <= cell.address.column && cell.address.column <= last);
        }
        loop {
            if let Some(cell) = self.remaining.next() {
                return Some(cell);
            }
            let row = self.rows.next()?;
            let cells = &self.cells[row.cells()];
            self.remaining = cell_columns(cells, self.first_column, self.last_column).iter();
        }
    }

    fn fold<B, F>(self, initial: B, mut fold: F) -> B
    where
        F: FnMut(B, Self::Item) -> B,
    {
        // Choose the traversal once for reductions, rather than testing the
        // sparse/dense mode again for every cell in a whole-sheet sum or fold.
        if self.filter_remaining {
            // Moved into the closure, the bounds stay in registers rather than
            // being reloaded for every cell the fold's call might change.
            let (first, last) = (self.first_column, self.last_column);
            return self
                .remaining
                .filter(move |cell| first <= cell.address.column && cell.address.column <= last)
                .fold(initial, fold);
        }
        let mut result = self.remaining.fold(initial, &mut fold);
        for row in self.rows {
            let cells = &self.cells[row.cells()];
            result = cell_columns(cells, self.first_column, self.last_column)
                .iter()
                .fold(result, &mut fold);
        }
        result
    }
}

/// Lookup state for nondecreasing addresses. Adjacent cells and duplicates
/// reuse the current position; larger gaps seek within the current row.
#[derive(Clone)]
pub(super) struct CellCursor<'index> {
    cells: &'index [CellEntry],
    rows: &'index [RowEntry],
    row: Option<u32>,
    remaining: &'index [CellEntry],
}

// Move a sorted lookup's lower bound. Adjacent entries need one comparison;
// a gap uses binary search rather than walking every skipped entry.
fn seek<'index, T>(remaining: &mut &'index [T], before: impl Fn(&T) -> bool) -> Option<&'index T> {
    let (entry, rest) = remaining.split_first()?;
    if before(entry) {
        *remaining = rest;
        if rest.first().is_some_and(&before) {
            *remaining = &rest[rest.partition_point(before)..];
        }
    }
    remaining.first()
}

impl CellCursor<'_> {
    pub(super) fn cell(&mut self, address: CellRef) -> Option<usize> {
        if self.row != Some(address.row) {
            self.remaining = seek(&mut self.rows, |row| row.number < address.row)
                .filter(|row| row.number == address.row)
                .map_or(&[], |row| &self.cells[row.cells()]);
            self.row = Some(address.row);
        }
        seek(&mut self.remaining, |cell| {
            cell.address.column < address.column
        })
        .filter(|cell| cell.address.column == address.column)
        .map(CellEntry::element)
    }
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
                element: xml::compact(cell_index),
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
            element: xml::compact(row_index),
            cells: xml::compact(cells_start)..xml::compact(cells.len()),
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
    pub(super) elements: Vec<Element>,
    /// Attribute spans, with the worksheet XML that holds every element name
    /// and raw value, which `source` returns.
    pub(super) attributes: Attributes<'xml>,
    /// Every cell of `sheetData` in document order.
    pub(super) cells: Vec<CellEntry>,
    pub(super) namespace: NamespaceId,
    pub(super) data_index: usize,
    pub(super) rows: Vec<RowEntry>,
    row_search: RowSearch,
    pub(super) formula_ranges: Vec<CellRange>,
    pub(super) merged_ranges: Vec<CellRange>,
    /// The first formula cell whose shared, array, or data-table group lacks a
    /// reference range, leaving the protected cells unknown.
    pub(super) incomplete_formula_group: Option<CellRef>,
    column_styles: ColumnStyles,
}

impl<'xml> WorksheetIndex<'xml> {
    /// The worksheet XML, which holds every element name and raw value.
    #[inline]
    pub(super) fn source(&self) -> &'xml str {
        self.attributes.source()
    }

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
        // Row numbers strictly increase, so this never underflows.
        let missing_rows = rows.first().zip(rows.last()).map_or(0, |(first, last)| {
            (last.number - first.number) as usize + 1 - rows.len()
        });
        // A worksheet of at most one row has nothing to narrow, and a search
        // of all its records costs least.
        let row_search = match (missing_rows, rows.as_slice()) {
            (_, [] | [_]) => RowSearch::Full,
            (0, [first, ..]) => RowSearch::Gapless {
                first: first.number,
            },
            (..=NARROW_ROW_WINDOW, rows)
                if missing_rows * RECORDS_PER_MISSING_ROW <= rows.len() =>
            {
                RowSearch::Windowed
            }
            _ => RowSearch::Full,
        };
        Ok(Self {
            elements,
            attributes,
            cells,
            namespace,
            data_index,
            rows,
            row_search,
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

    /// The row records that can hold the position of `number`, the count of
    /// records numbered below it. Row numbers strictly increase, so the first
    /// and last numbers confine it to as many records as there are numbers
    /// missing.
    fn row_window(&self, number: u32) -> Range<usize> {
        let (Some(first), Some(last)) = (self.rows.first(), self.rows.last()) else {
            return 0..0;
        };
        // Record `i` is numbered from `first + i` to `last - (len - 1 - i)`.
        let high = (number.saturating_sub(first.number) as usize).min(self.rows.len());
        let low = (number as usize + self.rows.len() - 1)
            .saturating_sub(last.number as usize)
            .min(high);
        low..high
    }

    /// The positions of the row records numbered from `first` through `last`.
    #[inline]
    fn rows_numbered(&self, first: u32, last: u32) -> Range<usize> {
        match self.row_search {
            RowSearch::Full => {
                self.rows.partition_point(|row| row.number < first)
                    ..self.rows.partition_point(|row| row.number <= last)
            }
            RowSearch::Windowed => {
                self.windowed_rows_before(first)..self.windowed_rows_before(last + 1)
            }
            RowSearch::Gapless { first: start } => {
                let before =
                    |number: u32| (number.saturating_sub(start) as usize).min(self.rows.len());
                before(first)..before(last + 1)
            }
        }
    }

    /// The number of row records numbered below `number`, which is where that
    /// row's record is or would be, among rows missing few numbers.
    fn windowed_rows_before(&self, number: u32) -> usize {
        let window = self.row_window(number);
        window.start + self.rows[window].partition_point(|row| row.number < number)
    }

    // Inlined, as HEAD's search of all records alone was: as a call, the
    // choice of search measurably slowed reads of cells that are not stored.
    #[inline]
    pub(super) fn cell(&self, address: CellRef) -> Option<usize> {
        // Among rows numbered with many gaps, an exact search of all records
        // measured about 12% faster than a search for the row's position for
        // single-cell reads.
        let row = match self.row_search {
            RowSearch::Full => {
                let row = self
                    .rows
                    .binary_search_by_key(&address.row, |row| row.number)
                    .ok()?;
                &self.rows[row]
            }
            RowSearch::Windowed => {
                let row = self.windowed_rows_before(address.row);
                self.rows.get(row).filter(|row| row.number == address.row)?
            }
            RowSearch::Gapless { first } => {
                self.rows.get(address.row.checked_sub(first)? as usize)?
            }
        };
        let cells = &self.cells[row.cells()];
        let cell = cells
            .binary_search_by_key(&address.column, |cell| cell.address.column)
            .ok()?;
        Some(cells[cell].element())
    }

    pub(super) fn cell_cursor(&self) -> CellCursor<'_> {
        CellCursor {
            cells: &self.cells,
            rows: &self.rows,
            row: None,
            remaining: &[],
        }
    }

    #[inline]
    pub(super) fn cells_in(&self, range: Option<CellRange>) -> CellEntries<'_> {
        let mut entries = CellEntries {
            cells: &self.cells,
            remaining: self.cells.iter(),
            rows: [].iter(),
            first_column: 1,
            last_column: CellRef::MAX_COLUMN,
            filter_remaining: false,
        };
        if self.cells.is_empty() {
            return entries;
        }
        if let Some(range) = range {
            entries.first_column = range.first().column;
            entries.last_column = range.last().column;
            let all_columns =
                range.first().column == 1 && range.last().column == CellRef::MAX_COLUMN;
            let (first_row, last_row) = (range.first().row, range.last().row);
            // A worksheet with no more cells than rows, such as a table
            // followed by many formatted empty rows, or a form holding a few
            // values among them. The cell index is the shorter one to search,
            // and the cells between the range's corners form one span of it,
            // which is filtered by column, never visiting the empty rows
            // between them, unless it averages more than
            // `FILTERED_CELLS_PER_ROW` cells per requested row number: then
            // the requested rows are wide, and their columns are sought in
            // each one. Searching all of the index shares the first probes
            // with other searches.
            let row_heavy = self.cells.len() <= self.rows.len();
            if row_heavy {
                let start = self
                    .cells
                    .partition_point(|cell| cell.address < range.first());
                let end = self
                    .cells
                    .partition_point(|cell| cell.address <= range.last());
                let requested = (last_row - first_row) as usize + 1;
                if all_columns || end - start <= requested * FILTERED_CELLS_PER_ROW {
                    entries.remaining = self.cells[start..end].iter();
                    entries.filter_remaining = !all_columns;
                    return entries;
                }
            }
            // Otherwise the requested rows' cells form one span, walked
            // directly for whole rows, filtered when the rows average at most
            // one cell, and otherwise sought in each row, as are the wide rows
            // of a worksheet with no more cells than rows.
            let rows = &self.rows[self.rows_numbered(first_row, last_row)];
            let start = rows.first().map_or(0, |row| row.cells().start);
            let end = rows.last().map_or(start, |row| row.cells().end);
            if all_columns {
                entries.remaining = self.cells[start..end].iter();
            } else if !row_heavy && end - start <= rows.len() {
                // The first row's cells before the requested columns, and the
                // last row's after them, need not be filtered.
                let (start, end) = match (rows.first(), rows.last()) {
                    (Some(top), Some(bottom)) => (
                        top.cells().start
                            + self.cells[top.cells()]
                                .partition_point(|cell| cell.address.column < range.first().column),
                        bottom.cells().start
                            + self.cells[bottom.cells()]
                                .partition_point(|cell| cell.address.column <= range.last().column),
                    ),
                    _ => (start, end),
                };
                entries.remaining = self.cells[start..end].iter();
                entries.filter_remaining = true;
            } else {
                entries.remaining = [].iter();
                entries.rows = rows.iter();
            }
        }
        entries
    }

    // Whether an existing cell has a main-namespace formula child, the same
    // test that reads use. A new cell has none.
    fn has_formula(&self, address: CellRef) -> bool {
        self.cell(address).is_some_and(|cell| {
            self.elements[cell]
                .children(&self.elements)
                .any(|child| self.elements[child].is(self.source(), self.namespace, "f"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    // Random worksheets, under every kind of row search and with rows wider
    // than `FILTERED_CELLS_PER_ROW` among empty ones, and random ranges and
    // lookups in them: every way of reading a range, and every lookup, must
    // find what a search of every cell finds.
    #[test]
    fn random_ranges_and_lookups_match_full_searches() {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let mut seen = [0usize; 3];
        let mut row_heavy_wide = 0;
        for case in 0..400 {
            // Row numbers: gapless, a few missing, or many missing.
            let size = match next(4) {
                0 => 3,
                1 => 40,
                2 => 400,
                _ => 2000,
            };
            let rows = 1 + next(size) as u32;
            let start = 1 + next(50) as u32;
            let mut numbers: Vec<u32> = Vec::new();
            let mode = next(3);
            let mut number = start;
            for _ in 0..rows {
                numbers.push(number);
                number += match mode {
                    0 => 1,
                    1 => {
                        if next(60) == 0 {
                            2
                        } else {
                            1
                        }
                    }
                    _ => 1 + next(4) as u32,
                };
                if number > CellRef::MAX_ROW {
                    break;
                }
            }
            // Cells: most rows empty, some narrow, a few wider than 128 cells.
            let mut xml = String::from(
                "<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><sheetData>",
            );
            let empty_share = next(4);
            // Mostly empty rows with a few wide ones: no more cells than rows.
            let sparse_wide = next(3) == 0 && numbers.len() >= 40;
            let mut wide_rows = Vec::new();
            for (position, &row) in numbers.iter().enumerate() {
                let kind = next(20);
                let width: u16 = if sparse_wide {
                    if position == numbers.len() / 2 || next(numbers.len() as u64 / 4 + 1) == 0 {
                        129 + next(200) as u16
                    } else {
                        0
                    }
                } else if kind < empty_share * 5 {
                    0
                } else if kind == 19 {
                    129 + next(200) as u16
                } else {
                    1 + next(6) as u16
                };
                if width > 128 {
                    wide_rows.push(row);
                }
                if width == 0 {
                    write!(xml, "<row r='{row}'/>").unwrap();
                    continue;
                }
                write!(xml, "<row r='{row}'>").unwrap();
                let mut column = 1 + next(3) as u16;
                for _ in 0..width {
                    write!(xml, "<c r='{}'/>", CellRef { row, column }).unwrap();
                    column += 1 + u16::from(next(5) == 0);
                    if column > 400 {
                        break;
                    }
                }
                xml.push_str("</row>");
            }
            xml.push_str("</sheetData></worksheet>");
            let index = WorksheetIndex::parse(xml.as_bytes()).unwrap();
            row_heavy_wide +=
                usize::from(index.cells.len() <= index.rows.len() && !wide_rows.is_empty());
            seen[match index.row_search {
                RowSearch::Full => 0,
                RowSearch::Windowed => 1,
                RowSearch::Gapless { .. } => 2,
            }] += 1;
            let last = *numbers.last().unwrap();
            for _ in 0..40 {
                let r1 = if !wide_rows.is_empty() && next(2) == 0 {
                    let row = wide_rows[next(wide_rows.len() as u64) as usize];
                    row.saturating_sub(next(2) as u32).max(1)
                } else {
                    1 + next(u64::from(last) + 5) as u32
                };
                let span = match next(3) {
                    0 => 2,
                    1 => 50,
                    _ => u64::from(last) + 10,
                };
                let r2 = r1 + next(span) as u32;
                let c1 = 1 + next(450) as u16;
                let c2 = if next(5) == 0 {
                    CellRef::MAX_COLUMN
                } else {
                    c1 + next(300) as u16
                };
                let first = CellRef {
                    row: r1.min(CellRef::MAX_ROW),
                    column: if next(6) == 0 { 1 } else { c1 },
                };
                let second = CellRef {
                    row: r2.min(CellRef::MAX_ROW),
                    column: c2.min(CellRef::MAX_COLUMN),
                };
                let range = CellRange::new(first, second);
                let expected: Vec<_> = index
                    .cells
                    .iter()
                    .map(|cell| cell.address)
                    .filter(|&address| range.contains(address))
                    .collect();
                let entries = index.cells_in(Some(range));
                let (low, high) = entries.size_hint();
                assert!(low <= expected.len(), "case {case} {range} size_hint low");
                assert!(
                    high.is_none_or(|high| expected.len() <= high),
                    "case {case} {range} size_hint high"
                );
                let found: Vec<_> = entries.map(|cell| cell.address).collect();
                assert_eq!(found, expected, "case {case} {range} next");
                let folded = index
                    .cells_in(Some(range))
                    .fold(Vec::new(), |mut found, cell| {
                        found.push(cell.address);
                        found
                    });
                assert_eq!(folded, expected, "case {case} {range} fold");
                let mut rest = index.cells_in(Some(range));
                let taken: Vec<_> = rest
                    .by_ref()
                    .take(next(3) as usize)
                    .map(|cell| cell.address)
                    .collect();
                let after = rest.fold(taken, |mut found, cell| {
                    found.push(cell.address);
                    found
                });
                assert_eq!(after, expected, "case {case} {range} fold after next");
                assert_eq!(
                    index.cells_in(Some(range)).count(),
                    expected.len(),
                    "case {case} {range} count"
                );
            }
            for _ in 0..20 {
                let address = CellRef {
                    row: 1 + next(u64::from(last) + 3) as u32,
                    column: 1 + next(410) as u16,
                };
                let expected = index
                    .cells
                    .iter()
                    .find(|cell| cell.address == address)
                    .map(CellEntry::element);
                assert_eq!(index.cell(address), expected, "case {case} {address}");
            }
        }
        // Every kind of search is checked, and so are wide rows among more
        // empty ones, which a range may seek row by row.
        assert!(seen.iter().all(|&count| count > 40), "{seen:?}");
        assert!(row_heavy_wide > 5, "{row_heavy_wide}");
    }

    // Rows wider than `FILTERED_CELLS_PER_ROW` among many more empty rows:
    // range reads search the cell index, then seek such rows' columns one by
    // one, whatever search their rows' numbers choose.
    #[test]
    fn wide_rows_among_empty_ones_match_full_searches() {
        let wide = u16::try_from(FILTERED_CELLS_PER_ROW + 72).unwrap();
        let gapless: Vec<u32> = (4..=700).collect();
        let windowed: Vec<u32> = (4..=760).filter(|n| n % 70 != 0).collect();
        let full: Vec<u32> = (10_000..=10_700).collect();
        for (empty, search) in [(&gapless, 2), (&windowed, 1), (&full, 0)] {
            let mut xml = String::from(
                "<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><sheetData>",
            );
            for row in 1..=3 {
                write!(xml, "<row r='{row}'>").unwrap();
                for column in 1..=wide {
                    write!(xml, "<c r='{}'/>", CellRef { row, column }).unwrap();
                }
                xml.push_str("</row>");
            }
            for row in empty {
                write!(xml, "<row r='{row}'/>").unwrap();
            }
            xml.push_str("</sheetData></worksheet>");
            let index = WorksheetIndex::parse(xml.as_bytes()).unwrap();
            assert!(index.cells.len() <= index.rows.len());
            let kind = match index.row_search {
                RowSearch::Full => 0,
                RowSearch::Windowed => 1,
                RowSearch::Gapless { .. } => 2,
            };
            assert_eq!(kind, search);
            for range in [
                "B1:C3",
                "B2:C2",
                "A1:GR3",
                "GQ1:XFD3",
                "A2:XFD2",
                "B1:C2000",
                "D3:E650",
                "A5:XFD699",
                "A1:XFD1048576",
                "GR1:GR1048576",
            ] {
                let range: CellRange = range.parse().unwrap();
                let expected: Vec<_> = index
                    .cells
                    .iter()
                    .map(|cell| cell.address)
                    .filter(|&address| range.contains(address))
                    .collect();
                let found: Vec<_> = index
                    .cells_in(Some(range))
                    .map(|cell| cell.address)
                    .collect();
                assert_eq!(found, expected, "{search} {range}");
                let folded = index
                    .cells_in(Some(range))
                    .fold(Vec::new(), |mut found, cell| {
                        found.push(cell.address);
                        found
                    });
                assert_eq!(folded, expected, "{search} {range} fold");
                let mut rest = index.cells_in(Some(range));
                let mut after_next: Vec<_> =
                    rest.next().map(|cell| cell.address).into_iter().collect();
                after_next = rest.fold(after_next, |mut found, cell| {
                    found.push(cell.address);
                    found
                });
                assert_eq!(after_next, expected, "{search} {range} fold after next");
            }
        }
    }

    // Row searches narrowed by row numbers, and range reads that search the
    // cell index instead, must find what a search of every record finds.
    #[test]
    fn narrowed_searches_match_full_searches() {
        // More records than a narrow window, numbered with gaps.
        let wide: Vec<u32> = (1..=300).step_by(2).collect();
        // Enough records per missing number for windows: one with room to
        // spare, and one with exactly `RECORDS_PER_MISSING_ROW` per number.
        let windowed: Vec<u32> = (1..=42).filter(|&n| n != 10 && n != 30).collect();
        let boundary: Vec<u32> = (1..=68).filter(|n| ![5, 20, 21, 50].contains(n)).collect();
        let layouts: [&[u32]; 12] = [
            &wide,
            &windowed,
            &boundary,
            &[],
            &[7],
            &[1, 2, 3, 4, 5, 6],
            &[3, 4, 5, 9, 10, 11],
            &[1, 3, 5, 7, 9, 11, 13, 15, 17],
            &[2, 100, 101, 102, 1000],
            &[1, 2, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50],
            &[CellRef::MAX_ROW],
            &[1, CellRef::MAX_ROW],
        ];
        // The layouts that use each kind of search: full, windowed, gapless.
        let mut searches = [0; 3];
        for numbers in layouts {
            let mut xml = String::from(
                "<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><sheetData>",
            );
            // Every third row holds cells, so some layouts have fewer cells
            // than rows; the others are empty rows.
            for (position, number) in numbers.iter().enumerate() {
                if position % 3 == 0 {
                    write!(
                        xml,
                        "<row r='{number}'><c r='B{number}'/><c r='D{number}'/></row>"
                    )
                } else {
                    write!(xml, "<row r='{number}'/>")
                }
                .unwrap();
            }
            xml.push_str("</sheetData></worksheet>");
            let index = WorksheetIndex::parse(xml.as_bytes()).unwrap();
            searches[match index.row_search {
                RowSearch::Full => 0,
                RowSearch::Windowed => 1,
                RowSearch::Gapless { .. } => 2,
            }] += 1;
            let last = numbers.last().copied().unwrap_or(0);
            let rows = (0..=last.saturating_add(2).min(1_000))
                .chain(last.saturating_sub(2)..=last.saturating_add(2))
                .chain([CellRef::MAX_ROW]);
            for number in rows {
                assert_eq!(
                    index.rows_numbered(number, number),
                    index.rows.partition_point(|row| row.number < number)
                        ..index.rows.partition_point(|row| row.number <= number),
                    "{numbers:?} at {number}"
                );
                if (1..=CellRef::MAX_ROW).contains(&number) {
                    for column in 1..=5 {
                        let address = CellRef {
                            row: number,
                            column,
                        };
                        let expected = index
                            .cells
                            .iter()
                            .find(|cell| cell.address == address)
                            .map(CellEntry::element);
                        assert_eq!(index.cell(address), expected, "{numbers:?} {address}");
                    }
                }
            }
            let bounds = numbers
                .iter()
                .flat_map(|&number| [number.saturating_sub(1).max(1), number, number + 1])
                .chain([1, CellRef::MAX_ROW])
                .filter(|&row| row <= CellRef::MAX_ROW)
                .collect::<Vec<_>>();
            for &first in &bounds {
                for &last in bounds.iter().filter(|&&last| last >= first) {
                    for (first_column, last_column) in [(1, CellRef::MAX_COLUMN), (2, 2), (3, 5)] {
                        let range = CellRange::new(
                            CellRef {
                                row: first,
                                column: first_column,
                            },
                            CellRef {
                                row: last,
                                column: last_column,
                            },
                        );
                        let found: Vec<_> = index
                            .cells_in(Some(range))
                            .map(|cell| cell.address)
                            .collect();
                        let expected: Vec<_> = index
                            .cells
                            .iter()
                            .map(|cell| cell.address)
                            .filter(|&address| range.contains(address))
                            .collect();
                        assert_eq!(found, expected, "{numbers:?} {range}");
                        // Reductions take `fold`'s own traversal, from the
                        // start and after `next` has taken a cell.
                        let fold = |cells: CellEntries<'_>, mut addresses: Vec<CellRef>| {
                            cells.fold(addresses.len(), |count, cell| {
                                addresses.push(cell.address);
                                count + 1
                            });
                            addresses
                        };
                        assert_eq!(
                            fold(index.cells_in(Some(range)), Vec::new()),
                            expected,
                            "{numbers:?} {range} fold"
                        );
                        let mut rest = index.cells_in(Some(range));
                        let first_cell = rest.next().map(|cell| cell.address);
                        assert_eq!(
                            fold(rest, first_cell.into_iter().collect()),
                            expected,
                            "{numbers:?} {range} fold after next"
                        );
                    }
                }
            }
        }
        assert_eq!(searches, [9, 2, 1], "every kind of search is checked");
    }
}
