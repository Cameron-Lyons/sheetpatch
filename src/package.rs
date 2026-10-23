//! The workbook package: its worksheets, their read views, and edits.

mod discovery;
mod read_batch;
mod sheet;
mod workbook;
mod worksheet_view;

pub use sheet::{Sheet, SheetVisibility};
pub use workbook::Workbook;
pub use worksheet_view::WorksheetView;
