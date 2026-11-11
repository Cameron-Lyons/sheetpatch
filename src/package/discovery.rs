//! Validated OOXML package discovery and relationship target resolution.

mod namespaces;
mod part;
mod relationships;
mod tables;
mod targets;
mod workbook;

pub(super) use tables::table_headers;
pub(super) use workbook::{DiscoveredSheet, PackageMetadata, discover};
