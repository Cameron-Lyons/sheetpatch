//! Validated package-part XML and workbook compatibility paths.
use super::namespaces::{self, COMPATIBILITY_NAMESPACE};
use crate::xml::{
    self, AttributeChecks, Attributes, DocumentRules, NameKind, NamespaceId, NamespacePool,
    NamespaceReader,
};
use crate::{Error, Result};
use quick_xml::events::Event;
use quick_xml::name::{QName, ResolveResult};
use std::borrow::Cow;
#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
#[cfg(test)]
use std::thread::LocalKey;

#[cfg(test)]
#[path = "tests/part.rs"]
mod tests;

/// Selects the elements kept from a package part by their depth below the
/// root, namespace URI, local name, and retained parent. The root is always kept.
pub(super) type Retain = fn(usize, &str, &str, Option<&Node>) -> bool;

pub(super) fn root_only(_: usize, _: &str, _: &str, _: Option<&Node>) -> bool {
    false
}

// Retained elements and attributes keep their namespace URIs rather than pool
// IDs, since the pool can be cleared while a part is parsed. Each URI is
// shared with the pool that resolved it instead of being copied for every
// element and attribute.
pub(super) struct Attribute {
    pub(super) namespace: Arc<str>,
    pub(super) local: String,
    pub(super) value: String,
}

pub(super) struct Node {
    pub(super) namespace: Arc<str>,
    pub(super) local: String,
    pub(super) attributes: Vec<Attribute>,
    /// The parent's index when the parent was retained.
    pub(super) parent: Option<usize>,
}
impl Node {
    pub(super) fn attribute(&self, namespace: &str, local: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|a| &*a.namespace == namespace && a.local == local)
            .map(|a| a.value.as_str())
    }
}

/// A package part's retained elements, and whether any element of the part
/// has a non-empty mc:ProcessContent attribute.
pub(super) struct PartXml {
    pub(super) nodes: Vec<Node>,
    pub(super) process_content: bool,
}

// Parts use few namespaces. Past this many spellings a parser that keeps no
// namespace IDs clears its pool, so that distinct declarations cannot grow it
// without bound.
pub(super) const NAMESPACE_LIMIT: usize = 64;

// The most attribute records and namespace spellings that package parsing has
// held at once on this thread, for tests of its memory bounds.
#[cfg(test)]
thread_local! {
    static ATTRIBUTE_PEAK: Cell<usize> = const { Cell::new(0) };
    static SPELLING_PEAK: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn record_peak(peak: &'static LocalKey<Cell<usize>>, value: usize) {
    peak.set(peak.get().max(value));
}

pub(super) fn xml_nodes(bytes: &[u8], retain: Retain) -> Result<Vec<Node>> {
    part_xml(bytes, retain).map(|part| part.nodes)
}

/// Parses and validates a whole package part, keeping its root and the
/// elements that `retain` selects when they start. Memory therefore grows
/// with the retained elements, not with unrelated markup.
pub(super) fn part_xml(bytes: &[u8], retain: Retain) -> Result<PartXml> {
    let mut nodes: Vec<Node> = Vec::new();
    // Each open element's index among the retained nodes, if it was retained.
    let process_content = walk_part(bytes, |tag, parent: Option<&Option<usize>>| {
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
        let tag_attributes = tag.spans.clone().map(|index| tag.attributes.view(index));
        for attribute in tag_attributes {
            let qualified = QName(attribute.name);
            // Namespace declarations are not attributes of a node.
            if qualified.as_namespace_binding().is_some() {
                continue;
            }
            let id = tag
                .namespaces
                .resolve(tag.reader, qualified, NameKind::Attribute)?;
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
pub(super) struct Tag<'visit, 'xml> {
    /// How many elements enclose this one: the root's depth is zero.
    pub(super) depth: usize,
    namespace: NamespaceId,
    pub(super) local: &'visit str,
    namespaces: &'visit mut NamespacePool,
    reader: &'visit NamespaceReader<'xml>,
    attributes: &'visit Attributes<'xml>,
    spans: Range<usize>,
}

impl<'xml> Tag<'_, 'xml> {
    pub(super) fn namespace(&self) -> &str {
        self.namespaces.uri(self.namespace)
    }

    /// The value of the unprefixed attribute `name`, which has no namespace,
    /// borrowed from the part unless XML normalization changed it.
    pub(super) fn attribute(&self, name: &str) -> Option<Cow<'xml, str>> {
        self.attributes.find_value(self.spans.clone(), name)
    }
}

/// Parses and validates a whole package part with the worksheet parser's
/// rules. Each start tag goes to `visit` with what `visit` returned for its
/// parent, if any; the value is kept while the element is open. Returns
/// whether any element of the part has a non-empty mc:ProcessContent
/// attribute.
pub(super) fn walk_part<'xml, T>(
    bytes: &'xml [u8],
    mut visit: impl FnMut(&mut Tag<'_, 'xml>, Option<&T>) -> Result<T>,
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
                let namespace = namespaces.resolve(&reader, name, NameKind::Element)?;
                attributes.clear();
                let spans = xml::attribute_spans(
                    start + 1 + name.as_ref().len(),
                    end,
                    &mut attributes,
                    &mut namespaces,
                    &reader,
                    &mut checks,
                )?;
                // The part's attributes matter as a whole only as
                // mc:ProcessContent.
                let tag_attributes = spans.clone().map(|index| attributes.view(index));
                for attribute in tag_attributes {
                    let qualified = QName(attribute.name);
                    if qualified.local_name().into_inner() != "ProcessContent"
                        || qualified.as_namespace_binding().is_some()
                    {
                        continue;
                    }
                    let id = namespaces.resolve(&reader, qualified, NameKind::Attribute)?;
                    process_content |= &**namespaces.uri(id) == COMPATIBILITY_NAMESPACE
                        && !xml::trim_whitespace(attribute.value).is_empty();
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
//
// Each name is held twice: by its namespace's set, for lookup, and by
// `added`, the log that closing scopes truncate. `Rc` shares one allocation
// between the two.
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
        if length >= self.added.len() {
            return;
        }
        for (namespace, local) in self.added.drain(length..) {
            let names = self
                .names
                .get_mut(&namespace)
                .expect("`insert` and `renumber` add every logged name to its namespace's set");
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

pub(super) fn reject_promoted_workbook_properties(xml: &[u8]) -> Result<()> {
    #[derive(Clone, Copy)]
    enum Parent {
        Workbook,
        Promoted,
        AlternateContent { promoted: bool },
        Opaque,
    }
    // An open element, and the length of each rule list to restore when it
    // closes.
    struct Scope {
        parent: Parent,
        checkpoint: [usize; 2],
    }
    // Indexes into `rules`.
    const IGNORABLE: usize = 0;
    const PROCESS_CONTENT: usize = 1;
    let mut reader = NamespaceReader::from_reader(xml);
    let mut namespaces = NamespacePool::new();
    // The pool in use before the last restart, kept for its allocations.
    let mut previous = NamespacePool::new();
    // Namespace / expanded-name lookup stays bounded even for long inherited
    // rule lists. Only newly inserted names need removing when a scope closes.
    let mut rules: [CompatibilityNames; 2] = Default::default();
    let mut stack: Vec<Scope> = Vec::new();
    loop {
        let event = reader.read_event().map_err(Error::xml)?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                // As in part_xml, the pool starts over past a limit. The rules
                // in scope hold IDs, so they move to the other pool, and the
                // limit grows with them to keep that work linear.
                let live: usize = rules.iter().map(|names| names.added.len()).sum();
                #[cfg(test)]
                record_peak(&SPELLING_PEAK, namespaces.spellings());
                if namespaces.spellings() > NAMESPACE_LIMIT + live {
                    previous.clear();
                    mem::swap(&mut namespaces, &mut previous);
                    for names in &mut rules {
                        names.renumber(&previous, &mut namespaces)?;
                    }
                }
                let parent = stack.last().map(|scope| scope.parent);
                let checkpoint = rules.each_ref().map(|names| names.added.len());
                let namespace = namespaces.resolve(&reader, element.name(), NameKind::Element)?;
                let local = element.local_name();
                // Rules on the wrapper itself apply before deciding whether
                // to ignore or process that wrapper.
                if !matches!(parent, Some(Parent::Opaque)) {
                    for attribute in element.attributes() {
                        let attribute = attribute.map_err(|error| Error::Xml(error.to_string()))?;
                        let name = attribute.key.local_name();
                        let attribute_namespace =
                            namespaces.resolve(&reader, attribute.key, NameKind::Attribute)?;
                        if &**namespaces.uri(attribute_namespace) != COMPATIBILITY_NAMESPACE {
                            continue;
                        }
                        let name = name.as_ref();
                        let kind = match name {
                            "Ignorable" => IGNORABLE,
                            "ProcessContent" => PROCESS_CONTENT,
                            _ => continue,
                        };
                        let value = xml::attribute_value(&attribute)?;
                        let tokens = value
                            .split([' ', '\t', '\r', '\n'])
                            .filter(|token| !token.is_empty());
                        for token in tokens {
                            let qualified;
                            let token = if kind == IGNORABLE {
                                qualified = format!("{token}:_");
                                qualified.as_str()
                            } else {
                                token
                            };
                            let (resolved, token_local) =
                                reader.resolver().resolve_element(QName(token));
                            // An unbound directive cannot name a valid
                            // prefixed element in this already validated XML.
                            if !matches!(resolved, ResolveResult::Bound(_)) {
                                continue;
                            }
                            let token_namespace =
                                namespaces.resolve(&reader, QName(token), NameKind::Element)?;
                            let token_name = if kind == IGNORABLE {
                                "*"
                            } else {
                                token_local.as_ref()
                            };
                            rules[kind].insert(token_namespace, token_name);
                        }
                    }
                }
                let uri: &str = namespaces.uri(namespace);
                let current = match parent {
                    None => Parent::Workbook,
                    Some(Parent::Workbook | Parent::Promoted) => {
                        let promoted = matches!(parent, Some(Parent::Promoted));
                        if promoted
                            && namespaces::main_namespace(uri)
                            && local.as_ref() == "workbookPr"
                        {
                            return Err(Error::Unsupported(
                                "workbookPr inside an mc:ProcessContent wrapper may change the date system"
                                    .into(),
                            ));
                        }
                        if uri == COMPATIBILITY_NAMESPACE && local.as_ref() == "AlternateContent" {
                            Parent::AlternateContent { promoted }
                        } else if !namespaces::main_namespace(uri)
                            && uri != COMPATIBILITY_NAMESPACE
                            && rules[IGNORABLE].matches(namespace, "*")
                            && rules[PROCESS_CONTENT].matches(namespace, local.as_ref())
                        {
                            Parent::Promoted
                        } else {
                            Parent::Opaque
                        }
                    }
                    Some(Parent::AlternateContent { promoted })
                        if uri == COMPATIBILITY_NAMESPACE
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
                    stack.push(Scope {
                        parent: current,
                        checkpoint,
                    });
                }
            }
            Event::End(_) => {
                let Scope { checkpoint, .. } = stack.pop().expect("validated XML is balanced");
                for (names, length) in rules.iter_mut().zip(checkpoint) {
                    names.truncate(length);
                }
            }
            Event::Eof => return Ok(()),
            _ => (),
        }
    }
}
