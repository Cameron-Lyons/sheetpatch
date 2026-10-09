use crate::{
    CellContent, CellEdit, CellRef, CellValue, Error, Result,
    archive::Archive,
    worksheet::{patch_cells, read_cells, read_shared_strings},
    xml::{excel_text, namespace, valid_qname},
};
use quick_xml::{events::Event, reader::NsReader};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::Path,
    sync::OnceLock,
};

const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const SIGNATURE_ORIGIN: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin";
const CONTENT_TYPES_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const STRICT_MAIN_NS: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const STRICT_OFFICE_NS: &str = "http://purl.oclc.org/ooxml/officeDocument/relationships";

/// A worksheet discovered through the package's relationships.
#[derive(Clone, Debug)]
pub struct Sheet {
    name: String,
    path: String,
}

impl Sheet {
    /// The decoded worksheet name used by the workbook's read and edit methods.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The worksheet part's resolved path within the ZIP archive.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// An existing OOXML workbook with pending surgical cell edits.
///
/// Untouched ZIP entries retain their local headers, compressed bytes, extra
/// fields, and comments. Changed worksheet records are replaced in the archive's
/// local-record sequence; central-directory offsets may change. A save with no
/// edits returns the original archive byte for byte. This does not calculate
/// formulas or refresh charts/pivot caches.
///
/// ```no_run
/// use sheetpatch::{CellValue, Workbook};
/// let mut book = Workbook::open("report.xlsm")?;
/// book.set_cell("Summary", "B2", "Revised")?;
/// book.set_cell("Summary", "C2", 42.5)?;
/// book.set_cell("Summary", "D2", true)?;
/// book.set_cell("Summary", "E2", CellValue::Blank)?;
/// book.save("report-edited.xlsm")?;
/// # Ok::<(), sheetpatch::Error>(())
/// ```
pub struct Workbook {
    archive: Archive,
    sheets: Vec<Sheet>,
    changes: BTreeMap<String, Vec<u8>>,
    signed: bool,
    originals: Vec<OnceLock<Vec<u8>>>,
    sheet_indexes: BTreeMap<String, usize>,
    shared_strings_path: Option<String>,
    shared_strings: OnceLock<Vec<String>>,
}

impl Workbook {
    /// Read a workbook file and discover worksheets through its relationships.
    /// Worksheet XML and shared strings are parsed lazily when first accessed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_bytes(fs::read(path)?)
    }

    /// Open an in-memory workbook, retaining its original archive bytes.
    /// Worksheet XML and shared strings are parsed lazily when first accessed.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let archive = Archive::new(bytes)?;
        if !archive.contains("[Content_Types].xml") {
            return Err(Error::InvalidWorkbook("missing [Content_Types].xml".into()));
        }
        let content_types = xml_nodes(&archive.read("[Content_Types].xml")?)?;
        if content_types[0].local != "Types" || content_types[0].namespace != CONTENT_TYPES_NS {
            return Err(Error::InvalidWorkbook("invalid content-types root".into()));
        }
        let root_rels_path = archive.part_name("_rels/.rels")?.unwrap_or("_rels/.rels");
        let roots = relationships(&archive.read(root_rels_path)?)?;
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
        let workbook_uri = resolve_target("", &office.target)?;
        let workbook_path = resolve_part_target(&archive, "", &office.target)?;
        if workbook_path
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("bin"))
        {
            return Err(Error::Unsupported(
                "binary Excel workbooks (.xlsb) are not supported".into(),
            ));
        }
        let workbook_nodes = xml_nodes(&archive.read(&workbook_path)?)?;
        let root = &workbook_nodes[0];
        if root.local != "workbook" || !main_namespace(&root.namespace) {
            return Err(Error::InvalidWorkbook(
                "officeDocument is not a SpreadsheetML workbook".into(),
            ));
        }
        let mut sheet_containers = workbook_nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.local == "sheets" && node.namespace == root.namespace);
        let sheets_index = match (sheet_containers.next(), sheet_containers.next()) {
            (Some((index, node)), None) if node.parent == Some(0) => index,
            _ => {
                return Err(Error::InvalidWorkbook(
                    "workbook must contain exactly one direct sheets element".into(),
                ));
            }
        };
        let rels_path = relationships_path(&workbook_path);
        let rels_path = archive.part_name(&rels_path)?.unwrap_or(&rels_path);
        let rels = relationships(&archive.read(rels_path)?)?;
        let mut shared = rels
            .iter()
            .filter(|r| office_type(&r.kind, "sharedStrings"));
        let shared = match (shared.next(), shared.next()) {
            (Some(relationship), None) if !relationship.external => Some(relationship),
            (None, None) => None,
            _ => {
                return Err(Error::InvalidWorkbook(
                    "expected at most one internal shared-string relationship".into(),
                ));
            }
        };
        let shared_strings_path = shared
            .map(|r| resolve_part_target(&archive, &workbook_uri, &r.target))
            .transpose()?;
        let relationships_by_id: BTreeMap<_, _> = rels.iter().map(|r| (r.id.as_str(), r)).collect();
        let mut sheets = Vec::new();
        let mut names = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for node in &workbook_nodes {
            if node.local != "sheet" || node.namespace != root.namespace {
                continue;
            }
            if node.parent != Some(sheets_index) {
                return Err(Error::Unsupported("sheet outside workbook sheets".into()));
            }
            let name = excel_text(
                node.attr("", "name")
                    .ok_or_else(|| Error::InvalidWorkbook("sheet lacks a name".into()))?,
            )?;
            if !names.insert(name.clone()) {
                return Err(Error::InvalidWorkbook("duplicate sheet name".into()));
            }
            let mut ids = node.attributes.iter().filter(|a| {
                a.local == "id" && matches!(a.namespace.as_str(), OFFICE_NS | STRICT_OFFICE_NS)
            });
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
            let path = resolve_part_target(&archive, &workbook_uri, &relationship.target)?;
            if !archive.contains(&path) {
                return Err(Error::InvalidWorkbook(format!(
                    "missing worksheet part {path}"
                )));
            }
            if !paths.insert(path.clone()) {
                return Err(Error::InvalidWorkbook(
                    "duplicate worksheet name or target".into(),
                ));
            }
            sheets.push(Sheet { name, path });
        }
        let signed = roots.iter().any(|r| r.kind == SIGNATURE_ORIGIN)
            || archive.entries().iter().any(|e| {
                e.name
                    .split_once('/')
                    .is_some_and(|(root, _)| root.eq_ignore_ascii_case("_xmlsignatures"))
            });
        let originals = (0..sheets.len()).map(|_| OnceLock::new()).collect();
        let sheet_indexes = sheets
            .iter()
            .enumerate()
            .map(|(index, sheet)| (sheet.name.clone(), index))
            .collect();
        Ok(Self {
            archive,
            sheets,
            changes: BTreeMap::new(),
            signed,
            originals,
            sheet_indexes,
            shared_strings_path,
            shared_strings: OnceLock::new(),
        })
    }

    /// The editable worksheets in workbook tab order.
    /// Chart, dialog, and macro sheets are omitted. Names match exactly,
    /// including case, in all read and edit methods.
    pub fn sheets(&self) -> &[Sheet] {
        &self.sheets
    }

    /// Set a scalar cell value, inserting a cell/row if needed.
    ///
    /// Formula cells and shared/array formula ranges are deliberately protected.
    /// Text and numbers are validated before applying changes. Failed edits leave
    /// this workbook unchanged.
    pub fn set_cell(
        &mut self,
        sheet: &str,
        address: &str,
        value: impl Into<CellValue>,
    ) -> Result<()> {
        let cell = address.parse()?;
        let value = value.into();
        value.validate()?;
        let index = self.sheet_index(sheet)?;
        self.apply_grouped_edits([(index, BTreeMap::from([(cell, value)]))])
    }

    /// Apply edits to a worksheet in one parse and one XML patch.
    /// Duplicate addresses use the last value; every input is validated.
    /// The entire batch is committed only after every edit succeeds.
    pub fn set_cells<I, S, V>(&mut self, sheet: &str, cells: I) -> Result<()>
    where
        I: IntoIterator<Item = (S, V)>,
        S: AsRef<str>,
        V: Into<CellValue>,
    {
        let index = self.sheet_index(sheet)?;
        let mut values = BTreeMap::new();
        for (address, value) in cells {
            let cell = address.as_ref().parse()?;
            let value = value.into();
            value.validate()?;
            values.insert(cell, value);
        }
        if values.is_empty() {
            return Ok(());
        }
        self.apply_grouped_edits([(index, values)])
    }

    /// Apply a transaction across worksheets. On any error, all pending cell
    /// values remain unchanged. Worksheets are decompressed lazily and cached.
    pub fn apply_edits(&mut self, edits: impl IntoIterator<Item = CellEdit>) -> Result<()> {
        let mut grouped: BTreeMap<usize, BTreeMap<CellRef, CellValue>> = BTreeMap::new();
        for edit in edits {
            let index = self.sheet_index(&edit.sheet)?;
            grouped
                .entry(index)
                .or_default()
                .insert(edit.cell, edit.value);
        }
        self.apply_grouped_edits(grouped)
    }

    fn apply_grouped_edits(
        &mut self,
        grouped: impl IntoIterator<Item = (usize, BTreeMap<CellRef, CellValue>)>,
    ) -> Result<()> {
        let mut grouped = grouped.into_iter().peekable();
        if grouped.peek().is_none() {
            return Ok(());
        }
        if self.signed {
            return Err(Error::Unsupported(
                "editing a digitally signed OOXML package would invalidate its signature".into(),
            ));
        }
        let mut staged = Vec::with_capacity(grouped.size_hint().0);
        for (index, cells) in grouped {
            let sheet = &self.sheets[index];
            let original = self.original_sheet(index)?;
            let current = self
                .changes
                .get(&sheet.path)
                .map(Vec::as_slice)
                .unwrap_or(original);
            let Cow::Owned(patched) = patch_cells(current, &cells)? else {
                continue;
            };
            let changed = (patched != original).then_some(patched);
            staged.push((sheet.path.clone(), changed));
        }
        for (path, changed) in staged {
            if let Some(xml) = changed {
                self.changes.insert(path, xml);
            } else {
                self.changes.remove(&path);
            }
        }
        Ok(())
    }

    /// Read a cell's current value, cached formula result, and style index.
    /// Missing cells return a blank value without modifying the workbook.
    pub fn get_cell(&self, sheet: &str, address: &str) -> Result<CellContent> {
        Ok(self.get_cells(sheet, [address])?.remove(0))
    }

    /// Read many cells with one worksheet parse, retaining input order.
    pub fn get_cells<I, S>(&self, sheet: &str, addresses: I) -> Result<Vec<CellContent>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let index = self.sheet_index(sheet)?;
        let cells = addresses
            .into_iter()
            .map(|address| address.as_ref().parse())
            .collect::<Result<Vec<CellRef>>>()?;
        if cells.is_empty() {
            return Ok(Vec::new());
        }
        let xml = self
            .changes
            .get(&self.sheets[index].path)
            .map(Vec::as_slice)
            .map(Ok)
            .unwrap_or_else(|| self.original_sheet(index))?;
        read_cells(xml, &cells, || self.shared_strings())
    }

    /// Whether any package parts have pending edits.
    pub fn has_changes(&self) -> bool {
        !self.changes.is_empty()
    }

    /// Discard every pending edit, restoring byte-identical original output.
    pub fn reset_changes(&mut self) {
        self.changes.clear();
    }

    fn sheet_index(&self, name: &str) -> Result<usize> {
        self.sheet_indexes
            .get(name)
            .copied()
            .ok_or_else(|| Error::SheetNotFound(name.to_owned()))
    }

    fn original_sheet(&self, index: usize) -> Result<&[u8]> {
        let cache = &self.originals[index];
        if cache.get().is_none() {
            let xml = self.archive.read(&self.sheets[index].path)?;
            let _ = cache.set(xml);
        }
        Ok(cache.get().expect("worksheet cache initialized").as_slice())
    }

    fn shared_strings(&self) -> Result<&[String]> {
        if self.shared_strings.get().is_none() {
            let path = self
                .shared_strings_path
                .as_ref()
                .ok_or(Error::SharedStringsUnavailable)?;
            let strings = read_shared_strings(&self.archive.read(path)?)?;
            let _ = self.shared_strings.set(strings);
        }
        Ok(self
            .shared_strings
            .get()
            .expect("shared-string cache initialized")
            .as_slice())
    }

    /// Serialize the current workbook, replacing changed worksheet ZIP records.
    /// Untouched local records and pre-existing ZIP gaps are retained. An archive
    /// with contiguous local records stays contiguous; edited records do not
    /// accumulate as obsolete copies. Central-directory offsets may change.
    /// Without edits this returns the original bytes. Pending edits are retained.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.archive.write(&self.changes)
    }

    /// Serialize into a writer without allocating a second complete archive.
    /// Changed worksheet records replace their original records in the local
    /// record sequence. Untouched local records and pre-existing ZIP gaps remain;
    /// central-directory offsets may change.
    /// A failing writer may contain a partial archive; use `save` for atomic files.
    pub fn write_to(&self, mut writer: impl Write) -> Result<()> {
        self.archive.write_to(&self.changes, &mut writer)
    }

    /// Remove pre-existing obsolete ZIP records while retaining untouched active
    /// local headers, compressed bytes, and descriptors. Central-directory
    /// offsets may change. Opaque ZIP prefixes or gaps cause an error.
    /// A failing writer may contain a partial archive; use `save_compact` for files.
    pub fn write_compact_to(&self, mut writer: impl Write) -> Result<()> {
        self.archive.compact_to(&self.changes, &mut writer)
    }

    /// Serialize a compact archive, removing obsolete ZIP records.
    /// Opaque ZIP prefixes or gaps cause an error. Pending edits are retained.
    pub fn to_bytes_compact(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.write_compact_to(&mut bytes)?;
        Ok(bytes)
    }

    /// Save through a temporary file in the destination directory, then rename.
    /// The original destination is retained if generation or writing fails.
    /// Using the input path as destination is supported.
    /// Saving does not clear pending edits or replace the retained original.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        crate::atomic::write(path.as_ref(), |writer| self.write_to(writer))
    }

    /// Atomically save while removing recognizable obsolete ZIP records already
    /// present in the input. Opaque ZIP prefixes or gaps cause an error.
    /// Saving does not clear pending edits or replace the retained original.
    pub fn save_compact(&self, path: impl AsRef<Path>) -> Result<()> {
        crate::atomic::write(path.as_ref(), |writer| self.write_compact_to(writer))
    }
}

struct Attribute {
    namespace: String,
    local: String,
    value: String,
}
struct Node {
    namespace: String,
    local: String,
    attributes: Vec<Attribute>,
    parent: Option<usize>,
}
impl Node {
    fn attr(&self, namespace: &str, local: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|a| a.namespace == namespace && a.local == local)
            .map(|a| a.value.as_str())
    }
}

fn xml_nodes(bytes: &[u8]) -> Result<Vec<Node>> {
    let xml = std::str::from_utf8(bytes)
        .map_err(|_| Error::Unsupported("only UTF-8 workbook XML is supported".into()))?;
    if !xml.chars().all(crate::xml::valid_char) {
        return Err(Error::Xml("invalid XML character".into()));
    }
    let mut reader = NsReader::from_str(xml.strip_prefix('\u{feff}').unwrap_or(xml));
    reader.config_mut().check_comments = true;
    let mut nodes = Vec::new();
    let mut stack = Vec::new();
    let mut roots = 0;
    let mut declaration_seen = false;
    loop {
        let event_start = reader.buffer_position();
        let event = reader.read_event()?;
        let is_empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(e) | Event::Empty(e) => {
                if stack.is_empty() {
                    roots += 1;
                }
                if stack.len() >= 256 {
                    return Err(Error::Xml("XML nesting exceeds 256 levels".into()));
                }
                let name = e.name();
                let name = name.as_ref();
                if !valid_qname(name) || name.starts_with("xmlns:") {
                    return Err(Error::Xml("invalid XML element name".into()));
                }
                crate::xml::validate_namespaces(&e)?;
                crate::xml::validate_attribute_spacing(&e)?;
                let ns = namespace(reader.resolver().resolve_element(e.name()).0)?;
                let mut attributes = Vec::new();
                let mut keys = BTreeSet::new();
                for attr in e.attributes() {
                    let attr = attr.map_err(|e| Error::Xml(e.to_string()))?;
                    let name = attr.key.as_ref();
                    if !valid_qname(name) {
                        return Err(Error::Xml("invalid XML attribute name".into()));
                    }
                    if attr.value.contains('<') {
                        return Err(Error::Xml("literal '<' in XML attribute value".into()));
                    }
                    let (resolved, local) = reader.resolver().resolve_attribute(attr.key);
                    let namespace = namespace(resolved)?;
                    let local = local.as_ref().to_owned();
                    if !keys.insert((namespace.clone(), local.clone())) {
                        return Err(Error::Xml("duplicate XML attribute".into()));
                    }
                    let value = attr
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                        .into_owned();
                    if !value.chars().all(crate::xml::valid_char) {
                        return Err(Error::Xml("invalid XML attribute character".into()));
                    }
                    attributes.push(Attribute {
                        namespace,
                        local,
                        value,
                    });
                }
                let node = Node {
                    namespace: ns,
                    local: e.local_name().as_ref().to_owned(),
                    attributes,
                    parent: stack.last().copied(),
                };
                nodes.push(node);
                if !is_empty {
                    stack.push(nodes.len() - 1);
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
    let nodes = xml_nodes(bytes)?;
    if nodes[0].local != "Relationships" || nodes[0].namespace != PACKAGE_NS {
        return Err(Error::InvalidWorkbook("invalid relationships root".into()));
    }
    let mut ids = BTreeSet::new();
    let mut result = Vec::new();
    if nodes.iter().any(|node| {
        node.namespace == PACKAGE_NS && node.local == "Relationship" && node.parent != Some(0)
    }) {
        return Err(Error::InvalidWorkbook(
            "relationship outside relationships root".into(),
        ));
    }
    for node in nodes
        .iter()
        .filter(|n| n.parent == Some(0) && n.namespace == PACKAGE_NS && n.local == "Relationship")
    {
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
            Archive::new(include_bytes!("../tests/fixtures/libreoffice.xlsx").to_vec()).unwrap();
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
        ] {
            assert!(xml_nodes(xml.as_bytes()).is_err(), "{xml}");
        }
    }
    #[test]
    fn complete_xml_supports_bom_declarations_and_unicode_names() {
        for xml in [
            "\u{feff}<?xml version='1.0' encoding='UTF-8' standalone='yes'?><a/>",
            "<?xml version='1.0' standalone='no'?><a/>",
            "<?xml-stylesheet href='style.xsl'?><a xmlns:雪='urn:vendor' 雪:属性='value'/>",
        ] {
            assert!(xml_nodes(xml.as_bytes()).is_ok(), "{xml}");
        }
    }
}
