//! Shared XML invariants for values and package metadata.

use crate::{Error, Result};
use quick_xml::{
    events::{BytesDecl, BytesStart, attributes::Attribute},
    name::{QName, ResolveResult},
};
use std::borrow::Cow;

pub(crate) fn valid_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
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

pub(crate) fn whitespace(text: &str) -> bool {
    text.bytes()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
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
                if raw
                    .get(position + 1)
                    .is_some_and(|next| !matches!(next, b' ' | b'\t' | b'\r' | b'\n'))
                {
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
    const XML: &str = "http://www.w3.org/XML/1998/namespace";
    const XMLNS: &str = "http://www.w3.org/2000/xmlns/";
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
        let value = attribute.normalized_value(quick_xml::XmlVersion::Implicit1_0)?;
        if !value.chars().all(valid_char) {
            return Err(Error::Xml("invalid XML namespace character".into()));
        }
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
            let value = attribute.normalized_value(quick_xml::XmlVersion::Implicit1_0)?;
            if !value.chars().all(valid_char) {
                return Err(Error::Xml("invalid XML namespace character".into()));
            }
            Ok(value.into_owned())
        }
        ResolveResult::Unbound => Ok(String::new()),
        ResolveResult::Unknown(prefix) => {
            Err(Error::Xml(format!("unbound namespace prefix {prefix}")))
        }
    }
}

// ST_Xstring escapes represent UTF-16 code units, including surrogate pairs.
// Decode only once so the escaped underscore in `_x005F_x0041_` stays literal.
pub(crate) fn excel_text(text: &str) -> Result<String> {
    let bytes = text.as_bytes();
    if !bytes.contains(&b'_') {
        return Ok(text.to_owned());
    }
    let mut units = Vec::with_capacity(text.len());
    let mut position = 0;
    while position < bytes.len() {
        if bytes[position] == b'_'
            && position + 7 <= bytes.len()
            && matches!(bytes[position + 1], b'x' | b'X')
            && bytes[position + 2..position + 6]
                .iter()
                .all(u8::is_ascii_hexdigit)
            && bytes[position + 6] == b'_'
        {
            units.push(u16::from_str_radix(&text[position + 2..position + 6], 16).unwrap());
            position += 7;
        } else {
            let character = text[position..].chars().next().unwrap();
            let mut encoded = [0; 2];
            units.extend_from_slice(character.encode_utf16(&mut encoded));
            position += character.len_utf8();
        }
    }
    String::from_utf16(&units).map_err(|_| Error::Xml("invalid UTF-16 Excel text escape".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for text in ["\u{a0}", "\u{2000}", "\u{85}", "text"] {
            assert!(!whitespace(text));
        }
    }

    #[test]
    fn excel_escape_decoding_is_single_pass_and_utf16_aware() {
        assert_eq!(excel_text("Plain 雪😀").unwrap(), "Plain 雪😀");
        assert_eq!(excel_text("_x005F_x0041_").unwrap(), "_x0041_");
        assert_eq!(excel_text("_xD83D__xDE00_").unwrap(), "😀");
        assert_eq!(excel_text("_X0041_雪").unwrap(), "A雪");
        assert!(excel_text("_xD800_").is_err());
    }
}
