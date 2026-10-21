//! A ZIP32 container that retains every untouched record verbatim.
//!
//! Changed members replace their original local records; untouched records and
//! opaque gaps are copied verbatim. This avoids decoding unrelated parts and
//! introducing unreferenced local records that some spreadsheet readers reject.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    fmt::Display,
    io::{Read, Write},
    ops::Range,
};

use flate2::{Compression, Decompress, FlushDecompress, Status, write::DeflateEncoder};

use crate::{Error, Result};

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;
const DESCRIPTOR: u32 = 0x0807_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
// Legacy .xls files and password-encrypted OOXML packages are not ZIP archives
// but OLE compound files.
const OLE_SIGNATURE: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];

const LOCAL_HEADER_LEN: usize = 30;
const CENTRAL_HEADER_LEN: usize = 46;
const END_LEN: usize = 22;
const ZIP64_LOCATOR_LEN: usize = 20;
const DESCRIPTOR_LEN: usize = 12;
const SIGNED_DESCRIPTOR_LEN: usize = 16;

const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_DESCRIPTOR: u16 = 0x0008;
const FLAG_PATCHED: u16 = 0x0020;
const FLAG_STRONG_ENCRYPTION: u16 = 0x0040;
const FLAG_UTF8: u16 = 0x0800;
const FLAG_MASKED_HEADERS: u16 = 0x2000;
// Patched data is not encryption: active patched members stay opaque, while
// compaction refuses to discard obsolete ones (`FLAG_PATCHED` at those sites).
const ENCRYPTION_FLAGS: u16 = FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTION | FLAG_MASKED_HEADERS;

const STORED: u16 = 0;
const DEFLATED: u16 = 8;

const EXTRA_ZIP64: u16 = 0x0001;
const EXTRA_UNICODE_PATH: u16 = 0x7075;

const MAX_READ: usize = 64 * 1024 * 1024;
/// The length of the largest possible ZIP32 archive: the central directory's
/// offset and size are each below `u32::MAX`, and only a 22-byte end record
/// and a comment of at most 64 KiB follow it.
pub(crate) const MAX_ARCHIVE_LEN: u64 = 2 * u32::MAX as u64 + END_LEN as u64 + u16::MAX as u64;
// Level 4 is the fastest level that keeps lazy matching. For worksheet XML it
// produces archives about as small as the default level 6, several times faster.
const DEFLATE_LEVEL: u32 = 4;

pub(crate) trait PartChanges {
    // Names are unique; iteration order is also the validation error order.
    fn parts(&self) -> impl Iterator<Item = (&str, &[u8])>;

    fn is_empty(&self) -> bool {
        self.parts().next().is_none()
    }
}

impl PartChanges for BTreeMap<String, Vec<u8>> {
    fn parts(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.iter()
            .map(|(name, data)| (name.as_str(), data.as_slice()))
    }

    fn is_empty(&self) -> bool {
        BTreeMap::is_empty(self)
    }
}

/// A member's CRC-32 and sizes, which local headers, central headers and data
/// descriptors all store as consecutive fields in this order.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Sizes {
    crc: u32,
    compressed_size: u32,
    size: u32,
}

impl Sizes {
    fn read(bytes: &[u8], offset: usize) -> Result<Self> {
        Ok(Self {
            crc: u32_at(bytes, offset)?,
            compressed_size: u32_at(bytes, add(offset, 4)?)?,
            size: u32_at(bytes, add(offset, 8)?)?,
        })
    }

    fn write(self, bytes: &mut [u8], offset: usize) {
        set_u32(bytes, offset, self.crc);
        set_u32(bytes, offset + 4, self.compressed_size);
        set_u32(bytes, offset + 8, self.size);
    }

    /// Whether header fields agree with the actual values. A header followed
    /// by a data descriptor may leave any field zero instead.
    fn matches(self, actual: Self, has_descriptor: bool) -> bool {
        [
            (self.crc, actual.crc),
            (self.compressed_size, actual.compressed_size),
            (self.size, actual.size),
        ]
        .into_iter()
        .all(|(header, actual)| header == actual || (has_descriptor && header == 0))
    }
}

pub(crate) struct Entry {
    pub(crate) name: String,
    flags: u16,
    method: u16,
    sizes: Sizes,
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

/// A central-directory header whose variable fields lie within the directory.
struct CentralHeader<'a> {
    flags: u16,
    method: u16,
    sizes: Sizes,
    name: &'a str,
    extra: &'a [u8],
    local_start: usize,
    record: Range<usize>,
}

/// A local header whose fixed fields, name and extra field end by a limit.
struct LocalHeader<'a> {
    flags: u16,
    method: u16,
    sizes: Sizes,
    raw_name: &'a [u8],
    extra: &'a [u8],
    data_start: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct StoredDescriptor {
    data_end: usize,
    end: usize,
    sizes: Sizes,
}

struct ChangePlan<'a> {
    // Keep only changed entry indexes and borrowed payloads, ordered by their
    // physical local-record positions for a single merge during emission.
    data: Vec<(usize, &'a [u8])>,
}

#[derive(Clone, Copy)]
enum RecordPolicy {
    PreserveGaps,
    Compact,
}

// `None` means more than one structurally plausible descriptor points to that
// payload start. Compaction refuses this ambiguity rather than choosing one.
type StoredDescriptors = HashMap<usize, Option<StoredDescriptor>>;

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidWorkbook(message.into())
}

fn unsupported(message: impl Into<String>) -> Error {
    Error::Unsupported(message.into())
}

fn zip64() -> Error {
    unsupported("ZIP64 workbooks are not supported")
}

fn over_read_limit(member: impl Display) -> Error {
    unsupported(format!(
        "{member} exceeds the {} MiB read limit",
        MAX_READ >> 20
    ))
}

pub(crate) fn archive_too_large() -> Error {
    unsupported("input exceeds the largest possible ZIP32 archive")
}

/// Append everything `reader` yields to `bytes`, refusing input longer than
/// any ZIP32 archive once that much has been read.
pub(crate) fn read_archive(reader: impl Read, bytes: Vec<u8>) -> Result<Vec<u8>> {
    read_at_most(reader, bytes, MAX_ARCHIVE_LEN)
}

fn read_at_most(reader: impl Read, mut bytes: Vec<u8>, limit: u64) -> Result<Vec<u8>> {
    let start = bytes.len();
    // One byte past the limit shows that the input is too long.
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if (bytes.len() - start) as u64 > limit {
        return Err(archive_too_large());
    }
    Ok(bytes)
}

fn field<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk::<N>)
        .copied()
        .ok_or_else(|| invalid("truncated ZIP record"))
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    field(bytes, offset).map(u16::from_le_bytes)
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    field(bytes, offset).map(u32::from_le_bytes)
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
        .map_err(|_| unsupported(format!("{description} exceeds ZIP32 limits")))?;
    if value == u32::MAX {
        return Err(unsupported(format!("{description} requires ZIP64")));
    }
    Ok(value)
}

/// Reject extra fields that change how other readers interpret a member: ZIP64
/// sizes and Unicode paths that rename it.
fn check_extra(mut bytes: &[u8], name: &str) -> Result<()> {
    // A long name can have thousands of stale Unicode path fields. Hash it
    // only once, rather than once per field, to keep validation linear.
    let mut name_crc = None;
    while !bytes.is_empty() {
        if bytes.len() < 4 {
            return Err(invalid("truncated ZIP extra field"));
        }
        let kind = u16_at(bytes, 0)?;
        let length = u16_at(bytes, 2)? as usize;
        if kind == EXTRA_ZIP64 {
            return Err(zip64());
        }
        let field = bytes
            .get(4..4 + length)
            .ok_or_else(|| invalid("truncated ZIP extra field"))?;
        // Python, Info-ZIP and libarchive replace the raw name with a Unicode
        // path whose CRC (after a version byte) matches the raw name. They
        // disagree on versions: Python takes version 1, Info-ZIP also version
        // 0 in the central directory, and libarchive any version in a local
        // header. So the version is ignored here. Repeating the raw name is
        // harmless; any other path would alias a member for some reader.
        if kind == EXTRA_UNICODE_PATH
            && u32_at(field, 1)
                .is_ok_and(|crc| crc == *name_crc.get_or_insert_with(|| crc32(name.as_bytes())))
            && field[5..] != *name.as_bytes()
        {
            return Err(invalid(format!(
                "ZIP Unicode path extra field renames member {name}"
            )));
        }
        bytes = &bytes[4 + length..];
    }
    Ok(())
}

/// Whether an end record followed by its comment, `ending`, contains another
/// end-record signature with room for a complete record before EOF.
///
/// Readers that scan back from EOF select the last such signature even when
/// its comment does not reach EOF, so one inside the record's own fields or
/// comment makes the archive ambiguous. A signature too close to EOF for a
/// complete record cannot be read as one; see also [`shadows_end_record`].
fn hides_end_record(ending: &[u8]) -> bool {
    let last = ending.len().saturating_sub(END_LEN);
    (1..=last).any(|offset| ending[offset..offset + 4] == END.to_le_bytes())
}

/// Whether Python's `zipfile` would select an end-record signature after the
/// start of `ending`, an end record followed by its comment. It takes the last
/// signature in the archive's tail, and fails to open the archive when no
/// complete record follows it. Only an archive without a comment is exempt:
/// Python first looks for a record in exactly the last 22 bytes.
fn shadows_end_record(ending: &[u8]) -> bool {
    ending.len() > END_LEN
        && ending[1..]
            .windows(4)
            .any(|window| window == END.to_le_bytes())
}

/// Locate the end-of-central-directory record. It must be the only record
/// that ends at EOF with its comment, and the only one readers could select.
fn end_of_central_directory(bytes: &[u8]) -> Result<usize> {
    if bytes.len() < END_LEN {
        return Err(invalid("file is not a ZIP workbook"));
    }
    // A valid EOCD ends at EOF, including its comment. Multiple matching
    // EOCDs would make the member index ambiguous and are rejected.
    let search_start = bytes.len().saturating_sub(END_LEN + u16::MAX as usize);
    let mut endings = Vec::new();
    for offset in (search_start..=bytes.len() - END_LEN).rev() {
        if u32_at(bytes, offset)? == END {
            let comment = u16_at(bytes, offset + 20)? as usize;
            if add(add(offset, END_LEN)?, comment)? == bytes.len() {
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
    // Fixed fields may legitimately hold the signature's bytes, for example a
    // central directory at offset 0x06054b50. That matters only when the
    // comment leaves room for a complete record after the signature.
    if hides_end_record(&bytes[end..]) {
        return Err(invalid("ambiguous ZIP end-of-directory records"));
    }
    // libzip before 1.11 instead scans its tail window forward and keeps the
    // first record whose directory it can read, unless a later one spans more
    // local data. Refuse any earlier record it could select.
    let window = bytes
        .len()
        .saturating_sub(END_LEN + u16::MAX as usize + ZIP64_LOCATOR_LEN);
    for offset in window..end {
        if decoy_end_record(bytes, offset)? {
            return Err(invalid("ambiguous ZIP end-of-directory records"));
        }
    }
    Ok(end)
}

/// Whether a reader scanning forward could select an end record at `offset`:
/// a complete single-disk record for a nonempty directory that ends before the
/// record and starts with a central-directory header, and a comment that ends
/// by EOF. Such a reader never prefers an empty directory to another one.
///
/// A Stored embedded ZIP's own end record normally fails the last test,
/// because its directory offset is relative to the embedded archive.
fn decoy_end_record(bytes: &[u8], offset: usize) -> Result<bool> {
    let record_end = add(offset, END_LEN)?;
    if record_end > bytes.len() || u32_at(bytes, offset)? != END {
        return Ok(false);
    }
    let entries = u16_at(bytes, offset + 10)?;
    let comment_end = add(record_end, u16_at(bytes, offset + 20)? as usize)?;
    let central_start = u32_at(bytes, offset + 16)? as usize;
    let central_end = add(central_start, u32_at(bytes, offset + 12)? as usize)?;
    Ok(u16_at(bytes, offset + 4)? == 0
        && u16_at(bytes, offset + 6)? == 0
        && u16_at(bytes, offset + 8)? == entries
        && entries != 0
        && comment_end <= bytes.len()
        && central_end <= offset
        && u32_at(bytes, central_start)? == CENTRAL)
}

/// Return the central directory's start and entry count from the EOCD.
fn central_directory(bytes: &[u8], end: usize) -> Result<(usize, u16)> {
    if end >= ZIP64_LOCATOR_LEN && u32_at(bytes, end - ZIP64_LOCATOR_LEN)? == ZIP64_LOCATOR {
        return Err(zip64());
    }
    let disk = u16_at(bytes, end + 4)?;
    let central_disk = u16_at(bytes, end + 6)?;
    let disk_entries = u16_at(bytes, end + 8)?;
    let count = u16_at(bytes, end + 10)?;
    let central_size = u32_at(bytes, end + 12)?;
    let central_offset = u32_at(bytes, end + 16)?;
    if disk != 0 || central_disk != 0 || disk_entries != count {
        return Err(unsupported("multi-disk ZIP workbooks are not supported"));
    }
    if count == u16::MAX || central_size == u32::MAX || central_offset == u32::MAX {
        return Err(zip64());
    }
    let central_start = central_offset as usize;
    // Ending exactly at the EOCD also keeps the directory within the file.
    if add(central_start, central_size as usize)? != end {
        return Err(invalid("ZIP central-directory bounds are inconsistent"));
    }
    Ok((central_start, count))
}

fn member_name(raw_name: &[u8], flags: u16) -> Result<&str> {
    // Without bit 11, ZIP readers can decode these bytes as CP437 instead of
    // UTF-8, even when the bytes happen to be valid UTF-8.
    if flags & FLAG_UTF8 == 0 && !raw_name.is_ascii() {
        return Err(unsupported(
            "non-ASCII ZIP member names require the UTF-8 flag",
        ));
    }
    std::str::from_utf8(raw_name).map_err(|_| unsupported("ZIP member names must be UTF-8"))
}

fn central_header(bytes: &[u8], position: usize, end: usize) -> Result<CentralHeader<'_>> {
    if add(position, CENTRAL_HEADER_LEN)? > end || u32_at(bytes, position)? != CENTRAL {
        return Err(invalid("truncated or invalid ZIP central-directory entry"));
    }
    let flags = u16_at(bytes, position + 8)?;
    if flags & ENCRYPTION_FLAGS != 0 {
        return Err(unsupported("encrypted ZIP workbooks are not supported"));
    }
    let method = u16_at(bytes, position + 10)?;
    let sizes = Sizes::read(bytes, position + 16)?;
    let name_length = u16_at(bytes, position + 28)? as usize;
    let extra_length = u16_at(bytes, position + 30)? as usize;
    let comment_length = u16_at(bytes, position + 32)? as usize;
    let disk = u16_at(bytes, position + 34)?;
    let local_offset = u32_at(bytes, position + 42)?;
    if sizes.compressed_size == u32::MAX
        || sizes.size == u32::MAX
        || local_offset == u32::MAX
        || disk == u16::MAX
    {
        return Err(zip64());
    }
    if disk != 0 {
        return Err(unsupported("multi-disk ZIP workbooks are not supported"));
    }
    let name_start = add(position, CENTRAL_HEADER_LEN)?;
    let name_end = add(name_start, name_length)?;
    let extra_end = add(name_end, extra_length)?;
    let record_end = add(extra_end, comment_length)?;
    if record_end > end {
        return Err(invalid(
            "ZIP central-directory entry exceeds directory bounds",
        ));
    }
    let name = member_name(&bytes[name_start..name_end], flags)?;
    Ok(CentralHeader {
        flags,
        method,
        sizes,
        name,
        extra: &bytes[name_end..extra_end],
        local_start: local_offset as usize,
        record: position..record_end,
    })
}

/// Decode the local header at `start`. `None` means no complete local header,
/// including its name and extra field, ends by `limit`.
fn local_header(bytes: &[u8], start: usize, limit: usize) -> Result<Option<LocalHeader<'_>>> {
    let name_start = add(start, LOCAL_HEADER_LEN)?;
    if name_start > limit || u32_at(bytes, start)? != LOCAL {
        return Ok(None);
    }
    let name_end = add(name_start, u16_at(bytes, start + 26)? as usize)?;
    let data_start = add(name_end, u16_at(bytes, start + 28)? as usize)?;
    if data_start > limit {
        return Ok(None);
    }
    Ok(Some(LocalHeader {
        flags: u16_at(bytes, start + 6)?,
        method: u16_at(bytes, start + 8)?,
        sizes: Sizes::read(bytes, start + 14)?,
        raw_name: &bytes[name_start..name_end],
        extra: &bytes[name_end..data_start],
        data_start,
    }))
}

fn descriptor_end(
    bytes: &[u8],
    data_end: usize,
    limit: usize,
    expected: Sizes,
    name: &str,
) -> Result<usize> {
    let matches = |offset: usize| -> bool {
        add(offset, DESCRIPTOR_LEN).is_ok_and(|end| end <= limit)
            && Sizes::read(bytes, offset).ok() == Some(expected)
    };
    let unsigned = matches(data_end);
    let signed = u32_at(bytes, data_end).ok() == Some(DESCRIPTOR) && matches(add(data_end, 4)?);
    match (unsigned, signed) {
        (true, false) => add(data_end, DESCRIPTOR_LEN),
        (false, true) => add(data_end, SIGNED_DESCRIPTOR_LEN),
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

impl Entry {
    /// Validate a member's local record against its central header.
    fn new(bytes: &[u8], central: &CentralHeader<'_>, central_start: usize) -> Result<Self> {
        let name = central.name;
        let Some(local) = local_header(bytes, central.local_start, central_start)? else {
            return Err(invalid(format!("invalid ZIP local header for {name}")));
        };
        if local.flags != central.flags || local.method != central.method {
            return Err(invalid(format!("conflicting ZIP headers for {name}")));
        }
        if local.raw_name != name.as_bytes() {
            return Err(invalid(format!("conflicting ZIP member name for {name}")));
        }
        check_extra(local.extra, name)?;
        let sizes = central.sizes;
        let has_descriptor = central.flags & FLAG_DESCRIPTOR != 0;
        if !local.sizes.matches(sizes, has_descriptor) {
            return Err(invalid(format!(
                "conflicting ZIP sizes or checksum for {name}"
            )));
        }
        if central.method == STORED && sizes.compressed_size != sizes.size {
            return Err(invalid(format!(
                "invalid stored ZIP member size for {name}"
            )));
        }
        let data_end = add(local.data_start, sizes.compressed_size as usize)?;
        if data_end > central_start {
            return Err(invalid(format!(
                "ZIP member {name} exceeds local-record bounds"
            )));
        }
        let local_end = if has_descriptor {
            descriptor_end(bytes, data_end, central_start, sizes, name)?
        } else {
            data_end
        };
        Ok(Self {
            name: name.to_owned(),
            flags: central.flags,
            method: central.method,
            sizes,
            central: central.record.clone(),
            local_header: central.local_start..local.data_start,
            data: local.data_start..data_end,
            local_record: central.local_start..local_end,
        })
    }
}

impl Archive {
    pub(crate) fn new(original: Vec<u8>) -> Result<Self> {
        if original.starts_with(&OLE_SIGNATURE) {
            return Err(unsupported(
                "legacy .xls and password-encrypted workbooks (OLE compound files) are not supported",
            ));
        }
        let end = end_of_central_directory(&original)?;
        let (central_start, count) = central_directory(&original, end)?;
        let mut entries = Vec::with_capacity(count as usize);
        let mut index = HashMap::with_capacity(count as usize);
        let mut part_index = HashMap::with_capacity(count as usize);
        let mut position = central_start;
        for _ in 0..count {
            let central = central_header(&original, position, end)?;
            let name = central.name;
            // Some readers treat '\' as '/', so such a name can alias another
            // member for them. OPC part names cannot contain it.
            if name.contains('\\') {
                return Err(invalid(format!(
                    "ZIP member name contains a backslash: {name}"
                )));
            }
            if name.is_empty()
                || name.contains('\0')
                || index.insert(name.to_owned(), entries.len()).is_some()
            {
                return Err(invalid("empty, invalid, or duplicate ZIP member name"));
            }
            check_extra(central.extra, name)?;
            // OPC part names compare without ASCII case, including the hex
            // digits in URI escapes. Retain exact ZIP names for preservation;
            // reject an ambiguous equivalent name only if that part is used.
            part_index
                .entry(name.to_ascii_lowercase())
                .and_modify(|entry| *entry = None)
                .or_insert(Some(entries.len()));
            let entry = Entry::new(&original, &central, central_start)?;
            position = entry.central.end;
            entries.push(entry);
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
        if entry.flags & FLAG_PATCHED != 0 {
            return Err(unsupported(format!("patched ZIP member data in {name}")));
        }
        let sizes = entry.sizes;
        if sizes.size as usize > MAX_READ {
            return Err(over_read_limit(format_args!("ZIP member {name}")));
        }
        let compressed = &self.original[entry.data.clone()];
        let output = match entry.method {
            STORED => compressed.to_vec(),
            DEFLATED => {
                let mut decoder = Decompress::new(false);
                // The declared length also bounds malformed streams that expand
                // beyond their header, rather than trusting that header.
                let mut output = Vec::with_capacity(sizes.size as usize + 1);
                let status = decoder
                    .decompress_vec(compressed, &mut output, FlushDecompress::Finish)
                    .map_err(|error| {
                        invalid(format!("invalid deflate stream in {name}: {error}"))
                    })?;
                if status != Status::StreamEnd || decoder.total_in() != sizes.compressed_size as u64
                {
                    return Err(invalid(format!(
                        "incomplete deflate stream or length mismatch in {name}"
                    )));
                }
                output
            }
            method => {
                return Err(unsupported(format!(
                    "ZIP compression method {method} in {name}"
                )));
            }
        };
        if output.len() != sizes.size as usize || crc32(&output) != sizes.crc {
            return Err(invalid(format!("ZIP size or checksum mismatch in {name}")));
        }
        Ok(output)
    }

    pub(crate) fn write(&self, changes: &impl PartChanges) -> Result<Vec<u8>> {
        let mut output = Vec::with_capacity(self.original.len());
        self.write_to(changes, &mut output)?;
        Ok(output)
    }

    /// Stream local records in their original order, replacing changed members.
    ///
    /// Only one changed member's compressed payload is held in memory at a
    /// time. As with `Write::write_all`, a writer error may leave partial output,
    /// and so may the rare refusal of an ambiguous end record, which can only be
    /// detected once the directory offset is known. The writer is flushed before
    /// success is reported.
    pub(crate) fn write_to<W: Write>(
        &self,
        changes: &impl PartChanges,
        writer: &mut W,
    ) -> Result<()> {
        if changes.is_empty() {
            writer.write_all(&self.original)?;
        } else {
            let plan = self.validate_changes(changes)?;
            self.emit_records(&plan, writer, RecordPolicy::PreserveGaps)?;
        }
        // A buffered writer moved into a public save method is dropped right
        // after it returns, and dropping discards errors from its final write.
        writer.flush()?;
        Ok(())
    }

    fn emit_records<W: Write>(
        &self,
        plan: &ChangePlan<'_>,
        writer: &mut W,
        policy: RecordPolicy,
    ) -> Result<()> {
        let preserve_gaps = match policy {
            RecordPolicy::PreserveGaps => true,
            RecordPolicy::Compact => {
                self.check_compactable()?;
                false
            }
        };
        let mut position = 0;
        let mut original_position = 0;
        let mut replacements = vec![None; self.entries.len()];
        let mut offsets = vec![0; self.entries.len()];
        let mut pending = plan.data.iter().peekable();
        // Both policies keep the physical local-record order.
        for &index in &self.local_order {
            let entry = &self.entries[index];
            // Prefixes, gaps and any obsolete records from older versions are
            // opaque here. Preserve them; only explicit compaction removes
            // obsolete records after proving their boundaries and checksums.
            if preserve_gaps {
                emit(
                    writer,
                    &mut position,
                    &self.original[original_position..entry.local_record.start],
                )?;
            }
            offsets[index] = zip32(position, "ZIP local-record offset")?;
            if let Some(&(_, data)) = pending.next_if(|change| change.0 == index) {
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
        if preserve_gaps {
            emit(
                writer,
                &mut position,
                &self.original[original_position..self.central_start],
            )?;
        }
        self.emit_directory(writer, &mut position, &offsets, &replacements)
    }

    /// Reproduce pre-1.0 append saves to exercise migration and compaction.
    #[cfg(test)]
    fn append_to<W: Write>(&self, changes: &impl PartChanges, writer: &mut W) -> Result<()> {
        if changes.is_empty() {
            writer.write_all(&self.original)?;
            return Ok(());
        }
        let mut plan = self.validate_changes(changes)?;
        // Legacy appends emit replacements in central-directory order.
        plan.data.sort_unstable_by_key(|change| change.0);
        let mut position = 0;
        emit(writer, &mut position, &self.original[..self.central_start])?;
        let mut replacements = vec![None; self.entries.len()];
        let mut offsets = Vec::with_capacity(self.entries.len());
        let mut pending = plan.data.iter().peekable();
        for (index, entry) in self.entries.iter().enumerate() {
            if let Some(&(_, data)) = pending.next_if(|change| change.0 == index) {
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
    /// silently discarded by compaction. The writer is flushed before success
    /// is reported.
    pub(crate) fn compact_to<W: Write>(
        &self,
        changes: &impl PartChanges,
        writer: &mut W,
    ) -> Result<()> {
        let plan = self.validate_changes(changes)?;
        self.emit_records(&plan, writer, RecordPolicy::Compact)?;
        writer.flush()?;
        Ok(())
    }

    fn validate_changes<'a>(&self, changes: &'a impl PartChanges) -> Result<ChangePlan<'a>> {
        let parts = changes.parts();
        let mut plan = ChangePlan {
            data: Vec::with_capacity(parts.size_hint().0),
        };
        for (name, data) in parts {
            let Some(&index) = self.index.get(name) else {
                return Err(invalid(format!(
                    "cannot replace missing ZIP member: {name}"
                )));
            };
            if data.len() > MAX_READ {
                return Err(over_read_limit(format_args!("ZIP member {name}")));
            }
            let entry = &self.entries[index];
            if entry.flags & FLAG_PATCHED != 0 {
                return Err(unsupported(format!("patched ZIP member data in {name}")));
            }
            let method = entry.method;
            if !matches!(method, STORED | DEFLATED) {
                return Err(unsupported(format!(
                    "ZIP compression method {method} in {name}"
                )));
            }
            plan.data.push((index, data));
        }
        plan.data
            .sort_unstable_by_key(|&(index, _)| self.entries[index].local_record.start);
        Ok(plan)
    }

    fn emit_replacement<W: Write>(
        &self,
        entry: &Entry,
        data: &[u8],
        writer: &mut W,
        position: &mut usize,
    ) -> Result<Sizes> {
        let size = zip32(data.len(), "ZIP member size")?;
        let compressed = if entry.method == STORED {
            Cow::Borrowed(data)
        } else {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(DEFLATE_LEVEL));
            encoder.write_all(data)?;
            Cow::Owned(encoder.finish()?)
        };
        let compressed_size = zip32(compressed.len(), "compressed ZIP member size")?;
        let sizes = Sizes {
            crc: crc32(data),
            compressed_size,
            size,
        };
        let original_header = &self.original[entry.local_header.clone()];
        let mut header = [0; LOCAL_HEADER_LEN];
        header.copy_from_slice(&original_header[..LOCAL_HEADER_LEN]);
        set_u16(&mut header, 6, entry.flags & !FLAG_DESCRIPTOR);
        sizes.write(&mut header, 14);
        zip32(
            add(add(*position, original_header.len())?, compressed.len())?,
            "ZIP local-record area",
        )?;
        emit(writer, position, &header)?;
        emit(writer, position, &original_header[LOCAL_HEADER_LEN..])?;
        emit(writer, position, &compressed)?;
        Ok(sizes)
    }

    fn emit_directory<W: Write>(
        &self,
        writer: &mut W,
        position: &mut usize,
        offsets: &[u32],
        replacements: &[Option<Sizes>],
    ) -> Result<()> {
        let central_start = zip32(*position, "ZIP central-directory offset")?;
        // Central records are copied whole, so the directory keeps its size,
        // which the input's end record already stored below u32::MAX.
        let central_size = (self.end - self.central_start) as u32;
        let mut ending = self.original[self.end..].to_vec();
        set_u32(&mut ending, 12, central_size);
        set_u32(&mut ending, 16, central_start);
        // Only the directory offset (record bytes 16..20) can differ from the
        // input's end record, which passed `hides_end_record` with the same
        // comment. So a new signature starts at record offset 13 to 19, and
        // only an archive comment exposes it to backward-scanning readers.
        // Refuse that rare layout rather than write an archive that they would
        // misread, or that Python could open before the save but not after.
        if hides_end_record(&ending)
            || (shadows_end_record(&ending) && !shadows_end_record(&self.original[self.end..]))
        {
            return Err(unsupported(
                "saving would make the ZIP end-of-directory record ambiguous: the new \
                 central-directory offset forms a record signature that readers searching \
                 back through the archive comment would select",
            ));
        }
        for (index, entry) in self.entries.iter().enumerate() {
            let record = &self.original[entry.central.clone()];
            let mut header = [0; CENTRAL_HEADER_LEN];
            header.copy_from_slice(&record[..CENTRAL_HEADER_LEN]);
            if let Some(sizes) = replacements[index] {
                set_u16(&mut header, 8, entry.flags & !FLAG_DESCRIPTOR);
                sizes.write(&mut header, 16);
            }
            // The local offset is the only field compaction changes for an
            // untouched member; comments, extras and all other metadata survive.
            set_u32(&mut header, 42, offsets[index]);
            emit(writer, position, &header)?;
            emit(writer, position, &record[CENTRAL_HEADER_LEN..])?;
        }
        debug_assert_eq!(*position - central_start as usize, central_size as usize);
        emit(writer, position, &ending)
    }

    /// Require every byte before the central directory to belong to an active
    /// record or a recognizable obsolete one, so compaction drops nothing opaque.
    fn check_compactable(&self) -> Result<()> {
        let mut position = 0;
        let mut stored_descriptors = None;
        for &index in &self.local_order {
            let entry = &self.entries[index];
            while position < entry.local_record.start {
                position = self.orphan_end(position, &mut stored_descriptors)?;
            }
            if position != entry.local_record.start {
                return Err(unsupported(
                    "cannot compact overlapping or unrecognized ZIP local records",
                ));
            }
            position = entry.local_record.end;
        }
        while position < self.central_start {
            position = self.orphan_end(position, &mut stored_descriptors)?;
        }
        if position != self.central_start {
            return Err(unsupported(
                "cannot compact unrecognized data before the ZIP directory",
            ));
        }
        Ok(())
    }

    fn orphan_end(
        &self,
        start: usize,
        stored_descriptors: &mut Option<StoredDescriptors>,
    ) -> Result<usize> {
        let opaque = || {
            unsupported("cannot compact opaque ZIP prefix, gaps, or unrecognized obsolete records")
        };
        let bytes = &self.original;
        let Some(LocalHeader {
            flags,
            method,
            sizes: header,
            raw_name,
            extra,
            data_start,
        }) = local_header(bytes, start, self.central_start)?
        else {
            return Err(opaque());
        };
        if flags & (ENCRYPTION_FLAGS | FLAG_PATCHED) != 0 || !matches!(method, STORED | DEFLATED) {
            return Err(opaque());
        }
        if header.compressed_size == u32::MAX || header.size == u32::MAX {
            return Err(opaque());
        }
        let name = member_name(raw_name, flags)?;
        let Some(&index) = self.index.get(name) else {
            return Err(opaque());
        };
        if self.entries[index].method != method {
            return Err(opaque());
        }
        check_extra(extra, name)?;
        let has_descriptor = flags & FLAG_DESCRIPTOR != 0;
        let actual = if method == DEFLATED {
            let end = if has_descriptor && header.compressed_size == 0 {
                self.central_start
            } else {
                add(data_start, header.compressed_size as usize)?
            };
            if end > self.central_start {
                return Err(opaque());
            }
            inspect_deflate(&bytes[data_start..end], name)?
        } else if has_descriptor && header.compressed_size == 0 {
            let descriptors = stored_descriptors.get_or_insert_with(|| self.stored_descriptors());
            return self.stored_orphan_end(data_start, header, name, descriptors);
        } else {
            let end = add(data_start, header.compressed_size as usize)?;
            if end > self.central_start || header.compressed_size as usize > MAX_READ {
                return Err(opaque());
            }
            Sizes {
                crc: crc32(&bytes[data_start..end]),
                compressed_size: header.compressed_size,
                size: header.compressed_size,
            }
        };
        if !header.matches(actual, has_descriptor) {
            return Err(opaque());
        }
        let data_end = add(data_start, actual.compressed_size as usize)?;
        if has_descriptor {
            descriptor_end(bytes, data_end, self.central_start, actual, name)
        } else {
            Ok(data_end)
        }
    }

    // Stored streams carry no end marker. Index descriptors immediately before
    // recognizable local headers or the central directory in a single pass.
    // Their size fields identify the possible payload start. The index avoids scanning
    // the rest of the archive separately for every obsolete streaming member.
    fn stored_descriptors(&self) -> StoredDescriptors {
        let mut descriptors = HashMap::new();
        let local_signature = LOCAL.to_le_bytes();
        let directory = self.central_start..self.central_start;
        // Active records are already bounded and cannot contain obsolete
        // records. Only scan gaps, retaining the active start after each gap.
        let boundaries = self
            .local_order
            .iter()
            .map(|&index| &self.entries[index].local_record)
            .chain(std::iter::once(&directory))
            .scan(0, |previous_end, record| {
                let gap = *previous_end..record.start;
                *previous_end = record.end;
                Some(gap)
            })
            .flat_map(|gap| {
                let start = gap.start;
                let end = gap.end;
                self.original[gap]
                    .windows(4)
                    .enumerate()
                    .filter(move |(_, bytes)| *bytes == local_signature)
                    .map(move |(offset, _)| (start + offset, start, end))
                    .chain(std::iter::once((end, start, end)))
            });
        for (boundary, gap_start, gap_end) in boundaries {
            for signed in [false, true] {
                let length = if signed {
                    SIGNED_DESCRIPTOR_LEN
                } else {
                    DESCRIPTOR_LEN
                };
                let Some(data_end) = boundary.checked_sub(length) else {
                    continue;
                };
                if signed && u32_at(&self.original, data_end).ok() != Some(DESCRIPTOR) {
                    continue;
                }
                // Both forms end with these fields, entirely before the boundary.
                let sizes = Sizes::read(&self.original, boundary - DESCRIPTOR_LEN)
                    .expect("descriptor fields in bounds");
                if sizes.size != sizes.compressed_size || sizes.size as usize > MAX_READ {
                    continue;
                }
                let Some(data_start) = data_end.checked_sub(sizes.size as usize) else {
                    continue;
                };
                // An obsolete stream must lie entirely within one gap. Known
                // active records cannot be payload or descriptors of another
                // member, even when their bytes imitate a valid header.
                if data_start < gap_start {
                    continue;
                }
                // A signature inside stored payload is not a record boundary.
                // Only index descriptors whose following header could be an
                // active member or an obsolete member compaction understands.
                if boundary != self.central_start
                    && !self.recognizable_local_header(boundary, gap_end)
                {
                    continue;
                }
                let descriptor = StoredDescriptor {
                    data_end,
                    end: boundary,
                    sizes,
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

    fn recognizable_local_header(&self, start: usize, gap_end: usize) -> bool {
        if self
            .local_order
            .binary_search_by_key(&start, |&index| self.entries[index].local_record.start)
            .is_ok()
        {
            // Active records were already validated, including opaque methods
            // and flags that compaction retains without decoding.
            return true;
        }
        let Ok(Some(header)) = local_header(&self.original, start, gap_end) else {
            return false;
        };
        // Reject unsupported obsolete records before scanning their names.
        // Fake headers can otherwise use nonzero method/flag bytes to make
        // many overlapping long names pass the NUL check below.
        if header.flags & (ENCRYPTION_FLAGS | FLAG_PATCHED) != 0
            || !matches!(header.method, STORED | DEFLATED)
        {
            return false;
        }
        // Accepted member names cannot contain NUL. Check before decoding and
        // hashing: false headers in stored data can overlap thousands of long
        // candidate names, with an early NUL in each following header.
        if header.raw_name.contains(&0) {
            return false;
        }
        let Ok(name) = member_name(header.raw_name, header.flags) else {
            return false;
        };
        let Some(&index) = self.index.get(name) else {
            return false;
        };
        let entry = &self.entries[index];
        header.method == entry.method && check_extra(header.extra, name).is_ok()
    }

    fn stored_orphan_end(
        &self,
        data_start: usize,
        header: Sizes,
        name: &str,
        descriptors: &StoredDescriptors,
    ) -> Result<usize> {
        let Some(candidate) = descriptors.get(&data_start) else {
            return Err(unsupported(
                "cannot compact an unrecognized stored ZIP data descriptor",
            ));
        };
        let Some(descriptor) = candidate else {
            return Err(unsupported(
                "cannot compact an ambiguous stored ZIP data descriptor",
            ));
        };
        // The header's compressed size is zero here; its CRC and size must be
        // zero or agree with the descriptor.
        if !header.matches(descriptor.sizes, true)
            || crc32(&self.original[data_start..descriptor.data_end]) != descriptor.sizes.crc
        {
            return Err(invalid(format!(
                "obsolete ZIP size or checksum mismatch in {name}"
            )));
        }
        let end = descriptor_end(
            &self.original,
            descriptor.data_end,
            self.central_start,
            descriptor.sizes,
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

fn inspect_deflate(compressed: &[u8], name: &str) -> Result<Sizes> {
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
            return Err(over_read_limit(format_args!("obsolete ZIP member {name}")));
        }
        let produced = (decoder.total_out() - previous_out) as usize;
        crc.update(&buffer[..produced]);
        if status == Status::StreamEnd {
            let compressed_size = zip32(decoder.total_in() as usize, "compressed ZIP member size")?;
            return Ok(Sizes {
                crc: crc.finalize(),
                compressed_size,
                size: decoder.total_out() as u32,
            });
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
        named_fixture(b"part.xml", 0, descriptor)
    }

    fn named_fixture(name: &[u8], flags: u16, descriptor: bool) -> Vec<u8> {
        stored_fixture(name, b"<part/>", flags, descriptor.then_some(true))
    }

    fn stored_fixture(name: &[u8], data: &[u8], flags: u16, descriptor: Option<bool>) -> Vec<u8> {
        let crc = crc32(data);
        let flags = flags
            | if descriptor.is_some() {
                FLAG_DESCRIPTOR
            } else {
                0
            };
        let mut bytes = vec![0; 30];
        set_u32(&mut bytes, 0, LOCAL);
        set_u16(&mut bytes, 4, 20);
        set_u16(&mut bytes, 6, flags);
        if descriptor.is_none() {
            set_u32(&mut bytes, 14, crc);
            set_u32(&mut bytes, 18, data.len() as u32);
            set_u32(&mut bytes, 22, data.len() as u32);
        }
        set_u16(&mut bytes, 26, name.len() as u16);
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(data);
        if let Some(signed) = descriptor {
            if signed {
                bytes.extend_from_slice(&DESCRIPTOR.to_le_bytes());
            }
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
        set_u32(&mut expected_local, 14, after.sizes.crc);
        set_u32(&mut expected_local, 18, after.sizes.compressed_size);
        set_u32(&mut expected_local, 22, after.sizes.size);
        assert_eq!(updated.original[after.local_header.clone()], expected_local);
        assert_eq!(after.local_record.end, after.data.end);

        let mut expected_central = archive.original[before.central.clone()].to_vec();
        set_u16(&mut expected_central, 8, after.flags);
        set_u32(&mut expected_central, 16, after.sizes.crc);
        set_u32(&mut expected_central, 20, after.sizes.compressed_size);
        set_u32(&mut expected_central, 24, after.sizes.size);
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
        bytes.extend_from_slice(&entry.sizes.crc.to_le_bytes());
        bytes.extend_from_slice(&entry.sizes.compressed_size.to_le_bytes());
        bytes.extend_from_slice(&entry.sizes.size.to_le_bytes());
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
    fn many_replacements_follow_local_order_and_retain_central_order_and_metadata() {
        let archive = Archive::new(combine_members((0..96).map(|index| {
            let name: [u8; 8] = format!("part{index:04}").into_bytes().try_into().unwrap();
            decorate_fixture(&name, index % 3 != 0)
        })))
        .unwrap();
        let mut reordered = archive.original[..archive.central_start].to_vec();
        for entry in archive.entries.iter().rev() {
            reordered.extend_from_slice(&archive.original[entry.central.clone()]);
        }
        reordered.extend_from_slice(&archive.original[archive.end..]);
        let archive = Archive::new(reordered).unwrap();
        let changes: BTreeMap<_, _> = (0..96)
            .step_by(3)
            .map(|index| {
                (
                    format!("part{index:04}"),
                    format!("replacement {index} {}", "x".repeat(index)).into_bytes(),
                )
            })
            .collect();
        let obsolete = Archive::new(append(&archive, &changes)).unwrap();
        for (archive, has_obsolete_records) in [(&archive, false), (&obsolete, true)] {
            let preserved = archive.write(&changes).unwrap();
            let mut compacted = Vec::new();
            archive.compact_to(&changes, &mut compacted).unwrap();
            if has_obsolete_records {
                assert!(compacted.len() < preserved.len());
            } else {
                assert_eq!(compacted, preserved);
            }
            for output in [preserved, compacted] {
                let reopened = Archive::new(output).unwrap();
                assert_eq!(
                    reopened
                        .entries
                        .iter()
                        .map(|entry| &entry.name)
                        .collect::<Vec<_>>(),
                    archive
                        .entries
                        .iter()
                        .map(|entry| &entry.name)
                        .collect::<Vec<_>>()
                );
                assert_eq!(
                    reopened
                        .local_order
                        .iter()
                        .map(|&index| &reopened.entries[index].name)
                        .collect::<Vec<_>>(),
                    archive
                        .local_order
                        .iter()
                        .map(|&index| &archive.entries[index].name)
                        .collect::<Vec<_>>()
                );
                for before in &archive.entries {
                    let after = &reopened.entries[reopened.index[&before.name]];
                    if let Some(data) = changes.get(&before.name) {
                        assert_eq!(reopened.read(&before.name).unwrap(), *data);
                    } else {
                        assert_eq!(
                            reopened.original[after.local_record.clone()],
                            archive.original[before.local_record.clone()]
                        );
                        let before = &archive.original[before.central.clone()];
                        let after = &reopened.original[after.central.clone()];
                        assert_eq!(after[..42], before[..42]);
                        assert_eq!(after[46..], before[46..]);
                    }
                }
                assert!(reopened.original.ends_with(b"comment"));
            }
        }
    }

    #[test]
    fn borrowed_changes_keep_validation_order_and_fail_before_output() {
        struct BorrowedChanges<'a>(&'a [(&'a str, &'a [u8])]);

        impl PartChanges for BorrowedChanges<'_> {
            fn parts(&self) -> impl Iterator<Item = (&str, &[u8])> {
                self.0.iter().copied()
            }
        }

        let original = fixture(false);
        let archive = Archive::new(original.clone()).unwrap();
        let empty = BorrowedChanges(&[]);
        assert_eq!(archive.write(&empty).unwrap(), original);
        let valid = BorrowedChanges(&[("part.xml", b"borrowed replacement")]);
        assert_eq!(
            Archive::new(archive.write(&valid).unwrap())
                .unwrap()
                .read("part.xml")
                .unwrap(),
            b"borrowed replacement"
        );
        let invalid = BorrowedChanges(&[("z-missing.xml", b""), ("a-missing.xml", b"")]);
        for compact in [false, true] {
            let mut output = Vec::new();
            let result = if compact {
                archive.compact_to(&invalid, &mut output)
            } else {
                archive.write_to(&invalid, &mut output)
            };
            assert!(matches!(
                result,
                Err(Error::InvalidWorkbook(message)) if message.ends_with("z-missing.xml")
            ));
            assert!(output.is_empty());
        }
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
    fn stored_orphan_payload_signatures_do_not_create_false_descriptor_ambiguity() {
        let archive = Archive::new(fixture(true)).unwrap();
        let entry = &archive.entries[0];
        let mut payload = b"prefix".to_vec();
        // These bytes mimic a descriptor pointing to the real payload start,
        // but the following signature belongs to payload, not a local header.
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&6u32.to_le_bytes());
        payload.extend_from_slice(&6u32.to_le_bytes());
        payload.extend_from_slice(&LOCAL.to_le_bytes());
        payload.extend_from_slice(b"ordinary payload after the signature");
        // A complete apparent header also stays payload when its name only
        // matches an active UTF-8 name under the wrong encoding flag.
        let false_size = payload.len() as u32;
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&false_size.to_le_bytes());
        payload.extend_from_slice(&false_size.to_le_bytes());
        let false_header = named_fixture("é.xml".as_bytes(), 0, false);
        payload.extend_from_slice(&false_header[..LOCAL_HEADER_LEN + "é.xml".len()]);
        payload.extend_from_slice(b"ordinary payload after the encoded name");
        let checksum = crc32(&payload);
        let mut bytes = archive.original[entry.local_header.clone()].to_vec();
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&DESCRIPTOR.to_le_bytes());
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        let central_start = bytes.len();
        let mut central = archive.original[entry.central.clone()].to_vec();
        set_u32(&mut central, 16, checksum);
        set_u32(&mut central, 20, payload.len() as u32);
        set_u32(&mut central, 24, payload.len() as u32);
        bytes.extend_from_slice(&central);
        let mut ending = archive.original[archive.end..].to_vec();
        set_u32(&mut ending, 16, central_start as u32);
        bytes.extend_from_slice(&ending);
        let archive = Archive::new(combine_members([
            bytes,
            named_fixture("é.xml".as_bytes(), FLAG_UTF8, false),
            decorate_fixture(b"keep.xml", true),
        ]))
        .unwrap();
        assert_eq!(archive.read("part.xml").unwrap(), payload);
        let opaque_record = archive.original[archive.entries[2].local_record.clone()].to_vec();
        let changes = BTreeMap::from([("part.xml".into(), b"replacement".to_vec())]);
        let archive = Archive::new(append(&archive, &changes)).unwrap();
        let mut compacted = Vec::new();
        archive
            .compact_to(&BTreeMap::new(), &mut compacted)
            .unwrap();
        let archive = Archive::new(compacted).unwrap();
        assert_eq!(archive.read("part.xml").unwrap(), b"replacement");
        assert_eq!(
            archive.original[archive.entries[2].local_record.clone()],
            opaque_record
        );
    }

    #[test]
    fn active_payloads_cannot_supply_obsolete_stored_descriptors() {
        for signed in [false, true] {
            let original = stored_fixture(b"part.xml", b"old", 0, Some(signed));
            let old = Archive::new(original.clone()).unwrap();
            let old_data_start = old.entries[0].data.start;
            let keep_data_start = old.central_start + LOCAL_HEADER_LEN + b"keep.xml".len();
            for tail in [false, true] {
                let mut payload = b"opaque payload".to_vec();
                let false_size = (keep_data_start + payload.len() - old_data_start) as u32;
                payload.extend_from_slice(&0u32.to_le_bytes());
                payload.extend_from_slice(&false_size.to_le_bytes());
                payload.extend_from_slice(&false_size.to_le_bytes());
                if !tail {
                    // A complete apparent record inside active payload.
                    let false_header = fixture(false);
                    payload
                        .extend_from_slice(&false_header[..LOCAL_HEADER_LEN + b"part.xml".len()]);
                    payload.extend_from_slice(b"remaining payload");
                }
                for opaque in [false, true] {
                    let mut keep = stored_fixture(b"keep.xml", &payload, 0, None);
                    if opaque {
                        let central = Archive::new(keep.clone()).unwrap().central_start;
                        set_u16(&mut keep, 8, 99);
                        set_u16(&mut keep, central + 10, 99);
                    }
                    let archive = Archive::new(combine(original.clone(), keep)).unwrap();
                    let changes = BTreeMap::from([("part.xml".into(), b"new".to_vec())]);
                    let appended = Archive::new(append(&archive, &changes)).unwrap();
                    // In the tail case, the next real active record starts
                    // immediately after the apparent descriptor.
                    let keep = &appended.entries[1];
                    let expected = appended.original[keep.local_record.clone()].to_vec();
                    let mut compacted = Vec::new();
                    appended
                        .compact_to(&BTreeMap::new(), &mut compacted)
                        .unwrap();
                    assert_eq!(compacted.len(), appended.original.len() - old.central_start);
                    let compacted = Archive::new(compacted).unwrap();
                    assert_eq!(compacted.read("part.xml").unwrap(), b"new");
                    assert_eq!(
                        compacted.original[compacted.entries[1].local_record.clone()],
                        expected
                    );
                    if !opaque {
                        assert_eq!(compacted.read("keep.xml").unwrap(), payload);
                    }
                }
            }
        }
    }

    #[test]
    fn ambiguous_descriptors_inside_obsolete_payload_still_refuse_compaction() {
        for signed in [false, true] {
            let mut payload = b"prefix".to_vec();
            payload.extend_from_slice(&0u32.to_le_bytes());
            payload.extend_from_slice(&6u32.to_le_bytes());
            payload.extend_from_slice(&6u32.to_le_bytes());
            let false_header = fixture(false);
            payload.extend_from_slice(&false_header[..LOCAL_HEADER_LEN + b"part.xml".len()]);
            payload.extend_from_slice(b"remaining obsolete payload");
            let archive =
                Archive::new(stored_fixture(b"part.xml", &payload, 0, Some(signed))).unwrap();
            let appended = Archive::new(append(
                &archive,
                &BTreeMap::from([("part.xml".into(), b"new".to_vec())]),
            ))
            .unwrap();
            assert_eq!(appended.read("part.xml").unwrap(), b"new");
            let mut output = Vec::new();
            assert!(matches!(
                appended.compact_to(&BTreeMap::new(), &mut output),
                Err(Error::Unsupported(message)) if message.contains("ambiguous stored ZIP")
            ));
            assert!(output.is_empty());
        }
    }

    #[test]
    fn obsolete_header_candidates_cannot_extend_into_active_records() {
        for signed in [false, true] {
            let replacement = b"new";
            let active_length = LOCAL_HEADER_LEN + b"part.xml".len() + replacement.len();
            let descriptor_length = if signed {
                SIGNED_DESCRIPTOR_LEN
            } else {
                DESCRIPTOR_LEN
            };
            let mut payload = b"prefix".to_vec();
            payload.extend_from_slice(&0u32.to_le_bytes());
            payload.extend_from_slice(&6u32.to_le_bytes());
            payload.extend_from_slice(&6u32.to_le_bytes());
            let mut false_header = fixture(false)[..LOCAL_HEADER_LEN + b"part.xml".len()].to_vec();
            // One unknown extra field would swallow the real descriptor and
            // the next active member if parsed past this obsolete gap.
            let extra_length = descriptor_length + active_length;
            set_u16(&mut false_header, 28, (4 + extra_length) as u16);
            payload.extend_from_slice(&false_header);
            payload.extend_from_slice(&0xbeefu16.to_le_bytes());
            payload.extend_from_slice(&(extra_length as u16).to_le_bytes());
            let archive =
                Archive::new(stored_fixture(b"part.xml", &payload, 0, Some(signed))).unwrap();
            let changes = BTreeMap::from([("part.xml".into(), replacement.to_vec())]);
            let appended = Archive::new(append(&archive, &changes)).unwrap();
            let mut compacted = Vec::new();
            appended
                .compact_to(&BTreeMap::new(), &mut compacted)
                .unwrap();
            assert_eq!(compacted, archive.write(&changes).unwrap());
        }
    }

    #[test]
    fn stored_orphan_payload_with_many_false_long_names_compacts_exactly() {
        for (fill, repeats) in [(0, 4096), (1, 18 * 1024 * 1024 / 64)] {
            let original = fixture(true);
            let archive = Archive::new(original.clone()).unwrap();
            let entry = &archive.entries[0];
            // Each apparent header has a 16 KiB name spanning hundreds of later
            // headers. Zero-filled headers have impossible NUL-containing names;
            // nonzero-filled headers have unsupported flags and methods, with
            // preceding descriptor sizes of 0x01010101 (just over 16 MiB).
            let mut pattern = [fill; 64];
            set_u32(&mut pattern, 0, LOCAL);
            if fill == 0 {
                set_u16(&mut pattern, 4, 20);
            }
            set_u16(&mut pattern, 26, 0x4040);
            let payload = pattern.repeat(repeats);
            let mut bytes = archive.original[entry.local_header.clone()].to_vec();
            bytes.extend_from_slice(&payload);
            bytes.extend_from_slice(&DESCRIPTOR.to_le_bytes());
            bytes.extend_from_slice(&crc32(&payload).to_le_bytes());
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            let active_start = bytes.len();
            bytes.extend_from_slice(&archive.original[..archive.central_start]);
            let central_start = bytes.len();
            let mut central = archive.original[entry.central.clone()].to_vec();
            set_u32(&mut central, 42, active_start as u32);
            bytes.extend_from_slice(&central);
            let mut ending = archive.original[archive.end..].to_vec();
            set_u32(&mut ending, 16, central_start as u32);
            bytes.extend_from_slice(&ending);
            let archive = Archive::new(bytes).unwrap();
            let mut compacted = Vec::new();
            archive
                .compact_to(&BTreeMap::new(), &mut compacted)
                .unwrap();
            assert_eq!(compacted, original);
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

    #[test]
    fn rejects_end_of_directory_signatures_after_the_selected_record() {
        let original = fixture(false);
        let end = original.len() - 29;
        // Signatures before the selected record, such as in stored member
        // data, are not candidates for readers that scan back from the end.
        let archive = Archive::new(original.clone()).unwrap();
        let embedded = [b"stored ".as_slice(), &END.to_le_bytes(), b" data"].concat();
        let changes = BTreeMap::from([("part.xml".into(), embedded.clone())]);
        let stored = Archive::new(archive.write(&changes).unwrap()).unwrap();
        assert_eq!(stored.read("part.xml").unwrap(), embedded);
        // Backward-scanning readers select the last signature even when its
        // comment does not reach EOF: here an EOCD with an empty comment plus
        // one trailing byte, or a signature starting in the length field.
        let mut hidden = original[end..end + 22].to_vec();
        set_u16(&mut hidden, 20, 0);
        hidden.push(b'!');
        let mut overlapping = vec![0x05, 0x06];
        overlapping.resize(0x4b50, 0);
        for comment in [hidden, overlapping] {
            let mut bytes = original[..end + 20].to_vec();
            bytes.extend_from_slice(&(comment.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&comment);
            assert!(matches!(
                Archive::new(bytes),
                Err(Error::InvalidWorkbook(message))
                    if message == "ambiguous ZIP end-of-directory records"
            ));
        }
    }

    #[test]
    fn rejects_earlier_end_records_that_forward_scanning_readers_could_select() {
        // libzip before 1.11 keeps the first end record in its tail window
        // whose directory it can read. Here a stale member and its directory
        // precede the live member, and an end record for that directory hides
        // in the comment of the live directory's last entry.
        let stale = Archive::new(fixture(false)).unwrap();
        let live = Archive::new(stored_fixture(b"part.xml", b"<live/>", 0, None)).unwrap();
        let with_decoy = |edit: fn(&mut [u8])| {
            let mut bytes = stale.original[..stale.end].to_vec();
            let live_start = bytes.len();
            bytes.extend_from_slice(&live.original[..live.central_start]);
            let central_start = bytes.len();
            let mut decoy = stale.original[stale.end..stale.end + END_LEN].to_vec();
            set_u16(&mut decoy, 20, 0);
            edit(&mut decoy);
            let mut central = live.original[live.central_start..live.end].to_vec();
            set_u16(&mut central, 32, END_LEN as u16);
            set_u32(&mut central, 42, live_start as u32);
            central.extend_from_slice(&decoy);
            bytes.extend_from_slice(&central);
            let mut ending = live.original[live.end..].to_vec();
            set_u32(&mut ending, 12, central.len() as u32);
            set_u32(&mut ending, 16, central_start as u32);
            bytes.extend_from_slice(&ending);
            bytes
        };
        assert!(matches!(
            Archive::new(with_decoy(|_| {})),
            Err(Error::InvalidWorkbook(message))
                if message == "ambiguous ZIP end-of-directory records"
        ));
        // That reader cannot select these records, so they stay comment bytes.
        let unreadable: [fn(&mut [u8]); 7] = [
            // Another disk, or a directory spanning disks.
            |decoy| set_u16(decoy, 4, 1),
            |decoy| set_u16(decoy, 6, 1),
            |decoy| set_u16(decoy, 8, 0),
            // An empty directory never spans more local data than another.
            |decoy| {
                set_u16(decoy, 8, 0);
                set_u16(decoy, 10, 0);
            },
            // A comment running past EOF.
            |decoy| set_u16(decoy, 20, 30),
            // A directory overlapping the record, or not starting with a
            // central-directory header.
            |decoy| set_u32(decoy, 12, 1000),
            |decoy| set_u32(decoy, 16, 1),
        ];
        for edit in unreadable {
            let archive = Archive::new(with_decoy(edit)).unwrap();
            assert_eq!(archive.read("part.xml").unwrap(), b"<live/>");
        }
    }

    #[test]
    fn stored_embedded_archives_do_not_make_the_end_record_ambiguous() {
        // The embedded archive's own end record lies in the tail window, but
        // its directory offset is relative to that archive and finds no
        // central-directory header in this one.
        let inner = fixture(false);
        let original = combine(fixture(false), stored_fixture(b"embedded", &inner, 0, None));
        let archive = Archive::new(original).unwrap();
        let inner_end = archive.entries[1].data.start + inner.len() - 29;
        assert_eq!(u32_at(&archive.original, inner_end).unwrap(), END);
        assert!(archive.original.len() - inner_end < END_LEN + u16::MAX as usize);
        assert_eq!(archive.read("embedded").unwrap(), inner);
        let changes = BTreeMap::from([("part.xml".into(), b"updated".to_vec())]);
        let updated = Archive::new(archive.write(&changes).unwrap()).unwrap();
        assert_eq!(updated.read("part.xml").unwrap(), b"updated");
        assert_eq!(updated.read("embedded").unwrap(), inner);
    }

    #[test]
    fn rejects_member_names_with_backslashes() {
        // Some readers treat '\' as '/', so for them the second member here
        // would alias the first, and the last of the two would win.
        let mut aliased = combine(
            named_fixture(b"xl/part.xml", 0, false),
            named_fixture(b"xl_part.xml", 0, false),
        );
        // `combine` opens its parts, so respell the second name afterwards.
        let second = &Archive::new(aliased.clone()).unwrap().entries[1];
        for name_start in [second.local_header.start + 30, second.central.start + 46] {
            aliased[name_start + 2] = b'\\';
        }
        let message = "ZIP member name contains a backslash: xl\\part.xml";
        for bytes in [aliased, named_fixture(b"xl\\part.xml", 0, false)] {
            assert!(matches!(
                Archive::new(bytes),
                Err(Error::InvalidWorkbook(actual)) if actual == message
            ));
        }
    }

    /// An end record for `entries` members whose directory of `size` bytes
    /// starts at `offset`, after bytes standing in for members and directory.
    fn end_record(entries: u16, size: u32, offset: u32, comment: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; 64 + END_LEN];
        set_u32(&mut bytes, 64, END);
        set_u16(&mut bytes, 64 + 8, entries);
        set_u16(&mut bytes, 64 + 10, entries);
        set_u32(&mut bytes, 64 + 12, size);
        set_u32(&mut bytes, 64 + 16, offset);
        set_u16(&mut bytes, 64 + 20, comment.len() as u16);
        bytes.extend_from_slice(comment);
        bytes
    }

    #[test]
    fn end_record_fields_may_hold_the_signature_without_room_for_another_record() {
        // A directory at offset 0x06054b50, which a ~101 MB archive can have,
        // puts the signature at record offset 16. So do 0x4b50 entries and a
        // directory size ending in 0x0605 at offset 10.
        for (entries, size, offset, at) in [(1, 46, END, 16), (0x4b50, 0x0001_0605, 64, 10)] {
            // Readers scanning back from EOF only select a signature followed by
            // a complete record, which needs at least `at` comment bytes.
            for length in [0, 1, at - 1] {
                let bytes = end_record(entries, size, offset, &vec![b'c'; length]);
                assert_eq!(end_of_central_directory(&bytes).unwrap(), 64, "{length}");
            }
            for length in [at, at + 1, u16::MAX as usize] {
                let bytes = end_record(entries, size, offset, &vec![b'c'; length]);
                assert!(matches!(
                    end_of_central_directory(&bytes),
                    Err(Error::InvalidWorkbook(message))
                        if message == "ambiguous ZIP end-of-directory records"
                ));
            }
        }
    }

    /// `bytes` with its archive comment replaced by `comment`.
    fn with_comment(mut bytes: Vec<u8>, comment: &[u8]) -> Vec<u8> {
        let end = Archive::new(bytes.clone()).unwrap().end;
        bytes.truncate(end + END_LEN);
        set_u16(&mut bytes, end + 20, comment.len() as u16);
        bytes.extend_from_slice(comment);
        bytes
    }

    #[test]
    fn saves_refuse_directory_offsets_that_would_make_the_end_record_ambiguous() {
        let comment = |first: &[u8], length: usize| {
            let mut comment = first.to_vec();
            comment.resize(length, b'c');
            comment
        };
        // Local records would have to fill hundreds of megabytes to move the
        // directory this far, so emit the directory from these positions.
        for (offset, comment, refused) in [
            (END, Vec::new(), false),
            // Python would select the signature at offset 16 and, finding no
            // complete record after it, fail to open the archive.
            (END, comment(b"", 15), true),
            // Other readers would select it too, and misread the archive.
            (END, comment(b"", 16), true),
            // The offset's top byte, the comment length 0x054b and the
            // comment's first byte together spell a signature at offset 19.
            (0x5000_0000, comment(&[6], 0x054b), true),
            (0x5000_0000, comment(&[7], 0x054b), false),
        ] {
            let archive = Archive::new(with_comment(fixture(false), &comment)).unwrap();
            let mut output = Vec::new();
            let mut position = offset as usize;
            let result = archive.emit_directory(&mut output, &mut position, &[0], &[None]);
            if refused {
                assert!(matches!(
                    result,
                    Err(Error::Unsupported(message)) if message.contains("ambiguous")
                ));
                assert!(output.is_empty());
            } else {
                result.unwrap();
                // Readers select the end record that was written.
                let end = output.len() - END_LEN - comment.len();
                assert_eq!(end_of_central_directory(&output).unwrap(), end);
                assert!(!shadows_end_record(&output[end..]));
                assert_eq!(u32_at(&output, end + 16).unwrap(), offset);
            }
        }

        // A signature Python cannot use in the input's own record is kept: here
        // 0x4b50 entries and a 0x0605-byte directory, with a 5-byte comment.
        // (The entry count is set after validation; the writer copies it.)
        let mut padding = 0xcafe_u16.to_le_bytes().to_vec();
        padding.extend_from_slice(&1483_u16.to_le_bytes());
        padding.resize(4 + 1483, 0);
        let input = with_comment(extra_fixture(&[], &padding), b"12345");
        let mut archive = Archive::new(input).unwrap();
        assert_eq!(archive.end - archive.central_start, 0x0605);
        set_u16(&mut archive.original, archive.end + 8, 0x4b50);
        set_u16(&mut archive.original, archive.end + 10, 0x4b50);
        assert!(shadows_end_record(&archive.original[archive.end..]));
        let mut output = Vec::new();
        archive
            .emit_directory(&mut output, &mut 0x100, &[0], &[None])
            .unwrap();
        let end = output.len() - END_LEN - 5;
        assert!(shadows_end_record(&output[end..]));
        assert_eq!(u32_at(&output, end + 16).unwrap(), 0x100);
    }

    #[test]
    fn python_selects_any_later_signature_once_there_is_a_comment() {
        // Without a comment Python reads the last 22 bytes as the record.
        let bytes = end_record(1, 46, END, b"");
        assert!(!shadows_end_record(&bytes[64..]));
        // With one, it selects the last signature, here at offset 16.
        for length in [1, 15, 16, 100] {
            let bytes = end_record(1, 46, END, &vec![b'c'; length]);
            assert!(shadows_end_record(&bytes[64..]), "{length}");
            assert_eq!(hides_end_record(&bytes[64..]), length >= 16, "{length}");
        }
        let bytes = end_record(1, 46, 64, &[b'c'; 100]);
        assert!(!shadows_end_record(&bytes[64..]));
    }

    fn unicode_path(raw_name: &[u8], name: &[u8], version: u8) -> Vec<u8> {
        let mut field = 0x7075u16.to_le_bytes().to_vec();
        field.extend_from_slice(&(5 + name.len() as u16).to_le_bytes());
        field.push(version);
        field.extend_from_slice(&crc32(raw_name).to_le_bytes());
        field.extend_from_slice(name);
        field
    }

    fn extra_fixture(local_extra: &[u8], central_extra: &[u8]) -> Vec<u8> {
        let archive = Archive::new(fixture(false)).unwrap();
        let entry = &archive.entries[0];
        let mut bytes = archive.original[entry.local_header.clone()].to_vec();
        set_u16(&mut bytes, 28, local_extra.len() as u16);
        bytes.extend_from_slice(local_extra);
        bytes.extend_from_slice(&archive.original[entry.data.clone()]);
        let central_start = bytes.len();
        let mut central = archive.original[entry.central.clone()].to_vec();
        set_u16(&mut central, 30, central_extra.len() as u16);
        central.extend_from_slice(central_extra);
        bytes.extend_from_slice(&central);
        let mut ending = archive.original[archive.end..].to_vec();
        set_u32(&mut ending, 12, central.len() as u32);
        set_u32(&mut ending, 16, central_start as u32);
        bytes.extend_from_slice(&ending);
        bytes
    }

    #[test]
    fn rejects_unicode_path_extra_fields_that_rename_members() {
        let name = b"part.xml";
        // Readers differ in the versions they apply: Info-ZIP takes version 0
        // in the central directory, and libarchive any version locally.
        for version in [0, 1, 2] {
            let renamed = unicode_path(name, b"evil.xml", version);
            let same = unicode_path(name, name, version);
            let late = [same.clone(), renamed.clone()].concat();
            for (local, central) in [
                (&Vec::new(), &renamed),
                (&renamed, &Vec::new()),
                (&same, &renamed),
                (&renamed, &same),
                (&renamed, &renamed),
                (&same, &late),
                (&late, &same),
            ] {
                assert!(matches!(
                    Archive::new(extra_fixture(local, central)),
                    Err(Error::InvalidWorkbook(message)) if message.contains("Unicode path")
                ));
            }
        }
        // Readers ignore fields whose CRC does not match the raw name or that
        // are too short to hold one, so those cannot rename a member for them.
        let same = unicode_path(name, name, 1);
        let stale = unicode_path(b"part.bin", b"evil.xml", 1);
        let short = [0x75, 0x70, 4, 0, 1, 0, 0, 0].to_vec();
        let both = [same.clone(), stale.clone(), short.clone()].concat();
        let changes = BTreeMap::from([("part.xml".into(), b"updated".to_vec())]);
        let versions = [0, 2].map(|version| unicode_path(name, name, version));
        for extra in [same, stale, short, both].into_iter().chain(versions) {
            let original = extra_fixture(&extra, &extra);
            let archive = Archive::new(original.clone()).unwrap();
            assert_eq!(archive.read("part.xml").unwrap(), b"<part/>");
            assert_eq!(archive.write(&BTreeMap::new()).unwrap(), original);
            let mut compacted = Vec::new();
            archive.compact_to(&changes, &mut compacted).unwrap();
            assert_eq!(compacted, archive.write(&changes).unwrap());
            let updated = Archive::new(compacted).unwrap();
            assert_eq!(updated.read("part.xml").unwrap(), b"updated");
        }
    }

    #[test]
    fn repeated_unicode_path_fields_with_long_names_still_check_the_last_field() {
        let name = "a".repeat(u16::MAX as usize);
        let stale = unicode_path(b"old-name", b"", 1);
        assert_ne!(crc32(b"old-name"), crc32(name.as_bytes()));
        let mut extra = stale.repeat(7000);
        assert!(check_extra(&extra, &name).is_ok());
        extra.extend_from_slice(&unicode_path(name.as_bytes(), b"renamed", 1));
        assert!(extra.len() <= u16::MAX as usize);
        assert!(matches!(
            check_extra(&extra, &name),
            Err(Error::InvalidWorkbook(message)) if message.contains("Unicode path")
        ));
    }

    #[test]
    fn compaction_refuses_obsolete_records_renamed_by_unicode_path_fields() {
        for version in [0, 1, 2] {
            let same = unicode_path(b"part.xml", b"part.xml", version);
            let archive = Archive::new(extra_fixture(&same, &same)).unwrap();
            let changes = BTreeMap::from([("part.xml".into(), b"updated".to_vec())]);
            let appended = append(&archive, &changes);
            let mut compacted = Vec::new();
            Archive::new(appended.clone())
                .unwrap()
                .compact_to(&BTreeMap::new(), &mut compacted)
                .unwrap();
            assert_eq!(
                Archive::new(compacted).unwrap().read("part.xml").unwrap(),
                b"updated"
            );
            // The obsolete record at offset 0 is opaque to normal saves, but
            // compaction must parse it and so applies the same name rule.
            let mut renamed = appended;
            let unicode_name = 30 + b"part.xml".len() + 9;
            renamed[unicode_name..unicode_name + 8].copy_from_slice(b"evil.xml");
            let archive = Archive::new(renamed.clone()).unwrap();
            assert_eq!(archive.write(&BTreeMap::new()).unwrap(), renamed);
            let mut output = Vec::new();
            assert!(matches!(
                archive.compact_to(&BTreeMap::new(), &mut output),
                Err(Error::InvalidWorkbook(message)) if message.contains("Unicode path")
            ));
            assert!(output.is_empty());
        }
    }

    #[test]
    fn stream_reads_stop_one_byte_past_the_limit() {
        let input = b"0123456789";
        assert_eq!(read_at_most(&input[..], Vec::new(), 10).unwrap(), input);
        let too_long = |result: Result<Vec<u8>>| {
            matches!(
                result,
                Err(Error::Unsupported(message))
                    if message == "input exceeds the largest possible ZIP32 archive"
            )
        };
        assert!(too_long(read_at_most(&input[..], Vec::new(), 9)));
        // An endless stream is not read beyond the limit.
        assert!(too_long(read_at_most(
            std::io::repeat(0),
            Vec::new(),
            1 << 16
        )));
        // Reserved capacity in the buffer does not count as input.
        let reserved = Vec::with_capacity(64);
        assert_eq!(read_at_most(&input[..], reserved, 10).unwrap(), input);
        assert_eq!(MAX_ARCHIVE_LEN, 2 * u64::from(u32::MAX) + 22 + 65535);
    }

    #[test]
    fn rejects_ole_compound_files_as_unsupported() {
        let mut bytes = vec![0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
        bytes.resize(4096, 0);
        assert!(matches!(
            Archive::new(bytes),
            Err(Error::Unsupported(message)) if message.contains("OLE compound files")
        ));
        assert!(matches!(
            Archive::new(vec![0; 4096]),
            Err(Error::InvalidWorkbook(_))
        ));
    }

    #[test]
    fn streaming_flushes_the_writer_on_every_path() {
        struct Flushes {
            bytes: Vec<u8>,
            flushes: usize,
        }
        impl Write for Flushes {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }
        let archive = Archive::new(fixture(true)).unwrap();
        let changes = BTreeMap::from([("part.xml".into(), b"changed".to_vec())]);
        for changes in [&BTreeMap::new(), &changes] {
            for compact in [false, true] {
                let mut writer = Flushes {
                    bytes: Vec::new(),
                    flushes: 0,
                };
                if compact {
                    archive.compact_to(changes, &mut writer).unwrap();
                } else {
                    archive.write_to(changes, &mut writer).unwrap();
                }
                assert_eq!(writer.flushes, 1);
                assert_eq!(writer.bytes, archive.write(changes).unwrap());
            }
        }
    }

    #[test]
    fn rejects_encryption_flags_and_accepts_informational_flags() {
        let central = Archive::new(fixture(false)).unwrap().entries[0]
            .central
            .start;
        let flagged = |flags: u16| {
            let mut bytes = fixture(false);
            set_u16(&mut bytes, 6, flags);
            set_u16(&mut bytes, central + 8, flags);
            Archive::new(bytes)
        };
        // Traditional encryption, strong encryption, and masked local headers.
        for flags in [0x0001, 0x0040, 0x2000, 0x0002 | 0x0040] {
            assert!(matches!(
                flagged(flags),
                Err(Error::Unsupported(message))
                    if message == "encrypted ZIP workbooks are not supported"
            ));
        }
        // Compression options and the UTF-8 name flag do not affect reading.
        for flags in [0x0002, 0x0004, 0x0800] {
            assert_eq!(
                flagged(flags).unwrap().read("part.xml").unwrap(),
                b"<part/>"
            );
        }
    }

    #[test]
    fn member_name_encoding_is_preserved_when_reading_editing_and_compacting() {
        for (name, flags) in [
            ("part.xml", 0),
            ("part.xml", FLAG_UTF8),
            ("é.xml", FLAG_UTF8),
        ] {
            for descriptor in [false, true] {
                let original = named_fixture(name.as_bytes(), flags, descriptor);
                let archive = Archive::new(original.clone()).unwrap();
                assert_eq!(archive.read(name).unwrap(), b"<part/>");
                assert_eq!(archive.write(&BTreeMap::new()).unwrap(), original);
                let changes = BTreeMap::from([(name.into(), b"updated".to_vec())]);
                let appended = Archive::new(append(&archive, &changes)).unwrap();
                let mut compacted = Vec::new();
                appended
                    .compact_to(&BTreeMap::new(), &mut compacted)
                    .unwrap();
                assert_eq!(compacted, archive.write(&changes).unwrap());
                let updated = Archive::new(compacted).unwrap();
                assert_eq!(updated.read(name).unwrap(), b"updated");
                assert_eq!(updated.entries[0].flags & FLAG_UTF8, flags);
            }
        }
        // An explicitly encoded active member remains opaque when its method
        // is unsupported; compaction preserves it without decoding its data.
        let mut bytes = named_fixture("é.xml".as_bytes(), FLAG_UTF8, false);
        let central = Archive::new(bytes.clone()).unwrap().central_start;
        set_u16(&mut bytes, 8, 99);
        set_u16(&mut bytes, central + 10, 99);
        let archive = Archive::new(bytes.clone()).unwrap();
        let mut compacted = Vec::new();
        archive
            .compact_to(&BTreeMap::new(), &mut compacted)
            .unwrap();
        assert_eq!(compacted, bytes);
    }

    #[test]
    fn rejects_ambiguous_name_encodings_and_conflicting_encoding_flags() {
        for descriptor in [false, true] {
            for name in ["é.xml".as_bytes(), b"\xe9.xml"] {
                assert!(matches!(
                    Archive::new(named_fixture(name, 0, descriptor)),
                    Err(Error::Unsupported(message))
                        if message == "non-ASCII ZIP member names require the UTF-8 flag"
                ));
            }
            assert!(matches!(
                Archive::new(named_fixture(b"\xe9.xml", FLAG_UTF8, descriptor)),
                Err(Error::Unsupported(message)) if message == "ZIP member names must be UTF-8"
            ));
            let original = named_fixture("é.xml".as_bytes(), FLAG_UTF8, descriptor);
            let central = Archive::new(original.clone()).unwrap().central_start;
            for flag_at in [6, central + 8] {
                let mut bytes = original.clone();
                let flags = u16_at(&bytes, flag_at).unwrap() & !FLAG_UTF8;
                set_u16(&mut bytes, flag_at, flags);
                let result = Archive::new(bytes);
                if flag_at == 6 {
                    assert!(matches!(
                        result,
                        Err(Error::InvalidWorkbook(message)) if message.contains("conflicting ZIP headers")
                    ));
                } else {
                    assert!(matches!(
                        result,
                        Err(Error::Unsupported(message))
                            if message == "non-ASCII ZIP member names require the UTF-8 flag"
                    ));
                }
            }
        }
    }

    #[test]
    fn compaction_refuses_obsolete_names_without_an_explicit_encoding() {
        let name = "é.xml";
        for descriptor in [false, true] {
            let archive =
                Archive::new(named_fixture(name.as_bytes(), FLAG_UTF8, descriptor)).unwrap();
            let changes = BTreeMap::from([(name.into(), b"updated".to_vec())]);
            let mut bytes = append(&archive, &changes);
            // Only the obsolete record loses its encoding flag. Normal saves
            // preserve it, while compaction must understand it before removal.
            let flags = u16_at(&bytes, 6).unwrap() & !FLAG_UTF8;
            set_u16(&mut bytes, 6, flags);
            let archive = Archive::new(bytes.clone()).unwrap();
            assert_eq!(archive.read(name).unwrap(), b"updated");
            assert_eq!(archive.write(&BTreeMap::new()).unwrap(), bytes);
            let mut compacted = Vec::new();
            assert!(matches!(
                archive.compact_to(&BTreeMap::new(), &mut compacted),
                Err(Error::Unsupported(message))
                    if message == "non-ASCII ZIP member names require the UTF-8 flag"
            ));
            assert!(compacted.is_empty());
        }
    }

    #[test]
    fn rejects_declared_sizes_over_the_read_limit_before_decompressing() {
        // A single final stored DEFLATE block holding the original payload.
        let mut compressed = vec![1, 7, 0, 0xf8, 0xff];
        compressed.extend_from_slice(b"<part/>");
        let valid = deflate_fixture(&compressed);
        let archive = Archive::new(valid.clone()).unwrap();
        assert_eq!(archive.read("part.xml").unwrap(), b"<part/>");
        let central = archive.entries[0].central.start;
        let mut oversized = valid;
        set_u32(&mut oversized, 22, MAX_READ as u32 + 1);
        set_u32(&mut oversized, central + 24, MAX_READ as u32 + 1);
        assert!(matches!(
            Archive::new(oversized).unwrap().read("part.xml"),
            Err(Error::Unsupported(message))
                if message == "ZIP member part.xml exceeds the 64 MiB read limit"
        ));
    }

    #[test]
    fn rejects_overlapping_local_records() {
        let keep = Archive::new(decorate_fixture(b"keep.xml", false)).unwrap();
        let record = keep.original[keep.entries[0].local_record.clone()].to_vec();
        let separate = combine(fixture(false), decorate_fixture(b"keep.xml", false));
        assert_eq!(Archive::new(separate).unwrap().entries.len(), 2);
        // A stored payload that is itself the other member's complete record.
        let part = Archive::new(fixture(false)).unwrap();
        let changes = BTreeMap::from([("part.xml".into(), record)]);
        let part = Archive::new(part.write(&changes).unwrap()).unwrap();
        let mut bytes = part.original[..part.central_start].to_vec();
        let central_start = bytes.len();
        bytes.extend_from_slice(&part.original[part.entries[0].central.clone()]);
        let mut central = keep.original[keep.entries[0].central.clone()].to_vec();
        set_u32(&mut central, 42, part.entries[0].data.start as u32);
        bytes.extend_from_slice(&central);
        let mut ending = part.original[part.end..].to_vec();
        set_u16(&mut ending, 8, 2);
        set_u16(&mut ending, 10, 2);
        set_u32(&mut ending, 12, (bytes.len() - central_start) as u32);
        set_u32(&mut ending, 16, central_start as u32);
        bytes.extend_from_slice(&ending);
        assert!(matches!(
            Archive::new(bytes),
            Err(Error::InvalidWorkbook(message)) if message == "overlapping ZIP local records"
        ));
    }
}
