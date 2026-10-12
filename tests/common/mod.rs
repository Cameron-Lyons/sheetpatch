//! Shared helpers for integration tests and the benchmark.
//!
//! The ZIP writer here is deliberately independent of the crate's own writer:
//! package discovery and preservation are checked against archives the crate
//! did not produce. Each test crate uses a subset of these helpers.
#![allow(dead_code)]

use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};

const LOCAL_HEADER: u32 = 0x0403_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const END_OF_CENTRAL_DIRECTORY: u32 = 0x0605_4b50;

/// CRC-32 as stored in ZIP records.
pub fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

/// How [`write_zip`] records each entry.
#[derive(Clone, Copy, Debug)]
pub struct ZipLayout<'a> {
    /// Deflate level (method 8); `None` stores every entry (method 0).
    pub deflate_level: Option<u32>,
    /// Entries larger than this many bytes are stored even when deflating.
    pub store_larger_than: usize,
    /// When set, every entry records DOS timestamps, a Unix producer and file
    /// mode, and this central-directory comment. Otherwise those fields are zero.
    pub entry_comment: Option<&'a [u8]>,
    /// End-of-central-directory comment.
    pub archive_comment: &'a [u8],
}

impl ZipLayout<'static> {
    /// Stored entries with zeroed optional metadata and no comments.
    pub const STORED: Self = Self {
        deflate_level: None,
        store_larger_than: usize::MAX,
        entry_comment: None,
        archive_comment: b"",
    };

    /// Deflated entries with zeroed optional metadata and no comments.
    pub const fn deflated(level: u32) -> Self {
        Self {
            deflate_level: Some(level),
            ..Self::STORED
        }
    }
}

/// Writes a minimal single-disk ZIP archive of `entries`, in order.
pub fn write_zip(entries: &[(&str, &[u8])], layout: &ZipLayout<'_>) -> Vec<u8> {
    let (made_by, time, date, mode, entry_comment) = match layout.entry_comment {
        Some(comment) => (0x0314, 0x4220, 0x5d47, 0o100640 << 16, comment),
        None => (20, 0, 0, 0, &[][..]),
    };
    let mut output = Vec::new();
    let mut central = Vec::new();
    for &(name, bytes) in entries {
        let level = layout
            .deflate_level
            .filter(|_| bytes.len() <= layout.store_larger_than);
        let (method, compressed) = match level {
            Some(level) => {
                let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(level));
                encoder.write_all(bytes).unwrap();
                (8, Cow::Owned(encoder.finish().unwrap()))
            }
            None => (0, Cow::Borrowed(bytes)),
        };
        let crc = crc32(bytes);
        let offset = zip32(output.len());
        put_u32(&mut output, LOCAL_HEADER);
        for value in [20, 0, method, time, date] {
            put_u16(&mut output, value);
        }
        for value in [crc, zip32(compressed.len()), zip32(bytes.len())] {
            put_u32(&mut output, value);
        }
        put_u16(&mut output, zip16(name.len()));
        put_u16(&mut output, 0); // extra field length
        output.extend_from_slice(name.as_bytes());
        output.extend_from_slice(&compressed);

        put_u32(&mut central, CENTRAL_HEADER);
        for value in [made_by, 20, 0, method, time, date] {
            put_u16(&mut central, value);
        }
        for value in [crc, zip32(compressed.len()), zip32(bytes.len())] {
            put_u32(&mut central, value);
        }
        // Name, extra field, and comment lengths; starting disk; internal attributes.
        for value in [zip16(name.len()), 0, zip16(entry_comment.len()), 0, 0] {
            put_u16(&mut central, value);
        }
        put_u32(&mut central, mode);
        put_u32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
        central.extend_from_slice(entry_comment);
    }
    let central_offset = zip32(output.len());
    output.extend_from_slice(&central);
    put_u32(&mut output, END_OF_CENTRAL_DIRECTORY);
    let count = zip16(entries.len());
    for value in [0, 0, count, count] {
        put_u16(&mut output, value);
    }
    put_u32(&mut output, zip32(central.len()));
    put_u32(&mut output, central_offset);
    put_u16(&mut output, zip16(layout.archive_comment.len()));
    output.extend_from_slice(layout.archive_comment);
    output
}

/// Writes `entries` stored (method 0), without optional metadata or comments.
pub fn stored_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    write_zip(entries, &ZipLayout::STORED)
}

fn zip16(value: usize) -> u16 {
    u16::try_from(value).expect("test ZIP field exceeds 16 bits")
}

fn zip32(value: usize) -> u32 {
    u32::try_from(value).expect("test ZIP field exceeds 32 bits")
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

/// Little-endian `u16` at `offset`, widened for offset arithmetic.
pub fn u16_at(bytes: &[u8], offset: usize) -> usize {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()).into()
}

/// Little-endian `u32` at `offset`.
pub fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Offset of the end-of-central-directory record, whose comment must end the archive.
pub fn end_of_central_directory(bytes: &[u8]) -> usize {
    (0..=bytes.len().saturating_sub(22))
        .rev()
        .find(|&at| {
            u32_at(bytes, at) == END_OF_CENTRAL_DIRECTORY
                && at + 22 + u16_at(bytes, at + 20) == bytes.len()
        })
        .expect("missing end-of-central-directory record")
}

/// Offset of the first central-directory record.
pub fn central_directory_offset(bytes: &[u8]) -> usize {
    u32_at(bytes, end_of_central_directory(bytes) + 16) as usize
}

/// The archive comment.
pub fn archive_comment(bytes: &[u8]) -> &[u8] {
    let end = end_of_central_directory(bytes);
    &bytes[end + 22..]
}

/// One central-directory record and the local record it points to.
pub struct ZipEntry<'a> {
    pub name: String,
    pub method: u16,
    pub crc: u32,
    pub uncompressed_size: usize,
    /// Offset of the local record.
    pub local_offset: usize,
    /// Complete local record: header, name, extra field, and compressed bytes.
    pub local: &'a [u8],
    /// Compressed bytes only.
    pub compressed: &'a [u8],
    /// Complete central-directory record, including name, extra field, and comment.
    pub central: &'a [u8],
}

impl ZipEntry<'_> {
    /// Decompressed bytes, after checking their size and CRC-32.
    pub fn payload(&self) -> Vec<u8> {
        let output = match self.method {
            0 => self.compressed.to_vec(),
            8 => {
                let mut output = Vec::new();
                DeflateDecoder::new(self.compressed)
                    .read_to_end(&mut output)
                    .unwrap();
                output
            }
            method => panic!("unexpected compression method {method} in {}", self.name),
        };
        assert_eq!(
            output.len(),
            self.uncompressed_size,
            "uncompressed size: {}",
            self.name
        );
        assert_eq!(crc32(&output), self.crc, "CRC: {}", self.name);
        output
    }

    /// Central-directory bytes before the local-record offset.
    pub fn central_before_offset(&self) -> &[u8] {
        &self.central[..42]
    }

    /// Central-directory bytes after the local-record offset.
    pub fn central_after_offset(&self) -> &[u8] {
        &self.central[46..]
    }
}

/// Central-directory entries in directory order.
pub fn zip_entries(bytes: &[u8]) -> Vec<ZipEntry<'_>> {
    let end = end_of_central_directory(bytes);
    let mut at = u32_at(bytes, end + 16) as usize;
    let mut entries = Vec::new();
    for _ in 0..u16_at(bytes, end + 10) {
        assert_eq!(u32_at(bytes, at), CENTRAL_HEADER);
        let name_len = u16_at(bytes, at + 28);
        let central_len = 46 + name_len + u16_at(bytes, at + 30) + u16_at(bytes, at + 32);
        let compressed_size = u32_at(bytes, at + 20) as usize;
        let local = u32_at(bytes, at + 42) as usize;
        assert_eq!(u32_at(bytes, local), LOCAL_HEADER);
        let start = local + 30 + u16_at(bytes, local + 26) + u16_at(bytes, local + 28);
        entries.push(ZipEntry {
            name: String::from_utf8(bytes[at + 46..at + 46 + name_len].to_vec()).unwrap(),
            method: u16_at(bytes, at + 10) as u16,
            crc: u32_at(bytes, at + 16),
            uncompressed_size: u32_at(bytes, at + 24) as usize,
            local_offset: local,
            local: &bytes[local..start + compressed_size],
            compressed: &bytes[start..start + compressed_size],
            central: &bytes[at..at + central_len],
        });
        at += central_len;
    }
    entries
}

/// The entry named `name`.
pub fn zip_entry<'a>(bytes: &'a [u8], name: &str) -> ZipEntry<'a> {
    zip_entries(bytes)
        .into_iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("missing ZIP member {name}"))
}

/// Decompressed bytes of the entry named `name`.
pub fn payload(bytes: &[u8], name: &str) -> Vec<u8> {
    zip_entry(bytes, name).payload()
}

/// Every decompressed payload by name; member names must be unique.
pub fn payloads(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut parts = BTreeMap::new();
    for entry in zip_entries(bytes) {
        let payload = entry.payload();
        assert!(
            parts.insert(entry.name, payload).is_none(),
            "duplicate ZIP member"
        );
    }
    parts
}

/// A uniquely named temporary directory, removed with its contents on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sheetpatch-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: impl AsRef<Path>) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const DATA_DESCRIPTOR: u32 = 0x0807_4b50;
const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;

/// Rewrites a contiguous archive with local records in directory order, such
/// as [`write_zip`] output, the way a streaming writer records sizes. Entries
/// for which `descriptor(name)` returns `Some(signed)` set general-purpose flag
/// bit 3, zero the CRC-32 and sizes in their local header, and follow their
/// compressed bytes with a data descriptor, with its optional signature when
/// `signed`. Other entries and the end record's comment are copied unchanged.
pub fn with_data_descriptors(archive: &[u8], descriptor: impl Fn(&str) -> Option<bool>) -> Vec<u8> {
    let end = end_of_central_directory(archive);
    let mut output = Vec::with_capacity(archive.len() + 16 * u16_at(archive, end + 10));
    let mut central = Vec::new();
    let mut cursor = 0;
    for entry in zip_entries(archive) {
        assert_eq!(
            entry.local_offset, cursor,
            "local records must be contiguous and in directory order"
        );
        cursor += entry.local.len();
        let offset = zip32(output.len());
        let header_len = entry.local.len() - entry.compressed.len();
        let mut header = entry.local[..header_len].to_vec();
        let mut record = entry.central.to_vec();
        let signed = descriptor(&entry.name);
        if signed.is_some() {
            let flags = u16_at(&header, 6) as u16 | FLAG_DATA_DESCRIPTOR;
            header[6..8].copy_from_slice(&flags.to_le_bytes());
            record[8..10].copy_from_slice(&flags.to_le_bytes());
            // CRC-32, compressed size, and uncompressed size follow the data.
            header[14..26].fill(0);
        }
        output.extend_from_slice(&header);
        output.extend_from_slice(entry.compressed);
        if let Some(signed) = signed {
            if signed {
                put_u32(&mut output, DATA_DESCRIPTOR);
            }
            output.extend_from_slice(&entry.central[16..28]);
        }
        record[42..46].copy_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(&record);
    }
    assert_eq!(
        cursor,
        central_directory_offset(archive),
        "unexpected ZIP gap"
    );
    let central_offset = zip32(output.len());
    output.extend_from_slice(&central);
    let mut end_record = archive[end..].to_vec();
    end_record[12..16].copy_from_slice(&zip32(central.len()).to_le_bytes());
    end_record[16..20].copy_from_slice(&central_offset.to_le_bytes());
    output.extend_from_slice(&end_record);
    output
}

/// Length of the data descriptor following `entry`'s compressed bytes in
/// `bytes`: 16 with its signature, 12 without, or 0 when flag bit 3 is clear.
pub fn data_descriptor_len(bytes: &[u8], entry: &ZipEntry<'_>) -> usize {
    if u16_at(entry.central, 8) as u16 & FLAG_DATA_DESCRIPTOR == 0 {
        return 0;
    }
    if u32_at(bytes, entry.local_offset + entry.local.len()) == DATA_DESCRIPTOR {
        16
    } else {
        12
    }
}
