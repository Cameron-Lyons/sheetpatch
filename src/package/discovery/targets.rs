//! OPC target URI normalization and lookup of retained ZIP member names.
use crate::archive::Archive;
use crate::{Error, Result};

#[cfg(test)]
#[path = "tests/targets.rs"]
mod tests;

pub(super) fn relationships_path(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((parent, name)) => format!("{parent}/_rels/{name}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}
pub(super) fn resolve_target(source: &str, target: &str) -> Result<String> {
    let target = escaped_target(target)?;
    let base = TargetBase::try_new(source)?;
    let (kept, appended) = base.normalize(&target)?;
    Ok(base.join(kept, &appended))
}

// Validates an internal target's syntax and normalizes its escapes.
pub(super) fn escaped_target(target: &str) -> Result<String> {
    if target.is_empty()
        || target.starts_with("//")
        || target.contains(['\\', '#', '?', '\0'])
        || target
            .split('/')
            .next()
            .is_some_and(|segment| segment.contains(':'))
    {
        return Err(Error::InvalidWorkbook(format!(
            "invalid internal relationship target {target}"
        )));
    }
    escape_path(target)
}

/// The escaped directory segments of a source part, against which its
/// relationship targets resolve.
pub(super) struct TargetBase {
    segments: Vec<String>,
    first_empty: Option<usize>,
}

impl TargetBase {
    pub(super) fn try_new(source: &str) -> Result<Self> {
        let source = escape_path(source)?;
        let segments: Vec<String> = source
            .rsplit_once('/')
            .map(|(directory, _)| directory.split('/').map(str::to_owned).collect())
            .unwrap_or_default();
        let first_empty = segments.iter().position(String::is_empty);
        Ok(Self {
            segments,
            first_empty,
        })
    }

    /// The number of leading directory segments an escaped target keeps, and
    /// the path it appends to them. This costs the target's length, not the
    /// source's.
    pub(super) fn normalize(&self, target: &str) -> Result<(usize, String)> {
        let mut kept = if target.starts_with('/') {
            0
        } else {
            self.segments.len()
        };
        let mut parts: Vec<&str> = Vec::new();
        // Empty URI segments are significant: removing them changes both direct
        // lookup and which segment a following '..' removes. Only the leading '/'
        // denotes the package root. Retain a terminal directory separator after
        // removing '.' or '..', so it cannot alias a file part.
        let mut segments = target
            .strip_prefix('/')
            .unwrap_or(target)
            .split('/')
            .peekable();
        while let Some(part) = segments.next() {
            match part {
                "." => {
                    if segments.peek().is_none() {
                        parts.push("");
                    }
                }
                ".." => {
                    // Appended segments are removed before kept ones.
                    if parts.pop().is_none() {
                        if kept == 0 {
                            return Err(Error::InvalidWorkbook(
                                "relationship target escapes package root".into(),
                            ));
                        }
                        kept -= 1;
                    }
                    if segments.peek().is_none() {
                        parts.push("");
                    }
                }
                value => parts.push(value),
            }
        }
        if kept == 0 && parts.is_empty() {
            return Err(Error::InvalidWorkbook(
                "relationship target names no part".into(),
            ));
        }
        if self.first_empty.is_some_and(|index| index < kept) || parts.iter().any(|p| p.is_empty())
        {
            return Err(Error::InvalidWorkbook(
                "relationship target contains an empty part-name segment".into(),
            ));
        }
        Ok((kept, parts.join("/")))
    }

    pub(super) fn join(&self, kept: usize, appended: &str) -> String {
        let mut path = String::new();
        for segment in &self.segments[..kept] {
            path.push_str(segment);
            path.push('/');
        }
        if appended.is_empty() {
            path.pop();
        } else {
            path.push_str(appended);
        }
        path
    }
}

// ZIP item names in OPC retain URI escapes rather than storing decoded file
// system names. Normalize escape spelling and escape raw UTF-8/space bytes.
fn escape_path(path: &str) -> Result<String> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut escaped = String::with_capacity(path.len());
    let mut input = path.as_bytes().iter().copied();
    while let Some(input_byte) = input.next() {
        let (byte, was_escaped) = if input_byte == b'%' {
            let high = input.next().and_then(|c| (c as char).to_digit(16));
            let low = input.next().and_then(|c| (c as char).to_digit(16));
            let (high, low) = high
                .zip(low)
                .ok_or_else(|| Error::InvalidWorkbook("invalid percent-encoded target".into()))?;
            let decoded_byte = (high * 16 + low) as u8;
            if matches!(decoded_byte, b'/' | b'\\' | b'.' | 0) {
                return Err(Error::InvalidWorkbook(
                    "encoded path separator, dot, or null in target".into(),
                ));
            }
            (decoded_byte, true)
        } else {
            (input_byte, false)
        };
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        let path_character = matches!(
            byte,
            b'/' | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b':'
                | b'='
                | b'@'
        );
        if unreserved || (!was_escaped && path_character) {
            escaped.push(byte as char);
        } else {
            escaped.push('%');
            escaped.push(HEX[(byte >> 4) as usize] as char);
            escaped.push(HEX[(byte & 15) as usize] as char);
        }
    }
    Ok(escaped)
}

pub(super) fn resolve_part_target(archive: &Archive, source: &str, target: &str) -> Result<String> {
    let path = resolve_target(source, target)?;
    part_path(archive, &path)
}

// Resolve a normalized part URI to its actual ZIP member name. Relationship
// parts use the same encoded-first lookup as the parts they describe.
pub(super) fn part_path(archive: &Archive, path: &str) -> Result<String> {
    if let Some(name) = archive.part_name(path)? {
        return Ok(name.to_owned());
    }
    // Earlier versions accepted ZIP names with decoded spaces and Unicode.
    // Keep those packages readable when the canonical encoded part is absent.
    // An encoded name always wins when both spellings occur in the archive.
    let mut decoded = Vec::with_capacity(path.len());
    let mut input = path.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            // Every part URI comes from `escape_path`, which writes two hex
            // digits after each '%'.
            let mut hex_digit = || {
                input
                    .next()
                    .and_then(|digit| char::from(digit).to_digit(16))
                    .expect("part URIs are escaped by escape_path")
            };
            let high = hex_digit();
            let low = hex_digit();
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    if let Ok(decoded) = str::from_utf8(&decoded)
        && let Some(name) = archive.part_name(decoded)?
    {
        return Ok(name.to_owned());
    }
    Ok(path.to_owned())
}
