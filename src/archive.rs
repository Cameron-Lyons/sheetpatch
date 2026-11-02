//! A ZIP32 container that retains every untouched record verbatim.
//!
//! Changed members replace their original local records; untouched records and
//! opaque gaps are copied verbatim. This avoids decoding unrelated parts and
//! introducing unreferenced local records that some spreadsheet readers reject.

mod compaction;
mod compression;
mod records;
#[cfg(test)]
mod tests;
mod validation;
mod writer;

use crate::{Error, Result};
use flate2::{Decompress, FlushDecompress, Status};
use records::{Codec, Sizes, central_directory, central_header, check_extra};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Display;
use std::io::{self, Read};
use std::num::NonZeroUsize;
use std::ops::Range;
use validation::end_of_central_directory;

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;
const DESCRIPTOR: u32 = 0x0807_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
/// Legacy .xls files and password-encrypted OOXML packages are not ZIP archives
/// but OLE compound files.
const OLE_SIGNATURE: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];

const LOCAL_HEADER_LEN: usize = 30;
const CENTRAL_HEADER_LEN: usize = 46;
const END_LEN: usize = 22;
const ZIP64_LOCATOR_LEN: usize = 20;
const DESCRIPTOR_LEN: usize = 12;
const SIGNED_DESCRIPTOR_LEN: usize = 16;
/// The bytes before EOF that the end-record rules examine: an end record with
/// the longest comment, and a ZIP64 locator before it. This is also the tail
/// window in which libzip before 1.11 looks for end records.
const END_TAIL_LEN: usize = ZIP64_LOCATOR_LEN + END_LEN + u16::MAX as usize;

const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_DESCRIPTOR: u16 = 0x0008;
const FLAG_PATCHED: u16 = 0x0020;
const FLAG_STRONG_ENCRYPTION: u16 = 0x0040;
const FLAG_UTF8: u16 = 0x0800;
const FLAG_MASKED_HEADERS: u16 = 0x2000;
// Patched data is not encryption: active patched members stay opaque, while
// compaction refuses to discard obsolete ones (both through `Codec::of`).
const ENCRYPTION_FLAGS: u16 = FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTION | FLAG_MASKED_HEADERS;

const STORED: u16 = 0;
const DEFLATED: u16 = 8;

const EXTRA_ZIP64: u16 = 0x0001;
const EXTRA_UNICODE_PATH: u16 = 0x7075;

pub(crate) const MAX_PART_SIZE: usize = 64 * 1024 * 1024;
/// The length of the largest possible ZIP32 archive: the central directory's
/// offset and size are each below `u32::MAX`, and only a 22-byte end record
/// and a comment of at most 64 KiB follow it.
const MAX_ARCHIVE_LEN: u64 = 2 * u32::MAX as u64 + END_LEN as u64 + u16::MAX as u64;
pub(crate) trait PartChanges {
    /// Names are unique; iteration order is also the validation error order.
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
    compression_threads: Option<NonZeroUsize>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidWorkbook(message.into())
}

fn unsupported(message: impl Into<String>) -> Error {
    Error::Unsupported(message.into())
}

/// Compaction cannot prove a record in a gap obsolete. Readers never see the
/// gap, so this refuses the operation rather than reporting an invalid archive.
fn not_obsolete(error: Error) -> Error {
    match error {
        Error::InvalidWorkbook(message) => unsupported(format!("cannot compact: {message}")),
        error => error,
    }
}

fn zip64() -> Error {
    unsupported("ZIP64 workbooks are not supported")
}

fn over_read_limit(member: impl Display) -> Error {
    unsupported(format!(
        "{member} exceeds the {} MiB read limit",
        MAX_PART_SIZE >> 20
    ))
}

fn archive_too_large() -> Error {
    unsupported("input exceeds the largest possible ZIP32 archive")
}

/// Read everything `reader` yields, refusing input longer than any ZIP32
/// archive. A regular file's length, `file_len`, is refused before reading
/// when too large and otherwise reserved exactly; other input is refused once
/// that much has been read.
pub(crate) fn read_archive(reader: impl Read, file_len: Option<u64>) -> Result<Vec<u8>> {
    read_at_most(reader, file_len, MAX_ARCHIVE_LEN)
}

fn read_at_most(reader: impl Read, file_len: Option<u64>, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(len) = file_len {
        if len > limit {
            return Err(archive_too_large());
        }
        // Fall back to an unsized read on targets where the length does not
        // fit in memory; reading then reports the allocation failure.
        if let Ok(len) = usize::try_from(len) {
            bytes
                .try_reserve_exact(len)
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        }
    }
    // One byte past the limit shows that the input is too long.
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(archive_too_large());
    }
    Ok(bytes)
}

impl Archive {
    pub(crate) fn try_new(original: Vec<u8>) -> Result<Self> {
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
            if name.is_empty() {
                return Err(invalid("empty ZIP member name"));
            }
            // Python's zipfile truncates names at NUL.
            if name.contains('\0') {
                return Err(invalid(format!("ZIP member name contains NUL: {name}")));
            }
            // Some readers treat '\' as '/', so such a name can alias another
            // member for them. OPC part names cannot contain it.
            if name.contains('\\') {
                return Err(invalid(format!(
                    "ZIP member name contains a backslash: {name}"
                )));
            }
            if index.insert(name.to_owned(), entries.len()).is_some() {
                return Err(invalid(format!("duplicate ZIP member: {name}")));
            }
            check_extra(central.extra, name)?;
            // OPC part names compare without ASCII case, including the hex
            // digits in URI escapes. Retain exact ZIP names for preservation;
            // reject an ambiguous equivalent name only if that part is used.
            part_index
                .entry(name.to_ascii_lowercase())
                .and_modify(|entry| *entry = None)
                .or_insert(Some(entries.len()));
            let entry = Entry::try_new(&original, &central, central_start)?;
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
            compression_threads: None,
        })
    }

    pub(crate) fn compression_threads(&self) -> Option<NonZeroUsize> {
        self.compression_threads
    }

    pub(crate) fn set_compression_threads(&mut self, threads: Option<NonZeroUsize>) {
        self.compression_threads = threads;
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
        let codec = entry.codec()?;
        let sizes = entry.sizes;
        if sizes.size as usize > MAX_PART_SIZE {
            return Err(over_read_limit(format_args!("ZIP member {name}")));
        }
        let compressed = &self.original[entry.data.clone()];
        let output = match codec {
            Codec::Stored => compressed.to_vec(),
            Codec::Deflated => {
                let mut decoder = Decompress::new(/* zlib_header: */ false);
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
        };
        if output.len() != sizes.size as usize || crc32(&output) != sizes.crc {
            return Err(invalid(format!("ZIP size or checksum mismatch in {name}")));
        }
        Ok(output)
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}
