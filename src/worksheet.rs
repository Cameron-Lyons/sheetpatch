//! Namespace-aware XML edits against byte spans, without reserializing the sheet.

mod date;
mod errors;
mod index;
mod parse;
mod patch;
mod prepared_worksheet;
pub(crate) mod protection;
mod read;
#[cfg(test)]
mod tests;
mod validation;

#[cfg(test)]
pub(crate) use parse::parse_xml;
pub(crate) use prepared_worksheet::{PreparedWorksheet, read_cells};
pub(crate) use read::{ReadError, SharedStrings, read_shared_strings};
