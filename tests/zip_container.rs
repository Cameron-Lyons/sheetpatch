mod common;

use sheetpatch::{Error, Workbook};
use std::io::{self, BufWriter, Write};

const FIXTURE: &[u8] = include_bytes!("fixtures/libreoffice.xlsx");

#[test]
fn ambiguous_legacy_zip_names_cannot_select_a_different_worksheet() {
    // The UTF-8 bytes of é mean ├⌐ in ZIP's legacy CP437 encoding. Without
    // bit 11, the second entry aliases the first for a legacy-name reader.
    const TARGET: &str = "xl/worksheets/├⌐.xml";
    const LEGACY: &str = "xl/worksheets/é.xml";
    const ENCODED: &str = "worksheets/%E2%94%9C%E2%8C%90.xml";
    const MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    let mut parts = common::payloads(FIXTURE);
    parts.remove("xl/worksheets/sheet1.xml");
    for name in ["xl/_rels/workbook.xml.rels", "[Content_Types].xml"] {
        let xml = String::from_utf8(parts[name].clone()).unwrap();
        parts.insert(
            name.to_owned(),
            xml.replace("worksheets/sheet1.xml", ENCODED).into_bytes(),
        );
    }
    let sheet = |value| {
        format!(
            "<worksheet xmlns='{MAIN}'><sheetData><row r='1'><c r='A1'><v>{value}</v></c></row></sheetData></worksheet>"
        )
    };
    let target = sheet(2);
    let legacy = sheet(1);
    let mut entries: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(name, data)| (name.as_str(), data.as_slice()))
        .collect();
    entries.extend([(TARGET, target.as_bytes()), (LEGACY, legacy.as_bytes())]);
    let mut bytes = common::stored_archive(&entries);
    let mut legacy_headers = None;
    let mut central = common::central_directory_offset(&bytes);
    for (name, _) in &entries {
        let local = common::u32_at(&bytes, central + 42) as usize;
        if !name.is_ascii() {
            bytes[local + 6..local + 8].copy_from_slice(&0x0800u16.to_le_bytes());
            bytes[central + 8..central + 10].copy_from_slice(&0x0800u16.to_le_bytes());
        }
        if *name == LEGACY {
            legacy_headers = Some((local, central));
        }
        central += 46
            + common::u16_at(&bytes, central + 28)
            + common::u16_at(&bytes, central + 30)
            + common::u16_at(&bytes, central + 32);
    }
    let mut book = Workbook::from_bytes(bytes.clone()).unwrap();
    assert_eq!(book.get_cell("Data", "A1").unwrap().value, 2.into());
    assert_eq!(book.to_bytes().unwrap(), bytes);
    book.set_cell("Data", "A1", 3).unwrap();
    for saved in [book.to_bytes().unwrap(), book.to_bytes_compact().unwrap()] {
        assert_eq!(common::payload(&saved, LEGACY), legacy.as_bytes());
        let reopened = Workbook::from_bytes(saved).unwrap();
        assert_eq!(reopened.get_cell("Data", "A1").unwrap().value, 3.into());
    }

    let (local, central) = legacy_headers.unwrap();
    bytes[local + 6..local + 8].copy_from_slice(&0u16.to_le_bytes());
    bytes[central + 8..central + 10].copy_from_slice(&0u16.to_le_bytes());
    assert!(matches!(
        Workbook::from_bytes(bytes),
        Err(Error::Unsupported(message)) if message.contains("UTF-8 flag")
    ));
}

/// Accepts at most `quota` bytes and then fails, like a full disk.
struct Quota {
    bytes: Vec<u8>,
    quota: usize,
}

impl Write for Quota {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = self.quota - self.bytes.len();
        if room == 0 && !bytes.is_empty() {
            return Err(io::Error::other("quota exceeded"));
        }
        let length = bytes.len().min(room);
        self.bytes.extend_from_slice(&bytes[..length]);
        Ok(length)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn save(book: &Workbook, compact: bool, writer: impl Write) -> sheetpatch::Result<()> {
    if compact {
        book.write_compact_to(writer)
    } else {
        book.write_to(writer)
    }
}

#[test]
fn by_value_buffered_writers_report_a_failing_final_write() {
    let unchanged = Workbook::from_bytes(FIXTURE.to_vec()).unwrap();
    let mut edited = Workbook::from_bytes(FIXTURE.to_vec()).unwrap();
    let sheet = edited.sheets()[0].name().to_owned();
    edited.set_cell(&sheet, "Z100", 5.0).unwrap();
    for book in [&unchanged, &edited] {
        for compact in [false, true] {
            let expected = if compact {
                book.to_bytes_compact()
            } else {
                book.to_bytes()
            }
            .unwrap();
            let mut complete = Quota {
                bytes: Vec::new(),
                quota: usize::MAX,
            };
            save(book, compact, BufWriter::new(&mut complete)).unwrap();
            assert_eq!(complete.bytes, expected);
            // The buffered tail, written when the moved writer is flushed or
            // dropped, is the write that fails. The default buffer holds the
            // whole small fixture; the larger one also covers the fast path.
            for capacity in [8 * 1024, 1024 * 1024] {
                let mut full = Quota {
                    bytes: Vec::new(),
                    quota: expected.len() - 1,
                };
                let writer = BufWriter::with_capacity(capacity, &mut full);
                assert!(matches!(
                    save(book, compact, writer),
                    Err(Error::Io(error)) if error.to_string() == "quota exceeded"
                ));
                assert_eq!(full.bytes, expected[..expected.len() - 1]);
            }
        }
    }
}

#[test]
fn ole_compound_files_are_unsupported_rather_than_invalid() {
    // Legacy .xls files and password-encrypted OOXML packages both start
    // with the OLE compound file signature instead of a ZIP record.
    let mut bytes = vec![0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];
    bytes.resize(4096, 0);
    assert!(matches!(
        Workbook::from_bytes(bytes),
        Err(Error::Unsupported(message))
            if message.contains(".xls") && message.contains("password-encrypted")
    ));
    assert!(matches!(
        Workbook::from_bytes(vec![0; 4096]),
        Err(Error::InvalidWorkbook(_))
    ));
}
