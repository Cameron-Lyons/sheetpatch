//! Archive preservation, validation, compaction and codec regressions.
use super::compression::{
    DEFLATE_LEVEL, MAX_DEFLATE_THREADS, deflate, deflate_segments, inspect_deflate,
};
use super::records::{Sizes, check_extra, set_u16, set_u32, u16_at, u32_at};
use super::validation::{
    end_of_central_directory, end_record_candidates, hides_end_record, shadows_end_record,
};
use super::writer::KeepTail;
use flate2::write::DeflateEncoder;
use flate2::{Compression, Decompress, FlushDecompress, Status};
use std::io::Write;
use std::iter;

use std::cell::Cell;

use super::*;
use crate::common;

thread_local! {
    /// Replaces `DEFLATE_SEGMENT` in saves on the current thread, so that
    /// small replacements are deflated in many segments.
    pub(super) static SEGMENT_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
    /// Makes the compression threads of saves on the current thread fail
    /// to start, as they can under a limit on the user's processes.
    pub(super) static SPAWN_FAILS: Cell<bool> = const { Cell::new(false) };
    /// Counts attempted compression workers on the saving thread.
    pub(super) static SPAWNS: Cell<usize> = const { Cell::new(0) };
}

fn append(archive: &Archive, changes: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut bytes = Vec::new();
    archive.append_to(changes, &mut bytes).unwrap();
    bytes
}

/// Where a fixture member stores its CRC-32 and sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Descriptor {
    /// In its local header, without a data descriptor.
    Absent,
    /// In a data descriptor without a signature.
    Unsigned,
    /// In a data descriptor that starts with its signature.
    Signed,
}

/// The compression method of a decorated fixture member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Stored,
    /// Method 99, which saves preserve but never read or replace.
    Opaque,
}

fn fixture(descriptor: Descriptor) -> Vec<u8> {
    named_fixture(b"part.xml", 0, descriptor)
}

fn named_fixture(name: &[u8], flags: u16, descriptor: Descriptor) -> Vec<u8> {
    stored_fixture(name, b"<part/>", flags, descriptor)
}

fn stored_fixture(name: &[u8], data: &[u8], flags: u16, descriptor: Descriptor) -> Vec<u8> {
    let crc = crc32(data);
    let flags = match descriptor {
        Descriptor::Absent => flags,
        Descriptor::Unsigned | Descriptor::Signed => flags | FLAG_DESCRIPTOR,
    };
    let mut bytes = vec![0; 30];
    set_u32(&mut bytes, 0, LOCAL);
    set_u16(&mut bytes, 4, 20);
    set_u16(&mut bytes, 6, flags);
    if descriptor == Descriptor::Absent {
        set_u32(&mut bytes, 14, crc);
        set_u32(&mut bytes, 18, data.len() as u32);
        set_u32(&mut bytes, 22, data.len() as u32);
    }
    set_u16(&mut bytes, 26, name.len() as u16);
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(data);
    if descriptor == Descriptor::Signed {
        bytes.extend_from_slice(&DESCRIPTOR.to_le_bytes());
    }
    if descriptor != Descriptor::Absent {
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
    let original = fixture(Descriptor::Absent);
    let archive = Archive::try_new(original.clone()).unwrap();
    assert_eq!(archive.read("part.xml").unwrap(), b"<part/>");
    assert_eq!(archive.write(&BTreeMap::new()).unwrap(), original);
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    assert!(archive.contains("part.xml"));
    assert!(!archive.contains("missing.xml"));
}

#[test]
fn opc_part_lookup_preserves_spelling_and_refuses_equivalent_duplicates() {
    let archive = Archive::try_new(fixture(Descriptor::Absent)).unwrap();
    assert_eq!(archive.part_name("PART.XML").unwrap(), Some("part.xml"));
    assert_eq!(archive.part_name("missing.xml").unwrap(), None);
    let original = combine(
        fixture(Descriptor::Absent),
        decorate_fixture(b"PART.XML", Method::Stored),
    );
    let archive = Archive::try_new(original.clone()).unwrap();
    assert!(archive.part_name("part.xml").is_err());
    // ZIP indexing itself remains exact, so opaque duplicate OPC names do
    // not prevent preserving an otherwise unaccessed archive member.
    assert_eq!(archive.read("part.xml").unwrap(), b"<part/>");
    assert!(archive.contains("PART.XML"));
    assert_eq!(archive.write(&BTreeMap::new()).unwrap(), original);
}

#[test]
fn appends_replacement_and_retains_original_records_and_comment() {
    for descriptor in [Descriptor::Absent, Descriptor::Signed] {
        let original = fixture(descriptor);
        let archive = Archive::try_new(original.clone()).unwrap();
        let changes = BTreeMap::from([("part.xml".into(), b"<part changed=\"yes\"/>".to_vec())]);
        let edited = append(&archive, &changes);
        assert_eq!(
            &edited[..archive.central_start],
            &original[..archive.central_start]
        );
        assert!(edited.ends_with(b"comment"));
        let reopened = Archive::try_new(edited).unwrap();
        assert_eq!(reopened.read("part.xml").unwrap(), changes["part.xml"]);
        assert_eq!(reopened.entries[0].flags & 8, 0);
    }
}

#[test]
fn replacements_leave_no_unreferenced_records_and_preserve_untouched_metadata() {
    let original = Archive::try_new(combine(
        decorate_fixture(b"part.xml", Method::Stored),
        decorate_fixture(b"keep.xml", Method::Opaque),
    ))
    .unwrap();
    // Central-directory order need not match physical local-record order.
    let mut bytes = original.original[..original.central_start].to_vec();
    bytes.extend_from_slice(&original.original[original.entries[1].central.clone()]);
    bytes.extend_from_slice(&original.original[original.entries[0].central.clone()]);
    bytes.extend_from_slice(&original.original[original.end..]);
    let mut archive = Archive::try_new(bytes).unwrap();
    let untouched = &archive.entries[archive.index["keep.xml"]];
    let local = archive.original[untouched.local_record.clone()].to_vec();
    let central = archive.original[untouched.central.clone()].to_vec();
    for data in [b"longer replacement".as_slice(), b"short".as_slice()] {
        let changes = BTreeMap::from([("part.xml".into(), data.to_vec())]);
        let edited = archive.write(&changes).unwrap();
        let mut compacted = Vec::new();
        archive.compact_to(&changes, &mut compacted).unwrap();
        assert_eq!(edited, compacted);
        archive = Archive::try_new(edited).unwrap();
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
    let original = Archive::try_new(combine(
        fixture(Descriptor::Absent),
        decorate_fixture(b"keep.xml", Method::Opaque),
    ))
    .unwrap();
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
    let archive = Archive::try_new(bytes).unwrap();
    let edited = archive
        .write(&BTreeMap::from([("part.xml".into(), b"updated".to_vec())]))
        .unwrap();
    let updated = Archive::try_new(edited).unwrap();
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
    let mut original = decorate_fixture(b"part.xml", Method::Stored);
    let end = original.len() - 29;
    original.truncate(end + 22);
    set_u16(&mut original, end + 20, u16::MAX);
    original.extend(iter::repeat_n(b'!', u16::MAX as usize));
    let archive = Archive::try_new(original).unwrap();
    let changes = BTreeMap::from([("part.xml".into(), b"updated payload".to_vec())]);
    let updated = Archive::try_new(archive.write(&changes).unwrap()).unwrap();
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
    let original = fixture(Descriptor::Absent);
    for end in 0..original.len() {
        assert!(Archive::try_new(original[..end].to_vec()).is_err());
    }
    let mut corrupted = original;
    corrupted[30 + b"part.xml".len()] ^= 1;
    let archive = Archive::try_new(corrupted).unwrap();
    assert!(archive.read("part.xml").is_err());
}

#[test]
fn rejects_zip64_and_inconsistent_offsets() {
    let mut bytes = fixture(Descriptor::Absent);
    let end = bytes.len() - 29;
    set_u32(&mut bytes, end + 16, u32::MAX);
    assert!(matches!(
        Archive::try_new(bytes),
        Err(Error::Unsupported(_))
    ));
    let mut bytes = fixture(Descriptor::Absent);
    let end = bytes.len() - 29;
    set_u32(&mut bytes, end + 16, 1);
    assert!(Archive::try_new(bytes).is_err());
}

#[test]
fn rejects_archives_other_readers_could_interpret_differently() {
    // fixture(Descriptor::Absent) has its local header at 0..38 with the
    // name at 30, data at 38..45, the central record at 45..99, and the
    // end record with its comment at 99..128. Member names have their own
    // tests, such as rejects_empty_nul_and_duplicate_member_names.
    const DIRECTORY: usize = 45;
    const END_RECORD: usize = 99;
    const ZIP64_EXTRA: [u8; 12] = [1, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let base = Archive::try_new(fixture(Descriptor::Absent)).unwrap();
    assert_eq!((base.central_start, base.end), (DIRECTORY, END_RECORD));
    let edited = |surgery: fn(&mut Vec<u8>)| {
        let mut bytes = fixture(Descriptor::Absent);
        surgery(&mut bytes);
        bytes
    };
    let mut fewer_entries = combine(
        fixture(Descriptor::Absent),
        decorate_fixture(b"keep.xml", Method::Stored),
    );
    let end = Archive::try_new(fewer_entries.clone()).unwrap().end;
    set_u16(&mut fewer_entries, end + 8, 1);
    set_u16(&mut fewer_entries, end + 10, 1);
    let multi_disk = || unsupported("multi-disk ZIP workbooks are not supported");
    let cases = [
        // Local and central headers must agree.
        (
            "local header offset",
            edited(|b| set_u32(b, DIRECTORY + 42, 1)),
            invalid("invalid ZIP local header for part.xml"),
        ),
        (
            "local method",
            edited(|b| set_u16(b, 8, DEFLATED)),
            invalid("conflicting ZIP headers for part.xml"),
        ),
        (
            "local name",
            edited(|b| b[30] = b'P'),
            invalid("conflicting ZIP member name for part.xml"),
        ),
        (
            "local checksum",
            edited(|b| b[14] ^= 0xff),
            invalid("conflicting ZIP sizes or checksum for part.xml"),
        ),
        (
            "stored sizes",
            edited(|b| {
                set_u32(b, 22, 6);
                set_u32(b, DIRECTORY + 24, 6);
            }),
            invalid("invalid stored ZIP member size for part.xml"),
        ),
        (
            "data past the directory",
            edited(|b| {
                for at in [18, 22, DIRECTORY + 20, DIRECTORY + 24] {
                    set_u32(b, at, 100);
                }
            }),
            invalid("ZIP member part.xml exceeds local-record bounds"),
        ),
        // The directory must fill the space before the end record with
        // exactly the counted records.
        (
            "directory bounds",
            edited(|b| set_u32(b, END_RECORD + 16, 44)),
            invalid("ZIP central-directory bounds are inconsistent"),
        ),
        (
            "more entries than records",
            edited(|b| {
                set_u16(b, END_RECORD + 8, 2);
                set_u16(b, END_RECORD + 10, 2);
            }),
            invalid("truncated or invalid ZIP central-directory entry"),
        ),
        (
            "fewer entries than records",
            fewer_entries,
            invalid("ZIP central-directory entry count is inconsistent"),
        ),
        // Multi-disk archives.
        (
            "end record disk",
            edited(|b| set_u16(b, END_RECORD + 4, 1)),
            multi_disk(),
        ),
        (
            "end record directory disk",
            edited(|b| set_u16(b, END_RECORD + 6, 1)),
            multi_disk(),
        ),
        (
            "end record disk entries",
            edited(|b| set_u16(b, END_RECORD + 8, 0)),
            multi_disk(),
        ),
        (
            "central disk",
            edited(|b| set_u16(b, DIRECTORY + 34, 1)),
            multi_disk(),
        ),
        // ZIP64 locators, sentinels and extra fields.
        (
            "ZIP64 locator",
            edited(|b| set_u32(b, END_RECORD - ZIP64_LOCATOR_LEN, ZIP64_LOCATOR)),
            zip64(),
        ),
        (
            "end record entry sentinel",
            edited(|b| {
                set_u16(b, END_RECORD + 8, u16::MAX);
                set_u16(b, END_RECORD + 10, u16::MAX);
            }),
            zip64(),
        ),
        (
            "end record size sentinel",
            edited(|b| set_u32(b, END_RECORD + 12, u32::MAX)),
            zip64(),
        ),
        (
            "central disk sentinel",
            edited(|b| set_u16(b, DIRECTORY + 34, u16::MAX)),
            zip64(),
        ),
        (
            "central compressed size sentinel",
            edited(|b| set_u32(b, DIRECTORY + 20, u32::MAX)),
            zip64(),
        ),
        (
            "central size sentinel",
            edited(|b| set_u32(b, DIRECTORY + 24, u32::MAX)),
            zip64(),
        ),
        (
            "central offset sentinel",
            edited(|b| set_u32(b, DIRECTORY + 42, u32::MAX)),
            zip64(),
        ),
        (
            "central ZIP64 extra",
            edited(|b| {
                b.splice(END_RECORD..END_RECORD, ZIP64_EXTRA);
                set_u16(b, DIRECTORY + 30, 12);
                set_u32(b, END_RECORD + 12 + 12, 54 + 12);
            }),
            zip64(),
        ),
        (
            "local ZIP64 extra",
            edited(|b| {
                b.splice(38..38, ZIP64_EXTRA);
                set_u16(b, 28, 12);
                set_u32(b, END_RECORD + 12 + 16, (DIRECTORY + 12) as u32);
            }),
            zip64(),
        ),
    ];
    for (case, bytes, expected) in cases {
        match (Archive::try_new(bytes), &expected) {
            (Err(Error::InvalidWorkbook(actual)), Error::InvalidWorkbook(message))
            | (Err(Error::Unsupported(actual)), Error::Unsupported(message)) => {
                assert_eq!(actual, *message, "{case}");
            }
            (result, _) => panic!("{case}: {:?} instead of {expected:?}", result.err()),
        }
    }
}

fn deflate_fixture(compressed: &[u8]) -> Vec<u8> {
    let archive = Archive::try_new(fixture(Descriptor::Absent)).unwrap();
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
    let valid = Archive::try_new(deflate_fixture(&compressed)).unwrap();
    assert_eq!(valid.read("part.xml").unwrap(), b"<part/>");
    assert!(
        Archive::try_new(truncated)
            .unwrap()
            .read("part.xml")
            .is_err()
    );
    let changed = valid
        .write(&BTreeMap::from([("part.xml".into(), b"changed".to_vec())]))
        .unwrap();
    assert_eq!(
        Archive::try_new(changed).unwrap().read("part.xml").unwrap(),
        b"changed"
    );
}

fn deflate_descriptor_fixture(signed: bool) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(b"<part/>").unwrap();
    let compressed = encoder.finish().unwrap();
    let archive = Archive::try_new(deflate_fixture(&compressed)).unwrap();
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

fn decorate_fixture(name: &[u8; 8], method: Method) -> Vec<u8> {
    let archive = Archive::try_new(fixture(Descriptor::Signed)).unwrap();
    let entry = &archive.entries[0];
    let extra = [0xef, 0xbe, 3, 0, b'x', b'y', b'z'];
    let mut bytes = archive.original[entry.local_header.clone()].to_vec();
    bytes[30..38].copy_from_slice(name);
    set_u16(&mut bytes, 28, extra.len() as u16);
    set_u16(&mut bytes, 10, 0x1234);
    set_u16(&mut bytes, 12, 0x5678);
    if method == Method::Opaque {
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
    if method == Method::Opaque {
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
        let part = Archive::try_new(part).unwrap();
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
    let archive = Archive::try_new(combine_members((0..96).map(|index| {
        let name: [u8; 8] = format!("part{index:04}").into_bytes().try_into().unwrap();
        let method = if index % 3 == 0 {
            Method::Stored
        } else {
            Method::Opaque
        };
        decorate_fixture(&name, method)
    })))
    .unwrap();
    let mut reordered = archive.original[..archive.central_start].to_vec();
    for entry in archive.entries.iter().rev() {
        reordered.extend_from_slice(&archive.original[entry.central.clone()]);
    }
    reordered.extend_from_slice(&archive.original[archive.end..]);
    let archive = Archive::try_new(reordered).unwrap();
    let changes: BTreeMap<_, _> = (0..96)
        .step_by(3)
        .map(|index| {
            (
                format!("part{index:04}"),
                format!("replacement {index} {}", "x".repeat(index)).into_bytes(),
            )
        })
        .collect();
    let obsolete = Archive::try_new(append(&archive, &changes)).unwrap();
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
            let reopened = Archive::try_new(output).unwrap();
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
    struct BorrowedChanges<'changes>(&'changes [(&'changes str, &'changes [u8])]);

    impl PartChanges for BorrowedChanges<'_> {
        fn parts(&self) -> impl Iterator<Item = (&str, &[u8])> {
            self.0.iter().copied()
        }
    }

    let original = fixture(Descriptor::Absent);
    let archive = Archive::try_new(original.clone()).unwrap();
    let empty = BorrowedChanges(&[]);
    assert_eq!(archive.write(&empty).unwrap(), original);
    let valid = BorrowedChanges(&[("part.xml", b"borrowed replacement")]);
    assert_eq!(
        Archive::try_new(archive.write(&valid).unwrap())
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
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.bytes.len() == self.limit {
                return Err(io::Error::other("injected failure"));
            }
            let length = bytes.len().min(3).min(self.limit - self.bytes.len());
            self.bytes.extend_from_slice(&bytes[..length]);
            Ok(length)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let archive = Archive::try_new(fixture(Descriptor::Signed)).unwrap();
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
    let archive = Archive::try_new(decorate_fixture(b"part.xml", Method::Opaque)).unwrap();
    let opaque = |result: Result<()>| {
        matches!(
            result,
            Err(Error::Unsupported(message))
                if message == "ZIP compression method 99 in part.xml"
        )
    };
    assert!(opaque(archive.read("part.xml").map(drop)));
    for name in ["part.xml", "missing.xml"] {
        let changes = BTreeMap::from([(name.into(), b"changed".to_vec())]);
        let mut output = Vec::new();
        let result = archive.write_to(&changes, &mut output);
        assert_eq!(opaque(result), name == "part.xml");
        assert!(output.is_empty());
        assert!(archive.compact_to(&changes, &mut output).is_err());
        assert!(output.is_empty());
    }
}

#[test]
fn patched_zip_members_remain_opaque_and_cannot_be_read_or_replaced() {
    let mut patched = fixture(Descriptor::Absent);
    let archive = Archive::try_new(patched.clone()).unwrap();
    let central = archive.entries[0].central.start;
    set_u16(&mut patched, 6, 0x20);
    set_u16(&mut patched, central + 8, 0x20);
    let original = combine(patched, decorate_fixture(b"keep.xml", Method::Stored));
    let archive = Archive::try_new(original.clone()).unwrap();
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
    let reopened = Archive::try_new(output).unwrap();
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
    let archive = Archive::try_new(fixture(Descriptor::Absent)).unwrap();
    let changes = BTreeMap::from([("part.xml".into(), b"replacement".to_vec())]);
    let mut original = append(&archive, &changes);
    set_u16(&mut original, 6, 0x20);
    let archive = Archive::try_new(original.clone()).unwrap();
    assert_eq!(archive.read("part.xml").unwrap(), b"replacement");
    assert_eq!(archive.write(&BTreeMap::new()).unwrap(), original);
    let mut output = Vec::new();
    assert!(archive.compact_to(&BTreeMap::new(), &mut output).is_err());
    assert!(output.is_empty());
}

#[test]
fn compaction_keeps_clean_archives_and_all_descriptor_forms_byte_identical() {
    let mut unsigned = fixture(Descriptor::Signed);
    let signed_offset = 30 + b"part.xml".len() + b"<part/>".len();
    unsigned.drain(signed_offset..signed_offset + 4);
    let end = unsigned.len() - 29;
    let offset = u32_at(&unsigned, end + 16).unwrap() - 4;
    set_u32(&mut unsigned, end + 16, offset);
    for original in [
        fixture(Descriptor::Absent),
        fixture(Descriptor::Signed),
        unsigned,
        deflate_descriptor_fixture(/* signed: */ false),
        deflate_descriptor_fixture(/* signed: */ true),
    ] {
        let archive = Archive::try_new(original.clone()).unwrap();
        let mut output = Vec::new();
        archive.compact_to(&BTreeMap::new(), &mut output).unwrap();
        assert_eq!(output, original);
        let changed = append(
            &archive,
            &BTreeMap::from([("part.xml".into(), b"new".to_vec())]),
        );
        let mut compacted = Vec::new();
        Archive::try_new(changed)
            .unwrap()
            .compact_to(&BTreeMap::new(), &mut compacted)
            .unwrap();
        assert_eq!(
            Archive::try_new(compacted)
                .unwrap()
                .read("part.xml")
                .unwrap(),
            b"new"
        );
    }
}

#[test]
fn repeated_compaction_stays_small_and_preserves_untouched_records_and_metadata() {
    let original = combine(
        decorate_fixture(b"part.xml", Method::Stored),
        decorate_fixture(b"keep.xml", Method::Opaque),
    );
    let archive = Archive::try_new(original).unwrap();
    let untouched = &archive.entries[1];
    let local = archive.original[untouched.local_record.clone()].to_vec();
    let central = archive.original[untouched.central.clone()].to_vec();
    let mut current = archive;
    let mut stable_size = None;
    for index in 0..10 {
        let changes = BTreeMap::from([("part.xml".into(), format!("value {index}").into_bytes())]);
        let appended = append(&current, &changes);
        let mut compacted = Vec::new();
        let appended = Archive::try_new(appended).unwrap();
        appended
            .compact_to(&BTreeMap::new(), &mut compacted)
            .unwrap();
        assert!(compacted.len() < appended.original.len());
        if let Some(size) = stable_size {
            assert_eq!(compacted.len(), size);
        } else {
            stable_size = Some(compacted.len());
        }
        current = Archive::try_new(compacted).unwrap();
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
    let archive = Archive::try_new(fixture(Descriptor::Absent)).unwrap();
    let changes = BTreeMap::from([("part.xml".into(), b"updated".to_vec())]);
    let mut compacted = Vec::new();
    archive.compact_to(&changes, &mut compacted).unwrap();
    let expected_len = archive.original.len() - b"<part/>".len() + b"updated".len();
    assert_eq!(compacted.len(), expected_len);
    assert_eq!(
        Archive::try_new(compacted)
            .unwrap()
            .read("part.xml")
            .unwrap(),
        b"updated"
    );
}

#[test]
fn compaction_checks_obsolete_deflate_payloads_across_output_chunks() {
    let archive = Archive::try_new(deflate_descriptor_fixture(/* signed: */ true)).unwrap();
    let large = b"123456789".repeat(3000);
    let archive = Archive::try_new(
        archive
            .write(&BTreeMap::from([("part.xml".into(), large.clone())]))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(archive.read("part.xml").unwrap(), large);
    let appended = Archive::try_new(append(
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
        Archive::try_new(compacted)
            .unwrap()
            .read("part.xml")
            .unwrap(),
        b"updated"
    );
}

/// Bytes from a fixed pseudo-random sequence, which deflate cannot shrink.
fn noise(length: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_u32;
    let mut bytes = Vec::with_capacity(length);
    for _ in 0..length {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        bytes.push(state as u8);
    }
    bytes
}

fn cells(count: usize) -> Vec<u8> {
    (0..count)
        .flat_map(|row| format!("<c r=\"A{row}\"><v>{}</v></c>", row * 7919 % 10007).into_bytes())
        .collect()
}

#[test]
fn deflate_segments_form_one_stream_whatever_the_thread_count() {
    const SEGMENT: usize = 4096;
    for data in [noise(3 * SEGMENT + 1), cells(1000)] {
        for length in [SEGMENT, 2 * SEGMENT + 1, 3 * SEGMENT, 3 * SEGMENT + 1] {
            let data = &data[..length];
            let segments = deflate_segments(data, SEGMENT, 1).unwrap();
            assert_eq!(segments.len(), length.div_ceil(SEGMENT));
            for threads in [2, 3, MAX_DEFLATE_THREADS] {
                assert_eq!(deflate_segments(data, SEGMENT, threads).unwrap(), segments);
            }
            // Inflating stops at the end of the last segment and not
            // before: only that segment ends the stream.
            let compressed = segments.concat();
            let mut decoder = Decompress::new(/* zlib_header: */ false);
            let mut output = Vec::with_capacity(length + 1);
            let status = decoder
                .decompress_vec(&compressed, &mut output, FlushDecompress::Finish)
                .unwrap();
            assert_eq!(status, Status::StreamEnd);
            assert_eq!(decoder.total_in() as usize, compressed.len());
            assert_eq!(output, data);
        }
    }
}

#[test]
fn segments_whose_threads_cannot_start_are_deflated_by_the_calling_thread() {
    const SEGMENT: usize = 4096;
    let data = cells(4000);
    let expected = deflate_segments(&data, SEGMENT, 1).unwrap();
    assert!(expected.len() > MAX_DEFLATE_THREADS);
    SPAWN_FAILS.set(true);
    let inline = deflate_segments(&data, SEGMENT, MAX_DEFLATE_THREADS);
    SPAWN_FAILS.set(false);
    assert_eq!(inline.unwrap(), expected);
}

#[test]
fn configured_compression_limits_workers_without_changing_archive_bytes() {
    const SEGMENT: usize = 4096;
    SEGMENT_OVERRIDE.set(Some(SEGMENT));
    let large = cells(4000);
    let changes = BTreeMap::from([("part.xml".into(), large.clone())]);
    let mut archive = Archive::try_new(deflate_descriptor_fixture(true)).unwrap();
    assert_eq!(archive.compression_threads(), None);
    let automatic = archive.write(&changes).unwrap();
    for threads in [1, 2] {
        let limit = NonZeroUsize::new(threads);
        archive.set_compression_threads(limit);
        assert_eq!(archive.compression_threads(), limit);
        SPAWNS.set(0);
        let output = archive.write(&changes).unwrap();
        assert!(SPAWNS.get() < threads);
        assert_eq!(output, automatic);
        assert_eq!(common::payload(&output, "part.xml"), large);
        let mut compact = Vec::new();
        archive.compact_to(&changes, &mut compact).unwrap();
        assert_eq!(compact, automatic);
    }
    archive.set_compression_threads(None);
    assert_eq!(archive.write(&changes).unwrap(), automatic);
    SEGMENT_OVERRIDE.set(None);
}

#[test]
fn large_replacements_are_deflated_in_segments_that_read_back_and_compact() {
    const SEGMENT: usize = 4096;
    SEGMENT_OVERRIDE.set(Some(SEGMENT));
    let large = cells(4000);
    let segments = deflate_segments(&large, SEGMENT, 1).unwrap();
    assert!(segments.len() > MAX_DEFLATE_THREADS);
    let archive = Archive::try_new(deflate_descriptor_fixture(/* signed: */ true)).unwrap();
    let written = archive
        .write(&BTreeMap::from([("part.xml".into(), large.clone())]))
        .unwrap();
    let reopened = Archive::try_new(written.clone()).unwrap();
    assert_eq!(reopened.read("part.xml").unwrap(), large);
    // Threads compressed the segments, with the same bytes as one.
    let entry = &reopened.entries[0];
    assert_eq!(reopened.original[entry.data.clone()], segments.concat());
    // An independent reader agrees, and inflating everything after the
    // payload start stops exactly at its recorded size.
    assert_eq!(common::payload(&written, "part.xml"), large);
    let sizes = inspect_deflate(&reopened.original[entry.data.start..], "part.xml").unwrap();
    assert!(sizes == entry.sizes);
    // Compaction recognizes the multi-segment record once obsolete.
    let changes = BTreeMap::from([("part.xml".into(), b"updated".to_vec())]);
    let appended = Archive::try_new(append(&reopened, &changes)).unwrap();
    let mut compacted = Vec::new();
    appended
        .compact_to(&BTreeMap::new(), &mut compacted)
        .unwrap();
    assert_eq!(compacted, reopened.write(&changes).unwrap());
    // Replacements of at most two segments stay one stream.
    let small = &large[..2 * SEGMENT];
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(DEFLATE_LEVEL));
    encoder.write_all(small).unwrap();
    assert_eq!(deflate(small, None).unwrap(), [encoder.finish().unwrap()]);
}

#[test]
fn compacts_many_stored_streaming_orphans_in_one_gap() {
    let original = combine_members((0..64).map(|index| {
        let name: [u8; 8] = format!("part{index:04}").into_bytes().try_into().unwrap();
        decorate_fixture(&name, Method::Stored)
    }));
    let archive = Archive::try_new(original).unwrap();
    let changes: BTreeMap<_, _> = (0..64)
        .map(|index| {
            (
                format!("part{index:04}"),
                format!("updated {index}").into_bytes(),
            )
        })
        .collect();
    let appended = Archive::try_new(append(&archive, &changes)).unwrap();
    let mut compacted = Vec::new();
    appended
        .compact_to(&BTreeMap::new(), &mut compacted)
        .unwrap();
    assert!(compacted.len() < appended.original.len());
    let compacted = Archive::try_new(compacted).unwrap();
    for (name, expected) in changes {
        assert_eq!(compacted.read(&name).unwrap(), expected);
    }
}

#[test]
fn stored_orphan_payload_signatures_do_not_create_false_descriptor_ambiguity() {
    let archive = Archive::try_new(fixture(Descriptor::Signed)).unwrap();
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
    let false_header = named_fixture("é.xml".as_bytes(), 0, Descriptor::Absent);
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
    let archive = Archive::try_new(combine_members([
        bytes,
        named_fixture("é.xml".as_bytes(), FLAG_UTF8, Descriptor::Absent),
        decorate_fixture(b"keep.xml", Method::Opaque),
    ]))
    .unwrap();
    assert_eq!(archive.read("part.xml").unwrap(), payload);
    let opaque_record = archive.original[archive.entries[2].local_record.clone()].to_vec();
    let changes = BTreeMap::from([("part.xml".into(), b"replacement".to_vec())]);
    let archive = Archive::try_new(append(&archive, &changes)).unwrap();
    let mut compacted = Vec::new();
    archive
        .compact_to(&BTreeMap::new(), &mut compacted)
        .unwrap();
    let archive = Archive::try_new(compacted).unwrap();
    assert_eq!(archive.read("part.xml").unwrap(), b"replacement");
    assert_eq!(
        archive.original[archive.entries[2].local_record.clone()],
        opaque_record
    );
}

#[test]
fn active_payloads_cannot_supply_obsolete_stored_descriptors() {
    for descriptor in [Descriptor::Unsigned, Descriptor::Signed] {
        let original = stored_fixture(b"part.xml", b"old", 0, descriptor);
        let old = Archive::try_new(original.clone()).unwrap();
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
                let false_header = fixture(Descriptor::Absent);
                payload.extend_from_slice(&false_header[..LOCAL_HEADER_LEN + b"part.xml".len()]);
                payload.extend_from_slice(b"remaining payload");
            }
            for opaque in [false, true] {
                let mut keep = stored_fixture(b"keep.xml", &payload, 0, Descriptor::Absent);
                if opaque {
                    let central = Archive::try_new(keep.clone()).unwrap().central_start;
                    set_u16(&mut keep, 8, 99);
                    set_u16(&mut keep, central + 10, 99);
                }
                let archive = Archive::try_new(combine(original.clone(), keep)).unwrap();
                let changes = BTreeMap::from([("part.xml".into(), b"new".to_vec())]);
                let appended = Archive::try_new(append(&archive, &changes)).unwrap();
                // In the tail case, the next real active record starts
                // immediately after the apparent descriptor.
                let keep = &appended.entries[1];
                let expected = appended.original[keep.local_record.clone()].to_vec();
                let mut compacted = Vec::new();
                appended
                    .compact_to(&BTreeMap::new(), &mut compacted)
                    .unwrap();
                assert_eq!(compacted.len(), appended.original.len() - old.central_start);
                let compacted = Archive::try_new(compacted).unwrap();
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
    for descriptor in [Descriptor::Unsigned, Descriptor::Signed] {
        let mut payload = b"prefix".to_vec();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&6u32.to_le_bytes());
        payload.extend_from_slice(&6u32.to_le_bytes());
        let false_header = fixture(Descriptor::Absent);
        payload.extend_from_slice(&false_header[..LOCAL_HEADER_LEN + b"part.xml".len()]);
        payload.extend_from_slice(b"remaining obsolete payload");
        let archive =
            Archive::try_new(stored_fixture(b"part.xml", &payload, 0, descriptor)).unwrap();
        let appended = Archive::try_new(append(
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
fn obsolete_header_candidates_with_zip64_sizes_are_payload() {
    // As above, but the false header declares a ZIP64 compressed size,
    // which no obsolete record compaction removes can have. So its
    // preceding descriptor is payload too, and only one remains.
    for descriptor in [Descriptor::Unsigned, Descriptor::Signed] {
        let mut payload = b"prefix".to_vec();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&6u32.to_le_bytes());
        payload.extend_from_slice(&6u32.to_le_bytes());
        let mut false_header =
            fixture(Descriptor::Absent)[..LOCAL_HEADER_LEN + b"part.xml".len()].to_vec();
        set_u32(&mut false_header, 18, u32::MAX);
        payload.extend_from_slice(&false_header);
        payload.extend_from_slice(b"remaining obsolete payload");
        let archive =
            Archive::try_new(stored_fixture(b"part.xml", &payload, 0, descriptor)).unwrap();
        let changes = BTreeMap::from([("part.xml".into(), b"new".to_vec())]);
        let appended = Archive::try_new(append(&archive, &changes)).unwrap();
        let mut compacted = Vec::new();
        appended
            .compact_to(&BTreeMap::new(), &mut compacted)
            .unwrap();
        assert_eq!(compacted, archive.write(&changes).unwrap());
    }
}

#[test]
fn obsolete_header_candidates_cannot_extend_into_active_records() {
    for descriptor in [Descriptor::Unsigned, Descriptor::Signed] {
        let replacement = b"new";
        let active_length = LOCAL_HEADER_LEN + b"part.xml".len() + replacement.len();
        let descriptor_length = if descriptor == Descriptor::Signed {
            SIGNED_DESCRIPTOR_LEN
        } else {
            DESCRIPTOR_LEN
        };
        let mut payload = b"prefix".to_vec();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&6u32.to_le_bytes());
        payload.extend_from_slice(&6u32.to_le_bytes());
        let mut false_header =
            fixture(Descriptor::Absent)[..LOCAL_HEADER_LEN + b"part.xml".len()].to_vec();
        // One unknown extra field would swallow the real descriptor and
        // the next active member if parsed past this obsolete gap.
        let extra_length = descriptor_length + active_length;
        set_u16(&mut false_header, 28, (4 + extra_length) as u16);
        payload.extend_from_slice(&false_header);
        payload.extend_from_slice(&0xbeefu16.to_le_bytes());
        payload.extend_from_slice(&(extra_length as u16).to_le_bytes());
        let archive =
            Archive::try_new(stored_fixture(b"part.xml", &payload, 0, descriptor)).unwrap();
        let changes = BTreeMap::from([("part.xml".into(), replacement.to_vec())]);
        let appended = Archive::try_new(append(&archive, &changes)).unwrap();
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
        let original = fixture(Descriptor::Signed);
        let archive = Archive::try_new(original.clone()).unwrap();
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
        let archive = Archive::try_new(bytes).unwrap();
        let mut compacted = Vec::new();
        archive
            .compact_to(&BTreeMap::new(), &mut compacted)
            .unwrap();
        assert_eq!(compacted, original);
    }
}

#[test]
fn compaction_refuses_opaque_prefix_gaps_and_unrecognized_orphans_without_writing() {
    let archive = Archive::try_new(fixture(Descriptor::Absent)).unwrap();
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
        let with_gap = Archive::try_new(bytes.clone()).unwrap();
        assert_eq!(with_gap.write(&BTreeMap::new()).unwrap(), bytes);
        let mut output = Vec::new();
        assert!(matches!(
            with_gap.compact_to(&BTreeMap::new(), &mut output),
            Err(Error::Unsupported(_))
        ));
        assert!(output.is_empty());
    }
    let unrelated = Archive::try_new(decorate_fixture(b"keep.xml", Method::Stored)).unwrap();
    let mut bytes = archive.original[..archive.central_start].to_vec();
    bytes.extend_from_slice(&unrelated.original[..unrelated.central_start]);
    let central_start = bytes.len();
    bytes.extend_from_slice(&archive.original[archive.central_start..archive.end]);
    let mut ending = archive.original[archive.end..].to_vec();
    set_u32(&mut ending, 16, central_start as u32);
    bytes.extend_from_slice(&ending);
    let archive = Archive::try_new(bytes).unwrap();
    let mut output = Vec::new();
    assert!(matches!(
        archive.compact_to(&BTreeMap::new(), &mut output),
        Err(Error::Unsupported(_))
    ));
    assert!(output.is_empty());
}

#[test]
fn compaction_does_not_discard_corrupt_obsolete_records() {
    // A single final stored DEFLATE block holding the original payload.
    let mut stored_block = vec![1, 7, 0, 0xf8, 0xff];
    stored_block.extend_from_slice(b"<part/>");
    let changes = BTreeMap::from([("part.xml".into(), b"updated".to_vec())]);
    // Corrupt the first payload byte of every encoding, or the checksum
    // in a streaming stored header, which must be zero or match.
    for (original, corrupt_header_byte) in [
        (fixture(Descriptor::Absent), None),
        (fixture(Descriptor::Signed), None),
        (deflate_fixture(&stored_block), None),
        (deflate_descriptor_fixture(/* signed: */ true), None),
        (fixture(Descriptor::Signed), Some(14)),
    ] {
        let archive = Archive::try_new(original).unwrap();
        let mut bytes = append(&archive, &changes);
        bytes[corrupt_header_byte.unwrap_or(archive.entries[0].data.start)] ^= 1;
        // Readers never see the obsolete record, and saves keep it.
        let archive = Archive::try_new(bytes.clone()).unwrap();
        assert_eq!(archive.read("part.xml").unwrap(), b"updated");
        assert_eq!(archive.write(&BTreeMap::new()).unwrap(), bytes);
        // Compaction cannot verify it, so it refuses to discard it.
        let mut output = Vec::new();
        assert!(matches!(
            archive.compact_to(&BTreeMap::new(), &mut output),
            Err(Error::Unsupported(message)) if message.starts_with("cannot compact")
        ));
        assert!(output.is_empty());
    }
}

#[test]
fn rejects_end_of_directory_signatures_after_the_selected_record() {
    let original = fixture(Descriptor::Absent);
    let end = original.len() - 29;
    // Signatures before the selected record, such as in stored member
    // data, are not candidates for readers that scan back from the end.
    let archive = Archive::try_new(original.clone()).unwrap();
    let embedded = [b"stored ".as_slice(), &END.to_le_bytes(), b" data"].concat();
    let changes = BTreeMap::from([("part.xml".into(), embedded.clone())]);
    let stored = Archive::try_new(archive.write(&changes).unwrap()).unwrap();
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
            Archive::try_new(bytes),
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
    let stale = Archive::try_new(fixture(Descriptor::Absent)).unwrap();
    let live = Archive::try_new(stored_fixture(
        b"part.xml",
        b"<live/>",
        0,
        Descriptor::Absent,
    ))
    .unwrap();
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
        Archive::try_new(with_decoy(|_| {})),
        Err(Error::InvalidWorkbook(message))
            if message == "ambiguous ZIP end-of-directory records"
    ));
    // A complete older archive followed by newer records puts the decoy
    // right after its directory, which then ends exactly at the record.
    let mut adjacent = stale.original[..stale.end + END_LEN].to_vec();
    set_u16(&mut adjacent, stale.end + 20, 0);
    let live_start = adjacent.len();
    adjacent.extend_from_slice(&live.original[..live.central_start]);
    let central_start = adjacent.len();
    let mut central = live.original[live.central_start..live.end].to_vec();
    set_u32(&mut central, 42, live_start as u32);
    adjacent.extend_from_slice(&central);
    let mut ending = live.original[live.end..].to_vec();
    set_u32(&mut ending, 16, central_start as u32);
    adjacent.extend_from_slice(&ending);
    assert!(matches!(
        Archive::try_new(adjacent),
        Err(Error::InvalidWorkbook(message))
            if message == "ambiguous ZIP end-of-directory records"
    ));
    // That reader cannot select these records, so they stay comment bytes
    // that neither opening nor saving refuses.
    let unreadable: [fn(&mut [u8]); 8] = [
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
        // A directory ending at 4 GiB, past the address space of 32-bit
        // targets.
        |decoy| {
            set_u32(decoy, 12, 0x8000_0000);
            set_u32(decoy, 16, 0x8000_0000);
        },
    ];
    let changes = BTreeMap::from([("part.xml".into(), b"<new/>".to_vec())]);
    for edit in unreadable {
        let archive = Archive::try_new(with_decoy(edit)).unwrap();
        assert_eq!(archive.read("part.xml").unwrap(), b"<live/>");
        let saved = Archive::try_new(archive.write(&changes).unwrap()).unwrap();
        assert_eq!(saved.read("part.xml").unwrap(), b"<new/>");
    }
}

#[test]
fn stored_embedded_archives_do_not_make_the_end_record_ambiguous() {
    // The embedded archive's own end record lies in the tail window, but
    // its directory offset is relative to that archive and finds no
    // central-directory header in this one.
    let inner = fixture(Descriptor::Absent);
    let original = combine(
        fixture(Descriptor::Absent),
        stored_fixture(b"embedded", &inner, 0, Descriptor::Absent),
    );
    let archive = Archive::try_new(original).unwrap();
    let inner_end = archive.entries[1].data.start + inner.len() - 29;
    assert_eq!(u32_at(&archive.original, inner_end).unwrap(), END);
    assert!(archive.original.len() - inner_end < END_LEN + u16::MAX as usize);
    assert_eq!(archive.read("embedded").unwrap(), inner);
    let changes = BTreeMap::from([("part.xml".into(), b"updated".to_vec())]);
    let updated = Archive::try_new(archive.write(&changes).unwrap()).unwrap();
    assert_eq!(updated.read("part.xml").unwrap(), b"updated");
    assert_eq!(updated.read("embedded").unwrap(), inner);
}

#[test]
fn rejects_empty_nul_and_duplicate_member_names() {
    // Python truncates names at NUL, and readers differ on which of two
    // duplicate members they select.
    for (bytes, expected) in [
        (
            named_fixture(b"", 0, Descriptor::Absent),
            "empty ZIP member name",
        ),
        (
            named_fixture(b"a\0b.xml", 0, Descriptor::Absent),
            "ZIP member name contains NUL: a\0b.xml",
        ),
        (
            combine(fixture(Descriptor::Absent), fixture(Descriptor::Signed)),
            "duplicate ZIP member: part.xml",
        ),
    ] {
        assert!(matches!(
            Archive::try_new(bytes),
            Err(Error::InvalidWorkbook(message)) if message == expected
        ));
    }
}

#[test]
fn rejects_member_names_with_backslashes() {
    // Some readers treat '\' as '/', so for them the second member here
    // would alias the first, and the last of the two would win.
    let mut aliased = combine(
        named_fixture(b"xl/part.xml", 0, Descriptor::Absent),
        named_fixture(b"xl_part.xml", 0, Descriptor::Absent),
    );
    // `combine` opens its parts, so respell the second name afterwards.
    let second = &Archive::try_new(aliased.clone()).unwrap().entries[1];
    for name_start in [second.local_header.start + 30, second.central.start + 46] {
        aliased[name_start + 2] = b'\\';
    }
    let message = "ZIP member name contains a backslash: xl\\part.xml";
    for bytes in [
        aliased,
        named_fixture(b"xl\\part.xml", 0, Descriptor::Absent),
    ] {
        assert!(matches!(
            Archive::try_new(bytes),
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
    let end = Archive::try_new(bytes.clone()).unwrap().end;
    bytes.truncate(end + END_LEN);
    set_u16(&mut bytes, end + 20, comment.len() as u16);
    bytes.extend_from_slice(comment);
    bytes
}

/// Emit `archive`'s directory as if zero-filled local records ended at
/// `position`, returning the result and the bytes written.
fn emit_directory_at(
    archive: &Archive,
    position: usize,
    offsets: &[u32],
    replacements: &[Option<Sizes>],
) -> (Result<()>, Vec<u8>) {
    let local_tail = vec![0; position.min(archive.local_tail_len())];
    let mut output = Vec::new();
    let mut output_position = position;
    let result = archive.emit_directory(
        &mut output,
        &mut output_position,
        &local_tail,
        offsets,
        replacements,
        |probe| {
            io::copy(&mut io::repeat(0).take(position as u64), probe)?;
            Ok(())
        },
    );
    (result, output)
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
        let archive =
            Archive::try_new(with_comment(fixture(Descriptor::Absent), &comment)).unwrap();
        let (result, output) = emit_directory_at(&archive, offset as usize, &[0], &[None]);
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
    let mut archive = Archive::try_new(input).unwrap();
    assert_eq!(archive.end - archive.central_start, 0x0605);
    set_u16(&mut archive.original, archive.end + 8, 0x4b50);
    set_u16(&mut archive.original, archive.end + 10, 0x4b50);
    assert!(shadows_end_record(&archive.original[archive.end..]));
    let (result, output) = emit_directory_at(&archive, 0x100, &[0], &[None]);
    result.unwrap();
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

fn refused_as_ambiguous(result: Result<()>) -> bool {
    matches!(
        result,
        Err(Error::Unsupported(message))
            if message.contains("would contain another end-of-directory record")
    )
}

#[test]
fn saves_refuse_rewritten_fields_that_would_form_another_end_record() {
    // A signature in a rewritten central-header field starts a complete
    // record that ends at EOF when the u16 20 bytes later counts the
    // bytes left after that record. This crate would refuse its own
    // output, so the save is refused instead.
    //
    // First the field is a new checksum, at header offset 16, and the
    // member's internal attributes, which saves copy, count the rest.
    let mut bytes = fixture(Descriptor::Absent);
    let central = Archive::try_new(bytes.clone()).unwrap().central_start;
    let rest = bytes.len() - (central + 16) - END_LEN;
    set_u16(&mut bytes, central + 36, rest as u16);
    let archive = Archive::try_new(bytes).unwrap();
    let new_sizes = |crc| {
        Some(Sizes {
            crc,
            compressed_size: 7,
            size: 7,
        })
    };
    let (result, output) = emit_directory_at(&archive, 45, &[0], &[new_sizes(END)]);
    assert!(refused_as_ambiguous(result));
    assert!(output.is_empty());
    let (result, output) = emit_directory_at(&archive, 45, &[0], &[new_sizes(END + 1)]);
    result.unwrap();
    assert_eq!(end_record_candidates(&output).count(), 1);

    // Then the field is a new local-record offset, at header offset 42,
    // and name bytes 16..18, "00", count the rest: the archive comment.
    // Local records of about 101 MB would move a member that far, so
    // emit the directory from there.
    let name = b"xl/media/image0000.png";
    let rest = u16::from_le_bytes(*b"00") as usize;
    let comment = vec![b'c'; 42 + END_LEN + rest - (CENTRAL_HEADER_LEN + name.len() + END_LEN)];
    let archive = Archive::try_new(with_comment(
        named_fixture(name, 0, Descriptor::Absent),
        &comment,
    ))
    .unwrap();
    let position = END as usize + 0x100;
    let (result, output) = emit_directory_at(&archive, position, &[END], &[None]);
    assert!(refused_as_ambiguous(result));
    assert!(output.is_empty());
    let (result, output) = emit_directory_at(&archive, position, &[END + 1], &[None]);
    result.unwrap();
    assert_eq!(end_record_candidates(&output).count(), 1);
}

#[test]
fn saves_refuse_offsets_that_would_form_a_zip64_locator() {
    // When the last central record's name, extra field and comment take
    // 16 bytes, its local-record offset is the 4 bytes 20 before the end
    // record, where readers look for a ZIP64 locator. Local records of
    // about 118 MB would move a member that far.
    for (name, offset, refused) in [
        (b"docProps/app.xml".as_slice(), ZIP64_LOCATOR, true),
        (b"docProps/app.xml", ZIP64_LOCATOR + 1, false),
        // With a 17-byte name those bytes start inside the offset.
        (b"docProps/app.xmlx", ZIP64_LOCATOR, false),
    ] {
        let archive = Archive::try_new(named_fixture(name, 0, Descriptor::Absent)).unwrap();
        let position = offset as usize + 0x100;
        let (result, output) = emit_directory_at(&archive, position, &[offset], &[None]);
        if refused {
            assert!(matches!(
                result,
                Err(Error::Unsupported(message)) if message.contains("ZIP64")
            ));
            assert!(output.is_empty());
        } else {
            result.unwrap();
            let end = output.len() - END_LEN - b"comment".len();
            assert_ne!(
                u32_at(&output, end - ZIP64_LOCATOR_LEN).unwrap(),
                ZIP64_LOCATOR
            );
        }
    }
}

/// `bytes` with `comment` as its last central-directory entry's comment.
fn with_entry_comment(bytes: Vec<u8>, comment: &[u8]) -> Vec<u8> {
    let archive = Archive::try_new(bytes).unwrap();
    let record = archive.entries.last().unwrap().central.clone();
    let old = u16_at(&archive.original, record.start + 32).unwrap() as usize;
    let mut bytes = archive.original[..record.end - old].to_vec();
    set_u16(&mut bytes, record.start + 32, comment.len() as u16);
    bytes.extend_from_slice(comment);
    let mut ending = archive.original[archive.end..].to_vec();
    set_u32(
        &mut ending,
        12,
        (bytes.len() - archive.central_start) as u32,
    );
    bytes.extend_from_slice(&ending);
    bytes
}

/// A complete end record for a one-entry directory of `size` bytes at
/// `directory`, which libzip before 1.11 selects if a central-directory
/// header starts there and the directory ends by the record.
fn decoy(directory: usize, size: usize) -> Vec<u8> {
    let mut decoy = vec![0; END_LEN];
    set_u32(&mut decoy, 0, END);
    set_u16(&mut decoy, 8, 1);
    set_u16(&mut decoy, 10, 1);
    set_u32(&mut decoy, 12, size as u32);
    set_u32(&mut decoy, 16, directory as u32);
    decoy
}

#[test]
fn saves_refuse_end_records_that_forward_scanning_readers_could_select() {
    let filler = |signature_at: usize| {
        let mut filler = vec![0; END_TAIL_LEN + 4096];
        set_u32(&mut filler, signature_at, CENTRAL);
        filler
    };
    // Replacing part.xml at offset 0 moves everything after it. Four more
    // bytes move the directory from 45 onto a decoy pointing at 49, and
    // with a long member after part.xml, its central-directory signature
    // from 183 onto a decoy pointing at 187. That decoy's directory lies
    // before the output's last END_TAIL_LEN bytes. The third decoy's
    // directory spans part.xml's central header and ends exactly where
    // the decoy, that entry's comment, starts.
    let cases = [
        with_entry_comment(fixture(Descriptor::Absent), &decoy(49, CENTRAL_HEADER_LEN)),
        with_entry_comment(
            combine(
                fixture(Descriptor::Absent),
                stored_fixture(b"fill.bin", &filler(100), 0, Descriptor::Absent),
            ),
            &decoy(
                45 + 4 + LOCAL_HEADER_LEN + b"fill.bin".len() + 100,
                CENTRAL_HEADER_LEN,
            ),
        ),
        with_entry_comment(
            fixture(Descriptor::Absent),
            &decoy(49, CENTRAL_HEADER_LEN + b"part.xml".len()),
        ),
    ];
    for original in cases {
        let archive = Archive::try_new(original).unwrap();
        assert_eq!(archive.read("part.xml").unwrap(), b"<part/>");
        for (replacement, refused) in [(b"<part/>abcd".as_slice(), true), (b"<part/>abc", false)] {
            let changes = BTreeMap::from([("part.xml".into(), replacement.to_vec())]);
            for compact in [false, true] {
                let mut output = Vec::new();
                let result = if compact {
                    archive.compact_to(&changes, &mut output)
                } else {
                    archive.write_to(&changes, &mut output)
                };
                if refused {
                    assert!(refused_as_ambiguous(result));
                    // Only the local records were written.
                    assert_eq!(output.len(), archive.central_start + 4);
                } else {
                    result.unwrap();
                    let reopened = Archive::try_new(output).unwrap();
                    assert_eq!(reopened.read("part.xml").unwrap(), replacement);
                }
            }
        }
    }
}

#[test]
fn saves_check_records_in_untouched_members_longer_than_the_kept_tail() {
    // An untouched stored member longer than the bytes the end-record
    // check keeps is written in one piece. It holds an end record whose
    // comment reaches EOF once the member after it grows by four bytes.
    let layout = |comment: u16| {
        let mut data = vec![b'x'; 70_000];
        let at = data.len() - 100;
        set_u32(&mut data, at, END);
        set_u16(&mut data, at + 20, comment);
        let bytes = combine(
            stored_fixture(b"big.bin", &data, 0, Descriptor::Absent),
            fixture(Descriptor::Absent),
        );
        (bytes, at)
    };
    let (bytes, at) = layout(0);
    let record = Archive::try_new(bytes.clone()).unwrap().entries[0]
        .data
        .start
        + at;
    let (original, _) = layout((bytes.len() + 4 - record - END_LEN) as u16);
    let archive = Archive::try_new(original).unwrap();
    assert!(archive.entries[0].local_record.len() > archive.local_tail_len());
    for (replacement, refused) in [(b"<part/>abcd".as_slice(), true), (b"<part/>abc", false)] {
        let changes = BTreeMap::from([("part.xml".into(), replacement.to_vec())]);
        let mut output = Vec::new();
        let result = archive.write_to(&changes, &mut output);
        if refused {
            assert!(refused_as_ambiguous(result));
        } else {
            result.unwrap();
            let reopened = Archive::try_new(output).unwrap();
            assert_eq!(reopened.read("part.xml").unwrap(), replacement);
        }
    }
}

#[test]
fn compaction_reads_back_earlier_output_without_obsolete_records() {
    // A pre-1.0 append save leaves part.xml's first record obsolete
    // before a long member with a central-directory signature in its
    // data. Compaction drops that 45-byte record, moving the signature
    // onto a decoy in the last entry's comment. The signature lies before
    // the output's kept tail, so only the records written again show it.
    let mut filler = vec![0; END_TAIL_LEN + 4096];
    set_u32(&mut filler, 100, CENTRAL);
    let base = Archive::try_new(combine(
        fixture(Descriptor::Absent),
        stored_fixture(b"fill.bin", &filler, 0, Descriptor::Absent),
    ))
    .unwrap();
    let changes = BTreeMap::from([("part.xml".into(), b"<part/>abcd".to_vec())]);
    let appended = append(&base, &changes);
    let moved = LOCAL_HEADER_LEN + b"fill.bin".len() + 100;
    for (directory, refused) in [(moved, true), (moved + 1, false)] {
        let original = with_entry_comment(appended.clone(), &decoy(directory, CENTRAL_HEADER_LEN));
        let archive = Archive::try_new(original).unwrap();
        let mut output = Vec::new();
        let result = archive.compact_to(&BTreeMap::new(), &mut output);
        if refused {
            assert!(refused_as_ambiguous(result));
        } else {
            result.unwrap();
            let compacted = Archive::try_new(output).unwrap();
            assert_eq!(compacted.entries[1].local_record.start, 0);
            assert_eq!(compacted.read("part.xml").unwrap(), b"<part/>abcd");
        }
    }
}

#[test]
fn kept_tails_are_the_last_bytes_written_whatever_the_write_lengths() {
    // Accepts at most three bytes per write, like a pipe.
    struct ShortWriter(Vec<u8>);
    impl Write for ShortWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let length = bytes.len().min(3);
            self.0.extend_from_slice(&bytes[..length]);
            Ok(length)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn keep(writer: &mut impl Write, len: usize, piece: usize, data: &[u8]) -> Vec<u8> {
        let mut output = KeepTail::new(writer, len);
        for chunk in data.chunks(piece) {
            output.write_all(chunk).unwrap();
        }
        output.into_tail()
    }
    let data: Vec<u8> = (0..=255).cycle().take(300).collect();
    for len in [0, 1, 10, 100] {
        let expected = &data[data.len() - len..];
        // Writes shorter than, as long as, and longer than the tail.
        for piece in [1, len.max(2) - 1, len.max(1), len + 1, data.len()] {
            let mut written = Vec::new();
            assert_eq!(keep(&mut written, len, piece, &data), expected);
            assert_eq!(written, data);
            let mut written = ShortWriter(Vec::new());
            assert_eq!(keep(&mut written, len, piece, &data), expected);
            assert_eq!(written.0, data);
        }
    }
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
    let archive = Archive::try_new(fixture(Descriptor::Absent)).unwrap();
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
        let late = [same.as_slice(), renamed.as_slice()].concat();
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
                Archive::try_new(extra_fixture(local, central)),
                Err(Error::InvalidWorkbook(message)) if message.contains("Unicode path")
            ));
        }
    }
    // Readers ignore fields whose CRC does not match the raw name or that
    // are too short to hold one, so those cannot rename a member for them.
    let same = unicode_path(name, name, 1);
    let stale = unicode_path(b"part.bin", b"evil.xml", 1);
    let short = [0x75, 0x70, 4, 0, 1, 0, 0, 0].to_vec();
    let both = [same.as_slice(), stale.as_slice(), short.as_slice()].concat();
    let changes = BTreeMap::from([("part.xml".into(), b"updated".to_vec())]);
    let versions = [0, 2].map(|version| unicode_path(name, name, version));
    let extras = [same, stale, short, both].into_iter().chain(versions);
    for extra in extras {
        let original = extra_fixture(&extra, &extra);
        let archive = Archive::try_new(original.clone()).unwrap();
        assert_eq!(archive.read("part.xml").unwrap(), b"<part/>");
        assert_eq!(archive.write(&BTreeMap::new()).unwrap(), original);
        let mut compacted = Vec::new();
        archive.compact_to(&changes, &mut compacted).unwrap();
        assert_eq!(compacted, archive.write(&changes).unwrap());
        let updated = Archive::try_new(compacted).unwrap();
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
        let archive = Archive::try_new(extra_fixture(&same, &same)).unwrap();
        let changes = BTreeMap::from([("part.xml".into(), b"updated".to_vec())]);
        let appended = append(&archive, &changes);
        let mut compacted = Vec::new();
        Archive::try_new(appended.clone())
            .unwrap()
            .compact_to(&BTreeMap::new(), &mut compacted)
            .unwrap();
        assert_eq!(
            Archive::try_new(compacted)
                .unwrap()
                .read("part.xml")
                .unwrap(),
            b"updated"
        );
        // The obsolete record at offset 0 is opaque to normal saves, but
        // compaction must parse it and so applies the same name rule.
        let mut renamed = appended;
        let unicode_name = 30 + b"part.xml".len() + 9;
        renamed[unicode_name..unicode_name + 8].copy_from_slice(b"evil.xml");
        let archive = Archive::try_new(renamed.clone()).unwrap();
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
    assert_eq!(read_at_most(&input[..], None, 10).unwrap(), input);
    let too_long = |result: Result<Vec<u8>>| {
        matches!(
            result,
            Err(Error::Unsupported(message))
                if message == "input exceeds the largest possible ZIP32 archive"
        )
    };
    assert!(too_long(read_at_most(&input[..], None, 9)));
    // An endless stream is not read beyond the limit.
    assert!(too_long(read_at_most(io::repeat(0), None, 1 << 16)));
    // A file length over the limit is refused before reading, although
    // these 10 bytes would fit. One within the limit only reserves
    // capacity, so a stale length changes neither the result nor the limit.
    assert!(too_long(read_at_most(&input[..], Some(11), 10)));
    assert_eq!(read_at_most(&input[..], Some(10), 10).unwrap(), input);
    assert_eq!(read_at_most(&input[..], Some(4), 10).unwrap(), input);
    assert!(too_long(read_at_most(io::repeat(0), Some(4), 10)));
    assert_eq!(MAX_ARCHIVE_LEN, 2 * u64::from(u32::MAX) + 22 + 65535);
}

#[test]
fn rejects_ole_compound_files_as_unsupported() {
    let mut bytes = vec![0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
    bytes.resize(4096, 0);
    assert!(matches!(
        Archive::try_new(bytes),
        Err(Error::Unsupported(message)) if message.contains("OLE compound files")
    ));
    assert!(matches!(
        Archive::try_new(vec![0; 4096]),
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
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }
    let archive = Archive::try_new(fixture(Descriptor::Signed)).unwrap();
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
    let central = Archive::try_new(fixture(Descriptor::Absent))
        .unwrap()
        .entries[0]
        .central
        .start;
    let flagged = |flags: u16| {
        let mut bytes = fixture(Descriptor::Absent);
        set_u16(&mut bytes, 6, flags);
        set_u16(&mut bytes, central + 8, flags);
        Archive::try_new(bytes)
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
        for descriptor in [Descriptor::Absent, Descriptor::Signed] {
            let original = named_fixture(name.as_bytes(), flags, descriptor);
            let archive = Archive::try_new(original.clone()).unwrap();
            assert_eq!(archive.read(name).unwrap(), b"<part/>");
            assert_eq!(archive.write(&BTreeMap::new()).unwrap(), original);
            let changes = BTreeMap::from([(name.into(), b"updated".to_vec())]);
            let appended = Archive::try_new(append(&archive, &changes)).unwrap();
            let mut compacted = Vec::new();
            appended
                .compact_to(&BTreeMap::new(), &mut compacted)
                .unwrap();
            assert_eq!(compacted, archive.write(&changes).unwrap());
            let updated = Archive::try_new(compacted).unwrap();
            assert_eq!(updated.read(name).unwrap(), b"updated");
            assert_eq!(updated.entries[0].flags & FLAG_UTF8, flags);
        }
    }
    // An explicitly encoded active member remains opaque when its method
    // is unsupported; compaction preserves it without decoding its data.
    let mut bytes = named_fixture("é.xml".as_bytes(), FLAG_UTF8, Descriptor::Absent);
    let central = Archive::try_new(bytes.clone()).unwrap().central_start;
    set_u16(&mut bytes, 8, 99);
    set_u16(&mut bytes, central + 10, 99);
    let archive = Archive::try_new(bytes.clone()).unwrap();
    let mut compacted = Vec::new();
    archive
        .compact_to(&BTreeMap::new(), &mut compacted)
        .unwrap();
    assert_eq!(compacted, bytes);
}

#[test]
fn rejects_ambiguous_name_encodings_and_conflicting_encoding_flags() {
    for descriptor in [Descriptor::Absent, Descriptor::Signed] {
        for name in ["é.xml".as_bytes(), b"\xe9.xml"] {
            assert!(matches!(
                Archive::try_new(named_fixture(name, 0, descriptor)),
                Err(Error::Unsupported(message))
                    if message == "non-ASCII ZIP member names require the UTF-8 flag"
            ));
        }
        assert!(matches!(
            Archive::try_new(named_fixture(b"\xe9.xml", FLAG_UTF8, descriptor)),
            Err(Error::Unsupported(message)) if message == "ZIP member names must be UTF-8"
        ));
        let original = named_fixture("é.xml".as_bytes(), FLAG_UTF8, descriptor);
        let central = Archive::try_new(original.clone()).unwrap().central_start;
        for flag_at in [6, central + 8] {
            let mut bytes = original.clone();
            let flags = u16_at(&bytes, flag_at).unwrap() & !FLAG_UTF8;
            set_u16(&mut bytes, flag_at, flags);
            let result = Archive::try_new(bytes);
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
    for descriptor in [Descriptor::Absent, Descriptor::Signed] {
        let archive =
            Archive::try_new(named_fixture(name.as_bytes(), FLAG_UTF8, descriptor)).unwrap();
        let changes = BTreeMap::from([(name.into(), b"updated".to_vec())]);
        let mut bytes = append(&archive, &changes);
        // Only the obsolete record loses its encoding flag. Normal saves
        // preserve it, while compaction must understand it before removal.
        let flags = u16_at(&bytes, 6).unwrap() & !FLAG_UTF8;
        set_u16(&mut bytes, 6, flags);
        let archive = Archive::try_new(bytes.clone()).unwrap();
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
    let archive = Archive::try_new(valid.clone()).unwrap();
    assert_eq!(archive.read("part.xml").unwrap(), b"<part/>");
    let central = archive.entries[0].central.start;
    let mut oversized = valid;
    set_u32(&mut oversized, 22, MAX_PART_SIZE as u32 + 1);
    set_u32(&mut oversized, central + 24, MAX_PART_SIZE as u32 + 1);
    assert!(matches!(
        Archive::try_new(oversized).unwrap().read("part.xml"),
        Err(Error::Unsupported(message))
            if message == "ZIP member part.xml exceeds the 64 MiB read limit"
    ));
}

#[test]
fn rejects_overlapping_local_records() {
    let keep = Archive::try_new(decorate_fixture(b"keep.xml", Method::Stored)).unwrap();
    let record = keep.original[keep.entries[0].local_record.clone()].to_vec();
    let separate = combine(
        fixture(Descriptor::Absent),
        decorate_fixture(b"keep.xml", Method::Stored),
    );
    assert_eq!(Archive::try_new(separate).unwrap().entries.len(), 2);
    // A stored payload that is itself the other member's complete record.
    let part = Archive::try_new(fixture(Descriptor::Absent)).unwrap();
    let changes = BTreeMap::from([("part.xml".into(), record)]);
    let part = Archive::try_new(part.write(&changes).unwrap()).unwrap();
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
        Archive::try_new(bytes),
        Err(Error::InvalidWorkbook(message)) if message == "overlapping ZIP local records"
    ));
}
