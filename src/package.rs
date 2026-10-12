use crate::{
    CellContent, CellEdit, CellRange, CellRef, CellValue, Error, Result,
    archive::{Archive, PartChanges},
    worksheet::{PreparedWorksheet, patch_cells, read_cells, read_shared_strings},
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
    // patching it, retaining the error's variant.
    fn context(&self, error: Error) -> Error {
        let name = &self.name;
        match error {
            Error::Xml(message) => Error::Xml(format!("worksheet {name:?}: {message}")),
            Error::InvalidWorkbook(message) => {
                Error::InvalidWorkbook(format!("worksheet {name:?}: {message}"))
            }
            Error::Unsupported(message) => {
                Error::Unsupported(format!("worksheet {name:?}: {message}"))
            }
            error => error,
        }
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
    // Permissions of the file this workbook was opened from, used as the
    // creation mode for new save destinations so a copy is not more permissive.
    source_permissions: Option<fs::Permissions>,
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
            .field("cells", &self.worksheet.cell_refs().len())
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
    /// let cells: Vec<_> = sheet.cell_refs().collect();
    /// let contents = sheet.get_cells_at(cells.iter().copied())?;
    /// for (cell, content) in cells.iter().zip(&contents) {
    ///     println!("{cell}: {}", content.value);
    /// }
    /// assert_eq!(cells[0].to_string(), "A1");
    /// assert_eq!(contents[3].formula.as_deref(), Some("B1*2"));
    /// # Ok::<(), sheetpatch::Error>(())
    /// ```
    pub fn cell_refs(&self) -> impl Iterator<Item = CellRef> + '_ {
        self.worksheet.cell_refs()
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
        let mut cells = self.worksheet.cell_refs();
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
    pub fn get_cell(&self, address: &str) -> Result<CellContent> {
        self.get_cell_at(address.parse()?)
    }

    /// Read a cell using an already validated, one-based address.
    pub fn get_cell_at(&self, cell: CellRef) -> Result<CellContent> {
        Ok(self.read_cells(&[cell])?.remove(0))
    }

    /// Read multiple addresses, retaining input order and duplicate requests.
    pub fn get_cells<I, S>(&self, addresses: I) -> Result<Vec<CellContent>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.read_cells(&parse_addresses(addresses)?)
    }

    /// Read typed addresses, retaining input order and duplicate requests.
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
            .map_err(|error| self.sheet.context(error))
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
    /// file's permissions become the creation mode for new destinations of
    /// [`Self::save`] and [`Self::save_compact`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        use std::io::{self, Read};

        // The central directory's offset and size are each below u32::MAX, and
        // only a 22-byte end record and a comment of at most 64 KiB follow it.
        const MAX_LEN: u64 = 2 * u32::MAX as u64 + 22 + u16::MAX as u64;
        let too_large =
            || Error::Unsupported("input exceeds the largest possible ZIP32 archive".into());
        let file = fs::File::open(path)?;
        let metadata = file.metadata()?;
        let mut bytes = Vec::new();
        if metadata.is_file() {
            if metadata.len() > MAX_LEN {
                return Err(too_large());
            }
            // Fall back to an unsized read on targets where the length does
            // not fit in memory; reading then reports the allocation failure.
            if let Ok(len) = usize::try_from(metadata.len()) {
                bytes
                    .try_reserve_exact(len)
                    .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
            }
        }
        // Devices and pipes have no useful length; stop reading at the limit.
        file.take(MAX_LEN + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_LEN {
            return Err(too_large());
        }
        let mut workbook = Self::from_bytes(bytes)?;
        // Pipes and devices carry no meaningful file permissions to inherit.
        workbook.source_permissions = metadata.is_file().then(|| metadata.permissions());
        Ok(workbook)
    }

    /// Open an in-memory workbook, retaining its original archive bytes.
    /// Worksheet XML and shared strings are parsed lazily when first accessed.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let archive = Archive::new(bytes)?;
        let discovery::PackageMetadata {
            sheets,
            signed,
            date1904,
            shared_strings_path,
        } = discovery::discover(&archive)?;
        let sheet_xml = (0..sheets.len()).map(|_| SheetXml::default()).collect();
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
    /// Formula cells and shared/array formula ranges are deliberately protected.
    /// Text and numbers are validated before applying changes. Failed edits leave
    /// this workbook unchanged.
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
            let Cow::Owned(patched) =
                patch_cells(current, &cells, || self.shared_strings()).map_err(in_sheet)?
            else {
                continue;
            };
            self.check_table_headers(index, current, &cells)
                .map_err(in_sheet)?;
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
    // rewrite a header cell's current value, but not change it.
    fn check_table_headers(
        &self,
        index: usize,
        xml: &[u8],
        cells: &BTreeMap<CellRef, CellValue>,
    ) -> Result<()> {
        let headers = self.table_headers(index)?;
        let (header_cells, values): (Vec<CellRef>, Vec<&CellValue>) = cells
            .iter()
            .filter(|(cell, _)| headers.iter().any(|header| header.contains(**cell)))
            .unzip();
        if header_cells.is_empty() {
            return Ok(());
        }
        let current = read_cells(xml, &header_cells, || self.shared_strings(), self.date1904)?;
        for ((cell, value), current) in header_cells.iter().zip(values).zip(current) {
            if current.value != *value {
                return Err(Error::Unsupported(format!(
                    "{cell} is an Excel table header cell; \
                     editing it would desynchronize the table's column names"
                )));
            }
        }
        Ok(())
    }

    /// Read a cell's current value, cached formula result, and style index.
    /// Missing cells return a blank value without modifying the workbook.
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
    pub fn get_cell_at(&self, sheet: &str, cell: CellRef) -> Result<CellContent> {
        let index = self.sheet_index(sheet)?;
        Ok(self.read_cells_in_sheet(index, &[cell])?.remove(0))
    }

    /// Read many cells with one worksheet parse, retaining input order.
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
    /// conversions are needed.
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
        let in_sheet = |error| self.sheets[index].context(error);
        read_cells(
            self.current_sheet(index).map_err(in_sheet)?,
            cells,
            || self.shared_strings(),
            self.date1904,
        )
        .map_err(in_sheet)
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

    fn table_headers(&self, index: usize) -> Result<&[CellRange]> {
        let cache = &self.sheet_xml[index].table_headers;
        if let Some(headers) = cache.get() {
            return Ok(headers);
        }
        let headers = discovery::table_headers(&self.archive, &self.sheets[index].path)?;
        Ok(cache.get_or_init(|| headers))
    }

    fn shared_strings(&self) -> Result<&[String]> {
        if let Some(strings) = self.shared_strings.get() {
            return Ok(strings);
        }
        let path = self
            .shared_strings_path
            .as_ref()
            .ok_or(Error::SharedStringsUnavailable)?;
        let strings = read_shared_strings(&self.archive.read(path)?)?;
        Ok(self.shared_strings.get_or_init(|| strings))
    }

    /// Serialize the current workbook, replacing changed worksheet ZIP records.
    /// Untouched local records and pre-existing ZIP gaps are retained. An archive
    /// with contiguous local records stays contiguous; edited records do not
    /// accumulate as obsolete copies. Central-directory offsets may change.
    /// Without edits this returns the original bytes. Pending edits are retained.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.archive.write(&self.changed_parts())
    }

    /// Serialize into a writer without allocating a second complete archive.
    /// Changed worksheet records replace their original records in the local
    /// record sequence. Untouched local records and pre-existing ZIP gaps remain;
    /// central-directory offsets may change. The writer is flushed before this
    /// returns, so buffered writers passed by value still report write errors.
    /// A failing writer may contain a partial archive; use `save` for atomic files.
    pub fn write_to(&self, mut writer: impl Write) -> Result<()> {
        self.archive.write_to(&self.changed_parts(), &mut writer)
    }

    /// Remove pre-existing obsolete ZIP records while retaining untouched active
    /// local headers, compressed bytes, and descriptors. Central-directory
    /// offsets may change. Opaque ZIP prefixes or gaps cause an error. The writer
    /// is flushed before this returns.
    /// A failing writer may contain a partial archive; use `save_compact` for files.
    pub fn write_compact_to(&self, mut writer: impl Write) -> Result<()> {
        self.archive.compact_to(&self.changed_parts(), &mut writer)
    }

    /// Serialize a compact archive, removing obsolete ZIP records.
    /// Opaque ZIP prefixes or gaps cause an error. Pending edits are retained.
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
    /// group when the process may assign it, and otherwise loses group access.
    /// On Unix, a new destination for a workbook read by [`Self::open`] is
    /// created with the source file's permissions, restricted by the umask.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        crate::atomic::write(path.as_ref(), self.source_permissions.as_ref(), |writer| {
            self.write_to(writer)
        })
    }

    /// Atomically save while removing recognizable obsolete ZIP records already
    /// present in the input. Opaque ZIP prefixes or gaps cause an error.
    /// Saving does not clear pending edits or replace the retained original.
    /// Destinations and permissions are handled as described for [`Self::save`].
    pub fn save_compact(&self, path: impl AsRef<Path>) -> Result<()> {
        crate::atomic::write(path.as_ref(), self.source_permissions.as_ref(), |writer| {
            self.write_compact_to(writer)
        })
    }
}
