//! A ZIP32 container that retains every untouched record verbatim.
//!
//! Changed members are appended to the original local-record area; only their
//! central-directory records are repointed. This deliberately avoids decoding
//! and re-encoding unrelated workbook parts.

use std::{
    collections::{BTreeMap, HashSet},
    io::Write,
    ops::Range,
};

use flate2::{Compression, Decompress, FlushDecompress, Status, write::DeflateEncoder};

use crate::{Error, Result};

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;
const DESCRIPTOR: u32 = 0x0807_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
const MAX_READ: usize = 64 * 1024 * 1024;

pub(crate) struct Entry {
    pub(crate) name: String,
    flags: u16,
    method: u16,
    crc: u32,
    compressed_size: u32,
    size: u32,
    central: Range<usize>,
    local_header: Range<usize>,
    data: Range<usize>,
}

pub(crate) struct Archive {
    original: Vec<u8>,
    entries: Vec<Entry>,
    central_start: usize,
    end: usize,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidWorkbook(message.into())
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(
            offset
                ..offset
                    .checked_add(2)
                    .ok_or_else(|| invalid("ZIP offset overflow"))?,
        )
        .ok_or_else(|| invalid("truncated ZIP record"))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(
            offset
                ..offset
                    .checked_add(4)
                    .ok_or_else(|| invalid("ZIP offset overflow"))?,
        )
        .ok_or_else(|| invalid("truncated ZIP record"))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn add(a: usize, b: usize) -> Result<usize> {
    a.checked_add(b)
        .ok_or_else(|| invalid("ZIP offset overflow"))
}

fn set_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn zip32(value: usize, description: &str) -> Result<u32> {
    let value = u32::try_from(value)
        .map_err(|_| Error::Unsupported(format!("{description} exceeds ZIP32 limits")))?;
    if value == u32::MAX {
        return Err(Error::Unsupported(format!("{description} requires ZIP64")));
    }
    Ok(value)
}

fn check_extra(mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        if bytes.len() < 4 {
            return Err(invalid("truncated ZIP extra field"));
        }
        let kind = u16_at(bytes, 0)?;
        let length = u16_at(bytes, 2)? as usize;
        if kind == 1 {
            return Err(Error::Unsupported(
                "ZIP64 workbooks are not supported".into(),
            ));
        }
        bytes = bytes
            .get(4 + length..)
            .ok_or_else(|| invalid("truncated ZIP extra field"))?;
    }
    Ok(())
}

impl Archive {
    pub(crate) fn new(original: Vec<u8>) -> Result<Self> {
        if original.len() < 22 {
            return Err(invalid("file is not a ZIP workbook"));
        }
        // A valid EOCD ends at EOF, including its comment. Multiple matching
        // EOCDs would make the member index ambiguous and are rejected.
        let search_start = original.len().saturating_sub(22 + u16::MAX as usize);
        let mut endings = Vec::new();
        for offset in (search_start..=original.len() - 22).rev() {
            if u32_at(&original, offset)? == END {
                let comment = u16_at(&original, offset + 20)? as usize;
                if add(add(offset, 22)?, comment)? == original.len() {
                    endings.push(offset);
                }
            }
        }
        let end = match endings.as_slice() {
            [end] => *end,
            [] => {
                return Err(invalid(
                    "ZIP end-of-directory record is missing or truncated",
                ));
            }
            _ => return Err(invalid("ambiguous ZIP end-of-directory records")),
        };
        if end >= 20 && u32_at(&original, end - 20)? == ZIP64_LOCATOR {
            return Err(Error::Unsupported(
                "ZIP64 workbooks are not supported".into(),
            ));
        }
        let disk = u16_at(&original, end + 4)?;
        let central_disk = u16_at(&original, end + 6)?;
        let disk_entries = u16_at(&original, end + 8)?;
        let count = u16_at(&original, end + 10)?;
        let central_size = u32_at(&original, end + 12)?;
        let central_offset = u32_at(&original, end + 16)?;
        if disk != 0 || central_disk != 0 || disk_entries != count {
            return Err(Error::Unsupported(
                "multi-disk ZIP workbooks are not supported".into(),
            ));
        }
        if count == u16::MAX || central_size == u32::MAX || central_offset == u32::MAX {
            return Err(Error::Unsupported(
                "ZIP64 workbooks are not supported".into(),
            ));
        }
        let central_start = central_offset as usize;
        if add(central_start, central_size as usize)? != end {
            return Err(invalid("ZIP central-directory bounds are inconsistent"));
        }
        if central_start > end {
            return Err(invalid("ZIP central directory is outside the file"));
        }
        let mut entries = Vec::with_capacity(count as usize);
        let mut names = HashSet::with_capacity(count as usize);
        let mut local_ranges = Vec::with_capacity(count as usize);
        let mut position = central_start;
        for _ in 0..count {
            if add(position, 46)? > end || u32_at(&original, position)? != CENTRAL {
                return Err(invalid("truncated or invalid ZIP central-directory entry"));
            }
            let flags = u16_at(&original, position + 8)?;
            if flags & (1 | 0x40 | 0x2000) != 0 {
                return Err(Error::Unsupported(
                    "encrypted ZIP workbooks are not supported".into(),
                ));
            }
            let method = u16_at(&original, position + 10)?;
            let crc = u32_at(&original, position + 16)?;
            let compressed_size = u32_at(&original, position + 20)?;
            let size = u32_at(&original, position + 24)?;
            let name_length = u16_at(&original, position + 28)? as usize;
            let extra_length = u16_at(&original, position + 30)? as usize;
            let comment_length = u16_at(&original, position + 32)? as usize;
            let entry_disk = u16_at(&original, position + 34)?;
            let local_offset = u32_at(&original, position + 42)?;
            if compressed_size == u32::MAX
                || size == u32::MAX
                || local_offset == u32::MAX
                || entry_disk == u16::MAX
            {
                return Err(Error::Unsupported(
                    "ZIP64 workbooks are not supported".into(),
                ));
            }
            if entry_disk != 0 {
                return Err(Error::Unsupported(
                    "multi-disk ZIP workbooks are not supported".into(),
                ));
            }
            let name_start = add(position, 46)?;
            let name_end = add(name_start, name_length)?;
            let extra_end = add(name_end, extra_length)?;
            let record_end = add(extra_end, comment_length)?;
            if record_end > end {
                return Err(invalid(
                    "ZIP central-directory entry exceeds directory bounds",
                ));
            }
            let raw_name = &original[name_start..name_end];
            let name = std::str::from_utf8(raw_name)
                .map_err(|_| Error::Unsupported("ZIP member names must be UTF-8".into()))?
                .to_owned();
            if name.is_empty() || name.contains('\0') || !names.insert(name.clone()) {
                return Err(invalid("empty, invalid, or duplicate ZIP member name"));
            }
            check_extra(&original[name_end..extra_end])?;

            let local_start = local_offset as usize;
            if add(local_start, 30)? > central_start || u32_at(&original, local_start)? != LOCAL {
                return Err(invalid(format!("invalid ZIP local header for {name}")));
            }
            let local_flags = u16_at(&original, local_start + 6)?;
            let local_method = u16_at(&original, local_start + 8)?;
            if local_flags != flags || local_method != method {
                return Err(invalid(format!("conflicting ZIP headers for {name}")));
            }
            let local_name_length = u16_at(&original, local_start + 26)? as usize;
            let local_extra_length = u16_at(&original, local_start + 28)? as usize;
            let local_name_start = add(local_start, 30)?;
            let local_name_end = add(local_name_start, local_name_length)?;
            let data_start = add(local_name_end, local_extra_length)?;
            if data_start > central_start
                || original.get(local_name_start..local_name_end) != Some(raw_name)
            {
                return Err(invalid(format!("conflicting ZIP member name for {name}")));
            }
            check_extra(&original[local_name_end..data_start])?;
            for (local_field, central_value) in [(14, crc), (18, compressed_size), (22, size)] {
                let local_value = u32_at(&original, local_start + local_field)?;
                if local_value != central_value && !(flags & 8 != 0 && local_value == 0) {
                    return Err(invalid(format!(
                        "conflicting ZIP sizes or checksum for {name}"
                    )));
                }
            }
            if method == 0 && compressed_size != size {
                return Err(invalid(format!(
                    "invalid stored ZIP member size for {name}"
                )));
            }
            let data_end = add(data_start, compressed_size as usize)?;
            if data_end > central_start {
                return Err(invalid(format!(
                    "ZIP member {name} exceeds local-record bounds"
                )));
            }
            let local_end = if flags & 8 != 0 {
                let descriptor_matches = |offset: usize| -> bool {
                    add(offset, 12).is_ok_and(|end| end <= central_start)
                        && u32_at(&original, offset).ok() == Some(crc)
                        && u32_at(&original, offset + 4).ok() == Some(compressed_size)
                        && u32_at(&original, offset + 8).ok() == Some(size)
                };
                let unsigned = descriptor_matches(data_end);
                let signed = u32_at(&original, data_end).ok() == Some(DESCRIPTOR)
                    && descriptor_matches(add(data_end, 4)?);
                match (unsigned, signed) {
                    (true, false) => add(data_end, 12)?,
                    (false, true) => add(data_end, 16)?,
                    _ => {
                        return Err(invalid(format!(
                            "missing or ambiguous ZIP data descriptor for {name}"
                        )));
                    }
                }
            } else {
                data_end
            };
            local_ranges.push(local_start..local_end);
            entries.push(Entry {
                name,
                flags,
                method,
                crc,
                compressed_size,
                size,
                central: position..record_end,
                local_header: local_start..data_start,
                data: data_start..data_end,
            });
            position = record_end;
        }
        if position != end {
            return Err(invalid("ZIP central-directory entry count is inconsistent"));
        }
        local_ranges.sort_unstable_by_key(|range| range.start);
        if local_ranges
            .windows(2)
            .any(|pair| pair[0].end > pair[1].start)
        {
            return Err(invalid("overlapping ZIP local records"));
        }
        Ok(Self {
            original,
            entries,
            central_start,
            end,
        })
    }

    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub(crate) fn read(&self, name: &str) -> Result<Vec<u8>> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| invalid(format!("ZIP member is missing: {name}")))?;
        if entry.size as usize > MAX_READ {
            return Err(Error::Unsupported(format!(
                "ZIP member {name} exceeds the 64 MiB read limit"
            )));
        }
        let compressed = &self.original[entry.data.clone()];
        let output = match entry.method {
            0 => compressed.to_vec(),
            8 => {
                let mut decoder = Decompress::new(false);
                // The declared length also bounds malformed streams that expand
                // beyond their header, rather than trusting that header.
                let mut output = Vec::with_capacity(entry.size as usize + 1);
                let status = decoder
                    .decompress_vec(compressed, &mut output, FlushDecompress::Finish)
                    .map_err(|error| {
                        invalid(format!("invalid deflate stream in {name}: {error}"))
                    })?;
                if status != Status::StreamEnd || decoder.total_in() != entry.compressed_size as u64
                {
                    return Err(invalid(format!(
                        "incomplete deflate stream or length mismatch in {name}"
                    )));
                }
                output
            }
            method => {
                return Err(Error::Unsupported(format!(
                    "ZIP compression method {method} in {name}"
                )));
            }
        };
        if output.len() != entry.size as usize || crc32(&output) != entry.crc {
            return Err(invalid(format!("ZIP size or checksum mismatch in {name}")));
        }
        Ok(output)
    }

    pub(crate) fn write(&self, changes: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>> {
        if changes.is_empty() {
            return Ok(self.original.clone());
        }
        for name in changes.keys() {
            if !self.entries.iter().any(|entry| &entry.name == name) {
                return Err(invalid(format!(
                    "cannot replace missing ZIP member: {name}"
                )));
            }
        }
        let mut output = self.original[..self.central_start].to_vec();
        let mut replacements = BTreeMap::new();
        for entry in &self.entries {
            let Some(data) = changes.get(&entry.name) else {
                continue;
            };
            if data.len() > MAX_READ {
                return Err(Error::Unsupported(format!(
                    "ZIP member {} exceeds the 64 MiB read limit",
                    entry.name
                )));
            }
            let size = zip32(data.len(), "ZIP member size")?;
            let compressed = match entry.method {
                0 => data.clone(),
                8 => {
                    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
                    encoder.write_all(data)?;
                    encoder.finish()?
                }
                method => {
                    return Err(Error::Unsupported(format!(
                        "ZIP compression method {method} in {}",
                        entry.name
                    )));
                }
            };
            let compressed_size = zip32(compressed.len(), "compressed ZIP member size")?;
            let local_offset = zip32(output.len(), "ZIP local-record offset")?;
            let crc = crc32(data);
            let flags = entry.flags & !8;
            let mut header = self.original[entry.local_header.clone()].to_vec();
            set_u16(&mut header, 6, flags);
            set_u32(&mut header, 14, crc);
            set_u32(&mut header, 18, compressed_size);
            set_u32(&mut header, 22, size);
            zip32(
                add(add(output.len(), header.len())?, compressed.len())?,
                "ZIP local-record area",
            )?;
            output.extend_from_slice(&header);
            output.extend_from_slice(&compressed);
            let mut central = self.original[entry.central.clone()].to_vec();
            set_u16(&mut central, 8, flags);
            set_u32(&mut central, 16, crc);
            set_u32(&mut central, 20, compressed_size);
            set_u32(&mut central, 24, size);
            set_u32(&mut central, 42, local_offset);
            replacements.insert(entry.name.as_str(), central);
        }
        let central_start = zip32(output.len(), "ZIP central-directory offset")?;
        for entry in &self.entries {
            match replacements.get(entry.name.as_str()) {
                Some(record) => output.extend_from_slice(record),
                None => output.extend_from_slice(&self.original[entry.central.clone()]),
            }
        }
        let central_size = zip32(
            output.len() - central_start as usize,
            "ZIP central-directory size",
        )?;
        let mut ending = self.original[self.end..].to_vec();
        set_u32(&mut ending, 12, central_size);
        set_u32(&mut ending, 16, central_start);
        output.extend_from_slice(&ending);
        Ok(output)
    }
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < table.len() {
        let mut value = i as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[i] = value;
        i += 1;
    }
    table
}

fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = crc_table();
    let mut value = u32::MAX;
    for &byte in bytes {
        value = TABLE[((value as u8) ^ byte) as usize] ^ (value >> 8);
    }
    !value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(descriptor: bool) -> Vec<u8> {
        let name = b"part.xml";
        let data = b"<part/>";
        let crc = crc32(data);
        let flags = if descriptor { 8 } else { 0 };
        let mut bytes = vec![0; 30];
        set_u32(&mut bytes, 0, LOCAL);
        set_u16(&mut bytes, 4, 20);
        set_u16(&mut bytes, 6, flags);
        if !descriptor {
            set_u32(&mut bytes, 14, crc);
            set_u32(&mut bytes, 18, data.len() as u32);
            set_u32(&mut bytes, 22, data.len() as u32);
        }
        set_u16(&mut bytes, 26, name.len() as u16);
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(data);
        if descriptor {
            bytes.extend_from_slice(&DESCRIPTOR.to_le_bytes());
            bytes.extend_from_slice(&crc.to_le_bytes());
            bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        }
        let central_start = bytes.len();
        let mut central = vec![0; 46];
        set_u32(&mut central, 0, CENTRAL);
        set_u16(&mut central, 4, 20);
        set_u16(&mut central, 6, 20);
        set_u16(&mut central, 8, flags);
        set_u32(&mut central, 16, crc);
        set_u32(&mut central, 20, data.len() as u32);
        set_u32(&mut central, 24, data.len() as u32);
        set_u16(&mut central, 28, name.len() as u16);
        central.extend_from_slice(name);
        bytes.extend_from_slice(&central);
        let mut end = vec![0; 22];
        set_u32(&mut end, 0, END);
        set_u16(&mut end, 8, 1);
        set_u16(&mut end, 10, 1);
        set_u32(&mut end, 12, central.len() as u32);
        set_u32(&mut end, 16, central_start as u32);
        set_u16(&mut end, 20, 7);
        end.extend_from_slice(b"comment");
        bytes.extend_from_slice(&end);
        bytes
    }

    #[test]
    fn no_change_is_byte_identical() {
        let original = fixture(false);
        let archive = Archive::new(original.clone()).unwrap();
        assert_eq!(archive.read("part.xml").unwrap(), b"<part/>");
        assert_eq!(archive.write(&BTreeMap::new()).unwrap(), original);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn appends_replacement_and_retains_original_records_and_comment() {
        for descriptor in [false, true] {
            let original = fixture(descriptor);
            let archive = Archive::new(original.clone()).unwrap();
            let changes =
                BTreeMap::from([("part.xml".into(), b"<part changed=\"yes\"/>".to_vec())]);
            let edited = archive.write(&changes).unwrap();
            assert_eq!(
                &edited[..archive.central_start],
                &original[..archive.central_start]
            );
            assert!(edited.ends_with(b"comment"));
            let reopened = Archive::new(edited).unwrap();
            assert_eq!(reopened.read("part.xml").unwrap(), changes["part.xml"]);
            assert_eq!(reopened.entries[0].flags & 8, 0);
        }
    }

    #[test]
    fn rejects_corrupt_member_and_truncated_archives() {
        let original = fixture(false);
        for end in 0..original.len() {
            assert!(Archive::new(original[..end].to_vec()).is_err());
        }
        let mut corrupted = original;
        corrupted[30 + b"part.xml".len()] ^= 1;
        let archive = Archive::new(corrupted).unwrap();
        assert!(archive.read("part.xml").is_err());
    }

    #[test]
    fn rejects_zip64_and_inconsistent_offsets() {
        let mut bytes = fixture(false);
        let end = bytes.len() - 29;
        set_u32(&mut bytes, end + 16, u32::MAX);
        assert!(matches!(Archive::new(bytes), Err(Error::Unsupported(_))));
        let mut bytes = fixture(false);
        let end = bytes.len() - 29;
        set_u32(&mut bytes, end + 16, 1);
        assert!(Archive::new(bytes).is_err());
    }

    fn deflate_fixture(compressed: &[u8]) -> Vec<u8> {
        let archive = Archive::new(fixture(false)).unwrap();
        let entry = &archive.entries[0];
        let mut bytes = archive.original[entry.local_header.clone()].to_vec();
        set_u16(&mut bytes, 8, 8);
        set_u32(&mut bytes, 18, compressed.len() as u32);
        bytes.extend_from_slice(compressed);
        let central_start = bytes.len();
        let mut central = archive.original[entry.central.clone()].to_vec();
        set_u16(&mut central, 10, 8);
        set_u32(&mut central, 20, compressed.len() as u32);
        bytes.extend_from_slice(&central);
        let mut ending = archive.original[archive.end..].to_vec();
        set_u32(&mut ending, 16, central_start as u32);
        bytes.extend_from_slice(&ending);
        bytes
    }

    #[test]
    fn requires_complete_deflate_stream_even_when_all_payload_bytes_exist() {
        // An uncompressed DEFLATE block with BFINAL=0 produces the complete
        // expected payload; the following empty BFINAL=1 block ends the stream.
        let mut compressed = vec![0, 7, 0, 0xf8, 0xff];
        compressed.extend_from_slice(b"<part/>");
        let truncated = deflate_fixture(&compressed);
        compressed.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
        let valid = Archive::new(deflate_fixture(&compressed)).unwrap();
        assert_eq!(valid.read("part.xml").unwrap(), b"<part/>");
        assert!(Archive::new(truncated).unwrap().read("part.xml").is_err());
        let changed = valid
            .write(&BTreeMap::from([("part.xml".into(), b"changed".to_vec())]))
            .unwrap();
        assert_eq!(
            Archive::new(changed).unwrap().read("part.xml").unwrap(),
            b"changed"
        );
    }
}
