//! Namespace-aware XML edits against byte spans, without reserializing the sheet.
use crate::xml::{namespace as decoded_namespace, valid_char as valid_xml_char};
use crate::{CellContent, CellRef, CellValue, Error, Result};
use std::{borrow::Cow, collections::BTreeMap};

const TRANSITIONAL: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const STRICT: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";

mod date;
mod index;
mod parse;
mod patch;
mod read;
mod validation;

use index::WorksheetIndex;
pub(crate) use read::read_shared_strings;

/// A validated worksheet index retained only for the lifetime of an explicit view.
/// All spans borrow the immutable XML supplied at construction.
pub(crate) struct PreparedWorksheet<'xml> {
    xml: &'xml [u8],
    index: WorksheetIndex<'xml>,
}

impl<'xml> PreparedWorksheet<'xml> {
    pub(crate) fn parse(xml: &'xml [u8]) -> Result<Self> {
        Ok(Self {
            xml,
            index: WorksheetIndex::parse(xml)?,
        })
    }

    /// The address of every `<c>` element, in row-major order.
    pub(crate) fn cells(&self) -> impl ExactSizeIterator<Item = CellRef> + '_ {
        self.index.cells.iter().map(|&(cell, _)| cell)
    }

    /// Reads cells in request order. Shared strings are loaded at most once,
    /// only for a requested `t="s"` cell; the loader returns `None` for a
    /// workbook without a table. `date1904` selects the workbook's date
    /// system for ISO 8601 (`t="d"`) cells, which read as Excel serials.
    pub(crate) fn read_cells<'strings>(
        &self,
        cells: &[CellRef],
        shared_strings: impl FnMut() -> Result<Option<&'strings [String]>>,
        date1904: bool,
    ) -> std::result::Result<Vec<CellContent>, ReadError> {
        read::read_cells(self.xml, &self.index, cells, shared_strings, date1904)
    }
}

/// Why a read failed: a problem in the worksheet itself, or a failure to load
/// the shared-string table a `t="s"` cell needed. The table's errors come
/// from its loader, which names that part, so only worksheet errors need the
/// worksheet's name.
#[derive(Debug)]
pub(crate) enum ReadError {
    Worksheet(Error),
    SharedStrings(Error),
}

impl From<Error> for ReadError {
    fn from(error: Error) -> Self {
        Self::Worksheet(error)
    }
}

/// Parses and patches a worksheet. Every public edit path validates its values
/// before reaching the worksheet, so they are not checked again here.
///
/// Shared strings are loaded at most once, only to recognize rewriting a
/// `t="s"` cell's current text; a table that is absent or fails to load leaves
/// such an edit as a change. `date1904` likewise recognizes rewriting an ISO
/// 8601 (`t="d"`) cell's serial. Returns the original XML, borrowed, when
/// nothing changes.
pub(crate) fn patch_cells<'xml, 'strings>(
    xml: &'xml [u8],
    cells: &BTreeMap<CellRef, CellValue>,
    shared_strings: impl FnMut() -> Option<&'strings [String]>,
    date1904: bool,
) -> Result<Cow<'xml, [u8]>> {
    if cells.is_empty() {
        return Ok(Cow::Borrowed(xml));
    }
    debug_assert!(cells.values().all(|value| value.validate().is_ok()));
    let index = WorksheetIndex::parse(xml)?;
    patch::patch_cells(xml, &index, cells, shared_strings, date1904)
}

pub(crate) fn read_cells<'strings>(
    xml: &[u8],
    cells: &[CellRef],
    shared_strings: impl FnMut() -> Result<Option<&'strings [String]>>,
    date1904: bool,
) -> std::result::Result<Vec<CellContent>, ReadError> {
    if cells.is_empty() {
        return Ok(Vec::new());
    }
    PreparedWorksheet::parse(xml)?.read_cells(cells, shared_strings, date1904)
}

/// Why each kind of protected cell refuses edits, completing a sentence that
/// starts with the cell's address.
pub(crate) mod protection {
    pub(crate) const FORMULA: &str = "contains a formula";
    pub(crate) const FORMULA_RANGE: &str = "belongs to a formula range";
    pub(crate) const MERGED: &str = "is merged into another cell's range";
    pub(crate) const TABLE_HEADER: &str = "is an Excel table header cell; \
        editing it would desynchronize the table's column names";
    pub(crate) const VALUE_METADATA: &str = "has value metadata (a linked or rich data type)";
}

// A protected-cell refusal. The worksheet's name is not known here: the
// workbook fills it in, along with the context of every other worksheet error.
fn protected(cell: CellRef, reason: &str) -> Error {
    Error::ProtectedCell {
        sheet: String::new(),
        cell,
        reason: reason.to_owned(),
    }
}

fn xml_error(message: impl Into<String>) -> Error {
    Error::Xml(message.into())
}
fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidWorkbook(message.into())
}
fn unsupported(message: impl Into<String>) -> Error {
    Error::Unsupported(message.into())
}

// Names the cell an error concerns, retaining the error's variant. A
// protected-cell refusal names its cell in a field instead.
fn at_cell(cell: CellRef, error: Error) -> Error {
    match error {
        Error::Unsupported(message) => unsupported(format!("{cell}: {message}")),
        Error::InvalidWorkbook(message) => invalid(format!("{cell}: {message}")),
        Error::Xml(message) => xml_error(format!("{cell}: {message}")),
        error => error,
    }
}

fn unique<T>(mut items: impl Iterator<Item = T>, message: &'static str) -> Result<Option<T>> {
    let first = items.next();
    if items.next().is_some() {
        return Err(unsupported(message));
    }
    Ok(first)
}

fn decoded(input: &str) -> Result<String> {
    let value = quick_xml::escape::unescape(input).map_err(|e| xml_error(e.to_string()))?;
    if !value.chars().all(valid_xml_char) {
        return Err(xml_error("invalid XML character reference"));
    }
    Ok(value.into_owned())
}

#[cfg(test)]
fn patch_cell(xml: &[u8], cell: CellRef, value: &CellValue) -> Result<Vec<u8>> {
    patch_cells(
        xml,
        &BTreeMap::from([(cell, value.clone())]),
        || None,
        false,
    )
    .map(Cow::into_owned)
}

#[cfg(test)]
fn read_cell(xml: &[u8], cell: CellRef, shared_strings: Option<&[String]>) -> Result<CellContent> {
    read_cells(xml, &[cell], || Ok(shared_strings), false)
        .map(|mut cells| cells.remove(0))
        .map_err(|error| match error {
            ReadError::Worksheet(error) | ReadError::SharedStrings(error) => error,
        })
}

#[cfg(test)]
mod tests;
