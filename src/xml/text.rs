//! XML character/reference validation and Excel text escapes.

use crate::{Error, Result};
use quick_xml::events::BytesRef;
#[cfg(test)]
use std::borrow::Cow;
use std::fmt::Write as _;

fn valid_char(character: char) -> bool {
    matches!(character,
        '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' |
        '\u{10000}'..='\u{10ffff}')
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
pub(super) fn valid_text(text: &str) -> bool {
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
#[cfg(test)]
fn excel_escaped(text: &str) -> Cow<'_, str> {
    if text.chars().all(|c| c != '_' && valid_char(c)) {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len() + 8);
    append_escaped(&mut output, text, false);
    Cow::Owned(output)
}

/// Appends text for an XML cell payload, escaping both Excel tokens and XML
/// syntax in one pass without an intermediate encoded string.
pub(crate) fn append_cell_text(output: &mut String, text: &str) {
    output.reserve(text.len());
    append_escaped(output, text, true);
}

fn append_escaped(output: &mut String, text: &str, xml: bool) {
    let bytes = text.as_bytes();
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
            match (xml, character) {
                (true, '&') => output.push_str("&amp;"),
                (true, '<') => output.push_str("&lt;"),
                (true, '>') => output.push_str("&gt;"),
                (true, '\r') => output.push_str("&#13;"),
                _ => output.push(character),
            }
        } else {
            write!(output, "_x{:04X}_", u32::from(character))
                .expect("writing to a String cannot fail");
        }
    }
}

// ST_Xstring escapes represent UTF-16 code units, including surrogate pairs.
// Decode only once so the escaped underscore in `_x005F_x0041_` stays literal.
pub(crate) fn excel_text(text: &str) -> Result<String> {
    let Some(mut position) = text.find('_') else {
        return Ok(text.to_owned());
    };
    let bytes = text.as_bytes();
    let unit_at = |position: usize| {
        if !excel_escape_prefix_at(bytes, position) || bytes.get(position + 6) != Some(&b'_') {
            return None;
        }
        // The prefix test found four ASCII hex digits, which always parse.
        u16::from_str_radix(&text[position + 2..position + 6], 16).ok()
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
#[path = "tests/text.rs"]
mod tests;
