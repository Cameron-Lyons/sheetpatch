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
    /// Reading, writing, or replacing a file failed, or memory for a file's
    /// contents or a batch read's results could not be allocated, which is
    /// reported with kind [`io::ErrorKind::OutOfMemory`].
    ///
    /// The message includes the I/O error's own message, so
    /// [`source`](std::error::Error::source) returns that error's source
    /// rather than the I/O error itself, and error-chain reports do not repeat
    /// it. Match this variant to inspect the [`io::Error`], such as its kind.
    Io(io::Error),
    /// An accessed XML part could not be parsed safely.
    Xml(String),
    /// The archive or workbook structure is invalid or ambiguous, including
    /// malformed cell data such as a numeric cell whose value is not a number,
    /// and malformed worksheet or shared-string structure such as a cell
    /// reference that is not an address, rows out of order, or a cell with
    /// two values.
    InvalidWorkbook(String),
    /// An address or range is invalid or outside Excel's row and column limits.
    InvalidCellReference(String),
    /// A scalar value cannot be represented within the supported cell format.
    /// From [`Workbook::set_cells`](crate::Workbook::set_cells) or
    /// [`Workbook::set_cells_at`](crate::Workbook::set_cells_at), the message
    /// names the cell.
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
    /// time-zone offset or a worksheet row without an explicit row number, or
    /// an edit refused for a reason other than a protected cell, such as a
    /// digitally signed package. See the
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

    // A copy with the same variant and message, for an error that is kept
    // and reported again. `io::Error` is not `Clone`, so an I/O error's copy
    // keeps its kind and message but not its source.
    pub(crate) fn duplicate(&self) -> Self {
        match self {
            Self::Io(error) => Self::Io(io::Error::new(error.kind(), error.to_string())),
            Self::Xml(message) => Self::Xml(message.clone()),
            Self::InvalidWorkbook(message) => Self::InvalidWorkbook(message.clone()),
            Self::InvalidCellReference(address) => Self::InvalidCellReference(address.clone()),
            Self::InvalidValue(message) => Self::InvalidValue(message.clone()),
            Self::SheetNotFound(name) => Self::SheetNotFound(name.clone()),
            Self::ProtectedCell {
                sheet,
                cell,
                reason,
            } => Self::ProtectedCell {
                sheet: sheet.clone(),
                cell: *cell,
                reason: reason.clone(),
            },
            Self::Unsupported(message) => Self::Unsupported(message.clone()),
        }
    }

    // Names what an error concerns, such as a cell, worksheet, or part, by
    // prefixing its message and retaining its variant. Every variant is
    // listed, so a new one must be classified here.
    pub(crate) fn context(self, prefix: impl fmt::Display) -> Self {
        match self {
            Self::Xml(message) => Self::Xml(format!("{prefix}: {message}")),
            Self::InvalidWorkbook(message) => Self::InvalidWorkbook(format!("{prefix}: {message}")),
            Self::InvalidValue(message) => Self::InvalidValue(format!("{prefix}: {message}")),
            Self::Unsupported(message) => Self::Unsupported(format!("{prefix}: {message}")),
            // These identify their subject in fields or in quoted input.
            error @ (Self::Io(_)
            | Self::InvalidCellReference(_)
            | Self::SheetNotFound(_)
            | Self::ProtectedCell { .. }) => error,
        }
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
    fn duplicates_keep_the_variant_and_message() {
        let errors = [
            Error::Io(io::Error::other(Wrapper(Cause))),
            Error::Io(io::ErrorKind::OutOfMemory.into()),
            Error::Xml("xml".into()),
            Error::InvalidWorkbook("invalid".into()),
            Error::InvalidCellReference("A0".into()),
            Error::InvalidValue("value".into()),
            Error::SheetNotFound("Data".into()),
            Error::ProtectedCell {
                sheet: "Data".into(),
                cell: CellRef::new(2, 3).unwrap(),
                reason: "contains a formula".into(),
            },
            Error::Unsupported("unsupported".into()),
        ];
        for error in &errors {
            let copy = error.duplicate();
            assert_eq!(std::mem::discriminant(&copy), std::mem::discriminant(error));
            assert_eq!(copy.to_string(), error.to_string());
        }
        assert!(matches!(
            Error::Io(io::ErrorKind::OutOfMemory.into()).duplicate(),
            Error::Io(error) if error.kind() == io::ErrorKind::OutOfMemory
        ));
    }

    #[test]
    fn context_prefixes_only_messages_that_do_not_identify_their_subject() {
        let cell = CellRef::new(7, 2).unwrap();
        let prefixed = [
            Error::Xml("xml".into()),
            Error::InvalidWorkbook("invalid".into()),
            Error::InvalidValue("value".into()),
            Error::Unsupported("unsupported".into()),
        ];
        for error in prefixed {
            let expected = error.to_string().replacen(": ", ": B7: ", 1);
            let kind = std::mem::discriminant(&error);
            let error = error.context(cell);
            assert_eq!(std::mem::discriminant(&error), kind);
            assert_eq!(error.to_string(), expected);
        }
        assert_eq!(
            Error::Xml("xml".into())
                .context(cell)
                .context(format_args!("worksheet {:?}", "Data"))
                .to_string(),
            "XML error: worksheet \"Data\": B7: xml"
        );

        let unchanged = [
            Error::Io(io::Error::other(Wrapper(Cause))),
            Error::InvalidCellReference("A0".into()),
            Error::SheetNotFound("Data".into()),
            Error::ProtectedCell {
                sheet: "Data".into(),
                cell,
                reason: "contains a formula".into(),
            },
        ];
        for error in unchanged {
            let expected = error.to_string();
            let kind = std::mem::discriminant(&error);
            let error = error.context(cell);
            assert_eq!(std::mem::discriminant(&error), kind);
            assert_eq!(error.to_string(), expected);
        }
        assert!(
            Error::Io(io::Error::other(Wrapper(Cause)))
                .context(cell)
                .source()
                .is_some()
        );
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
