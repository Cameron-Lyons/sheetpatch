//! XML document structure, names, declarations, and encoding rules.

use super::namespace::NamespaceReader;
use super::text::{self, valid_text};
use crate::{Error, Result};
use quick_xml::events::{BytesDecl, BytesStart, Event};

fn name_start(character: char) -> bool {
    matches!(character,
        'A'..='Z' | '_' | 'a'..='z' |
        '\u{c0}'..='\u{d6}' | '\u{d8}'..='\u{f6}' | '\u{f8}'..='\u{2ff}' |
        '\u{370}'..='\u{37d}' | '\u{37f}'..='\u{1fff}' | '\u{200c}'..='\u{200d}' |
        '\u{2070}'..='\u{218f}' | '\u{2c00}'..='\u{2fef}' | '\u{3001}'..='\u{d7ff}' |
        '\u{f900}'..='\u{fdcf}' | '\u{fdf0}'..='\u{fffd}' | '\u{10000}'..='\u{effff}')
}

fn name_character(character: char) -> bool {
    name_start(character)
        || matches!(character,
            '-' | '.' | '0'..='9' | '\u{b7}' | '\u{300}'..='\u{36f}' | '\u{203f}'..='\u{2040}')
}

fn ncname(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(name_start) && characters.all(name_character)
}

// Element and attribute names follow Namespaces in XML's QName grammar.
pub(super) fn valid_qname(name: &str) -> bool {
    match name.split_once(':') {
        Some((prefix, local)) => ncname(prefix) && ncname(local),
        None => ncname(name),
    }
}

// PI targets follow XML's Name grammar, which allows colons without a prefix.
fn valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(|c| c == ':' || name_start(c))
        && characters.all(|c| c == ':' || name_character(c))
}

// The deepest element nesting accepted in an XML part. Deeper input is
// refused as unsupported rather than malformed.
const MAX_DEPTH: usize = 256;

// XML whitespace is exactly these four characters, unlike Rust's ASCII and
// Unicode whitespace sets.
pub(super) fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn whitespace(text: &str) -> bool {
    text.bytes().all(is_space)
}

// Schema numeric values collapse XML whitespace, rather than Rust's broader
// Unicode whitespace set. Internal whitespace remains invalid for a number.
pub(crate) fn trim_whitespace(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\r', '\n'])
}

fn validate_attribute_spacing(start: &BytesStart<'_>) -> Result<()> {
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

fn validate_declaration(declaration: &BytesDecl<'_>) -> Result<()> {
    let content = declaration.as_ref();
    let start = BytesStart::from_content(content, 3);
    validate_attribute_spacing(&start)?;
    let mut version_seen = false;
    let mut encoding_seen = false;
    let mut standalone_seen = false;
    for (index, attribute) in start.attributes().enumerate() {
        let attribute = attribute.map_err(|error| Error::Xml(error.to_string()))?;
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
const BYTE_ORDER_MARK: &[u8] = &[0xef, 0xbb, 0xbf];

/// The rules every XML part follows as a whole: its size, encoding and
/// characters, its declaration, one root element, and what may appear outside
/// it. Both parsers check each event with these rules and each tag's
/// attributes with `attribute_spans`, so they accept exactly the same parts and
/// report the same errors.
pub(crate) struct DocumentRules {
    root_seen: bool,
    declaration_seen: bool,
    // The length of a leading byte order mark. quick-xml strips it without
    // counting it in buffer_position(), so `position` adds it back.
    bom: usize,
}

impl DocumentRules {
    /// Checks a whole part's size, encoding and characters, and returns it as
    /// text with the rules for its events.
    pub(crate) fn source(xml: &[u8]) -> Result<(&str, Self)> {
        // Enforce the compact records' offset range at the parser boundary,
        // independently of the archive and edit size limits.
        if xml.len() >= u32::MAX as usize {
            return Err(Error::Unsupported(
                "XML part exceeds the 4 GiB index limit".into(),
            ));
        }
        let source = str::from_utf8(xml)
            .map_err(|error| Error::Xml(format!("XML must be UTF-8: {error}")))?;
        if !valid_text(source) {
            return Err(Error::Xml("invalid XML character".into()));
        }
        let bom = if xml.starts_with(BYTE_ORDER_MARK) {
            BYTE_ORDER_MARK.len()
        } else {
            0
        };
        // After the first byte order mark, U+FEFF is text before the root.
        // The reader would silently strip it too.
        if xml[bom..].starts_with(BYTE_ORDER_MARK) {
            return Err(Error::Xml("text outside the XML root".into()));
        }
        let rules = Self {
            root_seen: false,
            declaration_seen: false,
            bom,
        };
        Ok((source, rules))
    }

    /// A reader for the events of the text that `source` returned.
    pub(crate) fn reader<'xml>(&self, source: &'xml str) -> NamespaceReader<'xml> {
        let mut reader = NamespaceReader::from_str(&source[self.bom..]);
        reader.config_mut().check_comments = true;
        reader
    }

    /// The reader's position as an offset into the whole source.
    pub(crate) fn position(&self, reader: &NamespaceReader<'_>) -> usize {
        reader.buffer_position() as usize + self.bom
    }

    /// Checks an element named `name` that opens inside `depth` others.
    // This and `event` run for nearly every event of a worksheet. Called out
    // of line from another module, they slowed its parse by more than 10%.
    #[inline]
    pub(crate) fn start_element(&mut self, depth: usize, name: &str) -> Result<()> {
        if depth >= MAX_DEPTH {
            return Err(Error::Unsupported(format!(
                "XML nesting deeper than {MAX_DEPTH} elements"
            )));
        }
        if depth == 0 {
            if self.root_seen {
                return Err(Error::Xml("multiple XML root elements".into()));
            }
            self.root_seen = true;
        }
        if !valid_qname(name) {
            return Err(Error::Xml("invalid XML element name".into()));
        }
        // Namespace declarations are checked with the attributes.
        if name.starts_with("xmlns:") {
            return Err(Error::Xml("xmlns cannot be an element prefix".into()));
        }
        Ok(())
    }

    /// Checks an event other than a tag, which starts at offset `start` inside
    /// `depth` elements. Tags are checked by `start_element` and
    /// `attribute_spans`.
    #[inline]
    pub(crate) fn event(&mut self, event: &Event<'_>, start: usize, depth: usize) -> Result<()> {
        match event {
            Event::Text(text) => {
                let raw = text.as_ref();
                if raw.contains("]]>") {
                    return Err(Error::Xml("CDATA terminator in ordinary XML text".into()));
                }
                if depth == 0 && !whitespace(raw) {
                    return Err(Error::Xml("text outside the XML root".into()));
                }
            }
            Event::GeneralRef(entity) => {
                if depth == 0 {
                    return Err(Error::Xml("entity reference outside the XML root".into()));
                }
                text::reference(entity)?;
            }
            Event::CData(_) if depth == 0 => {
                return Err(Error::Xml("CDATA outside the XML root".into()));
            }
            Event::PI(instruction) => {
                let target = instruction.target();
                if !valid_name(target) || target.eq_ignore_ascii_case("xml") {
                    return Err(Error::Xml(
                        "invalid XML processing instruction target".into(),
                    ));
                }
            }
            Event::DocType(_) => {
                return Err(Error::Unsupported("XML document type declarations".into()));
            }
            Event::Decl(declaration) => {
                if self.root_seen || self.declaration_seen || start != self.bom {
                    return Err(Error::Xml("misplaced XML declaration".into()));
                }
                self.declaration_seen = true;
                validate_declaration(declaration)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Checks that the document is complete, with `depth` elements still open
    /// at its end.
    pub(crate) fn finish(&self, depth: usize) -> Result<()> {
        if depth != 0 || !self.root_seen {
            return Err(Error::Xml("incomplete XML document".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests/document.rs"]
mod tests;
