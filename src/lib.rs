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

pub use cell::{CellContent, CellEdit, CellRef, CellValue};
pub use error::{Error, Result};
pub use package::{Sheet, Workbook, WorksheetView};
