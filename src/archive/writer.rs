//! Emit replacements in physical record order and preserve untouched bytes.
use super::records::{Codec, Sizes, add, set_u16, set_u32, zip32};
use super::validation::{Probe, check_end_records, hides_end_record, shadows_end_record};
use super::{
    Archive, CENTRAL_HEADER_LEN, END_TAIL_LEN, Entry, FLAG_DESCRIPTOR, LOCAL_HEADER_LEN,
    MAX_PART_SIZE, PartChanges, compression, crc32, invalid, over_read_limit, unsupported,
};
use crate::Result;
use std::borrow::Cow;
use std::io::{self, Write};

/// A validated replacement for a member's data.
struct Change<'changes> {
    /// The member's index in `Archive::entries`.
    index: usize,
    codec: Codec,
    data: &'changes [u8],
}

struct ChangePlan<'changes> {
    /// Only the changed members' indexes, codecs and borrowed payloads,
    /// ordered by their physical local-record positions for a single merge
    /// during emission.
    changes: Vec<Change<'changes>>,
}

#[derive(Clone, Copy)]
enum RecordPolicy {
    PreserveGaps,
    Compact,
}

/// Where emitted local records end, and where they placed each member.
struct LocalLayout {
    /// The output position after the local records, where the directory starts.
    position: usize,
    /// Each member's new local-record offset.
    offsets: Vec<u32>,
    /// The new sizes of replaced members.
    replacements: Vec<Option<Sizes>>,
}

fn emit<W: Write>(writer: &mut W, position: &mut usize, bytes: &[u8]) -> Result<()> {
    let end = add(*position, bytes.len())?;
    writer.write_all(bytes)?;
    *position = end;
    Ok(())
}

/// Passes output on to a writer, keeping the last `len` bytes written.
pub(super) struct KeepTail<'writer, W> {
    writer: &'writer mut W,
    len: usize,
    tail: Vec<u8>,
}

impl<'writer, W: Write> KeepTail<'writer, W> {
    pub(super) fn new(writer: &'writer mut W, len: usize) -> Self {
        Self {
            writer,
            len,
            tail: Vec::new(),
        }
    }

    pub(super) fn into_tail(mut self) -> Vec<u8> {
        let excess = self.tail.len().saturating_sub(self.len);
        self.tail.drain(..excess);
        self.tail
    }
}

impl<W: Write> Write for KeepTail<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(bytes)?;
        let kept = &bytes[written.saturating_sub(self.len)..written];
        // Trim only once the buffer doubles, so each byte moves at most once.
        if self.tail.len() + kept.len() > 2 * self.len {
            self.tail.drain(..self.tail.len() + kept.len() - self.len);
        }
        self.tail.extend_from_slice(kept);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl Archive {
    pub(crate) fn write(&self, changes: &impl PartChanges) -> Result<Vec<u8>> {
        let mut output = Vec::with_capacity(self.original.len());
        self.write_to(changes, &mut output)?;
        Ok(output)
    }

    /// Stream local records in their original order, replacing changed members.
    ///
    /// Only one changed member's compressed payload is held in memory at a
    /// time. As with `Write::write_all`, a writer error may leave partial output,
    /// and so may the rare refusal of output whose last bytes would form another
    /// end record or a ZIP64 locator, which can only be detected once every local
    /// record is written. The writer is flushed before success is reported.
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
        let mut output = KeepTail::new(writer, self.local_tail_len());
        let LocalLayout {
            mut position,
            offsets,
            replacements,
        } = self.emit_local_records(plan, &mut output, preserve_gaps)?;
        let local_tail = output.into_tail();
        self.emit_directory(
            writer,
            &mut position,
            &local_tail,
            &offsets,
            &replacements,
            |probe| {
                self.emit_local_records(plan, probe, preserve_gaps)?;
                Ok(())
            },
        )
    }

    /// How many bytes of local records the end-record check needs: the
    /// directory and end record, which keep their input length, may not fill
    /// the `END_TAIL_LEN` bytes before EOF.
    pub(super) fn local_tail_len(&self) -> usize {
        END_TAIL_LEN.saturating_sub(self.original.len() - self.central_start)
    }

    /// Write every local record, and the gaps between them when
    /// `preserve_gaps`. Returns the directory offset, each member's new
    /// local-record offset, and the new sizes of replaced members.
    fn emit_local_records<W: Write>(
        &self,
        plan: &ChangePlan<'_>,
        writer: &mut W,
        preserve_gaps: bool,
    ) -> Result<LocalLayout> {
        let mut position = 0;
        let mut original_position = 0;
        let mut replacements: Vec<Option<Sizes>> = vec![None; self.entries.len()];
        let mut offsets: Vec<u32> = vec![0; self.entries.len()];
        let mut pending = plan.changes.iter().peekable();
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
            if let Some(&Change { codec, data, .. }) =
                pending.next_if(|change| change.index == index)
            {
                replacements[index] =
                    Some(self.emit_replacement(entry, codec, data, writer, &mut position)?);
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
        Ok(LocalLayout {
            position,
            offsets,
            replacements,
        })
    }

    /// Reproduce pre-1.0 append saves to exercise migration and compaction.
    #[cfg(test)]
    pub(super) fn append_to<W: Write>(
        &self,
        changes: &impl PartChanges,
        writer: &mut W,
    ) -> Result<()> {
        if changes.is_empty() {
            writer.write_all(&self.original)?;
            return Ok(());
        }
        let mut plan = self.validate_changes(changes)?;
        // Legacy appends emit replacements in central-directory order.
        plan.changes.sort_unstable_by_key(|change| change.index);
        // These saves only build test inputs, so the end-record check reads
        // all of their local records from memory.
        let mut local = Vec::new();
        let mut position = 0;
        emit(
            &mut local,
            &mut position,
            &self.original[..self.central_start],
        )?;
        let mut replacements: Vec<Option<Sizes>> = vec![None; self.entries.len()];
        let mut offsets: Vec<u32> = Vec::with_capacity(self.entries.len());
        let mut pending = plan.changes.iter().peekable();
        for (index, entry) in self.entries.iter().enumerate() {
            if let Some(&Change { codec, data, .. }) =
                pending.next_if(|change| change.index == index)
            {
                offsets.push(zip32(position, "ZIP local-record offset")?);
                replacements[index] =
                    Some(self.emit_replacement(entry, codec, data, &mut local, &mut position)?);
            } else {
                offsets.push(entry.local_record.start as u32);
            }
        }
        writer.write_all(&local)?;
        self.emit_directory(
            writer,
            &mut position,
            &local,
            &offsets,
            &replacements,
            |probe| Ok(probe.write_all(&local)?),
        )
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
            changes: Vec::with_capacity(parts.size_hint().0),
        };
        for (name, data) in parts {
            let Some(&index) = self.index.get(name) else {
                return Err(invalid(format!(
                    "cannot replace missing ZIP member: {name}"
                )));
            };
            if data.len() > MAX_PART_SIZE {
                return Err(over_read_limit(format_args!("ZIP member {name}")));
            }
            let codec = self.entries[index].codec()?;
            plan.changes.push(Change { index, codec, data });
        }
        plan.changes
            .sort_unstable_by_key(|change| self.entries[change.index].local_record.start);
        Ok(plan)
    }

    fn emit_replacement<W: Write>(
        &self,
        entry: &Entry,
        codec: Codec,
        data: &[u8],
        writer: &mut W,
        position: &mut usize,
    ) -> Result<Sizes> {
        let size = zip32(data.len(), "ZIP member size")?;
        let compressed: Vec<Cow<'_, [u8]>> = match codec {
            Codec::Stored => vec![Cow::Borrowed(data)],
            Codec::Deflated => compression::deflate(data, self.compression_threads)?
                .into_iter()
                .map(Cow::Owned)
                .collect(),
        };
        let compressed_len: usize = compressed.iter().map(|piece| piece.len()).sum();
        let compressed_size = zip32(compressed_len, "compressed ZIP member size")?;
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
            add(add(*position, original_header.len())?, compressed_len)?,
            "ZIP local-record area",
        )?;
        emit(writer, position, &header)?;
        emit(writer, position, &original_header[LOCAL_HEADER_LEN..])?;
        for piece in &compressed {
            emit(writer, position, piece)?;
        }
        Ok(sizes)
    }

    /// Write the central directory and end record after local records ending
    /// at `position`, unless readers could misread the output's last bytes.
    /// `local_tail` holds the last local-record bytes written: all of them, or
    /// at least those within `END_TAIL_LEN` bytes of EOF. `reemit` writes every
    /// local record again, for reading back earlier output.
    pub(super) fn emit_directory<W: Write>(
        &self,
        writer: &mut W,
        position: &mut usize,
        local_tail: &[u8],
        offsets: &[u32],
        replacements: &[Option<Sizes>],
        reemit: impl FnOnce(&mut Probe) -> Result<()>,
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
        // Build the directory after the last local bytes, which the check of
        // the output's end needs, and write it only once that check passes.
        let mut tail =
            Vec::with_capacity(local_tail.len() + self.original.len() - self.central_start);
        tail.extend_from_slice(local_tail);
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
            tail.extend_from_slice(&header);
            tail.extend_from_slice(&record[CENTRAL_HEADER_LEN..]);
        }
        debug_assert_eq!(
            tail.len() - local_tail.len(),
            central_size as usize,
            "central records are copied whole, so the directory keeps the input's size"
        );
        tail.extend_from_slice(&ending);
        let base = *position - local_tail.len();
        check_end_records(&tail, base, local_tail.len(), reemit)?;
        emit(writer, position, &tail[local_tail.len()..])
    }
}
