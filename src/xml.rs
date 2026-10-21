//! Shared XML invariants for values and package metadata.

use crate::{Error, Result};
use quick_xml::{
    events::{BytesDecl, BytesRef, BytesStart, Event, attributes::Attribute},
    name::{Namespace, NamespaceError, NamespaceResolver, QName, ResolveResult},
};
use std::{borrow::Cow, collections::HashMap, fmt::Write as _};

// NsReader checks the reserved `xml` binding against its raw spelling before
// returning an event. A character reference in that URI is legal XML, so use
// the same resolver with a rare retry that omits a validated, redundant `xml`
// declaration. Events and byte positions always refer to the original input.
pub(crate) struct NamespaceReader<'xml> {
    reader: quick_xml::Reader<&'xml [u8]>,
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
            reader: quick_xml::Reader::from_reader(xml),
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

    pub(crate) fn config_mut(&mut self) -> &mut quick_xml::reader::Config {
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
                            quick_xml::XmlVersion::Implicit1_0,
                            1,
                            quick_xml::escape::resolve_xml_entity,
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
                    .expect("recorded namespace scope");
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
pub(crate) struct NamespaceCache<T> {
    scopes: Vec<CachedNamespaceScope<T>>,
    last: (String, Option<T>),
}

struct CachedNamespaceScope<T> {
    id: usize,
    prefixes: HashMap<String, T>,
}

impl<T> Default for NamespaceCache<T> {
    fn default() -> Self {
        Self {
            scopes: vec![CachedNamespaceScope {
                id: 0,
                prefixes: HashMap::new(),
            }],
            last: (String::new(), None),
        }
    }
}

impl<T: Clone> NamespaceCache<T> {
    fn key(name: QName<'_>, attribute: bool) -> Option<&str> {
        match name.prefix() {
            Some(prefix) => Some(prefix.into_inner()),
            // Unprefixed attributes never inherit the default namespace.
            None if attribute => None,
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
        self.last.1 = None;
    }

    fn scope(&self, reader: &NamespaceReader<'_>, prefix: &str) -> usize {
        reader.prefix_scopes.get(prefix).copied().unwrap_or(0)
    }

    fn remember(&mut self, prefix: &str, value: T) {
        self.last.0.clear();
        self.last.0.push_str(prefix);
        self.last.1 = Some(value);
    }

    pub(crate) fn get(
        &mut self,
        reader: &NamespaceReader<'_>,
        name: QName<'_>,
        attribute: bool,
    ) -> Option<T> {
        let prefix = Self::key(name, attribute)?;
        self.synchronize(reader);
        if self.last.0 == prefix
            && let Some(value) = &self.last.1
        {
            return Some(value.clone());
        }
        let scope = self.scope(reader, prefix);
        let value = self.scopes[scope].prefixes.get(prefix)?.clone();
        self.remember(prefix, value.clone());
        Some(value)
    }

    pub(crate) fn insert(
        &mut self,
        reader: &NamespaceReader<'_>,
        name: QName<'_>,
        attribute: bool,
        value: T,
    ) {
        if let Some(prefix) = Self::key(name, attribute) {
            self.synchronize(reader);
            let scope = self.scope(reader, prefix);
            self.scopes[scope]
                .prefixes
                .insert(prefix.to_owned(), value.clone());
            self.remember(prefix, value);
        }
    }
}

pub(crate) fn valid_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

/// Whether XML 1.0 allows every character of `text`: exactly
/// `text.chars().all(valid_char)`, decided from the UTF-8 bytes without
/// decoding them.
///
/// A `str` holds no surrogates and nothing above U+10FFFF, so the characters
/// left to refuse are the C0 controls other than tab, LF and CR, and U+FFFE
/// and U+FFFF. UTF-8 encodes each control as one byte below 0x20, a value no
/// multibyte sequence contains, and the two noncharacters as EF BF BE and
/// EF BF BF. Each chunk is first folded, without branching so that the fold
/// vectorizes, into whether it holds such a control or any EF byte. ASCII and
/// most other text holds neither; a chunk that does, such as one of
/// full-width forms, takes a second look.
pub(crate) fn valid_text(text: &str) -> bool {
    valid_text_in_chunks::<128>(text)
}

// `valid_text` with `CHUNK` bytes tested together. Tests use small chunks to
// place every character across a chunk boundary cheaply.
fn valid_text_in_chunks<const CHUNK: usize>(text: &str) -> bool {
    fn any(bytes: &[u8], test: impl Fn(u8) -> bool) -> bool {
        bytes.iter().fold(false, |any, &byte| any | test(byte))
    }
    fn control(byte: u8) -> bool {
        (byte < 0x20) & !matches!(byte, b'\t' | b'\n' | b'\r')
    }
    let bytes = text.as_bytes();
    // Whether EF BF BE or EF BF BF begins at one of the `length` bytes from
    // `start`. Such a sequence can cross into the next chunk, so the bytes
    // after each come from the whole text. Only the last two bytes of a
    // `str` lack two successors, and no three-byte sequence begins there.
    let noncharacter = |start: usize, length: usize| {
        let second = bytes.get(start + 1..).unwrap_or_default();
        let third = bytes.get(start + 2..).unwrap_or_default();
        bytes[start..start + length]
            .iter()
            .zip(second)
            .zip(third)
            .fold(false, |any, ((&first, &second), &third)| {
                // `third | 1` is 0xBF exactly for 0xBE and 0xBF.
                any | ((first == 0xef) & (second == 0xbf) & ((third | 1) == 0xbf))
            })
    };
    let invalid = |start: usize, chunk: &[u8]| {
        any(chunk, |byte| control(byte) | (byte == 0xef))
            && (any(chunk, control) || noncharacter(start, chunk.len()))
    };
    let (chunks, rest) = bytes.as_chunks::<CHUNK>();
    let mut start = 0;
    for chunk in chunks {
        if invalid(start, chunk) {
            return false;
        }
        start += CHUNK;
    }
    !invalid(start, rest)
}

/// Resolves a reference in XML text: a character reference, or one of the
/// five entities XML 1.0 predefines. Documents with a DTD are refused, so no
/// other entity can be declared. None is recognized either, whatever entity
/// support quick-xml's features gain through dependency feature unification.
pub(crate) fn reference(reference: &BytesRef<'_>) -> Result<char> {
    let character = match reference.as_ref() {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "apos" => '\'',
        "quot" => '"',
        name => match reference.resolve_char_ref() {
            Ok(Some(character)) => character,
            Ok(None) => return Err(Error::Xml(format!("undefined XML entity &{name};"))),
            Err(_) => return Err(Error::Xml("invalid XML character reference".into())),
        },
    };
    if valid_char(character) {
        Ok(character)
    } else {
        Err(Error::Xml("invalid XML character reference".into()))
    }
}

/// An attribute's normalized value, resolving only XML 1.0's predefined
/// entities and character references, as `reference` does in text.
///
/// Every caller has already checked the document's literal characters with
/// `valid_text`. Only a value that normalization changed can therefore hold
/// a character that XML forbids, which a character reference introduced.
pub(crate) fn attribute_value<'a>(attribute: &Attribute<'a>) -> Result<Cow<'a, str>> {
    let value = attribute
        .normalized_value_with(
            quick_xml::XmlVersion::Implicit1_0,
            1,
            quick_xml::escape::resolve_xml_entity,
        )
        .map_err(Error::xml)?;
    if let Cow::Owned(value) = &value
        && !valid_text(value)
    {
        return Err(Error::Xml("invalid XML character reference".into()));
    }
    Ok(value)
}

fn name_start(c: char) -> bool {
    matches!(c,
        'A'..='Z' | '_' | 'a'..='z' |
        '\u{c0}'..='\u{d6}' | '\u{d8}'..='\u{f6}' | '\u{f8}'..='\u{2ff}' |
        '\u{370}'..='\u{37d}' | '\u{37f}'..='\u{1fff}' | '\u{200c}'..='\u{200d}' |
        '\u{2070}'..='\u{218f}' | '\u{2c00}'..='\u{2fef}' | '\u{3001}'..='\u{d7ff}' |
        '\u{f900}'..='\u{fdcf}' | '\u{fdf0}'..='\u{fffd}' | '\u{10000}'..='\u{effff}')
}

fn name_character(c: char) -> bool {
    name_start(c)
        || matches!(c, '-' | '.' | '0'..='9' | '\u{b7}' | '\u{300}'..='\u{36f}' | '\u{203f}'..='\u{2040}')
}

fn ncname(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(name_start) && characters.all(name_character)
}

// Element and attribute names follow Namespaces in XML's QName grammar.
pub(crate) fn valid_qname(name: &str) -> bool {
    match name.split_once(':') {
        Some((prefix, local)) => ncname(prefix) && ncname(local),
        None => ncname(name),
    }
}

// PI targets follow XML's Name grammar, which allows colons without a prefix.
pub(crate) fn valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(|c| c == ':' || name_start(c))
        && characters.all(|c| c == ':' || name_character(c))
}

// The deepest element nesting accepted in an XML part. Deeper input is
// refused as unsupported rather than malformed.
pub(crate) const MAX_DEPTH: usize = 256;

// XML whitespace is exactly these four characters, unlike Rust's ASCII and
// Unicode whitespace sets.
pub(crate) fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

pub(crate) fn whitespace(text: &str) -> bool {
    text.bytes().all(is_space)
}

// Schema numeric values collapse XML whitespace, rather than Rust's broader
// Unicode whitespace set. Internal whitespace remains invalid for a number.
pub(crate) fn trim_whitespace(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\r', '\n'])
}

pub(crate) fn validate_attribute_spacing(start: &BytesStart<'_>) -> Result<()> {
    let raw = start.as_ref().as_bytes();
    let name_length = start.name().as_ref().len();
    let mut quote = None;
    for position in name_length..raw.len() {
        let byte = raw[position];
        match quote {
            Some(delimiter) if byte == delimiter => {
                quote = None;
                if raw.get(position + 1).is_some_and(|&next| !is_space(next)) {
                    return Err(Error::Xml(
                        "missing whitespace between XML attributes".into(),
                    ));
                }
            }
            None if matches!(byte, b'\'' | b'"') => quote = Some(byte),
            _ => {}
        }
    }
    Ok(())
}

// Namespace constraints apply to normalized attribute values. quick-xml's
// resolver also checks reserved bindings, but does so against the raw spelling
// and permits prefix undeclarations used by Namespaces in XML 1.1.
pub(crate) fn validate_namespaces(start: &BytesStart<'_>) -> Result<()> {
    if start.name().as_ref().starts_with("xmlns:") {
        return Err(Error::Xml("xmlns cannot be an element prefix".into()));
    }
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|e| Error::Xml(e.to_string()))?;
        let name = attribute.key.as_ref();
        let prefix = if name == "xmlns" {
            None
        } else if let Some(prefix) = name.strip_prefix("xmlns:") {
            Some(prefix)
        } else {
            continue;
        };
        let value = attribute_value(&attribute)?;
        validate_namespace_binding(prefix, &value)?;
    }
    Ok(())
}

// Checks one declaration's normalized value. `prefix` is `None` for a default
// namespace declaration (`xmlns`), and the declared prefix for `xmlns:prefix`.
pub(crate) fn validate_namespace_binding(prefix: Option<&str>, value: &str) -> Result<()> {
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

pub(crate) fn validate_declaration(declaration: &BytesDecl<'_>) -> Result<()> {
    let content = declaration.as_ref();
    let start = BytesStart::from_content(content, 3);
    validate_attribute_spacing(&start)?;
    let mut version_seen = false;
    let mut encoding_seen = false;
    let mut standalone_seen = false;
    for (index, attribute) in start.attributes().enumerate() {
        let attribute = attribute.map_err(|e| Error::Xml(e.to_string()))?;
        match attribute.key.as_ref() {
            "version" if index == 0 => {
                version_seen = true;
                if attribute.value.as_ref() != "1.0" {
                    return Err(Error::Unsupported("only XML 1.0 is supported".into()));
                }
            }
            "encoding" if version_seen && !encoding_seen && !standalone_seen => {
                encoding_seen = true;
                if !attribute.value.eq_ignore_ascii_case("UTF-8") {
                    return Err(Error::Unsupported("only UTF-8 XML is supported".into()));
                }
            }
            "standalone" if version_seen && !standalone_seen => {
                standalone_seen = true;
                if !matches!(attribute.value.as_ref(), "yes" | "no") {
                    return Err(Error::Xml("invalid XML standalone declaration".into()));
                }
            }
            _ => return Err(Error::Xml("invalid XML declaration attributes".into())),
        }
    }
    if !version_seen {
        return Err(Error::Xml("XML declaration lacks a version".into()));
    }
    Ok(())
}

// quick-xml returns namespace declarations without attribute normalization.
// Resolve entity references before comparing URIs or expanded attribute names.
pub(crate) fn namespace(result: ResolveResult<'_>) -> Result<String> {
    match result {
        ResolveResult::Bound(ns) => {
            let attribute = Attribute {
                key: QName("xmlns"),
                value: Cow::Borrowed(ns.as_ref()),
            };
            Ok(attribute_value(&attribute)?.into_owned())
        }
        ResolveResult::Unbound => Ok(String::new()),
        ResolveResult::Unknown(prefix) => {
            Err(Error::Xml(format!("unbound namespace prefix {prefix}")))
        }
    }
}

// An ST_Xstring escape token, `_xHHHH_`, starts at `position` when this
// `_xHHHH` prefix is followed by an underscore. The decoder and the encoder
// share this test, so encoded text always decodes to the original string.
// ECMA-376 Part 1, 22.9.2.19 specifies a lowercase x; hex digits may use either case.
fn excel_escape_prefix_at(bytes: &[u8], position: usize) -> bool {
    bytes.get(position..position + 6).is_some_and(|prefix| {
        prefix[0] == b'_' && prefix[1] == b'x' && prefix[2..].iter().all(u8::is_ascii_hexdigit)
    })
}

// The inverse of `excel_text`. Characters XML 1.0 forbids, all within the BMP,
// become `_xHHHH_` escapes as Excel writes them. A literal underscore is
// escaped as `_x005F_` where it would otherwise start a token in the output:
// before `xHHHH_`, or before `xHHHH` and a character that is itself escaped.
pub(crate) fn excel_escaped(text: &str) -> Cow<'_, str> {
    if text.chars().all(|c| c != '_' && valid_char(c)) {
        return Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len() + 8);
    for (position, character) in text.char_indices() {
        if character == '_'
            && excel_escape_prefix_at(bytes, position)
            && text[position + 6..]
                .chars()
                .next()
                .is_some_and(|next| next == '_' || !valid_char(next))
        {
            output.push_str("_x005F_");
        } else if valid_char(character) {
            output.push(character);
        } else {
            write!(output, "_x{:04X}_", u32::from(character))
                .expect("writing to a String cannot fail");
        }
    }
    Cow::Owned(output)
}

// ST_Xstring escapes represent UTF-16 code units, including surrogate pairs.
// Decode only once so the escaped underscore in `_x005F_x0041_` stays literal.
pub(crate) fn excel_text(text: &str) -> Result<String> {
    let Some(mut position) = text.find('_') else {
        return Ok(text.to_owned());
    };
    let bytes = text.as_bytes();
    let unit_at = |position| {
        (excel_escape_prefix_at(bytes, position) && bytes.get(position + 6) == Some(&b'_'))
            .then(|| u16::from_str_radix(&text[position + 2..position + 6], 16).unwrap())
    };
    // A lone surrogate is valid ST_Xstring, but no Rust string can hold it,
    // so it is unsupported rather than malformed.
    let unpaired = || Error::Unsupported("unpaired UTF-16 surrogate in Excel text escape".into());
    // Copy ordinary UTF-8 spans directly instead of converting the entire
    // value to a temporary UTF-16 buffer merely because it has an underscore.
    let mut output = String::with_capacity(text.len());
    let mut start = 0;
    loop {
        output.push_str(&text[start..position]);
        if let Some(unit) = unit_at(position) {
            position += 7;
            let character = if (0xd800..=0xdbff).contains(&unit) {
                let low = unit_at(position)
                    .filter(|low| (0xdc00..=0xdfff).contains(low))
                    .ok_or_else(unpaired)?;
                position += 7;
                char::from_u32(
                    0x10000 + ((u32::from(unit) - 0xd800) << 10) + u32::from(low) - 0xdc00,
                )
                .ok_or_else(unpaired)?
            } else {
                char::from_u32(u32::from(unit)).ok_or_else(unpaired)?
            };
            output.push(character);
        } else {
            output.push('_');
            position += 1;
        }
        start = position;
        match text[start..].find('_') {
            Some(offset) => position = start + offset,
            None => break,
        }
    }
    output.push_str(&text[start..]);
    // Escapes can shorten text considerably. Decoded text, such as a cell's
    // value or a sheet name, should not retain the capacity of its spelling.
    output.shrink_to_fit();
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_scan_matches_the_character_predicate() {
        let expected = |text: &str| text.chars().all(valid_char);
        // Every scalar value alone, which the scan reads as a remainder, and
        // across a boundary between full chunks. One prefix leaves its chunk
        // to be flagged by the character alone; the other flags it anyway.
        let mut text = String::new();
        for character in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
            for prefix in ["", "abc", "\u{fffd}"] {
                text.clear();
                text.push_str(prefix);
                text.push(character);
                text.push_str(&"abcdefg"[prefix.len()..]);
                assert_eq!(
                    valid_text_in_chunks::<4>(&text),
                    expected(&text),
                    "{text:?}"
                );
                if prefix.is_empty() {
                    let alone = &text[..character.len_utf8()];
                    assert_eq!(valid_text(alone), expected(alone), "{alone:?}");
                }
            }
        }
        // The scan's special cases and their near misses, at every offset of
        // texts that end in and around each of the first chunk boundaries.
        let specials = [
            "\0",
            "\u{1}",
            "\u{1f}",
            "\t",
            "\n",
            "\r",
            " ",
            "\u{7f}",
            "\u{ef}",
            "\u{efbf}",
            "\u{f000}",
            "\u{ffbe}",
            "\u{fffd}",
            "\u{fffe}",
            "\u{ffff}",
            "\u{10ffff}",
        ];
        for length in (0..8).chain(124..132).chain(252..260) {
            for position in 0..=length {
                for special in specials {
                    text.clear();
                    text.extend(std::iter::repeat_n('a', position));
                    text.push_str(special);
                    text.extend(std::iter::repeat_n('a', length - position));
                    assert_eq!(valid_text(&text), expected(&text), "{text:?}");
                }
            }
        }
        let valid = [
            "a",
            " ",
            "\t",
            "\n",
            "\r",
            "\u{7f}",
            "\u{80}",
            "\u{ef}",
            "雪",
            "\u{d7ff}",
            "\u{e000}",
            "\u{efbf}",
            "\u{f8ff}",
            "\u{feff}",
            "\u{ffbe}",
            "\u{fffd}",
            "\u{10000}",
            "\u{10ffff}",
        ];
        let invalid = ["\0", "\u{8}", "\u{1f}", "\u{fffe}", "\u{ffff}"];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            text.clear();
            // Invalid pieces are rare, so that one often falls in a late chunk.
            for _ in 0..next() % 160 {
                let random = next();
                text.push_str(if random % 64 == 0 {
                    invalid[(random >> 8) as usize % invalid.len()]
                } else {
                    valid[(random >> 8) as usize % valid.len()]
                });
            }
            assert_eq!(valid_text(&text), expected(&text), "{text:?}");
            assert_eq!(
                valid_text_in_chunks::<4>(&text),
                expected(&text),
                "{text:?}"
            );
        }
    }

    #[test]
    fn references_resolve_only_xml_10_entities_and_legal_characters() {
        let resolve = |name: &str| match reference(&BytesRef::new(name)) {
            Ok(character) => Ok(character),
            Err(Error::Xml(message)) => Err(message),
            Err(error) => panic!("{name:?}: {error:?}"),
        };
        let invalid = || Err("invalid XML character reference".to_owned());
        let mut name = String::new();
        for codepoint in (0..=0x11_0000).chain([u32::MAX]) {
            let expected = char::from_u32(codepoint)
                .filter(|&c| c != '\0' && valid_char(c))
                .map_or_else(invalid, Ok);
            for spelling in [
                format_args!("#{codepoint}"),
                format_args!("#x{codepoint:x}"),
                format_args!("#x{codepoint:X}"),
            ] {
                name.clear();
                write!(name, "{spelling}").unwrap();
                assert_eq!(resolve(&name), expected, "{name}");
            }
        }
        for (name, character) in [
            ("amp", '&'),
            ("lt", '<'),
            ("gt", '>'),
            ("apos", '\''),
            ("quot", '"'),
            ("#00000065", 'A'),
            ("#x00000041", 'A'),
        ] {
            assert_eq!(resolve(name), Ok(character), "{name}");
        }
        // HTML and differently spelled names are undefined in XML, whatever
        // quick-xml's escape-html feature adds.
        for name in [
            "nbsp",
            "NotEqualTilde",
            "eacute",
            "AMP",
            "Amp",
            "unknown",
            "雪",
            "",
        ] {
            assert_eq!(
                resolve(name),
                Err(format!("undefined XML entity &{name};")),
                "{name}"
            );
        }
        for name in [
            "#",
            "#x",
            "#X41",
            "#-65",
            "#+65",
            "#x+41",
            "#x-41",
            "# 65",
            "#65 ",
            "#999999999999999999999999999999",
            "#xFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
            "#xD800",
            "#xDFFF",
            "#0",
            "#x0",
            "#x1",
            "#xFFFE",
        ] {
            assert_eq!(resolve(name), invalid(), "{name}");
        }
    }

    #[test]
    fn namespace_cache_reuses_inherited_bindings_and_restores_shadowed_parents() {
        let mut xml = String::from("<root xmlns:v='urn:parent'>");
        for _ in 0..32 {
            xml.push_str(
                "<v:item xmlns:q='urn:unrelated'/>\
                 <v:child xmlns:v='urn:child'><v:item/></v:child>\
                 <v:item/>\
                 <v:other xmlns:v='urn:other'/>\
                 <v:item/>",
            );
        }
        xml.push_str("</root>");
        let mut reader = NamespaceReader::from_str(&xml);
        let mut cache = NamespaceCache::default();
        let mut resolutions = HashMap::new();
        let mut values = Vec::new();
        loop {
            match reader.read_event().unwrap() {
                Event::Start(tag) | Event::Empty(tag) if tag.name().prefix().is_some() => {
                    let name = tag.name();
                    let value = cache.get(&reader, name, false).unwrap_or_else(|| {
                        let uri = namespace(reader.resolver().resolve_element(name).0).unwrap();
                        *resolutions.entry(uri.clone()).or_insert(0) += 1;
                        cache.insert(&reader, name, false, uri.clone());
                        uri
                    });
                    assert_eq!(
                        cache.get(&reader, QName("v:attribute"), true),
                        Some(value.clone())
                    );
                    assert!(cache.get(&reader, QName("attribute"), true).is_none());
                    values.push(value);
                }
                Event::Eof => break,
                _ => {}
            }
        }
        assert_eq!(
            values,
            [
                "urn:parent",
                "urn:child",
                "urn:child",
                "urn:parent",
                "urn:other",
                "urn:parent"
            ]
            .repeat(32)
        );
        assert_eq!(resolutions["urn:parent"], 1);
        assert_eq!(resolutions["urn:child"], 32);
        assert_eq!(resolutions["urn:other"], 32);
    }

    #[test]
    fn excel_escape_units_match_utf16_decoding() {
        for unit in 0..=u16::MAX {
            let escaped = format!("before雪_x{unit:04X}_😀after");
            let expected = String::from_utf16(&[unit])
                .ok()
                .map(|text| format!("before雪{text}😀after"));
            assert_eq!(excel_text(&escaped).ok(), expected, "{escaped}");
        }
        // Both ends of the surrogate ranges, valid pairs, and every way a
        // non-surrogate, high surrogate or low surrogate can meet another.
        let units = [
            0, 0x41, 0xd7ff, 0xd800, 0xdbff, 0xdc00, 0xdfff, 0xe000, 0xffff,
        ];
        for first in units {
            for second in units {
                let escaped = format!("_x{first:04X}__x{second:04X}_");
                assert_eq!(
                    excel_text(&escaped).ok(),
                    String::from_utf16(&[first, second]).ok(),
                    "{escaped}"
                );
            }
        }
        for text in [
            "_xD800_",
            "_xD800_雪",
            "_xD800_😀",
            "😀_xDC00_",
            "_xD800__XDC00_",
        ] {
            assert!(excel_text(text).is_err(), "{text}");
        }
    }

    #[test]
    fn escaped_xml_binding_retry_keeps_the_namespace_binding_limit() {
        let mut reader = NamespaceReader::from_str(
            "<root xmlns:a='urn:a' xmlns:xml='http://www.w3.org/XML/1998/namespac&#101;' xmlns:b='urn:b' xmlns:c='urn:c'/>",
        );
        reader.namespaces.set_max_namespace_bindings(2);
        assert!(matches!(
            reader.read_event(),
            Err(quick_xml::Error::Namespace(
                NamespaceError::TooManyBindings(2)
            ))
        ));
    }

    #[test]
    fn namespace_constraints_use_decoded_values_and_xml_10_rules() {
        for raw in [
            "tag xmlns:p=''",
            "tag xmlns:p='http://www.w3.org/XML/1998/namespac&#101;'",
            "tag xmlns:p='http://www.w3.org/2000/xmlns&#47;'",
            "tag xmlns='http://www.w3.org/XML/1998/namespac&#101;'",
            "tag xmlns='http://www.w3.org/2000/xmlns&#47;'",
            "tag xmlns:xml='urn:incorrect'",
            "tag xmlns:xmlns='urn:incorrect'",
            "xmlns:tag",
        ] {
            let name_len = raw.find(' ').unwrap_or(raw.len());
            assert!(
                matches!(
                    validate_namespaces(&BytesStart::from_content(raw, name_len)),
                    Err(Error::Xml(_))
                ),
                "{raw}"
            );
        }
        for raw in [
            "tag xmlns=''",
            "tag xmlns:p='urn&#58;correct'",
            "tag xmlns:xml='http://www.w3.org/XML/1998/namespac&#101;'",
        ] {
            validate_namespaces(&BytesStart::from_content(raw, 3)).unwrap();
        }
    }

    #[test]
    fn qualified_names_accept_the_xml_unicode_ranges() {
        for name in [
            "row",
            "xmlns",
            "xmlns:雪",
            "雪:属性",
            "_one",
            "a-1.b",
            "a\u{300}",
            "\u{10000}:\u{effff}",
        ] {
            assert!(valid_qname(name), "{name}");
        }
        for name in [
            "",
            "0bad",
            ":name",
            "name:",
            "a:b:c",
            "a b",
            "a/b",
            "\u{d7}",
            "\u{300}a",
            "\u{f0000}",
        ] {
            assert!(!valid_qname(name), "{name}");
        }
        assert!(valid_name(":target:one"));
        assert!(!valid_name("1target"));
    }

    #[test]
    fn xml_space_is_limited_to_the_four_defined_characters() {
        assert!(whitespace(" \t\r\n"));
        assert_eq!(trim_whitespace(" \t\r\n17\n\t "), "17");
        let spaces: Vec<u8> = (0..=u8::MAX).filter(|&byte| is_space(byte)).collect();
        assert_eq!(spaces, b"\t\n\r ");
        for text in ["\u{a0}", "\u{2000}", "\u{85}", "text"] {
            assert!(!whitespace(text));
            assert_eq!(trim_whitespace(text), text);
        }
    }

    #[test]
    fn excel_escaping_is_the_exact_inverse_of_decoding() {
        assert!(matches!(excel_escaped("Plain 雪 x0041"), Cow::Borrowed(_)));
        for (text, escaped) in [
            (
                "\0\u{1}\u{8}\u{b}\u{c}\u{e}\u{1f}",
                "_x0000__x0001__x0008__x000B__x000C__x000E__x001F_",
            ),
            ("\u{fffe}\u{ffff}\t\n\r", "_xFFFE__xFFFF_\t\n\r"),
            ("_x0041_ _Xbeef_", "_x005F_x0041_ _Xbeef_"),
            // A literal prefix before an escaped character would otherwise
            // read back as one token.
            ("_x0001\u{1}", "_x005F_x0001_x0001_"),
            ("_X0001\u{1}", "_X0001_x0001_"),
            ("_x0041 _x004G_ _x041_ _", "_x0041 _x004G_ _x041_ _"),
        ] {
            assert_eq!(excel_escaped(text), escaped, "{text:?}");
            assert_eq!(excel_text(escaped).unwrap(), text);
        }
        let pieces = [
            "_", "x", "X", "0", "1", "F", "d", "a", "\u{1}", "\u{fffe}", "\0", "雪", "_x0041_",
            "_x0001", "_xD83D_", "_X005F", "x005F_",
        ];
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..20_000 {
            let mut text = String::new();
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            for step in 0..(state % 12) {
                text.push_str(pieces[(state >> (step * 5 + 4)) as usize % pieces.len()]);
            }
            let escaped = excel_escaped(&text);
            assert!(escaped.chars().all(valid_char), "{text:?}");
            assert_eq!(
                excel_text(&escaped).unwrap(),
                text,
                "{text:?} -> {escaped:?}"
            );
        }
    }

    #[test]
    fn excel_escape_decoding_is_single_pass_and_utf16_aware() {
        assert_eq!(excel_text("Plain 雪😀").unwrap(), "Plain 雪😀");
        assert_eq!(excel_text("_x005F_x0041_").unwrap(), "_x0041_");
        assert_eq!(excel_text("_x005F_X0041_").unwrap(), "_X0041_");
        assert_eq!(excel_text("_xD83D__xDE00_").unwrap(), "😀");
        assert_eq!(excel_text("_xd83d__xde00_").unwrap(), "😀");
        assert_eq!(excel_text("_X0041_雪").unwrap(), "_X0041_雪");
        assert_eq!(excel_text("_XD83D__XDE00_").unwrap(), "_XD83D__XDE00_");
        assert_eq!(excel_text("_XD800_").unwrap(), "_XD800_");
        // A lone surrogate is valid ST_Xstring, which a Rust string cannot hold.
        for text in ["_xD800_", "_xDBFF_x", "_xDC00_", "_xD83D__xD83D__xDE00_"] {
            assert!(
                matches!(
                    excel_text(text),
                    Err(Error::Unsupported(message))
                        if message == "unpaired UTF-16 surrogate in Excel text escape"
                ),
                "{text}"
            );
        }
    }
}
