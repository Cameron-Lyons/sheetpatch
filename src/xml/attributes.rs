//! Compact attribute spans and one validating attribute scanner.

use super::document::{is_space, valid_qname};
use super::namespace::{NameKind, NamespaceId, NamespacePool, NamespaceReader};
use super::text::valid_text;
use crate::{Error, Result};
use quick_xml::events::attributes::Attribute as XmlAttribute;
use quick_xml::name::QName;
use quick_xml::{XmlVersion, escape};
use std::borrow::Cow;
use std::collections::HashSet;
use std::hash::Hash;
use std::ops::Range;

/// An attribute's normalized value, resolving only XML 1.0's predefined
/// entities and character references, as `reference` does in text.
///
/// Every caller has already checked the document's literal characters with
/// `valid_text`. Only a value that normalization changed can therefore hold
/// a character that XML forbids, which a character reference introduced.
pub(crate) fn attribute_value<'a>(attribute: &XmlAttribute<'a>) -> Result<Cow<'a, str>> {
    let value = attribute
        .normalized_value_with(XmlVersion::Implicit1_0, 1, escape::resolve_xml_entity)
        .map_err(Error::xml)?;
    if let Cow::Owned(value) = &value
        && !valid_text(value)
    {
        return Err(Error::Xml("invalid XML character reference".into()));
    }
    Ok(value)
}

// Checks one declaration's normalized value. `prefix` is `None` for a default
// namespace declaration (`xmlns`), and the declared prefix for `xmlns:prefix`.
// quick-xml's resolver also checks reserved bindings, but does so against the
// raw spelling and permits prefix undeclarations used by Namespaces in XML 1.1.
fn validate_namespace_binding(prefix: Option<&str>, value: &str) -> Result<()> {
    const XML: &str = "http://www.w3.org/XML/1998/namespace";
    const XMLNS: &str = "http://www.w3.org/2000/xmlns/";
    if prefix == Some("xmlns")
        || value == XMLNS
        || (value == XML && prefix != Some("xml"))
        || (prefix == Some("xml") && value != XML)
    {
        return Err(Error::Xml("invalid reserved XML namespace binding".into()));
    }
    if prefix.is_some() && value.is_empty() {
        return Err(Error::Xml(
            "namespace prefixes cannot be undeclared in XML 1.0".into(),
        ));
    }
    Ok(())
}

/// Converts an offset or index for a compact record. `DocumentRules::source`
/// refuses parts of `u32::MAX` bytes or more, and every element or attribute
/// spans several bytes, so all values fit below `u32::MAX`.
pub(crate) fn compact(value: usize) -> u32 {
    debug_assert!(
        value < u32::MAX as usize,
        "offset or index {value} does not fit a compact record: `DocumentRules::source` \
         should have refused the part"
    );
    value as u32
}

// An attribute's value is its raw span unless XML normalization changed it.
// Changed values live in a side table, which ordinary parts leave empty; this
// index marks a value read from its span.
const RAW_VALUE: u32 = u32::MAX;

// One attribute's spans. Its name and raw value are read from the source, so
// the record holds no pointers of its own.
#[derive(Debug)]
pub(crate) struct Attribute {
    start: u32,
    name_start: u32,
    name_end: u32,
    value_start: u32,
    value_end: u32,
    // The value's index in `Attributes::normalized`, or `RAW_VALUE`.
    normalized: u32,
}

impl Attribute {
    fn name<'s>(&self, xml: &'s [u8]) -> &'s [u8] {
        &xml[self.name_start as usize..self.name_end as usize]
    }
}

/// Recorded attributes, with the source their spans index.
pub(crate) struct Attributes<'xml> {
    source: &'xml str,
    records: Vec<Attribute>,
    normalized: Vec<Box<str>>,
}

impl<'xml> Attributes<'xml> {
    pub(crate) fn new(source: &'xml str) -> Self {
        Self {
            source,
            records: Vec::new(),
            normalized: Vec::new(),
        }
    }

    pub(crate) fn view(&self, index: usize) -> AttributeView<'_> {
        let record = &self.records[index];
        // The parser checked that these spans lie on character boundaries.
        let name = &self.source[record.name_start as usize..record.name_end as usize];
        let value = if record.normalized == RAW_VALUE {
            &self.source[record.value_start as usize..record.value_end as usize]
        } else {
            &self.normalized[record.normalized as usize]
        };
        AttributeView {
            name,
            value,
            start: record.start,
            value_start: record.value_start,
            value_end: record.value_end,
        }
    }

    /// The attribute named `name` among those at `indexes`.
    pub(crate) fn find(&self, indexes: Range<usize>, name: &str) -> Option<AttributeView<'_>> {
        self.position(indexes, name).map(|index| self.view(index))
    }

    /// The value of the attribute named `name` among those at `indexes`,
    /// borrowed from the source unless XML normalization changed it.
    pub(crate) fn find_value(&self, indexes: Range<usize>, name: &str) -> Option<Cow<'xml, str>> {
        let record = &self.records[self.position(indexes, name)?];
        Some(if record.normalized == RAW_VALUE {
            Cow::Borrowed(&self.source[record.value_start as usize..record.value_end as usize])
        } else {
            Cow::Owned(self.normalized[record.normalized as usize].to_string())
        })
    }

    fn position(&self, indexes: Range<usize>, name: &str) -> Option<usize> {
        // Names compare as bytes, so only a match reads a value.
        let xml = self.source.as_bytes();
        indexes
            .into_iter()
            .find(|&index| self.records[index].name(xml) == name.as_bytes())
    }

    /// Discards the attributes from index `first` on, with their values.
    pub(crate) fn truncate(&mut self, first: usize) {
        // Normalized values are stored in attribute order, so the discarded
        // values begin with the first discarded attribute that has one.
        if let Some(record) = self.records[first..]
            .iter()
            .find(|record| record.normalized != RAW_VALUE)
        {
            self.normalized.truncate(record.normalized as usize);
        }
        self.records.truncate(first);
    }

    pub(crate) fn clear(&mut self) {
        self.records.clear();
        self.normalized.clear();
    }
}

#[cfg(test)]
impl Attributes<'_> {
    /// How many attributes are recorded.
    pub(crate) fn records_len(&self) -> usize {
        self.records.len()
    }

    /// How many values the side table holds.
    pub(crate) fn normalized_len(&self) -> usize {
        self.normalized.len()
    }
}

/// An attribute's name and normalized value, with its spans.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AttributeView<'xml> {
    pub(crate) name: &'xml str,
    pub(crate) value: &'xml str,
    start: u32,
    value_start: u32,
    value_end: u32,
}

impl AttributeView<'_> {
    /// The start of the whitespace that separates the attribute from the
    /// preceding name or attribute.
    pub(crate) fn start(&self) -> usize {
        self.start as usize
    }
    pub(crate) fn value_start(&self) -> usize {
        self.value_start as usize
    }
    pub(crate) fn value_end(&self) -> usize {
        self.value_end as usize
    }
    /// The end of the attribute: the closing quote follows the value.
    pub(crate) fn end(&self) -> usize {
        self.value_end as usize + 1
    }
}

// Large shared buffers grow by half rather than doubling. This bounds unused
// capacity and the memory an allocator can retain across repeated sheet parses.
// Small buffers keep Vec's usual growth policy.
pub(crate) fn arena_push<T>(arena: &mut Vec<T>, item: T) {
    if arena.len() == arena.capacity() && arena.capacity() >= 4096 {
        arena.reserve_exact(arena.capacity() / 2);
    }
    arena.push(item);
}

/// Scratch space for one tag's duplicate checks, reused across tags so that
/// ordinary tags allocate nothing.
#[derive(Default)]
pub(crate) struct AttributeChecks<'xml> {
    // Expanded names of the tag's ordinary prefixed attributes so far.
    expanded: Vec<ExpandedName<'xml>>,
    // Used only by tags with many attributes.
    names: HashSet<&'xml [u8]>,
    expanded_names: HashSet<ExpandedName<'xml>>,
}

// A prefixed attribute's name as Namespaces in XML compares it: its
// namespace and local part, whatever prefix spells the namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ExpandedName<'xml> {
    namespace: NamespaceId,
    local: &'xml str,
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

pub(crate) fn markup(source: &str, range: Range<usize>) -> Result<&str> {
    source
        .get(range)
        .ok_or_else(|| Error::Xml("XML markup splits a UTF-8 character".into()))
}

/// The only attribute scanner: it records spans and checks syntax, names,
/// exact and expanded-name duplicates, and namespace declarations in one pass
/// over the tag between the element name and `open_end`. Returns the indexes
/// of the tag's attributes in `attributes`.
pub(crate) fn attribute_spans<'xml>(
    mut position: usize,
    open_end: usize,
    attributes: &mut Attributes<'xml>,
    namespaces: &mut NamespacePool,
    reader: &NamespaceReader<'_>,
    checks: &mut AttributeChecks<'xml>,
) -> Result<Range<usize>> {
    let source = attributes.source;
    let xml = source.as_bytes();
    let attributes_start = attributes.records.len();
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
            return Err(Error::Xml("missing XML attribute separator".into()));
        }
        let name_start = position;
        while position < open_end && !is_space(xml[position]) && xml[position] != b'=' {
            position += 1;
        }
        let name = markup(source, name_start..position)?;
        if !valid_qname(name) {
            return Err(Error::Xml("invalid XML attribute name".into()));
        }
        let earlier = attributes.records[attributes_start..]
            .iter()
            .map(|attribute| attribute.name(xml));
        if repeated(earlier, &mut checks.names, name.as_bytes()) {
            return Err(Error::Xml("duplicate XML attribute".into()));
        }
        while position < open_end && is_space(xml[position]) {
            position += 1;
        }
        if position >= open_end || xml[position] != b'=' {
            return Err(Error::Xml("invalid XML attribute".into()));
        }
        position += 1;
        while position < open_end && is_space(xml[position]) {
            position += 1;
        }
        let quote = match xml[..open_end].get(position) {
            Some(&quote @ (b'\'' | b'"')) => quote,
            _ => return Err(Error::Xml("unquoted XML attribute".into())),
        };
        let value_start = position + 1;
        let value_end = xml[value_start..open_end]
            .iter()
            .position(|&byte| byte == quote)
            .map(|length| value_start + length)
            .ok_or_else(|| Error::Xml("unterminated XML attribute".into()))?;
        let raw = markup(source, value_start..value_end)?;
        if raw.contains('<') {
            return Err(Error::Xml("less-than sign in an XML attribute".into()));
        }
        // XML normalizes literal attribute whitespace before expanding entity
        // references; referenced whitespace keeps its actual value. Literal
        // characters were validated with the whole document.
        let value = attribute_value(&XmlAttribute {
            key: QName(name),
            value: Cow::Borrowed(raw),
        })?;
        if name == "xmlns" {
            validate_namespace_binding(None, &value)?;
        } else if let Some(prefix) = name.strip_prefix("xmlns:") {
            validate_namespace_binding(Some(prefix), &value)?;
        } else if name.contains(':') {
            // Different prefixes bound to one namespace can still repeat an
            // expanded name. Unprefixed attributes have no namespace.
            let qualified = QName(name);
            let expanded = ExpandedName {
                namespace: namespaces.resolve(reader, qualified, NameKind::Attribute)?,
                local: qualified.local_name().into_inner(),
            };
            if repeated(
                checks.expanded.iter().copied(),
                &mut checks.expanded_names,
                expanded,
            ) {
                return Err(Error::Xml("duplicate expanded attribute name".into()));
            }
            checks.expanded.push(expanded);
        }
        // A borrowed value is the raw span itself and needs no storage.
        let normalized = match value {
            Cow::Borrowed(_) => RAW_VALUE,
            Cow::Owned(value) => {
                attributes.normalized.push(value.into_boxed_str());
                compact(attributes.normalized.len() - 1)
            }
        };
        arena_push(
            &mut attributes.records,
            Attribute {
                start: compact(whitespace_start),
                name_start: compact(name_start),
                name_end: compact(name_start + name.len()),
                value_start: compact(value_start),
                value_end: compact(value_end),
                normalized,
            },
        );
        position = value_end + 1;
    }
    Ok(attributes_start..attributes.records.len())
}
