use crate::{Error, Result};
use std::{fmt, str::FromStr};

/// An A1 address within Excel's 16,384 columns and 1,048,576 rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CellRef {
    pub(crate) row: u32,
    pub(crate) column: u16,
}

impl CellRef {
    pub fn row(self) -> u32 {
        self.row
    }
    pub fn column(self) -> u16 {
        self.column
    }
}

impl FromStr for CellRef {
    type Err = Error;

    fn from_str(address: &str) -> Result<Self> {
        let invalid = || Error::InvalidCellReference(address.to_owned());
        let bytes = address.as_bytes();
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
    Text(String),
    Number(f64),
    Bool(bool),
    /// Remove the value, retaining the cell's formatting and unknown content.
    Blank,
}

impl CellValue {
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::Number(n) if !n.is_finite() => Err(Error::InvalidValue("numbers must be finite".into())),
            Self::Text(s) if s.encode_utf16().count() > 32_767 => Err(Error::InvalidValue("text exceeds Excel's 32,767 UTF-16 code unit limit".into())),
            Self::Text(s) if s.chars().any(|c| !matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')) => {
                Err(Error::InvalidValue("text contains a character forbidden by XML 1.0".into()))
            }
            _ => Ok(()),
        }
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
        ] {
            assert!(value.validate().is_err());
        }
    }
}
