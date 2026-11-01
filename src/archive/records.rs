//! ZIP32 field decoding and agreement between local and central headers.
use super::{
    CENTRAL, CENTRAL_HEADER_LEN, DEFLATED, DESCRIPTOR, DESCRIPTOR_LEN, ENCRYPTION_FLAGS,
    EXTRA_UNICODE_PATH, EXTRA_ZIP64, Entry, FLAG_DESCRIPTOR, FLAG_PATCHED, FLAG_UTF8, LOCAL,
    LOCAL_HEADER_LEN, SIGNED_DESCRIPTOR_LEN, STORED, ZIP64_LOCATOR, ZIP64_LOCATOR_LEN, crc32,
    invalid, unsupported, zip64,
};
use crate::Result;
use std::ops::Range;

/// A member's CRC-32 and sizes, which local headers, central headers and data
/// descriptors all store as consecutive fields in this order.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Sizes {
    pub(super) crc: u32,
    pub(super) compressed_size: u32,
    pub(super) size: u32,
}

impl Sizes {
    pub(super) fn read(bytes: &[u8], offset: usize) -> Result<Self> {
        Ok(Self {
            crc: u32_at(bytes, offset)?,
            compressed_size: u32_at(bytes, add(offset, 4)?)?,
            size: u32_at(bytes, add(offset, 8)?)?,
        })
    }

    pub(super) fn write(self, bytes: &mut [u8], offset: usize) {
        set_u32(bytes, offset, self.crc);
        set_u32(bytes, offset + 4, self.compressed_size);
        set_u32(bytes, offset + 8, self.size);
    }

    /// Whether header fields agree with the actual values. A header followed
    /// by a data descriptor may leave any field zero instead.
    pub(super) fn matches(self, actual: Self, data_descriptor: DataDescriptor) -> bool {
        let zero_allowed = data_descriptor == DataDescriptor::Present;
        [
            (self.crc, actual.crc),
            (self.compressed_size, actual.compressed_size),
            (self.size, actual.size),
        ]
        .into_iter()
        .all(|(header, actual)| header == actual || (zero_allowed && header == 0))
    }
}

/// Whether a data descriptor follows a member's data, as its general-purpose
/// flags announce. Its headers may then leave the CRC-32 and sizes zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DataDescriptor {
    Absent,
    Present,
}

impl DataDescriptor {
    pub(super) fn of(flags: u16) -> Self {
        if flags & FLAG_DESCRIPTOR == 0 {
            Self::Absent
        } else {
            Self::Present
        }
    }
}

/// A central-directory header whose variable fields lie within the directory.
pub(super) struct CentralHeader<'bytes> {
    pub(super) flags: u16,
    pub(super) method: u16,
    pub(super) sizes: Sizes,
    pub(super) name: &'bytes str,
    pub(super) extra: &'bytes [u8],
    pub(super) local_start: usize,
    pub(super) record: Range<usize>,
}

/// A local header whose fixed fields, name and extra field end by a limit.
pub(super) struct LocalHeader<'bytes> {
    pub(super) flags: u16,
    pub(super) method: u16,
    pub(super) sizes: Sizes,
    pub(super) raw_name: &'bytes [u8],
    pub(super) extra: &'bytes [u8],
    pub(super) data_start: usize,
}

/// How member data is decoded and replacements are encoded. Patched data and
/// other compression methods stay opaque: preserved, but never read or replaced.
#[derive(Clone, Copy)]
pub(super) enum Codec {
    Stored,
    Deflated,
}

impl Codec {
    pub(super) fn of(flags: u16, method: u16) -> Option<Self> {
        if flags & FLAG_PATCHED != 0 {
            return None;
        }
        match method {
            STORED => Some(Self::Stored),
            DEFLATED => Some(Self::Deflated),
            _ => None,
        }
    }
}

pub(super) fn field<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk::<N>)
        .copied()
        .ok_or_else(|| invalid("truncated ZIP record"))
}

pub(super) fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    field(bytes, offset).map(u16::from_le_bytes)
}

pub(super) fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    field(bytes, offset).map(u32::from_le_bytes)
}

pub(super) fn add(a: usize, b: usize) -> Result<usize> {
    a.checked_add(b)
        .ok_or_else(|| invalid("ZIP offset overflow"))
}

pub(super) fn set_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

pub(super) fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

pub(super) fn zip32(value: usize, description: &str) -> Result<u32> {
    let value = u32::try_from(value)
        .map_err(|_| unsupported(format!("{description} exceeds ZIP32 limits")))?;
    if value == u32::MAX {
        return Err(unsupported(format!("{description} requires ZIP64")));
    }
    Ok(value)
}

/// Reject extra fields that change how other readers interpret a member: ZIP64
/// sizes and Unicode paths that rename it.
pub(super) fn check_extra(mut bytes: &[u8], name: &str) -> Result<()> {
    // A long name can have thousands of stale Unicode path fields. Hash it
    // only once, rather than once per field, to keep validation linear.
    let mut name_crc: Option<u32> = None;
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

/// Return the central directory's start and entry count from the EOCD.
pub(super) fn central_directory(bytes: &[u8], end: usize) -> Result<(usize, u16)> {
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

pub(super) fn member_name(raw_name: &[u8], flags: u16) -> Result<&str> {
    // Without bit 11, ZIP readers can decode these bytes as CP437 instead of
    // UTF-8, even when the bytes happen to be valid UTF-8.
    if flags & FLAG_UTF8 == 0 && !raw_name.is_ascii() {
        return Err(unsupported(
            "non-ASCII ZIP member names require the UTF-8 flag",
        ));
    }
    str::from_utf8(raw_name).map_err(|_| unsupported("ZIP member names must be UTF-8"))
}

pub(super) fn central_header(
    bytes: &[u8],
    position: usize,
    end: usize,
) -> Result<CentralHeader<'_>> {
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
pub(super) fn local_header(
    bytes: &[u8],
    start: usize,
    limit: usize,
) -> Result<Option<LocalHeader<'_>>> {
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

pub(super) fn descriptor_end(
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

impl Entry {
    pub(super) fn codec(&self) -> Result<Codec> {
        Codec::of(self.flags, self.method).ok_or_else(|| {
            let name = &self.name;
            if self.flags & FLAG_PATCHED != 0 {
                unsupported(format!("patched ZIP member data in {name}"))
            } else {
                let method = self.method;
                unsupported(format!("ZIP compression method {method} in {name}"))
            }
        })
    }

    /// Validate a member's local record against its central header.
    pub(super) fn try_new(
        bytes: &[u8],
        central: &CentralHeader<'_>,
        central_start: usize,
    ) -> Result<Self> {
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
        let data_descriptor = DataDescriptor::of(central.flags);
        if !local.sizes.matches(sizes, data_descriptor) {
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
        let local_end = match data_descriptor {
            DataDescriptor::Present => descriptor_end(bytes, data_end, central_start, sizes, name)?,
            DataDescriptor::Absent => data_end,
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
