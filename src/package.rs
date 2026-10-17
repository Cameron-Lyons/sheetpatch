use crate::{
    CellContent, CellEdit, CellRange, CellRef, CellValue, Error, Result,
    archive::{self, Archive, PartChanges},
    atomic::SourcePermissions,
    worksheet::{
        PreparedWorksheet, ReadError, patch_cells, protection, read_cells, read_shared_strings,
    },
};
use std::{borrow::Cow, collections::BTreeMap, fmt, fs, io::Write, path::Path, sync::OnceLock};

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
            error => prefixed(error, format_args!("worksheet {:?}", self.name)),
        }
    }

    // Like `context`, but a failure to load the shared-string table already
    // names that part and is not the worksheet's.
    fn read_context(&self, error: ReadError) -> Error {
        match error {
            ReadError::Worksheet(error) => self.context(error),
            ReadError::SharedStrings(error) => error,
        }
    }
}

// Names the package part an error concerns, such as the shared-string table
// or a table part read while serving a worksheet operation, retaining the
// error's variant. Such errors are never attributed to the worksheet.
fn in_part(path: &str, error: Error) -> Error {
    prefixed(error, format_args!("part {path:?}"))
}

fn prefixed(error: Error, prefix: fmt::Arguments<'_>) -> Error {
    match error {
        Error::Xml(message) => Error::Xml(format!("{prefix}: {message}")),
        Error::InvalidWorkbook(message) => Error::InvalidWorkbook(format!("{prefix}: {message}")),
        Error::Unsupported(message) => Error::Unsupported(format!("{prefix}: {message}")),
        error => error,
    }
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
/// - Changing a cell in a row whose `spans` hint, or in a worksheet whose
///   `dimension` reference, is malformed, since the edit may need to widen it.
/// - Replacing a value whose payload holds markup the crate does not
///   recognize, which the replacement would discard.
///
/// Values are validated before any worksheet is read: non-finite numbers,
/// text longer than Excel's 32,767 UTF-16 code units, and error values that
/// are not Excel error codes return [`Error::InvalidValue`].
///
/// # Errors while reading
///
/// Worksheets, the shared-string table, and the parts that define a
/// worksheet's tables are parsed on first use, so a damaged or unsupported
/// part is reported by the first read or edit that needs it, as
/// [`Error::Xml`], [`Error::InvalidWorkbook`], or [`Error::Unsupported`].
/// Messages about a worksheet name it, and messages about another part, such
/// as a shared-string table that cannot be loaded, name that part's path in
/// the archive instead.
pub struct Workbook {
    archive: Archive,
    sheets: Vec<Sheet>,
    sheet_xml: Vec<SheetXml>,
    changed_sheets: BTreeMap<String, usize>,
    signed: bool,
    date1904: bool,
    sheet_indexes: BTreeMap<String, usize>,
    shared_strings_path: Option<String>,
    shared_strings: OnceLock<Vec<String>>,
    // Permissions and group of the file this workbook was opened from, used
    // for new save destinations so a copy is not more permissive.
    source_permissions: Option<SourcePermissions>,
}

// The archive and XML are omitted: they can be many megabytes.
impl fmt::Debug for Workbook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workbook")
            .field("sheets", &self.sheets)
            .field("signed", &self.signed)
            .field("date1904", &self.date1904)
            .field("changed_parts", &self.changed_sheets.keys())
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct SheetXml {
    // The part URI can differ from its ZIP name in legacy decoded archives.
    uri: String,
    original: OnceLock<Vec<u8>>,
    // Present only when the XML differs from the original. Transactions update
    // this value and the workbook's changed-path index together at commit.
    current: Option<Vec<u8>>,
    // Header rows of the worksheet's Excel tables, resolved on its first edit.
    table_headers: OnceLock<Vec<CellRange>>,
}

struct ChangedParts<'a> {
    sheets: &'a BTreeMap<String, usize>,
    sheet_xml: &'a [SheetXml],
}

impl PartChanges for ChangedParts<'_> {
    fn parts(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.sheets.iter().map(|(path, &index)| {
            (
                path.as_str(),
                self.sheet_xml[index]
                    .current
                    .as_deref()
                    .expect("changed worksheet has pending XML"),
            )
        })
    }

    fn is_empty(&self) -> bool {
        self.sheets.is_empty()
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
    /// ```
    /// # let bytes = include_bytes!("../tests/fixtures/libreoffice.xlsx").to_vec();
    /// let book = sheetpatch::Workbook::from_bytes(bytes)?;
    /// let range = book.prepare_sheet("Data")?.used_range().unwrap();
    /// assert_eq!(range.to_string(), "A1:E1");
    /// let values = book.get_cells_at("Data", range.cells())?;
    /// assert_eq!(values.len(), 5);
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
        Ok(self.read_cells(&[cell])?.remove(0))
    }

    /// Read multiple addresses, retaining input order and duplicate requests.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCellReference`] for the first invalid address,
    /// before reading any cell, and otherwise fails like [`Self::get_cell`]
    /// for the first cell that cannot be read.
    pub fn get_cells<I, S>(&self, addresses: I) -> Result<Vec<CellContent>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.read_cells(&parse_addresses(addresses)?)
    }

    /// Read typed addresses, retaining input order and duplicate requests.
    /// A [`CellRange`] reads its cells in row-major order.
    ///
    /// # Errors
    ///
    /// Fails like [`Self::get_cell`] for the first cell that cannot be read.
    pub fn get_cells_at(
        &self,
        cells: impl IntoIterator<Item = CellRef>,
    ) -> Result<Vec<CellContent>> {
        self.read_cells(&cells.into_iter().collect::<Vec<_>>())
    }

    fn read_cells(&self, cells: &[CellRef]) -> Result<Vec<CellContent>> {
        self.worksheet
            .read_cells(
                cells,
                || self.workbook.shared_strings(),
                self.workbook.date1904,
            )
            .map_err(|error| self.sheet.read_context(error))
    }
}

fn parse_addresses<S: AsRef<str>>(addresses: impl IntoIterator<Item = S>) -> Result<Vec<CellRef>> {
    addresses
        .into_iter()
        .map(|address| address.as_ref().parse())
        .collect()
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
        let mut bytes = Vec::new();
        if metadata.is_file() {
            if metadata.len() > archive::MAX_ARCHIVE_LEN {
                return Err(archive::archive_too_large());
            }
            // Fall back to an unsized read on targets where the length does
            // not fit in memory; reading then reports the allocation failure.
            if let Ok(len) = usize::try_from(metadata.len()) {
                bytes
                    .try_reserve_exact(len)
                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
            }
        }
        // Devices and pipes have no useful length; reading stops at the limit.
        let mut workbook = Self::from_bytes(archive::read_archive(file, bytes)?)?;
        // Pipes and devices carry no meaningful file permissions to inherit.
        workbook.source_permissions = metadata
            .is_file()
            .then(|| SourcePermissions::new(&metadata));
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
        Self::from_bytes(archive::read_archive(reader, Vec::new())?)
    }

    /// Open an in-memory workbook, retaining its original archive bytes.
    /// Worksheet XML and shared strings are parsed lazily when first accessed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] for packages outside the supported scope,
    /// such as legacy `.xls` and password-encrypted (OLE compound) files,
    /// binary `.xlsb` workbooks, ZIP64 or multi-disk archives, and encrypted
    /// ZIP entries. Returns [`Error::InvalidWorkbook`] or [`Error::Xml`] for a
    /// damaged or ambiguous archive or package metadata. Worksheet XML is
    /// checked when a worksheet is first read or edited.
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
            changed_sheets: BTreeMap::new(),
            signed,
            date1904,
            sheet_indexes,
            shared_strings_path,
            shared_strings: OnceLock::new(),
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
        self.set_cells_in_sheet(index, [address.parse().map(|cell| (cell, value))])
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
        self.set_cells_in_sheet(index, [Ok((cell, value))])
    }

    /// Apply edits to a worksheet in one parse and one XML patch.
    /// Duplicate addresses use the last value; every input is validated.
    /// The entire batch is committed only after every edit succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SheetNotFound`] before reading the batch if no
    /// worksheet has this name, then the first invalid address or value, and
    /// otherwise fails like [`Self::set_cell`] for the first edit refused.
    /// No edit of a failed batch is applied.
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
    /// Fails like [`Self::set_cells`], other than for addresses.
    pub fn set_cells_at<I, V>(&mut self, sheet: &str, cells: I) -> Result<()>
    where
        I: IntoIterator<Item = (CellRef, V)>,
        V: Into<CellValue>,
    {
        let index = self.sheet_index(sheet)?;
        self.set_cells_in_sheet(index, cells.into_iter().map(Ok))
    }

    fn set_cells_in_sheet<I, V>(&mut self, index: usize, cells: I) -> Result<()>
    where
        I: IntoIterator<Item = Result<(CellRef, V)>>,
        V: Into<CellValue>,
    {
        let mut values = BTreeMap::new();
        for cell in cells {
            let (cell, value) = cell?;
            let value = value.into();
            value.validate()?;
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
    /// worksheet, and otherwise fails like [`Self::set_cell`] for the first
    /// edit refused. No edit of a failed transaction is applied.
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
            // Shared strings only recognize rewriting a cell's current text,
            // so a table that cannot be loaded leaves such an edit a change.
            let shared_strings = || self.shared_strings().ok().flatten();
            let Cow::Owned(patched) =
                patch_cells(current, &cells, shared_strings, self.date1904).map_err(in_sheet)?
            else {
                continue;
            };
            self.check_table_headers(index, current, &cells)?;
            let changed = (patched != original).then_some(patched);
            staged.push((index, changed));
        }
        for (index, changed) in staged {
            if changed.is_some() {
                self.changed_sheets
                    .insert(self.sheets[index].path.clone(), index);
            } else {
                self.changed_sheets.remove(&self.sheets[index].path);
            }
            self.sheet_xml[index].current = changed;
        }
        Ok(())
    }

    // Excel requires each table header cell's text to equal its table column's
    // name, and repairs or removes a table whose header differs. An edit may
    // rewrite a header cell's current value, as it reads, but not change it.
    // Errors name the worksheet or the package part they concern.
    fn check_table_headers(
        &self,
        index: usize,
        xml: &[u8],
        cells: &BTreeMap<CellRef, CellValue>,
    ) -> Result<()> {
        let sheet = &self.sheets[index];
        let headers = self.table_headers(index)?;
        let (header_cells, values): (Vec<CellRef>, Vec<&CellValue>) = cells
            .iter()
            .filter(|(cell, _)| headers.iter().any(|header| header.contains(**cell)))
            .unzip();
        if header_cells.is_empty() {
            return Ok(());
        }
        let current = read_cells(xml, &header_cells, || self.shared_strings(), self.date1904)
            .map_err(|error| sheet.read_context(error))?;
        for ((&cell, value), current) in header_cells.iter().zip(values).zip(current) {
            if current.value != *value {
                return Err(Error::ProtectedCell {
                    sheet: sheet.name.clone(),
                    cell,
                    reason: protection::TABLE_HEADER.into(),
                });
            }
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
    /// first cell that cannot be read.
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
    /// row-major order.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SheetNotFound`] if no worksheet has this name, and
    /// otherwise fails like [`Self::get_cell`] for the first cell that cannot
    /// be read.
    pub fn get_cells_at(
        &self,
        sheet: &str,
        cells: impl IntoIterator<Item = CellRef>,
    ) -> Result<Vec<CellContent>> {
        let index = self.sheet_index(sheet)?;
        let cells: Vec<CellRef> = cells.into_iter().collect();
        self.read_cells_in_sheet(index, &cells)
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
        .map_err(|error| sheet.read_context(error))
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
        !self.changed_sheets.is_empty()
    }

    /// Discard every pending edit, restoring byte-identical original output.
    pub fn reset_changes(&mut self) {
        for &index in self.changed_sheets.values() {
            self.sheet_xml[index].current = None;
        }
        self.changed_sheets.clear();
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
            sheets: &self.changed_sheets,
            sheet_xml: &self.sheet_xml,
        }
    }

    fn sheet_index(&self, name: &str) -> Result<usize> {
        self.sheet_indexes
            .get(name)
            .copied()
            .ok_or_else(|| Error::SheetNotFound(name.to_owned()))
    }

    // Each cache is loaded outside `get_or_init`, so a failed load is not
    // cached. Concurrent first loads can only duplicate the work.
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
        let headers = discovery::table_headers(&self.archive, &self.sheet_xml[index].uri)?;
        Ok(cache.get_or_init(|| headers))
    }

    // The shared-string table, or `None` for a workbook without one. Errors
    // name the table's part, rather than the worksheet that needed it.
    fn shared_strings(&self) -> Result<Option<&[String]>> {
        let Some(path) = &self.shared_strings_path else {
            return Ok(None);
        };
        if let Some(strings) = self.shared_strings.get() {
            return Ok(Some(strings));
        }
        let strings = self
            .archive
            .read(path)
            .and_then(|xml| read_shared_strings(&xml))
            .map_err(|error| in_part(path, error))?;
        Ok(Some(self.shared_strings.get_or_init(|| strings)))
    }

    /// Serialize the current workbook, replacing changed worksheet ZIP records.
    /// Untouched local records and pre-existing ZIP gaps are retained. An archive
    /// with contiguous local records stays contiguous; edited records do not
    /// accumulate as obsolete copies. Central-directory offsets may change.
    /// Without edits this returns the original bytes. Pending edits are retained.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when an edited worksheet cannot be
    /// written, for example because the output would exceed a ZIP32 limit or
    /// its new central-directory offset would form an end-of-directory
    /// signature exposed by the archive comment, making the archive ambiguous.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.archive.write(&self.changed_parts())
    }

    /// Serialize into a writer without allocating a second complete archive.
    /// Changed worksheet records replace their original records in the local
    /// record sequence. Untouched local records and pre-existing ZIP gaps remain;
    /// central-directory offsets may change. The writer is flushed before this
    /// returns, so buffered writers passed by value still report write errors.
    /// An error after output has started, such as a failing writer, may leave a
    /// partial archive; use `save` for atomic files.
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
    /// files.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when writing or flushing fails, and
    /// [`Error::Unsupported`] when the archive has an opaque prefix, gaps, or
    /// unrecognized obsolete records, which compaction would discard.
    /// Otherwise fails like [`Self::to_bytes`].
    pub fn write_compact_to(&self, mut writer: impl Write) -> Result<()> {
        self.archive.compact_to(&self.changed_parts(), &mut writer)
    }

    /// Serialize a compact archive, removing obsolete ZIP records.
    /// Opaque ZIP prefixes or gaps cause an error. Pending edits are retained.
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
    /// Using the input path as destination is supported.
    /// Saving does not clear pending edits or replace the retained original.
    ///
    /// The destination must name a file. An existing destination must resolve
    /// to a regular file: directories, FIFOs, devices, and sockets are rejected
    /// before output is generated. Both cases return an [`Error::Io`] of kind
    /// [`std::io::ErrorKind::InvalidInput`]. A symlink to a regular file is
    /// replaced by the new file rather than written through.
    /// An existing destination keeps its permissions; on Unix it also keeps its
    /// group when the process may assign it. Otherwise group access is removed,
    /// and other access is limited to permissions shared by the original group
    /// and others, so members of the original group cannot gain access.
    /// On Unix, a new destination for a workbook read by [`Self::open`] is
    /// created with the source file's permissions, restricted by the umask,
    /// and the same group rule: it gets the source file's group when the
    /// process may assign it, and otherwise no group access. Setuid, setgid
    /// and sticky bits are never applied; writing the contents would clear
    /// setuid and setgid anyway.
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
    /// Destinations and permissions are handled as described for [`Self::save`].
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
