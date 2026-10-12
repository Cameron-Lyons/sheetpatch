use sheetpatch::{Error, Workbook};
use std::io::{self, BufWriter, Write};

const FIXTURE: &[u8] = include_bytes!("fixtures/libreoffice.xlsx");

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
