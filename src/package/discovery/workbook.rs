//! Workbook metadata and worksheet discovery.
use super::namespaces::{
    self, COMPATIBILITY_NAMESPACE, CONTENT_TYPES_NAMESPACE, OFFICE_NAMESPACE, SIGNATURE_ORIGIN,
    STRICT_OFFICE_NAMESPACE,
};
use super::part::{self, Node};
use super::relationships::{self, Relationship};
use super::targets;
use crate::archive::Archive;
use crate::package::{Sheet, SheetVisibility};
use crate::{Error, Result, xml};
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[cfg(test)]
#[path = "tests/workbook.rs"]
mod tests;

pub(in crate::package) struct PackageMetadata {
    pub(in crate::package) sheets: Vec<DiscoveredSheet>,
    pub(in crate::package) signed: bool,
    pub(in crate::package) date1904: bool,
    pub(in crate::package) shared_strings_path: Option<String>,
}

pub(in crate::package) struct DiscoveredSheet {
    pub(in crate::package) sheet: Sheet,
    /// Relationships resolve against the URI, even for legacy decoded ZIP names.
    pub(in crate::package) uri: String,
}

pub(in crate::package) fn discover(archive: &Archive) -> Result<PackageMetadata> {
    validate_content_types(archive)?;
    let office = office_document(archive)?;
    let workbook = {
        let xml = archive.read(&office.path)?;
        let part = part::part_xml(&xml, workbook_node)?;
        let root = &part.nodes[0];
        if part.process_content
            && root.local == "workbook"
            && namespaces::main_namespace(&root.namespace)
        {
            part::reject_promoted_workbook_properties(&xml)?;
        }
        part.nodes
    };
    let (sheets_index, date1904) = workbook_properties(&workbook)?;
    let workbook_relationships_path =
        targets::part_path(archive, &targets::relationships_path(&office.uri))?;
    // Discovery uses only the shared-string relationships and those that
    // sheets name.
    let sheet_ids: HashSet<&str> = sheet_nodes(&workbook)
        .flat_map(sheet_relationship_ids)
        .collect();
    let workbook_relationships =
        relationships::parse(&archive.read(&workbook_relationships_path)?, &|id, kind| {
            namespaces::office_type(kind, "sharedStrings") || sheet_ids.contains(id)
        })?;
    let shared_strings_path = shared_strings_path(archive, &office.uri, &workbook_relationships)?;
    let sheets = sheets(
        archive,
        &office.uri,
        &workbook,
        sheets_index,
        &workbook_relationships,
    )?;
    Ok(PackageMetadata {
        sheets,
        signed: office.signed,
        date1904,
        shared_strings_path,
    })
}

fn validate_content_types(archive: &Archive) -> Result<()> {
    // Like the other OPC parts, the content-types stream is found without
    // regard to ASCII case, and equivalent names are ambiguous.
    let path = archive
        .part_name("[Content_Types].xml")?
        .ok_or_else(|| Error::InvalidWorkbook("missing [Content_Types].xml".into()))?;
    let types = part::xml_nodes(&archive.read(path)?, part::root_only)?;
    if types[0].local != "Types" || &*types[0].namespace != CONTENT_TYPES_NAMESPACE {
        return Err(Error::InvalidWorkbook("invalid content-types root".into()));
    }
    Ok(())
}

struct OfficeDocument {
    /// The workbook part's URI, against which its relationships resolve.
    uri: String,
    /// The workbook part's ZIP member name.
    path: String,
    signed: bool,
}

fn office_document(archive: &Archive) -> Result<OfficeDocument> {
    let root_relationships_path = archive.part_name("_rels/.rels")?.unwrap_or("_rels/.rels");
    // Only the office document and a signature origin are used.
    let roots = relationships::parse(&archive.read(root_relationships_path)?, &|_, kind| {
        namespaces::office_type(kind, "officeDocument") || kind == SIGNATURE_ORIGIN
    })?;
    let mut office = roots
        .iter()
        .filter(|r| namespaces::office_type(&r.kind, "officeDocument"));
    let office = match (office.next(), office.next()) {
        (Some(relationship), None) if !relationship.external => relationship,
        _ => {
            return Err(Error::InvalidWorkbook(
                "expected one internal officeDocument relationship".into(),
            ));
        }
    };
    let uri = targets::resolve_target("", &office.target)?;
    let path = targets::part_path(archive, &uri)?;
    if path
        .rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("bin"))
    {
        return Err(Error::Unsupported(
            "binary Excel workbooks (.xlsb) are not supported".into(),
        ));
    }
    Ok(OfficeDocument {
        uri,
        path,
        signed: signed(archive, &roots),
    })
}

// A signature origin relationship, or any part in the conventional signature
// folder, marks a signed package.
fn signed(archive: &Archive, roots: &[Relationship]) -> bool {
    roots.iter().any(|r| r.kind == SIGNATURE_ORIGIN)
        || archive.entries().iter().any(|entry| {
            entry
                .name
                .split_once('/')
                .is_some_and(|(root, _)| root.eq_ignore_ascii_case("_xmlsignatures"))
        })
}

/// Validates the workbook root. Returns the index of its `sheets` element and
/// whether its `workbookPr` element selects the 1904 date system.
fn workbook_properties(nodes: &[Node]) -> Result<(usize, bool)> {
    let root = &nodes[0];
    if root.local != "workbook" || !namespaces::main_namespace(&root.namespace) {
        return Err(Error::InvalidWorkbook(
            "officeDocument is not a SpreadsheetML workbook".into(),
        ));
    }
    // Other spreadsheet readers recognize both dialects in one document.
    // Ignoring the other known namespace could omit a worksheet or silently
    // select the wrong date system.
    if nodes
        .iter()
        .any(|node| namespaces::main_namespace(&node.namespace) && node.namespace != root.namespace)
    {
        return Err(Error::Unsupported(
            "workbook mixes strict and transitional SpreadsheetML namespaces".into(),
        ));
    }
    let in_workbook =
        |node: &Node, local: &str| node.local == local && node.namespace == root.namespace;
    // Which sheet list a reader sees would depend on the branch it selects.
    if nodes
        .iter()
        .any(|node| in_workbook(node, "sheets") && in_branch(nodes, node))
    {
        return Err(Error::Unsupported(
            "sheets inside a markup-compatibility branch".into(),
        ));
    }
    let mut containers = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| in_workbook(node, "sheets"));
    let sheets = match (containers.next(), containers.next()) {
        (Some((index, node)), None) if node.parent == Some(0) => index,
        _ => {
            return Err(Error::InvalidWorkbook(
                "workbook must contain exactly one direct sheets element".into(),
            ));
        }
    };
    // Compatibility branches can substitute a workbookPr with a different
    // date system. Do not silently select 1900 dates or guess which branch
    // another spreadsheet reader will process.
    if nodes
        .iter()
        .any(|node| in_workbook(node, "workbookPr") && node.parent != Some(0))
    {
        return Err(Error::Unsupported(
            "workbookPr inside a markup-compatibility branch may change the date system".into(),
        ));
    }
    let mut properties = nodes.iter().filter(|node| in_workbook(node, "workbookPr"));
    let date1904 = match (properties.next(), properties.next()) {
        (None, _) => false,
        // An xsd:boolean, which collapses surrounding XML whitespace.
        (Some(properties), None) => match properties
            .attribute("", "date1904")
            .map(xml::trim_whitespace)
        {
            None | Some("0" | "false") => false,
            Some("1" | "true") => true,
            Some(value) => {
                return Err(Error::InvalidWorkbook(format!(
                    "invalid workbookPr date1904 value {value:?}"
                )));
            }
        },
        _ => {
            return Err(Error::InvalidWorkbook(
                "workbook must contain at most one workbookPr element".into(),
            ));
        }
    };
    Ok((sheets, date1904))
}

fn shared_strings_path(
    archive: &Archive,
    workbook_uri: &str,
    relationships: &[Relationship],
) -> Result<Option<String>> {
    let mut shared = relationships
        .iter()
        .filter(|r| namespaces::office_type(&r.kind, "sharedStrings"));
    match (shared.next(), shared.next()) {
        (Some(relationship), None) if !relationship.external => {
            targets::resolve_part_target(archive, workbook_uri, &relationship.target).map(Some)
        }
        (None, None) => Ok(None),
        _ => Err(Error::InvalidWorkbook(
            "expected at most one internal shared-string relationship".into(),
        )),
    }
}

/// The worksheets in tab order. Chart, dialog, and macro sheets are skipped,
/// after their names and locations are validated like a worksheet's.
fn sheets(
    archive: &Archive,
    workbook_uri: &str,
    nodes: &[Node],
    sheets_index: usize,
    relationships: &[Relationship],
) -> Result<Vec<DiscoveredSheet>> {
    let relationships_by_id: BTreeMap<&str, &Relationship> = relationships
        .iter()
        .map(|relationship| (relationship.id.as_str(), relationship))
        .collect();
    let mut sheets = Vec::new();
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for (position, node) in (1usize..).zip(sheet_nodes(nodes)) {
        if in_branch(nodes, node) {
            return Err(Error::Unsupported(
                "sheet inside a markup-compatibility branch".into(),
            ));
        }
        if node.parent != Some(sheets_index) {
            return Err(Error::InvalidWorkbook(
                "sheet outside workbook sheets".into(),
            ));
        }
        // A name that cannot be decoded is reported by its sheet's position.
        let name = xml::excel_text(
            node.attribute("", "name")
                .ok_or_else(|| Error::InvalidWorkbook("sheet lacks a name".into()))?,
        )
        .map_err(|error| error.context(format_args!("name of sheet {position}")))?;
        if !names.insert(name.clone()) {
            return Err(Error::InvalidWorkbook("duplicate sheet name".into()));
        }
        let visibility = visibility(node, &name)?;
        let mut ids = sheet_relationship_ids(node);
        let id = match (ids.next(), ids.next()) {
            (Some(id), None) => id,
            _ => {
                return Err(Error::InvalidWorkbook(format!(
                    "sheet {name} needs one relationship id"
                )));
            }
        };
        let relationship = relationships_by_id.get(id).ok_or_else(|| {
            Error::InvalidWorkbook(format!("missing relationship for sheet {name}"))
        })?;
        if relationship.external {
            return Err(Error::Unsupported(
                "external sheets are not supported".into(),
            ));
        }
        if !namespaces::office_type(&relationship.kind, "worksheet") {
            continue;
        }
        let uri = targets::resolve_target(workbook_uri, &relationship.target)?;
        let path = targets::part_path(archive, &uri)?;
        if !archive.contains(&path) {
            return Err(Error::InvalidWorkbook(format!(
                "missing worksheet part {path}"
            )));
        }
        // Earlier duplicate-name checks leave a shared part as the only cause.
        if !paths.insert(path.clone()) {
            return Err(Error::InvalidWorkbook(format!(
                "duplicate worksheet target {path}"
            )));
        }
        sheets.push(DiscoveredSheet {
            sheet: Sheet {
                name,
                path,
                visibility,
            },
            uri,
        });
    }
    Ok(sheets)
}

// The workbook's sheet elements, in the workbook root's namespace.
fn sheet_nodes(nodes: &[Node]) -> impl Iterator<Item = &Node> {
    let namespace = &nodes[0].namespace;
    nodes
        .iter()
        .filter(move |node| node.local == "sheet" && node.namespace == *namespace)
}

// The relationship IDs a sheet element names; a valid sheet names one.
fn sheet_relationship_ids(sheet: &Node) -> impl Iterator<Item = &str> {
    sheet
        .attributes
        .iter()
        .filter(|a| {
            a.local == "id" && matches!(&*a.namespace, OFFICE_NAMESPACE | STRICT_OFFICE_NAMESPACE)
        })
        .map(|a| a.value.as_str())
}

fn visibility(sheet: &Node, name: &str) -> Result<SheetVisibility> {
    // ST_SheetState is an enumeration, which collapses XML whitespace.
    match sheet.attribute("", "state").map(xml::trim_whitespace) {
        None | Some("visible") => Ok(SheetVisibility::Visible),
        Some("hidden") => Ok(SheetVisibility::Hidden),
        Some("veryHidden") => Ok(SheetVisibility::VeryHidden),
        Some(state) => Err(Error::InvalidWorkbook(format!(
            "sheet {name} has an invalid state {state:?}"
        ))),
    }
}

fn compatibility_branch(node: &Node) -> bool {
    &*node.namespace == COMPATIBILITY_NAMESPACE
        && matches!(node.local.as_str(), "Choice" | "Fallback")
}

// Whether a retained node's parent is a retained compatibility branch.
fn in_branch(nodes: &[Node], node: &Node) -> bool {
    node.parent
        .is_some_and(|parent| compatibility_branch(&nodes[parent]))
}

// Misplaced sheet lists and sheets are kept so that they can be rejected;
// unrelated elements, wherever they occur, are not.
pub(super) fn workbook_node(
    depth: usize,
    namespace: &str,
    local: &str,
    parent: Option<&Node>,
) -> bool {
    // Retain only compatibility branches that can substitute direct workbook
    // children or sheets, including nested AlternateContent. Ordinary opaque
    // extensions and their unrelated workbookPr elements stay outside
    // discovery.
    if namespace == COMPATIBILITY_NAMESPACE {
        return match local {
            "AlternateContent" => {
                depth == 1
                    || parent.is_some_and(|node| {
                        compatibility_branch(node)
                            || (node.local == "sheets"
                                && namespaces::main_namespace(&node.namespace))
                    })
            }
            "Choice" | "Fallback" => parent.is_some_and(|node| {
                &*node.namespace == COMPATIBILITY_NAMESPACE && node.local == "AlternateContent"
            }),
            _ => false,
        };
    }
    namespaces::main_namespace(namespace)
        && match local {
            "sheets" | "sheet" => true,
            "workbookPr" => depth == 1 || parent.is_some_and(compatibility_branch),
            _ => false,
        }
}
