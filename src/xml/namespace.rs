//! Namespace resolution and scoped caches shared by both XML parsers.

use super::attributes::attribute_value;
use crate::{Error, Result};
use quick_xml::events::attributes::Attribute as XmlAttribute;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{Namespace, NamespaceError, NamespaceResolver, QName, ResolveResult};
use quick_xml::reader::Config;
use quick_xml::{Reader, XmlVersion, escape};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

// NsReader checks the reserved `xml` binding against its raw spelling before
// returning an event. A character reference in that URI is legal XML, so use
// the same resolver with a rare retry that omits a validated, redundant `xml`
// declaration. Events and byte positions always refer to the original input.
pub(crate) struct NamespaceReader<'xml> {
    reader: Reader<&'xml [u8]>,
    namespaces: NamespaceResolver,
    pending_pop: bool,
    scope_changes: Vec<bool>,
    namespace_scopes: Vec<NamespaceScope>,
    prefix_scopes: HashMap<String, usize>,
    next_scope_id: usize,
}

struct NamespaceScope {
    id: usize,
    previous: Vec<(String, Option<usize>)>,
}

impl<'xml> NamespaceReader<'xml> {
    pub(crate) fn from_reader(xml: &'xml [u8]) -> Self {
        Self {
            reader: Reader::from_reader(xml),
            namespaces: NamespaceResolver::default(),
            pending_pop: false,
            scope_changes: Vec::new(),
            namespace_scopes: vec![NamespaceScope {
                id: 0,
                previous: Vec::new(),
            }],
            prefix_scopes: HashMap::new(),
            next_scope_id: 0,
        }
    }

    pub(crate) fn from_str(xml: &'xml str) -> Self {
        Self::from_reader(xml.as_bytes())
    }

    pub(crate) fn config_mut(&mut self) -> &mut Config {
        self.reader.config_mut()
    }

    pub(crate) fn buffer_position(&self) -> u64 {
        self.reader.buffer_position()
    }

    pub(crate) fn resolver(&self) -> &NamespaceResolver {
        &self.namespaces
    }

    fn push(&mut self, tag: &BytesStart<'_>) -> quick_xml::Result<()> {
        match self.namespaces.push(tag) {
            Ok(()) => Ok(()),
            Err(error @ NamespaceError::InvalidXmlPrefixBind(_)) => {
                // The failed push already opened its scope and may have
                // registered earlier declarations. Discard it before retrying.
                self.namespaces.pop();
                let name = tag.name();
                self.namespaces.push(&BytesStart::new(name.as_ref()))?;
                for attribute in tag.attributes().with_checks(false) {
                    let attribute = attribute?;
                    if attribute.key.as_ref() == "xmlns:xml" {
                        let value = attribute.normalized_value_with(
                            XmlVersion::Implicit1_0,
                            1,
                            escape::resolve_xml_entity,
                        )?;
                        if value != "http://www.w3.org/XML/1998/namespace" {
                            return Err(error.into());
                        }
                    } else if let Some(prefix) = attribute.key.as_namespace_binding() {
                        // Add raw values directly: reserializing the tag could
                        // change delimiters around literal quotes in attributes.
                        self.namespaces
                            .add(prefix, Namespace(attribute.value.as_ref()))?;
                    }
                }
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn record_scope(&mut self, tag: &BytesStart<'_>) {
        // Ordinary tags need no second attribute scan. A matching word in a
        // name or value does not itself change bindings.
        let mut previous = Vec::new();
        if tag.as_ref().contains("xmlns") {
            let scope = self.namespace_scopes.len();
            for attribute in tag.attributes().with_checks(false) {
                let Ok(attribute) = attribute else {
                    break;
                };
                if attribute.key.as_namespace_binding().is_some() {
                    let prefix = attribute.key.as_ref().strip_prefix("xmlns:").unwrap_or("");
                    let old = self.prefix_scopes.insert(prefix.to_owned(), scope);
                    previous.push((prefix.to_owned(), old));
                }
            }
        }
        let changes = !previous.is_empty();
        if changes {
            self.next_scope_id += 1;
            self.namespace_scopes.push(NamespaceScope {
                id: self.next_scope_id,
                previous,
            });
        }
        self.scope_changes.push(changes);
    }

    pub(crate) fn read_event(&mut self) -> quick_xml::Result<Event<'xml>> {
        if self.pending_pop {
            self.namespaces.pop();
            if self.scope_changes.pop().unwrap_or(false) {
                let scope = self
                    .namespace_scopes
                    .pop()
                    .expect("`record_scope` pushed a scope for the tag that changed bindings");
                for (prefix, previous) in scope.previous.into_iter().rev() {
                    if let Some(previous) = previous {
                        self.prefix_scopes.insert(prefix, previous);
                    } else {
                        self.prefix_scopes.remove(&prefix);
                    }
                }
            }
            self.pending_pop = false;
        }
        let event = self.reader.read_event()?;
        match &event {
            Event::Start(tag) | Event::Empty(tag) => {
                self.push(tag)?;
                self.record_scope(tag);
                self.pending_pop = matches!(event, Event::Empty(_));
            }
            Event::End(_) => self.pending_pop = true,
            _ => {}
        }
        Ok(event)
    }
}

/// Resolved namespaces by prefix for one reader's active declaration scopes.
/// Unrelated declarations do not evict inherited bindings, and ending a scope
/// restores its parent's cache. Storage stays bounded by active bindings.
struct NamespaceCache<T> {
    scopes: Vec<CachedNamespaceScope<T>>,
    last: LastLookup<T>,
}

struct CachedNamespaceScope<T> {
    id: usize,
    prefixes: HashMap<String, T>,
}

/// The prefix most recently looked up or inserted, with its value until the
/// cache moves to another scope.
struct LastLookup<T> {
    prefix: String,
    value: Option<T>,
}

/// Whether a name is an element's or an attribute's, which decides whether
/// an unprefixed name takes the default namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NameKind {
    Element,
    Attribute,
}

impl<T> Default for NamespaceCache<T> {
    fn default() -> Self {
        Self {
            scopes: vec![CachedNamespaceScope {
                id: 0,
                prefixes: HashMap::new(),
            }],
            last: LastLookup {
                prefix: String::new(),
                value: None,
            },
        }
    }
}

impl<T: Clone> NamespaceCache<T> {
    fn key(name: QName<'_>, kind: NameKind) -> Option<&str> {
        match name.prefix() {
            Some(prefix) => Some(prefix.into_inner()),
            // Unprefixed attributes never inherit the default namespace.
            None if kind == NameKind::Attribute => None,
            None => Some(""),
        }
    }

    fn synchronize(&mut self, reader: &NamespaceReader<'_>) {
        if self.scopes.last().map(|scope| scope.id)
            == reader.namespace_scopes.last().map(|scope| scope.id)
        {
            return;
        }
        let common = self
            .scopes
            .iter()
            .zip(&reader.namespace_scopes)
            .take_while(|(cached, active)| cached.id == active.id)
            .count();
        self.scopes.truncate(common);
        self.scopes
            .extend(
                reader.namespace_scopes[common..]
                    .iter()
                    .map(|scope| CachedNamespaceScope {
                        id: scope.id,
                        prefixes: HashMap::new(),
                    }),
            );
        self.last.value = None;
    }

    fn scope(&self, reader: &NamespaceReader<'_>, prefix: &str) -> usize {
        reader.prefix_scopes.get(prefix).copied().unwrap_or(0)
    }

    fn remember(&mut self, prefix: &str, value: T) {
        self.last.prefix.clear();
        self.last.prefix.push_str(prefix);
        self.last.value = Some(value);
    }

    fn get(&mut self, reader: &NamespaceReader<'_>, name: QName<'_>, kind: NameKind) -> Option<T> {
        let prefix = Self::key(name, kind)?;
        self.synchronize(reader);
        if self.last.prefix == prefix
            && let Some(value) = &self.last.value
        {
            return Some(value.clone());
        }
        let scope = self.scope(reader, prefix);
        let value = self.scopes[scope].prefixes.get(prefix)?.clone();
        self.remember(prefix, value.clone());
        Some(value)
    }

    fn insert(&mut self, reader: &NamespaceReader<'_>, name: QName<'_>, kind: NameKind, value: T) {
        if let Some(prefix) = Self::key(name, kind) {
            self.synchronize(reader);
            let scope = self.scope(reader, prefix);
            self.scopes[scope]
                .prefixes
                .insert(prefix.to_owned(), value.clone());
            self.remember(prefix, value);
        }
    }
}

/// Document-local identity of a normalized namespace URI; zero is unbound.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct NamespaceId(u32);

impl NamespaceId {
    const NONE: Self = Self(0);

    pub(crate) fn is_unbound(self) -> bool {
        self == Self::NONE
    }
}

/// The normalized namespace URIs a parser has resolved, each with its ID.
pub(crate) struct NamespacePool {
    // Resolve inherited bindings by prefix without revisiting a potentially
    // large URI. Nested declarations retain the surrounding scope's cache.
    prefixes: NamespaceCache<NamespaceId>,
    raw: HashMap<String, NamespaceId>,
    // Each URI is allocated once and shared by reference count between this
    // map, the ID table, and callers' records that keep it past a `clear`,
    // such as package discovery's retained nodes. Unique ownership would copy
    // a potentially long URI into every one of them.
    canonical: HashMap<Arc<str>, NamespaceId>,
    values: Vec<Arc<str>>,
}

impl Default for NamespacePool {
    fn default() -> Self {
        Self::new()
    }
}

impl NamespacePool {
    pub(crate) fn new() -> Self {
        Self {
            prefixes: NamespaceCache::default(),
            raw: HashMap::new(),
            canonical: HashMap::new(),
            values: vec![Arc::from("")],
        }
    }

    /// The namespace of an element or attribute name in the reader's scope.
    pub(crate) fn resolve(
        &mut self,
        reader: &NamespaceReader<'_>,
        name: QName<'_>,
        kind: NameKind,
    ) -> Result<NamespaceId> {
        if let Some(id) = self.prefixes.get(reader, name, kind) {
            return Ok(id);
        }
        let (namespace, _) = match kind {
            NameKind::Element => reader.resolver().resolve_element(name),
            NameKind::Attribute => reader.resolver().resolve_attribute(name),
        };
        let id = self.resolve_uncached(namespace)?;
        self.prefixes.insert(reader, name, kind, id);
        Ok(id)
    }

    fn resolve_uncached(&mut self, result: ResolveResult<'_>) -> Result<NamespaceId> {
        match result {
            ResolveResult::Unbound => Ok(NamespaceId::NONE),
            ResolveResult::Bound(raw) => {
                if let Some(&id) = self.raw.get(raw.as_ref()) {
                    return Ok(id);
                }
                let value = namespace(ResolveResult::Bound(raw))?;
                let id = if value.is_empty() {
                    NamespaceId::NONE
                } else {
                    self.intern(value.as_str())?
                };
                self.raw.insert(raw.as_ref().to_owned(), id);
                Ok(id)
            }
            unknown => namespace(unknown).map(|_| NamespaceId::NONE),
        }
    }

    /// The ID of a normalized, non-empty URI, which it receives if it has none.
    pub(crate) fn intern(&mut self, uri: &str) -> Result<NamespaceId> {
        if let Some(&id) = self.canonical.get(uri) {
            return Ok(id);
        }
        let id = NamespaceId(
            u32::try_from(self.values.len())
                .map_err(|_| Error::Unsupported("too many XML namespaces".into()))?,
        );
        // Only the URI table shares strings; elements carry plain IDs,
        // avoiding reference-count operations on the hot path.
        let uri: Arc<str> = uri.into();
        self.values.push(Arc::clone(&uri));
        self.canonical.insert(uri, id);
        Ok(id)
    }

    /// The normalized URI with this ID, empty when unbound.
    pub(crate) fn uri(&self, id: NamespaceId) -> &Arc<str> {
        &self.values[id.0 as usize]
    }

    /// The ID of a normalized URI that this pool has resolved.
    pub(crate) fn id(&self, uri: &str) -> Option<NamespaceId> {
        self.canonical.get(uri).copied()
    }

    /// How many distinct URI spellings this pool has resolved since it was
    /// created or cleared. Each adds at most one URI, so a parser that keeps
    /// no IDs can clear the pool once this passes a limit.
    pub(crate) fn spellings(&self) -> usize {
        self.raw.len()
    }

    /// Forgets every URI, ID and cached binding but keeps the allocations,
    /// so that a pool cleared again and again does not regrow its tables.
    pub(crate) fn clear(&mut self) {
        self.prefixes = NamespaceCache::default();
        self.raw.clear();
        self.canonical.clear();
        self.values.truncate(1);
    }
}

// Resolve entity references before comparing URIs or expanded attribute names.
fn namespace(result: ResolveResult<'_>) -> Result<String> {
    match result {
        ResolveResult::Bound(raw) => {
            let attribute = XmlAttribute {
                key: QName("xmlns"),
                value: Cow::Borrowed(raw.as_ref()),
            };
            Ok(attribute_value(&attribute)?.into_owned())
        }
        ResolveResult::Unbound => Ok(String::new()),
        ResolveResult::Unknown(prefix) => {
            Err(Error::Xml(format!("unbound namespace prefix {prefix}")))
        }
    }
}

#[cfg(test)]
#[path = "tests/namespace.rs"]
mod tests;
