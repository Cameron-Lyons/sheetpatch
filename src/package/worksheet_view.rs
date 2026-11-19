use super::read_batch;
use super::sheet::Sheet;
use super::workbook::Workbook;
use crate::worksheet::PreparedWorksheet;
use crate::{CellContent, CellRange, CellRef, Result};
use std::fmt::{self, Debug, Formatter};
use std::sync::OnceLock;

/// A read-only worksheet view that reuses one validated XML index.
///
/// Create a view with [`Workbook::prepare_sheet`] when making repeated reads.
/// Its index is retained only for the lifetime of the view. The view borrows
/// the workbook, so its XML cannot change until the view is dropped.
pub struct WorksheetView<'workbook> {
    sheet: &'workbook Sheet,
    workbook: &'workbook Workbook,
    worksheet: PreparedWorksheet<'workbook>,
    used_range: OnceLock<Option<CellRange>>,
}

impl Debug for WorksheetView<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorksheetView")
            .field("sheet", self.sheet)
            .field("cells", &self.worksheet.cells().len())
            .finish_non_exhaustive()
    }
}

impl<'workbook> WorksheetView<'workbook> {
    pub(super) fn new(
        sheet: &'workbook Sheet,
        workbook: &'workbook Workbook,
        worksheet: PreparedWorksheet<'workbook>,
    ) -> Self {
        Self {
            sheet,
            workbook,
            worksheet,
            used_range: OnceLock::new(),
        }
    }

    /// The worksheet's name and resolved package path.
    #[must_use]
    pub fn sheet(&self) -> &Sheet {
        self.sheet
    }

    /// The address of every cell element in the worksheet, in row-major order.
    ///
    /// Cells that hold only a style and no value are included, as are cells
    /// added by pending edits. Pass the addresses to [`Self::get_cells_at`]
    /// to read the cells:
    ///
    /// ```
    /// # let bytes = include_bytes!("../../tests/fixtures/libreoffice.xlsx").to_vec();
    /// let book = sheetpatch::Workbook::from_bytes(bytes)?;
    /// let sheet = book.prepare_sheet("Data")?;
    /// let cells: Vec<_> = sheet.cells().collect();
    /// let contents = sheet.get_cells_at(cells.iter().copied())?;
    /// for (cell, content) in cells.iter().zip(&contents) {
    ///     println!("{cell}: {}", content.value);
    /// }
    /// assert_eq!(cells[0].to_string(), "A1");
    /// assert_eq!(contents[3].formula.as_deref(), Some("B1*2"));
    /// assert_eq!(sheet.cells().len(), cells.len());
    /// # Ok::<(), sheetpatch::Error>(())
    /// ```
    pub fn cells(&self) -> impl ExactSizeIterator<Item = CellRef> + '_ {
        self.worksheet.cells()
    }

    /// Read every existing cell in row-major order without collecting its
    /// address or searching the worksheet index again.
    ///
    /// Cells holding only formatting are included. Values are decoded lazily;
    /// calling this method again starts a fresh traversal of the same view.
    /// Shared strings are loaded only when a visited cell needs them.
    /// Cloning the iterator keeps its current position without decoding cells.
    ///
    /// # Errors
    ///
    /// Each item fails like [`Self::get_cell_at`] for that cell. An error does
    /// not prevent visiting subsequent cells. Once loading the shared-string
    /// table fails during a traversal, even transiently and even in another
    /// traversal, later cells that need it return that error rather than
    /// loading it again, until another call loads the table; a new traversal
    /// tries again.
    pub fn cell_contents(
        &self,
    ) -> impl Iterator<Item = Result<(CellRef, CellContent)>> + Clone + '_ {
        self.cell_contents_in(None)
    }

    /// Read existing cells inside `range`, or every existing cell for `None`.
    ///
    /// The range is tested before decoding, and is never expanded into empty
    /// cells. A whole-grid range therefore costs the same as [`Self::cell_contents`].
    /// The index skips unrelated rows. On a worksheet with no more cells than
    /// rows, it filters the cells between the range's corners by column, which
    /// also skips empty rows, unless they average more than 128 per requested
    /// row number, when it seeks the requested columns in each row. On others,
    /// it seeks them whenever the requested rows average more than one cell.
    /// Cells outside the range cannot produce value or shared-string errors.
    ///
    /// # Errors
    ///
    /// Each item fails like [`Self::get_cell_at`] for that cell, with the same
    /// worksheet, cell, or package-part context. As for [`Self::cell_contents`],
    /// a traversal does not load the shared-string table again after a load
    /// fails.
    pub fn cell_contents_in(
        &self,
        range: Option<CellRange>,
    ) -> impl Iterator<Item = Result<(CellRef, CellContent)>> + Clone + '_ {
        self.worksheet
            .cell_contents_in(
                range,
                self.workbook.shared_strings_for_pass(),
                self.workbook.uses_1904_dates(),
            )
            .map(|result| result.map_err(|error| self.workbook.read_context(self.sheet, error)))
    }

    /// Read every address in a rectangle, row by row, including missing cells.
    ///
    /// A missing cell yields a blank value with no formula or style index.
    /// Stored cells retain their style and formula, including style-only cells.
    /// The iterator decodes one cell at a time and can be cloned at its current
    /// position. Even a whole-grid range can be consumed a few items at a time.
    /// Use [`Self::cell_contents_in`] to visit only stored cells in the rectangle.
    ///
    /// # Errors
    ///
    /// Each item can fail like [`Self::get_cell_at`] for its address. Iteration
    /// can continue after an error. Shared-string loading follows the same
    /// retry behavior as [`Self::cell_contents`].
    ///
    /// ```
    /// # let bytes = include_bytes!("../../tests/fixtures/libreoffice.xlsx").to_vec();
    /// let book = sheetpatch::Workbook::from_bytes(bytes)?;
    /// let sheet = book.prepare_sheet("Data")?;
    /// for result in sheet.range_contents("A1:C2".parse()?) {
    ///     let (address, content) = result?;
    ///     println!("{address}: {}", content.value);
    /// }
    /// # Ok::<(), sheetpatch::Error>(())
    /// ```
    pub fn range_contents(
        &self,
        range: CellRange,
    ) -> impl Iterator<Item = Result<(CellRef, CellContent)>> + Clone + '_ {
        self.worksheet
            .range_contents(
                range,
                self.workbook.shared_strings_for_pass(),
                self.workbook.uses_1904_dates(),
            )
            .map(|result| result.map_err(|error| self.workbook.read_context(self.sheet, error)))
    }

    /// The smallest range containing every cell element, or `None` for a
    /// worksheet without cells.
    ///
    /// Like Excel's used range, this includes cells that hold only a style.
    /// The view computes it once from the indexed cells. This also works when
    /// another tool has left the worksheet's stored `<dimension>` stale.
    ///
    /// A single formatted cell far from the data, such as a style-only cell
    /// at `XFD1048576`, can make the range cover the whole grid of
    /// 17,179,869,184 cells. Reading a range returns one result for each of
    /// its cells, so check [`CellRange::cell_count`] before reading one, or
    /// read the cells that exist with [`Self::cells`].
    ///
    /// ```
    /// # let bytes = include_bytes!("../../tests/fixtures/libreoffice.xlsx").to_vec();
    /// let book = sheetpatch::Workbook::from_bytes(bytes)?;
    /// let sheet = book.prepare_sheet("Data")?;
    /// let range = sheet.used_range().unwrap();
    /// assert_eq!(range.to_string(), "A1:E1");
    /// if range.cell_count() <= 10_000 {
    ///     let values = sheet.get_cells_at(range)?;
    ///     assert_eq!(values.len(), 5);
    /// }
    /// # Ok::<(), sheetpatch::Error>(())
    /// ```
    #[must_use]
    pub fn used_range(&self) -> Option<CellRange> {
        *self.used_range.get_or_init(|| self.worksheet.used_range())
    }

    /// Read a cell without reparsing the worksheet.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCellReference`](crate::Error::InvalidCellReference)
    /// for an invalid address. A cell whose stored value is malformed or
    /// unsupported, or a shared-string table that cannot be loaded, returns
    /// [`Error::Xml`](crate::Error::Xml),
    /// [`Error::InvalidWorkbook`](crate::Error::InvalidWorkbook), or
    /// [`Error::Unsupported`](crate::Error::Unsupported), as described in
    /// [errors while reading](Workbook#errors-while-reading).
    /// Refused allocations for copied text return [`Error::Io`](crate::Error::Io)
    /// of kind [`std::io::ErrorKind::OutOfMemory`].
    pub fn get_cell(&self, address: &str) -> Result<CellContent> {
        self.get_cell_at(address.parse()?)
    }

    /// Read a cell using an already validated, one-based address.
    ///
    /// # Errors
    ///
    /// Fails like [`Self::get_cell`], other than for the address.
    pub fn get_cell_at(&self, cell: CellRef) -> Result<CellContent> {
        self.worksheet
            .read_cell(
                cell,
                || self.workbook.shared_strings(),
                self.workbook.uses_1904_dates(),
            )
            .map_err(|error| self.workbook.read_context(self.sheet, error))
    }

    /// Read at most 16,777,216 (2²⁴) addresses, retaining input order and
    /// duplicate requests.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`](crate::Error::Unsupported) when the
    /// iterator's size hint already exceeds the limit, before parsing any
    /// address. Otherwise returns
    /// [`Error::InvalidCellReference`](crate::Error::InvalidCellReference) for
    /// the first invalid address within the limit, or `Error::Unsupported`
    /// upon exceeding it, before reading any cell. Reads otherwise fail like
    /// [`Self::get_cell`] for the first cell that cannot be read.
    /// Results that cannot be allocated return
    /// [`Error::Io`](crate::Error::Io) of kind
    /// [`std::io::ErrorKind::OutOfMemory`].
    pub fn get_cells<I, S>(&self, addresses: I) -> Result<Vec<CellContent>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.read_cells(&read_batch::parse_addresses(addresses)?)
    }

    /// Read typed addresses, retaining input order and duplicate requests.
    /// A [`CellRange`] reads its cells in row-major order, one result for
    /// each cell, and one read returns at most 16,777,216 (2²⁴) cells.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`](crate::Error::Unsupported) when more
    /// than 16,777,216 cells are requested, before reading any cell, and
    /// otherwise fails like [`Self::get_cell`] for the first cell that cannot
    /// be read. Results that cannot be allocated return
    /// [`Error::Io`](crate::Error::Io) of kind
    /// [`std::io::ErrorKind::OutOfMemory`].
    pub fn get_cells_at(
        &self,
        cells: impl IntoIterator<Item = CellRef>,
    ) -> Result<Vec<CellContent>> {
        self.read_cells(&read_batch::collect_cells(cells)?)
    }

    fn read_cells(&self, cells: &[CellRef]) -> Result<Vec<CellContent>> {
        self.worksheet
            .read_cells(
                cells,
                || self.workbook.shared_strings(),
                self.workbook.uses_1904_dates(),
            )
            .map_err(|error| self.workbook.read_context(self.sheet, error))
    }
}
