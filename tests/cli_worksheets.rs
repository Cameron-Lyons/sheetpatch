//! CLI commands over constructed worksheets: cell listings, ranges of sparse
//! sheets, sheet visibility, and the patch lines of refused edits.
mod common;

use common::TempDir;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// Values of every scalar type in a sparse sheet, with style-only cells, an
/// empty row, and formulas with and without cached values.
const SPARSE: &str = "<sheetData>\
    <row r='2'><c r='B2' s='3'/><c r='C2'><v>1.5</v></c>\
    <c r='E2' t='inlineStr'><is><t>two words</t></is></c></row>\
    <row r='4' ht='20' customHeight='1'/>\
    <row r='5'><c r='A5' t='b'><v>1</v></c><c r='B5' t='e'><v>#N/A</v></c>\
    <c r='C5'><f>A5*2</f></c><c r='D5' t='str'><f>\"x\"</f><v>x</v></c>\
    <c r='E5' t='inlineStr' s='2'><is><t></t></is></c><c r='F5' s='2'/></row>\
    <row r='9'><c r='AA9' s='1'><v>-0.25</v></c>\
    <c r='AB9' t='inlineStr'><is><t>tab&#9;and&#10;line</t></is></c></row>\
    </sheetData>";

const SPARSE_CELLS: &str = "C2\tnumber\t1.5\nE2\ttext\ttwo words\nA5\tbool\ttrue\nB5\terror\t#N/A\nC5\tblank\t\nD5\ttext\tx\nE5\ttext\t\nAA9\tnumber\t-0.25\nAB9\ttext\ttab\tand\nline\n";

/// A workbook with one worksheet for each `(name, sheet attributes,
/// worksheet children)`, where the name is XML-escaped and the attributes,
/// if any, start with a space.
fn workbook(sheets: &[(&str, &str, &str)]) -> Vec<u8> {
    let mut list = String::new();
    let mut relationships = String::new();
    let mut parts = Vec::new();
    for (index, (name, attributes, children)) in sheets.iter().enumerate() {
        let id = index + 1;
        list.push_str(&format!(
            "<sheet name='{name}' sheetId='{id}' r:id='s{id}'{attributes}/>"
        ));
        relationships.push_str(&format!(
            "<Relationship Id='s{id}' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet{id}.xml'/>"
        ));
        parts.push((
            format!("xl/worksheets/sheet{id}.xml"),
            format!("<worksheet xmlns='{MAIN_NS}'>{children}</worksheet>"),
        ));
    }
    let root = format!(
        "<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='office' Type='{OFFICE_NS}/officeDocument' Target='xl/workbook.xml'/></Relationships>"
    );
    let workbook = format!(
        "<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheets>{list}</sheets></workbook>"
    );
    let rels = format!("<Relationships xmlns='{PACKAGE_NS}'>{relationships}</Relationships>");
    let mut entries: Vec<(&str, &[u8])> = vec![
        (
            "[Content_Types].xml",
            b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>",
        ),
        ("_rels/.rels", root.as_bytes()),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
    ];
    entries.extend(
        parts
            .iter()
            .map(|(path, xml)| (path.as_str(), xml.as_bytes())),
    );
    common::stored_archive(&entries)
}

/// Write `workbook(sheets)` to `name` in `directory`.
fn saved(directory: &TempDir, name: &str, sheets: &[(&str, &str, &str)]) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, workbook(sheets)).unwrap();
    path
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn cli(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The child may exit before reading its input.
    let _ = child.stdin.take().unwrap().write_all(input);
    child.wait_with_output().unwrap()
}

fn success(args: &[&str]) -> String {
    let output = cli(args, b"");
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

/// Run a command that must fail with exit status 1, and return its message.
fn failure(args: &[&str], input: &[u8]) -> String {
    let output = cli(args, input);
    assert_eq!(output.status.code(), Some(1), "{args:?}");
    assert!(output.stdout.is_empty(), "{args:?}");
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn cells_list_values_and_formulas_but_skip_formatting_only_cells() {
    let directory = TempDir::new();
    let path = saved(&directory, "sparse.xlsx", &[("Data", "", SPARSE)]);
    let path = text(&path);
    assert_eq!(success(&["cells", path, "Data"]), SPARSE_CELLS);
    // JSON objects match get --json for the listed cells, in the same order.
    let listed = ["C2", "E2", "A5", "B5", "C5", "D5", "E5", "AA9", "AB9"];
    let mut get = vec!["get", "--json", path, "Data"];
    get.extend(listed);
    let json = success(&["cells", "--json", path, "Data"]);
    assert_eq!(json, success(&get));
    for object in [
        "{\"sheet\":\"Data\",\"cell\":\"C5\",\"type\":\"blank\",\"value\":null,\"formula\":\"A5*2\",\"style_index\":null}",
        "{\"sheet\":\"Data\",\"cell\":\"D5\",\"type\":\"text\",\"value\":\"x\",\"formula\":\"\\\"x\\\"\",\"style_index\":null}",
        "{\"sheet\":\"Data\",\"cell\":\"E5\",\"type\":\"text\",\"value\":\"\",\"formula\":null,\"style_index\":2}",
        "{\"sheet\":\"Data\",\"cell\":\"AB9\",\"type\":\"text\",\"value\":\"tab\\tand\\nline\",\"formula\":null,\"style_index\":null}",
    ] {
        assert!(json.contains(object), "{json}");
    }
    assert!(!json.contains("\"B2\""), "{json}");
    assert!(!json.contains("\"F5\""), "{json}");
}

#[test]
fn cells_ranges_filter_the_worksheet_cells() {
    let directory = TempDir::new();
    let far = "<sheetData><row r='1'><c r='A1'><v>1</v></c></row>\
               <row r='1048576'><c r='XFD1048576'><v>7</v></c></row></sheetData>";
    let path = saved(
        &directory,
        "ranges.xlsx",
        &[("Data", "", SPARSE), ("Far", "", far)],
    );
    let path = text(&path);
    for (range, expected) in [
        (
            "B2:D5",
            "C2\tnumber\t1.5\nB5\terror\t#N/A\nC5\tblank\t\nD5\ttext\tx\n",
        ),
        // Corners in either order, lowercase letters, and single cells.
        (
            "d9:a1",
            "C2\tnumber\t1.5\nA5\tbool\ttrue\nB5\terror\t#N/A\nC5\tblank\t\nD5\ttext\tx\n",
        ),
        ("c5", "C5\tblank\t\n"),
        ("AB1:AB1048576", "AB9\ttext\ttab\tand\nline\n"),
        ("A1:XFD1048576", SPARSE_CELLS),
        // Formatting-only and missing cells list nothing.
        ("B2", ""),
        ("F5:Z8", ""),
        ("XFD1048576", ""),
    ] {
        assert_eq!(
            success(&["cells", path, "Data", range]),
            expected,
            "{range}"
        );
    }
    assert_eq!(success(&["cells", "--json", path, "Data", "F5:Z8"]), "[]\n");
    assert_eq!(
        success(&["cells", path, "Far", "B1:XFD1048576"]),
        "XFD1048576\tnumber\t7\n"
    );
    assert_eq!(
        success(&["cells", "--json", path, "Far", "A1"]),
        "[{\"sheet\":\"Far\",\"cell\":\"A1\",\"type\":\"number\",\"value\":1,\"formula\":null,\"style_index\":null}]\n"
    );
}

#[test]
fn cells_of_sheets_without_values_are_empty() {
    let directory = TempDir::new();
    let path = saved(
        &directory,
        "empty.xlsx",
        &[
            ("Empty", "", "<sheetData/>"),
            (
                "Styled",
                "",
                "<dimension ref='A1:C3'/><sheetData><row r='1' s='4' customFormat='1'><c r='A1' s='1'/><c r='C1' s='2'/></row><row r='3'/></sheetData>",
            ),
        ],
    );
    let path = text(&path);
    for sheet in ["Empty", "Styled"] {
        assert_eq!(success(&["cells", path, sheet]), "", "{sheet}");
        assert_eq!(success(&["cells", path, sheet, "A1:C3"]), "", "{sheet}");
        assert_eq!(
            success(&["cells", "--json", path, sheet]),
            "[]\n",
            "{sheet}"
        );
    }
    assert_eq!(
        failure(&["cells", path, "Missing"], b""),
        "sheetpatch: worksheet not found: \"Missing\"\n"
    );
}

#[test]
fn cells_lines_prefixed_with_a_sheet_name_are_patch_rows() {
    let directory = TempDir::new();
    let values = "<sheetData><row r='1'>\
        <c r='A1'><v>0.1</v></c><c r='B1'><v>-1E-7</v></c><c r='C1'><v>1.5e20</v></c>\
        <c r='D1' t='b'><v>0</v></c><c r='E1' t='e'><v>#DIV/0!</v></c></row>\
        <row r='3'><c r='A3' t='inlineStr'><is><t>=not a formula</t></is></c>\
        <c r='B3' t='inlineStr'><is><t xml:space='preserve'> a\tb  </t></is></c>\
        <c r='C3' t='inlineStr'><is><t></t></is></c>\
        <c r='D3' t='inlineStr'><is><t>Café ☕</t></is></c></row></sheetData>";
    let input = saved(
        &directory,
        "in.xlsx",
        &[("Values", "", values), ("Copy", "", "<sheetData/>")],
    );
    let output = directory.join("out.xlsx");
    let listed = success(&["cells", text(&input), "Values"]);
    assert_eq!(
        listed,
        "A1\tnumber\t0.1\nB1\tnumber\t-0.0000001\nC1\tnumber\t150000000000000000000\nD1\tbool\tfalse\nE1\terror\t#DIV/0!\nA3\ttext\t=not a formula\nB3\ttext\t a\tb  \nC3\ttext\t\nD3\ttext\tCafé ☕\n"
    );
    let patch: String = listed
        .lines()
        .map(|line| format!("Copy\t{line}\n"))
        .collect();
    let applied = cli(
        &["patch", text(&input), text(&output), "-"],
        patch.as_bytes(),
    );
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(success(&["cells", text(&output), "Copy"]), listed);
    assert_eq!(
        success(&["cells", "--json", text(&output), "Copy"]),
        success(&["cells", "--json", text(&input), "Values"]).replace("Values", "Copy")
    );
}

#[test]
fn get_ranges_read_sparse_sheets_row_by_row() {
    let directory = TempDir::new();
    let path = saved(&directory, "sparse.xlsx", &[("Data", "", SPARSE)]);
    let path = text(&path);
    assert_eq!(
        success(&["get", path, "Data", "B4:C5", "aa9", "E2:E2"]),
        "\n\n#N/A\n\n-0.25\ntwo words\n"
    );
    assert_eq!(
        success(&["get", "--json", path, "Data", "A5:C5", "B2"]),
        success(&["get", "--json", path, "Data", "A5", "B5", "C5", "B2"])
    );
}

#[test]
fn list_reports_each_sheet_visibility() {
    let directory = TempDir::new();
    let empty = "<sheetData/>";
    let path = saved(
        &directory,
        "visibility.xlsx",
        &[
            ("Shown", "", empty),
            ("Hidden", " state='hidden'", empty),
            ("Very &amp; hidden", " state='veryHidden'", empty),
            ("Explicit", " state='visible'", empty),
        ],
    );
    let path = text(&path);
    // Plain output still lists every sheet by name only.
    assert_eq!(
        success(&["list", path]),
        "Shown\nHidden\nVery & hidden\nExplicit\n"
    );
    assert_eq!(
        success(&["list", "--json", path]),
        concat!(
            "[{\"name\":\"Shown\",\"path\":\"xl/worksheets/sheet1.xml\",\"visibility\":\"visible\"},",
            "{\"name\":\"Hidden\",\"path\":\"xl/worksheets/sheet2.xml\",\"visibility\":\"hidden\"},",
            "{\"name\":\"Very & hidden\",\"path\":\"xl/worksheets/sheet3.xml\",\"visibility\":\"veryHidden\"},",
            "{\"name\":\"Explicit\",\"path\":\"xl/worksheets/sheet4.xml\",\"visibility\":\"visible\"}]\n"
        )
    );
    // Hidden sheets are read like visible ones.
    assert_eq!(success(&["cells", path, "Very & hidden"]), "");
}

#[test]
fn piped_lists_keep_control_characters_in_names() {
    let directory = TempDir::new();
    let path = saved(
        &directory,
        "names.xlsx",
        &[
            ("Two&#10;Lines", "", "<sheetData/>"),
            ("Tab&#9;Name", "", "<sheetData/>"),
        ],
    );
    // Only a terminal shows escapes; pipes receive the exact names.
    assert_eq!(success(&["list", text(&path)]), "Two\nLines\nTab\tName\n");
    assert_eq!(
        success(&["list", "--json", text(&path)]),
        concat!(
            "[{\"name\":\"Two\\nLines\",\"path\":\"xl/worksheets/sheet1.xml\",\"visibility\":\"visible\"},",
            "{\"name\":\"Tab\\tName\",\"path\":\"xl/worksheets/sheet2.xml\",\"visibility\":\"visible\"}]\n"
        )
    );
}

#[test]
fn refused_patch_edits_name_only_lines_for_the_refused_cell() {
    let directory = TempDir::new();
    let data = "<sheetData>\
        <row r='1'><c r='B1'><v>1</v></c></row>\
        <row r='10'><c r='A10'><f>1+1</f><v>2</v></c></row></sheetData>\
        <mergeCells count='1'><mergeCell ref='B1:C2'/></mergeCells>";
    let grouped =
        "<sheetData><row r='1'><c r='Z1'><f t='array'>1</f><v>1</v></c></row></sheetData>";
    let path = saved(
        &directory,
        "guards.xlsx",
        &[
            ("Data", "", data),
            ("Other", "", "<sheetData/>"),
            ("Grouped", "", grouped),
        ],
    );
    let output = directory.join("out.xlsx");
    let formula = "unsupported: worksheet \"Data\": A10 contains a formula";
    for (patch, expected) in [
        // A1 is a prefix of A10, but names a different cell.
        (
            "Data\tA10\tnumber\t1\nData\tA1\tnumber\t2\n",
            format!("Patch line 1: {formula}"),
        ),
        // The same cell on another worksheet is a different target.
        (
            "Other\tA10\tnumber\t1\nData\tA10\tnumber\t1\nOther\tA10\tnumber\t2\n",
            format!("Patch line 2: {formula}"),
        ),
        (
            "Data\tA1\ttext\tx\nData\tc2\ttext\ty\n",
            "Patch line 2: unsupported: worksheet \"Data\": C2 is not the anchor of its merged range"
                .to_owned(),
        ),
        // A refusal naming a cell that no line edits has no line.
        (
            "Grouped\tA1\ttext\tx\n",
            "unsupported: worksheet \"Grouped\": Z1 belongs to a formula group without a reference range"
                .to_owned(),
        ),
    ] {
        for args in [
            vec!["patch", "--check", text(&path), "-"],
            vec!["patch", text(&path), text(&output), "-"],
        ] {
            assert_eq!(
                failure(&args, patch.as_bytes()),
                format!("sheetpatch: {expected}\n"),
                "{patch:?}"
            );
        }
    }
    assert!(!output.exists());
}
