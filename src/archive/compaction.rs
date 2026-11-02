//! Prove obsolete records complete before dropping them during compaction.
use super::compression::inspect_deflate;
use super::records::{
    Codec, DataDescriptor, LocalHeader, Sizes, add, check_extra, descriptor_end, local_header,
    member_name, u32_at,
};
use super::{
    Archive, DESCRIPTOR, DESCRIPTOR_LEN, ENCRYPTION_FLAGS, LOCAL, MAX_PART_SIZE,
    SIGNED_DESCRIPTOR_LEN, crc32, not_obsolete, unsupported,
};
use crate::Result;
use std::collections::HashMap;
use std::iter;
use std::ops::Range;

/// The local header of an obsolete record that compaction understands, with
/// the codec of its data and its member name.
struct ObsoleteRecord<'bytes> {
    header: LocalHeader<'bytes>,
    codec: Codec,
    name: &'bytes str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct StoredDescriptor {
    data_end: usize,
    end: usize,
    sizes: Sizes,
}

/// `None` means more than one structurally plausible descriptor points to that
/// payload start. Compaction refuses this ambiguity rather than choosing one.
type StoredDescriptors = HashMap<usize, Option<StoredDescriptor>>;

impl Archive {
    /// Require every byte before the central directory to belong to an active
    /// record or a recognizable obsolete one, so compaction drops nothing opaque.
    pub(super) fn check_compactable(&self) -> Result<()> {
        let mut position = 0;
        let mut stored_descriptors: Option<StoredDescriptors> = None;
        for &index in &self.local_order {
            let entry = &self.entries[index];
            while position < entry.local_record.start {
                position =
                    self.orphan_end(position, entry.local_record.start, &mut stored_descriptors)?;
            }
            if position != entry.local_record.start {
                return Err(unsupported(
                    "cannot compact overlapping or unrecognized ZIP local records",
                ));
            }
            position = entry.local_record.end;
        }
        while position < self.central_start {
            position = self.orphan_end(position, self.central_start, &mut stored_descriptors)?;
        }
        if position != self.central_start {
            return Err(unsupported(
                "cannot compact unrecognized data before the ZIP directory",
            ));
        }
        Ok(())
    }

    /// Parse the local header at `start` as an obsolete record that compaction
    /// understands, ending by `gap_end`, the end of the gap holding it.
    /// `Ok(None)` means it is not one; name encodings and extra fields that
    /// `Archive::new` would refuse in an active member are errors.
    fn obsolete_header(&self, start: usize, gap_end: usize) -> Result<Option<ObsoleteRecord<'_>>> {
        let Some(header) = local_header(&self.original, start, gap_end)? else {
            return Ok(None);
        };
        // Reject unsupported obsolete records before scanning their names.
        // Fake headers can otherwise use nonzero method/flag bytes to make
        // many overlapping long names pass the NUL check below.
        if header.flags & ENCRYPTION_FLAGS != 0 {
            return Ok(None);
        }
        let Some(codec) = Codec::of(header.flags, header.method) else {
            return Ok(None);
        };
        if header.sizes.compressed_size == u32::MAX || header.sizes.size == u32::MAX {
            return Ok(None);
        }
        // Archive::new refuses member names containing NUL. Check before
        // decoding and hashing: false headers in stored data can overlap
        // thousands of long candidate names, with an early NUL in each
        // following header.
        if header.raw_name.contains(&0) {
            return Ok(None);
        }
        let name = member_name(header.raw_name, header.flags)?;
        let Some(&index) = self.index.get(name) else {
            return Ok(None);
        };
        if self.entries[index].method != header.method {
            return Ok(None);
        }
        check_extra(header.extra, name)?;
        Ok(Some(ObsoleteRecord {
            header,
            codec,
            name,
        }))
    }

    fn orphan_end(
        &self,
        start: usize,
        gap_end: usize,
        stored_descriptors: &mut Option<StoredDescriptors>,
    ) -> Result<usize> {
        let opaque = || {
            unsupported("cannot compact opaque ZIP prefix, gaps, or unrecognized obsolete records")
        };
        let bytes = &self.original;
        let Some(ObsoleteRecord {
            header:
                LocalHeader {
                    flags,
                    sizes: header,
                    data_start,
                    ..
                },
            codec,
            name,
        }) = self.obsolete_header(start, gap_end)?
        else {
            return Err(opaque());
        };
        // An obsolete record ends within its gap, which bounds every search.
        let data_descriptor = DataDescriptor::of(flags);
        let streaming = data_descriptor == DataDescriptor::Present && header.compressed_size == 0;
        let actual = match codec {
            Codec::Deflated => {
                let end = if streaming {
                    gap_end
                } else {
                    add(data_start, header.compressed_size as usize)?
                };
                if end > gap_end {
                    return Err(opaque());
                }
                inspect_deflate(&bytes[data_start..end], name)?
            }
            Codec::Stored if streaming => {
                let descriptors =
                    stored_descriptors.get_or_insert_with(|| self.stored_descriptors());
                return self.stored_orphan_end(data_start, gap_end, header, name, descriptors);
            }
            Codec::Stored => {
                let end = add(data_start, header.compressed_size as usize)?;
                if end > gap_end || header.compressed_size as usize > MAX_PART_SIZE {
                    return Err(opaque());
                }
                Sizes {
                    crc: crc32(&bytes[data_start..end]),
                    compressed_size: header.compressed_size,
                    size: header.compressed_size,
                }
            }
        };
        if !header.matches(actual, data_descriptor) {
            return Err(opaque());
        }
        let data_end = add(data_start, actual.compressed_size as usize)?;
        match data_descriptor {
            DataDescriptor::Present => {
                descriptor_end(bytes, data_end, gap_end, actual, name).map_err(not_obsolete)
            }
            DataDescriptor::Absent => Ok(data_end),
        }
    }

    /// Stored streams carry no end marker. Index descriptors immediately before
    /// recognizable local headers or the central directory in a single pass.
    /// Their size fields identify the possible payload start. The index avoids
    /// scanning the rest of the archive separately for every obsolete streaming
    /// member.
    fn stored_descriptors(&self) -> StoredDescriptors {
        let mut descriptors: StoredDescriptors = HashMap::new();
        let local_signature = LOCAL.to_le_bytes();
        let directory = self.central_start..self.central_start;
        let active_records = self
            .local_order
            .iter()
            .map(|&index| &self.entries[index].local_record)
            .chain(iter::once(&directory));
        // Active records are already bounded and cannot contain obsolete
        // records. Only scan gaps, retaining the active start after each gap.
        let mut gap_start = 0;
        for record in active_records {
            let gap_end = record.start;
            let signatures = self.original[gap_start..gap_end]
                .windows(4)
                .enumerate()
                .filter(|(_, bytes)| *bytes == local_signature)
                .map(|(offset, _)| gap_start + offset);
            let boundaries = signatures.chain(iter::once(gap_end));
            for boundary in boundaries {
                self.index_stored_descriptors(&mut descriptors, boundary, gap_start..gap_end);
            }
            gap_start = record.end;
        }
        descriptors
    }

    /// Add to `descriptors` the stored descriptors of either form that end at
    /// `boundary`, a local signature in `gap` or the end of it.
    fn index_stored_descriptors(
        &self,
        descriptors: &mut StoredDescriptors,
        boundary: usize,
        gap: Range<usize>,
    ) {
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
                .expect("boundaries lie within the archive, after a whole descriptor");
            if sizes.size != sizes.compressed_size || sizes.size as usize > MAX_PART_SIZE {
                continue;
            }
            let Some(data_start) = data_end.checked_sub(sizes.size as usize) else {
                continue;
            };
            // An obsolete stream must lie entirely within one gap. Known
            // active records cannot be payload or descriptors of another
            // member, even when their bytes imitate a valid header.
            if data_start < gap.start {
                continue;
            }
            // A signature inside stored payload is not a record boundary.
            // Only index descriptors whose following header could be an
            // active member or an obsolete member compaction understands.
            if boundary != self.central_start && !self.recognizable_local_header(boundary, gap.end)
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
        matches!(self.obsolete_header(start, gap_end), Ok(Some(_)))
    }

    fn stored_orphan_end(
        &self,
        data_start: usize,
        gap_end: usize,
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
        if !header.matches(descriptor.sizes, DataDescriptor::Present)
            || crc32(&self.original[data_start..descriptor.data_end]) != descriptor.sizes.crc
        {
            return Err(unsupported(format!(
                "cannot compact obsolete ZIP record with a size or checksum mismatch in {name}"
            )));
        }
        let end = descriptor_end(
            &self.original,
            descriptor.data_end,
            gap_end,
            descriptor.sizes,
            name,
        )
        .map_err(not_obsolete)?;
        if end != descriptor.end {
            return Err(unsupported(format!(
                "cannot compact conflicting obsolete ZIP descriptor in {name}"
            )));
        }
        Ok(end)
    }
}
