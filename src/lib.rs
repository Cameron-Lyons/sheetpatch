#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
// API checks from the Google Rust Style Guide and the Rust API Guidelines.
#![warn(
    clippy::exhaustive_enums,
    clippy::exhaustive_structs,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::return_self_not_must_use,
    clippy::unwrap_used,
    missing_debug_implementations
)]

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
