//! Reject a selected allocation in a child process, so an infallible
//! allocation regression fails this test instead of aborting the whole suite.

mod common;

use sheetpatch::{CellEdit, CellRef, CellValue, Error, Workbook};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::ErrorKind;
use std::process::Command;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::{env, ptr, thread};

// Deliberately unlike the sizes of the address/result vectors and XML arenas.
const TEXT_BYTES: usize = 65_537;
const CHILD_CASE: &str = "SHEETPATCH_ALLOCATION_CASE";
// Cells of the "shared_cells" and "shared_table" fixtures, which all use the
// table's one entry. In "shared_table" they are an Excel table's header row.
const SHARED_CELLS: u16 = 40;
static FAIL_NEXT_TEXT: AtomicBool = AtomicBool::new(false);
// Refuses every such allocation while set, counting the refusals.
static REFUSE_TEXT: AtomicBool = AtomicBool::new(false);
static REFUSALS: AtomicUsize = AtomicUsize::new(0);
// Every allocation of exactly TEXT_BYTES, refused or not.
static TEXT_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
// While nonzero, also refuses every allocation of at least this many bytes.
static REFUSE_FROM: AtomicUsize = AtomicUsize::new(0);

struct FailOnce;

fn reject(size: usize) -> bool {
    if size == TEXT_BYTES {
        TEXT_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    }
    let limit = REFUSE_FROM.load(Ordering::SeqCst);
    let refused = (limit != 0 && size >= limit)
        || size == TEXT_BYTES
            && (REFUSE_TEXT.load(Ordering::SeqCst) || FAIL_NEXT_TEXT.swap(false, Ordering::SeqCst));
    if refused {
        REFUSALS.fetch_add(1, Ordering::SeqCst);
    }
    refused
}

// SAFETY: Every successful allocation and every deallocation is delegated to
// System with the original layout. A rejected allocation returns null, as the
// allocator contract permits. The failure switches and the counters are atomic
// and allocate nothing.
unsafe impl GlobalAlloc for FailOnce {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if reject(layout.size()) {
            return ptr::null_mut();
        }
        // SAFETY: The caller supplies the valid layout required by System.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if reject(layout.size()) {
            return ptr::null_mut();
        }
        // SAFETY: The caller supplies the valid layout required by System.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: Successful allocations come from System; the caller supplies
        // the same live pointer and layout used to allocate them.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if reject(new_size) {
            return ptr::null_mut();
        }
        // SAFETY: The pointer comes from System, and the caller supplies its
        // original layout and a valid new size.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: FailOnce = FailOnce;

fn run_child(test: &str, case: &str) {
    let output = Command::new(env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD_CASE, case)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{test} ({case}) failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(case: &str) -> Vec<u8> {
    let text = "x".repeat(TEXT_BYTES);
    let cell = match case {
        "shared" => "<c r='A1' t='s'><v>0</v></c>".to_owned(),
        "shared_cells" | "shared_table" => (1..=SHARED_CELLS)
            .map(|column| {
                let cell = CellRef::try_new(1, column).unwrap();
                format!("<c r='{cell}' t='s'><v>0</v></c>")
            })
            .collect(),
        "inline" => format!("<c r='A1' t='inlineStr'><is><t>{text}</t></is></c>"),
        // The spelling's own capacity, TEXT_BYTES, is reserved first.
        "escaped" => format!(
            "<c r='A1' t='inlineStr'><is><t>_x0041_{}</t></is></c>",
            &text[7..]
        ),
        // Initial capacity is TEXT_BYTES + 6; fail only the allocation that
        // right-sizes the decoded result after its escape has been shortened.
        "decoded" => format!(
            "<c r='A1' t='inlineStr'><is><t>_x0041_{}</t></is></c>",
            &text[1..]
        ),
        "formula" => format!("<c r='A1'><f>{text}</f><v>1</v></c>"),
        "string" => format!("<c r='A1' t='str'><v>{text}</v></c>"),
        "error" => format!("<c r='A1' t='e'><v>{text}</v></c>"),
        "markup" => format!("<c r='A1' t='str'><v><![CDATA[{text}]]></v></c>"),
        "numeric-spelling" => format!(
            "<c r='A1' t='n'><v><![CDATA[{}1e0]]></v></c>",
            " ".repeat(TEXT_BYTES - 3)
        ),
        "rich" => format!(
            "<c r='A1' t='inlineStr'><is><r><t>x</t></r><r><t>{}</t></r></is></c>",
            &text[1..]
        ),
        _ => panic!("unknown allocation case {case}"),
    };
    let table = case == "shared_table";
    let table_parts = if table {
        "<tableParts count='1'><tablePart r:id='table'/></tableParts>"
    } else {
        ""
    };
    let worksheet = format!(
        "<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' xmlns:r='http://schemas.openxmlformats.org/officeDocument/2006/relationships'><sheetData><row r='1'>{cell}</row></sheetData>{table_parts}</worksheet>"
    );
    let strings = format!(
        "<sst xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><si><t>{text}</t></si></sst>"
    );
    let last = CellRef::try_new(2, SHARED_CELLS).unwrap();
    let table_xml = format!(
        "<table xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' id='1' name='T' displayName='T' ref='A1:{last}'/>"
    );
    let mut parts: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>"),
        ("_rels/.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='office' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ("xl/workbook.xml", b"<workbook xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' xmlns:r='http://schemas.openxmlformats.org/officeDocument/2006/relationships'><sheets><sheet name='Data' sheetId='1' r:id='sheet'/></sheets></workbook>"),
        ("xl/_rels/workbook.xml.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='sheet' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet' Target='worksheets/sheet1.xml'/><Relationship Id='strings' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings' Target='sharedStrings.xml'/></Relationships>"),
        ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
        ("xl/sharedStrings.xml", strings.as_bytes()),
    ];
    if table {
        parts.extend([
            ("xl/worksheets/_rels/sheet1.xml.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='table' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/table' Target='../tables/table1.xml'/></Relationships>".as_slice()),
            ("xl/tables/table1.xml", table_xml.as_bytes()),
        ]);
    }
    common::stored_archive(&parts)
}

#[test]
fn deflated_parts_retry_after_a_refused_output_allocation() {
    if env::var(CHILD_CASE).as_deref() == Ok("inflate") {
        for path in ["xl/worksheets/sheet1.xml", "xl/sharedStrings.xml"] {
            let mut parts = common::payloads(include_bytes!("fixtures/libreoffice.xlsx"));
            // The inflater reserves the declared size plus one to detect an
            // overlong stream. Trailing XML whitespace reaches that exact size.
            parts.get_mut(path).unwrap().resize(TEXT_BYTES - 1, b' ');
            let entries: Vec<_> = parts
                .iter()
                .map(|(name, xml)| (name.as_str(), xml.as_slice()))
                .collect();
            let bytes = common::write_zip(&entries, &common::ZipLayout::deflated(1));
            let book = Workbook::from_bytes(bytes).unwrap();
            FAIL_NEXT_TEXT.store(true, Ordering::SeqCst);
            let result = book.get_cell("Data", "A1");
            assert!(
                matches!(result, Err(Error::Io(error)) if error.kind() == ErrorKind::OutOfMemory),
                "{path}"
            );
            assert_eq!(
                book.get_cell("Data", "A1").unwrap().value.as_text(),
                Some("Original text")
            );
        }
        return;
    }
    run_child(
        "deflated_parts_retry_after_a_refused_output_allocation",
        "inflate",
    );
}

#[test]
fn concurrent_readers_inflate_a_worksheet_once() {
    if env::var(CHILD_CASE).as_deref() != Ok("worksheet-inflate") {
        run_child(
            "concurrent_readers_inflate_a_worksheet_once",
            "worksheet-inflate",
        );
        return;
    }
    let mut parts = common::payloads(include_bytes!("fixtures/libreoffice.xlsx"));
    // The output allocation includes a byte for detecting oversized streams.
    parts
        .get_mut("xl/worksheets/sheet1.xml")
        .unwrap()
        .resize(TEXT_BYTES - 1, b' ');
    let entries: Vec<_> = parts
        .iter()
        .map(|(name, xml)| (name.as_str(), xml.as_slice()))
        .collect();
    let bytes = common::write_zip(&entries, &common::ZipLayout::deflated(1));
    let book = Workbook::from_bytes(bytes).unwrap();
    let readers = Barrier::new(8);
    TEXT_ATTEMPTS.store(0, Ordering::SeqCst);
    thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                readers.wait();
                assert_eq!(book.get_cell("Data", "B1").unwrap().value, 12.5.into());
            });
        }
    });
    assert_eq!(TEXT_ATTEMPTS.load(Ordering::SeqCst), 1);
}

#[test]
fn refused_worksheet_output_rolls_back_a_multi_sheet_edit() {
    if env::var(CHILD_CASE).as_deref() != Ok("edit-output") {
        run_child(
            "refused_worksheet_output_rolls_back_a_multi_sheet_edit",
            "edit-output",
        );
        return;
    }
    let mut parts = common::payloads(include_bytes!("fixtures/libreoffice.xlsx"));
    // Editing 7 to 8 keeps the XML length. Only the final output for the
    // second worksheet needs this allocation size.
    let mut sheet =
        b"<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'>\
        <sheetData><row r='1'><c r='A1'><v>7</v></c></row></sheetData></worksheet>"
            .to_vec();
    sheet.resize(TEXT_BYTES, b' ');
    parts.insert("xl/worksheets/sheet2.xml".into(), sheet);
    let entries: Vec<_> = parts
        .iter()
        .map(|(name, xml)| (name.as_str(), xml.as_slice()))
        .collect();
    let mut book = Workbook::from_bytes(common::stored_archive(&entries)).unwrap();
    book.set_cell("Data", "B1", 19).unwrap();
    let before = book.to_bytes().unwrap();
    let edits = [
        CellEdit::try_new("Data", "B1", 23).unwrap(),
        CellEdit::try_new("Notes", "A1", 8).unwrap(),
    ];
    let first_attempt = edits.clone();
    FAIL_NEXT_TEXT.store(true, Ordering::SeqCst);
    let result = book.apply_edits(first_attempt);
    assert!(matches!(result, Err(Error::Io(error)) if error.kind() == ErrorKind::OutOfMemory));
    assert_eq!(book.to_bytes().unwrap(), before);
    book.apply_edits(edits).unwrap();
    let saved = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    assert_eq!(
        saved.get_cell("Data", "B1").unwrap().value.as_number(),
        Some(23.0)
    );
    assert_eq!(
        saved.get_cell("Notes", "A1").unwrap().value.as_number(),
        Some(8.0)
    );
}

#[test]
fn read_result_allocations_report_out_of_memory() {
    if let Ok(case) = env::var(CHILD_CASE) {
        let workbook = Workbook::from_bytes(fixture(&case)).unwrap();
        let view = workbook.prepare_sheet("Data").unwrap();
        let cell = CellRef::try_new(1, 1).unwrap();
        // Load shared strings before rejecting only the result's own copy.
        let expected = view.get_cell_at(cell).unwrap();
        FAIL_NEXT_TEXT.store(true, Ordering::SeqCst);
        let result = view.get_cells_at([cell, cell]);
        let armed = FAIL_NEXT_TEXT.swap(false, Ordering::SeqCst);
        assert!(!armed, "the test must exercise the selected allocation");
        assert!(
            matches!(result, Err(Error::Io(error)) if error.kind() == ErrorKind::OutOfMemory),
            "a refused result allocation must return OutOfMemory"
        );
        assert_eq!(view.get_cell_at(cell).unwrap(), expected);
        assert!(!workbook.has_changes());
        return;
    }

    for case in [
        "shared", "inline", "escaped", "decoded", "formula", "string", "error", "markup", "rich",
    ] {
        run_child("read_result_allocations_report_out_of_memory", case);
    }
}

#[test]
fn refused_value_comparison_preserves_the_cell_spelling() {
    let case = "numeric-spelling";
    if env::var(CHILD_CASE).as_deref() != Ok(case) {
        run_child("refused_value_comparison_preserves_the_cell_spelling", case);
        return;
    }

    let mut book = Workbook::from_bytes(fixture(case)).unwrap();
    book.set_cell("Data", "B1", 7).unwrap();
    let before = book.to_bytes().unwrap();
    FAIL_NEXT_TEXT.store(true, Ordering::SeqCst);
    let result = book.set_cell("Data", "A1", 1);
    assert!(!FAIL_NEXT_TEXT.swap(false, Ordering::SeqCst));
    assert!(matches!(result, Err(Error::Io(error)) if error.kind() == ErrorKind::OutOfMemory));
    assert_eq!(book.to_bytes().unwrap(), before);
    // With enough memory, comparing the same number preserves its original
    // CDATA, whitespace, and scientific notation as well as the pending edit.
    book.set_cell("Data", "A1", 1).unwrap();
    assert_eq!(book.to_bytes().unwrap(), before);
}

#[test]
fn changing_scalar_type_does_not_copy_the_old_value() {
    if let Ok(case) = env::var(CHILD_CASE) {
        let mut workbook = Workbook::from_bytes(fixture(&case)).unwrap();
        let replacement = if case == "error" {
            CellValue::Bool(true)
        } else {
            CellValue::Number(7.0)
        };
        FAIL_NEXT_TEXT.store(true, Ordering::SeqCst);
        let result = workbook.set_cell("Data", "A1", replacement.clone());
        let armed = FAIL_NEXT_TEXT.swap(false, Ordering::SeqCst);
        result.unwrap();
        assert!(
            armed,
            "changing scalar type must not allocate a copy of the old text"
        );
        assert_eq!(workbook.get_cell("Data", "A1").unwrap().value, replacement);
        return;
    }

    for case in [
        "shared", "inline", "escaped", "decoded", "string", "error", "markup",
    ] {
        run_child("changing_scalar_type_does_not_copy_the_old_value", case);
    }
}

// Compaction refuses an archive with opaque bytes before its directory, and
// must do so before reserving its output, about the archive's size, which an
// infallible reservation would abort the process for.
#[test]
fn a_refused_compaction_reserves_no_output() {
    if env::var(CHILD_CASE).as_deref() == Ok("compact") {
        let mut bytes = fixture("inline");
        let central = common::central_directory_offset(&bytes);
        bytes.splice(central..central, *b"opaque bytes before the directory");
        let end = common::end_of_central_directory(&bytes);
        let offset = u32::try_from(central + 33).unwrap();
        bytes[end + 16..end + 20].copy_from_slice(&offset.to_le_bytes());
        let workbook = Workbook::from_bytes(bytes).unwrap();
        REFUSE_FROM.store(TEXT_BYTES, Ordering::SeqCst);
        let result = workbook.to_bytes_compact();
        REFUSE_FROM.store(0, Ordering::SeqCst);
        assert!(
            matches!(&result, Err(Error::Unsupported(message)) if message.starts_with("cannot compact")),
            "{result:?}"
        );
        assert_eq!(REFUSALS.load(Ordering::SeqCst), 0, "no output was reserved");
        return;
    }

    run_child("a_refused_compaction_reserves_no_output", "compact");
}

// A refused allocation is transient, unlike a damaged part, so a table whose
// load ran out of memory loads again on its next use.
#[test]
fn a_shared_string_table_that_ran_out_of_memory_loads_again() {
    if env::var(CHILD_CASE).as_deref() == Ok("table") {
        let workbook = Workbook::from_bytes(fixture("shared")).unwrap();
        let cell = CellRef::try_new(1, 1).unwrap();
        FAIL_NEXT_TEXT.store(true, Ordering::SeqCst);
        let refused = workbook.get_cell_at("Data", cell);
        let armed = FAIL_NEXT_TEXT.swap(false, Ordering::SeqCst);
        assert!(
            !armed,
            "loading the table must exercise the refused allocation"
        );
        assert!(
            matches!(refused, Err(Error::Io(error)) if error.kind() == ErrorKind::OutOfMemory),
            "a refused table allocation must return OutOfMemory"
        );
        let content = workbook.get_cell_at("Data", cell).unwrap();
        assert_eq!(
            content.value.as_text(),
            Some("x".repeat(TEXT_BYTES).as_str())
        );
        return;
    }

    run_child(
        "a_shared_string_table_that_ran_out_of_memory_loads_again",
        "table",
    );
}

// While its allocation keeps being refused, a table is tried once per call or
// iteration pass, not once for each cell that needs it, and later calls still
// try again.
#[test]
fn a_shared_string_table_that_cannot_be_loaded_is_tried_once_per_call() {
    if env::var(CHILD_CASE).as_deref() == Ok("per-call") {
        let mut workbook = Workbook::from_bytes(fixture("shared_cells")).unwrap();
        let mut with_table = Workbook::from_bytes(fixture("shared_table")).unwrap();
        fn out_of_memory<T>(result: &sheetpatch::Result<T>) -> bool {
            matches!(result, Err(Error::Io(error)) if error.kind() == ErrorKind::OutOfMemory)
        }
        REFUSE_TEXT.store(true, Ordering::SeqCst);

        // A pass that continues past errors reports every cell's failure.
        let view = workbook.prepare_sheet("Data").unwrap();
        let results: Vec<_> = view.cell_contents().collect();
        assert_eq!(results.len(), usize::from(SHARED_CELLS));
        assert!(results.iter().all(out_of_memory));
        assert_eq!(REFUSALS.swap(0, Ordering::SeqCst), 1);
        // Another pass, and another call, try again.
        assert!(view.cell_contents().all(|result| out_of_memory(&result)));
        assert_eq!(REFUSALS.swap(0, Ordering::SeqCst), 1);
        assert!(out_of_memory(&view.get_cell("A1")));
        assert_eq!(REFUSALS.swap(0, Ordering::SeqCst), 1);
        // A pass that began before another pass failed does not try again
        // either.
        {
            let mut first = view.cell_contents();
            let mut second = view.cell_contents();
            assert!(out_of_memory(&first.next().unwrap()));
            assert!(second.all(|result| out_of_memory(&result)));
        }
        assert_eq!(REFUSALS.swap(0, Ordering::SeqCst), 1);
        drop(view);

        // A batch compares every shared-string cell against the table, which
        // it tries once; without it, the edits are changes.
        let cells = (1..=SHARED_CELLS).map(|column| (CellRef::try_new(1, column).unwrap(), "y"));
        workbook.set_cells_at("Data", cells).unwrap();
        assert_eq!(REFUSALS.swap(0, Ordering::SeqCst), 1);
        assert!(workbook.has_changes());

        // Checking edits of table header cells needs the table, so they fail,
        // still after only one try: the header check reuses the outcome.
        let headers = (1..=SHARED_CELLS).map(|column| (CellRef::try_new(1, column).unwrap(), "y"));
        assert!(out_of_memory(&with_table.set_cells_at("Data", headers)));
        assert_eq!(REFUSALS.swap(0, Ordering::SeqCst), 1);
        assert!(!with_table.has_changes());

        // A pass that began before a failure reads the table once a later call
        // has loaded it, rather than that failure.
        workbook.reset_changes();
        {
            let view = workbook.prepare_sheet("Data").unwrap();
            let mut pass = view.cell_contents();
            assert!(out_of_memory(&view.get_cell("A1")));
            assert_eq!(REFUSALS.swap(0, Ordering::SeqCst), 1);
            REFUSE_TEXT.store(false, Ordering::SeqCst);
            assert!(view.get_cell("A1").is_ok());
            assert!(pass.all(|result| result.is_ok()));
            REFUSE_TEXT.store(true, Ordering::SeqCst);
        }

        // Once memory is available again, the table loads.
        REFUSE_TEXT.store(false, Ordering::SeqCst);
        workbook.reset_changes();
        let content = workbook.get_cell("Data", "B1").unwrap();
        assert_eq!(
            content.value.as_text(),
            Some("x".repeat(TEXT_BYTES).as_str())
        );
        assert_eq!(REFUSALS.load(Ordering::SeqCst), 0);
        return;
    }

    run_child(
        "a_shared_string_table_that_cannot_be_loaded_is_tried_once_per_call",
        "per-call",
    );
}

// Threads share a table's loads. Concurrent first uses wait for a single load,
// as do concurrent uses after a failed one. A pass that began before another
// thread's load failed gets that failure without loading, even once memory is
// available again, and while allocations keep being refused each pass tries
// at most once. None of these depends on how the threads interleave, so every
// round checks them exactly.
#[test]
fn concurrent_uses_of_a_shared_string_table_share_its_loads() {
    if env::var(CHILD_CASE).as_deref() == Ok("concurrent") {
        const THREADS: usize = 8;
        const ROUNDS: usize = 50;
        fn out_of_memory<T>(result: &sheetpatch::Result<T>) -> bool {
            matches!(result, Err(Error::Io(error)) if error.kind() == ErrorKind::OutOfMemory)
        }
        let bytes = fixture("shared_cells");
        let attempts = || TEXT_ATTEMPTS.swap(0, Ordering::SeqCst);
        let refuse = |on: bool| REFUSE_TEXT.store(on, Ordering::SeqCst);
        // Reads A1 on every thread at once.
        let read_together = |workbook: &Workbook| {
            let barrier = Barrier::new(THREADS);
            thread::scope(|scope| {
                let readers: Vec<_> = (0..THREADS)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            workbook.get_cell("Data", "A1")
                        })
                    })
                    .collect();
                readers
                    .into_iter()
                    .map(|reader| reader.join().unwrap())
                    .collect::<Vec<_>>()
            })
        };

        // The allocations of this size that loading the table, and reading
        // A1, make on their own.
        let workbook = Workbook::from_bytes(bytes.clone()).unwrap();
        attempts();
        workbook.get_cell("Data", "A1").unwrap();
        let load_and_read = attempts();
        workbook.get_cell("Data", "A1").unwrap();
        let read = attempts();
        let load = load_and_read - read;
        assert!(load > 0 && read > 0, "{load} {read}");

        for _ in 0..ROUNDS {
            // Concurrent first uses wait for one load.
            let workbook = Workbook::from_bytes(bytes.clone()).unwrap();
            attempts();
            assert!(read_together(&workbook).iter().all(Result::is_ok));
            assert_eq!(attempts(), load + THREADS * read);

            // So do uses after a failed load, which all begin after it, so
            // each may load the table again.
            let workbook = Workbook::from_bytes(bytes.clone()).unwrap();
            refuse(true);
            assert!(out_of_memory(&workbook.get_cell("Data", "A1")));
            refuse(false);
            attempts();
            assert!(read_together(&workbook).iter().all(Result::is_ok));
            assert_eq!(attempts(), load + THREADS * read);

            // While allocations are refused, concurrent passes each try at
            // most once, whether or not a load has already failed.
            for after_failure in [false, true] {
                let workbook = Workbook::from_bytes(bytes.clone()).unwrap();
                refuse(true);
                if after_failure {
                    assert!(out_of_memory(&workbook.get_cell("Data", "A1")));
                }
                REFUSALS.store(0, Ordering::SeqCst);
                let barrier = Barrier::new(THREADS);
                thread::scope(|scope| {
                    for _ in 0..THREADS {
                        scope.spawn(|| {
                            let view = workbook.prepare_sheet("Data").unwrap();
                            barrier.wait();
                            let results: Vec<_> = view.cell_contents().collect();
                            assert_eq!(results.len(), usize::from(SHARED_CELLS));
                            assert!(results.iter().all(out_of_memory));
                        });
                    }
                });
                refuse(false);
                let tries = REFUSALS.swap(0, Ordering::SeqCst);
                assert!((1..=THREADS).contains(&tries), "{tries} tries");
            }

            // A pass that began before another thread's load failed gets that
            // failure without loading, though memory is available again.
            let workbook = Workbook::from_bytes(bytes.clone()).unwrap();
            let view = workbook.prepare_sheet("Data").unwrap();
            let mut pass = view.cell_contents();
            refuse(true);
            thread::scope(|scope| {
                scope.spawn(|| assert!(out_of_memory(&workbook.get_cell("Data", "A1"))));
            });
            refuse(false);
            attempts();
            assert!(pass.all(|result| out_of_memory(&result)));
            assert_eq!(attempts(), 0);
            // A use that begins now loads it.
            assert!(view.get_cell("A1").is_ok());
        }
        return;
    }

    run_child(
        "concurrent_uses_of_a_shared_string_table_share_its_loads",
        "concurrent",
    );
}
