use std::{fmt, io};

/// Errors are returned before an unsafe or ambiguous edit is applied.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Xml(String),
    InvalidWorkbook(String),
    InvalidCellReference(String),
    InvalidValue(String),
    SheetNotFound(String),
    SharedStringsUnavailable,
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Xml(e) => write!(f, "XML error: {e}"),
            Self::InvalidWorkbook(e) => write!(f, "invalid workbook: {e}"),
            Self::InvalidCellReference(e) => write!(f, "invalid cell reference: {e}"),
            Self::InvalidValue(e) => write!(f, "invalid cell value: {e}"),
            Self::SheetNotFound(e) => write!(f, "worksheet not found: {e}"),
            Self::SharedStringsUnavailable => write!(f, "shared-string table is unavailable"),
            Self::Unsupported(e) => write!(f, "unsupported edit: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<quick_xml::Error> for Error {
    fn from(value: quick_xml::Error) -> Self {
        Self::Xml(value.to_string())
    }
}
