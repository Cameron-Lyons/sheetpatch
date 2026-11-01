//! Select one unambiguous ZIP end record, before and after rewriting.
use super::records::{add, u16_at, u32_at};
use super::{
    CENTRAL, END, END_LEN, END_TAIL_LEN, ZIP64_LOCATOR, ZIP64_LOCATOR_LEN, invalid, unsupported,
};
use crate::Result;
use std::io::{self, Write};

/// Whether an end record followed by its comment, `ending`, contains another
/// end-record signature with room for a complete record before EOF.
///
/// Readers that scan back from EOF select the last such signature even when
/// its comment does not reach EOF, so one inside the record's own fields or
/// comment makes the archive ambiguous. A signature too close to EOF for a
/// complete record cannot be read as one; see also [`shadows_end_record`].
pub(super) fn hides_end_record(ending: &[u8]) -> bool {
    let last = ending.len().saturating_sub(END_LEN);
    (1..=last).any(|offset| ending[offset..offset + 4] == END.to_le_bytes())
}

/// Whether Python's `zipfile` would select an end-record signature after the
/// start of `ending`, an end record followed by its comment. It takes the last
/// signature in the archive's tail, and fails to open the archive when no
/// complete record follows it. Only an archive without a comment is exempt:
/// Python first looks for a record in exactly the last 22 bytes.
pub(super) fn shadows_end_record(ending: &[u8]) -> bool {
    ending.len() > END_LEN
        && ending[1..]
            .windows(4)
            .any(|window| window == END.to_le_bytes())
}

/// Locate the end-of-central-directory record. It must be the only record
/// that ends at EOF with its comment, and the only one readers could select.
pub(super) fn end_of_central_directory(bytes: &[u8]) -> Result<usize> {
    if bytes.len() < END_LEN {
        return Err(invalid("file is not a ZIP workbook"));
    }
    let (end, decoy_directories) = locate_end_record(bytes, 0)?;
    for directory in decoy_directories {
        if u32_at(bytes, directory)? == CENTRAL {
            return Err(invalid("ambiguous ZIP end-of-directory records"));
        }
    }
    Ok(end)
}

/// Offsets in `tail`, the end of an archive, of end-record signatures whose
/// record and comment end exactly at EOF, last first. A comment of at most
/// 64 KiB puts every such record in the last `END_LEN + u16::MAX` bytes.
pub(super) fn end_record_candidates(tail: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let first = tail.len().saturating_sub(END_LEN + u16::MAX as usize);
    (first..=tail.len().saturating_sub(END_LEN))
        .rev()
        .filter(|&offset| {
            u32_at(tail, offset).ok() == Some(END)
                && u16_at(tail, offset + 20)
                    .is_ok_and(|comment| offset + END_LEN + comment as usize == tail.len())
        })
}

/// Apply the end-record rules to `tail`, the last bytes of an archive that
/// has `base` bytes before them. `tail` must hold the whole archive or at
/// least its last `END_TAIL_LEN` bytes, since every rule is relative to EOF
/// except where a decoy record's directory starts.
///
/// Returns the offset in `tail` of the end record, which must be the only
/// record that ends at EOF with its comment and the only one readers scanning
/// back from EOF could select. Also returns the directory offsets of earlier
/// records that libzip could select instead: the archive is ambiguous if a
/// central-directory header starts at any of them.
fn locate_end_record(tail: &[u8], base: usize) -> Result<(usize, Vec<usize>)> {
    // Multiple records ending at EOF would make the member index ambiguous.
    let mut candidates = end_record_candidates(tail);
    let end = match (candidates.next(), candidates.next()) {
        (Some(end), None) => end,
        (None, _) => {
            return Err(invalid(
                "ZIP end-of-directory record is missing or truncated",
            ));
        }
        (Some(_), Some(_)) => return Err(invalid("ambiguous ZIP end-of-directory records")),
    };
    // Fixed fields may legitimately hold the signature's bytes, for example a
    // central directory at offset 0x06054b50. That matters only when the
    // comment leaves room for a complete record after the signature.
    if hides_end_record(&tail[end..]) {
        return Err(invalid("ambiguous ZIP end-of-directory records"));
    }
    // libzip before 1.11 instead scans its tail window forward and keeps the
    // first record whose directory it can read, unless a later one spans more
    // local data. Report every earlier record it could select.
    let window = tail.len().saturating_sub(END_TAIL_LEN);
    let mut decoy_directories = Vec::new();
    for offset in window..end {
        if let Some(directory) = decoy_directory(tail, base, offset)? {
            decoy_directories.push(directory);
        }
    }
    Ok((end, decoy_directories))
}

/// The directory offset of a record at `offset` in `tail`, the end of an
/// archive with `base` bytes before it, if a reader scanning forward could
/// select it as the end record once its directory starts with a
/// central-directory header: a complete single-disk record for a nonempty
/// directory, with a comment that ends by EOF, whose directory ends before the
/// record. Such a reader never prefers an empty directory.
///
/// A Stored embedded ZIP's own end record normally fails the header test,
/// because its directory offset is relative to the embedded archive.
fn decoy_directory(tail: &[u8], base: usize, offset: usize) -> Result<Option<usize>> {
    let record_end = add(offset, END_LEN)?;
    if record_end > tail.len() || u32_at(tail, offset)? != END {
        return Ok(None);
    }
    let entries = u16_at(tail, offset + 10)?;
    let comment_end = add(record_end, u16_at(tail, offset + 20)? as usize)?;
    let central_start = u32_at(tail, offset + 16)? as usize;
    // These bytes need not be a record. On 32-bit targets their directory can
    // end past the address space, and then it does not end before the record.
    let central_end = central_start.checked_add(u32_at(tail, offset + 12)? as usize);
    let record_start = add(base, offset)?;
    let selectable = u16_at(tail, offset + 4)? == 0
        && u16_at(tail, offset + 6)? == 0
        && u16_at(tail, offset + 8)? == entries
        && entries != 0
        && comment_end <= tail.len()
        && central_end.is_some_and(|central_end| central_end <= record_start);
    Ok(selectable.then_some(central_start))
}

/// Refuse output that readers could interpret differently from its own end
/// record, the last one in `tail`. `tail` holds the output's last bytes after
/// `base` earlier ones, as [`locate_end_record`] requires: `local_len` bytes
/// of local records, then the central directory and end record. `reemit`
/// writes the local records again, for reading back earlier bytes.
pub(super) fn check_end_records(
    tail: &[u8],
    base: usize,
    local_len: usize,
    reemit: impl FnOnce(&mut Probe) -> Result<()>,
) -> Result<()> {
    debug_assert!(
        base == 0 || tail.len() >= END_TAIL_LEN,
        "the end-record rules need the output's last {END_TAIL_LEN} bytes, but only {} \
         follow {base} dropped ones",
        tail.len()
    );
    let ambiguous = || {
        unsupported(
            "saving would make the ZIP end-of-directory record ambiguous: the saved \
             archive would contain another end-of-directory record that readers could select",
        )
    };
    // Rewritten local offsets, sizes and checksums can form a record, and so
    // can bytes that a save moves to a new distance from EOF. Records whose
    // comment reaches EOF would make this crate refuse its own output.
    let (end, decoy_directories) = locate_end_record(tail, base).map_err(|_| ambiguous())?;
    if end >= ZIP64_LOCATOR_LEN
        && tail[end - ZIP64_LOCATOR_LEN..].starts_with(&ZIP64_LOCATOR.to_le_bytes())
    {
        return Err(unsupported(
            "saving would make the archive look like ZIP64: the saved archive would have a \
             ZIP64 end-of-directory locator signature before its end record",
        ));
    }
    let central = CENTRAL.to_le_bytes();
    let mut earlier = Vec::new();
    for directory in decoy_directories {
        match directory
            .checked_sub(base)
            .and_then(|offset| tail.get(offset..offset + central.len()))
        {
            Some(bytes) if bytes == central => return Err(ambiguous()),
            Some(_) => {}
            None => earlier.push(directory),
        }
    }
    // Output before `tail` is no longer held. Records that point there are
    // rare: in ordinary workbooks only a Stored embedded ZIP's own end record,
    // whose directory offset is relative to that ZIP. Emission is
    // deterministic, so write the output again and read those bytes back.
    if !earlier.is_empty() {
        earlier.sort_unstable();
        earlier.dedup();
        let mut probe = Probe::new(earlier);
        reemit(&mut probe)?;
        debug_assert_eq!(
            probe.position,
            base + local_len,
            "emission is deterministic, so writing the local records again must end where \
             they did"
        );
        probe.write_all(&tail[local_len..])?;
        if probe.words.iter().any(|word| word.bytes == central) {
            return Err(ambiguous());
        }
    }
    Ok(())
}

/// The four bytes written at an offset.
struct Word {
    offset: usize,
    bytes: [u8; 4],
}

/// Discards output, recording the four bytes at each of some offsets.
pub(super) struct Probe {
    position: usize,
    /// The words at each offset, in ascending order.
    words: Vec<Word>,
}

impl Probe {
    fn new(offsets: Vec<usize>) -> Self {
        debug_assert!(
            offsets.is_sorted(),
            "writes find words by binary search, so callers sort the offsets"
        );
        Self {
            position: 0,
            words: offsets
                .into_iter()
                .map(|offset| Word {
                    offset,
                    bytes: [0; 4],
                })
                .collect(),
        }
    }
}

impl Write for Probe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let start = self.position;
        self.position += bytes.len();
        let first = self.words.partition_point(|word| word.offset + 4 <= start);
        for word in &mut self.words[first..] {
            if word.offset >= self.position {
                break;
            }
            for (index, byte) in word.bytes.iter_mut().enumerate() {
                if let Some(&value) = (word.offset + index)
                    .checked_sub(start)
                    .and_then(|at| bytes.get(at))
                {
                    *byte = value;
                }
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
