use crate::{Error, Result};
use std::{fmt, str::FromStr};

/// An A1 address within Excel's 16,384 columns and 1,048,576 rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CellRef {
    pub(crate) row: u32,
    pub(crate) column: u16,
}

impl CellRef {
    /// Construct an address from one-based row and column numbers.
    pub fn new(row: u32, column: u16) -> Result<Self> {
        if !(1..=1_048_576).contains(&row) || !(1..=16_384).contains(&column) {
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
        if column > 16_384 || !bytes[split..].iter().all(u8::is_ascii_digit) {
            return Err(invalid());
        }
        let row: u32 = address[split..].parse().map_err(|_| invalid())?;
        if !(1..=1_048_576).contains(&row) {
            return Err(invalid());
        }
        Ok(Self {
            row,
            column: column as u16,
        })
    }
}

impl fmt::Display for CellRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut column = self.column;
        let mut letters = [0u8; 3];
        let mut start = letters.len();
        while column != 0 {
            start -= 1;
            column -= 1;
            letters[start] = b'A' + (column % 26) as u8;
            column /= 26;
        }
        write!(
            f,
            "{}{}",
            std::str::from_utf8(&letters[start..]).unwrap(),
            self.row
        )
    }
}

/// Cell contents. Existing number formats and other cell attributes are retained.
/// Text is stored inline, so the workbook's shared-string table stays untouched.
#[derive(Clone, Debug, PartialEq)]
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
    Blank,
}

impl CellValue {
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::Number(n) if !n.is_finite() => {
                Err(Error::InvalidValue("numbers must be finite".into()))
            }
            Self::Text(s) if s.len() > 32_767 && s.encode_utf16().take(32_768).count() > 32_767 => {
                Err(Error::InvalidValue(
                    "text exceeds Excel's 32,767 UTF-16 code unit limit".into(),
                ))
            }
            Self::Text(s) if !s.chars().all(crate::xml::valid_char) => Err(Error::InvalidValue(
                "text contains a character forbidden by XML 1.0".into(),
            )),
            Self::Error(s)
                if s.len() > 64
                    || !s.starts_with('#')
                    || s.len() < 2
                    || !s.bytes().all(|c| {
                        c.is_ascii_uppercase() || c.is_ascii_digit() || b"#/!?._".contains(&c)
                    }) =>
            {
                Err(Error::InvalidValue(
                    "error values must be an Excel error token such as #N/A".into(),
                ))
            }
            _ => Ok(()),
        }
    }
}

/// A cell's scalar value, optional formula text, and existing style index.
/// Formula values are cached results; no calculation is performed.
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug)]
pub struct CellEdit {
    pub(crate) sheet: String,
    pub(crate) cell: CellRef,
    pub(crate) value: CellValue,
}

impl CellEdit {
    /// Construct an edit from a sheet name, an A1 address, and a value.
    ///
    /// The address and value are validated immediately. The sheet's existence
    /// and its formula or merged-cell restrictions are checked when applied.
    pub fn new(
        sheet: impl Into<String>,
        address: &str,
        value: impl Into<CellValue>,
    ) -> Result<Self> {
        Self::at(sheet, address.parse()?, value)
    }

    /// Construct an edit from a sheet name, a typed address, and a value.
    ///
    /// The value is validated immediately; worksheet restrictions are checked
    /// when the edit is applied to a workbook.
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
impl From<f64> for CellValue {
    fn from(v: f64) -> Self {
        Self::Number(v)
    }
}
impl From<i32> for CellValue {
    fn from(v: i32) -> Self {
        Self::Number(f64::from(v))
    }
}
impl From<bool> for CellValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
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
            CellValue::from("a\0b"),
            CellValue::from("a".repeat(32_768)),
            CellValue::Error("not-an-error".into()),
            CellValue::Error("#BAD\nTOKEN".into()),
        ] {
            assert!(value.validate().is_err());
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
