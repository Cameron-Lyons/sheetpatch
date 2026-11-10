//! Reject one result allocation in a child process, so an infallible allocation
//! regression fails this test instead of aborting the whole integration suite.

mod common;

use sheetpatch::{CellRef, Error, Workbook};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::ErrorKind;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{env, ptr};

// Deliberately unlike the sizes of the address/result vectors and XML arenas.
const TEXT_BYTES: usize = 65_537;
const CHILD_CASE: &str = "SHEETPATCH_ALLOCATION_CASE";
static FAIL_NEXT_TEXT: AtomicBool = AtomicBool::new(false);

struct FailOnce;

fn reject(size: usize) -> bool {
    size == TEXT_BYTES && FAIL_NEXT_TEXT.swap(false, Ordering::SeqCst)
}

// SAFETY: Every successful allocation and every deallocation is delegated to
// System with the original layout. A rejected allocation returns null, as the
// allocator contract permits. The failure switch is atomic and allocates nothing.
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

fn fixture(case: &str) -> Vec<u8> {
    let text = "x".repeat(TEXT_BYTES);
    let cell = match case {
        "shared" => "<c r='A1' t='s'><v>0</v></c>".to_owned(),
        "inline" => format!("<c r='A1' t='inlineStr'><is><t>{text}</t></is></c>"),
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
        "rich" => format!(
            "<c r='A1' t='inlineStr'><is><r><t>x</t></r><r><t>{}</t></r></is></c>",
            &text[1..]
        ),
        _ => panic!("unknown allocation case {case}"),
    };
    let worksheet = format!(
        "<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><sheetData><row r='1'>{cell}</row></sheetData></worksheet>"
    );
    let strings = format!(
        "<sst xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><si><t>{text}</t></si></sst>"
    );
    common::stored_archive(&[
        ("[Content_Types].xml", b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>"),
        ("_rels/.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='office' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ("xl/workbook.xml", b"<workbook xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' xmlns:r='http://schemas.openxmlformats.org/officeDocument/2006/relationships'><sheets><sheet name='Data' sheetId='1' r:id='sheet'/></sheets></workbook>"),
        ("xl/_rels/workbook.xml.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='sheet' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet' Target='worksheets/sheet1.xml'/><Relationship Id='strings' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings' Target='sharedStrings.xml'/></Relationships>"),
        ("xl/worksheets/sheet1.xml", worksheet.as_bytes()),
        ("xl/sharedStrings.xml", strings.as_bytes()),
    ])
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
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "read_result_allocations_report_out_of_memory",
                "--nocapture",
            ])
            .env(CHILD_CASE, case)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: allocation failure killed or failed the child: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
