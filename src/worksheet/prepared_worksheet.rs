use super::index::WorksheetIndex;
use super::patch::{self, EditPlan};
use super::read::{self, ReadError, SharedStrings};
use crate::{CellContent, CellRef, CellValue, Result};
#[cfg(test)]
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::result::Result as StdResult;

/// A validated worksheet index retained only for the lifetime of an explicit
/// view or of one worksheet's edits. All spans borrow the immutable XML
/// supplied at construction.
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
        self.index.cells.iter().map(|cell| cell.address)
    }

    /// Reads one cell without allocating a batch result. The shared-string
    /// loader is called only when the cell refers to that table.
    pub(crate) fn read_cell<'strings>(
        &self,
        cell: CellRef,
        mut shared_strings: impl FnMut() -> Result<Option<&'strings SharedStrings>>,
        date1904: bool,
    ) -> StdResult<CellContent, ReadError> {
        read::cell_content(self.xml, &self.index, cell, &mut shared_strings, date1904)
    }

    /// Reads cells in request order. The shared-string loader is called only
    /// for requested `t="s"` cells, and returns `None` for a workbook without
    /// a table; the workbook loads its table once and keeps it. `date1904`
    /// selects the workbook's date system for ISO 8601 (`t="d"`) cells, which
    /// read as Excel serials.
    pub(crate) fn read_cells<'strings>(
        &self,
        cells: &[CellRef],
        shared_strings: impl FnMut() -> Result<Option<&'strings SharedStrings>>,
        date1904: bool,
    ) -> StdResult<Vec<CellContent>, ReadError> {
        read::read_cells(self.xml, &self.index, cells, shared_strings, date1904)
    }

    /// Plans writing `cells` into the indexed XML, or returns `None` when
    /// nothing changes. The plan holds only the new markup; its `apply`
    /// writes the edited XML. This worksheet is left unchanged, so its reads
    /// still return the cells as they were before the edits. Every public
    /// edit path validates its values before reaching the worksheet, so they
    /// are not checked again here.
    ///
    /// The shared-string loader is called only to recognize rewriting a
    /// `t="s"` cell's current text; a table that is absent or fails to load,
    /// or an entry it cannot decode, leaves such an edit as a change.
    /// `date1904` likewise recognizes rewriting an ISO 8601 (`t="d"`) cell's
    /// serial.
    pub(crate) fn plan_cells<'strings>(
        &self,
        cells: &BTreeMap<CellRef, CellValue>,
        shared_strings: impl FnMut() -> Option<&'strings SharedStrings>,
        date1904: bool,
    ) -> Result<Option<CellPatch<'xml>>> {
        debug_assert!(
            cells.values().all(|value| value.validate().is_ok()),
            "every public edit path validates its values before planning them"
        );
        let plan = patch::plan_cells(self.xml, &self.index, cells, shared_strings, date1904)?;
        Ok((!plan.is_empty()).then_some(CellPatch {
            xml: self.xml,
            plan,
        }))
    }

    /// Returns the indexed XML with `cells` written, or the original XML,
    /// borrowed, when nothing changes.
    #[cfg(test)]
    pub(crate) fn patch_cells<'strings>(
        &self,
        cells: &BTreeMap<CellRef, CellValue>,
        shared_strings: impl FnMut() -> Option<&'strings SharedStrings>,
        date1904: bool,
    ) -> Result<Cow<'xml, [u8]>> {
        match self.plan_cells(cells, shared_strings, date1904)? {
            Some(patch) => patch.apply().map(Cow::Owned),
            None => Ok(Cow::Borrowed(self.xml)),
        }
    }
}

/// The planned edits of a batch that changes a worksheet.
pub(crate) struct CellPatch<'xml> {
    xml: &'xml [u8],
    plan: EditPlan,
}

impl CellPatch<'_> {
    /// Writes the edited worksheet XML.
    pub(crate) fn apply(self) -> Result<Vec<u8>> {
        self.plan.apply(self.xml)
    }
}

pub(crate) fn read_cells<'strings>(
    xml: &[u8],
    cells: &[CellRef],
    shared_strings: impl FnMut() -> Result<Option<&'strings SharedStrings>>,
    date1904: bool,
) -> StdResult<Vec<CellContent>, ReadError> {
    if cells.is_empty() {
        return Ok(Vec::new());
    }
    PreparedWorksheet::parse(xml)?.read_cells(cells, shared_strings, date1904)
}
