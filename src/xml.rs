//! Shared XML invariants for values and package metadata.

mod attributes;
mod document;
mod namespace;
mod text;

#[cfg(test)]
pub(crate) use attributes::Attribute;
pub(crate) use attributes::{
    AttributeChecks, AttributeView, Attributes, arena_push, attribute_spans, attribute_value,
    compact, markup,
};
pub(crate) use document::{DocumentRules, trim_whitespace};
pub(crate) use namespace::{NameKind, NamespaceId, NamespacePool, NamespaceReader};
pub(crate) use text::{append_cell_text, copy_text, excel_text, reference};
