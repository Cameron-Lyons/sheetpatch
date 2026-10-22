use crate::{Error, Result};
use std::borrow::Cow;
use std::fmt;
use std::iter::FusedIterator;
use std::str::FromStr;

/// An A1 address within Excel's 16,384 columns and 1,048,576 rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CellRef {
    pub(crate) row: u32,
    pub(crate) column: u16,
}

impl CellRef {
    /// The largest one-based row number in a worksheet, 1,048,576.
    pub const MAX_ROW: u32 = 1_048_576;
    /// The largest one-based column number in a worksheet, 16,384 (column `XFD`).
    pub const MAX_COLUMN: u16 = 16_384;

    /// Construct an address from one-based row and column numbers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCellReference`] unless the row is within
    /// `1..=`[`Self::MAX_ROW`] and the column is within `1..=`[`Self::MAX_COLUMN`].
    pub fn new(row: u32, column: u16) -> Result<Self> {
        if !(1..=Self::MAX_ROW).contains(&row) || !(1..=Self::MAX_COLUMN).contains(&column) {
            return Err(Error::InvalidCellReference(format!(
                "row {row}, column {column}"
            )));
        }
        Ok(Self { row, column })
    }
    /// Return the one-based row number.
    pub fn row(self) -> u32 {
        self.row
    }
    /// Return the one-based column number.
    pub fn column(self) -> u16 {
        self.column
    }
}

impl FromStr for CellRef {
    type Err = Error;

    /// Parse an A1 address such as `"B2"`; column letters may be lowercase.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCellReference`], holding the input, for `$`
    /// markers, ranges, sheet prefixes, surrounding whitespace, and addresses
    /// outside Excel's grid.
    fn from_str(address: &str) -> Result<Self> {
        let invalid = || Error::InvalidCellReference(address.to_owned());
        let bytes = address.as_bytes();
        if !(2..=10).contains(&bytes.len()) {
            return Err(invalid());
        }
        let split = bytes
            .iter()
            .position(|b| !b.is_ascii_alphabetic())
            .ok_or_else(invalid)?;
        if split == 0 || split > 3 || split == bytes.len() || bytes[split] == b'0' {
            return Err(invalid());
        }
        let mut column: u32 = 0;
        for &b in &bytes[..split] {
            column = column * 26 + u32::from(b.to_ascii_uppercase() - b'A' + 1);
        }
        if column > u32::from(Self::MAX_COLUMN) || !bytes[split..].iter().all(u8::is_ascii_digit) {
            return Err(invalid());
        }
        let row: u32 = address[split..].parse().map_err(|_| invalid())?;
        if !(1..=Self::MAX_ROW).contains(&row) {
            return Err(invalid());
        }
        Ok(Self {
            row,
            column: column as u16,
        })
    }
}

impl CellRef {
    // The longest address, `XFD1048576`.
    const MAX_LEN: usize = 10;

    // Writes the A1 address to the start of `buffer`, returning its length.
    fn encode(self, buffer: &mut [u8]) -> usize {
        let mut column = self.column;
        let mut letters = [0u8; 3];
        let mut start = letters.len();
        while column != 0 {
            start -= 1;
            column -= 1;
            letters[start] = b'A' + (column % 26) as u8;
            column /= 26;
        }
        let mut digits = [0u8; 7];
        let mut first_digit = digits.len();
        let mut row = self.row;
        while row != 0 {
            first_digit -= 1;
            digits[first_digit] = b'0' + (row % 10) as u8;
            row /= 10;
        }
        let letters = &letters[start..];
        let digits = &digits[first_digit..];
        buffer[..letters.len()].copy_from_slice(letters);
        buffer[letters.len()..letters.len() + digits.len()].copy_from_slice(digits);
        letters.len() + digits.len()
    }
}

// Formats an ASCII address built on the stack, honoring width, fill, and
// alignment like a string.
fn pad_ascii(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    f.pad(std::str::from_utf8(bytes).map_err(|_| fmt::Error)?)
}

impl fmt::Display for CellRef {
    /// Format as an A1 address, such as `"XFD1048576"`. Width, fill, and
    /// alignment are honored as for a string.
    ///
    /// ```
    /// use sheetpatch::CellRef;
    ///
    /// let cell = CellRef::new(2, 28)?;
    /// assert_eq!(cell.to_string(), "AB2");
    /// assert_eq!(format!("[{cell:>5}]"), "[  AB2]");
    /// # Ok::<(), sheetpatch::Error>(())
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buffer = [0u8; Self::MAX_LEN];
        let len = self.encode(&mut buffer);
        pad_ascii(f, &buffer[..len])
    }
}

/// A rectangular block of cells, such as `A1:C3`.
///
/// A range is stored by its top-left and bottom-right corners, so it is always
/// non-empty and within Excel's grid. [`CellRange::cells`] visits its cells in
/// row-major order, which pairs with the typed batch APIs for block reads and
/// writes.
///
/// ```
/// use sheetpatch::{CellRange, CellRef};
///
/// let range: CellRange = "c2:A1".parse()?;
/// assert_eq!(range.to_string(), "A1:C2");
/// assert_eq!((range.height(), range.width(), range.cell_count()), (2, 3, 6));
/// assert!(range.contains(CellRef::new(2, 2)?));
///
/// let cells: Vec<String> = range.cells().map(|cell| cell.to_string()).collect();
/// assert_eq!(cells, ["A1", "B1", "C1", "A2", "B2", "C2"]);
/// # Ok::<(), sheetpatch::Error>(())
/// ```
///
/// Reading and writing a block with a workbook. A range is itself an iterable
/// of its cells, in the same order as [`CellRange::cells`]:
///
/// ```no_run
/// use sheetpatch::{CellRange, Workbook};
///
/// let mut book = Workbook::open("report.xlsx")?;
/// let range: CellRange = "B2:D3".parse()?;
/// let values = [1, 2, 3, 4, 5, 6];
/// book.set_cells_at("Data", range.cells().zip(values))?;
/// let cells = book.get_cells_at("Data", range)?;
/// assert_eq!(cells.len(), 6);
/// # Ok::<(), sheetpatch::Error>(())
/// ```
///
/// Ranges are ordered by their top-left cell and then by their bottom-right
/// cell, each compared in row-major order like [`CellRef`]: row first, then
/// column. This makes ranges usable as sorted map keys; it is not a
/// containment or size order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellRange {
    first: CellRef,
    last: CellRef,
}

impl CellRange {
    /// Construct the smallest range containing both cells.
    ///
    /// The corners can be given in any order; they are normalized so that
    /// [`Self::first`] is the top-left and [`Self::last`] the bottom-right cell.
    pub fn new(a: CellRef, b: CellRef) -> Self {
        Self {
            first: CellRef {
                row: a.row.min(b.row),
                column: a.column.min(b.column),
            },
            last: CellRef {
                row: a.row.max(b.row),
                column: a.column.max(b.column),
            },
        }
    }
    /// Return the top-left cell.
    pub fn first(self) -> CellRef {
        self.first
    }
    /// Return the bottom-right cell.
    pub fn last(self) -> CellRef {
        self.last
    }
    /// Return the number of rows, at least 1.
    pub fn height(self) -> u32 {
        self.last.row - self.first.row + 1
    }
    /// Return the number of columns, at least 1.
    pub fn width(self) -> u16 {
        self.last.column - self.first.column + 1
    }
    /// Return the number of cells, `height × width`.
    ///
    /// This is a `u64` because a whole worksheet holds more than 2³² cells.
    pub fn cell_count(self) -> u64 {
        u64::from(self.height()) * u64::from(self.width())
    }
    /// Return whether the cell lies within this range.
    pub fn contains(self, cell: CellRef) -> bool {
        (self.first.row..=self.last.row).contains(&cell.row)
            && (self.first.column..=self.last.column).contains(&cell.column)
    }
    // The smallest range containing this range and the cell.
    pub(crate) fn including(self, cell: CellRef) -> Self {
        Self::new(
            Self::new(self.first, cell).first,
            Self::new(self.last, cell).last,
        )
    }
    /// Iterate over the range's cells in row-major order: every cell of the
    /// first row from left to right, then the next row.
    pub fn cells(self) -> CellRangeIter {
        CellRangeIter {
            range: self,
            next: Some(self.first),
        }
    }
}

impl From<CellRef> for CellRange {
    /// Convert a cell into a range containing only that cell.
    fn from(cell: CellRef) -> Self {
        Self {
            first: cell,
            last: cell,
        }
    }
}

impl FromStr for CellRange {
    type Err = Error;

    /// Parse `"A1:C3"` (corners in either order) or a single cell such as `"B2"`.
    ///
    /// Each corner follows [`CellRef`]'s rules: lowercase letters are accepted.
    ///
    /// # Errors
    ///
    /// `$` markers, whole rows or columns such as `"A:C"`, sheet prefixes and
    /// surrounding whitespace are rejected with
    /// [`Error::InvalidCellReference`], which holds the whole input.
    fn from_str(range: &str) -> Result<Self> {
        let invalid = |_: Error| Error::InvalidCellReference(range.to_owned());
        match range.split_once(':') {
            Some((first, last)) => Ok(Self::new(
                first.parse().map_err(invalid)?,
                last.parse().map_err(invalid)?,
            )),
            None => range.parse::<CellRef>().map(Self::from).map_err(invalid),
        }
    }
}

impl fmt::Display for CellRange {
    /// Format as `"A1:C3"`, or as a single address such as `"B2"` when the
    /// range holds one cell. Width, fill, and alignment are honored as for a
    /// string.
    ///
    /// ```
    /// use sheetpatch::CellRange;
    ///
    /// let range: CellRange = "C3:A1".parse()?;
    /// assert_eq!(format!("[{range:-^9}]"), "[--A1:C3--]");
    /// # Ok::<(), sheetpatch::Error>(())
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buffer = [0u8; 2 * CellRef::MAX_LEN + 1];
        let mut len = self.first.encode(&mut buffer);
        if self.first != self.last {
            buffer[len] = b':';
            len += 1;
            len += self.last.encode(&mut buffer[len..]);
        }
        pad_ascii(f, &buffer[..len])
    }
}

/// Iterates over the range's cells in row-major order, like
/// [`CellRange::cells`], so a range can be passed directly to the batch
/// methods that take cells.
impl IntoIterator for CellRange {
    type Item = CellRef;
    type IntoIter = CellRangeIter;

    fn into_iter(self) -> CellRangeIter {
        self.cells()
    }
}

/// An iterator over a [`CellRange`]'s cells in row-major order, returned by
/// [`CellRange::cells`].
///
/// Its `size_hint` is exact whenever the remaining count fits in `usize`. It
/// does not implement [`ExactSizeIterator`], because a whole worksheet holds
/// more cells than a 32-bit `usize` can count.
/// [`Iterator::nth`], [`Iterator::last`] and [`Iterator::count`] take constant
/// time, even for a range covering the whole worksheet. Counting panics if
/// the remaining count exceeds `usize::MAX`.
#[derive(Clone, Debug)]
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct CellRangeIter {
    range: CellRange,
    next: Option<CellRef>,
}

impl CellRangeIter {
    fn remaining(&self) -> u64 {
        self.next.map_or(0, |cell| {
            u64::from(self.range.last.row - cell.row) * u64::from(self.range.width())
                + u64::from(self.range.last.column - cell.column)
                + 1
        })
    }
}

impl Iterator for CellRangeIter {
    type Item = CellRef;

    fn next(&mut self) -> Option<CellRef> {
        let cell = self.next?;
        let CellRange { first, last } = self.range;
        self.next = if cell.column < last.column {
            Some(CellRef {
                row: cell.row,
                column: cell.column + 1,
            })
        } else if cell.row < last.row {
            Some(CellRef {
                row: cell.row + 1,
                column: first.column,
            })
        } else {
            None
        };
        Some(cell)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match usize::try_from(self.remaining()) {
            Ok(remaining) => (remaining, Some(remaining)),
            Err(_) => (usize::MAX, None),
        }
    }

    fn nth(&mut self, n: usize) -> Option<CellRef> {
        let cell = self.next?;
        let n = n as u64;
        // Check before adding the offset, so an out-of-range skip exhausts
        // the iterator without overflowing. A surviving offset fits in the grid.
        if n >= self.remaining() {
            self.next = None;
            return None;
        }
        let width = u64::from(self.range.width());
        let offset = u64::from(cell.column - self.range.first.column) + n;
        self.next = Some(CellRef {
            row: cell.row + (offset / width) as u32,
            column: self.range.first.column + (offset % width) as u16,
        });
        self.next()
    }

    fn last(self) -> Option<CellRef> {
        self.next.map(|_| self.range.last)
    }

    fn count(self) -> usize {
        usize::try_from(self.remaining()).expect("cell range count exceeds usize::MAX")
    }
}

impl FusedIterator for CellRangeIter {}

/// Cell contents. Existing number formats and other cell attributes are retained.
/// Text is stored inline, so the workbook's shared-string table stays untouched.
///
/// Editing methods accept any `impl Into<CellValue>`. Strings (`&str`,
/// `String`, `&String`, `Cow<str>`) and `char` become [`CellValue::Text`];
/// `f64` and the integer types that convert exactly (`i8`, `i16`, `i32`, `u8`,
/// `u16`, `u32`) become [`CellValue::Number`]; `bool` becomes
/// [`CellValue::Bool`]; and `Option<T>` maps `None` to [`CellValue::Blank`].
/// The 64-bit and pointer-sized integers (`i64`, `u64`, `isize`, `usize`) use
/// `TryFrom` instead, which rejects values that `f64` cannot hold exactly.
/// There is deliberately no `f32` conversion: `0.1f32` would widen to
/// `0.10000000149011612`.
///
/// Unlike most of this crate's enums, `CellValue` is exhaustive, so a `match`
/// on it needs no fallback arm. Its variants are the closed set of scalar
/// value types a SpreadsheetML cell can store (shared and inline strings and
/// formula strings all read as text, and ISO 8601 dates read as numbers), so
/// none is expected to be added. Other information that reads may report in
/// future, such as more cell metadata, belongs on [`CellContent`], which is
/// `#[non_exhaustive]`.
///
/// ```
/// use sheetpatch::CellValue;
///
/// let name = String::from("Ada");
/// assert_eq!(CellValue::from(&name), CellValue::Text("Ada".into()));
/// assert_eq!(CellValue::from(7u32), CellValue::Number(7.0));
/// assert_eq!(CellValue::from(None::<f64>), CellValue::Blank);
/// assert_eq!(CellValue::try_from(42usize)?, CellValue::Number(42.0));
/// assert!(CellValue::try_from(9_007_199_254_740_993i64).is_err());
/// # Ok::<(), sheetpatch::Error>(())
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub enum CellValue {
    /// Text, written as an inline string without creating a formula.
    Text(String),
    /// A finite numeric value. Existing number formats determine its display.
    Number(f64),
    /// A boolean value.
    Bool(bool),
    /// An Excel error value, such as `#DIV/0!` or `#N/A`.
    Error(String),
    /// Remove the value, retaining the cell's formatting and unknown content.
    ///
    /// This is the [`Default`] value.
    #[default]
    Blank,
}

impl CellValue {
    pub(crate) fn validate(&self) -> Result<()> {
        // Excel stores error cells as one of a fixed set of codes. Other
        // spellings, even plausible typos such as `#DIV/0`, display as a
        // different error. Reads stay lenient for codes added in the future.
        const ERROR_VALUES: [&str; 17] = [
            "#NULL!",
            "#DIV/0!",
            "#VALUE!",
            "#REF!",
            "#NAME?",
            "#NUM!",
            "#N/A",
            "#GETTING_DATA",
            "#SPILL!",
            "#CALC!",
            "#FIELD!",
            "#BLOCKED!",
            "#CONNECT!",
            "#BUSY!",
            "#UNKNOWN!",
            "#PYTHON!",
            "#EXTERNAL!",
        ];
        match self {
            Self::Number(n) if !n.is_finite() => {
                Err(Error::InvalidValue("numbers must be finite".into()))
            }
            // Characters XML 1.0 forbids are written as Excel `_xHHHH_` escapes,
            // so any text a cell can hold, including text read back, is valid.
            Self::Text(s) if s.len() > 32_767 && s.encode_utf16().take(32_768).count() > 32_767 => {
                Err(Error::InvalidValue(
                    "text exceeds Excel's 32,767 UTF-16 code unit limit".into(),
                ))
            }
            Self::Error(s) if !ERROR_VALUES.contains(&s.as_str()) => Err(Error::InvalidValue(
                "error values must be an Excel error code such as #N/A or #DIV/0!".into(),
            )),
            _ => Ok(()),
        }
    }
}

/// A cell's scalar value, optional formula text, and existing style index.
/// Formula values are cached results; no calculation is performed.
///
/// Values are returned by the read methods such as
/// [`Workbook::get_cell`](crate::Workbook::get_cell). The fields are public for
/// reading, but the struct is `#[non_exhaustive]` so that more read metadata
/// can be added in compatible releases: it cannot be built with a struct
/// literal outside this crate, and destructuring patterns need a `..` rest.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct CellContent {
    /// The scalar value, or a formula's stored cached result.
    pub value: CellValue,
    /// Stored formula text, without a leading equals sign.
    ///
    /// Shared-formula followers can contain empty formula text; formulas are
    /// neither expanded nor calculated.
    pub formula: Option<String>,
    /// The existing cell style index, when explicitly present.
    pub style_index: Option<u32>,
}

/// One validated edit, usable in a transaction spanning multiple worksheets.
#[derive(Clone, Debug, PartialEq)]
pub struct CellEdit {
    pub(crate) sheet: String,
    pub(crate) cell: CellRef,
    pub(crate) value: CellValue,
}

impl CellEdit {
    /// Construct an edit from a sheet name, an A1 address, and a value.
    ///
    /// The address and value are validated immediately. The sheet's existence
    /// and its [edit restrictions](crate::Workbook#edit-restrictions) are
    /// checked when the edit is applied.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCellReference`] for an invalid address, and
    /// otherwise [`Error::InvalidValue`] for a value no cell can hold.
    pub fn new(
        sheet: impl Into<String>,
        address: &str,
        value: impl Into<CellValue>,
    ) -> Result<Self> {
        Self::at(sheet, address.parse()?, value)
    }

    /// Construct an edit from a sheet name, a typed address, and a value.
    ///
    /// The value is validated immediately; the sheet's existence and its
    /// [edit restrictions](crate::Workbook#edit-restrictions) are checked when
    /// the edit is applied to a workbook.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidValue`] for a non-finite number, text longer
    /// than Excel's 32,767 UTF-16 code units, or an error value that is not
    /// one of Excel's error codes.
    pub fn at(
        sheet: impl Into<String>,
        cell: CellRef,
        value: impl Into<CellValue>,
    ) -> Result<Self> {
        let value = value.into();
        value.validate()?;
        Ok(Self {
            sheet: sheet.into(),
            cell,
            value,
        })
    }

    /// Return the target sheet name.
    pub fn sheet(&self) -> &str {
        &self.sheet
    }
    /// Return the target cell address.
    pub fn cell(&self) -> CellRef {
        self.cell
    }
    /// Return the validated value to write.
    pub fn value(&self) -> &CellValue {
        &self.value
    }
}

/// Typed accessors, which return `None` (or `false`) for other variants.
///
/// ```
/// use sheetpatch::CellValue;
///
/// let values = [
///     CellValue::from(2.5),
///     CellValue::from("n/a"),
///     CellValue::Blank,
///     CellValue::from(4),
/// ];
/// let total: f64 = values.iter().filter_map(CellValue::as_number).sum();
/// assert_eq!(total, 6.5);
/// assert_eq!(values[1].as_text(), Some("n/a"));
/// assert!(values[2].is_blank());
/// ```
impl CellValue {
    /// Return the text of a [`CellValue::Text`] value.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }
    /// Return the number of a [`CellValue::Number`] value.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(number) => Some(*number),
            _ => None,
        }
    }
    /// Return the boolean of a [`CellValue::Bool`] value.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }
    /// Return the error token, such as `#N/A`, of a [`CellValue::Error`] value.
    pub fn as_error(&self) -> Option<&str> {
        match self {
            Self::Error(token) => Some(token),
            _ => None,
        }
    }
    /// Return whether this is [`CellValue::Blank`].
    pub fn is_blank(&self) -> bool {
        matches!(self, Self::Blank)
    }
}

impl fmt::Display for CellValue {
    /// Format the value as plain text, matching the CLI's `get` output.
    ///
    /// Text and error tokens are written verbatim, numbers use `f64`'s
    /// `Display` (shortest round-trip digits, never an exponent), booleans are
    /// `true` or `false`, and a blank is an empty string. Number formats are
    /// not applied.
    ///
    /// Width, fill, and alignment are honored for every value. Precision
    /// applies only to numbers, as decimal places: other values are never
    /// truncated, so formatting a column of mixed values with `{:.2}` keeps
    /// every text value whole.
    ///
    /// ```
    /// use sheetpatch::CellValue;
    ///
    /// assert_eq!(CellValue::from(42).to_string(), "42");
    /// assert_eq!(CellValue::from(0.1 + 0.2).to_string(), "0.30000000000000004");
    /// assert_eq!(format!("{:.2}", CellValue::from(2.0 / 3.0)), "0.67");
    /// assert_eq!(format!("{:.2}", CellValue::from("text")), "text");
    /// assert_eq!(format!("[{:>6.1}]", CellValue::from(true)), "[  true]");
    /// assert_eq!(CellValue::Error("#N/A".into()).to_string(), "#N/A");
    /// assert_eq!(CellValue::Blank.to_string(), "");
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) | Self::Error(text) => pad_whole(f, text),
            Self::Number(number) => fmt::Display::fmt(number, f),
            Self::Bool(true) => pad_whole(f, "true"),
            Self::Bool(false) => pad_whole(f, "false"),
            Self::Blank => pad_whole(f, ""),
        }
    }
}

// Pads `text` to the formatter's width with its fill and alignment (left by
// default, as for strings), but ignores precision, which `Formatter::pad`
// would apply by truncating the text.
fn pad_whole(f: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    if f.precision().is_none() {
        return f.pad(text);
    }
    let padding = f
        .width()
        .map_or(0, |width| width.saturating_sub(text.chars().count()));
    if padding == 0 {
        return f.write_str(text);
    }
    let (before, after) = match f.align() {
        Some(fmt::Alignment::Right) => (padding, 0),
        Some(fmt::Alignment::Center) => (padding / 2, padding.div_ceil(2)),
        Some(fmt::Alignment::Left) | None => (0, padding),
    };
    let fill = f.fill();
    for _ in 0..before {
        fmt::Write::write_char(f, fill)?;
    }
    f.write_str(text)?;
    for _ in 0..after {
        fmt::Write::write_char(f, fill)?;
    }
    Ok(())
}

impl From<&str> for CellValue {
    fn from(v: &str) -> Self {
        Self::Text(v.to_owned())
    }
}
impl From<String> for CellValue {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}
impl From<&String> for CellValue {
    fn from(v: &String) -> Self {
        Self::Text(v.clone())
    }
}
impl From<Cow<'_, str>> for CellValue {
    fn from(v: Cow<'_, str>) -> Self {
        Self::Text(v.into_owned())
    }
}
impl From<char> for CellValue {
    fn from(v: char) -> Self {
        Self::Text(v.to_string())
    }
}
impl From<f64> for CellValue {
    fn from(v: f64) -> Self {
        Self::Number(v)
    }
}
macro_rules! exact_number_from {
    ($($int:ty),*) => {$(
        /// Converts the integer to an exactly equal [`CellValue::Number`].
        impl From<$int> for CellValue {
            fn from(v: $int) -> Self {
                Self::Number(f64::from(v))
            }
        }
    )*};
}
exact_number_from!(i8, i16, i32, u8, u16, u32);
impl From<bool> for CellValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}
/// Converts `Some(value)` like `value` itself, and `None` to
/// [`CellValue::Blank`], which clears the cell's value.
impl<T: Into<CellValue>> From<Option<T>> for CellValue {
    fn from(v: Option<T>) -> Self {
        v.map_or(Self::Blank, Into::into)
    }
}

/// 2^53: every integer up to this magnitude has an exact `f64` representation.
const MAX_EXACT_INTEGER: u64 = 1 << 53;

fn inexact_integer(v: impl fmt::Display) -> Error {
    Error::InvalidValue(format!(
        "integer {v} exceeds 2^53 in magnitude, so it cannot be stored exactly as a number; \
         write it as text"
    ))
}

/// Converts the integer to an exactly equal [`CellValue::Number`], or returns
/// [`Error::InvalidValue`] when its magnitude exceeds 2^53
/// (9,007,199,254,740,992), beyond which `f64` cannot represent every integer.
/// Write identifiers that need more digits as text.
impl TryFrom<i64> for CellValue {
    type Error = Error;

    fn try_from(v: i64) -> Result<Self> {
        if v.unsigned_abs() > MAX_EXACT_INTEGER {
            return Err(inexact_integer(v));
        }
        Ok(Self::Number(v as f64))
    }
}
/// Converts the integer to an exactly equal [`CellValue::Number`], or returns
/// [`Error::InvalidValue`] when it exceeds 2^53 (9,007,199,254,740,992), beyond
/// which `f64` cannot represent every integer. Write identifiers that need more
/// digits as text.
impl TryFrom<u64> for CellValue {
    type Error = Error;

    fn try_from(v: u64) -> Result<Self> {
        if v > MAX_EXACT_INTEGER {
            return Err(inexact_integer(v));
        }
        Ok(Self::Number(v as f64))
    }
}
/// Converts like the [`i64`] conversion, rejecting magnitudes above 2^53.
impl TryFrom<isize> for CellValue {
    type Error = Error;

    fn try_from(v: isize) -> Result<Self> {
        let v = i64::try_from(v).map_err(|_| inexact_integer(v))?;
        Self::try_from(v)
    }
}
/// Converts like the [`u64`] conversion, rejecting values above 2^53.
impl TryFrom<usize> for CellValue {
    type Error = Error;

    fn try_from(v: usize) -> Result<Self> {
        let v = u64::try_from(v).map_err(|_| inexact_integer(v))?;
        Self::try_from(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn addresses_round_trip_at_excel_boundaries() {
        for address in ["A1", "Z9", "AA10", "ZZ11", "AAA12", "XFD1048576"] {
            assert_eq!(address.parse::<CellRef>().unwrap().to_string(), address);
        }
        assert_eq!("b12".parse::<CellRef>().unwrap().to_string(), "B12");
        for address in [
            "",
            "A",
            "1",
            "A0",
            "A01",
            "$A$1",
            "A1:B2",
            "XFE1",
            "A1048577",
            "A9999999999999",
            "é1",
        ] {
            assert!(address.parse::<CellRef>().is_err(), "{address}");
        }
    }
    #[test]
    fn invalid_values_are_rejected() {
        for value in [
            CellValue::Number(f64::NAN),
            CellValue::Number(f64::INFINITY),
            CellValue::from("a".repeat(32_768)),
            CellValue::Error("not-an-error".into()),
            CellValue::Error("#BAD\nTOKEN".into()),
        ] {
            assert!(value.validate().is_err(), "{value:?}");
        }
        for error in [
            "#FOO", "##", "#1", "#DIV/0", "#N/A!", "#na", "#n/a", "#NA", "#", "", " #N/A", "#N/A ",
            "#REF!\0",
        ] {
            assert!(
                matches!(
                    CellValue::Error(error.into()).validate(),
                    Err(Error::InvalidValue(message)) if message.contains("#N/A")
                ),
                "{error:?}"
            );
        }
        for error in [
            "#NULL!",
            "#DIV/0!",
            "#VALUE!",
            "#REF!",
            "#NAME?",
            "#NUM!",
            "#N/A",
            "#GETTING_DATA",
            "#SPILL!",
            "#CALC!",
            "#FIELD!",
            "#BLOCKED!",
            "#CONNECT!",
            "#BUSY!",
            "#UNKNOWN!",
            "#PYTHON!",
            "#EXTERNAL!",
        ] {
            assert!(CellValue::Error(error.into()).validate().is_ok(), "{error}");
        }
        // Characters that XML 1.0 forbids are stored as `_xHHHH_` escapes.
        for text in [
            "a\0b",
            "\u{1}\u{8}\u{b}\u{c}\u{e}\u{1f}",
            "\u{fffe}\u{ffff}",
        ] {
            assert!(CellValue::from(text).validate().is_ok(), "{text:?}");
        }
    }

    #[test]
    fn text_limit_counts_utf16_units_instead_of_utf8_bytes() {
        for text in [
            "a".repeat(32_767),
            "é".repeat(32_767),
            format!("{}a", "😀".repeat(16_383)),
        ] {
            assert!(CellValue::Text(text).validate().is_ok());
        }
        for text in ["é".repeat(32_768), "😀".repeat(16_384)] {
            assert!(CellValue::Text(text).validate().is_err());
        }
    }

    #[test]
    fn typed_addresses_and_edits_validate_before_mutation() {
        assert_eq!(
            CellRef::new(1_048_576, 16_384).unwrap().to_string(),
            "XFD1048576"
        );
        for (row, column) in [(0, 1), (1, 0), (1_048_577, 1), (1, 16_385)] {
            assert!(CellRef::new(row, column).is_err());
        }
        assert!(CellEdit::new("Data", "A1", f64::NAN).is_err());
        let edit = CellEdit::at("Data", CellRef::new(2, 3).unwrap(), 7).unwrap();
        assert_eq!(edit.sheet(), "Data");
        assert_eq!(edit.cell().to_string(), "C2");
        assert_eq!(edit.value(), &CellValue::Number(7.0));
    }
}
