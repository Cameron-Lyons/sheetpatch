use super::{STRICT, TRANSITIONAL, decoded, decoded_namespace, unsupported, xml_error};
use crate::Result;
use crate::xml::{
    valid_char as valid_xml_char, valid_name, valid_qname, validate_declaration,
    validate_namespaces, whitespace as xml_whitespace,
};
use quick_xml::{
    events::{Event, attributes::Attribute as XmlAttribute},
    name::{QName, ResolveResult},
    reader::NsReader,
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// Document-local identity of a normalized namespace URI; zero is unbound.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct NamespaceId(u32);

impl NamespaceId {
    const NONE: Self = Self(0);

    pub(super) fn is_unbound(self) -> bool {
        self == Self::NONE
    }
}

struct NamespacePool {
    raw: HashMap<String, NamespaceId>,
    canonical: HashMap<Arc<str>, NamespaceId>,
    values: Vec<Arc<str>>,
}

impl NamespacePool {
    fn new() -> Self {
        Self {
            raw: HashMap::new(),
            canonical: HashMap::new(),
            values: vec![Arc::from("")],
        }
    }

    fn resolve(&mut self, result: ResolveResult<'_>) -> Result<NamespaceId> {
        match result {
            ResolveResult::Unbound => Ok(NamespaceId::NONE),
            ResolveResult::Bound(namespace) => {
                if let Some(&id) = self.raw.get(namespace.as_ref()) {
                    return Ok(id);
                }
                let value = decoded_namespace(ResolveResult::Bound(namespace))?;
                let id = if value.is_empty() {
                    NamespaceId::NONE
                } else if let Some(&id) = self.canonical.get(value.as_str()) {
                    id
                } else {
                    let id = NamespaceId(
                        u32::try_from(self.values.len())
                            .map_err(|_| unsupported("too many XML namespaces"))?,
                    );
                    // Only the URI table shares strings; elements carry plain
                    // IDs, avoiding reference-count operations on the hot path.
                    let value: Arc<str> = value.into();
                    self.values.push(Arc::clone(&value));
                    self.canonical.insert(value, id);
                    id
                };
                self.raw.insert(namespace.as_ref().to_owned(), id);
                Ok(id)
            }
            unknown => decoded_namespace(unknown).map(|_| NamespaceId::NONE),
        }
    }
}

#[derive(Debug)]
pub(super) struct Attribute<'a> {
    pub(super) name: &'a str,
    pub(super) value: Cow<'a, str>,
    pub(super) start: usize,
    pub(super) value_start: usize,
    pub(super) value_end: usize,
    pub(super) end: usize,
}

#[derive(Debug)]
pub(super) struct Element<'a> {
    pub(super) name: &'a str,
    pub(super) namespace: NamespaceId,
    pub(super) parent: Option<usize>,
    pub(super) children_start: usize,
    pub(super) children_end: usize,
    pub(super) attributes: std::ops::Range<usize>,
    pub(super) start: usize,
    pub(super) open_end: usize,
    pub(super) close_start: usize,
    pub(super) end: usize,
    pub(super) empty: bool,
    pub(super) opaque_markup: bool,
}

impl<'xml> Element<'xml> {
    pub(super) fn local_name(&self) -> &str {
        self.name.rsplit(':').next().unwrap()
    }
    pub(super) fn is(&self, namespace: NamespaceId, name: &str) -> bool {
        self.namespace == namespace && self.local_name() == name
    }
    pub(super) fn attr<'a>(
        &self,
        attributes: &'a [Attribute<'xml>],
        name: &str,
    ) -> Option<&'a Attribute<'xml>> {
        attributes[self.attributes.clone()]
            .iter()
            .find(|attr| attr.name == name)
    }
    pub(super) fn qualified(&self, name: &str) -> String {
        self.name
            .rsplit_once(':')
            .map_or_else(|| name.to_owned(), |(prefix, _)| format!("{prefix}:{name}"))
    }

    pub(super) fn has_children(&self) -> bool {
        self.children_start < self.children_end
    }

    pub(super) fn children<'a>(&self, elements: &'a [Element<'xml>]) -> Children<'a, 'xml> {
        Children {
            elements,
            next: self.children_start,
            end: self.children_end,
        }
    }
}

// Elements are recorded in preorder. A child's subtree ends at the next
// sibling, so the index needs no separately allocated list for each parent.
pub(super) struct Children<'a, 'xml> {
    pub(super) elements: &'a [Element<'xml>],
    pub(super) next: usize,
    pub(super) end: usize,
}

impl Iterator for Children<'_, '_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.end {
            return None;
        }
        let index = self.next;
        self.next = self.elements[index].children_end;
        Some(index)
    }
}

// Large shared buffers grow by half rather than doubling. This bounds unused
// capacity and the memory an allocator can retain across repeated sheet parses.
// Small buffers keep Vec's usual growth policy.
fn arena_push<T>(arena: &mut Vec<T>, item: T) {
    if arena.len() == arena.capacity() && arena.capacity() >= 4096 {
        arena.reserve_exact(arena.capacity() / 2);
    }
    arena.push(item);
}

fn attribute_spans<'xml>(
    xml: &'xml [u8],
    start: usize,
    open_end: usize,
    name_len: usize,
    attributes: &mut Vec<Attribute<'xml>>,
) -> Result<std::ops::Range<usize>> {
    let mut position = start + 1 + name_len;
    let attributes_start = attributes.len();
    while position < open_end {
        let whitespace_start = position;
        while position < open_end && xml[position].is_ascii_whitespace() {
            position += 1;
        }
        if matches!(xml.get(position), Some(b'/' | b'>')) {
            break;
        }
        if position == whitespace_start {
            return Err(xml_error("missing XML attribute separator"));
        }
        let name_start = position;
        while position < open_end && !xml[position].is_ascii_whitespace() && xml[position] != b'=' {
            position += 1;
        }
        let name = std::str::from_utf8(&xml[name_start..position])
            .map_err(|e| xml_error(e.to_string()))?;
        while position < open_end && xml[position].is_ascii_whitespace() {
            position += 1;
        }
        if xml.get(position) != Some(&b'=') {
            return Err(xml_error("invalid XML attribute"));
        }
        position += 1;
        while position < open_end && xml[position].is_ascii_whitespace() {
            position += 1;
        }
        let quote = *xml
            .get(position)
            .ok_or_else(|| xml_error("unterminated XML attribute"))?;
        if quote != b'\'' && quote != b'"' {
            return Err(xml_error("unquoted XML attribute"));
        }
        position += 1;
        let value_start = position;
        while position < open_end && xml[position] != quote {
            position += 1;
        }
        if position >= open_end {
            return Err(xml_error("unterminated XML attribute"));
        }
        let value_end = position;
        let raw = &xml[value_start..value_end];
        if raw.contains(&b'<') {
            return Err(xml_error("less-than sign in an XML attribute"));
        }
        // XML normalizes literal attribute whitespace before expanding entity
        // references; referenced whitespace keeps its actual value.
        let value = XmlAttribute {
            key: QName(name),
            value: Cow::Borrowed(std::str::from_utf8(raw).map_err(|e| xml_error(e.to_string()))?),
        }
        .normalized_value(quick_xml::XmlVersion::Implicit1_0)?;
        if !value.chars().all(valid_xml_char) {
            return Err(xml_error("invalid XML character reference"));
        }
        position += 1;
        arena_push(
            attributes,
            Attribute {
                name,
                value,
                start: whitespace_start,
                value_start,
                value_end,
                end: position,
            },
        );
    }
    Ok(attributes_start..attributes.len())
}

pub(super) struct XmlDocument<'xml> {
    pub(super) elements: Vec<Element<'xml>>,
    pub(super) attributes: Vec<Attribute<'xml>>,
    namespaces: Vec<Arc<str>>,
}

impl XmlDocument<'_> {
    pub(super) fn is_spreadsheet_namespace(&self, id: NamespaceId) -> bool {
        matches!(
            self.namespaces[id.0 as usize].as_ref(),
            TRANSITIONAL | STRICT
        )
    }
}

pub(super) fn parse(xml: &[u8]) -> Result<XmlDocument<'_>> {
    let source =
        std::str::from_utf8(xml).map_err(|e| xml_error(format!("worksheet must be UTF-8: {e}")))?;
    if !source.chars().all(valid_xml_char) {
        return Err(xml_error("invalid XML character"));
    }
    let bom = if xml.starts_with(&[0xef, 0xbb, 0xbf]) {
        3
    } else {
        0
    };
    // quick-xml strips a BOM without including it in buffer_position(). Keep
    // offsets relative to the original input explicitly.
    let mut reader = NsReader::from_reader(&xml[bom..]);
    reader.config_mut().check_comments = true;
    let mut elements: Vec<Element> = Vec::new();
    let mut attributes = Vec::new();
    let mut namespaces = NamespacePool::new();
    let mut stack = Vec::new();
    let mut root_seen = false;
    let mut declaration_seen = false;
    loop {
        let start = reader.buffer_position() as usize + bom;
        let event = reader.read_event()?;
        let end = reader.buffer_position() as usize + bom;
        match event {
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                if stack.len() >= 256 {
                    return Err(unsupported(
                        "worksheet XML nesting deeper than 256 elements",
                    ));
                }
                if stack.is_empty() {
                    if root_seen {
                        return Err(xml_error("multiple XML root elements"));
                    }
                    root_seen = true;
                }
                let name =
                    std::str::from_utf8(&xml[start + 1..start + 1 + tag.name().as_ref().len()])
                        .map_err(|e| xml_error(e.to_string()))?;
                if !valid_qname(name) {
                    return Err(xml_error("invalid XML element name"));
                }
                validate_namespaces(tag)?;
                let (namespace, _) = reader.resolver().resolve_element(tag.name());
                let namespace = namespaces.resolve(namespace)?;
                // Let quick-xml check attribute syntax and duplicate names, and
                // independently reject duplicate expanded names/prefix errors.
                let mut first_expanded = None;
                let mut expanded = HashSet::new();
                for attribute in tag.attributes() {
                    let attribute = attribute.map_err(|e| xml_error(e.to_string()))?;
                    let attribute_name = attribute.key.as_ref();
                    if !valid_qname(attribute_name) {
                        return Err(xml_error("invalid XML attribute name"));
                    }
                    let (ns, local) = reader.resolver().resolve_attribute(attribute.key);
                    let ns = namespaces.resolve(ns)?;
                    // Unprefixed names and namespace declarations are already
                    // checked for exact duplicates by quick-xml. Only ordinary
                    // prefixed attributes can alias another expanded name.
                    if attribute_name.contains(':') && !attribute_name.starts_with("xmlns:") {
                        let name = (ns, local.into_inner());
                        if let Some(first) = &first_expanded {
                            if first == &name || !expanded.insert(name) {
                                return Err(xml_error("duplicate expanded attribute name"));
                            }
                        } else {
                            first_expanded = Some(name);
                        }
                    }
                }
                let attributes = attribute_spans(xml, start, end, name.len(), &mut attributes)?;
                let empty = matches!(event, Event::Empty(_));
                let parent = stack.last().copied();
                let index = elements.len();
                arena_push(
                    &mut elements,
                    Element {
                        name,
                        namespace,
                        parent,
                        children_start: index + 1,
                        children_end: index + 1,
                        attributes,
                        start,
                        open_end: end,
                        close_start: end,
                        end,
                        empty,
                        opaque_markup: false,
                    },
                );
                if !empty {
                    stack.push(index);
                }
            }
            Event::End(_) => {
                let index = stack
                    .pop()
                    .ok_or_else(|| xml_error("unmatched closing XML tag"))?;
                elements[index].close_start = start;
                elements[index].end = end;
                elements[index].children_end = elements.len();
            }
            Event::Text(text) => {
                let raw = text.as_ref();
                if raw.contains("]]>") {
                    return Err(xml_error("CDATA terminator in ordinary XML text"));
                }
                if stack.is_empty() && !xml_whitespace(raw) {
                    return Err(xml_error("text outside the XML root"));
                }
            }
            Event::GeneralRef(reference) => {
                if stack.is_empty() {
                    return Err(xml_error("entity reference outside the XML root"));
                }
                let name = reference.as_ref();
                decoded(&format!("&{name};"))?;
            }
            Event::CData(_) if stack.is_empty() => {
                return Err(xml_error("CDATA outside the XML root"));
            }
            Event::Comment(_) => {
                if let Some(&index) = stack.last() {
                    elements[index].opaque_markup = true;
                }
            }
            Event::PI(instruction) => {
                let target = instruction.target();
                if !valid_name(target) || target.eq_ignore_ascii_case("xml") {
                    return Err(xml_error("invalid XML processing instruction target"));
                }
                if let Some(&index) = stack.last() {
                    elements[index].opaque_markup = true;
                }
            }
            Event::DocType(_) => return Err(unsupported("XML document type declarations")),
            Event::Decl(declaration) => {
                if root_seen || declaration_seen || start != bom {
                    return Err(xml_error("misplaced XML declaration"));
                }
                declaration_seen = true;
                validate_declaration(&declaration)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() || !root_seen {
        return Err(xml_error("incomplete XML document"));
    }
    Ok(XmlDocument {
        elements,
        attributes,
        namespaces: namespaces.values,
    })
}
