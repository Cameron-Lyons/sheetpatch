use crate::{CellRef, Error, Result};

// A protected-cell refusal. The worksheet's name is not known here: the
// workbook fills it in, along with the context of every other worksheet error.
pub(super) fn protected(cell: CellRef, reason: &str) -> Error {
    Error::ProtectedCell {
        sheet: String::new(),
        cell,
        reason: reason.to_owned(),
    }
}

pub(super) fn xml_error(message: impl Into<String>) -> Error {
    Error::Xml(message.into())
}
pub(super) fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidWorkbook(message.into())
}
pub(super) fn unsupported(message: impl Into<String>) -> Error {
    Error::Unsupported(message.into())
}

// At most one item, or `error()` when there are more.
pub(super) fn unique<T>(
    mut items: impl Iterator<Item = T>,
    error: impl FnOnce() -> Error,
) -> Result<Option<T>> {
    let first = items.next();
    if items.next().is_some() {
        return Err(error());
    }
    Ok(first)
}
