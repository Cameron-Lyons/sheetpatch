use crate::{CellRef, CellValue, Error, Result, archive::Archive, worksheet::patch_cell};
use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
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
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// An existing OOXML workbook with pending surgical cell edits.
///
/// Untouched ZIP entries retain their compressed bytes and metadata. Only edited
/// worksheet XML is replaced. A save with no edits returns the original archive
/// byte for byte. This does not calculate formulas or refresh charts/pivot caches.
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
}

impl Workbook {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_bytes(fs::read(path)?)
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let archive = Archive::new(bytes)?;
        if !archive
            .entries()
            .iter()
            .any(|e| e.name == "[Content_Types].xml")
        {
            return Err(Error::InvalidWorkbook("missing [Content_Types].xml".into()));
        }
        let roots = relationships(&archive.read("_rels/.rels")?)?;
        let office: Vec<_> = roots
            .iter()
            .filter(|r| office_type(&r.kind, "officeDocument"))
            .collect();
        if office.len() != 1 || office[0].external {
            return Err(Error::InvalidWorkbook(
                "expected one internal officeDocument relationship".into(),
            ));
        }
        let workbook_path = resolve_target("", &office[0].target)?;
        if workbook_path.ends_with(".bin") {
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
        let rels = relationships(&archive.read(&relationships_path(&workbook_path))?)?;
        let mut sheets = Vec::new();
        let mut names = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for node in &workbook_nodes {
            let Some(parent_index) = node.parent else {
                continue;
            };
            let parent = &workbook_nodes[parent_index];
            if node.local != "sheet"
                || node.namespace != root.namespace
                || parent.local != "sheets"
                || parent.namespace != root.namespace
                || parent.parent != Some(0)
            {
                continue;
            }
            let name = node
                .attr("", "name")
                .ok_or_else(|| Error::InvalidWorkbook("sheet lacks a name".into()))?;
            let ids: Vec<_> = node
                .attributes
                .iter()
                .filter(|a| {
                    a.local == "id" && matches!(a.namespace.as_str(), OFFICE_NS | STRICT_OFFICE_NS)
                })
                .collect();
            if ids.len() != 1 {
                return Err(Error::InvalidWorkbook(format!(
                    "sheet {name} needs one relationship id"
                )));
            }
            let relationship = rels.iter().find(|r| r.id == ids[0].value).ok_or_else(|| {
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
            let path = resolve_target(&workbook_path, &relationship.target)?;
            if !archive.entries().iter().any(|e| e.name == path) {
                return Err(Error::InvalidWorkbook(format!(
                    "missing worksheet part {path}"
                )));
            }
            if !names.insert(name.to_owned()) || !paths.insert(path.clone()) {
                return Err(Error::InvalidWorkbook(
                    "duplicate worksheet name or target".into(),
                ));
            }
            sheets.push(Sheet {
                name: name.to_owned(),
                path,
            });
        }
        let signed = roots
            .iter()
            .any(|r| r.kind == format!("{PACKAGE_NS}/digital-signature/origin"))
            || archive
                .entries()
                .iter()
                .any(|e| e.name.to_ascii_lowercase().starts_with("_xmlsignatures/"));
        Ok(Self {
            archive,
            sheets,
            changes: BTreeMap::new(),
            signed,
        })
    }

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
        let cell: CellRef = address.parse()?;
        let value = value.into();
        value.validate()?;
        let sheet = self
            .sheets
            .iter()
            .find(|s| s.name == sheet)
            .ok_or_else(|| Error::SheetNotFound(sheet.to_owned()))?;
        if self.signed {
            return Err(Error::Unsupported(
                "editing a digitally signed OOXML package would invalidate its signature".into(),
            ));
        }
        let original = self.archive.read(&sheet.path)?;
        let current = self.changes.get(&sheet.path).unwrap_or(&original);
        let patched = patch_cell(current, cell, &value)?;
        if patched == original {
            self.changes.remove(&sheet.path);
        } else {
            self.changes.insert(sheet.path.clone(), patched);
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.archive.write(&self.changes)
    }

    /// Save through a temporary file in the destination directory, then rename.
    /// The original destination is retained if generation or writing fails.
    /// Using the input path as destination is supported.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let bytes = self.to_bytes()?;
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let filename = path
            .file_name()
            .ok_or_else(|| Error::InvalidValue("destination must name a file".into()))?;
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
        let (temp_path, mut file) = loop {
            let mut name = OsString::from(".");
            name.push(filename);
            name.push(format!(
                ".sheetpatch-{}-{}.tmp",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            let candidate = parent.join(name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => break (candidate, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        let guard = TempFile(temp_path);
        file.write_all(&bytes)?;
        if let Ok(metadata) = fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&guard.0, path)?;
        Ok(())
    }
}

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
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

fn namespace(result: ResolveResult<'_>) -> Result<String> {
    match result {
        ResolveResult::Bound(ns) => std::str::from_utf8(ns.as_ref())
            .map(str::to_owned)
            .map_err(|e| Error::Xml(e.to_string())),
        ResolveResult::Unbound => Ok(String::new()),
        ResolveResult::Unknown(prefix) => Err(Error::Xml(format!(
            "unbound namespace prefix {}",
            String::from_utf8_lossy(&prefix)
        ))),
    }
}

fn xml_nodes(bytes: &[u8]) -> Result<Vec<Node>> {
    let xml = std::str::from_utf8(bytes)
        .map_err(|_| Error::Unsupported("only UTF-8 workbook XML is supported".into()))?;
    if !xml.chars().all(valid_xml_char) {
        return Err(Error::Xml("invalid XML character".into()));
    }
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().check_comments = true;
    let mut nodes = Vec::new();
    let mut stack = Vec::new();
    let mut roots = 0;
    let mut declaration_seen = false;
    loop {
        let event = reader.read_event()?;
        let is_empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(e) | Event::Empty(e) => {
                if stack.is_empty() {
                    roots += 1;
                }
                if stack.len() > 256 {
                    return Err(Error::Xml("XML nesting exceeds 256 levels".into()));
                }
                let ns = namespace(reader.resolver().resolve_element(e.name()).0)?;
                let mut attributes = Vec::new();
                let mut keys = BTreeSet::new();
                for attr in e.attributes() {
                    let attr = attr.map_err(|e| Error::Xml(e.to_string()))?;
                    let (resolved, local) = reader.resolver().resolve_attribute(attr.key);
                    let namespace = namespace(resolved)?;
                    let local = std::str::from_utf8(local.as_ref())
                        .map_err(|e| Error::Xml(e.to_string()))?
                        .to_owned();
                    if !keys.insert((namespace.clone(), local.clone())) {
                        return Err(Error::Xml("duplicate XML attribute".into()));
                    }
                    let value = attr
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )?
                        .into_owned();
                    if !value.chars().all(valid_xml_char) {
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
                    local: std::str::from_utf8(e.local_name().as_ref())
                        .map_err(|e| Error::Xml(e.to_string()))?
                        .to_owned(),
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
                if roots != 0 || declaration_seen {
                    return Err(Error::Xml("misplaced XML declaration".into()));
                }
                declaration_seen = true;
                if d.version().map_err(|e| Error::Xml(e.to_string()))?.as_ref() != b"1.0" {
                    return Err(Error::Unsupported("only XML 1.0 is supported".into()));
                }
                if let Some(encoding) = d.encoding() {
                    let encoding = encoding.map_err(|e| Error::Xml(e.to_string()))?;
                    if !encoding.eq_ignore_ascii_case(b"UTF-8") {
                        return Err(Error::Unsupported("only UTF-8 XML is supported".into()));
                    }
                }
            }
            Event::DocType(_) => {
                return Err(Error::Unsupported(
                    "DTD declarations are not supported".into(),
                ));
            }
            Event::Text(e) => {
                if e.as_ref().windows(3).any(|s| s == b"]]>") {
                    return Err(Error::Xml("CDATA terminator in XML text".into()));
                }
                if stack.is_empty() && !e.as_ref().iter().all(u8::is_ascii_whitespace) {
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
                let reference =
                    std::str::from_utf8(e.as_ref()).map_err(|e| Error::Xml(e.to_string()))?;
                let spelling = format!("&{reference};");
                let value = quick_xml::escape::unescape(&spelling)
                    .map_err(|e| Error::Xml(e.to_string()))?;
                if !value.chars().all(valid_xml_char) {
                    return Err(Error::Xml("invalid XML character reference".into()));
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

fn valid_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
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
    kind.strip_suffix(suffix).is_some_and(|prefix| {
        prefix == format!("{OFFICE_NS}/") || prefix == format!("{STRICT_OFFICE_NS}/")
    })
}
fn relationships_path(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((parent, name)) => format!("{parent}/_rels/{name}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}
fn resolve_target(source: &str, target: &str) -> Result<String> {
    if target.is_empty() || target.contains(['\\', '#', '?', ':']) {
        return Err(Error::InvalidWorkbook(format!(
            "invalid internal relationship target {target}"
        )));
    }
    let mut decoded = Vec::new();
    let mut input = target.as_bytes().iter().copied();
    while let Some(b) = input.next() {
        if b == b'%' {
            let high = input.next().and_then(|c| (c as char).to_digit(16));
            let low = input.next().and_then(|c| (c as char).to_digit(16));
            let value = high
                .zip(low)
                .ok_or_else(|| Error::InvalidWorkbook("invalid percent-encoded target".into()))?;
            let decoded_byte = (value.0 * 16 + value.1) as u8;
            if matches!(decoded_byte, b'/' | b'\\' | 0) {
                return Err(Error::InvalidWorkbook(
                    "encoded path separator in target".into(),
                ));
            }
            decoded.push(decoded_byte);
        } else {
            decoded.push(b);
        }
    }
    let target = std::str::from_utf8(&decoded)
        .map_err(|_| Error::InvalidWorkbook("target path is not UTF-8".into()))?;
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        source
            .rsplit_once('/')
            .map(|(p, _)| p.split('/').collect())
            .unwrap_or_default()
    };
    for part in target.split('/') {
        match part {
            "" | "." => (),
            ".." => {
                if parts.pop().is_none() {
                    return Err(Error::InvalidWorkbook(
                        "relationship target escapes package root".into(),
                    ));
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
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolve_relative_and_absolute_part_uris() {
        assert_eq!(
            resolve_target("books/main.xml", "../tabs/sheet%201.xml").unwrap(),
            "tabs/sheet 1.xml"
        );
        assert_eq!(
            resolve_target("books/main.xml", "/xl/worksheets/sheet1.xml").unwrap(),
            "xl/worksheets/sheet1.xml"
        );
        for target in [
            "../../escape",
            "http://example.com/x",
            "a%2fb",
            "a%",
            "a\\b",
            "",
        ] {
            assert!(resolve_target("books/main.xml", target).is_err());
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
        ] {
            assert!(xml_nodes(xml.as_bytes()).is_err(), "{xml}");
        }
    }
}
