//! Validated OOXML package discovery and relationship target resolution.
use super::{Sheet, SheetVisibility, in_part};
use crate::{
    CellRange, CellRef, Error, Result,
    archive::Archive,
    xml::{
        MAX_DEPTH, NamespaceCache, NamespaceReader, excel_text, namespace, trim_whitespace,
        valid_qname,
    },
};
use quick_xml::{
    events::Event,
    name::{QName, ResolveResult},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    ops::Deref,
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
const COMPATIBILITY_NS: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";

pub(super) struct PackageMetadata {
    pub(super) sheets: Vec<DiscoveredSheet>,
    pub(super) signed: bool,
    pub(super) date1904: bool,
    pub(super) shared_strings_path: Option<String>,
}

pub(super) struct DiscoveredSheet {
    pub(super) sheet: Sheet,
    /// Relationships resolve against the URI, even for legacy decoded ZIP names.
    pub(super) uri: String,
}

pub(super) fn discover(archive: &Archive) -> Result<PackageMetadata> {
    validate_content_types(archive)?;
    let office = office_document(archive)?;
    let workbook = xml_nodes(&archive.read(&office.path)?, workbook_node)?;
    let (sheets_index, date1904) = workbook_properties(&workbook)?;
    let rels_path = part_path(archive, &relationships_path(&office.uri))?;
    let rels = relationships(&archive.read(&rels_path)?)?;
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
///
/// `parsed` holds the header of each table part already read, by its ZIP
/// member name, for any worksheet of the archive: a header depends only on its
/// table part, which no edit changes. Parts that fail are not recorded.
pub(super) fn table_headers(
    archive: &Archive,
    worksheet_uri: &str,
    parsed: &mut HashMap<String, Option<CellRange>>,
) -> Result<Vec<CellRange>> {
    let rels_uri = relationships_path(worksheet_uri);
    let rels_path = part_path(archive, &rels_uri).map_err(|error| in_part(&rels_uri, error))?;
    if !archive.contains(&rels_path) {
        return Ok(Vec::new());
    }
    let in_rels = |error| in_part(&rels_path, error);
    let mut headers = Vec::new();
    let mut tables = HashSet::new();
    // Targets already resolved, keyed by the worksheet directory segments they
    // keep and their ASCII-lowercased appended path. Part lookup ignores ASCII
    // case, so spellings with one key name the same part.
    let mut resolved = HashSet::new();
    let mut base = None;
    let relationships = archive
        .read(&rels_path)
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
        // This worksheet's source URI is fixed and can be long. Normalizing a
        // target against its segments costs only the target's length, so only
        // the first relationship with each key builds and looks up the path.
        // Check external mode first, even when the target repeats.
        let target = escaped_target(&relationship.target).map_err(in_rels)?;
        if base.is_none() {
            base = Some(TargetBase::new(worksheet_uri).map_err(in_rels)?);
        }
        let base = base.as_ref().expect("initialized above");
        let (kept, appended) = base.normalize(&target).map_err(in_rels)?;
        let key = (kept, appended.to_ascii_lowercase());
        if resolved.contains(&key) {
            continue;
        }
        let path = part_path(archive, &base.join(kept, &appended)).map_err(in_rels)?;
        if !archive.contains(&path) {
            return Err(in_rels(Error::InvalidWorkbook(format!(
                "missing table part {path}"
            ))));
        }
        resolved.insert(key);
        // Different relationship IDs and URI spellings can name one part.
        // Validate every relationship, but read each resolved table only once.
        if !tables.insert(path.clone()) {
            continue;
        }
        let header = match parsed.get(&path) {
            Some(&header) => header,
            None => {
                let header = archive
                    .read(&path)
                    .and_then(|xml| table_header(&xml))
                    .map_err(|error| in_part(&path, error))?;
                parsed.insert(path, header);
                header
            }
        };
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
    let path = part_path(archive, &uri)?;
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
    // Other spreadsheet readers recognize both dialects in one document.
    // Ignoring the other known namespace could omit a worksheet or silently
    // select the wrong date system.
    if nodes
        .iter()
        .any(|node| main_namespace(&node.namespace) && node.namespace != root.namespace)
    {
        return Err(Error::Unsupported(
            "workbook mixes strict and transitional SpreadsheetML namespaces".into(),
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
) -> Result<Vec<DiscoveredSheet>> {
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
        let uri = resolve_target(workbook_uri, &relationship.target)?;
        let path = part_path(archive, &uri)?;
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
/// root, namespace URI, local name, and retained parent. The root is always kept.
type Retain = fn(usize, &str, &str, Option<&Node>) -> bool;

fn root_only(_: usize, _: &str, _: &str, _: Option<&Node>) -> bool {
    false
}

// Misplaced relationships, sheet lists, and sheets are kept so that they can
// be rejected; unrelated elements, wherever they occur, are not.
fn relationship_node(_: usize, namespace: &str, local: &str, _: Option<&Node>) -> bool {
    namespace == PACKAGE_NS && local == "Relationship"
}

fn compatibility_branch(node: &Node) -> bool {
    &*node.namespace == COMPATIBILITY_NS && matches!(node.local.as_str(), "Choice" | "Fallback")
}

fn workbook_node(depth: usize, namespace: &str, local: &str, parent: Option<&Node>) -> bool {
    // Retain only compatibility branches that can substitute direct workbook
    // children, including nested AlternateContent. Ordinary opaque extensions
    // and their unrelated workbookPr elements stay outside discovery.
    if namespace == COMPATIBILITY_NS {
        return match local {
            "AlternateContent" => depth == 1 || parent.is_some_and(compatibility_branch),
            "Choice" | "Fallback" => parent.is_some_and(|node| {
                &*node.namespace == COMPATIBILITY_NS && node.local == "AlternateContent"
            }),
            _ => false,
        };
    }
    main_namespace(namespace)
        && match local {
            "sheets" | "sheet" => true,
            "workbookPr" => depth == 1 || parent.is_some_and(compatibility_branch),
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

// Hash a URI once per binding, including when checking expanded attribute
// names. The fingerprint speeds lookup; exact URI equality still decides it.
#[derive(Clone)]
struct NamespaceName {
    uri: Rc<str>,
    fingerprint: u64,
}

impl NamespaceName {
    fn new(uri: Rc<str>) -> Self {
        let mut hash = DefaultHasher::new();
        uri.hash(&mut hash);
        Self {
            uri,
            fingerprint: hash.finish(),
        }
    }
}

impl Deref for NamespaceName {
    type Target = str;

    fn deref(&self) -> &str {
        &self.uri
    }
}

impl PartialEq for NamespaceName {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
            && (Rc::ptr_eq(&self.uri, &other.uri) || self.uri == other.uri)
    }
}

impl Eq for NamespaceName {}

impl Hash for NamespaceName {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fingerprint.hash(state);
    }
}

// Normalized namespace URIs by their raw spelling. Parts use few namespaces;
// the limit keeps distinct declarations from growing the cache without bound.
struct Namespaces {
    cache: HashMap<String, NamespaceName>,
    bindings: NamespaceCache<NamespaceName>,
    unbound: NamespaceName,
}

impl Namespaces {
    const CACHE_LIMIT: usize = 64;

    fn resolve(
        &mut self,
        reader: &NamespaceReader<'_>,
        name: QName<'_>,
        attribute: bool,
    ) -> Result<NamespaceName> {
        if let Some(uri) = self.bindings.get(reader, name, attribute) {
            return Ok(uri);
        }
        let result = if attribute {
            reader.resolver().resolve_attribute(name).0
        } else {
            reader.resolver().resolve_element(name).0
        };
        let uri = match result {
            ResolveResult::Bound(raw) => {
                if let Some(uri) = self.cache.get(raw.as_ref()) {
                    uri.clone()
                } else {
                    let uri = NamespaceName::new(namespace(ResolveResult::Bound(raw))?.into());
                    // Reuse equivalent escaped spellings within the bounded
                    // cache, so aliases share the cheap pointer equality path.
                    let uri = self
                        .cache
                        .values()
                        .find(|cached| **cached == uri)
                        .cloned()
                        .unwrap_or(uri);
                    if self.cache.len() < Self::CACHE_LIMIT {
                        self.cache.insert(raw.as_ref().to_owned(), uri.clone());
                    }
                    uri
                }
            }
            other => {
                namespace(other)?;
                self.unbound.clone()
            }
        };
        self.bindings.insert(reader, name, attribute, uri.clone());
        Ok(uri)
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
    let mut reader = NamespaceReader::from_str(xml);
    reader.config_mut().check_comments = true;
    let mut namespaces = Namespaces {
        cache: HashMap::new(),
        bindings: NamespaceCache::default(),
        unbound: NamespaceName::new(Rc::from("")),
    };
    let mut nodes = Vec::new();
    // Each open element's index among the retained nodes, if it was retained.
    let mut stack: Vec<Option<usize>> = Vec::new();
    let mut roots = 0;
    let mut declaration_seen = false;
    let mut has_process_content = false;
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
                let namespace = namespaces.resolve(&reader, e.name(), false)?;
                let local = e.local_name();
                let parent = stack.last().copied().flatten();
                let keep = stack.is_empty()
                    || retain(
                        stack.len(),
                        &namespace,
                        local.as_ref(),
                        parent.map(|index| &nodes[index]),
                    );
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
                    let attribute_local = attr.key.local_name();
                    let attribute_namespace = namespaces.resolve(&reader, attr.key, true)?;
                    if name.contains(':')
                        && !name.starts_with("xmlns:")
                        && !expanded
                            .insert((attribute_namespace.clone(), attribute_local.into_inner()))
                    {
                        return Err(Error::Xml("duplicate XML attribute".into()));
                    }
                    let value = attr
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(Error::xml)?;
                    if !value.chars().all(crate::xml::valid_char) {
                        return Err(Error::Xml("invalid XML attribute character".into()));
                    }
                    has_process_content |= &*attribute_namespace == COMPATIBILITY_NS
                        && attribute_local.as_ref() == "ProcessContent"
                        && !trim_whitespace(&value).is_empty();
                    if keep {
                        attributes.push(Attribute {
                            namespace: attribute_namespace.uri,
                            local: attribute_local.into_inner().to_owned(),
                            value: value.into_owned(),
                        });
                    }
                }
                let index = keep.then(|| {
                    nodes.push(Node {
                        namespace: namespace.uri,
                        local: local.as_ref().to_owned(),
                        attributes,
                        parent,
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
                crate::xml::reference(&e)?;
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
    if has_process_content && nodes[0].local == "workbook" && main_namespace(&nodes[0].namespace) {
        reject_promoted_workbook_properties(xml)?;
    }
    Ok(nodes)
}

// ProcessContent can promote children of an ignored vendor wrapper into the
// workbook. Inspect only documents using it, after complete XML validation.
// This follows possible workbook-level paths, not a general MC processor.
#[derive(Default)]
struct CompatibilityNames {
    names: HashMap<NamespaceName, HashSet<Rc<str>>>,
    added: Vec<(NamespaceName, Rc<str>)>,
}

impl CompatibilityNames {
    fn insert(&mut self, namespace: NamespaceName, local: &str) {
        let names = self.names.entry(namespace.clone()).or_default();
        if !names.contains(local) {
            let local: Rc<str> = local.into();
            names.insert(Rc::clone(&local));
            self.added.push((namespace, local));
        }
    }

    fn matches(&self, namespace: &NamespaceName, local: &str) -> bool {
        self.names
            .get(namespace)
            .is_some_and(|names| names.contains(local) || names.contains("*"))
    }

    fn truncate(&mut self, length: usize) {
        while self.added.len() > length {
            let (namespace, local) = self.added.pop().unwrap();
            let names = self.names.get_mut(&namespace).unwrap();
            names.remove(&local);
            if names.is_empty() {
                self.names.remove(&namespace);
            }
        }
    }
}

fn reject_promoted_workbook_properties(xml: &str) -> Result<()> {
    #[derive(Clone, Copy)]
    enum Parent {
        Workbook,
        Promoted,
        AlternateContent(bool),
        Opaque,
    }
    let mut reader = NamespaceReader::from_str(xml);
    let mut namespaces = Namespaces {
        cache: HashMap::new(),
        bindings: NamespaceCache::default(),
        unbound: NamespaceName::new(Rc::from("")),
    };
    // Namespace / expanded-name lookup stays bounded even for long inherited
    // rule lists. Only newly inserted names need removing when a scope closes.
    let mut rules: [CompatibilityNames; 2] = Default::default();
    let mut stack: Vec<(Parent, [usize; 2])> = Vec::new();
    loop {
        let event = reader.read_event().map_err(Error::xml)?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(e) | Event::Empty(e) => {
                let parent = stack.last().map(|frame| frame.0);
                let checkpoint = rules.each_ref().map(|names| names.added.len());
                let ns = namespaces.resolve(&reader, e.name(), false)?;
                let local = e.local_name();
                // Rules on the wrapper itself apply before deciding whether
                // to ignore or process that wrapper.
                if !matches!(parent, Some(Parent::Opaque)) {
                    for attr in e.attributes() {
                        let attr = attr.map_err(|error| Error::Xml(error.to_string()))?;
                        let name = attr.key.local_name();
                        if &*namespaces.resolve(&reader, attr.key, true)? != COMPATIBILITY_NS {
                            continue;
                        }
                        let name = name.as_ref();
                        let kind = match name {
                            "Ignorable" => 0,
                            "ProcessContent" => 1,
                            _ => continue,
                        };
                        let value = attr
                            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                            .map_err(Error::xml)?;
                        for token in value
                            .split([' ', '\t', '\r', '\n'])
                            .filter(|v| !v.is_empty())
                        {
                            let qualified;
                            let token = if name == "Ignorable" {
                                qualified = format!("{token}:_");
                                qualified.as_str()
                            } else {
                                token
                            };
                            let (resolved, local) = reader.resolver().resolve_element(QName(token));
                            // An unbound directive cannot name a valid
                            // prefixed element in this already validated XML.
                            if !matches!(resolved, ResolveResult::Bound(_)) {
                                continue;
                            }
                            let uri = namespaces.resolve(&reader, QName(token), false)?;
                            rules[kind].insert(uri, if kind == 0 { "*" } else { local.as_ref() });
                        }
                    }
                }
                let current = match parent {
                    None => Parent::Workbook,
                    Some(Parent::Workbook | Parent::Promoted) => {
                        if matches!(parent, Some(Parent::Promoted))
                            && main_namespace(&ns)
                            && local.as_ref() == "workbookPr"
                        {
                            return Err(Error::Unsupported(
                                "workbookPr inside an mc:ProcessContent wrapper may change the date system"
                                    .into(),
                            ));
                        }
                        if &*ns == COMPATIBILITY_NS && local.as_ref() == "AlternateContent" {
                            Parent::AlternateContent(matches!(parent, Some(Parent::Promoted)))
                        } else if !main_namespace(&ns)
                            && &*ns != COMPATIBILITY_NS
                            && rules[0].matches(&ns, "*")
                            && rules[1].matches(&ns, local.as_ref())
                        {
                            Parent::Promoted
                        } else {
                            Parent::Opaque
                        }
                    }
                    Some(Parent::AlternateContent(promoted))
                        if &*ns == COMPATIBILITY_NS
                            && matches!(local.as_ref(), "Choice" | "Fallback") =>
                    {
                        if promoted {
                            Parent::Promoted
                        } else {
                            Parent::Workbook
                        }
                    }
                    _ => Parent::Opaque,
                };
                if empty {
                    for (names, length) in rules.iter_mut().zip(checkpoint) {
                        names.truncate(length);
                    }
                } else {
                    stack.push((current, checkpoint));
                }
            }
            Event::End(_) => {
                let (_, checkpoint) = stack.pop().expect("validated XML is balanced");
                for (names, length) in rules.iter_mut().zip(checkpoint) {
                    names.truncate(length);
                }
            }
            Event::Eof => return Ok(()),
            _ => (),
        }
    }
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
    let target = escaped_target(target)?;
    let base = TargetBase::new(source)?;
    let (kept, appended) = base.normalize(&target)?;
    Ok(base.join(kept, &appended))
}

// Validates an internal target's syntax and normalizes its escapes.
fn escaped_target(target: &str) -> Result<String> {
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
    escape_path(target)
}

/// The escaped directory segments of a source part, against which its
/// relationship targets resolve.
struct TargetBase {
    segments: Vec<String>,
    first_empty: Option<usize>,
}

impl TargetBase {
    fn new(source: &str) -> Result<Self> {
        let source = escape_path(source)?;
        let segments: Vec<String> = source
            .rsplit_once('/')
            .map(|(directory, _)| directory.split('/').map(str::to_owned).collect())
            .unwrap_or_default();
        let first_empty = segments.iter().position(String::is_empty);
        Ok(Self {
            segments,
            first_empty,
        })
    }

    /// The number of leading directory segments an escaped target keeps, and
    /// the path it appends to them. This costs the target's length, not the
    /// source's.
    fn normalize(&self, target: &str) -> Result<(usize, String)> {
        let mut kept = if target.starts_with('/') {
            0
        } else {
            self.segments.len()
        };
        let mut parts: Vec<&str> = Vec::new();
        // Empty URI segments are significant: removing them changes both direct
        // lookup and which segment a following '..' removes. Only the leading '/'
        // denotes the package root. Retain a terminal directory separator after
        // removing '.' or '..', so it cannot alias a file part.
        let mut segments = target
            .strip_prefix('/')
            .unwrap_or(target)
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
                    // Appended segments are removed before kept ones.
                    if parts.pop().is_none() {
                        if kept == 0 {
                            return Err(Error::InvalidWorkbook(
                                "relationship target escapes package root".into(),
                            ));
                        }
                        kept -= 1;
                    }
                    if segments.peek().is_none() {
                        parts.push("");
                    }
                }
                value => parts.push(value),
            }
        }
        if kept == 0 && parts.is_empty() {
            return Err(Error::InvalidWorkbook(
                "relationship target names no part".into(),
            ));
        }
        if self.first_empty.is_some_and(|index| index < kept) || parts.iter().any(|p| p.is_empty())
        {
            return Err(Error::InvalidWorkbook(
                "relationship target contains an empty part-name segment".into(),
            ));
        }
        Ok((kept, parts.join("/")))
    }

    fn join(&self, kept: usize, appended: &str) -> String {
        let mut path = String::new();
        for segment in &self.segments[..kept] {
            path.push_str(segment);
            path.push('/');
        }
        if appended.is_empty() {
            path.pop();
        } else {
            path.push_str(appended);
        }
        path
    }
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
    part_path(archive, &path)
}

// Resolve a normalized part URI to its actual ZIP member name. Relationship
// parts use the same encoded-first lookup as the parts they describe.
fn part_path(archive: &Archive, path: &str) -> Result<String> {
    if let Some(name) = archive.part_name(path)? {
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
    Ok(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Workbook;

    #[test]
    fn namespace_fingerprint_collisions_keep_exact_uri_equality() {
        let first = NamespaceName::new(Rc::from("urn:first"));
        let collision = NamespaceName {
            uri: Rc::from("urn:other"),
            fingerprint: first.fingerprint,
        };
        let mut attributes = HashSet::new();
        assert!(attributes.insert((first, "value")));
        assert!(attributes.insert((collision, "value")));
        assert!(!attributes.insert((NamespaceName::new(Rc::from("urn:first")), "value")));
    }

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

    // The resolver before targets were normalized against a reusable base.
    fn reference_resolve_target(source: &str, target: &str) -> Result<String> {
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

    #[test]
    fn resolution_against_a_target_base_matches_the_reference_resolver() {
        let sources = [
            "",
            "a.xml",
            "xl/workbook.xml",
            "xl/worksheets/sheet1.xml",
            "a/b/c/d.xml",
            "x%20y/%C3%A9/s.xml",
            "a/b/",
            "a//b/c.xml",
        ];
        let atoms = [
            "a", "B", ".", "..", "", "%41", "%62", "c%20d", "t.xml", "%C3%A9", "x:y", "%2F", "%",
            "~", "A",
        ];
        let mut state: u64 = 0x1234_5678;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as usize
        };
        for _ in 0..100_000 {
            let source = sources[next() % sources.len()];
            let mut target = String::new();
            if next() % 5 == 0 {
                target.push('/');
            }
            for index in 0..1 + next() % 6 {
                if index > 0 {
                    target.push('/');
                }
                target.push_str(atoms[next() % atoms.len()]);
            }
            assert_eq!(
                resolve_target(source, &target).map_err(|error| error.to_string()),
                reference_resolve_target(source, &target).map_err(|error| error.to_string()),
                "source {source:?}, target {target:?}"
            );
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
            "<a>]]></a>",
            "<a/>&amp;",
            "<a/><![CDATA[x]]>",
            "<a x='&#1;'/>",
            "<a>\u{1}</a>",
            "<a></b>",
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
