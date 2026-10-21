//! Validated OOXML package discovery and relationship target resolution.
use super::{Sheet, SheetVisibility, in_part};
use crate::{
    CellRange, CellRef, Error, Result,
    archive::Archive,
    xml::{
        AttributeChecks, Attributes, DocumentRules, NamespaceId, NamespacePool, NamespaceReader,
        attribute_spans, excel_text, trim_whitespace,
    },
};
use quick_xml::{
    events::Event,
    name::{QName, ResolveResult},
};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ops::Range,
    rc::Rc,
    sync::Arc,
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
    let workbook = {
        let xml = archive.read(&office.path)?;
        let part = part_xml(&xml, workbook_node)?;
        let root = &part.nodes[0];
        if part.process_content && root.local == "workbook" && main_namespace(&root.namespace) {
            reject_promoted_workbook_properties(&xml)?;
        }
        part.nodes
    };
    let (sheets_index, date1904) = workbook_properties(&workbook)?;
    let rels_path = part_path(archive, &relationships_path(&office.uri))?;
    // Discovery uses only the shared-string relationships and those that
    // sheets name.
    let sheet_ids: HashSet<&str> = sheet_nodes(&workbook)
        .flat_map(sheet_relationship_ids)
        .collect();
    let rels = relationships(&archive.read(&rels_path)?, &|id, kind| {
        office_type(kind, "sharedStrings") || sheet_ids.contains(id)
    })?;
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
        .and_then(|xml| relationships(&xml, &|_, kind| office_type(kind, "table")))
        .map_err(in_rels)?;
    for relationship in relationships {
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
    // Only the office document and a signature origin are used.
    let roots = relationships(&archive.read(rels_path)?, &|_, kind| {
        office_type(kind, "officeDocument") || kind == SIGNATURE_ORIGIN
    })?;
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
    let relationships_by_id: BTreeMap<_, _> = rels.iter().map(|r| (r.id.as_str(), r)).collect();
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
        let name = excel_text(
            node.attr("", "name")
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
        .filter(|a| a.local == "id" && matches!(&*a.namespace, OFFICE_NS | STRICT_OFFICE_NS))
        .map(|a| a.value.as_str())
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

fn compatibility_branch(node: &Node) -> bool {
    &*node.namespace == COMPATIBILITY_NS && matches!(node.local.as_str(), "Choice" | "Fallback")
}

// Whether a retained node's parent is a retained compatibility branch.
fn in_branch(nodes: &[Node], node: &Node) -> bool {
    node.parent
        .is_some_and(|parent| compatibility_branch(&nodes[parent]))
}

// Misplaced sheet lists and sheets are kept so that they can be rejected;
// unrelated elements, wherever they occur, are not.
fn workbook_node(depth: usize, namespace: &str, local: &str, parent: Option<&Node>) -> bool {
    // Retain only compatibility branches that can substitute direct workbook
    // children or sheets, including nested AlternateContent. Ordinary opaque
    // extensions and their unrelated workbookPr elements stay outside
    // discovery.
    if namespace == COMPATIBILITY_NS {
        return match local {
            "AlternateContent" => {
                depth == 1
                    || parent.is_some_and(|node| {
                        compatibility_branch(node)
                            || (node.local == "sheets" && main_namespace(&node.namespace))
                    })
            }
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
    namespace: Arc<str>,
    local: String,
    value: String,
}
struct Node {
    namespace: Arc<str>,
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

/// A package part's retained elements, and whether any element of the part
/// has a non-empty mc:ProcessContent attribute.
struct PartXml {
    nodes: Vec<Node>,
    process_content: bool,
}

// Parts use few namespaces. Past this many spellings a parser that keeps no
// namespace IDs clears its pool, so that distinct declarations cannot grow it
// without bound.
const NAMESPACE_LIMIT: usize = 64;

// The most attribute records and namespace spellings that package parsing has
// held at once on this thread, for tests of its memory bounds.
#[cfg(test)]
thread_local! {
    static ATTRIBUTE_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SPELLING_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_peak(peak: &'static std::thread::LocalKey<std::cell::Cell<usize>>, value: usize) {
    peak.set(peak.get().max(value));
}

fn xml_nodes(bytes: &[u8], retain: Retain) -> Result<Vec<Node>> {
    part_xml(bytes, retain).map(|part| part.nodes)
}

/// Parses and validates a whole package part, keeping its root and the
/// elements that `retain` selects when they start. Memory therefore grows
/// with the retained elements, not with unrelated markup.
fn part_xml(bytes: &[u8], retain: Retain) -> Result<PartXml> {
    let mut nodes: Vec<Node> = Vec::new();
    // Each open element's index among the retained nodes, if it was retained.
    let process_content = walk_part(bytes, &mut |tag, parent: Option<&Option<usize>>| {
        let parent = parent.copied().flatten();
        let keep = tag.depth == 0
            || retain(
                tag.depth,
                tag.namespace(),
                tag.local,
                parent.map(|index| &nodes[index]),
            );
        if !keep {
            return Ok(None);
        }
        let mut attributes = Vec::new();
        for attribute in tag.spans.clone().map(|index| tag.attributes.view(index)) {
            let qualified = QName(attribute.name);
            // Namespace declarations are not attributes of a node.
            if qualified.as_namespace_binding().is_some() {
                continue;
            }
            let id = tag.namespaces.resolve(tag.reader, qualified, true)?;
            attributes.push(Attribute {
                namespace: Arc::clone(tag.namespaces.uri(id)),
                local: qualified.local_name().into_inner().to_owned(),
                value: attribute.value.to_owned(),
            });
        }
        nodes.push(Node {
            namespace: Arc::clone(tag.namespaces.uri(tag.namespace)),
            local: tag.local.to_owned(),
            attributes,
            parent,
        });
        Ok(Some(nodes.len() - 1))
    })?;
    Ok(PartXml {
        nodes,
        process_content,
    })
}

/// A start tag that `walk_part` passes on, with its scanned attributes and
/// the namespaces in scope. No namespace ID outlives the tag.
struct Tag<'t, 'xml> {
    /// How many elements enclose this one: the root's depth is zero.
    depth: usize,
    namespace: NamespaceId,
    local: &'t str,
    namespaces: &'t mut NamespacePool,
    reader: &'t NamespaceReader<'xml>,
    attributes: &'t Attributes<'xml>,
    spans: Range<usize>,
}

impl<'xml> Tag<'_, 'xml> {
    fn namespace(&self) -> &str {
        self.namespaces.uri(self.namespace)
    }

    /// The value of the unprefixed attribute `name`, which has no namespace,
    /// borrowed from the part unless XML normalization changed it.
    fn attr(&self, name: &str) -> Option<Cow<'xml, str>> {
        self.attributes.find_value(self.spans.clone(), name)
    }
}

/// Parses and validates a whole package part with the worksheet parser's
/// rules. Each start tag goes to `visit` with what `visit` returned for its
/// parent, if any; the value is kept while the element is open. Returns
/// whether any element of the part has a non-empty mc:ProcessContent
/// attribute.
fn walk_part<'xml, T>(
    bytes: &'xml [u8],
    visit: &mut dyn FnMut(&mut Tag<'_, 'xml>, Option<&T>) -> Result<T>,
) -> Result<bool> {
    let (source, mut rules) = DocumentRules::source(bytes)?;
    let mut reader = rules.reader(source);
    let mut namespaces = NamespacePool::new();
    // One tag's attributes at a time.
    let mut attributes = Attributes::new(source);
    let mut checks = AttributeChecks::default();
    let mut stack: Vec<T> = Vec::new();
    let mut process_content = false;
    loop {
        let start = rules.position(&reader);
        let event = reader.read_event().map_err(Error::xml)?;
        let end = rules.position(&reader);
        match event {
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                let name = tag.name();
                rules.start_element(stack.len(), name.as_ref())?;
                let namespace = namespaces.resolve(&reader, name, false)?;
                attributes.clear();
                let spans = attribute_spans(
                    start + 1 + name.as_ref().len(),
                    end,
                    &mut attributes,
                    &mut namespaces,
                    &reader,
                    &mut checks,
                )?;
                // The part's attributes matter as a whole only as
                // mc:ProcessContent.
                for attribute in spans.clone().map(|index| attributes.view(index)) {
                    let qualified = QName(attribute.name);
                    if qualified.local_name().into_inner() != "ProcessContent"
                        || qualified.as_namespace_binding().is_some()
                    {
                        continue;
                    }
                    let id = namespaces.resolve(&reader, qualified, true)?;
                    process_content |= &**namespaces.uri(id) == COMPATIBILITY_NS
                        && !trim_whitespace(attribute.value).is_empty();
                }
                let mut tag = Tag {
                    depth: stack.len(),
                    namespace,
                    local: tag.local_name().into_inner(),
                    namespaces: &mut namespaces,
                    reader: &reader,
                    attributes: &attributes,
                    spans,
                };
                let value = visit(&mut tag, stack.last())?;
                if matches!(event, Event::Start(_)) {
                    stack.push(value);
                }
                #[cfg(test)]
                {
                    record_peak(&ATTRIBUTE_PEAK, attributes.records_len());
                    record_peak(&SPELLING_PEAK, namespaces.spellings());
                }
                // Visitors keep URIs, not IDs, past their tags.
                if namespaces.spellings() > NAMESPACE_LIMIT {
                    namespaces.clear();
                }
            }
            Event::End(_) => {
                if stack.pop().is_none() {
                    return Err(Error::Xml("unmatched closing XML tag".into()));
                }
            }
            Event::Eof => break,
            _ => rules.event(&event, start, stack.len())?,
        }
    }
    rules.finish(stack.len())?;
    Ok(process_content)
}

// ProcessContent can promote children of an ignored vendor wrapper into the
// workbook. Inspect only documents using it, after complete XML validation.
// This follows possible workbook-level paths, not a general MC processor.
#[derive(Default)]
struct CompatibilityNames {
    names: HashMap<NamespaceId, HashSet<Rc<str>>>,
    added: Vec<(NamespaceId, Rc<str>)>,
}

impl CompatibilityNames {
    fn insert(&mut self, namespace: NamespaceId, local: &str) {
        let names = self.names.entry(namespace).or_default();
        if !names.contains(local) {
            let local: Rc<str> = local.into();
            names.insert(Rc::clone(&local));
            self.added.push((namespace, local));
        }
    }

    fn matches(&self, namespace: NamespaceId, local: &str) -> bool {
        self.names
            .get(&namespace)
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

    // Moves every name to its namespace's ID in a newly cleared pool. The
    // names keep their order, so earlier checkpoints remain valid.
    fn renumber(&mut self, previous: &NamespacePool, pool: &mut NamespacePool) -> Result<()> {
        self.names.clear();
        for (namespace, local) in &mut self.added {
            *namespace = pool.intern(previous.uri(*namespace))?;
            self.names
                .entry(*namespace)
                .or_default()
                .insert(Rc::clone(local));
        }
        Ok(())
    }
}

fn reject_promoted_workbook_properties(xml: &[u8]) -> Result<()> {
    #[derive(Clone, Copy)]
    enum Parent {
        Workbook,
        Promoted,
        AlternateContent(bool),
        Opaque,
    }
    let mut reader = NamespaceReader::from_reader(xml);
    let mut namespaces = NamespacePool::new();
    // The pool in use before the last restart, kept for its allocations.
    let mut previous = NamespacePool::new();
    // Namespace / expanded-name lookup stays bounded even for long inherited
    // rule lists. Only newly inserted names need removing when a scope closes.
    let mut rules: [CompatibilityNames; 2] = Default::default();
    let mut stack: Vec<(Parent, [usize; 2])> = Vec::new();
    loop {
        let event = reader.read_event().map_err(Error::xml)?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(e) | Event::Empty(e) => {
                // As in part_xml, the pool starts over past a limit. The rules
                // in scope hold IDs, so they move to the other pool, and the
                // limit grows with them to keep that work linear.
                let live: usize = rules.iter().map(|names| names.added.len()).sum();
                #[cfg(test)]
                record_peak(&SPELLING_PEAK, namespaces.spellings());
                if namespaces.spellings() > NAMESPACE_LIMIT + live {
                    previous.clear();
                    std::mem::swap(&mut namespaces, &mut previous);
                    for names in &mut rules {
                        names.renumber(&previous, &mut namespaces)?;
                    }
                }
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
                        let attribute_namespace = namespaces.resolve(&reader, attr.key, true)?;
                        if &**namespaces.uri(attribute_namespace) != COMPATIBILITY_NS {
                            continue;
                        }
                        let name = name.as_ref();
                        let kind = match name {
                            "Ignorable" => 0,
                            "ProcessContent" => 1,
                            _ => continue,
                        };
                        let value = crate::xml::attribute_value(&attr)?;
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
                let uri: &str = namespaces.uri(ns);
                let current = match parent {
                    None => Parent::Workbook,
                    Some(Parent::Workbook | Parent::Promoted) => {
                        if matches!(parent, Some(Parent::Promoted))
                            && main_namespace(uri)
                            && local.as_ref() == "workbookPr"
                        {
                            return Err(Error::Unsupported(
                                "workbookPr inside an mc:ProcessContent wrapper may change the date system"
                                    .into(),
                            ));
                        }
                        if uri == COMPATIBILITY_NS && local.as_ref() == "AlternateContent" {
                            Parent::AlternateContent(matches!(parent, Some(Parent::Promoted)))
                        } else if !main_namespace(uri)
                            && uri != COMPATIBILITY_NS
                            && rules[0].matches(ns, "*")
                            && rules[1].matches(ns, local.as_ref())
                        {
                            Parent::Promoted
                        } else {
                            Parent::Opaque
                        }
                    }
                    Some(Parent::AlternateContent(promoted))
                        if uri == COMPATIBILITY_NS
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

/// Validates a whole relationships part and returns, in document order, the
/// relationships that `keep` selects by their ID and type. The others are
/// not copied: only their IDs are kept while the part is parsed, to find
/// duplicates, borrowed from `bytes` unless XML normalization changed them.
fn relationships(bytes: &[u8], keep: &dyn Fn(&str, &str) -> bool) -> Result<Vec<Relationship>> {
    // Structural errors are reported only once the whole part is known to be
    // well-formed: an invalid root, then a relationship outside it, then the
    // first relationship with invalid attributes.
    let mut valid_root = false;
    let mut misplaced = false;
    let mut invalid = None;
    let mut ids = HashSet::new();
    let mut kept = Vec::new();
    walk_part(bytes, &mut |tag, _: Option<&()>| {
        if tag.depth == 0 {
            valid_root = tag.local == "Relationships" && tag.namespace() == PACKAGE_NS;
        } else if valid_root && tag.local == "Relationship" && tag.namespace() == PACKAGE_NS {
            if tag.depth > 1 {
                misplaced = true;
            } else if !misplaced && invalid.is_none() {
                match relationship(tag, &ids, keep) {
                    Ok((id, relationship)) => {
                        kept.extend(relationship);
                        ids.insert(id);
                    }
                    Err(error) => invalid = Some(error),
                }
            }
        }
        Ok(())
    })?;
    if !valid_root {
        return Err(Error::InvalidWorkbook("invalid relationships root".into()));
    }
    if misplaced {
        return Err(Error::InvalidWorkbook(
            "relationship outside relationships root".into(),
        ));
    }
    match invalid {
        Some(error) => Err(error),
        None => Ok(kept),
    }
}

// Checks a relationship element's attributes against the IDs before it.
// Returns its ID, and the relationship if `keep` selects it.
fn relationship<'xml>(
    tag: &Tag<'_, 'xml>,
    ids: &HashSet<Cow<'xml, str>>,
    keep: &dyn Fn(&str, &str) -> bool,
) -> Result<(Cow<'xml, str>, Option<Relationship>)> {
    let required = |key| {
        tag.attr(key)
            .ok_or_else(|| Error::InvalidWorkbook(format!("relationship lacks {key}")))
    };
    let id = required("Id")?;
    if ids.contains(&*id) {
        return Err(Error::InvalidWorkbook("duplicate relationship id".into()));
    }
    let external = match tag.attr("TargetMode").as_deref() {
        None | Some("Internal") => false,
        Some("External") => true,
        Some(_) => {
            return Err(Error::InvalidWorkbook(
                "invalid relationship TargetMode".into(),
            ));
        }
    };
    let kind = required("Type")?;
    let target = required("Target")?;
    let relationship = keep(&id, &kind).then(|| Relationship {
        id: id.to_string(),
        kind: kind.into_owned(),
        target: target.into_owned(),
        external,
    });
    Ok((id, relationship))
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

    // Relationship elements wherever they occur, so that misplaced ones can
    // be rejected.
    fn relationship_node(_: usize, namespace: &str, local: &str, _: Option<&Node>) -> bool {
        namespace == PACKAGE_NS && local == "Relationship"
    }

    // The relationships parser before it validated relationships as they
    // stream: it retained every relationship element, then checked them.
    fn reference_relationships(bytes: &[u8]) -> Result<Vec<Relationship>> {
        let nodes = xml_nodes(bytes, relationship_node)?;
        if nodes[0].local != "Relationships" || &*nodes[0].namespace != PACKAGE_NS {
            return Err(Error::InvalidWorkbook("invalid relationships root".into()));
        }
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

    fn expected_relationship(id: &str, kind: &str, target: &str, external: bool) -> Relationship {
        Relationship {
            id: id.into(),
            kind: kind.into(),
            target: target.into(),
            external,
        }
    }

    type Summary = std::result::Result<Vec<(String, String, String, bool)>, String>;

    fn summary(relationships: Result<Vec<Relationship>>) -> Summary {
        relationships
            .map(|relationships| {
                relationships
                    .into_iter()
                    .map(|r| (r.id, r.kind, r.target, r.external))
                    .collect()
            })
            .map_err(|error| format!("{error:?}"))
    }

    #[test]
    fn streamed_relationships_match_the_retaining_reference() {
        let roots = [
            (
                format!("<Relationships xmlns='{PACKAGE_NS}'>"),
                "</Relationships>",
            ),
            (
                format!(
                    "<?xml version='1.0'?><r:Relationships xmlns:r='{PACKAGE_NS}' xmlns='{PACKAGE_NS}'>"
                ),
                "</r:Relationships>",
            ),
            // Unprefixed children of these are in no namespace or another.
            (
                format!("<r:Relationships xmlns:r='{PACKAGE_NS}'>"),
                "</r:Relationships>",
            ),
            (
                "<Relationships xmlns='urn:x'>".to_owned(),
                "</Relationships>",
            ),
            (format!("<Relations xmlns='{PACKAGE_NS}'>"), "</Relations>"),
            (
                format!("<Relationship xmlns='{PACKAGE_NS}'>"),
                "</Relationship>",
            ),
        ];
        let restart: String = (0..NAMESPACE_LIMIT + 8)
            .map(|n| format!("<v:a xmlns:v='urn:{n}'/>"))
            .collect();
        let children = [
            format!("<Relationship Id='a' Type='{OFFICE_NS}/table' Target='t.xml'/>"),
            format!("<Relationship Id='b' Type='{OFFICE_NS}/hyperlink' Target='https://example.com/' TargetMode='External'/>"),
            format!("<Relationship Id='c' Type='{OFFICE_NS}/worksheet' Target='s.xml' TargetMode='Internal'></Relationship>"),
            "<Relationship Id='a&#98;' Type='t' Target='&#120;&amp;'/>".to_owned(),
            "<Relationship Id='ab' Type='t' Target='y'/>".to_owned(),
            "<Relationship Id='a\tb' Type='t\n' Target='z'/>".to_owned(),
            "<Relationship Id='a b' Type='t' Target='z'/>".to_owned(),
            "<Relationship Type='t' Target='x'/>".to_owned(),
            "<Relationship Id='m' Target='x'/>".to_owned(),
            "<Relationship Id='n' Type='t'/>".to_owned(),
            "<Relationship Id='e' Type='t' Target='x' TargetMode='external'/>".to_owned(),
            "<Relationship Id='f' Type='t' Target='x' TargetMode=' External'/>".to_owned(),
            "<Relationship Id='g' TargetMode='bad'/>".to_owned(),
            "<Relationship xmlns:p='urn:p' p:Id='h' Type='t' Target='x'/>".to_owned(),
            format!("<Relationship xmlns:p='{PACKAGE_NS}' p:Id='i' Id='j' p:Type='t' Type='u' Target='x'/>"),
            format!("<q:Relationship xmlns:q='{PACKAGE_NS}' Id='k' Type='t' Target='x'/>"),
            "<x:Relationship xmlns:x='urn:x' Id='a'/>".to_owned(),
            "<Relationship xmlns='urn:other' Id='a'/>".to_owned(),
            "<other Id='a'/>".to_owned(),
            "<x:wrap xmlns:x='urn:x'><Relationship Id='w' Type='t' Target='x'/></x:wrap>".to_owned(),
            "<Relationship Id='o' Type='t' Target='x'><Relationship Id='p' Type='t' Target='y'/></Relationship>".to_owned(),
            "<!-- comment --> ".to_owned(),
            restart,
            // Errors in the XML itself, which every structural error defers to.
            "<Relationship Id='a' Id='b'/>".to_owned(),
            "<u:x/>".to_owned(),
            "<a>&unknown;</a>".to_owned(),
            "<a></b>".to_owned(),
            "]]>".to_owned(),
        ];
        let mut state: u64 = 0x5eed_1234;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as usize
        };
        let selected = |id: &str, kind: &str| id.starts_with('a') || kind.ends_with("/table");
        let mut outcomes = BTreeSet::new();
        for _ in 0..4_000 {
            let (open, close) = &roots[next() % roots.len()];
            let mut xml = open.clone();
            for _ in 0..next() % 6 {
                xml.push_str(&children[next() % children.len()]);
            }
            xml.push_str(close);
            let reference = summary(reference_relationships(xml.as_bytes()));
            assert_eq!(
                summary(relationships(xml.as_bytes(), &|_, _| true)),
                reference,
                "{xml}"
            );
            let filtered = reference.clone().map(|relationships| {
                relationships
                    .into_iter()
                    .filter(|(id, kind, _, _)| selected(id, kind))
                    .collect()
            });
            assert_eq!(
                summary(relationships(xml.as_bytes(), &selected)),
                filtered,
                "{xml}"
            );
            outcomes.insert(match reference {
                Ok(relationships) => format!("Ok({})", relationships.len().min(2)),
                Err(error) => error,
            });
        }
        // Every structural refusal and an XML error occur among the cases.
        for expected in [
            "invalid relationships root",
            "relationship outside relationships root",
            "relationship lacks Id",
            "relationship lacks Type",
            "relationship lacks Target",
            "duplicate relationship id",
            "invalid relationship TargetMode",
            "Xml(",
            "Ok(0)",
            "Ok(2)",
        ] {
            assert!(
                outcomes.iter().any(|outcome| outcome.contains(expected)),
                "{expected}: {outcomes:?}"
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
    // Parts both parsers refuse.
    const MALFORMED_XML: &[&str] = &[
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
    ];

    #[test]
    fn malformed_xml_has_no_partial_document() {
        for xml in MALFORMED_XML {
            assert!(xml_nodes(xml.as_bytes(), root_only).is_err(), "{xml}");
        }
    }

    // Namespace constraints apply to normalized values, with the rules of
    // Namespaces in XML 1.0.
    const NAMESPACE_VIOLATIONS: &[&str] = &[
        "<tag xmlns:p=''/>",
        "<tag xmlns:p='http://www.w3.org/XML/1998/namespac&#101;'/>",
        "<tag xmlns:p='http://www.w3.org/2000/xmlns&#47;'/>",
        "<tag xmlns='http://www.w3.org/XML/1998/namespac&#101;'/>",
        "<tag xmlns='http://www.w3.org/2000/xmlns&#47;'/>",
        "<tag xmlns:xml='urn:incorrect'/>",
        "<tag xmlns:xmlns='urn:incorrect'/>",
        "<xmlns:tag/>",
    ];
    const NAMESPACE_DECLARATIONS: &[&str] = &[
        "<tag xmlns=''/>",
        "<tag xmlns:p='urn&#58;correct'/>",
        "<tag xmlns:xml='http://www.w3.org/XML/1998/namespac&#101;'/>",
    ];

    #[test]
    fn namespace_constraints_use_decoded_values_and_xml_10_rules() {
        for xml in NAMESPACE_VIOLATIONS {
            assert!(
                matches!(xml_nodes(xml.as_bytes(), root_only), Err(Error::Xml(_))),
                "{xml}"
            );
        }
        for xml in NAMESPACE_DECLARATIONS {
            xml_nodes(xml.as_bytes(), root_only).unwrap();
        }
    }

    fn nested(depth: usize) -> String {
        format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth))
    }

    // Parts refused with the variant the worksheet parser reports.
    fn variant_cases() -> [(Vec<u8>, Error); 5] {
        [
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
        ]
    }

    #[test]
    fn package_xml_errors_use_the_worksheet_parser_variants() {
        xml_nodes(nested(256).as_bytes(), root_only).unwrap();
        for (xml, expected) in variant_cases() {
            let error = xml_nodes(&xml, root_only).err().unwrap();
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&expected),
                "{error}"
            );
        }
    }

    // As in worksheets, whatever entities quick-xml's features recognize.
    #[test]
    fn package_xml_resolves_only_xml_10_entities() {
        let nodes = xml_nodes(b"<a x='&amp;&#160;&quot;'>&lt;&#xA0;</a>", root_only).unwrap();
        assert_eq!(nodes[0].attr("", "x"), Some("&\u{a0}\""));
        for (xml, entity) in [
            ("<a x='&nbsp;'/>", "nbsp"),
            ("<a><b x='&nbsp;'/></a>", "nbsp"),
            ("<a xmlns:p='urn:&nbsp;'/>", "nbsp"),
            (
                "<a xmlns:xml='http:&sol;&sol;www.w3.org/XML/1998/namespace'/>",
                "sol",
            ),
            ("<a>&nbsp;</a>", "nbsp"),
        ] {
            let error = xml_nodes(xml.as_bytes(), root_only).err();
            assert!(
                matches!(&error, Some(Error::Xml(message)) if message.contains(entity)),
                "{xml}: {error:?}"
            );
        }
        assert!(matches!(
            xml_nodes(b"<a>&nbsp;</a>", root_only),
            Err(Error::Xml(message)) if message == "undefined XML entity &nbsp;"
        ));
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
        assert_eq!(
            summary(relationships(rels.as_bytes(), &|_, _| true)),
            summary(Ok(vec![expected_relationship("a", "t", "b", false)]))
        );
        // Relationships that the caller does not select are not kept at all.
        assert!(
            relationships(rels.as_bytes(), &|_, _| false)
                .unwrap()
                .is_empty()
        );

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

    // Parts both parsers accept.
    const WELL_FORMED_XML: &[&str] = &[
        "\u{feff}<?xml version='1.0' encoding='UTF-8' standalone='yes'?><a/>",
        "<?xml version='1.0' standalone='no'?><a/>",
        "<?xml-stylesheet href='style.xsl'?><a xmlns:雪='urn:vendor' 雪:属性='value'/>",
    ];

    #[test]
    fn complete_xml_supports_bom_declarations_and_unicode_names() {
        for xml in WELL_FORMED_XML {
            assert!(xml_nodes(xml.as_bytes(), root_only).is_ok(), "{xml}");
        }
    }

    // The worksheet tests' XML cases, as worksheet bodies.
    const WORKSHEET_BODIES: &[&str] = &[
        "<sheetData 0bad='one'/>",
        "<sheetData><0bad/></sheetData>",
        "<sheetData><x:bad:name/></sheetData>",
        "<sheetData><?0bad data?></sheetData>",
        "<sheetData><?XML data?></sheetData>",
        "<sheetData/><x:a>]]></x:a>",
        "<sheetData/><x:a v='a<b'/>",
        "<sheetData/><x:a v='&#1;'/>",
        "<sheetData/><x:a>\u{1}</x:a>",
        "<sheetData xmlns:p='' />",
        "<sheetData xmlns:p='http://www.w3.org/XML/1998/namespac&#101;' />",
        "<sheetData xmlns:p='http://www.w3.org/2000/xmlns&#47;' />",
        "<sheetData/><x:foreign xmlns='http://www.w3.org/XML/1998/namespac&#101;'/>",
        "<sheetData/><x:foreign xmlns='http://www.w3.org/2000/xmlns&#47;'/>",
        "<sheetData/><xmlns:invalid/>",
        "<sheetData xmlns:p='urn:&nbsp;'/>",
        "<sheetData xmlns:xml='http:&sol;&sol;www.w3.org/XML/1998/namespace'/>",
        "<sheetData><row r='1'><c r='A1' t='inlineStr'><is><t>a&nbsp;b</t></is></c></row></sheetData>",
        "<sheetData><row r='1'><c r='A1' x:flag='&amp;&#160;'/></row></sheetData>",
        "<sheetData><row r='1'><c r='A1' t='n' t='b'/></row></sheetData>",
        "<sheetData><row r='1'><c r='A1't='n'/></row></sheetData>",
        "<sheetData><row r='1'><c r='A1'><v>&unknown;</v></c></row></sheetData>",
        "<sheetData><row r='1'><c r='A1' xmlns:a='urn:x' xmlns:b='urn&#58;x' a:flag='one' b:flag='two'/></row></sheetData>",
        "<sheetData><row r='1'><c r='A1' xmlns:a='urn:x' xmlns:b='urn&#58;x' a:first='one' a:flag='two' b:flag='three'/></row></sheetData>",
        "<sheetData><row r='1'><c r='A1' missing:flag='one'/></row></sheetData>",
        "<sheetData/><x:t /xml:space='preserve'>text</x:t>",
        "<sheetData/><x:t xml:space='preserve'/ >text</x:t>",
        "<sheetData/><x:t xml:space='preserve' / >text</x:t>",
        "<sheetData/><x:t xml:space='preserve'/x:a='1'>text</x:t>",
        "<sheetData/><x:t xml:space='preserve' /x:a='1'/>",
        "<sheetData/><x:t xml:space='preserve'x:a='1'/>",
        "<sheetData/><x:t x:a='1' x:a='2'/>",
        "<sheetData/><x:t x:a=1/>",
        "<sheetData/><x:t x:a/>",
        "<sheetData/><x:t xml:space='preserve' />",
        "<sheetData/><x:t\txml:space = 'preserve'\r\n/>",
        "<sheetData/><x:t x:a='1/>'>text</x:t>",
        "<sheetData/><x:t\n>text</x:t\n>",
        "<?vendor:tool untouched?><sheetData/><x:外 x:À·='keep'/><x:\u{10000} x:\u{200c}start='keep'/>",
        "<sheetData/><sheetData/>",
    ];

    // Declarations in place of the worksheet's `<?xml version="1.0"?>`.
    const WORKSHEET_DECLARATIONS: &[&str] = &[
        " <?xml version='1.0'?>",
        "<!--first--><?xml version='1.0'?>",
        "<?xml version='1.0' version='1.0'?>",
        "<?xml version='1.0' standalone='maybe'?>",
        "<?xml version='1.0' standalone='yes' encoding='UTF-8'?>",
        "<?xml version='1.0' unknown='value'?>",
        "<?xml version='1.0' encoding='UTF-16'?>",
        "<?xml version='1.1'?>",
        "<?xml version='2.0'?>",
        "<?xml version='1.0' encoding='utf-8' standalone='yes'?>\r\n",
        "\u{feff}<?xml version='1.0'?>",
        "\u{feff}\u{feff}<?xml version='1.0'?>",
    ];

    // The review's differential probe of attribute, declaration, namespace,
    // reference and markup edge cases.
    const EDGE_CASES: &[&str] = &[
        "<a b = 'x'/>",
        "<a xmlns:xml='http://www.w3.org/XML/1998/namespace'/>",
        "<p:a xmlns:p='u'><p:b xmlns:p=''/></p:a>",
        "<a x='&#x20;'/>",
        "<a\tb='1'/>",
        "<a b='1'\n/>",
        "<a b='&lt;'/>",
        "<a b=\"'\"/>",
        "<a:b/>",
        "<a xml:lang='en'/>",
        "<a xmlns:a='urn:x' a:xmlns='1'/>",
        "<a b='1' b='2'/>",
        "<a><!-- a -- b --></a>",
        "<a><![CDATA[x]]></a>",
        "<a>]]&gt;</a>",
        "<a>\r\n</a>",
        "<?xml-stylesheet?><a/>",
        "<?xmlfoo?><a/>",
        "<a/><?xml version='1.0'?>",
        "<a/><!-- c -->",
        "<a/><?pi?>",
        "<a/>\n",
        "<a b='x'/c='y'/>",
        "<a b='x' / >",
        "<a / >",
        "<a b='x'/ >",
        "<a =\"x\"/>",
        "<a xmlns:p='urn:a' xmlns:q='urn:%61'><b p:x='1' q:x='2'/></a>",
        "<a xmlns:p='urn:a' xmlns:q='urn:&#97;'><b p:x='1' q:x='2'/></a>",
        "<a xmlns:p='&#x20;urn:a' xmlns:q='urn:a'><b p:x='1' q:x='2'/></a>",
        "<a xmlns:p=' urn:a' xmlns:q='urn:a'><b p:x='1' q:x='2'/></a>",
        "<a xmlns:p='urn:a'><b xmlns:p='urn:b' p:x='1'/></a>",
        "<a xmlns='urn:a' xmlns=''/>",
        "<a xmlns:p='urn:a' p:p='1' xmlns:p='urn:b'/>",
        "<a xmlns='&#0;'/>",
        "<a xmlns='&#1;'/>",
        "<a>&#xD800;</a>",
        "<a>&#65536;</a>",
        "<a>&#x110000;</a>",
        "<a>&amp;&lt;&gt;&quot;&apos;</a>",
        "<a b='&unknown;'/>",
        "<a b='&#38;'/>",
        "<a b='&#38;#0;'/>",
        "<a b='&#60;'/>",
        "<a>&#60;</a>",
        "<a><b/></a >",
        "<a ><b/></a>",
        "< a/>",
        "<a></ a>",
        "<a>\u{fffe}</a>",
        "<a b='\u{fffe}'/>",
        "<a\u{a0}b='1'/>",
        "<a b\u{a0}='1'/>",
        "<?xml  version='1.0'  ?><a/>",
        "<?xml\tversion='1.0'?><a/>",
        "<?xml version = '1.0'?><a/>",
        "\u{feff} <a/>",
        "<a><?xml-x?></a>",
        "<a><?x y?></a>",
        "<a><!---></a>",
        "<a><!-- --->",
        "<a><!-- - --></a>",
        "<a xmlns:b='urn:b'><b:c xmlns:b='urn:c'/><b:c/></a>",
        "<a>x<!DOCTYPE a></a>",
        "<a><![CDATA[]]]]><![CDATA[>]]></a>",
        "<a>]]]]></a>",
        "<a>]]&#62;</a>",
        "<a>&#93;]></a>",
        "<a b='>'/>",
        "<a b=\"/>\" />",
        "<a b='&#9;&#10;&#13;'/>",
        "<a b='\t\n\r'/>",
        "<a xmlns:a='urn:a' xmlns:A='urn:a' a:x='1' A:x='2'/>",
        "<a xmlns:a='http://www.w3.org/XML/1998/namespace'/>",
        "<a xml:x='1' xmlns:p='http://www.w3.org/XML/1998/namespace' />",
        "<a xmlns=' http://www.w3.org/2000/xmlns/'/>",
        "<a:b xmlns:a='urn:a'></a:b>",
        "<a:b xmlns:a='urn:a'></b>",
        "<a>\u{85}\u{2028}</a>",
        "<a/>\u{feff}",
        "<a>\u{feff}</a>",
        "<a b='1' c='2' d='3' e='4' f='5' g='6' h='7' i='8' j='9' k='10' l='11' m='12' n='13' o='14' p='15' q='16' r='17' b='18'/>",
        "<a xmlns:x='urn:x' xmlns:y='urn:x' x:a1='' x:a2='' x:a3='' x:a4='' x:a5='' x:a6='' x:a7='' x:a8='' x:a9='' x:a10='' x:a11='' x:a12='' x:a13='' x:a14='' x:a15='' x:a16='' x:a17='' y:a1=''/>",
    ];

    // Package parts and worksheets share one validating parser core, so every
    // part gets the same result, variant and message from either parser.
    #[test]
    fn package_and_worksheet_parsers_agree_on_every_case() {
        let worksheet = |body: &str| {
            format!(
                "<?xml version=\"1.0\"?><worksheet xmlns=\"{MAIN_NS}\" xmlns:x=\"urn:custom\">{body}</worksheet>"
            )
        };
        let sheet = worksheet("<sheetData/>");
        let mut inputs: Vec<Vec<u8>> = [
            MALFORMED_XML,
            WELL_FORMED_XML,
            NAMESPACE_VIOLATIONS,
            NAMESPACE_DECLARATIONS,
            EDGE_CASES,
        ]
        .concat()
        .iter()
        .map(|xml| xml.as_bytes().to_vec())
        .collect();
        inputs.extend(variant_cases().map(|(xml, _)| xml));
        inputs.push(nested(256).into_bytes());
        inputs.extend(
            WORKSHEET_BODIES
                .iter()
                .map(|body| worksheet(body).into_bytes()),
        );
        inputs.extend(WORKSHEET_DECLARATIONS.iter().map(|declaration| {
            sheet
                .replace("<?xml version=\"1.0\"?>", declaration)
                .into_bytes()
        }));
        for (prefix, suffix) in [
            ("\u{a0}", ""),
            ("", "\u{a0}"),
            ("", "<worksheet/>"),
            ("", "&amp;"),
            ("", "<![CDATA[x]]>"),
        ] {
            inputs.push(format!("{prefix}{sheet}{suffix}").into_bytes());
        }
        let deep = format!("{}{}", "<x:a>".repeat(256), "</x:a>".repeat(256));
        inputs.push(worksheet(&deep).into_bytes());
        let mut invalid = sheet.clone().into_bytes();
        invalid.insert(invalid.len() - 2, 0xff);
        inputs.push(invalid);
        for xml in &inputs {
            let package = xml_nodes(xml, root_only).map(drop);
            let worksheet = crate::worksheet::parse_xml(xml);
            assert_eq!(
                package.map_err(|error| format!("{error:?}")),
                worksheet.map_err(|error| format!("{error:?}")),
                "{}",
                String::from_utf8_lossy(xml)
            );
        }
    }

    #[test]
    fn namespace_pool_restarts_keep_every_check_and_retained_namespace() {
        let padding: String = (0..3 * NAMESPACE_LIMIT)
            .map(|n| format!("<x:a xmlns:x='urn:{n}' x:n='{n}'/>"))
            .collect();
        // Retained elements that declare namespaces of their own make the
        // pool start over inside their tags, too.
        let relationships: String = (0..3 * NAMESPACE_LIMIT)
            .map(|n| {
                format!(
                    "<p:Relationship xmlns:p='{PACKAGE_NS}' xmlns:z='urn:z{n}' z:n='{n}' Id='r{n}' Type='t' Target='{n}'/>"
                )
            })
            .collect();
        let rels = format!(
            "<Relationships xmlns='{PACKAGE_NS}'>{padding}<Relationship Id='a' Type='t' Target='b'/>{relationships}</Relationships>"
        );
        let nodes = xml_nodes(rels.as_bytes(), relationship_node).unwrap();
        let retained: Vec<_> = nodes
            .iter()
            .map(|node| {
                let vendor = node
                    .attributes
                    .iter()
                    .find(|attribute| attribute.local == "n")
                    .map(|attribute| (&*attribute.namespace, attribute.value.as_str()));
                (&*node.namespace, node.attr("", "Target"), vendor)
            })
            .collect();
        let targets: Vec<_> = (0..3 * NAMESPACE_LIMIT).map(|n| n.to_string()).collect();
        let vendors: Vec<_> = (0..3 * NAMESPACE_LIMIT)
            .map(|n| format!("urn:z{n}"))
            .collect();
        let expected: Vec<_> = [(PACKAGE_NS, None, None), (PACKAGE_NS, Some("b"), None)]
            .into_iter()
            .chain((0..3 * NAMESPACE_LIMIT).map(|n| {
                (
                    PACKAGE_NS,
                    Some(targets[n].as_str()),
                    Some((vendors[n].as_str(), targets[n].as_str())),
                )
            }))
            .collect();
        assert_eq!(retained, expected);
        let streamed = super::relationships(rels.as_bytes(), &|_, _| true).unwrap();
        let streamed: Vec<_> = streamed
            .iter()
            .map(|r| (r.id.as_str(), r.target.as_str()))
            .collect();
        let ids: Vec<_> = (0..3 * NAMESPACE_LIMIT).map(|n| format!("r{n}")).collect();
        let expected: Vec<_> = [("a", "b")]
            .into_iter()
            .chain((0..3 * NAMESPACE_LIMIT).map(|n| (ids[n].as_str(), targets[n].as_str())))
            .collect();
        assert_eq!(streamed, expected);
        // An inherited binding still expands to the same name afterwards.
        let duplicate =
            format!("<a xmlns:p='urn:same'>{padding}<b xmlns:q='urn:same' p:x='1' q:x='2'/></a>");
        assert!(matches!(
            xml_nodes(duplicate.as_bytes(), root_only),
            Err(Error::Xml(message)) if message == "duplicate expanded attribute name"
        ));
        let process_content =
            format!("<a xmlns:m='{COMPATIBILITY_NS}'>{padding}<b m:ProcessContent='x:y'/></a>");
        assert!(
            part_xml(process_content.as_bytes(), root_only)
                .unwrap()
                .process_content
        );
    }

    #[test]
    fn package_parsing_holds_one_tag_of_attributes_and_a_bounded_namespace_pool() {
        // Each tag binds a new namespace and has three attributes.
        let padding: String = (0..10 * NAMESPACE_LIMIT)
            .map(|n| format!("<x:a xmlns:x='urn:{n}' b='1' c='2'/>"))
            .collect();
        let xml = format!("<Types xmlns='{CONTENT_TYPES_NS}'>{padding}</Types>");
        ATTRIBUTE_PEAK.set(0);
        SPELLING_PEAK.set(0);
        xml_nodes(xml.as_bytes(), root_only).unwrap();
        assert_eq!(ATTRIBUTE_PEAK.get(), 3);
        // The pool starts over once a tag takes it past the limit.
        assert_eq!(SPELLING_PEAK.get(), NAMESPACE_LIMIT + 1);

        // Each child of a workbook using mc:ProcessContent resolves a new
        // namespace. The limit grows by the two rules in scope.
        let padding: String = (0..10 * NAMESPACE_LIMIT)
            .map(|n| format!("<a xmlns='urn:{n}'/>"))
            .collect();
        let workbook = format!(
            "<workbook xmlns='{MAIN_NS}' xmlns:mc='{COMPATIBILITY_NS}' xmlns:v='urn:v' mc:Ignorable='v' mc:ProcessContent='v:w'>{padding}</workbook>"
        );
        SPELLING_PEAK.set(0);
        reject_promoted_workbook_properties(workbook.as_bytes()).unwrap();
        assert_eq!(SPELLING_PEAK.get(), NAMESPACE_LIMIT + 2 + 1);
    }

    #[test]
    fn promoted_workbook_properties_are_found_after_the_namespace_pool_restarts() {
        // Each direct child of the workbook resolves a new namespace, and the
        // rules declared on the root must survive every restart.
        let padding: String = (0..3 * NAMESPACE_LIMIT)
            .map(|n| format!("<a xmlns='urn:{n}'/>"))
            .collect();
        let workbook = format!(
            "<workbook xmlns='{MAIN_NS}' xmlns:mc='{COMPATIBILITY_NS}' xmlns:v='urn:v' mc:Ignorable='v' mc:ProcessContent='v:w'>{padding}<v:w><workbookPr date1904='1'/></v:w></workbook>"
        );
        assert!(matches!(
            reject_promoted_workbook_properties(workbook.as_bytes()),
            Err(Error::Unsupported(message)) if message.contains("ProcessContent")
        ));
        reject_promoted_workbook_properties(workbook.replace("v:w>", "v:x>").as_bytes()).unwrap();
    }
}
