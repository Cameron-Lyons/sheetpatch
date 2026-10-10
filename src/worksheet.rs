//! Namespace-aware XML edits against byte spans, without reserializing the sheet.
use crate::xml::{namespace as decoded_namespace, valid_char as valid_xml_char};
use crate::{CellContent, CellRef, CellValue, Error, Result};
use std::{borrow::Cow, collections::BTreeMap};

const TRANSITIONAL: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const STRICT: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";

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

    pub(crate) fn read_cells<'strings>(
        &self,
        cells: &[CellRef],
        shared_strings: impl FnMut() -> Result<&'strings [String]>,
    ) -> Result<Vec<CellContent>> {
        read::read_cells(self.xml, &self.index, cells, shared_strings)
    }

    pub(crate) fn patch_cells(
        &self,
        cells: &BTreeMap<CellRef, CellValue>,
    ) -> Result<Cow<'xml, [u8]>> {
        // Scalar values are validated before constructing the index, keeping
        // value errors ahead of worksheet parsing errors.
        patch::patch_cells(self.xml, &self.index, cells)
    }
}

pub(crate) fn patch_cells<'xml>(
    xml: &'xml [u8],
    cells: &BTreeMap<CellRef, CellValue>,
) -> Result<Cow<'xml, [u8]>> {
    if cells.is_empty() {
        return Ok(Cow::Borrowed(xml));
    }
    // Values fail before XML parsing, preserving the established error order.
    for value in cells.values() {
        value.validate()?;
    }
    let worksheet = PreparedWorksheet::parse(xml)?;
    worksheet.patch_cells(cells)
}

pub(crate) fn read_cells<'strings>(
    xml: &[u8],
    cells: &[CellRef],
    shared_strings: impl FnMut() -> Result<&'strings [String]>,
) -> Result<Vec<CellContent>> {
    if cells.is_empty() {
        return Ok(Vec::new());
    }
    PreparedWorksheet::parse(xml)?.read_cells(cells, shared_strings)
}

type RowCells = (u32, usize, std::ops::Range<usize>);

fn xml_error(message: impl Into<String>) -> Error {
    Error::Xml(message.into())
}
fn unsupported(message: impl Into<String>) -> Error {
    Error::Unsupported(message.into())
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
    patch_cells(xml, &BTreeMap::from([(cell, value.clone())])).map(Cow::into_owned)
}

#[cfg(test)]
fn read_cell(xml: &[u8], cell: CellRef, shared_strings: Option<&[String]>) -> Result<CellContent> {
    read_cells(xml, &[cell], || {
        shared_strings.ok_or(Error::SharedStringsUnavailable)
    })
    .map(|mut cells| cells.remove(0))
}

#[cfg(test)]
mod tests;
