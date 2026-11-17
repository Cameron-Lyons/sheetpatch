//! XML character/reference validation and Excel text escapes.

use crate::{Error, Result};
use quick_xml::events::BytesRef;
#[cfg(test)]
use std::borrow::Cow;
use std::{io, mem};

/// Copy text into a result without aborting when its allocation is refused.
pub(crate) fn copy_text(text: &str) -> Result<String> {
    let mut output = String::new();
    output
        .try_reserve_exact(text.len())
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    output.push_str(text);
    Ok(output)
}

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

// Bytes that start a character `append_escaped` may escape. `CHARACTER` marks
// `_`, which may start a token, ASCII controls XML 1.0 forbids, and 0xEF, which
// starts U+FFFE and U+FFFF: these are decided by their whole character. `XML`
// marks `&`, `<`, `>` and carriage return, each escaped as one byte in XML
// text. No UTF-8 continuation byte is among them, so every other byte is
// copied in runs.
const CHARACTER: u8 = 1;
const XML: u8 = 2;
const ESCAPE_STARTS: [u8; 256] = {
    let mut table = [0; 256];
    let mut byte = 0;
    while byte < 0x20 {
        if byte != b'\t' && byte != b'\n' && byte != b'\r' {
            table[byte as usize] = CHARACTER;
        }
        byte += 1;
    }
    table[b'_' as usize] = CHARACTER;
    table[0xef] = CHARACTER;
    table[b'&' as usize] = XML;
    table[b'<' as usize] = XML;
    table[b'>' as usize] = XML;
    table[b'\r' as usize] = XML;
    table
};

fn append_escaped(output: &mut String, text: &str, xml: bool) {
    let bytes = text.as_bytes();
    let starts = if xml { CHARACTER | XML } else { CHARACTER };
    // The start of the bytes not yet copied, and the byte being looked at,
    // which is always the first byte of a character when it can start one
    // that is escaped.
    let mut copied = 0;
    let mut position = 0;
    while position < bytes.len() {
        let byte = bytes[position];
        let class = ESCAPE_STARTS[usize::from(byte)] & starts;
        if class == 0 {
            position += 1;
            continue;
        }
        if class == XML {
            match byte {
                b'&' => push_escape(output, text, copied, "&amp;", position),
                b'<' => push_escape(output, text, copied, "&lt;", position),
                b'>' => push_escape(output, text, copied, "&gt;", position),
                _ => push_escape(output, text, copied, "&#13;", position),
            }
            position += 1;
            copied = position;
            continue;
        }
        let character = if byte.is_ascii() {
            char::from(byte)
        } else {
            text[position..]
                .chars()
                .next()
                .expect("an escapable byte starts a character")
        };
        let len = character.len_utf8();
        if character == '_' {
            if excel_escape_prefix_at(bytes, position)
                && text[position + 6..]
                    .chars()
                    .next()
                    .is_some_and(|next| next == '_' || !valid_char(next))
            {
                push_escape(output, text, copied, "_x005F_", position);
                copied = position + len;
            }
        } else if !valid_char(character) {
            let token = forbidden_escape(character);
            let token = str::from_utf8(&token).expect("an escape token is ASCII");
            push_escape(output, text, copied, token, position);
            copied = position + len;
        }
        position += len;
    }
    output.push_str(&text[copied..]);
}

// Copies the unescaped `text` from `copied` to `position`, then pushes
// `escape`. Always inlined, so that each literal escape is copied inline.
#[inline(always)]
fn push_escape(output: &mut String, text: &str, copied: usize, escape: &str, position: usize) {
    if copied < position {
        output.push_str(&text[copied..position]);
    }
    output.push_str(escape);
}

// The `_xHHHH_` escape of a character XML 1.0 forbids, all of which are in the
// BMP.
fn forbidden_escape(character: char) -> [u8; 7] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let code = u32::from(character);
    let mut token = *b"_x0000_";
    for (digit, shift) in token[2..6].iter_mut().zip([12, 8, 4, 0]) {
        *digit = HEX[((code >> shift) & 0xf) as usize];
    }
    token
}

// ST_Xstring escapes represent UTF-16 code units, including surrogate pairs.
// Decode only once so the escaped underscore in `_x005F_x0041_` stays literal.
pub(crate) fn excel_text(text: &str) -> Result<String> {
    // Only an underscore can start an escape, so most text is copied as is.
    let Some(underscore) = text.find('_') else {
        return copy_text(text);
    };
    let mut output = String::new();
    output
        .try_reserve_exact(text.len())
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    decode_excel_text(&mut output, text, underscore)?;
    // Escapes can shorten text considerably. Keep decoded text compact, but
    // do not use shrink_to_fit: its reallocation can abort on failure too.
    if output.len() < output.capacity() {
        copy_text(&output)
    } else {
        Ok(output)
    }
}

// Like `excel_text`, but appends the decoded text to `output`, which grows
// by at most `text.len()`. On an error, `output` may hold part of it.
pub(crate) fn append_excel_text(output: &mut String, text: &str) -> Result<()> {
    // Decoding never lengthens the spelling, so this one fallible reservation
    // covers every push, including ordinary spans and surrogate pairs.
    output
        .try_reserve(text.len())
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    match text.find('_') {
        Some(underscore) => {
            // Decoding into a local, rather than through the reference,
            // measured about 15 fewer instructions per underscore.
            let mut decoded = mem::take(output);
            let result = decode_excel_text(&mut decoded, text, underscore);
            *output = decoded;
            result
        }
        None => {
            output.push_str(text);
            Ok(())
        }
    }
}

// Appends `text`, whose first underscore is at `position`, to `output`,
// which has room for all of it, decoding its escapes. Inlined, as a call
// measurably slowed reads of short text with underscores.
#[inline(always)]
fn decode_excel_text(output: &mut String, text: &str, mut position: usize) -> Result<()> {
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
    Ok(())
}

#[cfg(test)]
#[path = "tests/text.rs"]
mod tests;
