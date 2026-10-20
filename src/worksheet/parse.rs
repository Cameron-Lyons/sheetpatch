use super::{STRICT, TRANSITIONAL, decoded_namespace, unsupported, xml_error};
use crate::xml::{
    MAX_DEPTH, NamespaceCache, NamespaceReader, is_space, valid_char as valid_xml_char, valid_name,
    valid_qname, validate_declaration, validate_namespace_binding, whitespace as xml_whitespace,
};
use crate::{Error, Result};
use quick_xml::{
    events::{Event, attributes::Attribute as XmlAttribute},
    name::{QName, ResolveResult},
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    hash::Hash,
    ops::Range,
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
    // Resolve inherited bindings by prefix without revisiting a potentially
    // large URI. Nested declarations retain the surrounding scope's cache.
    prefixes: NamespaceCache<NamespaceId>,
    raw: HashMap<String, NamespaceId>,
    canonical: HashMap<Arc<str>, NamespaceId>,
    values: Vec<Arc<str>>,
}

impl NamespacePool {
    fn new() -> Self {
        Self {
            prefixes: NamespaceCache::default(),
            raw: HashMap::new(),
            canonical: HashMap::new(),
            values: vec![Arc::from("")],
        }
    }

    fn resolve(
        &mut self,
        reader: &NamespaceReader<'_>,
        name: QName<'_>,
        attribute: bool,
    ) -> Result<NamespaceId> {
        if let Some(id) = self.prefixes.get(reader, name, attribute) {
            return Ok(id);
        }
        let (namespace, _) = if attribute {
            reader.resolver().resolve_attribute(name)
        } else {
            reader.resolver().resolve_element(name)
        };
        let id = self.resolve_uncached(namespace)?;
        self.prefixes.insert(reader, name, attribute, id);
        Ok(id)
    }

    fn resolve_uncached(&mut self, result: ResolveResult<'_>) -> Result<NamespaceId> {
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

// Records store byte offsets and arena indexes as u32, which keeps a large
// part's index compact. `parse_with` refuses parts of u32::MAX bytes or more,
// and every element or attribute spans several bytes, so all values fit below
// the parent sentinel.
const NO_PARENT: u32 = u32::MAX;

fn compact(value: usize) -> u32 {
    debug_assert!(value < NO_PARENT as usize);
    value as u32
}

#[derive(Debug)]
pub(super) struct Attribute<'a> {
    pub(super) name: &'a str,
    pub(super) value: Cow<'a, str>,
    start: u32,
    value_start: u32,
    value_end: u32,
}

impl Attribute<'_> {
    /// The start of the whitespace that separates the attribute from the
    /// preceding name or attribute.
    pub(super) fn start(&self) -> usize {
        self.start as usize
    }
    pub(super) fn value_start(&self) -> usize {
        self.value_start as usize
    }
    pub(super) fn value_end(&self) -> usize {
        self.value_end as usize
    }
    /// The end of the attribute: the closing quote follows the value.
    pub(super) fn end(&self) -> usize {
        self.value_end as usize + 1
    }
}

#[derive(Debug)]
pub(super) struct Element<'a> {
    pub(super) name: &'a str,
    pub(super) namespace: NamespaceId,
    parent: u32,
    children_start: u32,
    children_end: u32,
    attributes_start: u32,
    attributes_end: u32,
    start: u32,
    open_end: u32,
    close_start: u32,
    end: u32,
    pub(super) empty: bool,
    pub(super) opaque_markup: bool,
}

impl<'xml> Element<'xml> {
    pub(super) fn parent(&self) -> Option<usize> {
        (self.parent != NO_PARENT).then_some(self.parent as usize)
    }
    /// The start of the opening tag.
    pub(super) fn start(&self) -> usize {
        self.start as usize
    }
    /// The end of the opening tag, which ends an empty element.
    pub(super) fn open_end(&self) -> usize {
        self.open_end as usize
    }
    /// The start of the closing tag, or `open_end` for an empty element.
    pub(super) fn close_start(&self) -> usize {
        self.close_start as usize
    }
    pub(super) fn end(&self) -> usize {
        self.end as usize
    }
    /// Arena indexes of every descendant, in document order.
    pub(super) fn descendants(&self) -> Range<usize> {
        self.children_start as usize..self.children_end as usize
    }
    pub(super) fn local_name(&self) -> &str {
        self.name.rsplit(':').next().unwrap()
    }
    pub(super) fn is(&self, namespace: NamespaceId, name: &str) -> bool {
        self.namespace == namespace && self.local_name() == name
    }
    pub(super) fn attributes<'a>(
        &self,
        attributes: &'a [Attribute<'xml>],
    ) -> &'a [Attribute<'xml>] {
        &attributes[self.attributes_start as usize..self.attributes_end as usize]
    }
    pub(super) fn attr<'a>(
        &self,
        attributes: &'a [Attribute<'xml>],
        name: &str,
    ) -> Option<&'a Attribute<'xml>> {
        self.attributes(attributes)
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
            next: self.children_start as usize,
            end: self.children_end as usize,
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
        self.next = self.elements[index].children_end as usize;
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

// Scratch space for one tag's duplicate checks, reused across tags so that
// ordinary tags allocate nothing.
#[derive(Default)]
struct AttributeChecks<'xml> {
    // Expanded names of the tag's ordinary prefixed attributes so far.
    expanded: Vec<(NamespaceId, &'xml str)>,
    // Used only by tags with many attributes.
    names: HashSet<&'xml str>,
    expanded_names: HashSet<(NamespaceId, &'xml str)>,
}

// Short lists are scanned directly. Past this length a set keeps duplicate
// detection linear in the attribute count, so hostile tags cannot make it
// quadratic.
const LINEAR_SCAN_LIMIT: usize = 16;

// Whether `item` repeats any of `previous`, every earlier item of one tag.
// `set` holds them all once the list has outgrown a linear scan.
fn repeated<T: Copy + Eq + Hash>(
    mut previous: impl ExactSizeIterator<Item = T>,
    set: &mut HashSet<T>,
    item: T,
) -> bool {
    match previous.len() {
        length if length < LINEAR_SCAN_LIMIT => previous.any(|earlier| earlier == item),
        LINEAR_SCAN_LIMIT => {
            set.clear();
            set.extend(previous);
            !set.insert(item)
        }
        _ => !set.insert(item),
    }
}

fn markup(source: &str, range: Range<usize>) -> Result<&str> {
    source
        .get(range)
        .ok_or_else(|| xml_error("XML markup splits a UTF-8 character"))
}

// The only attribute scanner: it records spans and checks syntax, names,
// exact and expanded-name duplicates, and namespace declarations in one pass
// over the tag between the element name and `open_end`.
fn attribute_spans<'xml>(
    source: &'xml str,
    mut position: usize,
    open_end: usize,
    document: &mut XmlDocument<'xml>,
    reader: &NamespaceReader<'_>,
    checks: &mut AttributeChecks<'xml>,
) -> Result<Range<usize>> {
    let xml = source.as_bytes();
    let attributes = &mut document.attributes;
    let attributes_start = attributes.len();
    checks.expanded.clear();
    loop {
        let whitespace_start = position;
        while position < open_end && is_space(xml[position]) {
            position += 1;
        }
        // Only the tag's own `>` or `/>` ends the list. A slash anywhere
        // else is not a terminator.
        if position + 1 == open_end || (position + 2 == open_end && xml[position] == b'/') {
            break;
        }
        if position == whitespace_start {
            return Err(xml_error("missing XML attribute separator"));
        }
        let name_start = position;
        while position < open_end && !is_space(xml[position]) && xml[position] != b'=' {
            position += 1;
        }
        let name = markup(source, name_start..position)?;
        if !valid_qname(name) {
            return Err(xml_error("invalid XML attribute name"));
        }
        let earlier = attributes[attributes_start..]
            .iter()
            .map(|attribute| attribute.name);
        if repeated(earlier, &mut checks.names, name) {
            return Err(xml_error("duplicate XML attribute"));
        }
        while position < open_end && is_space(xml[position]) {
            position += 1;
        }
        if position >= open_end || xml[position] != b'=' {
            return Err(xml_error("invalid XML attribute"));
        }
        position += 1;
        while position < open_end && is_space(xml[position]) {
            position += 1;
        }
        let quote = match xml[..open_end].get(position) {
            Some(&quote @ (b'\'' | b'"')) => quote,
            _ => return Err(xml_error("unquoted XML attribute")),
        };
        let value_start = position + 1;
        let value_end = xml[value_start..open_end]
            .iter()
            .position(|&byte| byte == quote)
            .map(|length| value_start + length)
            .ok_or_else(|| xml_error("unterminated XML attribute"))?;
        let raw = markup(source, value_start..value_end)?;
        if raw.contains('<') {
            return Err(xml_error("less-than sign in an XML attribute"));
        }
        // XML normalizes literal attribute whitespace before expanding entity
        // references; referenced whitespace keeps its actual value.
        let value = XmlAttribute {
            key: QName(name),
            value: Cow::Borrowed(raw),
        }
        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
        .map_err(Error::xml)?;
        // Literal characters were validated with the whole document; only
        // normalization or expanded references can produce new ones.
        if let Cow::Owned(value) = &value
            && !value.chars().all(valid_xml_char)
        {
            return Err(xml_error("invalid XML character reference"));
        }
        if name == "xmlns" {
            validate_namespace_binding(None, &value)?;
        } else if let Some(prefix) = name.strip_prefix("xmlns:") {
            validate_namespace_binding(Some(prefix), &value)?;
        } else if name.contains(':') {
            // Different prefixes bound to one namespace can still repeat an
            // expanded name. Unprefixed attributes have no namespace.
            let qualified = QName(name);
            let expanded = (
                document.namespaces.resolve(reader, qualified, true)?,
                qualified.local_name().into_inner(),
            );
            if repeated(
                checks.expanded.iter().copied(),
                &mut checks.expanded_names,
                expanded,
            ) {
                return Err(xml_error("duplicate expanded attribute name"));
            }
            checks.expanded.push(expanded);
        }
        arena_push(
            attributes,
            Attribute {
                name,
                value,
                start: compact(whitespace_start),
                value_start: compact(value_start),
                value_end: compact(value_end),
            },
        );
        position = value_end + 1;
    }
    Ok(attributes_start..attributes.len())
}

pub(super) struct XmlDocument<'xml> {
    pub(super) elements: Vec<Element<'xml>>,
    pub(super) attributes: Vec<Attribute<'xml>>,
    namespaces: NamespacePool,
}

impl<'xml> XmlDocument<'xml> {
    pub(super) fn is_spreadsheet_namespace(&self, id: NamespaceId) -> bool {
        matches!(
            self.namespaces.values[id.0 as usize].as_ref(),
            TRANSITIONAL | STRICT
        )
    }

    pub(super) fn other_spreadsheet_namespace(
        &self,
        namespace: NamespaceId,
    ) -> Option<NamespaceId> {
        [TRANSITIONAL, STRICT]
            .into_iter()
            .filter_map(|uri| self.namespaces.canonical.get(uri).copied())
            .find(|&candidate| candidate != namespace)
    }

    // Offers a completed child of the root to `release`, which may discard
    // the child's subtree. The root is the only open element at that point.
    fn complete_child(&mut self, index: usize, release: &mut impl FnMut(&Self, usize) -> bool) {
        if release(self, index) {
            let attributes = self.elements[index].attributes_start as usize;
            self.elements.truncate(index);
            self.attributes.truncate(attributes);
        }
    }
}

pub(super) fn parse(xml: &[u8]) -> Result<XmlDocument<'_>> {
    parse_with(xml, |_, _| false)
}

/// Parses with every well-formedness and namespace check, calling `release`
/// as each child of the root element completes. When it returns true, that
/// child's subtree leaves the arenas, so a caller that consumes the children
/// one at a time needs memory only for the largest of them.
pub(super) fn parse_with<'xml>(
    xml: &'xml [u8],
    mut release: impl FnMut(&XmlDocument<'xml>, usize) -> bool,
) -> Result<XmlDocument<'xml>> {
    // Pending worksheet XML is not size-limited until it is saved, so the
    // compact records' offset range is enforced here.
    if xml.len() >= NO_PARENT as usize {
        return Err(unsupported("XML part exceeds the 4 GiB index limit"));
    }
    let source =
        std::str::from_utf8(xml).map_err(|e| xml_error(format!("XML must be UTF-8: {e}")))?;
    if !source.chars().all(valid_xml_char) {
        return Err(xml_error("invalid XML character"));
    }
    let bom = if xml.starts_with(&[0xef, 0xbb, 0xbf]) {
        3
    } else {
        0
    };
    // quick-xml strips a BOM without including it in buffer_position(). Keep
    // offsets relative to the original input explicitly. After the first BOM,
    // U+FEFF is text before the root; the reader would strip it too.
    if xml[bom..].starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(xml_error("text outside the XML root"));
    }
    let mut reader = NamespaceReader::from_reader(&xml[bom..]);
    reader.config_mut().check_comments = true;
    let mut document = XmlDocument {
        elements: Vec::new(),
        attributes: Vec::new(),
        namespaces: NamespacePool::new(),
    };
    let mut checks = AttributeChecks::default();
    let mut stack = Vec::new();
    let mut root_seen = false;
    let mut declaration_seen = false;
    loop {
        let start = reader.buffer_position() as usize + bom;
        let event = reader.read_event().map_err(Error::xml)?;
        let end = reader.buffer_position() as usize + bom;
        match event {
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(unsupported(format!(
                        "XML nesting deeper than {MAX_DEPTH} elements"
                    )));
                }
                if stack.is_empty() {
                    if root_seen {
                        return Err(xml_error("multiple XML root elements"));
                    }
                    root_seen = true;
                }
                let name_end = start + 1 + tag.name().as_ref().len();
                let name = markup(source, start + 1..name_end)?;
                if !valid_qname(name) {
                    return Err(xml_error("invalid XML element name"));
                }
                // Namespace declarations are checked with the attributes.
                if name.starts_with("xmlns:") {
                    return Err(xml_error("xmlns cannot be an element prefix"));
                }
                let namespace = document.namespaces.resolve(&reader, tag.name(), false)?;
                let attributes =
                    attribute_spans(source, name_end, end, &mut document, &reader, &mut checks)?;
                let empty = matches!(event, Event::Empty(_));
                let index = document.elements.len();
                arena_push(
                    &mut document.elements,
                    Element {
                        name,
                        namespace,
                        parent: stack.last().map_or(NO_PARENT, |&parent| compact(parent)),
                        children_start: compact(index + 1),
                        children_end: compact(index + 1),
                        attributes_start: compact(attributes.start),
                        attributes_end: compact(attributes.end),
                        start: compact(start),
                        open_end: compact(end),
                        close_start: compact(end),
                        end: compact(end),
                        empty,
                        opaque_markup: false,
                    },
                );
                if !empty {
                    stack.push(index);
                } else if stack.len() == 1 {
                    document.complete_child(index, &mut release);
                }
            }
            Event::End(_) => {
                let index = stack
                    .pop()
                    .ok_or_else(|| xml_error("unmatched closing XML tag"))?;
                let children_end = compact(document.elements.len());
                let element = &mut document.elements[index];
                element.close_start = compact(start);
                element.end = compact(end);
                element.children_end = children_end;
                if stack.len() == 1 {
                    document.complete_child(index, &mut release);
                }
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
                crate::xml::reference(&reference)?;
            }
            Event::CData(_) if stack.is_empty() => {
                return Err(xml_error("CDATA outside the XML root"));
            }
            Event::Comment(_) => {
                if let Some(&index) = stack.last() {
                    document.elements[index].opaque_markup = true;
                }
            }
            Event::PI(instruction) => {
                let target = instruction.target();
                if !valid_name(target) || target.eq_ignore_ascii_case("xml") {
                    return Err(xml_error("invalid XML processing instruction target"));
                }
                if let Some(&index) = stack.last() {
                    document.elements[index].opaque_markup = true;
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
    Ok(document)
}
