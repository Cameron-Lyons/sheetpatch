use crate::{
    CellContent, CellEdit, CellRange, CellRef, CellValue, Error, Result,
    archive::{self, Archive, PartChanges},
    atomic::SourcePermissions,
    worksheet::{
        PreparedWorksheet, ReadError, SharedStrings, protection, read_cells, read_shared_strings,
    },
};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    fmt, fs,
    io::Write,
    path::Path,
    sync::{Mutex, OnceLock, PoisonError},
};

mod discovery;

/// A worksheet discovered through the package's relationships.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Sheet {
    name: String,
    path: String,
    visibility: SheetVisibility,
}

impl Sheet {
    /// The decoded worksheet name used by the workbook's read and edit methods.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The worksheet part's resolved path within the ZIP archive.
    pub fn path(&self) -> &str {
        &self.path
    }
    /// Whether Excel shows the worksheet's tab, from the sheet's `state` in
    /// the workbook. Hidden worksheets are read and edited like visible ones.
    pub fn visibility(&self) -> SheetVisibility {
        self.visibility
    }

    // Names this worksheet in an error raised while reading, preparing, or
    // patching it, retaining the error's variant. Every worksheet error passes
    // through here, which is what gives a protected-cell refusal its sheet.
    fn context(&self, error: Error) -> Error {
        match error {
            Error::ProtectedCell { cell, reason, .. } => Error::ProtectedCell {
                sheet: self.name.clone(),
                cell,
                reason,
            },
            error => error.context(format_args!("worksheet {:?}", self.name)),
        }
    }
}

// Names the package part an error concerns, such as the shared-string table
// or a table part read while serving a worksheet operation, retaining the
// error's variant. Such errors are never attributed to the worksheet.
fn in_part(path: &str, error: Error) -> Error {
    error.context(format_args!("part {path:?}"))
}

/// Whether Excel shows a worksheet's tab.
///
/// Additional states may be added in compatible releases. Callers should
/// include a fallback arm when matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SheetVisibility {
    /// The tab is shown. A sheet without a `state` attribute is visible.
    Visible,
    /// The tab is hidden, and Excel's Unhide command can show it again
    /// (`state="hidden"`).
    Hidden,
    /// The tab is hidden and not offered by Excel's Unhide command; only
    /// macros or property editors can show it again (`state="veryHidden"`).
    VeryHidden,
}

impl SheetVisibility {
    /// The state's SpreadsheetML spelling: `"visible"`, `"hidden"`, or
    /// `"veryHidden"`, as in the CLI's `list --json` output.
    ///
    /// ```
    /// use sheetpatch::SheetVisibility;
    ///
    /// assert_eq!(SheetVisibility::VeryHidden.as_str(), "veryHidden");
    /// ```
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
            Self::VeryHidden => "veryHidden",
        }
    }
}

/// An existing OOXML workbook with pending surgical cell edits.
///
/// Untouched ZIP entries retain their local headers, compressed bytes, extra
/// fields, and comments. Changed worksheet records are replaced in the archive's
/// local-record sequence; central-directory offsets may change. A save with no
/// edits returns the original archive byte for byte. This does not calculate
/// formulas or refresh charts/pivot caches.
///
/// ```no_run
/// use sheetpatch::{CellValue, Workbook};
/// let mut book = Workbook::open("report.xlsm")?;
/// book.set_cell("Summary", "B2", "Revised")?;
/// book.set_cell("Summary", "C2", 42.5)?;
/// book.set_cell("Summary", "D2", true)?;
/// book.set_cell("Summary", "E2", CellValue::Blank)?;
/// book.save("report-edited.xlsm")?;
/// # Ok::<(), sheetpatch::Error>(())
/// ```
///
/// # Edit restrictions
///
/// Edits change scalar cell values only. An edit that could leave the
/// workbook inconsistent, or lose information the crate cannot carry over, is
/// refused before anything changes: the error leaves every pending edit as it
/// was, and a batch or transaction that contains a refused edit applies none
/// of its edits.
///
/// These cells are protected. Edits to them return [`Error::ProtectedCell`],
/// which names the worksheet, the cell, and the reason:
///
/// - **Formula cells.** A formula's stored value is a cached result, so no
///   value is written over one, not even the current value.
/// - **Cells in a formula range.** The `ref` range of a shared, array, or
///   data-table formula is computed by that formula group, including cells
///   that have no formula of their own.
/// - **Merged cells other than the anchor.** Excel shows a merged range's
///   top-left cell, which stays editable; the rest of the range is protected.
/// - **Excel table header cells.** Excel requires a header cell's text to
///   equal its table column's name, so a header may be rewritten with its
///   current value but not changed. Table bodies stay editable.
/// - **Cells with value metadata** (a `vm` attribute), such as linked data
///   types and other rich values, whose metadata describes the current value.
///   Writing the current value again is allowed.
/// - **Cells with cell metadata** (a `cm` attribute), whose metadata can require
///   removal or other handling when its value is edited or cleared. These
///   metadata rules are not interpreted; writing the current value again is allowed.
///
/// These edits return [`Error::Unsupported`]:
///
/// - Any edit of a digitally signed package (see [`Self::is_signed`]), which
///   would invalidate the signature.
/// - Any edit of a worksheet with a shared, array, or data-table formula group
///   that has no reference range, since any cell could belong to it.
/// - Adding a cell whose default style cannot be determined, because its
///   row's custom format or the worksheet's column definitions are malformed,
///   overlapping, or inside an unsupported container. Existing cells stay
///   editable.
/// - Changing a cell in a row whose `spans` hint is malformed, or in a
///   worksheet whose `dimension` is repeated or lacks a valid reference,
///   since the edit may need to widen it. Reads ignore both hints.
/// - Replacing a value whose payload holds markup the crate does not
///   recognize, which the replacement would discard.
///
/// Values are validated before any worksheet is read: non-finite numbers,
/// text longer than Excel's 32,767 UTF-16 code units, and error values that
/// are not Excel error codes return [`Error::InvalidValue`]. From a batch,
/// such as [`Self::set_cells`], its message names the cell, as in
/// `B7: numbers must be finite`, but not the worksheet.
///
/// # Errors while reading
///
/// Worksheets, the shared-string table, and the parts that define a
/// worksheet's tables are parsed on first use, so a damaged or unsupported
/// part is reported by the first read or edit that needs it, as
/// [`Error::Xml`], [`Error::InvalidWorkbook`], or [`Error::Unsupported`].
/// Malformed or ambiguous content, such as a value its cell type cannot hold,
/// a cell reference that is not an address, or rows out of order, returns
/// [`Error::InvalidWorkbook`]. Valid content that cannot be indexed safely,
/// such as rows without explicit numbers or cells inside a
/// markup-compatibility branch, returns [`Error::Unsupported`].
/// Messages about a worksheet name it, and messages about another part, such
/// as a shared-string table that cannot be loaded, name that part's path in
/// the archive instead.
///
/// Text with an unpaired UTF-16 surrogate escape, such as `_xD83D_` alone,
/// is valid but cannot be returned as a Rust string, so reading it returns
/// [`Error::Unsupported`]. In the shared-string table this concerns only
/// that entry: reads of cells that use it fail with a message naming the
/// part and the entry's index, such as
/// `part "xl/sharedStrings.xml": string 1: …`, while other cells read
/// normally. Rewriting such a cell counts as a change, so an edit of an
/// Excel table header cell that uses it, whose current value must be read,
/// fails with the same error.
pub struct Workbook {
    archive: Archive,
    sheets: Vec<Sheet>,
    sheet_xml: Vec<SheetXml>,
    signed: bool,
    date1904: bool,
    sheet_indexes: BTreeMap<String, usize>,
    shared_strings_path: Option<String>,
    // The outcome of the table's first load. Untouched parts never change,
    // so a table that fails to load fails the same way on every later use.
    shared_strings: OnceLock<std::result::Result<SharedStrings, Error>>,
    // Header rows of the table parts read so far, by ZIP member name. Several
    // worksheets can relate to one table part, which is then parsed only once.
    table_parts: Mutex<HashMap<String, Option<CellRange>>>,
    // Permission bits and group of the file this workbook was opened from,
    // used as the basis for new save destinations. ACLs are not copied.
    source_permissions: Option<SourcePermissions>,
}

// The archive and XML are omitted: they can be many megabytes.
impl fmt::Debug for Workbook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workbook")
            .field("sheets", &self.sheets)
            .field("signed", &self.signed)
            .field("date1904", &self.date1904)
            .field(
                "changed_parts",
                &self
                    .changed_parts()
                    .parts()
                    .map(|(path, _)| path)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct SheetXml {
    // The part URI can differ from its ZIP name in legacy decoded archives.
    uri: String,
    original: OnceLock<Vec<u8>>,
    // Present only when the XML differs from the original, and the only record
    // that the worksheet has a pending change.
    current: Option<Vec<u8>>,
    // Header rows of the worksheet's Excel tables, resolved on its first edit.
    table_headers: OnceLock<Vec<CellRange>>,
}

// Discovery rejects sheets that share a worksheet part, so names are unique.
struct ChangedParts<'a> {
    sheets: &'a [Sheet],
    sheet_xml: &'a [SheetXml],
}

impl PartChanges for ChangedParts<'_> {
    fn parts(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.sheets
            .iter()
            .zip(self.sheet_xml)
            .filter_map(|(sheet, xml)| Some((sheet.path.as_str(), xml.current.as_deref()?)))
    }
}

/// A read-only worksheet view that reuses one validated XML index.
///
/// Create a view with [`Workbook::prepare_sheet`] when making repeated reads.
/// Its index is retained only for the lifetime of the view. The view borrows
/// the workbook, so its XML cannot change until the view is dropped.
pub struct WorksheetView<'a> {
    sheet: &'a Sheet,
    workbook: &'a Workbook,
    worksheet: PreparedWorksheet<'a>,
}

impl fmt::Debug for WorksheetView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorksheetView")
            .field("sheet", self.sheet)
            .field("cells", &self.worksheet.cells().len())
            .finish_non_exhaustive()
    }
}

impl WorksheetView<'_> {
    /// The worksheet's name and resolved package path.
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
    /// # let bytes = include_bytes!("../tests/fixtures/libreoffice.xlsx").to_vec();
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
    /// # let bytes = include_bytes!("../tests/fixtures/libreoffice.xlsx").to_vec();
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
    /// Returns [`Error::InvalidCellReference`] for an invalid address. A cell
    /// whose stored value is malformed or unsupported, or a shared-string
    /// table that cannot be loaded, returns [`Error::Xml`],
    /// [`Error::InvalidWorkbook`], or [`Error::Unsupported`], as described in
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
                self.workbook.date1904,
            )
            .map_err(|error| self.workbook.read_context(self.sheet, error))
    }

    /// Read multiple addresses, retaining input order and duplicate requests.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCellReference`] for the first invalid address,
    /// before reading any cell, and otherwise fails like [`Self::get_cell`]
    /// for the first cell that cannot be read. Results that cannot be
    /// allocated return [`Error::Io`] of kind
    /// [`std::io::ErrorKind::OutOfMemory`].
    pub fn get_cells<I, S>(&self, addresses: I) -> Result<Vec<CellContent>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.read_cells(&parse_addresses(addresses)?)
    }

    /// Read typed addresses, retaining input order and duplicate requests.
    /// A [`CellRange`] reads its cells in row-major order, one result for
    /// each cell, and one read returns at most 16,777,216 (2²⁴) cells.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when more than 16,777,216 cells are
    /// requested, before reading any cell, and otherwise fails like
    /// [`Self::get_cell`] for the first cell that cannot be read. Results
    /// that cannot be allocated return [`Error::Io`] of kind
    /// [`std::io::ErrorKind::OutOfMemory`].
    pub fn get_cells_at(
        &self,
        cells: impl IntoIterator<Item = CellRef>,
    ) -> Result<Vec<CellContent>> {
        self.read_cells(&collect_cells(cells)?)
    }

    fn read_cells(&self, cells: &[CellRef]) -> Result<Vec<CellContent>> {
        self.worksheet
            .read_cells(
                cells,
                || self.workbook.shared_strings(),
                self.workbook.date1904,
            )
            .map_err(|error| self.workbook.read_context(self.sheet, error))
    }
}

fn parse_addresses<S: AsRef<str>>(addresses: impl IntoIterator<Item = S>) -> Result<Vec<CellRef>> {
    addresses
        .into_iter()
        .map(|address| address.as_ref().parse())
        .collect()
}

// The most cells one typed batch read returns: sixteen whole columns, and
// sixteen times the CLI's `get` limit. A worksheet part is read only up to
// 64 MiB, and each cell element needs an explicit reference of its own, so
// every cell of a worksheet as read fits. The addresses and results of a full
// read take a little over 1 GiB besides their text, where a range over the
// whole grid would need more than a terabyte.
const MAX_READ_CELLS: usize = 1 << 24;

// Collects typed addresses for a batch read, refusing a request over the limit
// before reading any cell. A range's exact size hint is checked before any
// address is stored, and other iterators are cut off at the limit.
fn collect_cells(cells: impl IntoIterator<Item = CellRef>) -> Result<Vec<CellRef>> {
    let too_many = || {
        Error::Unsupported(format!(
            "too many cells: one read returns at most {MAX_READ_CELLS}"
        ))
    };
    let cells = cells.into_iter();
    let (expected, _) = cells.size_hint();
    if expected > MAX_READ_CELLS {
        return Err(too_many());
    }
    let mut collected = Vec::new();
    collected
        .try_reserve_exact(expected)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
    for cell in cells {
        if collected.len() == MAX_READ_CELLS {
            return Err(too_many());
        }
        collected.push(cell);
    }
    Ok(collected)
}

impl Workbook {
    /// Read a workbook file and discover worksheets through its relationships.
    /// Worksheet XML and shared strings are parsed lazily when first accessed.
    ///
    /// Input larger than the largest possible ZIP32 archive (about 8 GiB) is
    /// rejected with [`Error::Unsupported`]: a regular file before it is read,
    /// and a pipe or device once that much has been read. On Unix, a regular
    /// file's permission bits and group apply to new destinations of
    /// [`Self::save`] and [`Self::save_compact`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the file cannot be opened or read, and
    /// otherwise fails like [`Self::from_bytes`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = fs::File::open(path)?;
        let metadata = file.metadata()?;
        // Devices and pipes have no useful length; reading stops at the limit.
        let file_len = metadata.is_file().then_some(metadata.len());
        let mut workbook = Self::from_bytes(archive::read_archive(&file, file_len)?)?;
        // Pipes and devices carry no meaningful file permissions to inherit.
        workbook.source_permissions = metadata
            .is_file()
            .then(|| SourcePermissions::new(&file, &metadata));
        Ok(workbook)
    }

    /// Read a workbook from a stream, such as standard input, and discover
    /// worksheets through its relationships. Worksheet XML and shared strings
    /// are parsed lazily when first accessed.
    ///
    /// At most the largest possible ZIP32 archive (about 8 GiB) is read;
    /// longer input is rejected with [`Error::Unsupported`] once that much has
    /// been read. Read errors are returned as [`Error::Io`]. No source
    /// permissions are recorded, so new destinations of [`Self::save`] and
    /// [`Self::save_compact`] are created with the default permissions, as
    /// for [`Self::from_bytes`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when reading fails, [`Error::Unsupported`] for
    /// input beyond the size limit, and otherwise fails like
    /// [`Self::from_bytes`].
    ///
    /// ```no_run
    /// use sheetpatch::Workbook;
    /// let book = Workbook::from_reader(std::io::stdin().lock())?;
    /// println!("{} worksheets", book.sheets().len());
    /// # Ok::<(), sheetpatch::Error>(())
    /// ```
    pub fn from_reader(reader: impl std::io::Read) -> Result<Self> {
        Self::from_bytes(archive::read_archive(reader, None)?)
    }

    /// Open an in-memory workbook, retaining its original archive bytes.
    /// Worksheet XML and shared strings are parsed lazily when first accessed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] for packages outside the supported scope,
    /// such as legacy `.xls` and password-encrypted (OLE compound) files,
    /// binary `.xlsb` workbooks, ZIP64 or multi-disk archives, encrypted
    /// ZIP entries, and a sheet name with an unpaired UTF-16 surrogate
    /// escape, which names the sheet's position in the workbook. Returns
    /// [`Error::InvalidWorkbook`] or [`Error::Xml`] for a damaged or ambiguous
    /// archive or package metadata. Worksheet XML is checked when a worksheet
    /// is first read or edited.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let archive = Archive::new(bytes)?;
        let discovery::PackageMetadata {
            sheets,
            signed,
            date1904,
            shared_strings_path,
        } = discovery::discover(&archive)?;
        let (sheets, sheet_xml): (Vec<_>, Vec<_>) = sheets
            .into_iter()
            .map(|discovery::DiscoveredSheet { sheet, uri }| {
                (
                    sheet,
                    SheetXml {
                        uri,
                        ..SheetXml::default()
                    },
                )
            })
            .unzip();
        let sheet_indexes = sheets
            .iter()
            .enumerate()
            .map(|(index, sheet)| (sheet.name.clone(), index))
            .collect();
        Ok(Self {
            archive,
            sheets,
            sheet_xml,
            signed,
            date1904,
            sheet_indexes,
            shared_strings_path,
            shared_strings: OnceLock::new(),
            table_parts: Mutex::default(),
            source_permissions: None,
        })
    }

    /// The editable worksheets in workbook tab order.
    /// Chart, dialog, and macro sheets are omitted. Names match exactly,
    /// including case, in all read and edit methods.
    pub fn sheets(&self) -> &[Sheet] {
        &self.sheets
    }

    /// The editable worksheet with this exact, case-sensitive name.
    pub fn sheet(&self, name: &str) -> Option<&Sheet> {
        self.sheet_indexes
            .get(name)
            .map(|&index| &self.sheets[index])
    }

    /// Whether the package is digitally signed.
    ///
    /// Editing would invalidate the signature, so every edit of a signed
    /// workbook is refused with [`Error::Unsupported`]. Reads are unaffected.
    pub fn is_signed(&self) -> bool {
        self.signed
    }

    /// Whether the workbook uses the 1904 date system.
    ///
    /// Excel stores a date as a serial number of days. When this returns
    /// `true`, serials for date-formatted cells count from 1904-01-01, which
    /// is serial 0; otherwise they use the 1900 date system, in which
    /// 1900-01-01 is serial 1. For example, 2024-01-15 is 43844 in the 1904
    /// system and 45306 in the 1900 system. Write date serials in the
    /// workbook's own system; ISO 8601 date cells (`t="d"`) also read as
    /// serials in it.
    pub fn uses_1904_dates(&self) -> bool {
        self.date1904
    }

    /// Set a scalar cell value, inserting a cell/row if needed.
    ///
    /// The cell keeps its style and other attributes. Text and numbers are
    /// validated before applying changes, and failed edits leave this
    /// workbook's pending edits unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SheetNotFound`] if no worksheet has this name,
    /// [`Error::InvalidCellReference`] for an invalid address, and
    /// [`Error::InvalidValue`] for a value no cell can hold. Edits that the
    /// [edit restrictions](Self#edit-restrictions) refuse return
    /// [`Error::ProtectedCell`] or [`Error::Unsupported`], and a worksheet
    /// that cannot be read fails as described in
    /// [errors while reading](Self#errors-while-reading).
    pub fn set_cell(
        &mut self,
        sheet: &str,
        address: &str,
        value: impl Into<CellValue>,
    ) -> Result<()> {
        // The sheet is resolved first, as in the batch and read methods.
        let index = self.sheet_index(sheet)?;
        self.set_cell_in_sheet(index, address.parse()?, value.into())
    }

    /// Set a scalar value using an already validated, one-based cell address.
    ///
    /// The value and worksheet restrictions are checked before applying changes,
    /// just as with [`Self::set_cell`]. No A1 string conversion is needed.
    ///
    /// # Errors
    ///
    /// Fails like [`Self::set_cell`], other than for the address.
    pub fn set_cell_at(
        &mut self,
        sheet: &str,
        cell: CellRef,
        value: impl Into<CellValue>,
    ) -> Result<()> {
        let index = self.sheet_index(sheet)?;
        self.set_cell_in_sheet(index, cell, value.into())
    }

    /// Apply edits to a worksheet in one parse and one XML patch.
    /// Duplicate addresses use the last value; every input is validated.
    /// The entire batch is committed only after every edit succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SheetNotFound`] before reading the batch if no
    /// worksheet has this name, then the first invalid address or value, where
    /// an [`Error::InvalidValue`] message names its cell, as in
    /// `B7: numbers must be finite`. Otherwise it fails like [`Self::set_cell`]
    /// for one refused edit, which need not be the batch's first. Formula
    /// cells, then cells in formula ranges, then merged cells are checked
    /// across the whole batch, each in row-major order. The cells are then
    /// patched in row-major order, where metadata and payload refusals arise,
    /// and Excel table headers are checked last. No edit of a failed batch is
    /// applied.
    pub fn set_cells<I, S, V>(&mut self, sheet: &str, cells: I) -> Result<()>
    where
        I: IntoIterator<Item = (S, V)>,
        S: AsRef<str>,
        V: Into<CellValue>,
    {
        let index = self.sheet_index(sheet)?;
        self.set_cells_in_sheet(
            index,
            cells
                .into_iter()
                .map(|(address, value)| address.as_ref().parse().map(|cell| (cell, value))),
        )
    }

    /// Apply a batch using typed addresses, parsing the worksheet once.
    ///
    /// Duplicate addresses use the last value, but every value is validated.
    /// The entire batch commits only after every edit succeeds. An empty batch
    /// still requires an existing worksheet. No A1 string conversions are needed.
    /// A [`CellRange`]'s [`cells`](CellRange::cells) zipped with values writes
    /// a block in row-major order.
    ///
    /// # Errors
    ///
    /// Fails like [`Self::set_cells`], other than for addresses. An
    /// [`Error::InvalidValue`] message names its cell.
    pub fn set_cells_at<I, V>(&mut self, sheet: &str, cells: I) -> Result<()>
    where
        I: IntoIterator<Item = (CellRef, V)>,
        V: Into<CellValue>,
    {
        let index = self.sheet_index(sheet)?;
        self.set_cells_in_sheet(index, cells.into_iter().map(Ok))
    }

    // The caller names the one cell, so its value error does not repeat it.
    fn set_cell_in_sheet(&mut self, index: usize, cell: CellRef, value: CellValue) -> Result<()> {
        value.validate()?;
        self.apply_grouped_edits([(index, BTreeMap::from([(cell, value)]))])
    }

    // A value error names its cell, since a batch can hold many, but not the
    // worksheet: only errors raised inside a worksheet name it.
    fn set_cells_in_sheet<I, V>(&mut self, index: usize, cells: I) -> Result<()>
    where
        I: IntoIterator<Item = Result<(CellRef, V)>>,
        V: Into<CellValue>,
    {
        let mut values = BTreeMap::new();
        for cell in cells {
            let (cell, value) = cell?;
            let value = value.into();
            value.validate().map_err(|error| error.context(cell))?;
            values.insert(cell, value);
        }
        if values.is_empty() {
            return Ok(());
        }
        self.apply_grouped_edits([(index, values)])
    }

    /// Apply a transaction across worksheets. On any error, all pending cell
    /// values remain unchanged. Worksheets are decompressed lazily and cached.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SheetNotFound`] for the first edit that names no
    /// worksheet, and otherwise fails like [`Self::set_cell`] for one refused
    /// edit. Worksheets are checked in the order of [`Self::sheets`], not of
    /// the edits, and each worksheet's edits in the order described for
    /// [`Self::set_cells`]. No edit of a failed transaction is applied.
    pub fn apply_edits(&mut self, edits: impl IntoIterator<Item = CellEdit>) -> Result<()> {
        let mut grouped: BTreeMap<usize, BTreeMap<CellRef, CellValue>> = BTreeMap::new();
        for edit in edits {
            let index = self.sheet_index(&edit.sheet)?;
            grouped
                .entry(index)
                .or_default()
                .insert(edit.cell, edit.value);
        }
        self.apply_grouped_edits(grouped)
    }

    fn apply_grouped_edits(
        &mut self,
        grouped: impl IntoIterator<Item = (usize, BTreeMap<CellRef, CellValue>)>,
    ) -> Result<()> {
        let mut grouped = grouped.into_iter().peekable();
        if grouped.peek().is_none() {
            return Ok(());
        }
        if self.signed {
            return Err(Error::Unsupported(
                "editing a digitally signed OOXML package would invalidate its signature".into(),
            ));
        }
        let mut staged = Vec::with_capacity(grouped.size_hint().0);
        for (index, cells) in grouped {
            let in_sheet = |error| self.sheets[index].context(error);
            let original = self.original_sheet(index).map_err(in_sheet)?;
            let current = self.sheet_xml[index].current.as_deref().unwrap_or(original);
            if cells.is_empty() {
                continue;
            }
            // Shared strings only recognize rewriting a cell's current text,
            // so a table that cannot be loaded leaves such an edit a change.
            let worksheet = PreparedWorksheet::parse(current).map_err(in_sheet)?;
            let Cow::Owned(patched) = worksheet
                .patch_cells(&cells, || self.loaded_shared_strings(), self.date1904)
                .map_err(in_sheet)?
            else {
                continue;
            };
            self.check_table_headers(index, worksheet, &cells)?;
            let changed = (patched != original).then_some(patched);
            staged.push((index, changed));
        }
        for (index, changed) in staged {
            self.sheet_xml[index].current = changed;
        }
        Ok(())
    }

    // Excel requires each table header cell's text to equal its table column's
    // name, and repairs or removes a table whose header differs. An edit may
    // rewrite a header cell's current value, as it reads, but not change it.
    // `worksheet` is the edited worksheet as it was before the edits. Errors
    // name the worksheet or the package part they concern.
    fn check_table_headers(
        &self,
        index: usize,
        worksheet: PreparedWorksheet<'_>,
        cells: &BTreeMap<CellRef, CellValue>,
    ) -> Result<()> {
        let sheet = &self.sheets[index];
        // A worksheet's first change resolves its tables, which can read and
        // parse many relationships. The index is released meanwhile and
        // rebuilt only for an edit in a header; once the tables are known, it
        // is kept for the header reads instead.
        let xml = worksheet.xml();
        let retained = if self.sheet_xml[index].table_headers.get().is_some() {
            Some(worksheet)
        } else {
            drop(worksheet);
            None
        };
        let edits = header_cells(cells, self.table_headers(index)?);
        if edits.is_empty() {
            return Ok(());
        }
        let worksheet = match retained {
            Some(worksheet) => worksheet,
            None => PreparedWorksheet::parse(xml).map_err(|error| sheet.context(error))?,
        };
        let mut changed_header = None;
        // Keep one value at a time: many headers can refer to the same long
        // shared string. Validate all reads before reporting the first changed
        // header, preserving read-error precedence without retaining copies.
        for (cell, value) in edits {
            let current = worksheet
                .read_cell(cell, || self.shared_strings(), self.date1904)
                .map_err(|error| self.read_context(sheet, error))?;
            if current.value != *value {
                changed_header.get_or_insert(cell);
            }
        }
        if let Some(cell) = changed_header {
            return Err(Error::ProtectedCell {
                sheet: sheet.name.clone(),
                cell,
                reason: protection::TABLE_HEADER.into(),
            });
        }
        Ok(())
    }

    /// Read a cell's current value, cached formula result, and style index.
    /// Missing cells return a blank value without modifying the workbook.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SheetNotFound`] if no worksheet has this name and
    /// [`Error::InvalidCellReference`] for an invalid address. A worksheet
    /// that cannot be parsed, a cell whose stored value is malformed or
    /// unsupported, or a shared-string table that cannot be loaded returns
    /// [`Error::Xml`], [`Error::InvalidWorkbook`], or [`Error::Unsupported`],
    /// as described in [errors while reading](Self#errors-while-reading).
    pub fn get_cell(&self, sheet: &str, address: &str) -> Result<CellContent> {
        let index = self.sheet_index(sheet)?;
        Ok(self
            .read_cells_in_sheet(index, &[address.parse()?])?
            .remove(0))
    }

    /// Read a cell using an already validated, one-based address.
    ///
    /// Returns the current scalar value, cached formula result, and style index,
    /// just as with [`Self::get_cell`]. Missing cells return a blank value.
    ///
    /// # Errors
    ///
    /// Fails like [`Self::get_cell`], other than for the address.
    pub fn get_cell_at(&self, sheet: &str, cell: CellRef) -> Result<CellContent> {
        let index = self.sheet_index(sheet)?;
        Ok(self.read_cells_in_sheet(index, &[cell])?.remove(0))
    }

    /// Read many cells with one worksheet parse, retaining input order.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SheetNotFound`] if no worksheet has this name, then
    /// [`Error::InvalidCellReference`] for the first invalid address, before
    /// reading any cell, and otherwise fails like [`Self::get_cell`] for the
    /// first cell that cannot be read. Results that cannot be allocated
    /// return [`Error::Io`] of kind [`std::io::ErrorKind::OutOfMemory`].
    pub fn get_cells<I, S>(&self, sheet: &str, addresses: I) -> Result<Vec<CellContent>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let index = self.sheet_index(sheet)?;
        self.read_cells_in_sheet(index, &parse_addresses(addresses)?)
    }

    /// Read a batch using typed addresses, parsing the worksheet once.
    ///
    /// Input order and duplicates are retained; missing cells return blanks.
    /// An empty batch still requires an existing worksheet. No A1 string
    /// conversions are needed, and a [`CellRange`] reads its cells in
    /// row-major order, one result for each cell. One read returns at most
    /// 16,777,216 (2²⁴) cells.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SheetNotFound`] if no worksheet has this name, then
    /// [`Error::Unsupported`] when more than 16,777,216 cells are requested,
    /// before reading any cell, and otherwise fails like [`Self::get_cell`]
    /// for the first cell that cannot be read. Results that cannot be
    /// allocated return [`Error::Io`] of kind
    /// [`std::io::ErrorKind::OutOfMemory`].
    pub fn get_cells_at(
        &self,
        sheet: &str,
        cells: impl IntoIterator<Item = CellRef>,
    ) -> Result<Vec<CellContent>> {
        let index = self.sheet_index(sheet)?;
        self.read_cells_in_sheet(index, &collect_cells(cells)?)
    }

    fn read_cells_in_sheet(&self, index: usize, cells: &[CellRef]) -> Result<Vec<CellContent>> {
        if cells.is_empty() {
            return Ok(Vec::new());
        }
        let sheet = &self.sheets[index];
        read_cells(
            self.current_sheet(index)
                .map_err(|error| sheet.context(error))?,
            cells,
            || self.shared_strings(),
            self.date1904,
        )
        .map_err(|error| self.read_context(sheet, error))
    }

    /// Prepare a worksheet for repeated reads using a single validated XML index.
    ///
    /// The view reflects pending edits and borrows this workbook. It validates
    /// the worksheet when created and loads shared strings only when needed.
    /// Dropping the view releases its index; ordinary read methods do not retain
    /// an index between calls.
    ///
    /// ```no_run
    /// let book = sheetpatch::Workbook::open("report.xlsx")?;
    /// let sheet = book.prepare_sheet("Summary")?;
    /// for address in ["B2", "C2", "D2"] {
    ///     println!("{:?}", sheet.get_cell(address)?.value);
    /// }
    /// # Ok::<(), sheetpatch::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::SheetNotFound`] if no worksheet has this name, and
    /// [`Error::Xml`], [`Error::InvalidWorkbook`], or [`Error::Unsupported`]
    /// for a worksheet that cannot be parsed and indexed.
    pub fn prepare_sheet(&self, sheet: &str) -> Result<WorksheetView<'_>> {
        let index = self.sheet_index(sheet)?;
        let sheet = &self.sheets[index];
        let worksheet = self
            .current_sheet(index)
            .and_then(PreparedWorksheet::parse)
            .map_err(|error| sheet.context(error))?;
        Ok(WorksheetView {
            sheet,
            workbook: self,
            worksheet,
        })
    }

    /// Whether any package parts have pending edits.
    pub fn has_changes(&self) -> bool {
        self.sheet_xml.iter().any(|xml| xml.current.is_some())
    }

    /// Discard every pending edit, restoring byte-identical original output.
    pub fn reset_changes(&mut self) {
        for xml in &mut self.sheet_xml {
            xml.current = None;
        }
    }

    fn current_sheet(&self, index: usize) -> Result<&[u8]> {
        self.sheet_xml[index]
            .current
            .as_deref()
            .map(Ok)
            .unwrap_or_else(|| self.original_sheet(index))
    }

    fn changed_parts(&self) -> ChangedParts<'_> {
        ChangedParts {
            sheets: &self.sheets,
            sheet_xml: &self.sheet_xml,
        }
    }

    fn sheet_index(&self, name: &str) -> Result<usize> {
        self.sheet_indexes
            .get(name)
            .copied()
            .ok_or_else(|| Error::SheetNotFound(name.to_owned()))
    }

    // The worksheet caches are loaded outside `get_or_init`, so a failed load
    // is not cached and fails the call that needed it, which can be retried.
    // Concurrent first loads can only duplicate the work.
    fn original_sheet(&self, index: usize) -> Result<&[u8]> {
        let cache = &self.sheet_xml[index].original;
        if let Some(xml) = cache.get() {
            return Ok(xml);
        }
        let xml = self.archive.read(&self.sheets[index].path)?;
        Ok(cache.get_or_init(|| xml))
    }

    // Errors name the relationships or table part they concern.
    fn table_headers(&self, index: usize) -> Result<&[CellRange]> {
        let cache = &self.sheet_xml[index].table_headers;
        if let Some(headers) = cache.get() {
            return Ok(headers);
        }
        // Poisoning is harmless: entries are inserted whole, never partially.
        let mut parts = self
            .table_parts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let headers =
            discovery::table_headers(&self.archive, &self.sheet_xml[index].uri, &mut parts)?;
        drop(parts);
        Ok(cache.get_or_init(|| headers))
    }

    // Like `Sheet::context`, but the shared-string table's errors are not the
    // worksheet's. A failure to load the table already names that part; an
    // entry that cannot be decoded is named with its part and index.
    fn read_context(&self, sheet: &Sheet, error: ReadError) -> Error {
        match error {
            ReadError::Worksheet(error) => sheet.context(error),
            ReadError::SharedStrings(error) => error,
            ReadError::UndecodableString { index, error } => {
                let error = error.context(format_args!("string {index}"));
                match &self.shared_strings_path {
                    Some(path) => in_part(path, error),
                    None => error,
                }
            }
        }
    }

    // The shared-string table, or `None` for a workbook without one. Errors
    // name the table's part, rather than the worksheet that needed it, and
    // every read that needs a table that cannot be loaded gets the same error.
    fn shared_strings(&self) -> Result<Option<&SharedStrings>> {
        self.load_shared_strings()
            .transpose()
            .map_err(Error::duplicate)
    }

    // The table if it loads. Edits only compare against it, so a table that
    // cannot be loaded makes their values changes, not errors.
    fn loaded_shared_strings(&self) -> Option<&SharedStrings> {
        self.load_shared_strings()?.ok()
    }

    // Decompresses and parses the table on first use and keeps the outcome,
    // so a damaged table is not read again for every call. Concurrent first
    // uses wait for one load, which never touches this cache.
    fn load_shared_strings(&self) -> Option<std::result::Result<&SharedStrings, &Error>> {
        let path = self.shared_strings_path.as_deref()?;
        let loaded = self.shared_strings.get_or_init(|| {
            self.archive
                .read(path)
                .and_then(|xml| read_shared_strings(&xml))
                .map_err(|error| in_part(path, error))
        });
        Some(loaded.as_ref())
    }

    /// Serialize the current workbook, replacing changed worksheet ZIP records.
    /// Untouched local records and pre-existing ZIP gaps are retained. An archive
    /// with contiguous local records stays contiguous; edited records do not
    /// accumulate as obsolete copies. Central-directory offsets may change.
    /// Without edits this returns the original bytes. Pending edits are retained.
    /// Changed worksheets keep their compression method; a deflated one larger
    /// than 2 MiB is compressed in 1 MiB segments on up to eight threads,
    /// including the calling thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when an edited worksheet cannot be
    /// written, for example because the output would exceed a ZIP32 limit, or
    /// because its new offsets, sizes, or checksums, or records moved to a new
    /// distance from the end of the file, would form another end-of-directory
    /// record or a ZIP64 locator that ZIP readers could select, making the
    /// archive ambiguous.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.archive.write(&self.changed_parts())
    }

    /// Serialize into a writer without allocating a second complete archive.
    /// Changed worksheet records replace their original records in the local
    /// record sequence. Untouched local records and pre-existing ZIP gaps remain;
    /// central-directory offsets may change. The writer is flushed before this
    /// returns, so buffered writers passed by value still report write errors.
    /// An error after output has started, such as a failing writer, may leave a
    /// partial archive; use `save` for atomic files. Changed worksheets are
    /// compressed as described for [`Self::to_bytes`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when writing or flushing fails, and otherwise
    /// fails like [`Self::to_bytes`].
    pub fn write_to(&self, mut writer: impl Write) -> Result<()> {
        self.archive.write_to(&self.changed_parts(), &mut writer)
    }

    /// Remove pre-existing obsolete ZIP records while retaining untouched active
    /// local headers, compressed bytes, and descriptors. Central-directory
    /// offsets may change. Opaque ZIP prefixes or gaps cause an error. The writer
    /// is flushed before this returns. An error after output has started, such
    /// as a failing writer, may leave a partial archive; use `save_compact` for
    /// files. Changed worksheets are compressed as described for
    /// [`Self::to_bytes`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when writing or flushing fails, and
    /// [`Error::Unsupported`] when the archive has an opaque prefix, gaps, or
    /// obsolete records that compaction cannot recognize or verify, which it
    /// would discard. Otherwise fails like [`Self::to_bytes`].
    pub fn write_compact_to(&self, mut writer: impl Write) -> Result<()> {
        self.archive.compact_to(&self.changed_parts(), &mut writer)
    }

    /// Serialize a compact archive, removing obsolete ZIP records.
    /// Opaque ZIP prefixes or gaps cause an error. Pending edits are retained.
    /// Changed worksheets are compressed as described for [`Self::to_bytes`].
    ///
    /// # Errors
    ///
    /// Fails like [`Self::write_compact_to`], other than with [`Error::Io`].
    pub fn to_bytes_compact(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.write_compact_to(&mut bytes)?;
        Ok(bytes)
    }

    /// Save through a temporary file in the destination directory, then rename.
    /// The original destination is retained if generation or writing fails.
    /// Using the input path as destination is supported. The temporary file is
    /// hidden and named after the destination: `.{name}.sheetpatch-{pid}-{n}.tmp`,
    /// where `{name}` is at most 32 bytes of the destination's file name, up to
    /// any colon. If the process is killed while saving, the destination is
    /// untouched, but this incomplete copy can remain beside it and is safe to
    /// delete.
    /// Saving does not clear pending edits or replace the retained original.
    /// Changed worksheets are compressed as described for [`Self::to_bytes`].
    ///
    /// The destination must name a file. An existing destination must resolve
    /// to a regular file: directories, FIFOs, devices, and sockets are rejected
    /// before output is generated. Both cases return an [`Error::Io`] of kind
    /// [`std::io::ErrorKind::InvalidInput`]. A symlink to a regular file is
    /// replaced by the new file rather than written through.
    /// An existing destination keeps its permission bits; on Unix it also keeps
    /// its group when the process may assign it. Otherwise group bits are cleared,
    /// and other permission bits are limited to those shared by the original
    /// group and others.
    /// On Unix, a new destination for a workbook read by [`Self::open`] is
    /// created using the source file's permission bits as its creation mode,
    /// subject to the operating system's umask or default ACL rules,
    /// and the same group rule: it gets the source file's group when the
    /// process may assign it, and otherwise its group bits are cleared.
    /// Owner, ACLs, and extended attributes are not copied. On Linux, the group
    /// permission bits of a file with an access ACL hold the ACL's mask. A new
    /// or replacement file that inherits an access ACL from the destination
    /// directory's default ACL gets that mask as its own; any other file gets
    /// the permissions of the owning group's own entry, as limited by the mask.
    /// Other platforms use the file's mode bits as they are. A directory's
    /// default ACL can grant additional users access to new and replacement
    /// files, even when their permission bits match the original; on Linux it
    /// takes precedence over the umask. Setuid, setgid and sticky bits are
    /// never applied; writing the contents would clear setuid and setgid
    /// anyway.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] of kind [`std::io::ErrorKind::InvalidInput`] for
    /// the destinations described above, and [`Error::Io`] when creating,
    /// writing, syncing, or renaming the temporary file fails. Otherwise fails
    /// like [`Self::to_bytes`].
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        crate::atomic::write(path.as_ref(), self.source_permissions, |writer| {
            self.write_to(writer)
        })
    }

    /// Atomically save while removing recognizable obsolete ZIP records already
    /// present in the input. Opaque ZIP prefixes or gaps cause an error.
    /// Saving does not clear pending edits or replace the retained original.
    /// Destinations, permissions, and compression are handled as described for
    /// [`Self::save`].
    ///
    /// # Errors
    ///
    /// Fails like [`Self::save`] and [`Self::write_compact_to`].
    pub fn save_compact(&self, path: impl AsRef<Path>) -> Result<()> {
        crate::atomic::write(path.as_ref(), self.source_permissions, |writer| {
            self.write_compact_to(writer)
        })
    }
}

// The edits inside any table header, once each and in edit order, so that
// overlapping headers keep the read and refusal order of ordinary worksheet
// batches. A package can relate many tables with overlapping headers, and the
// work must not repeat for each table. The rows spanned by headers are
// coalesced, so each edit is visited at most once, and the edits in those rows
// are then tested against every header in one row sweep.
fn header_cells<'a>(
    cells: &'a BTreeMap<CellRef, CellValue>,
    headers: &[CellRange],
) -> Vec<(CellRef, &'a CellValue)> {
    let mut spans: Vec<(u32, u32)> = headers
        .iter()
        .map(|header| (header.first().row, header.last().row))
        .collect();
    spans.sort_unstable();
    let mut spans = spans.into_iter();
    let Some(mut span) = spans.next() else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    let mut search = |(first, last): (u32, u32)| {
        let first = CellRef {
            row: first,
            column: 1,
        };
        let last = CellRef {
            row: last,
            column: CellRef::MAX_COLUMN,
        };
        candidates.extend(
            cells
                .range(first..=last)
                .map(|(&cell, value)| (cell, value)),
        );
    };
    for (first, last) in spans {
        // Spans that overlap or touch form one run of rows.
        if first > span.1 + 1 {
            search(span);
            span = (first, last);
        } else {
            span.1 = span.1.max(last);
        }
    }
    search(span);
    // With either collection small, direct membership checks cost less than
    // sorting events and allocating a column index.
    if candidates.len() <= 8
        || headers.len() <= 8
        || candidates.len().saturating_mul(headers.len()) <= 256
    {
        candidates.retain(|&(cell, _)| headers.iter().any(|header| header.contains(cell)));
        return candidates;
    }
    // Each header counts once in its columns, from its first row to its last.
    let mut events: Vec<(u32, u16, u16, i32)> = Vec::with_capacity(headers.len() * 2);
    for header in headers {
        let (first, last) = (header.first(), header.last());
        events.push((first.row, first.column, last.column, 1));
        events.push((last.row + 1, first.column, last.column, -1));
    }
    events.sort_unstable_by_key(|&(row, ..)| row);
    // Fenwick indexes are one-based columns. A header also updates the slot
    // after its last column, which is one past the grid's final column.
    let mut active = vec![0i32; usize::from(CellRef::MAX_COLUMN) + 2];
    let mut position = 0;
    // Candidates are visited in edit order. The events of every row up to a
    // candidate's own are applied before its column is counted.
    candidates.retain(|&(cell, _)| {
        while position < events.len() && events[position].0 <= cell.row {
            let (_, first_column, last_column, delta) = events[position];
            for (column, delta) in [
                (usize::from(first_column), delta),
                (usize::from(last_column) + 1, -delta),
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
        let mut index = usize::from(cell.column);
        while index > 0 {
            count += active[index];
            index &= index - 1;
        }
        count > 0
    });
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common;

    const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

    // A workbook whose Data worksheet's A1 and A2 are shared strings 0 and 1,
    // with `strings` as the body of its shared-string table.
    fn shared_string_book(strings: &str) -> Workbook {
        let parts = [
            (
                "[Content_Types].xml",
                "<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='rels' ContentType='application/vnd.openxmlformats-package.relationships+xml'/><Default Extension='xml' ContentType='application/xml'/></Types>".to_owned(),
            ),
            (
                "_rels/.rels",
                format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='office' Type='{OFFICE_NS}/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
            ),
            (
                "xl/workbook.xml",
                format!("<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheets><sheet name='Data' sheetId='1' r:id='data'/></sheets></workbook>"),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='data' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet1.xml'/><Relationship Id='strings' Type='{OFFICE_NS}/sharedStrings' Target='sharedStrings.xml'/></Relationships>"),
            ),
            (
                "xl/worksheets/sheet1.xml",
                format!("<worksheet xmlns='{MAIN_NS}'><sheetData><row r='1'><c r='A1' t='s'><v>0</v></c></row><row r='2'><c r='A2' t='s'><v>1</v></c></row></sheetData></worksheet>"),
            ),
            (
                "xl/sharedStrings.xml",
                format!("<sst xmlns='{MAIN_NS}'>{strings}</sst>"),
            ),
        ];
        let entries: Vec<(&str, &[u8])> = parts
            .iter()
            .map(|(name, xml)| (*name, xml.as_bytes()))
            .collect();
        Workbook::from_bytes(common::stored_archive(&entries)).unwrap()
    }

    #[test]
    fn the_shared_string_table_is_loaded_once_whatever_the_outcome() {
        // A table that cannot be loaded keeps its first error, and each read
        // that needs the table reports a copy with its variant and message.
        let mut book = shared_string_book("<si><t>first</t></si><x><si><t>hidden</t></si></x>");
        assert!(book.shared_strings.get().is_none());
        let first = book.get_cell("Data", "A1").unwrap_err();
        assert!(matches!(book.shared_strings.get(), Some(Err(_))));
        assert_eq!(
            first.to_string(),
            "unsupported: part \"xl/sharedStrings.xml\": shared string outside the shared-string table"
        );
        let view = book.prepare_sheet("Data").unwrap();
        for error in [
            book.get_cell("Data", "A2").unwrap_err(),
            book.get_cells("Data", ["A1", "A2"]).unwrap_err(),
            view.get_cell("A1").unwrap_err(),
        ] {
            assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
            assert_eq!(error.to_string(), first.to_string());
        }
        drop(view);
        // Edits only compare against the table, so they are changes.
        book.set_cell("Data", "A1", "first").unwrap();
        book.set_cells("Data", [("A2", "second")]).unwrap();
        assert!(book.has_changes());
        assert!(matches!(book.shared_strings.get(), Some(Err(_))));

        // A table with an entry that cannot be decoded loads, and is kept.
        let book = shared_string_book("<si><t>first</t></si><si><t>_xD83D_</t></si>");
        assert!(book.get_cell("Data", "A2").is_err());
        assert!(matches!(book.shared_strings.get(), Some(Ok(_))));
        assert_eq!(book.get_cell("Data", "A1").unwrap().value, "first".into());
    }

    // The search before header rows were coalesced: every header's rows, one
    // header at a time.
    fn reference_header_cells<'a>(
        cells: &'a BTreeMap<CellRef, CellValue>,
        headers: &[CellRange],
    ) -> Vec<(CellRef, &'a CellValue)> {
        let mut header_cells = BTreeMap::new();
        for header in headers {
            header_cells.extend(
                cells
                    .range(header.first()..=header.last())
                    .filter(|(cell, _)| header.contains(**cell)),
            );
        }
        header_cells
            .into_iter()
            .map(|(&cell, value)| (cell, value))
            .collect()
    }

    #[test]
    fn header_cells_match_a_search_of_each_header() {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        for round in 0..3_000 {
            // Mostly small grids, so headers overlap and edits fall inside
            // them, with occasional cells on the grid's last rows and columns.
            let rows = 1 + next(if round % 3 == 0 { 40 } else { 8 }) as u32;
            let columns = 1 + next(if round % 5 == 0 { 40 } else { 10 }) as u16;
            let (header_count, edit_count) = (next(30), next(60));
            let mut cell = || CellRef {
                row: if next(20) == 0 {
                    CellRef::MAX_ROW - next(2) as u32
                } else {
                    1 + next(rows.into()) as u32
                },
                column: if next(20) == 0 {
                    CellRef::MAX_COLUMN - next(2) as u16
                } else {
                    1 + next(columns.into()) as u16
                },
            };
            // Excel's headers span one row; crafted ones can span several.
            let headers: Vec<CellRange> = (0..header_count)
                .map(|index| {
                    let first = cell();
                    let mut last = cell();
                    if index % 2 == 0 {
                        last.row = first.row;
                    }
                    CellRange::new(first, last)
                })
                .collect();
            let cells: BTreeMap<CellRef, CellValue> = (0..edit_count)
                .map(|value| (cell(), CellValue::Number(value as f64)))
                .collect();
            assert_eq!(
                header_cells(&cells, &headers),
                reference_header_cells(&cells, &headers),
                "round {round}"
            );
        }
    }
}
