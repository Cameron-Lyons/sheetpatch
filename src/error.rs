use crate::CellRef;
use std::{fmt, io};

/// Errors are returned before an unsafe or ambiguous edit is applied.
///
/// Additional error variants may be added in compatible releases, and a
/// failure reported with one variant may move to a new, more specific variant.
/// Callers should include a fallback arm when matching errors.
///
/// Messages are written for people. Their wording is not part of the stable
/// API and may change in any release; match on variants and fields instead.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// Reading, writing, or replacing a file failed.
    ///
    /// The message includes the I/O error's own message, so
    /// [`source`](std::error::Error::source) returns that error's source
    /// rather than the I/O error itself, and error-chain reports do not repeat
    /// it. Match this variant to inspect the [`io::Error`], such as its kind.
    Io(io::Error),
    /// An accessed XML part could not be parsed safely.
    Xml(String),
    /// The archive or workbook structure is invalid or ambiguous, including
    /// malformed cell data such as a numeric cell whose value is not a number.
    InvalidWorkbook(String),
    /// An address or range is invalid or outside Excel's row and column limits.
    InvalidCellReference(String),
    /// A scalar value cannot be represented within the supported cell format.
    InvalidValue(String),
    /// No worksheet has the requested exact, case-sensitive name.
    SheetNotFound(String),
    /// An edit would change a cell the crate protects: a formula cell, a cell
    /// in a shared, array, or data-table formula's range, a merged cell other
    /// than its range's top-left anchor, an Excel table header cell, or a cell
    /// with cell or value metadata. Pending edits are unchanged.
    ///
    /// See the [edit restrictions](crate::Workbook#edit-restrictions) for when
    /// each kind of cell is protected.
    #[non_exhaustive]
    ProtectedCell {
        /// The name of the worksheet that holds the cell.
        sheet: String,
        /// The protected cell.
        cell: CellRef,
        /// Why the cell is protected, as a phrase that follows the cell's
        /// address, such as `contains a formula`. Like other messages, its
        /// wording is not stable.
        reason: String,
    },
    /// The input or requested operation is outside the supported scope: valid
    /// input the crate does not handle, such as an ISO 8601 date with a
    /// time-zone offset, or an edit refused for a reason other than a
    /// protected cell, such as a digitally signed package. See the
    /// [edit restrictions](crate::Workbook#edit-restrictions).
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
            Self::InvalidCellReference(e) => write!(f, "invalid cell reference: {e:?}"),
            Self::InvalidValue(e) => write!(f, "invalid cell value: {e}"),
            Self::SheetNotFound(e) => write!(f, "worksheet not found: {e:?}"),
            Self::ProtectedCell {
                sheet,
                cell,
                reason,
            } => write!(f, "protected cell: worksheet {sheet:?}: {cell} {reason}"),
            Self::Unsupported(e) => write!(f, "unsupported: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // The message already includes the I/O error's own message.
            Self::Io(e) => std::error::Error::source(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl Error {
    // quick-xml is a pre-1.0 dependency, so its error type is converted only
    // inside the crate rather than through a public `From` implementation.
    pub(crate) fn xml(error: quick_xml::Error) -> Self {
        Self::Xml(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[derive(Debug)]
    struct Cause;
    impl fmt::Display for Cause {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("root cause")
        }
    }
    impl std::error::Error for Cause {}

    #[derive(Debug)]
    struct Wrapper(Cause);
    impl fmt::Display for Wrapper {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("wrapper")
        }
    }
    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn io_errors_are_not_repeated_by_their_source_chain() {
        let error = Error::from(io::Error::other(Wrapper(Cause)));
        assert_eq!(error.to_string(), "I/O error: wrapper");
        let source = error.source().unwrap();
        assert_eq!(source.to_string(), "root cause");
        assert!(source.source().is_none());

        let plain = Error::from(io::Error::new(io::ErrorKind::NotFound, "missing"));
        assert_eq!(plain.to_string(), "I/O error: missing");
        assert!(plain.source().is_none());
        assert!(Error::Io(io::ErrorKind::NotFound.into()).source().is_none());
    }

    #[test]
    fn names_and_addresses_are_quoted() {
        assert_eq!(
            Error::SheetNotFound("Data \"2\"".into()).to_string(),
            "worksheet not found: \"Data \\\"2\\\"\""
        );
        assert_eq!(
            Error::InvalidCellReference("A0".into()).to_string(),
            "invalid cell reference: \"A0\""
        );
        let error = Error::ProtectedCell {
            sheet: "Q\"1".into(),
            cell: CellRef::new(2, 3).unwrap(),
            reason: "contains a formula".into(),
        };
        assert_eq!(
            error.to_string(),
            "protected cell: worksheet \"Q\\\"1\": C2 contains a formula"
        );
        assert!(error.source().is_none());
    }
}
