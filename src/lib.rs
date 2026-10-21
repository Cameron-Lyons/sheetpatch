#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod archive;
mod atomic;
mod cell;
mod error;
mod package;
mod worksheet;
mod xml;

// The integration tests' independent ZIP reader and writer.
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod common;

pub use cell::{CellContent, CellEdit, CellRange, CellRangeIter, CellRef, CellValue};
pub use error::{Error, Result};
pub use package::{Sheet, SheetVisibility, Workbook, WorksheetView};
