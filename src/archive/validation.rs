//! Select one unambiguous ZIP end record, before and after rewriting.
use super::records::{add, u16_at, u32_at};
use super::{
    CENTRAL, END, END_LEN, END_TAIL_LEN, ZIP64_LOCATOR, ZIP64_LOCATOR_LEN, invalid, unsupported,
};
use crate::Result;
use memchr::memmem;
use std::io::{self, Write};

const END_SIGNATURE: [u8; 4] = END.to_le_bytes();

/// Signature offsets with enough remaining bytes for a complete fixed header.
fn end_signatures(bytes: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let searchable = bytes.len().saturating_sub(END_LEN - END_SIGNATURE.len());
    memmem::find_iter(&bytes[..searchable], &END_SIGNATURE)
}

/// Whether an end record followed by its comment, `ending`, contains another
/// end-record signature with room for a complete record before EOF.
///
/// Readers that scan back from EOF select the last such signature even when
/// its comment does not reach EOF, so one inside the record's own fields or
/// comment makes the archive ambiguous. A signature too close to EOF for a
/// complete record cannot be read as one; see also [`shadows_end_record`].
pub(super) fn hides_end_record(ending: &[u8]) -> bool {
    end_signatures(&ending[1..]).next().is_some()
}

/// Whether Python's `zipfile` would select an end-record signature after the
/// start of `ending`, an end record followed by its comment. It takes the last
/// signature in the archive's tail, and fails to open the archive when no
/// complete record follows it. Only an archive without a comment is exempt:
/// Python first looks for a record in exactly the last 22 bytes.
pub(super) fn shadows_end_record(ending: &[u8]) -> bool {
    ending.len() > END_LEN && memmem::find(&ending[1..], &END_SIGNATURE).is_some()
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
    let window = tail.len().saturating_sub(END_TAIL_LEN);
    let mut signatures = end_signatures(&tail[window..]);
    let mut decoy_directories = Vec::new();
    while let Some(relative) = signatures.next() {
        let offset = window + relative;
        let comment = usize::from(u16_at(tail, offset + 20)?);
        if tail.len() - offset - END_LEN == comment {
            // A backward-scanning reader could select any later complete
            // header, even if its comment does not reach EOF.
            if signatures.next().is_some() {
                return Err(invalid("ambiguous ZIP end-of-directory records"));
            }
            return Ok((offset, decoy_directories));
        }
        // libzip before 1.11 scans forward. Keep earlier records whose
        // directories it could select instead.
        if let Some(directory) = decoy_directory(tail, base, offset)? {
            decoy_directories.push(directory);
        }
    }
    Err(invalid(
        "ZIP end-of-directory record is missing or truncated",
    ))
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
    let record_end = offset + END_LEN;
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
/// of local records, then the final central records and end record. `reemit`
/// writes all local records and any central records before `tail` again.
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
            "replayed records must end at the first central record in the retained tail"
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
