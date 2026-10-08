//! Edit cells in existing OOXML Excel workbooks while retaining untouched parts.
//!
//! See [`Workbook`] for the main API. This crate never reconstructs a workbook
//! from a spreadsheet object model.

mod archive;
mod cell;
mod error;
mod package;
mod worksheet;

pub use cell::{CellRef, CellValue};
pub use error::{Error, Result};
pub use package::{Sheet, Workbook};
