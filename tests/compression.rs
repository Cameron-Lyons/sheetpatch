//! Compression limits apply to every public save path without changing bytes.
mod common;

use common::TempDir;
use sheetpatch::Workbook;
use std::num::NonZeroUsize;
use std::{fs, thread};

const FIXTURE: &[u8] = include_bytes!("fixtures/libreoffice.xlsx");

fn edited_workbook() -> Workbook {
    let mut workbook = Workbook::from_bytes(FIXTURE.to_vec()).unwrap();
    let text = "x".repeat(32_767);
    workbook
        .set_cells(
            "Data",
            (1000..1096).map(|row| (format!("Z{row}"), text.as_str())),
        )
        .unwrap();
    workbook
}

#[test]
fn all_save_paths_honor_a_deterministic_compression_limit() {
    let directory = TempDir::new();
    let mut workbook = edited_workbook();
    assert_eq!(workbook.compression_threads(), None);
    let expected = workbook.to_bytes().unwrap();
    let worksheet = workbook.sheet("Data").unwrap().path();
    assert!(common::payload(&expected, worksheet).len() > 2 * 1024 * 1024);

    for limit in [NonZeroUsize::new(1), NonZeroUsize::new(2), None] {
        workbook.set_compression_threads(limit);
        assert_eq!(workbook.compression_threads(), limit);
        assert!(workbook.has_changes());
        assert_eq!(workbook.to_bytes().unwrap(), expected);
        assert_eq!(workbook.to_bytes_compact().unwrap(), expected);
        let mut streamed = Vec::new();
        workbook.write_to(&mut streamed).unwrap();
        assert_eq!(streamed, expected);
        streamed.clear();
        workbook.write_compact_to(&mut streamed).unwrap();
        assert_eq!(streamed, expected);
        let saved = directory.join("saved.xlsx");
        workbook.save(&saved).unwrap();
        assert_eq!(fs::read(&saved).unwrap(), expected);
        workbook.save_compact(&saved).unwrap();
        assert_eq!(fs::read(&saved).unwrap(), expected);
    }
    let reopened = Workbook::from_bytes(expected).unwrap();
    assert_eq!(
        reopened.get_cell("Data", "Z1095").unwrap().value.as_text(),
        Some("x".repeat(32_767).as_str())
    );
    workbook.set_compression_threads(NonZeroUsize::new(1));
    workbook.reset_changes();
    assert_eq!(workbook.compression_threads(), NonZeroUsize::new(1));
    assert_eq!(workbook.to_bytes().unwrap(), FIXTURE);
}

#[test]
fn concurrent_saves_keep_independent_compression_limits() {
    let workbooks: Vec<_> = [NonZeroUsize::new(1), NonZeroUsize::new(2), None]
        .into_iter()
        .map(|limit| {
            let mut workbook = edited_workbook();
            workbook.set_compression_threads(limit);
            workbook
        })
        .collect();
    let expected = workbooks[0].to_bytes().unwrap();
    thread::scope(|scope| {
        let saves: Vec<_> = workbooks
            .iter()
            .map(|workbook| scope.spawn(|| workbook.to_bytes().unwrap()))
            .collect();
        for save in saves {
            assert_eq!(save.join().unwrap(), expected);
        }
    });
}
