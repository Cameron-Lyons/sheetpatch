//! Shared XML invariants for values and package metadata.

mod attributes;
mod document;
mod namespace;
mod text;

// Only the 64-bit record-size test names this type.
#[cfg(all(test, target_pointer_width = "64"))]
pub(crate) use attributes::Attribute;
pub(crate) use attributes::{
    AttributeChecks, AttributeView, Attributes, arena_push, attribute_spans, attribute_value,
    compact, markup,
};
pub(crate) use document::{DocumentRules, is_space, trim_whitespace, valid_ncname};
pub(crate) use namespace::{NameKind, NamespaceId, NamespacePool, NamespaceReader};
pub(crate) use text::{append_cell_text, append_excel_text, copy_text, excel_text, reference};
