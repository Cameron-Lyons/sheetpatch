//! A ZIP32 container that retains every untouched record verbatim.
//!
//! Changed members replace their original local records; untouched records and
//! opaque gaps are copied verbatim. This avoids decoding unrelated parts and
//! introducing unreferenced local records that some spreadsheet readers reject.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
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
    local_record: Range<usize>,
}

pub(crate) struct Archive {
    original: Vec<u8>,
    entries: Vec<Entry>,
    local_order: Vec<usize>,
    index: HashMap<String, usize>,
    part_index: HashMap<String, Option<usize>>,
    central_start: usize,
    end: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct StoredDescriptor {
    data_end: usize,
    end: usize,
    crc: u32,
    size: u32,
}

#[derive(Clone, Copy)]
struct Replacement {
    crc: u32,
    compressed_size: u32,
    size: u32,
}

// `None` means more than one structurally plausible descriptor points to that
// payload start. Compaction refuses this ambiguity rather than choosing one.
type StoredDescriptors = HashMap<usize, Option<StoredDescriptor>>;

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

fn descriptor_end(
    bytes: &[u8],
    data_end: usize,
    limit: usize,
    crc: u32,
    compressed_size: u32,
    size: u32,
    name: &str,
) -> Result<usize> {
    let matches = |offset: usize| -> bool {
        add(offset, 12).is_ok_and(|end| end <= limit)
            && u32_at(bytes, offset).ok() == Some(crc)
            && u32_at(bytes, offset + 4).ok() == Some(compressed_size)
            && u32_at(bytes, offset + 8).ok() == Some(size)
    };
    let unsigned = matches(data_end);
    let signed = u32_at(bytes, data_end).ok() == Some(DESCRIPTOR) && matches(add(data_end, 4)?);
    match (unsigned, signed) {
        (true, false) => add(data_end, 12),
        (false, true) => add(data_end, 16),
        _ => Err(invalid(format!(
            "missing or ambiguous ZIP data descriptor for {name}"
        ))),
    }
}

fn emit<W: Write>(writer: &mut W, position: &mut usize, bytes: &[u8]) -> Result<()> {
    let end = add(*position, bytes.len())?;
    writer.write_all(bytes)?;
    *position = end;
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
        let mut index = HashMap::with_capacity(count as usize);
        let mut part_index = HashMap::with_capacity(count as usize);
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
            if name.is_empty()
                || name.contains('\0')
                || index.insert(name.clone(), entries.len()).is_some()
            {
                return Err(invalid("empty, invalid, or duplicate ZIP member name"));
            }
            check_extra(&original[name_end..extra_end])?;
            // OPC part names compare without ASCII case, including the hex
            // digits in URI escapes. Retain exact ZIP names for preservation;
            // reject an ambiguous equivalent name only if that part is used.
            part_index
                .entry(name.to_ascii_lowercase())
                .and_modify(|entry| *entry = None)
                .or_insert(Some(entries.len()));

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
                descriptor_end(
                    &original,
                    data_end,
                    central_start,
                    crc,
                    compressed_size,
                    size,
                    &name,
                )?
            } else {
                data_end
            };
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
                local_record: local_start..local_end,
            });
            position = record_end;
        }
        if position != end {
            return Err(invalid("ZIP central-directory entry count is inconsistent"));
        }
        let mut local_order: Vec<_> = (0..entries.len()).collect();
        local_order.sort_unstable_by_key(|&index| entries[index].local_record.start);
        if local_order
            .windows(2)
            .any(|pair| entries[pair[0]].local_record.end > entries[pair[1]].local_record.start)
        {
            return Err(invalid("overlapping ZIP local records"));
        }
        Ok(Self {
            original,
            entries,
            local_order,
            index,
            part_index,
            central_start,
            end,
        })
    }

    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    pub(crate) fn part_name(&self, name: &str) -> Result<Option<&str>> {
        let key = if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
            Cow::Owned(name.to_ascii_lowercase())
        } else {
            Cow::Borrowed(name)
        };
        match self.part_index.get(key.as_ref()) {
            Some(Some(index)) => Ok(Some(&self.entries[*index].name)),
            Some(None) => Err(invalid(format!(
                "ambiguous equivalent OPC part names: {name}"
            ))),
            None => Ok(None),
        }
    }

    pub(crate) fn read(&self, name: &str) -> Result<Vec<u8>> {
        let entry = self
            .index
            .get(name)
            .map(|&index| &self.entries[index])
            .ok_or_else(|| invalid(format!("ZIP member is missing: {name}")))?;
        if entry.flags & 0x20 != 0 {
            return Err(Error::Unsupported(format!(
                "patched ZIP member data in {name}"
            )));
        }
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
        let mut output = Vec::with_capacity(self.original.len());
        self.write_to(changes, &mut output)?;
        Ok(output)
    }

    /// Stream local records in their original order, replacing changed members.
    ///
    /// Only one changed member's compressed payload is held in memory at a
    /// time. As with `Write::write_all`, a writer error may leave partial output.
    pub(crate) fn write_to<W: Write>(
        &self,
        changes: &BTreeMap<String, Vec<u8>>,
        writer: &mut W,
    ) -> Result<()> {
        if changes.is_empty() {
            writer.write_all(&self.original)?;
            return Ok(());
        }
        self.validate_changes(changes)?;
        let mut position = 0;
        let mut original_position = 0;
        let mut replacements = vec![None; self.entries.len()];
        let mut offsets = vec![0; self.entries.len()];
        for &index in &self.local_order {
            let entry = &self.entries[index];
            // Prefixes, gaps and any obsolete records from older versions are
            // opaque here. Preserve them; only explicit compaction removes
            // obsolete records after proving their boundaries and checksums.
            emit(
                writer,
                &mut position,
                &self.original[original_position..entry.local_record.start],
            )?;
            offsets[index] = zip32(position, "ZIP local-record offset")?;
            if let Some(data) = changes.get(&entry.name) {
                replacements[index] =
                    Some(self.emit_replacement(entry, data, writer, &mut position)?);
            } else {
                emit(
                    writer,
                    &mut position,
                    &self.original[entry.local_record.clone()],
                )?;
            }
            original_position = entry.local_record.end;
        }
        emit(
            writer,
            &mut position,
            &self.original[original_position..self.central_start],
        )?;
        self.emit_directory(writer, &mut position, &offsets, &replacements)
    }

    /// Reproduce pre-1.0 append saves to exercise migration and compaction.
    #[cfg(test)]
    fn append_to<W: Write>(
        &self,
        changes: &BTreeMap<String, Vec<u8>>,
        writer: &mut W,
    ) -> Result<()> {
        if changes.is_empty() {
            writer.write_all(&self.original)?;
            return Ok(());
        }
        self.validate_changes(changes)?;
        let mut position = 0;
        emit(writer, &mut position, &self.original[..self.central_start])?;
        let mut replacements = vec![None; self.entries.len()];
        let mut offsets = Vec::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            if let Some(data) = changes.get(&entry.name) {
                offsets.push(zip32(position, "ZIP local-record offset")?);
                replacements[index] =
                    Some(self.emit_replacement(entry, data, writer, &mut position)?);
            } else {
                offsets.push(entry.local_record.start as u32);
            }
        }
        self.emit_directory(writer, &mut position, &offsets, &replacements)
    }

    /// Remove obsolete local records from earlier append saves. Unknown gaps
    /// are rejected before anything is written, so opaque content is never
    /// silently discarded by compaction.
    pub(crate) fn compact_to<W: Write>(
        &self,
        changes: &BTreeMap<String, Vec<u8>>,
        writer: &mut W,
    ) -> Result<()> {
        self.validate_changes(changes)?;
        let order = self.compaction_order()?;
        let mut position = 0;
        let mut replacements = vec![None; self.entries.len()];
        let mut offsets = vec![0; self.entries.len()];
        for &index in order {
            let entry = &self.entries[index];
            offsets[index] = zip32(position, "ZIP local-record offset")?;
            if let Some(data) = changes.get(&entry.name) {
                replacements[index] =
                    Some(self.emit_replacement(entry, data, writer, &mut position)?);
            } else {
                emit(
                    writer,
                    &mut position,
                    &self.original[entry.local_record.clone()],
                )?;
            }
        }
        self.emit_directory(writer, &mut position, &offsets, &replacements)
    }

    fn validate_changes(&self, changes: &BTreeMap<String, Vec<u8>>) -> Result<()> {
        for (name, data) in changes {
            let Some(&index) = self.index.get(name) else {
                return Err(invalid(format!(
                    "cannot replace missing ZIP member: {name}"
                )));
            };
            if data.len() > MAX_READ {
                return Err(Error::Unsupported(format!(
                    "ZIP member {name} exceeds the 64 MiB read limit"
                )));
            }
            let entry = &self.entries[index];
            if entry.flags & 0x20 != 0 {
                return Err(Error::Unsupported(format!(
                    "patched ZIP member data in {name}"
                )));
            }
            let method = entry.method;
            if !matches!(method, 0 | 8) {
                return Err(Error::Unsupported(format!(
                    "ZIP compression method {method} in {name}"
                )));
            }
        }
        Ok(())
    }

    fn emit_replacement<W: Write>(
        &self,
        entry: &Entry,
        data: &[u8],
        writer: &mut W,
        position: &mut usize,
    ) -> Result<Replacement> {
        let size = zip32(data.len(), "ZIP member size")?;
        let compressed = if entry.method == 0 {
            Cow::Borrowed(data)
        } else {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(data)?;
            Cow::Owned(encoder.finish()?)
        };
        let compressed_size = zip32(compressed.len(), "compressed ZIP member size")?;
        let crc = crc32(data);
        let flags = entry.flags & !8;
        let original_header = &self.original[entry.local_header.clone()];
        let mut header = [0; 30];
        header.copy_from_slice(&original_header[..30]);
        set_u16(&mut header, 6, flags);
        set_u32(&mut header, 14, crc);
        set_u32(&mut header, 18, compressed_size);
        set_u32(&mut header, 22, size);
        zip32(
            add(add(*position, original_header.len())?, compressed.len())?,
            "ZIP local-record area",
        )?;
        emit(writer, position, &header)?;
        emit(writer, position, &original_header[30..])?;
        emit(writer, position, &compressed)?;
        Ok(Replacement {
            crc,
            compressed_size,
            size,
        })
    }

    fn emit_directory<W: Write>(
        &self,
        writer: &mut W,
        position: &mut usize,
        offsets: &[u32],
        replacements: &[Option<Replacement>],
    ) -> Result<()> {
        let central_start = zip32(*position, "ZIP central-directory offset")?;
        for (index, entry) in self.entries.iter().enumerate() {
            let record = &self.original[entry.central.clone()];
            let mut header = [0; 46];
            header.copy_from_slice(&record[..46]);
            if let Some(replacement) = replacements[index] {
                set_u16(&mut header, 8, entry.flags & !8);
                set_u32(&mut header, 16, replacement.crc);
                set_u32(&mut header, 20, replacement.compressed_size);
                set_u32(&mut header, 24, replacement.size);
            }
            // The local offset is the only field compaction changes for an
            // untouched member; comments, extras and all other metadata survive.
            set_u32(&mut header, 42, offsets[index]);
            emit(writer, position, &header)?;
            emit(writer, position, &record[46..])?;
        }
        let central_size = zip32(
            *position - central_start as usize,
            "ZIP central-directory size",
        )?;
        let mut ending = [0; 22];
        ending.copy_from_slice(&self.original[self.end..self.end + 22]);
        set_u32(&mut ending, 12, central_size);
        set_u32(&mut ending, 16, central_start);
        emit(writer, position, &ending)?;
        emit(writer, position, &self.original[self.end + 22..])
    }

    fn compaction_order(&self) -> Result<&[usize]> {
        let mut position = 0;
        let mut stored_descriptors = None;
        for &index in &self.local_order {
            let entry = &self.entries[index];
            while position < entry.local_record.start {
                position = self.orphan_end(position, &mut stored_descriptors)?;
            }
            if position != entry.local_record.start {
                return Err(Error::Unsupported(
                    "cannot compact overlapping or unrecognized ZIP local records".into(),
                ));
            }
            position = entry.local_record.end;
        }
        while position < self.central_start {
            position = self.orphan_end(position, &mut stored_descriptors)?;
        }
        if position != self.central_start {
            return Err(Error::Unsupported(
                "cannot compact unrecognized data before the ZIP directory".into(),
            ));
        }
        Ok(&self.local_order)
    }

    fn orphan_end(
        &self,
        start: usize,
        stored_descriptors: &mut Option<StoredDescriptors>,
    ) -> Result<usize> {
        let unsupported = || {
            Error::Unsupported(
                "cannot compact opaque ZIP prefix, gaps, or unrecognized obsolete records".into(),
            )
        };
        let bytes = &self.original;
        if add(start, 30)? > self.central_start || u32_at(bytes, start)? != LOCAL {
            return Err(unsupported());
        }
        let flags = u16_at(bytes, start + 6)?;
        let method = u16_at(bytes, start + 8)?;
        if flags & (1 | 0x20 | 0x40 | 0x2000) != 0 || !matches!(method, 0 | 8) {
            return Err(unsupported());
        }
        let local_crc = u32_at(bytes, start + 14)?;
        let local_compressed_size = u32_at(bytes, start + 18)?;
        let local_size = u32_at(bytes, start + 22)?;
        if local_compressed_size == u32::MAX || local_size == u32::MAX {
            return Err(unsupported());
        }
        let name_length = u16_at(bytes, start + 26)? as usize;
        let extra_length = u16_at(bytes, start + 28)? as usize;
        let name_start = add(start, 30)?;
        let name_end = add(name_start, name_length)?;
        let data_start = add(name_end, extra_length)?;
        if data_start > self.central_start {
            return Err(unsupported());
        }
        let name = std::str::from_utf8(&bytes[name_start..name_end]).map_err(|_| unsupported())?;
        let Some(&index) = self.index.get(name) else {
            return Err(unsupported());
        };
        if self.entries[index].method != method {
            return Err(unsupported());
        }
        check_extra(&bytes[name_end..data_start])?;
        let has_descriptor = flags & 8 != 0;
        let (compressed_size, size, crc) = if method == 8 {
            let end = if has_descriptor && local_compressed_size == 0 {
                self.central_start
            } else {
                add(data_start, local_compressed_size as usize)?
            };
            if end > self.central_start {
                return Err(unsupported());
            }
            inspect_deflate(&bytes[data_start..end], name)?
        } else if has_descriptor && local_compressed_size == 0 {
            let descriptors = stored_descriptors.get_or_insert_with(|| self.stored_descriptors());
            return self.stored_orphan_end(data_start, local_crc, local_size, name, descriptors);
        } else {
            let end = add(data_start, local_compressed_size as usize)?;
            if end > self.central_start || local_compressed_size as usize > MAX_READ {
                return Err(unsupported());
            }
            (
                local_compressed_size,
                local_compressed_size,
                crc32(&bytes[data_start..end]),
            )
        };
        for (header_value, value) in [
            (local_crc, crc),
            (local_compressed_size, compressed_size),
            (local_size, size),
        ] {
            if header_value != value && !(has_descriptor && header_value == 0) {
                return Err(unsupported());
            }
        }
        let data_end = add(data_start, compressed_size as usize)?;
        if has_descriptor {
            descriptor_end(
                bytes,
                data_end,
                self.central_start,
                crc,
                compressed_size,
                size,
                name,
            )
        } else {
            Ok(data_end)
        }
    }

    // Stored streams carry no end marker. Index descriptors immediately before
    // local signatures or the central directory in a single pass; their size
    // fields identify the possible payload start. The index avoids scanning
    // the rest of the archive separately for every obsolete streaming member.
    fn stored_descriptors(&self) -> StoredDescriptors {
        let mut descriptors = HashMap::new();
        let local_signature = LOCAL.to_le_bytes();
        let boundaries = self.original[..self.central_start]
            .windows(4)
            .enumerate()
            .filter(|(_, bytes)| *bytes == local_signature)
            .map(|(offset, _)| offset)
            .chain(std::iter::once(self.central_start));
        for boundary in boundaries {
            for signed in [false, true] {
                let Some(data_end) = boundary.checked_sub(if signed { 16 } else { 12 }) else {
                    continue;
                };
                if signed && u32_at(&self.original, data_end).ok() != Some(DESCRIPTOR) {
                    continue;
                }
                let fields = data_end + if signed { 4 } else { 0 };
                // The descriptor lies entirely before this validated boundary.
                let crc = u32_at(&self.original, fields).expect("descriptor CRC in bounds");
                let compressed_size = u32_at(&self.original, fields + 4)
                    .expect("descriptor compressed size in bounds");
                let size = u32_at(&self.original, fields + 8).expect("descriptor size in bounds");
                if size != compressed_size || size as usize > MAX_READ {
                    continue;
                }
                let Some(data_start) = data_end.checked_sub(size as usize) else {
                    continue;
                };
                let descriptor = StoredDescriptor {
                    data_end,
                    end: boundary,
                    crc,
                    size,
                };
                descriptors
                    .entry(data_start)
                    .and_modify(|candidate| {
                        if *candidate != Some(descriptor) {
                            *candidate = None;
                        }
                    })
                    .or_insert(Some(descriptor));
            }
        }
        descriptors
    }

    fn stored_orphan_end(
        &self,
        data_start: usize,
        local_crc: u32,
        local_size: u32,
        name: &str,
        descriptors: &StoredDescriptors,
    ) -> Result<usize> {
        let Some(candidate) = descriptors.get(&data_start) else {
            return Err(Error::Unsupported(
                "cannot compact an unrecognized stored ZIP data descriptor".into(),
            ));
        };
        let Some(descriptor) = candidate else {
            return Err(Error::Unsupported(
                "cannot compact an ambiguous stored ZIP data descriptor".into(),
            ));
        };
        if (local_crc != 0 && local_crc != descriptor.crc)
            || (local_size != 0 && local_size != descriptor.size)
            || crc32(&self.original[data_start..descriptor.data_end]) != descriptor.crc
        {
            return Err(invalid(format!(
                "obsolete ZIP size or checksum mismatch in {name}"
            )));
        }
        let end = descriptor_end(
            &self.original,
            descriptor.data_end,
            self.central_start,
            descriptor.crc,
            descriptor.size,
            descriptor.size,
            name,
        )?;
        if end != descriptor.end {
            return Err(invalid(format!(
                "conflicting obsolete ZIP descriptor in {name}"
            )));
        }
        Ok(end)
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

fn inspect_deflate(compressed: &[u8], name: &str) -> Result<(u32, u32, u32)> {
    let mut decoder = Decompress::new(false);
    let mut buffer = [0; 8192];
    let mut crc = crc32fast::Hasher::new();
    loop {
        let previous_in = decoder.total_in();
        let previous_out = decoder.total_out();
        let status = decoder
            .decompress(
                &compressed[previous_in as usize..],
                &mut buffer,
                FlushDecompress::None,
            )
            .map_err(|error| {
                invalid(format!(
                    "invalid obsolete deflate stream in {name}: {error}"
                ))
            })?;
        if decoder.total_out() > MAX_READ as u64 {
            return Err(Error::Unsupported(format!(
                "obsolete ZIP member {name} exceeds the 64 MiB read limit"
            )));
        }
        let produced = (decoder.total_out() - previous_out) as usize;
        crc.update(&buffer[..produced]);
        if status == Status::StreamEnd {
            return Ok((
                zip32(decoder.total_in() as usize, "compressed ZIP member size")?,
                decoder.total_out() as u32,
                crc.finalize(),
            ));
        }
        if decoder.total_in() == previous_in && decoder.total_out() == previous_out {
            return Err(invalid(format!(
                "incomplete obsolete deflate stream in {name}"
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append(archive: &Archive, changes: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
        let mut bytes = Vec::new();
        archive.append_to(changes, &mut bytes).unwrap();
        bytes
    }

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
        assert!(archive.contains("part.xml"));
        assert!(!archive.contains("missing.xml"));
    }

    #[test]
    fn opc_part_lookup_preserves_spelling_and_refuses_equivalent_duplicates() {
        let archive = Archive::new(fixture(false)).unwrap();
        assert_eq!(archive.part_name("PART.XML").unwrap(), Some("part.xml"));
        assert_eq!(archive.part_name("missing.xml").unwrap(), None);
        let original = combine(fixture(false), decorate_fixture(b"PART.XML", false));
        let archive = Archive::new(original.clone()).unwrap();
        assert!(archive.part_name("part.xml").is_err());
        // ZIP indexing itself remains exact, so opaque duplicate OPC names do
        // not prevent preserving an otherwise unaccessed archive member.
        assert_eq!(archive.read("part.xml").unwrap(), b"<part/>");
        assert!(archive.contains("PART.XML"));
        assert_eq!(archive.write(&BTreeMap::new()).unwrap(), original);
    }

    #[test]
    fn appends_replacement_and_retains_original_records_and_comment() {
        for descriptor in [false, true] {
            let original = fixture(descriptor);
            let archive = Archive::new(original.clone()).unwrap();
            let changes =
                BTreeMap::from([("part.xml".into(), b"<part changed=\"yes\"/>".to_vec())]);
            let edited = append(&archive, &changes);
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
    fn replacements_leave_no_unreferenced_records_and_preserve_untouched_metadata() {
        let original = Archive::new(combine(
            decorate_fixture(b"part.xml", false),
            decorate_fixture(b"keep.xml", true),
        ))
        .unwrap();
        // Central-directory order need not match physical local-record order.
        let mut bytes = original.original[..original.central_start].to_vec();
        bytes.extend_from_slice(&original.original[original.entries[1].central.clone()]);
        bytes.extend_from_slice(&original.original[original.entries[0].central.clone()]);
        bytes.extend_from_slice(&original.original[original.end..]);
        let mut archive = Archive::new(bytes).unwrap();
        let untouched = &archive.entries[archive.index["keep.xml"]];
        let local = archive.original[untouched.local_record.clone()].to_vec();
        let central = archive.original[untouched.central.clone()].to_vec();
        for data in [b"longer replacement".as_slice(), b"short".as_slice()] {
            let changes = BTreeMap::from([("part.xml".into(), data.to_vec())]);
            let edited = archive.write(&changes).unwrap();
            let mut compacted = Vec::new();
            archive.compact_to(&changes, &mut compacted).unwrap();
            assert_eq!(edited, compacted);
            archive = Archive::new(edited).unwrap();
            let changed = &archive.entries[archive.index["part.xml"]];
            let untouched = &archive.entries[archive.index["keep.xml"]];
            // All bytes before the directory belong to exactly two records.
            assert_eq!(changed.local_record.start, 0);
            assert_eq!(changed.local_record.end, untouched.local_record.start);
            assert_eq!(untouched.local_record.end, archive.central_start);
            assert_eq!(archive.read("part.xml").unwrap(), data);
            assert_eq!(archive.original[untouched.local_record.clone()], local);
            let updated_central = &archive.original[untouched.central.clone()];
            assert_eq!(updated_central[..42], central[..42]);
            assert_eq!(updated_central[46..], central[46..]);
            assert_eq!(archive.entries[0].name, "keep.xml");
            assert!(archive.original.ends_with(b"comment"));
        }
    }

    #[test]
    fn replacements_preserve_opaque_prefixes_gaps_and_tails() {
        let original =
            Archive::new(combine(fixture(false), decorate_fixture(b"keep.xml", true))).unwrap();
        let prefix = b"opaque prefix";
        let gap = b"opaque gap";
        let tail = b"opaque tail";
        let first = &original.entries[0];
        let second = &original.entries[1];
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(&original.original[first.local_record.clone()]);
        bytes.extend_from_slice(gap);
        let second_start = bytes.len();
        bytes.extend_from_slice(&original.original[second.local_record.clone()]);
        bytes.extend_from_slice(tail);
        let central_start = bytes.len();
        for (entry, offset) in [(first, prefix.len()), (second, second_start)] {
            let mut record = original.original[entry.central.clone()].to_vec();
            set_u32(&mut record, 42, offset as u32);
            bytes.extend_from_slice(&record);
        }
        let mut ending = original.original[original.end..].to_vec();
        set_u32(&mut ending, 16, central_start as u32);
        bytes.extend_from_slice(&ending);
        let archive = Archive::new(bytes).unwrap();
        let edited = archive
            .write(&BTreeMap::from([("part.xml".into(), b"updated".to_vec())]))
            .unwrap();
        let updated = Archive::new(edited).unwrap();
        let first = &updated.entries[0];
        let second = &updated.entries[1];
        assert_eq!(&updated.original[..first.local_record.start], prefix);
        assert_eq!(
            &updated.original[first.local_record.end..second.local_record.start],
            gap
        );
        assert_eq!(
            &updated.original[second.local_record.end..updated.central_start],
            tail
        );
        assert_eq!(updated.read("part.xml").unwrap(), b"updated");
        assert_eq!(
            updated.original[second.local_record.clone()],
            original.original[original.entries[1].local_record.clone()]
        );
    }

    #[test]
    fn replacements_preserve_changed_member_metadata_and_large_archive_comments() {
        let mut original = decorate_fixture(b"part.xml", false);
        let end = original.len() - 29;
        original.truncate(end + 22);
        set_u16(&mut original, end + 20, u16::MAX);
        original.extend(std::iter::repeat_n(b'!', u16::MAX as usize));
        let archive = Archive::new(original).unwrap();
        let changes = BTreeMap::from([("part.xml".into(), b"updated payload".to_vec())]);
        let updated = Archive::new(archive.write(&changes).unwrap()).unwrap();
        assert_eq!(updated.read("part.xml").unwrap(), changes["part.xml"]);

        let before = &archive.entries[0];
        let after = &updated.entries[0];
        let mut expected_local = archive.original[before.local_header.clone()].to_vec();
        set_u16(&mut expected_local, 6, after.flags);
        set_u32(&mut expected_local, 14, after.crc);
        set_u32(&mut expected_local, 18, after.compressed_size);
        set_u32(&mut expected_local, 22, after.size);
        assert_eq!(updated.original[after.local_header.clone()], expected_local);
        assert_eq!(after.local_record.end, after.data.end);

        let mut expected_central = archive.original[before.central.clone()].to_vec();
        set_u16(&mut expected_central, 8, after.flags);
        set_u32(&mut expected_central, 16, after.crc);
        set_u32(&mut expected_central, 20, after.compressed_size);
        set_u32(&mut expected_central, 24, after.size);
        set_u32(&mut expected_central, 42, after.local_record.start as u32);
        assert_eq!(updated.original[after.central.clone()], expected_central);
        assert_eq!(
            updated.original[updated.end + 22..],
            archive.original[archive.end + 22..]
        );
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

    fn deflate_descriptor_fixture(signed: bool) -> Vec<u8> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"<part/>").unwrap();
        let compressed = encoder.finish().unwrap();
        let archive = Archive::new(deflate_fixture(&compressed)).unwrap();
        let entry = &archive.entries[0];
        let mut bytes = archive.original[entry.local_header.clone()].to_vec();
        set_u16(&mut bytes, 6, 8);
        set_u32(&mut bytes, 14, 0);
        set_u32(&mut bytes, 18, 0);
        set_u32(&mut bytes, 22, 0);
        bytes.extend_from_slice(&compressed);
        if signed {
            bytes.extend_from_slice(&DESCRIPTOR.to_le_bytes());
        }
        bytes.extend_from_slice(&entry.crc.to_le_bytes());
        bytes.extend_from_slice(&entry.compressed_size.to_le_bytes());
        bytes.extend_from_slice(&entry.size.to_le_bytes());
        let central_start = bytes.len();
        let mut central = archive.original[entry.central.clone()].to_vec();
        set_u16(&mut central, 8, 8);
        bytes.extend_from_slice(&central);
        let mut ending = archive.original[archive.end..].to_vec();
        set_u32(&mut ending, 16, central_start as u32);
        bytes.extend_from_slice(&ending);
        bytes
    }

    fn decorate_fixture(name: &[u8; 8], opaque_method: bool) -> Vec<u8> {
        let archive = Archive::new(fixture(true)).unwrap();
        let entry = &archive.entries[0];
        let extra = [0xef, 0xbe, 3, 0, b'x', b'y', b'z'];
        let mut bytes = archive.original[entry.local_header.clone()].to_vec();
        bytes[30..38].copy_from_slice(name);
        set_u16(&mut bytes, 28, extra.len() as u16);
        set_u16(&mut bytes, 10, 0x1234);
        set_u16(&mut bytes, 12, 0x5678);
        if opaque_method {
            set_u16(&mut bytes, 8, 99);
        }
        bytes.extend_from_slice(&extra);
        bytes.extend_from_slice(&archive.original[entry.data.start..entry.local_record.end]);
        let central_start = bytes.len();
        let mut central = archive.original[entry.central.clone()].to_vec();
        central[46..54].copy_from_slice(name);
        set_u16(&mut central, 12, 0x1234);
        set_u16(&mut central, 14, 0x5678);
        set_u16(&mut central, 30, extra.len() as u16);
        set_u16(&mut central, 32, 4);
        set_u16(&mut central, 36, 0x789a);
        set_u32(&mut central, 38, 0xcdef_4321);
        if opaque_method {
            set_u16(&mut central, 10, 99);
        }
        central.extend_from_slice(&extra);
        central.extend_from_slice(&[0, 0xff, 0x80, b'!']);
        bytes.extend_from_slice(&central);
        let mut ending = archive.original[archive.end..].to_vec();
        set_u32(&mut ending, 12, central.len() as u32);
        set_u32(&mut ending, 16, central_start as u32);
        bytes.extend_from_slice(&ending);
        bytes
    }

    fn combine(first: Vec<u8>, second: Vec<u8>) -> Vec<u8> {
        combine_members([first, second])
    }

    fn combine_members(parts: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut centrals = Vec::new();
        let mut ending = None;
        for part in parts {
            let part = Archive::new(part).unwrap();
            let offset = bytes.len();
            bytes.extend_from_slice(&part.original[..part.central_start]);
            let mut central = part.original[part.central_start..part.end].to_vec();
            set_u32(&mut central, 42, offset as u32);
            centrals.push(central);
            ending.get_or_insert_with(|| part.original[part.end..].to_vec());
        }
        let count = centrals.len();
        let central_start = bytes.len();
        for central in centrals {
            bytes.extend_from_slice(&central);
        }
        let mut ending = ending.unwrap();
        set_u16(&mut ending, 8, count as u16);
        set_u16(&mut ending, 10, count as u16);
        set_u32(&mut ending, 12, (bytes.len() - central_start) as u32);
        set_u32(&mut ending, 16, central_start as u32);
        bytes.extend_from_slice(&ending);
        bytes
    }

    #[test]
    fn streams_exact_output_through_short_writes_and_propagates_io_errors() {
        struct ShortWriter {
            bytes: Vec<u8>,
            limit: usize,
        }
        impl Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.bytes.len() == self.limit {
                    return Err(std::io::Error::other("injected failure"));
                }
                let length = bytes.len().min(3).min(self.limit - self.bytes.len());
                self.bytes.extend_from_slice(&bytes[..length]);
                Ok(length)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let archive = Archive::new(fixture(true)).unwrap();
        let changes = BTreeMap::from([("part.xml".into(), b"changed".to_vec())]);
        for changes in [&BTreeMap::new(), &changes] {
            let expected = archive.write(changes).unwrap();
            let mut writer = ShortWriter {
                bytes: Vec::new(),
                limit: usize::MAX,
            };
            archive.write_to(changes, &mut writer).unwrap();
            assert_eq!(writer.bytes, expected);
            for limit in [0, 1, archive.central_start, expected.len() - 1] {
                let mut writer = ShortWriter {
                    bytes: Vec::new(),
                    limit,
                };
                assert!(matches!(
                    archive.write_to(changes, &mut writer),
                    Err(Error::Io(_))
                ));
                assert_eq!(writer.bytes, expected[..limit]);
            }
        }
        let mut writer = ShortWriter {
            bytes: Vec::new(),
            limit: 1,
        };
        assert!(matches!(
            archive.compact_to(&changes, &mut writer),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn rejects_bad_changes_before_streaming_any_bytes() {
        let archive = Archive::new(decorate_fixture(b"part.xml", true)).unwrap();
        for name in ["part.xml", "missing.xml"] {
            let changes = BTreeMap::from([(name.into(), b"changed".to_vec())]);
            let mut output = Vec::new();
            assert!(archive.write_to(&changes, &mut output).is_err());
            assert!(output.is_empty());
            assert!(archive.compact_to(&changes, &mut output).is_err());
            assert!(output.is_empty());
        }
    }

    #[test]
    fn patched_zip_members_remain_opaque_and_cannot_be_read_or_replaced() {
        let mut patched = fixture(false);
        let archive = Archive::new(patched.clone()).unwrap();
        let central = archive.entries[0].central.start;
        set_u16(&mut patched, 6, 0x20);
        set_u16(&mut patched, central + 8, 0x20);
        let original = combine(patched, decorate_fixture(b"keep.xml", false));
        let archive = Archive::new(original.clone()).unwrap();
        assert!(matches!(
            archive.read("part.xml"),
            Err(Error::Unsupported(message)) if message.contains("patched")
        ));
        assert_eq!(archive.read("keep.xml").unwrap(), b"<part/>");
        assert_eq!(archive.write(&BTreeMap::new()).unwrap(), original);
        let changes = BTreeMap::from([("part.xml".into(), b"replacement".to_vec())]);
        let mut output = Vec::new();
        assert!(matches!(
            archive.write_to(&changes, &mut output),
            Err(Error::Unsupported(message)) if message.contains("patched")
        ));
        assert!(output.is_empty());
        let changes = BTreeMap::from([("keep.xml".into(), b"replacement".to_vec())]);
        let output = archive.write(&changes).unwrap();
        let reopened = Archive::new(output).unwrap();
        assert_eq!(reopened.read("keep.xml").unwrap(), b"replacement");
        let before = &archive.entries[archive.index["part.xml"]];
        let after = &reopened.entries[reopened.index["part.xml"]];
        assert_eq!(
            reopened.original[after.local_record.clone()],
            archive.original[before.local_record.clone()]
        );
    }

    #[test]
    fn compaction_never_discards_obsolete_patched_zip_members() {
        let archive = Archive::new(fixture(false)).unwrap();
        let changes = BTreeMap::from([("part.xml".into(), b"replacement".to_vec())]);
        let mut original = append(&archive, &changes);
        set_u16(&mut original, 6, 0x20);
        let archive = Archive::new(original.clone()).unwrap();
        assert_eq!(archive.read("part.xml").unwrap(), b"replacement");
        assert_eq!(archive.write(&BTreeMap::new()).unwrap(), original);
        let mut output = Vec::new();
        assert!(archive.compact_to(&BTreeMap::new(), &mut output).is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn compaction_keeps_clean_archives_and_all_descriptor_forms_byte_identical() {
        let mut unsigned = fixture(true);
        let signed_offset = 30 + b"part.xml".len() + b"<part/>".len();
        unsigned.drain(signed_offset..signed_offset + 4);
        let end = unsigned.len() - 29;
        let offset = u32_at(&unsigned, end + 16).unwrap() - 4;
        set_u32(&mut unsigned, end + 16, offset);
        for original in [
            fixture(false),
            fixture(true),
            unsigned,
            deflate_descriptor_fixture(false),
            deflate_descriptor_fixture(true),
        ] {
            let archive = Archive::new(original.clone()).unwrap();
            let mut output = Vec::new();
            archive.compact_to(&BTreeMap::new(), &mut output).unwrap();
            assert_eq!(output, original);
            let changed = append(
                &archive,
                &BTreeMap::from([("part.xml".into(), b"new".to_vec())]),
            );
            let mut compacted = Vec::new();
            Archive::new(changed)
                .unwrap()
                .compact_to(&BTreeMap::new(), &mut compacted)
                .unwrap();
            assert_eq!(
                Archive::new(compacted).unwrap().read("part.xml").unwrap(),
                b"new"
            );
        }
    }

    #[test]
    fn repeated_compaction_stays_small_and_preserves_untouched_records_and_metadata() {
        let original = combine(
            decorate_fixture(b"part.xml", false),
            decorate_fixture(b"keep.xml", true),
        );
        let archive = Archive::new(original).unwrap();
        let untouched = &archive.entries[1];
        let local = archive.original[untouched.local_record.clone()].to_vec();
        let central = archive.original[untouched.central.clone()].to_vec();
        let mut current = archive;
        let mut stable_size = None;
        for index in 0..10 {
            let changes =
                BTreeMap::from([("part.xml".into(), format!("value {index}").into_bytes())]);
            let appended = append(&current, &changes);
            let mut compacted = Vec::new();
            let appended = Archive::new(appended).unwrap();
            appended
                .compact_to(&BTreeMap::new(), &mut compacted)
                .unwrap();
            assert!(compacted.len() < appended.original.len());
            if let Some(size) = stable_size {
                assert_eq!(compacted.len(), size);
            } else {
                stable_size = Some(compacted.len());
            }
            current = Archive::new(compacted).unwrap();
            assert_eq!(current.read("part.xml").unwrap(), changes["part.xml"]);
            let untouched = &current.entries[1];
            assert_eq!(current.original[untouched.local_record.clone()], local);
            let new_central = &current.original[untouched.central.clone()];
            assert_eq!(new_central[..42], central[..42]);
            assert_eq!(new_central[46..], central[46..]);
            assert!(current.original.ends_with(b"comment"));
        }
        let mut compacted_again = Vec::new();
        current
            .compact_to(&BTreeMap::new(), &mut compacted_again)
            .unwrap();
        assert_eq!(compacted_again, current.original);
    }

    #[test]
    fn compaction_applies_pending_changes_without_retaining_the_previous_member() {
        let archive = Archive::new(fixture(false)).unwrap();
        let changes = BTreeMap::from([("part.xml".into(), b"updated".to_vec())]);
        let mut compacted = Vec::new();
        archive.compact_to(&changes, &mut compacted).unwrap();
        let expected_len = archive.original.len() - b"<part/>".len() + b"updated".len();
        assert_eq!(compacted.len(), expected_len);
        assert_eq!(
            Archive::new(compacted).unwrap().read("part.xml").unwrap(),
            b"updated"
        );
    }

    #[test]
    fn compaction_checks_obsolete_deflate_payloads_across_output_chunks() {
        let archive = Archive::new(deflate_descriptor_fixture(true)).unwrap();
        let large = b"123456789".repeat(3000);
        let archive = Archive::new(
            archive
                .write(&BTreeMap::from([("part.xml".into(), large.clone())]))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(archive.read("part.xml").unwrap(), large);
        let appended = Archive::new(append(
            &archive,
            &BTreeMap::from([("part.xml".into(), b"updated".to_vec())]),
        ))
        .unwrap();
        let mut compacted = Vec::new();
        appended
            .compact_to(&BTreeMap::new(), &mut compacted)
            .unwrap();
        assert!(compacted.len() < appended.original.len());
        assert_eq!(
            Archive::new(compacted).unwrap().read("part.xml").unwrap(),
            b"updated"
        );
    }

    #[test]
    fn compacts_many_stored_streaming_orphans_in_one_gap() {
        let original = combine_members((0..64).map(|index| {
            let name: [u8; 8] = format!("part{index:04}").into_bytes().try_into().unwrap();
            decorate_fixture(&name, false)
        }));
        let archive = Archive::new(original).unwrap();
        let changes: BTreeMap<_, _> = (0..64)
            .map(|index| {
                (
                    format!("part{index:04}"),
                    format!("updated {index}").into_bytes(),
                )
            })
            .collect();
        let appended = Archive::new(append(&archive, &changes)).unwrap();
        let mut compacted = Vec::new();
        appended
            .compact_to(&BTreeMap::new(), &mut compacted)
            .unwrap();
        assert!(compacted.len() < appended.original.len());
        let compacted = Archive::new(compacted).unwrap();
        for (name, expected) in changes {
            assert_eq!(compacted.read(&name).unwrap(), expected);
        }
    }

    #[test]
    fn compaction_refuses_opaque_prefix_gaps_and_unrecognized_orphans_without_writing() {
        let archive = Archive::new(fixture(false)).unwrap();
        for prefix in [false, true] {
            let mut bytes = if prefix {
                b"opaque".to_vec()
            } else {
                Vec::new()
            };
            bytes.extend_from_slice(&archive.original[..archive.central_start]);
            if !prefix {
                bytes.extend_from_slice(b"opaque");
            }
            let central_start = bytes.len();
            let mut central = archive.original[archive.central_start..archive.end].to_vec();
            if prefix {
                set_u32(&mut central, 42, 6);
            }
            bytes.extend_from_slice(&central);
            let mut ending = archive.original[archive.end..].to_vec();
            set_u32(&mut ending, 16, central_start as u32);
            bytes.extend_from_slice(&ending);
            let with_gap = Archive::new(bytes.clone()).unwrap();
            assert_eq!(with_gap.write(&BTreeMap::new()).unwrap(), bytes);
            let mut output = Vec::new();
            assert!(matches!(
                with_gap.compact_to(&BTreeMap::new(), &mut output),
                Err(Error::Unsupported(_))
            ));
            assert!(output.is_empty());
        }
        let unrelated = Archive::new(decorate_fixture(b"keep.xml", false)).unwrap();
        let mut bytes = archive.original[..archive.central_start].to_vec();
        bytes.extend_from_slice(&unrelated.original[..unrelated.central_start]);
        let central_start = bytes.len();
        bytes.extend_from_slice(&archive.original[archive.central_start..archive.end]);
        let mut ending = archive.original[archive.end..].to_vec();
        set_u32(&mut ending, 16, central_start as u32);
        bytes.extend_from_slice(&ending);
        let archive = Archive::new(bytes).unwrap();
        let mut output = Vec::new();
        assert!(matches!(
            archive.compact_to(&BTreeMap::new(), &mut output),
            Err(Error::Unsupported(_))
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn compaction_does_not_discard_corrupt_obsolete_records() {
        let archive = Archive::new(fixture(true)).unwrap();
        let mut changed = append(
            &archive,
            &BTreeMap::from([("part.xml".into(), b"updated".to_vec())]),
        );
        changed[archive.entries[0].data.start] ^= 1;
        let archive = Archive::new(changed).unwrap();
        let mut output = Vec::new();
        assert!(archive.compact_to(&BTreeMap::new(), &mut output).is_err());
        assert!(output.is_empty());
    }
}
