use super::errors;
use crate::xml::{
    AttrView, AttributeChecks, Attributes, DocumentRules, NamespaceId, NamespacePool, arena_push,
    attribute_spans, compact, markup,
};
use crate::{Error, Result};
use quick_xml::events::Event;
use std::ops::Range;

pub(super) const TRANSITIONAL: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
pub(super) const STRICT: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";

// Records store byte offsets and arena indexes as u32, which keeps a large
// part's index compact. `DocumentRules::source` refuses parts of u32::MAX
// bytes or more, and every element or attribute spans several bytes, so all
// values fit below the parent sentinel.
const NO_PARENT: u32 = u32::MAX;

#[derive(Debug)]
pub(super) struct Element {
    // The qualified name follows the tag's `<`, so only its length is stored.
    name_len: u32,
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

impl Element {
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
    fn name_span(&self) -> Range<usize> {
        let start = self.start as usize + 1;
        start..start + self.name_len as usize
    }
    /// The qualified name, read from the document's source.
    pub(super) fn name<'s>(&self, source: &'s str) -> &'s str {
        &source[self.name_span()]
    }
    pub(super) fn local_name<'s>(&self, source: &'s str) -> &'s str {
        self.name(source).rsplit(':').next().unwrap()
    }
    pub(super) fn is(&self, source: &str, namespace: NamespaceId, name: &str) -> bool {
        // Callers name an unprefixed local name, so it matches the whole
        // qualified name or the bytes after its colon.
        debug_assert!(!name.contains(':'));
        let qualified = &source.as_bytes()[self.name_span()];
        self.namespace == namespace
            && qualified.ends_with(name.as_bytes())
            && (qualified.len() == name.len()
                || qualified[qualified.len() - name.len() - 1] == b':')
    }
    pub(super) fn attributes<'a>(
        &self,
        attributes: &'a Attributes<'_>,
    ) -> impl Iterator<Item = AttrView<'a>> {
        (self.attributes_start as usize..self.attributes_end as usize)
            .map(|index| attributes.view(index))
    }
    pub(super) fn attr<'a>(
        &self,
        attributes: &'a Attributes<'_>,
        name: &str,
    ) -> Option<AttrView<'a>> {
        attributes.find(
            self.attributes_start as usize..self.attributes_end as usize,
            name,
        )
    }
    pub(super) fn qualified(&self, source: &str, name: &str) -> String {
        self.name(source)
            .rsplit_once(':')
            .map_or_else(|| name.to_owned(), |(prefix, _)| format!("{prefix}:{name}"))
    }

    pub(super) fn has_children(&self) -> bool {
        self.children_start < self.children_end
    }

    pub(super) fn children<'a>(&self, elements: &'a [Element]) -> Children<'a> {
        Children {
            elements,
            next: self.children_start as usize,
            end: self.children_end as usize,
        }
    }
}

// Elements are recorded in preorder. A child's subtree ends at the next
// sibling, so the index needs no separately allocated list for each parent.
pub(super) struct Children<'a> {
    pub(super) elements: &'a [Element],
    pub(super) next: usize,
    pub(super) end: usize,
}

impl Iterator for Children<'_> {
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

pub(super) struct XmlDocument<'xml> {
    /// The parsed XML, which holds every element name and raw value.
    pub(super) source: &'xml str,
    pub(super) elements: Vec<Element>,
    pub(super) attributes: Attributes<'xml>,
    namespaces: NamespacePool,
}

impl<'xml> XmlDocument<'xml> {
    pub(super) fn is_spreadsheet_namespace(&self, id: NamespaceId) -> bool {
        matches!(self.namespaces.uri(id).as_ref(), TRANSITIONAL | STRICT)
    }

    pub(super) fn other_spreadsheet_namespace(
        &self,
        namespace: NamespaceId,
    ) -> Option<NamespaceId> {
        [TRANSITIONAL, STRICT]
            .into_iter()
            .filter_map(|uri| self.namespaces.id(uri))
            .find(|&candidate| candidate != namespace)
    }

    // Offers a completed child of the root to `release`, which may discard
    // the child's subtree. The root is the only open element at that point.
    fn complete_child(&mut self, index: usize, release: &mut impl FnMut(&Self, usize) -> bool) {
        if release(self, index) {
            let first = self.elements[index].attributes_start as usize;
            self.elements.truncate(index);
            self.attributes.truncate(first);
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
    let (source, mut rules) = DocumentRules::source(xml)?;
    let mut reader = rules.reader(source);
    let mut document = XmlDocument {
        source,
        elements: Vec::new(),
        attributes: Attributes::new(source),
        namespaces: NamespacePool::new(),
    };
    let mut checks = AttributeChecks::default();
    let mut stack = Vec::new();
    loop {
        let start = rules.position(&reader);
        let event = reader.read_event().map_err(Error::xml)?;
        let end = rules.position(&reader);
        match event {
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                let name_end = start + 1 + tag.name().as_ref().len();
                let name = markup(source, start + 1..name_end)?;
                rules.start_element(stack.len(), name)?;
                let namespace = document.namespaces.resolve(&reader, tag.name(), false)?;
                let attributes = attribute_spans(
                    name_end,
                    end,
                    &mut document.attributes,
                    &mut document.namespaces,
                    &reader,
                    &mut checks,
                )?;
                let empty = matches!(event, Event::Empty(_));
                let index = document.elements.len();
                arena_push(
                    &mut document.elements,
                    Element {
                        name_len: compact(name.len()),
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
                    .ok_or_else(|| errors::xml_error("unmatched closing XML tag"))?;
                let children_end = compact(document.elements.len());
                let element = &mut document.elements[index];
                element.close_start = compact(start);
                element.end = compact(end);
                element.children_end = children_end;
                if stack.len() == 1 {
                    document.complete_child(index, &mut release);
                }
            }
            Event::Eof => break,
            _ => {
                rules.event(&event, start, stack.len())?;
                if let (Event::Comment(_) | Event::PI(_), Some(&index)) = (&event, stack.last()) {
                    document.elements[index].opaque_markup = true;
                }
            }
        }
    }
    rules.finish(stack.len())?;
    Ok(document)
}

/// Parses `xml` as any XML part, with every check of the worksheet parser,
/// for tests that compare the package parser with it.
#[cfg(test)]
pub(crate) fn parse_xml(xml: &[u8]) -> Result<()> {
    parse(xml).map(drop)
}
