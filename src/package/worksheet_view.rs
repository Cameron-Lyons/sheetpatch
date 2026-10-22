use super::read_batch;
use super::sheet::Sheet;
use super::workbook::Workbook;
use crate::worksheet::PreparedWorksheet;
use crate::{CellContent, CellRange, CellRef, Result};
use std::fmt::{self, Debug, Formatter};

/// A read-only worksheet view that reuses one validated XML index.
///
/// Create a view with [`Workbook::prepare_sheet`] when making repeated reads.
/// Its index is retained only for the lifetime of the view. The view borrows
/// the workbook, so its XML cannot change until the view is dropped.
pub struct WorksheetView<'workbook> {
    sheet: &'workbook Sheet,
    workbook: &'workbook Workbook,
    worksheet: PreparedWorksheet<'workbook>,
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

    /// The smallest range containing every cell element, or `None` for a
    /// worksheet without cells.
    ///
    /// Like Excel's used range, this includes cells that hold only a style.
    /// It is computed from the cells themselves rather than the worksheet's
    /// stored `<dimension>`, which other tools can leave stale.
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
        let mut cells = self.worksheet.cells();
        let first = cells.next()?;
        // Rows are in order, but a later row can start further left.
        let (left, right, last_row) = cells.fold(
            (first.column, first.column, first.row),
            |(left, right, _), cell| (left.min(cell.column), right.max(cell.column), cell.row),
        );
        Some(CellRange::new(
            CellRef {
                row: first.row,
                column: left,
            },
            CellRef {
                row: last_row,
                column: right,
            },
        ))
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

    /// Read multiple addresses, retaining input order and duplicate requests.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCellReference`](crate::Error::InvalidCellReference)
    /// for the first invalid address, before reading any cell, and otherwise
    /// fails like [`Self::get_cell`] for the first cell that cannot be read.
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
