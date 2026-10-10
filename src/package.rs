use crate::{
    CellContent, CellEdit, CellRef, CellValue, Error, Result,
    archive::{Archive, PartChanges},
    worksheet::{PreparedWorksheet, patch_cells, read_cells, read_shared_strings},
};
use std::{borrow::Cow, collections::BTreeMap, fs, io::Write, path::Path, sync::OnceLock};

mod discovery;

/// A worksheet discovered through the package's relationships.
#[derive(Clone, Debug)]
pub struct Sheet {
    name: String,
    path: String,
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
    states: Vec<SheetState>,
    changed_sheets: BTreeMap<String, usize>,
    signed: bool,
    sheet_indexes: BTreeMap<String, usize>,
    shared_strings_path: Option<String>,
    shared_strings: OnceLock<Vec<String>>,
}

#[derive(Default)]
struct SheetState {
    original: OnceLock<Vec<u8>>,
    // Present only when the XML differs from the original. Transactions update
    // this value and the workbook's changed-path index together at commit.
    current: Option<Vec<u8>>,
}

struct ChangedParts<'a> {
    sheets: &'a BTreeMap<String, usize>,
    states: &'a [SheetState],
}

impl PartChanges for ChangedParts<'_> {
    fn parts(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.sheets.iter().map(|(path, &index)| {
            (
                path.as_str(),
                self.states[index]
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

impl WorksheetView<'_> {
    /// The worksheet's name and resolved package path.
    pub fn sheet(&self) -> &Sheet {
        self.sheet
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
        let cells = addresses
            .into_iter()
            .map(|address| address.as_ref().parse())
            .collect::<Result<Vec<CellRef>>>()?;
        self.read_cells(&cells)
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
            .read_cells(cells, || self.workbook.shared_strings())
    }
}

impl Workbook {
    /// Read a workbook file and discover worksheets through its relationships.
    /// Worksheet XML and shared strings are parsed lazily when first accessed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_bytes(fs::read(path)?)
    }

    /// Open an in-memory workbook, retaining its original archive bytes.
    /// Worksheet XML and shared strings are parsed lazily when first accessed.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let archive = Archive::new(bytes)?;
        let discovery::PackageMetadata {
            sheets,
            signed,
            shared_strings_path,
        } = discovery::discover(&archive)?;
        let states = (0..sheets.len()).map(|_| SheetState::default()).collect();
        let sheet_indexes = sheets
            .iter()
            .enumerate()
            .map(|(index, sheet)| (sheet.name.clone(), index))
            .collect();
        Ok(Self {
            archive,
            sheets,
            states,
            changed_sheets: BTreeMap::new(),
            signed,
            sheet_indexes,
            shared_strings_path,
            shared_strings: OnceLock::new(),
        })
    }

    /// The editable worksheets in workbook tab order.
    /// Chart, dialog, and macro sheets are omitted. Names match exactly,
    /// including case, in all read and edit methods.
    pub fn sheets(&self) -> &[Sheet] {
        &self.sheets
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
        self.set_cell_at(sheet, address.parse()?, value)
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
        let value = value.into();
        value.validate()?;
        let index = self.sheet_index(sheet)?;
        self.apply_grouped_edits([(index, BTreeMap::from([(cell, value)]))])
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
            let original = self.original_sheet(index)?;
            let current = self.states[index].current.as_deref().unwrap_or(original);
            let Cow::Owned(patched) = patch_cells(current, &cells)? else {
                continue;
            };
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
            self.states[index].current = changed;
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
        let cells = addresses
            .into_iter()
            .map(|address| address.as_ref().parse())
            .collect::<Result<Vec<CellRef>>>()?;
        self.read_cells_in_sheet(index, &cells)
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
        read_cells(self.current_sheet(index)?, cells, || self.shared_strings())
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
        Ok(WorksheetView {
            sheet: &self.sheets[index],
            workbook: self,
            worksheet: PreparedWorksheet::parse(self.current_sheet(index)?)?,
        })
    }

    /// Whether any package parts have pending edits.
    pub fn has_changes(&self) -> bool {
        !self.changed_sheets.is_empty()
    }

    /// Discard every pending edit, restoring byte-identical original output.
    pub fn reset_changes(&mut self) {
        for &index in self.changed_sheets.values() {
            self.states[index].current = None;
        }
        self.changed_sheets.clear();
    }

    fn current_sheet(&self, index: usize) -> Result<&[u8]> {
        self.states[index]
            .current
            .as_deref()
            .map(Ok)
            .unwrap_or_else(|| self.original_sheet(index))
    }

    fn changed_parts(&self) -> ChangedParts<'_> {
        ChangedParts {
            sheets: &self.changed_sheets,
            states: &self.states,
        }
    }

    fn sheet_index(&self, name: &str) -> Result<usize> {
        self.sheet_indexes
            .get(name)
            .copied()
            .ok_or_else(|| Error::SheetNotFound(name.to_owned()))
    }

    fn original_sheet(&self, index: usize) -> Result<&[u8]> {
        let cache = &self.states[index].original;
        if cache.get().is_none() {
            let xml = self.archive.read(&self.sheets[index].path)?;
            let _ = cache.set(xml);
        }
        Ok(cache.get().expect("worksheet cache initialized").as_slice())
    }

    fn shared_strings(&self) -> Result<&[String]> {
        if self.shared_strings.get().is_none() {
            let path = self
                .shared_strings_path
                .as_ref()
                .ok_or(Error::SharedStringsUnavailable)?;
            let strings = read_shared_strings(&self.archive.read(path)?)?;
            let _ = self.shared_strings.set(strings);
        }
        Ok(self
            .shared_strings
            .get()
            .expect("shared-string cache initialized")
            .as_slice())
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
    /// central-directory offsets may change.
    /// A failing writer may contain a partial archive; use `save` for atomic files.
    pub fn write_to(&self, mut writer: impl Write) -> Result<()> {
        self.archive.write_to(&self.changed_parts(), &mut writer)
    }

    /// Remove pre-existing obsolete ZIP records while retaining untouched active
    /// local headers, compressed bytes, and descriptors. Central-directory
    /// offsets may change. Opaque ZIP prefixes or gaps cause an error.
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
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        crate::atomic::write(path.as_ref(), |writer| self.write_to(writer))
    }

    /// Atomically save while removing recognizable obsolete ZIP records already
    /// present in the input. Opaque ZIP prefixes or gaps cause an error.
    /// Saving does not clear pending edits or replace the retained original.
    pub fn save_compact(&self, path: impl AsRef<Path>) -> Result<()> {
        crate::atomic::write(path.as_ref(), |writer| self.write_compact_to(writer))
    }
}
