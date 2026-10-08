use std::{fmt, io};

/// Errors are returned before an unsafe or ambiguous edit is applied.
///
/// Additional error variants may be added in compatible releases. Callers should
/// include a fallback arm when matching errors.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// Reading, writing, or replacing a file failed.
    Io(io::Error),
    /// An accessed XML part could not be parsed safely.
    Xml(String),
    /// The archive or workbook structure is invalid or ambiguous.
    InvalidWorkbook(String),
    /// An address is invalid or outside Excel's row and column limits.
    InvalidCellReference(String),
    /// A scalar value cannot be represented within the supported cell format.
    InvalidValue(String),
    /// No worksheet has the requested exact, case-sensitive name.
    SheetNotFound(String),
    /// A requested shared-string value has no available string table.
    SharedStringsUnavailable,
    /// The input or requested operation is outside the supported scope.
    Unsupported(String),
}

/// The result of a workbook operation.
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
