//! Validated OOXML package discovery and relationship target resolution.
use super::{Sheet, SheetVisibility, in_part};
use crate::{
    CellRange, CellRef, Error, Result,
    archive::Archive,
    xml::{MAX_DEPTH, excel_text, namespace, trim_whitespace, valid_qname},
};
use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    rc::Rc,
};

const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const SIGNATURE_ORIGIN: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin";
const CONTENT_TYPES_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const STRICT_MAIN_NS: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const STRICT_OFFICE_NS: &str = "http://purl.oclc.org/ooxml/officeDocument/relationships";

pub(super) struct PackageMetadata {
    pub(super) sheets: Vec<Sheet>,
    pub(super) signed: bool,
    pub(super) date1904: bool,
    pub(super) shared_strings_path: Option<String>,
}

pub(super) fn discover(archive: &Archive) -> Result<PackageMetadata> {
    validate_content_types(archive)?;
    let office = office_document(archive)?;
    let workbook = xml_nodes(&archive.read(&office.path)?, workbook_node)?;
    let (sheets_index, date1904) = workbook_properties(&workbook)?;
    let rels_path = relationships_path(&office.path);
    let rels_path = archive.part_name(&rels_path)?.unwrap_or(&rels_path);
    let rels = relationships(&archive.read(rels_path)?)?;
    let shared_strings_path = shared_strings_path(archive, &office.uri, &rels)?;
    let sheets = sheets(archive, &office.uri, &workbook, sheets_index, &rels)?;
    Ok(PackageMetadata {
        sheets,
        signed: office.signed,
        date1904,
        shared_strings_path,
    })
}

/// The header rows of the Excel tables related to a worksheet part. Excel
/// requires each header cell's text to equal its table column's name. Errors
/// name the relationships or table part they concern, not the worksheet.
pub(super) fn table_headers(archive: &Archive, worksheet: &str) -> Result<Vec<CellRange>> {
    let rels_path = relationships_path(worksheet);
    let Some(rels_path) = archive
        .part_name(&rels_path)
        .map_err(|error| in_part(&rels_path, error))?
    else {
        return Ok(Vec::new());
    };
    let in_rels = |error| in_part(rels_path, error);
    let mut headers = Vec::new();
    let relationships = archive
        .read(rels_path)
        .and_then(|xml| relationships(&xml))
        .map_err(in_rels)?;
    for relationship in relationships {
        if !office_type(&relationship.kind, "table") {
            continue;
        }
        if relationship.external {
            return Err(in_rels(Error::InvalidWorkbook(format!(
                "table relationship {} is external",
                relationship.id
            ))));
        }
        let path =
            resolve_part_target(archive, worksheet, &relationship.target).map_err(in_rels)?;
        if !archive.contains(&path) {
            return Err(in_rels(Error::InvalidWorkbook(format!(
                "missing table part {path}"
            ))));
        }
        let header = archive
            .read(&path)
            .and_then(|xml| table_header(&xml))
            .map_err(|error| in_part(&path, error))?;
        headers.extend(header);
    }
    Ok(headers)
}

fn table_header(xml: &[u8]) -> Result<Option<CellRange>> {
    let invalid = |problem: &str| Error::InvalidWorkbook(format!("table {problem}"));
    let nodes = xml_nodes(xml, root_only)?;
    let table = &nodes[0];
    if table.local != "table" || !main_namespace(&table.namespace) {
        return Err(Error::InvalidWorkbook(
            "expected a SpreadsheetML table".into(),
        ));
    }
    let reference = table
        .attr("", "ref")
        .ok_or_else(|| invalid("lacks a ref"))?;
    let range: CellRange = trim_whitespace(reference)
        .parse()
        .map_err(|_| invalid("has an invalid ref"))?;
    let rows = match table.attr("", "headerRowCount") {
        None => 1,
        Some(count) => trim_whitespace(count)
            .parse::<u32>()
            .map_err(|_| invalid("has an invalid headerRowCount"))?,
    };
    if rows > range.height() {
        return Err(invalid("has more header rows than rows"));
    }
    Ok((rows > 0).then(|| {
        let first = range.first();
        let last = CellRef {
            row: first.row + rows - 1,
            column: range.last().column,
        };
        CellRange::new(first, last)
    }))
}

fn validate_content_types(archive: &Archive) -> Result<()> {
    // Like the other OPC parts, the content-types stream is found without
    // regard to ASCII case, and equivalent names are ambiguous.
    let path = archive
        .part_name("[Content_Types].xml")?
        .ok_or_else(|| Error::InvalidWorkbook("missing [Content_Types].xml".into()))?;
    let types = xml_nodes(&archive.read(path)?, root_only)?;
    if types[0].local != "Types" || &*types[0].namespace != CONTENT_TYPES_NS {
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
    let rels_path = archive.part_name("_rels/.rels")?.unwrap_or("_rels/.rels");
    let roots = relationships(&archive.read(rels_path)?)?;
    let mut office = roots
        .iter()
        .filter(|r| office_type(&r.kind, "officeDocument"));
    let office = match (office.next(), office.next()) {
        (Some(relationship), None) if !relationship.external => relationship,
        _ => {
            return Err(Error::InvalidWorkbook(
                "expected one internal officeDocument relationship".into(),
            ));
        }
    };
    let uri = resolve_target("", &office.target)?;
    let path = resolve_part_target(archive, "", &office.target)?;
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
        || archive.entries().iter().any(|e| {
            e.name
                .split_once('/')
                .is_some_and(|(root, _)| root.eq_ignore_ascii_case("_xmlsignatures"))
        })
}

/// Validates the workbook root. Returns the index of its `sheets` element and
/// whether its `workbookPr` element selects the 1904 date system.
fn workbook_properties(nodes: &[Node]) -> Result<(usize, bool)> {
    let root = &nodes[0];
    if root.local != "workbook" || !main_namespace(&root.namespace) {
        return Err(Error::InvalidWorkbook(
            "officeDocument is not a SpreadsheetML workbook".into(),
        ));
    }
    let in_workbook =
        |node: &Node, local: &str| node.local == local && node.namespace == root.namespace;
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
    // Only the root's own workbookPr children are retained.
    let mut properties = nodes.iter().filter(|node| in_workbook(node, "workbookPr"));
    let date1904 = match (properties.next(), properties.next()) {
        (None, _) => false,
        // An xsd:boolean, which collapses surrounding XML whitespace.
        (Some(properties), None) => match properties.attr("", "date1904").map(trim_whitespace) {
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
    rels: &[Relationship],
) -> Result<Option<String>> {
    let mut shared = rels
        .iter()
        .filter(|r| office_type(&r.kind, "sharedStrings"));
    match (shared.next(), shared.next()) {
        (Some(relationship), None) if !relationship.external => {
            resolve_part_target(archive, workbook_uri, &relationship.target).map(Some)
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
    rels: &[Relationship],
) -> Result<Vec<Sheet>> {
    let namespace = &nodes[0].namespace;
    let relationships_by_id: BTreeMap<_, _> = rels.iter().map(|r| (r.id.as_str(), r)).collect();
    let mut sheets = Vec::new();
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for node in nodes {
        if node.local != "sheet" || node.namespace != *namespace {
            continue;
        }
        if node.parent != Some(sheets_index) {
            return Err(Error::InvalidWorkbook(
                "sheet outside workbook sheets".into(),
            ));
        }
        let name = excel_text(
            node.attr("", "name")
                .ok_or_else(|| Error::InvalidWorkbook("sheet lacks a name".into()))?,
        )?;
        if !names.insert(name.clone()) {
            return Err(Error::InvalidWorkbook("duplicate sheet name".into()));
        }
        let visibility = visibility(node, &name)?;
        let mut ids = node
            .attributes
            .iter()
            .filter(|a| a.local == "id" && matches!(&*a.namespace, OFFICE_NS | STRICT_OFFICE_NS));
        let id = match (ids.next(), ids.next()) {
            (Some(attribute), None) => attribute.value.as_str(),
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
        if !office_type(&relationship.kind, "worksheet") {
            continue;
        }
        let path = resolve_part_target(archive, workbook_uri, &relationship.target)?;
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
        sheets.push(Sheet {
            name,
            path,
            visibility,
        });
    }
    Ok(sheets)
}

fn visibility(sheet: &Node, name: &str) -> Result<SheetVisibility> {
    // ST_SheetState is an enumeration, which collapses XML whitespace.
    Ok(match sheet.attr("", "state").map(trim_whitespace) {
        None | Some("visible") => SheetVisibility::Visible,
        Some("hidden") => SheetVisibility::Hidden,
        Some("veryHidden") => SheetVisibility::VeryHidden,
        Some(state) => {
            return Err(Error::InvalidWorkbook(format!(
                "sheet {name} has an invalid state {state:?}"
            )));
        }
    })
}

/// Selects the elements kept from a package part by their depth below the
/// root, namespace URI, and local name. The root is always kept.
type Retain = fn(usize, &str, &str) -> bool;

fn root_only(_: usize, _: &str, _: &str) -> bool {
    false
}

// Misplaced relationships, sheet lists, and sheets are kept so that they can
// be rejected; unrelated elements, wherever they occur, are not.
fn relationship_node(_: usize, namespace: &str, local: &str) -> bool {
    namespace == PACKAGE_NS && local == "Relationship"
}

fn workbook_node(depth: usize, namespace: &str, local: &str) -> bool {
    main_namespace(namespace)
        && match local {
            "sheets" | "sheet" => true,
            "workbookPr" => depth == 1,
            _ => false,
        }
}

struct Attribute {
    namespace: Rc<str>,
    local: String,
    value: String,
}
struct Node {
    namespace: Rc<str>,
    local: String,
    attributes: Vec<Attribute>,
    /// The parent's index when the parent was retained.
    parent: Option<usize>,
}
impl Node {
    fn attr(&self, namespace: &str, local: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|a| &*a.namespace == namespace && a.local == local)
            .map(|a| a.value.as_str())
    }
}

// Normalized namespace URIs by their raw spelling. Parts use few namespaces;
// the limit keeps distinct declarations from growing the cache without bound.
struct Namespaces {
    cache: HashMap<String, Rc<str>>,
    unbound: Rc<str>,
}

impl Namespaces {
    const CACHE_LIMIT: usize = 64;

    fn resolve(&mut self, result: ResolveResult<'_>) -> Result<Rc<str>> {
        match result {
            ResolveResult::Bound(raw) => {
                if let Some(uri) = self.cache.get(raw.as_ref()) {
                    return Ok(Rc::clone(uri));
                }
                let uri: Rc<str> = namespace(ResolveResult::Bound(raw))?.into();
                if self.cache.len() < Self::CACHE_LIMIT {
                    self.cache.insert(raw.as_ref().to_owned(), Rc::clone(&uri));
                }
                Ok(uri)
            }
            other => namespace(other).map(|_| Rc::clone(&self.unbound)),
        }
    }
}

/// Parses and validates a whole package part, keeping its root and the
/// elements that `retain` selects. Memory therefore grows with the retained
/// elements, not with unrelated markup.
fn xml_nodes(bytes: &[u8], retain: Retain) -> Result<Vec<Node>> {
    let xml = std::str::from_utf8(bytes)
        .map_err(|e| Error::Xml(format!("package XML must be UTF-8: {e}")))?;
    if !xml.chars().all(crate::xml::valid_char) {
        return Err(Error::Xml("invalid XML character".into()));
    }
    // After the first byte order mark, U+FEFF is text outside the root. The
    // reader would silently discard a second one.
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    if xml.starts_with('\u{feff}') {
        return Err(Error::Xml("text outside XML root".into()));
    }
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().check_comments = true;
    let mut namespaces = Namespaces {
        cache: HashMap::new(),
        unbound: Rc::from(""),
    };
    let mut nodes = Vec::new();
    // Each open element's index among the retained nodes, if it was retained.
    let mut stack: Vec<Option<usize>> = Vec::new();
    let mut roots = 0;
    let mut declaration_seen = false;
    loop {
        let event_start = reader.buffer_position();
        let event = reader.read_event().map_err(Error::xml)?;
        let is_empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(e) | Event::Empty(e) => {
                if stack.is_empty() {
                    roots += 1;
                }
                if stack.len() >= MAX_DEPTH {
                    return Err(Error::Unsupported(format!(
                        "XML nesting deeper than {MAX_DEPTH} elements"
                    )));
                }
                if !valid_qname(e.name().as_ref()) {
                    return Err(Error::Xml("invalid XML element name".into()));
                }
                crate::xml::validate_namespaces(&e)?;
                crate::xml::validate_attribute_spacing(&e)?;
                let namespace =
                    namespaces.resolve(reader.resolver().resolve_element(e.name()).0)?;
                let local = e.local_name();
                let keep = stack.is_empty() || retain(stack.len(), &namespace, local.as_ref());
                let mut attributes = Vec::new();
                // quick-xml rejects repeated attribute names. Distinct prefixes
                // can still expand to one namespace and local name.
                let mut expanded = HashSet::new();
                for attr in e.attributes() {
                    let attr = attr.map_err(|e| Error::Xml(e.to_string()))?;
                    let name = attr.key.as_ref();
                    if !valid_qname(name) {
                        return Err(Error::Xml("invalid XML attribute name".into()));
                    }
                    if attr.value.contains('<') {
                        return Err(Error::Xml("literal '<' in XML attribute value".into()));
                    }
                    let (resolved, attribute_local) = reader.resolver().resolve_attribute(attr.key);
                    let attribute_namespace = namespaces.resolve(resolved)?;
                    if name.contains(':')
                        && !name.starts_with("xmlns:")
                        && !expanded.insert((
                            Rc::clone(&attribute_namespace),
                            attribute_local.into_inner(),
                        ))
                    {
                        return Err(Error::Xml("duplicate XML attribute".into()));
                    }
                    let value = attr
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(Error::xml)?;
                    if !value.chars().all(crate::xml::valid_char) {
                        return Err(Error::Xml("invalid XML attribute character".into()));
                    }
                    if keep {
                        attributes.push(Attribute {
                            namespace: attribute_namespace,
                            local: attribute_local.into_inner().to_owned(),
                            value: value.into_owned(),
                        });
                    }
                }
                let index = keep.then(|| {
                    nodes.push(Node {
                        namespace,
                        local: local.as_ref().to_owned(),
                        attributes,
                        parent: stack.last().copied().flatten(),
                    });
                    nodes.len() - 1
                });
                if !is_empty {
                    stack.push(index);
                }
            }
            Event::End(_) => {
                if stack.pop().is_none() {
                    return Err(Error::Xml("unexpected closing tag".into()));
                }
            }
            Event::Decl(d) => {
                if event_start != 0 || roots != 0 || declaration_seen {
                    return Err(Error::Xml("misplaced XML declaration".into()));
                }
                declaration_seen = true;
                crate::xml::validate_declaration(&d)?;
            }
            Event::DocType(_) => {
                return Err(Error::Unsupported(
                    "DTD declarations are not supported".into(),
                ));
            }
            Event::Text(e) => {
                if e.as_ref().contains("]]>") {
                    return Err(Error::Xml("CDATA terminator in XML text".into()));
                }
                if stack.is_empty() && !crate::xml::whitespace(e.as_ref()) {
                    return Err(Error::Xml("text outside XML root".into()));
                }
            }
            Event::CData(_) if stack.is_empty() => {
                return Err(Error::Xml("content outside XML root".into()));
            }
            Event::GeneralRef(e) => {
                if stack.is_empty() {
                    return Err(Error::Xml("content outside XML root".into()));
                }
                let reference = e.as_ref();
                let spelling = format!("&{reference};");
                let value = quick_xml::escape::unescape(&spelling)
                    .map_err(|e| Error::Xml(e.to_string()))?;
                if !value.chars().all(crate::xml::valid_char) {
                    return Err(Error::Xml("invalid XML character reference".into()));
                }
            }
            Event::PI(e) => {
                let target = e.target();
                if !crate::xml::valid_name(target) || target.eq_ignore_ascii_case("xml") {
                    return Err(Error::Xml(
                        "invalid XML processing-instruction target".into(),
                    ));
                }
            }
            Event::Eof => break,
            _ => (),
        }
    }
    if roots != 1 || !stack.is_empty() {
        return Err(Error::Xml("expected one complete XML root".into()));
    }
    Ok(nodes)
}

struct Relationship {
    id: String,
    kind: String,
    target: String,
    external: bool,
}
fn relationships(bytes: &[u8]) -> Result<Vec<Relationship>> {
    let nodes = xml_nodes(bytes, relationship_node)?;
    if nodes[0].local != "Relationships" || &*nodes[0].namespace != PACKAGE_NS {
        return Err(Error::InvalidWorkbook("invalid relationships root".into()));
    }
    // Every retained element after the root is a relationship.
    let relationships = &nodes[1..];
    if relationships.iter().any(|node| node.parent != Some(0)) {
        return Err(Error::InvalidWorkbook(
            "relationship outside relationships root".into(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut result = Vec::new();
    for node in relationships {
        let required = |key| {
            node.attr("", key)
                .map(str::to_owned)
                .ok_or_else(|| Error::InvalidWorkbook(format!("relationship lacks {key}")))
        };
        let id = required("Id")?;
        if !ids.insert(id.clone()) {
            return Err(Error::InvalidWorkbook("duplicate relationship id".into()));
        }
        let mode = node.attr("", "TargetMode").unwrap_or("Internal");
        if !matches!(mode, "Internal" | "External") {
            return Err(Error::InvalidWorkbook(
                "invalid relationship TargetMode".into(),
            ));
        }
        result.push(Relationship {
            id,
            kind: required("Type")?,
            target: required("Target")?,
            external: mode == "External",
        });
    }
    Ok(result)
}

fn main_namespace(ns: &str) -> bool {
    matches!(ns, MAIN_NS | STRICT_MAIN_NS)
}
fn office_type(kind: &str, suffix: &str) -> bool {
    kind.rsplit_once('/').is_some_and(|(namespace, local)| {
        local == suffix && matches!(namespace, OFFICE_NS | STRICT_OFFICE_NS)
    })
}
fn relationships_path(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((parent, name)) => format!("{parent}/_rels/{name}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}
fn resolve_target(source: &str, target: &str) -> Result<String> {
    if target.is_empty()
        || target.starts_with("//")
        || target.contains(['\\', '#', '?', '\0'])
        || target
            .split('/')
            .next()
            .is_some_and(|segment| segment.contains(':'))
    {
        return Err(Error::InvalidWorkbook(format!(
            "invalid internal relationship target {target}"
        )));
    }
    let target = escape_path(target)?;
    let source = escape_path(source)?;
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        source
            .rsplit_once('/')
            .map(|(p, _)| p.split('/').collect())
            .unwrap_or_default()
    };
    // Empty URI segments are significant: removing them changes both direct
    // lookup and which segment a following '..' removes. Only the leading '/'
    // denotes the package root. Retain a terminal directory separator after
    // removing '.' or '..', so it cannot alias a file part.
    let mut segments = target
        .strip_prefix('/')
        .unwrap_or(&target)
        .split('/')
        .peekable();
    while let Some(part) = segments.next() {
        match part {
            "." => {
                if segments.peek().is_none() {
                    parts.push("");
                }
            }
            ".." => {
                if parts.pop().is_none() {
                    return Err(Error::InvalidWorkbook(
                        "relationship target escapes package root".into(),
                    ));
                }
                if segments.peek().is_none() {
                    parts.push("");
                }
            }
            value => parts.push(value),
        }
    }
    if parts.is_empty() {
        return Err(Error::InvalidWorkbook(
            "relationship target names no part".into(),
        ));
    }
    if parts.iter().any(|part| part.is_empty()) {
        return Err(Error::InvalidWorkbook(
            "relationship target contains an empty part-name segment".into(),
        ));
    }
    Ok(parts.join("/"))
}

// ZIP item names in OPC retain URI escapes rather than storing decoded file
// system names. Normalize escape spelling and escape raw UTF-8/space bytes.
fn escape_path(path: &str) -> Result<String> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut escaped = String::with_capacity(path.len());
    let mut input = path.as_bytes().iter().copied();
    while let Some(b) = input.next() {
        let (byte, was_escaped) = if b == b'%' {
            let high = input.next().and_then(|c| (c as char).to_digit(16));
            let low = input.next().and_then(|c| (c as char).to_digit(16));
            let value = high
                .zip(low)
                .ok_or_else(|| Error::InvalidWorkbook("invalid percent-encoded target".into()))?;
            let decoded_byte = (value.0 * 16 + value.1) as u8;
            if matches!(decoded_byte, b'/' | b'\\' | b'.' | 0) {
                return Err(Error::InvalidWorkbook(
                    "encoded path separator, dot, or null in target".into(),
                ));
            }
            (decoded_byte, true)
        } else {
            (b, false)
        };
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        let path_character = matches!(
            byte,
            b'/' | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b':'
                | b'='
                | b'@'
        );
        if unreserved || (!was_escaped && path_character) {
            escaped.push(byte as char);
        } else {
            escaped.push('%');
            escaped.push(HEX[(byte >> 4) as usize] as char);
            escaped.push(HEX[(byte & 15) as usize] as char);
        }
    }
    Ok(escaped)
}

fn resolve_part_target(archive: &Archive, source: &str, target: &str) -> Result<String> {
    let path = resolve_target(source, target)?;
    if let Some(name) = archive.part_name(&path)? {
        return Ok(name.to_owned());
    }
    // Earlier versions accepted ZIP names with decoded spaces and Unicode.
    // Keep those packages readable when the canonical encoded part is absent.
    // An encoded name always wins when both spellings occur in the archive.
    let mut decoded = Vec::with_capacity(path.len());
    let mut input = path.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let high = (input.next().expect("validated escape") as char)
                .to_digit(16)
                .unwrap();
            let low = (input.next().expect("validated escape") as char)
                .to_digit(16)
                .unwrap();
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    if let Ok(decoded) = std::str::from_utf8(&decoded)
        && let Some(name) = archive.part_name(decoded)?
    {
        return Ok(name.to_owned());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Workbook;
    #[test]
    fn resolve_relative_and_absolute_part_uris() {
        assert_eq!(
            resolve_target("books/main.xml", "../tabs/sheet%201.xml").unwrap(),
            "tabs/sheet%201.xml"
        );
        assert_eq!(
            resolve_target("books/main.xml", "/xl/worksheets/sheet1.xml").unwrap(),
            "xl/worksheets/sheet1.xml"
        );
        for (target, resolved) in [
            ("../tabs/sheet%20%3f.xml", "tabs/sheet%20%3F.xml"),
            ("../tabs/sheet%25.xml", "tabs/sheet%25.xml"),
            ("../tabs/雪.xml", "tabs/%E9%9B%AA.xml"),
            ("../tabs/sheet:1.xml", "tabs/sheet:1.xml"),
            ("./sheet:1.xml", "books/sheet:1.xml"),
            ("/tabs/sheet:1.xml", "tabs/sheet:1.xml"),
            ("workshee%74s/sheet1.xml", "books/worksheets/sheet1.xml"),
        ] {
            assert_eq!(resolve_target("books/main.xml", target).unwrap(), resolved);
        }
        for target in [
            "../../escape",
            "http://example.com/x",
            "a%2fb",
            "a%5cb",
            "a%00b",
            "%2e%2e/escape",
            "//example.com/a",
            "sheet:1.xml",
            "a%",
            "a\\b",
            "",
        ] {
            assert!(resolve_target("books/main.xml", target).is_err());
        }
    }

    #[test]
    fn relationship_resolution_preserves_empty_segments_and_directory_targets() {
        for target in [
            "tabs//sheet.xml",
            "tabs/sheet.xml/",
            "tabs/sheet.xml/.",
            "tabs/sheet.xml/child/..",
            ".",
            "..",
        ] {
            assert!(
                resolve_target("books/main.xml", target).is_err(),
                "{target}"
            );
        }
        // An empty segment removed by a following '..' does not remove its
        // preceding named segment. RFC URI resolution differs from collapsing
        // every consecutive slash before resolving traversal.
        assert_eq!(
            resolve_target("books/main.xml", "tabs//../sheet.xml").unwrap(),
            "books/tabs/sheet.xml"
        );
    }

    #[test]
    fn workbook_discovery_does_not_alias_invalid_relationship_paths() {
        let archive =
            Archive::new(include_bytes!("../../tests/fixtures/libreoffice.xlsx").to_vec()).unwrap();
        for (part, target, invalid_targets) in [
            (
                "_rels/.rels",
                "xl/workbook.xml",
                [
                    "xl//workbook.xml",
                    "xl/workbook.xml/",
                    "xl/workbook.xml/.",
                    "xl/workbook.xml/child/..",
                ],
            ),
            (
                "xl/_rels/workbook.xml.rels",
                "worksheets/sheet1.xml",
                [
                    "worksheets//sheet1.xml",
                    "worksheets/sheet1.xml/",
                    "worksheets/sheet1.xml/.",
                    "worksheets/sheet1.xml/child/..",
                ],
            ),
        ] {
            let xml = String::from_utf8(archive.read(part).unwrap()).unwrap();
            assert!(xml.contains(target));
            for invalid_target in invalid_targets {
                let changes = BTreeMap::from([(
                    part.to_owned(),
                    xml.replace(target, invalid_target).into_bytes(),
                )]);
                let bytes = archive.write(&changes).unwrap();
                assert!(
                    Workbook::from_bytes(bytes).is_err(),
                    "{part}: {invalid_target}"
                );
            }
        }
    }
    #[test]
    fn malformed_xml_has_no_partial_document() {
        for xml in [
            "<Relationships>",
            "<a/><b/>",
            "<!DOCTYPE a><a/>",
            "<a/><p:b/>",
            "text<a/>",
            "<a><?xml version='1.0'?></a>",
            "<a>&unknown;</a>",
            "<a>&#0;</a>",
            "<a x='&#0;'/>",
            "<a 0bad='one'/>",
            "<a x='<'/>",
            "<a x='one'y='two'/>",
            "<a><0bad/></a>",
            "\u{a0}<a/>",
            "<?0bad instruction?><a/>",
            "<?XML version='1.0'?><a/>",
            " <?xml version='1.0'?><a/>",
            "<!--comment--><?xml version='1.0'?><a/>",
            "<?xml version='1.0' standalone='invalid'?><a/>",
            "<?xml version='1.0' version='1.0'?><a/>",
            "<?xml version='1.0' ignored='one'?><a/>",
            "<?xml version='1.0' standalone='yes' encoding='UTF-8'?><a/>",
            "<?xml version='1.0'encoding='UTF-8'?><a/>",
            // Elements that are not retained are validated all the same.
            "<a xmlns:p='urn:u' xmlns:q='urn:u'><b><c p:x='1' q:x='2'/></b></a>",
            "<a><b><c q:x='1'/></b></a>",
            "<a><b>&#1;</b></a>",
            "\u{feff}\u{feff}<a/>",
        ] {
            assert!(xml_nodes(xml.as_bytes(), root_only).is_err(), "{xml}");
        }
    }

    #[test]
    fn package_xml_errors_use_the_worksheet_parser_variants() {
        let nested = |depth| format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth));
        xml_nodes(nested(256).as_bytes(), root_only).unwrap();
        for (xml, expected) in [
            (nested(257).into_bytes(), Error::Unsupported(String::new())),
            (b"<a>\xff</a>".to_vec(), Error::Xml(String::new())),
            (
                "\u{feff}\u{feff}<a/>".as_bytes().to_vec(),
                Error::Xml(String::new()),
            ),
            (
                b"<?xml version='1.0' encoding='UTF-16'?><a/>".to_vec(),
                Error::Unsupported(String::new()),
            ),
            (
                b"<!DOCTYPE a><a/>".to_vec(),
                Error::Unsupported(String::new()),
            ),
        ] {
            let error = xml_nodes(&xml, root_only).err().unwrap();
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&expected),
                "{error}"
            );
        }
    }

    #[test]
    fn discovery_retains_only_the_elements_it_needs() {
        // Unrelated markup costs no retained memory, wherever it occurs.
        let padding = "<a/><x:sheet x:id='1'/><b><Relationship/></b>".repeat(5_000);
        let types = format!("<Types xmlns='{CONTENT_TYPES_NS}' xmlns:x='urn:x'>{padding}</Types>");
        assert_eq!(xml_nodes(types.as_bytes(), root_only).unwrap().len(), 1);

        let rels = format!(
            "<Relationships xmlns='{PACKAGE_NS}' xmlns:x='urn:x'>{padding}<Relationship Id='a' Type='t' Target='b'/>{padding}</Relationships>"
        )
        .replace("<b><Relationship/></b>", "<b/>");
        let nodes = xml_nodes(rels.as_bytes(), relationship_node).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[1].attr("", "Target"), Some("b"));

        let workbook = format!(
            "<workbook xmlns='{MAIN_NS}' xmlns:x='urn:x'>{padding}<workbookPr date1904='1'/><sheets>{padding}<sheet name='a'/></sheets><extLst><workbookPr/>{padding}<ext><sheet name='b'/></ext></extLst></workbook>"
        );
        let nodes = xml_nodes(workbook.as_bytes(), workbook_node).unwrap();
        let retained: Vec<_> = nodes
            .iter()
            .map(|node| (node.local.as_str(), node.parent))
            .collect();
        // A misplaced sheet is kept, without its unretained parent, so that
        // discovery can reject it.
        assert_eq!(
            retained,
            [
                ("workbook", None),
                ("workbookPr", Some(0)),
                ("sheets", Some(0)),
                ("sheet", Some(2)),
                ("sheet", None),
            ]
        );
    }
    #[test]
    fn complete_xml_supports_bom_declarations_and_unicode_names() {
        for xml in [
            "\u{feff}<?xml version='1.0' encoding='UTF-8' standalone='yes'?><a/>",
            "<?xml version='1.0' standalone='no'?><a/>",
            "<?xml-stylesheet href='style.xsl'?><a xmlns:雪='urn:vendor' 雪:属性='value'/>",
        ] {
            assert!(xml_nodes(xml.as_bytes(), root_only).is_ok(), "{xml}");
        }
    }
}
