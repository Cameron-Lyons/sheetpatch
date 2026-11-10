//! OOXML namespaces and relationship type identifiers.
pub(super) const PACKAGE_NAMESPACE: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships";
pub(super) const SIGNATURE_ORIGIN: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin";
pub(super) const CONTENT_TYPES_NAMESPACE: &str =
    "http://schemas.openxmlformats.org/package/2006/content-types";
pub(super) const MAIN_NAMESPACE: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
pub(super) const STRICT_MAIN_NAMESPACE: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";
pub(super) const OFFICE_NAMESPACE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub(super) const STRICT_OFFICE_NAMESPACE: &str =
    "http://purl.oclc.org/ooxml/officeDocument/relationships";
pub(super) const COMPATIBILITY_NAMESPACE: &str =
    "http://schemas.openxmlformats.org/markup-compatibility/2006";

pub(super) fn main_namespace(namespace: &str) -> bool {
    matches!(namespace, MAIN_NAMESPACE | STRICT_MAIN_NAMESPACE)
}
pub(super) fn office_type(kind: &str, suffix: &str) -> bool {
    kind.rsplit_once('/').is_some_and(|(namespace, local)| {
        local == suffix && matches!(namespace, OFFICE_NAMESPACE | STRICT_OFFICE_NAMESPACE)
    })
}
