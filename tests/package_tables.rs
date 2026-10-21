//! Excel requires each table header cell's text to equal its table column's
//! name, so edits that would change a header cell are refused.
mod common;

use sheetpatch::{CellEdit, CellValue, Error, Workbook};
use std::collections::BTreeMap;

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const STRICT_MAIN_NS: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const STRICT_OFFICE_NS: &str = "http://purl.oclc.org/ooxml/officeDocument/relationships";
const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const TABLE_PATH: &str = "xl/tables/table1.xml";
const SHEET_RELS: &str = "xl/worksheets/_rels/sheet1.xml.rels";

const REFUSAL: &str =
    "is an Excel table header cell; editing it would desynchronize the table's column names";

/// A table on Data!A1:B3 with headers "Name" (a shared string) and "Amount"
/// (an inline string). Notes has no relationships.
fn parts(table_attributes: &str) -> BTreeMap<String, String> {
    let data = format!(
        "<worksheet xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheetData>\
         <row r='1'><c r='A1' t='s'><v>0</v></c><c r='B1' t='inlineStr'><is><t>Amount</t></is></c><c r='C1'><v>9</v></c></row>\
         <row r='2'><c r='A2' t='inlineStr'><is><t>x</t></is></c><c r='B2'><v>5</v></c></row>\
         <row r='3'><c r='A3' t='inlineStr'><is><t>y</t></is></c><c r='B3'><v>6</v></c></row>\
         </sheetData><tableParts count='1'><tablePart r:id='table'/></tableParts></worksheet>"
    );
    let notes = format!(
        "<worksheet xmlns='{MAIN_NS}'><sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData></worksheet>"
    );
    BTreeMap::from([
        (
            "[Content_Types].xml".into(),
            "<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>".into(),
        ),
        (
            "_rels/.rels".into(),
            format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='office' Type='{OFFICE_NS}/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ),
        (
            "xl/workbook.xml".into(),
            format!("<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheets><sheet name='Data' sheetId='1' r:id='one'/><sheet name='Notes' sheetId='2' r:id='two'/></sheets></workbook>"),
        ),
        (
            "xl/_rels/workbook.xml.rels".into(),
            format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='one' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet1.xml'/><Relationship Id='two' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet2.xml'/><Relationship Id='strings' Type='{OFFICE_NS}/sharedStrings' Target='sharedStrings.xml'/></Relationships>"),
        ),
        (
            "xl/sharedStrings.xml".into(),
            format!("<sst xmlns='{MAIN_NS}'><si><t>Name</t></si></sst>"),
        ),
        ("xl/worksheets/sheet1.xml".into(), data),
        ("xl/worksheets/sheet2.xml".into(), notes),
        (
            SHEET_RELS.into(),
            format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='link' Type='{OFFICE_NS}/hyperlink' Target='https://example.com/' TargetMode='External'/><Relationship Id='table' Type='{OFFICE_NS}/table' Target='../tables/table1.xml'/></Relationships>"),
        ),
        (
            TABLE_PATH.into(),
            format!("<table xmlns='{MAIN_NS}' id='1' name='Table1' displayName='Table1' {table_attributes}><autoFilter ref='A1:B3'/><tableColumns count='2'><tableColumn id='1' name='Name'/><tableColumn id='2' name='Amount'/></tableColumns></table>"),
        ),
    ])
}

fn archive(parts: &BTreeMap<String, String>) -> Vec<u8> {
    let entries: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(name, xml)| (name.as_str(), xml.as_bytes()))
        .collect();
    common::stored_archive(&entries)
}

fn book(parts: &BTreeMap<String, String>) -> Workbook {
    Workbook::from_bytes(archive(parts)).unwrap()
}

fn assert_refused_in(sheet_name: &str, result: sheetpatch::Result<()>, address: &str) {
    match result {
        Err(
            ref error @ Error::ProtectedCell {
                ref sheet,
                cell,
                ref reason,
                ..
            },
        ) => {
            assert_eq!(
                (sheet.as_str(), cell.to_string().as_str()),
                (sheet_name, address)
            );
            assert_eq!(reason, REFUSAL);
            assert_eq!(
                error.to_string(),
                format!("protected cell: worksheet \"{sheet_name}\": {address} {REFUSAL}")
            );
        }
        other => panic!("expected {address} to be refused, got {other:?}"),
    }
}

fn assert_refused(result: sheetpatch::Result<()>, cell: &str) {
    assert_refused_in("Data", result, cell);
}

#[test]
fn table_body_and_cells_outside_tables_remain_editable() {
    let mut book = book(&parts("ref='A1:B3'"));
    book.set_cell("Data", "B2", 7).unwrap();
    book.set_cells("Data", [("A3", "z"), ("C1", "beside")])
        .unwrap();
    book.set_cell("Data", "A4", "below").unwrap();
    book.set_cell("Notes", "A1", "free").unwrap();
    let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    assert_eq!(
        reopened.get_cell("Data", "B2").unwrap().value,
        CellValue::Number(7.0)
    );
    assert_eq!(
        reopened.get_cell("Data", "C1").unwrap().value,
        CellValue::from("beside")
    );
}

#[test]
fn table_relationships_resolve_independently_of_legacy_zip_name_spelling() {
    for (target, sheet_path, rels_path) in [
        (
            "worksheets/sheet%20one.xml",
            "xl/worksheets/sheet%20one.xml",
            "xl/worksheets/_rels/sheet one.xml.rels",
        ),
        (
            "worksheets/sheet%20one.xml",
            "xl/worksheets/sheet one.xml",
            "xl/worksheets/_rels/sheet%20one.xml.rels",
        ),
        (
            "worksheets/sheet%25.xml",
            "xl/worksheets/sheet%.xml",
            "xl/worksheets/_rels/sheet%.xml.rels",
        ),
    ] {
        let mut parts = parts("ref='A1:B3'");
        let sheet = parts.remove("xl/worksheets/sheet1.xml").unwrap();
        parts.insert(sheet_path.into(), sheet);
        let rels = parts.remove(SHEET_RELS).unwrap();
        parts.insert(rels_path.into(), rels);
        if rels_path.contains("%20") {
            parts.insert(
                "xl/worksheets/_rels/sheet one.xml.rels".into(),
                format!("<Relationships xmlns='{PACKAGE_NS}'/>"),
            );
        }
        let workbook_rels = parts.get_mut("xl/_rels/workbook.xml.rels").unwrap();
        *workbook_rels = workbook_rels.replace("worksheets/sheet1.xml", target);

        let mut book = book(&parts);
        assert_refused(book.set_cell("Data", "B1", "Changed"), "B1");
        assert!(!book.has_changes());
        book.set_cell("Data", "B2", 7).unwrap();
        let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
        assert_eq!(reopened.get_cell("Data", "B2").unwrap().value, 7.into());
    }
}

#[test]
fn edits_that_change_a_header_cell_are_refused_without_changes() {
    for attributes in ["ref='A1:B3'", "ref=' A1:B3 ' headerRowCount=' 1 '"] {
        let original = archive(&parts(attributes));
        let mut book = Workbook::from_bytes(original.clone()).unwrap();
        assert_refused(book.set_cell("Data", "B1", "Total"), "B1");
        assert_refused(book.set_cell("Data", "A1", "name"), "A1");
        assert_refused(book.set_cell("Data", "A1", 1), "A1");
        assert_refused(book.set_cell("Data", "B1", CellValue::Blank), "B1");
        assert_refused(book.set_cells("Data", [("B2", "8"), ("B1", "x")]), "B1");
        assert_refused(
            book.apply_edits([
                CellEdit::new("Notes", "A1", 2).unwrap(),
                CellEdit::new("Data", "A1", "Label").unwrap(),
            ]),
            "A1",
        );
        assert!(!book.has_changes());
        assert_eq!(book.to_bytes().unwrap(), original);
    }
}

#[test]
fn edits_that_keep_a_header_value_are_no_ops() {
    let original = archive(&parts("ref='A1:B3'"));
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    // A1 is a shared string and B1 an inline string.
    book.set_cells("Data", [("A1", "Name"), ("B1", "Amount")])
        .unwrap();
    assert!(!book.has_changes());
    book.set_cells("Data", [("A1", "Name"), ("B2", "8")])
        .unwrap();
    assert_eq!(
        book.get_cell("Data", "B2").unwrap().value,
        CellValue::from("8")
    );
    book.set_cell("Data", "B2", 5).unwrap();
    assert_eq!(book.to_bytes().unwrap(), original);
}

#[test]
fn tables_without_a_header_row_leave_the_first_row_editable() {
    let mut book = book(&parts("ref='A1:B3' headerRowCount='0'"));
    book.set_cell("Data", "A1", "first").unwrap();
    book.set_cell("Data", "B1", 3).unwrap();
    // Several header rows protect each of them.
    let mut book = self::book(&parts("ref='A1:B3' headerRowCount='2'"));
    assert_refused(book.set_cell("Data", "B2", 4), "B2");
    book.set_cell("Data", "B3", 4).unwrap();
}

#[test]
fn multiple_tables_keep_header_columns_and_cell_error_order() {
    let mut parts = parts("ref='B1:C3' headerRowCount='3'");
    let relationships = parts.get_mut(SHEET_RELS).unwrap();
    *relationships = relationships.replace(
        "</Relationships>",
        &format!(
            "<Relationship Id='other' Type='{OFFICE_NS}/table' Target='../tables/table2.xml'/></Relationships>"
        ),
    );
    parts.insert(
        "xl/tables/table2.xml".into(),
        format!(
            "<table xmlns='{MAIN_NS}' id='2' name='Other' displayName='Other' ref='A1:A3'><tableColumns count='1'><tableColumn id='1' name='Name'/></tableColumns></table>"
        ),
    );
    let sheet = parts.get_mut("xl/worksheets/sheet1.xml").unwrap();
    *sheet = sheet.replace(
        "<tableParts count='1'><tablePart r:id='table'/></tableParts>",
        "<tableParts count='2'><tablePart r:id='table'/><tablePart r:id='other'/></tableParts>",
    );
    let mut book = book(&parts);
    // Both cells lie between the header's first and last rows, but outside
    // its columns. A2 is also in the other table's editable body.
    book.set_cells("Data", [("A2", "left"), ("D2", "right")])
        .unwrap();
    let pending = book.to_bytes().unwrap();
    assert_refused(book.set_cell("Data", "B2", "changed"), "B2");
    assert_refused(book.set_cell("Data", "C3", "changed"), "C3");
    // Neither relationship order nor input order changes the first refusal.
    assert_refused(
        book.set_cells("Data", [("C1", "changed"), ("A1", "changed")]),
        "A1",
    );
    assert_eq!(book.to_bytes().unwrap(), pending);
}

#[test]
fn later_header_read_errors_take_precedence_over_changed_headers() {
    for (second_header, problem) in [
        (
            "<c r='B1' t='inlineStr' s='bad'><is><t>Amount</t></is></c>",
            "B1: invalid cell style index",
        ),
        (
            "<c r='B1' t='s'><v>1</v></c>",
            "B1: shared-string index 1 is out of bounds",
        ),
    ] {
        let mut parts = parts("ref='A1:B3'");
        let sheet = parts.get_mut("xl/worksheets/sheet1.xml").unwrap();
        *sheet = sheet.replace(
            "<c r='B1' t='inlineStr'><is><t>Amount</t></is></c>",
            second_header,
        );
        let mut book = book(&parts);
        book.set_cell("Notes", "A1", 2).unwrap();
        let pending = book.to_bytes().unwrap();
        let error = book
            .set_cells("Data", [("B1", "second change"), ("A1", "first change")])
            .unwrap_err();
        assert!(
            matches!(&error, Error::InvalidWorkbook(message)
                if message.starts_with("worksheet \"Data\": ") && message.contains(problem)),
            "{error}"
        );
        assert_eq!(book.to_bytes().unwrap(), pending);
    }

    // A later header can also fail while loading its separate shared-string
    // part; that error retains the part's context and outranks the first edit.
    let mut parts = parts("ref='A1:B3'");
    let sheet = parts.get_mut("xl/worksheets/sheet1.xml").unwrap();
    *sheet = sheet
        .replace(
            "<c r='A1' t='s'><v>0</v></c>",
            "<c r='A1' t='inlineStr'><is><t>Name</t></is></c>",
        )
        .replace(
            "<c r='B1' t='inlineStr'><is><t>Amount</t></is></c>",
            "<c r='B1' t='s'><v>0</v></c>",
        );
    parts.insert(
        "xl/sharedStrings.xml".into(),
        format!("<sst xmlns='{MAIN_NS}'>"),
    );
    let original = archive(&parts);
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    let error = book
        .set_cells("Data", [("A1", "first change"), ("B1", "second change")])
        .unwrap_err();
    assert!(
        matches!(&error, Error::Xml(message) if message.starts_with("part \"xl/sharedStrings.xml\": ")),
        "{error}"
    );
    assert_eq!(book.to_bytes().unwrap(), original);
}

#[test]
fn strict_table_relationships_and_namespaces_are_recognized() {
    let mut parts = parts("ref='A1:B3'");
    let rels = parts[SHEET_RELS].replace(
        &format!("{OFFICE_NS}/table"),
        &format!("{STRICT_OFFICE_NS}/table"),
    );
    parts.insert(SHEET_RELS.into(), rels);
    let mut book = self::book(&parts);
    assert_refused(book.set_cell("Data", "B1", "Total"), "B1");

    let table = parts[TABLE_PATH].replace(MAIN_NS, STRICT_MAIN_NS);
    parts.insert(TABLE_PATH.into(), table);
    let mut book = self::book(&parts);
    assert_refused(book.set_cell("Data", "B1", "Total"), "B1");
    book.set_cell("Data", "B2", 1).unwrap();
}

#[test]
fn repeated_table_targets_keep_protection_and_relationship_error_order() {
    let mut parts = parts("ref='A1:B3'");
    let aliases = [
        "../tables/table1.xml",
        "../tables/table1.xml",
        "../tables/./table1.xml",
        "../tables/./table1.xml",
        "../tables/%74able1.xml",
        "/XL/TABLES/TABLE1.XML",
    ]
    .iter()
    .enumerate()
    .map(|(index, target)| {
        format!("<Relationship Id='alias{index}' Type='{OFFICE_NS}/table' Target='{target}'/>")
    })
    .collect::<String>();
    let rels = parts.get_mut(SHEET_RELS).unwrap();
    *rels = rels.replace("</Relationships>", &format!("{aliases}</Relationships>"));

    let original = archive(&parts);
    let mut book = self::book(&parts);
    assert_refused(book.set_cell("Data", "B1", "Total"), "B1");
    assert_eq!(book.to_bytes().unwrap(), original);
    book.set_cell("Data", "B2", 7).unwrap();
    let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    assert_eq!(reopened.get_cell("Data", "B2").unwrap().value, 7.into());

    // A duplicate target must not hide invalid relationship metadata. All
    // relationships are parsed first, then target and table errors keep
    // document order, ahead of a header cell's edit refusal.
    for (late, broken_table, part, problem) in [
        (
            "Target='../tables/table1.xml' TargetMode='External'",
            false,
            SHEET_RELS,
            "table relationship late is external",
        ),
        (
            "Target='../../../outside.xml'",
            false,
            SHEET_RELS,
            "relationship target escapes package root",
        ),
        (
            "Target='../tables/missing.xml'",
            false,
            SHEET_RELS,
            "missing table part",
        ),
        (
            "Target='../tables/table1%xx.xml'",
            false,
            SHEET_RELS,
            "invalid percent-encoded target",
        ),
        (
            "Target='../tables/table1.xml' TargetMode='External'",
            true,
            TABLE_PATH,
            "table has an invalid ref",
        ),
        (
            "Target='../tables/table1.xml' TargetMode='invalid'",
            true,
            SHEET_RELS,
            "invalid relationship TargetMode",
        ),
    ] {
        let mut damaged = parts.clone();
        let rels = damaged.get_mut(SHEET_RELS).unwrap();
        *rels = rels.replace(
            "</Relationships>",
            &format!("<Relationship Id='late' Type='{OFFICE_NS}/table' {late}/></Relationships>"),
        );
        if broken_table {
            let table = damaged.get_mut(TABLE_PATH).unwrap();
            *table = table.replace("ref='A1:B3'", "ref='bad'");
        }
        let original = archive(&damaged);
        let mut book = Workbook::from_bytes(original.clone()).unwrap();
        let error = book.set_cell("Data", "B1", "Total").unwrap_err();
        assert!(
            matches!(&error, Error::InvalidWorkbook(message)
                if message.starts_with(&format!("part {part:?}: ")) && message.contains(problem)),
            "{late}, {broken_table}: {error}"
        );
        assert_eq!(book.to_bytes().unwrap(), original);
    }
}

#[test]
fn a_table_part_shared_by_worksheets_protects_each_of_them() {
    let mut shared = parts("ref='A1:B3'");
    let data = shared["xl/worksheets/sheet1.xml"].clone();
    shared.insert("xl/worksheets/sheet2.xml".into(), data);
    let rels = shared[SHEET_RELS].clone();
    shared.insert("xl/worksheets/_rels/sheet2.xml.rels".into(), rels);

    // In separate calls, the second worksheet reuses the first one's table.
    let mut separate = book(&shared);
    assert_refused(separate.set_cell("Data", "A1", "Changed"), "A1");
    assert_refused_in("Notes", separate.set_cell("Notes", "B1", "Changed"), "B1");
    separate.set_cell("Notes", "B2", 1).unwrap();
    separate.set_cell("Notes", "B1", "Amount").unwrap();

    let mut together = book(&shared);
    assert_refused_in(
        "Notes",
        together.apply_edits([
            CellEdit::new("Data", "B2", 3).unwrap(),
            CellEdit::new("Notes", "A1", "Changed").unwrap(),
        ]),
        "A1",
    );
    assert!(!together.has_changes());
    together
        .apply_edits([
            CellEdit::new("Data", "B2", 3).unwrap(),
            CellEdit::new("Notes", "B3", 4).unwrap(),
        ])
        .unwrap();

    // A table part that cannot be read is reported again for each worksheet,
    // naming the part rather than the worksheet.
    let mut broken = shared.clone();
    broken.insert(TABLE_PATH.into(), format!("<table xmlns='{MAIN_NS}'/>"));
    let mut broken = book(&broken);
    for sheet in ["Data", "Notes", "Data"] {
        let error = broken.set_cell(sheet, "B2", 1).unwrap_err();
        assert!(
            matches!(&error, Error::InvalidWorkbook(message)
                if message == &format!("part {TABLE_PATH:?}: table lacks a ref")),
            "{sheet}: {error}"
        );
    }
    assert!(!broken.has_changes());
}

#[test]
fn many_repeated_table_targets_share_long_source_uris() {
    let mut parts = parts("ref='A1:B3'");
    let directory = format!("xl/{}", "nested".repeat(10_000));
    let worksheet_path = format!("{directory}/sheet1.xml");
    let relationships_path = format!("{directory}/_rels/sheet1.xml.rels");
    let table_path = format!("{directory}/table1.xml");
    let worksheet = parts.remove("xl/worksheets/sheet1.xml").unwrap();
    parts.insert(worksheet_path, worksheet);
    let table = parts.remove(TABLE_PATH).unwrap();
    parts.insert(table_path, table);
    let relationships = parts.remove(SHEET_RELS).unwrap();
    let aliases = (0..1024)
        .map(|index| {
            format!(
                "<Relationship Id='repeat{index}' Type='{OFFICE_NS}/table' Target='table1.xml'/>"
            )
        })
        .collect::<String>();
    parts.insert(
        relationships_path,
        relationships
            .replace("../tables/table1.xml", "table1.xml")
            .replace("</Relationships>", &format!("{aliases}</Relationships>")),
    );
    let workbook_relationships = parts.get_mut("xl/_rels/workbook.xml.rels").unwrap();
    *workbook_relationships = workbook_relationships.replace(
        "Target='worksheets/sheet1.xml'",
        &format!("Target='/{directory}/sheet1.xml'"),
    );

    let original = archive(&parts);
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    assert_refused(book.set_cell("Data", "B1", "Total"), "B1");
    assert_eq!(book.to_bytes().unwrap(), original);
    book.set_cell("Data", "B2", 7).unwrap();
    let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    assert_eq!(reopened.get_cell("Data", "B2").unwrap().value, 7.into());
}

#[test]
fn alternate_table_target_spellings_share_long_source_uris() {
    let mut parts = parts("ref='A1:B3'");
    let directory = format!("xl/{}", "nested".repeat(10_000));
    let worksheet = parts.remove("xl/worksheets/sheet1.xml").unwrap();
    parts.insert(format!("{directory}/sheet1.xml"), worksheet);
    let table = parts.remove(TABLE_PATH).unwrap();
    parts.insert(format!("{directory}/table1.xml"), table);
    let relationships = parts.remove(SHEET_RELS).unwrap();
    // Every alias spells table1.xml differently, by letter case, escapes of
    // unreserved letters, and dot segments, so none repeats a spelling.
    let spell = |c: char, choice: usize| match choice {
        0 => c.to_string(),
        1 => c.to_ascii_uppercase().to_string(),
        2 => format!("%{:02x}", c as u32),
        _ => format!("%{:02X}", c as u32),
    };
    let aliases = (0..16_384usize)
        .map(|index| {
            let mut rest = index;
            let name: String = "table1.xml"
                .chars()
                .map(|c| {
                    if !c.is_ascii_alphabetic() {
                        return c.to_string();
                    }
                    let choice = rest % 4;
                    rest /= 4;
                    spell(c, choice)
                })
                .collect();
            let prefix = ["", "./", "d/../", "./d/./../"][index % 4];
            format!(
                "<Relationship Id='alias{index}' Type='{OFFICE_NS}/table' Target='{prefix}{name}'/>"
            )
        })
        .collect::<String>();
    parts.insert(
        format!("{directory}/_rels/sheet1.xml.rels"),
        relationships
            .replace("../tables/table1.xml", "table1.xml")
            .replace("</Relationships>", &format!("{aliases}</Relationships>")),
    );
    let workbook_relationships = parts.get_mut("xl/_rels/workbook.xml.rels").unwrap();
    *workbook_relationships = workbook_relationships.replace(
        "Target='worksheets/sheet1.xml'",
        &format!("Target='/{directory}/sheet1.xml'"),
    );

    let original = archive(&parts);
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    assert_refused(book.set_cell("Data", "B1", "Total"), "B1");
    assert_eq!(book.to_bytes().unwrap(), original);
    book.set_cell("Data", "B2", 7).unwrap();
    let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    assert_eq!(reopened.get_cell("Data", "B2").unwrap().value, 7.into());
}

#[test]
fn table_targets_resolve_against_the_worksheet_uri_not_its_decoded_zip_name() {
    for suffix in ["%41", "%", "%2F"] {
        let mut parts = parts("ref='A1:B3'");
        let directory = format!("xl/worksheets{suffix}");
        let worksheet_path = format!("{directory}/sheet1.xml");
        let worksheet = parts.remove("xl/worksheets/sheet1.xml").unwrap();
        parts.insert(worksheet_path.clone(), worksheet);
        let relationships = parts.remove(SHEET_RELS).unwrap();
        parts.insert(
            format!("{directory}/_rels/sheet1.xml.rels"),
            relationships.replace("../tables/table1.xml", "table1.xml"),
        );
        let table = parts.remove(TABLE_PATH).unwrap();
        parts.insert(format!("{directory}/table1.xml"), table);
        let relationships = parts.get_mut("xl/_rels/workbook.xml.rels").unwrap();
        *relationships = relationships.replace(
            "worksheets/sheet1.xml",
            &format!("worksheets{}/sheet1.xml", suffix.replace('%', "%25")),
        );
        // Mistaking the decoded ZIP name's literal "%41" for a URI escape
        // would read this unrelated table and leave the real headers editable.
        parts.insert(
            "xl/worksheetsA/table1.xml".into(),
            format!("<table xmlns='{MAIN_NS}' ref='Z1:Z3'/>"),
        );

        let original = archive(&parts);
        let mut book = Workbook::from_bytes(original.clone()).unwrap();
        assert_eq!(book.sheets()[0].path(), worksheet_path);
        assert_refused(book.set_cell("Data", "B1", "Total"), "B1");
        assert_eq!(book.to_bytes().unwrap(), original);
        book.set_cell("Data", "B2", 7).unwrap();
        let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
        assert_eq!(
            reopened.get_cell("Data", "B2").unwrap().value,
            CellValue::Number(7.0)
        );
    }
}

#[test]
fn sheets_without_tables_are_unaffected() {
    let mut parts = parts("ref='A1:B3'");
    parts.remove(SHEET_RELS);
    let mut book = self::book(&parts);
    book.set_cell("Data", "B1", "Total").unwrap();

    // Relationships to other parts, and table relationships of other sheets,
    // do not protect anything.
    let mut parts = self::parts("ref='A1:B3'");
    let rels = parts.remove(SHEET_RELS).unwrap();
    parts.insert(
        SHEET_RELS.into(),
        rels.replace(
            &format!("{OFFICE_NS}/table"),
            &format!("{OFFICE_NS}/drawing"),
        ),
    );
    parts.insert(
        "xl/worksheets/_rels/sheet2.xml.rels".into(),
        rels.replace("../tables/table1.xml", "/xl/tables/table1.xml"),
    );
    let mut book = self::book(&parts);
    book.set_cell("Data", "B1", "Total").unwrap();
    assert_eq!(
        book.get_cell("Data", "B1").unwrap().value,
        CellValue::from("Total")
    );
    book.set_cell("Notes", "B5", "free").unwrap();
    assert_refused_in("Notes", book.set_cell("Notes", "A1", "Total"), "A1");
}

#[test]
fn rewriting_a_date_header_with_its_serial_is_a_no_op() {
    for (properties, serial, other) in [
        ("", 45_306, 43_844),
        ("<workbookPr date1904='1'/>", 43_844, 45_306),
    ] {
        let mut parts = parts("ref='A1:B3'");
        let data = parts["xl/worksheets/sheet1.xml"].replace(
            "<c r='B1' t='inlineStr'><is><t>Amount</t></is></c>",
            "<c r='B1' t='d' s='2'><v>2024-01-15</v></c>",
        );
        parts.insert("xl/worksheets/sheet1.xml".into(), data);
        let workbook =
            parts["xl/workbook.xml"].replace("<sheets>", &format!("{properties}<sheets>"));
        parts.insert("xl/workbook.xml".into(), workbook);
        let original = archive(&parts);
        let mut book = Workbook::from_bytes(original.clone()).unwrap();
        assert_eq!(
            book.get_cell("Data", "B1").unwrap().value,
            CellValue::from(serial)
        );
        // The serial the date reads as is its current value.
        book.set_cell("Data", "B1", serial).unwrap();
        assert!(!book.has_changes());
        book.set_cells("Data", [("B1", serial), ("B2", 8)]).unwrap();
        book.set_cell("Data", "B2", 5).unwrap();
        assert!(!book.has_changes());
        assert_eq!(book.to_bytes().unwrap(), original);
        // Any other number would change the header.
        assert_refused(book.set_cell("Data", "B1", other), "B1");
        assert_refused(
            book.apply_edits([CellEdit::new("Data", "B1", serial + 1).unwrap()]),
            "B1",
        );
        assert_eq!(book.to_bytes().unwrap(), original);
    }
}

#[test]
fn malformed_tables_of_an_edited_sheet_are_invalid() {
    // Errors name the part at fault, not the worksheet being edited.
    let invalid = |change: &dyn Fn(&mut BTreeMap<String, String>), part: &str| {
        let mut parts = parts("ref='A1:B3'");
        change(&mut parts);
        let mut book = book(&parts);
        // Reads and other sheets do not need the table.
        assert_eq!(
            book.get_cell("Data", "B2").unwrap().value,
            CellValue::Number(5.0)
        );
        book.set_cell("Notes", "A1", 2).unwrap();
        match book.set_cell("Data", "B2", 7) {
            Err(Error::InvalidWorkbook(message)) => {
                assert!(
                    message.starts_with(&format!("part {part:?}: ")),
                    "{message}"
                );
                assert!(!message.contains("worksheet \""), "{message}");
                message
            }
            other => panic!("expected an invalid table, got {other:?}"),
        }
    };
    for attributes in [
        "",
        "ref='A1:'",
        "ref='$A$1:$B$3'",
        "ref='A1:B3' headerRowCount='x'",
        "ref='A1:B3' headerRowCount='-1'",
        "ref='A1:B3' headerRowCount='4'",
    ] {
        let attributes = attributes.to_owned();
        let message = invalid(
            &move |parts| {
                let table = parts[TABLE_PATH].replace("displayName='Table1' ref='A1:B3'", "");
                let table = table.replacen(" id='1'", &format!(" id='1' {attributes}"), 1);
                parts.insert(TABLE_PATH.into(), table);
            },
            TABLE_PATH,
        );
        assert!(message.contains(": table "), "{message}");
    }
    let message = invalid(
        &|parts| {
            let table = parts[TABLE_PATH]
                .replace("<table ", "<other ")
                .replace("</table>", "</other>");
            parts.insert(TABLE_PATH.into(), table);
        },
        TABLE_PATH,
    );
    assert!(
        message.ends_with("expected a SpreadsheetML table"),
        "{message}"
    );
    // A missing target, an external target, and a target outside the
    // package are the relationships part's errors.
    let message = invalid(
        &|parts| {
            parts.remove(TABLE_PATH);
        },
        SHEET_RELS,
    );
    assert!(
        message.ends_with(&format!("missing table part {TABLE_PATH}")),
        "{message}"
    );
    invalid(
        &|parts| {
            let rels = parts[SHEET_RELS].replace(
                "Target='../tables/table1.xml'",
                "Target='../tables/table1.xml' TargetMode='External'",
            );
            parts.insert(SHEET_RELS.into(), rels);
        },
        SHEET_RELS,
    );
    invalid(
        &|parts| {
            let rels = parts[SHEET_RELS]
                .replace("Target='../tables/table1.xml'", "Target='../../../x.xml'");
            parts.insert(SHEET_RELS.into(), rels);
        },
        SHEET_RELS,
    );
    invalid(
        &|parts| {
            let rels = parts[SHEET_RELS].replace("<Relationships ", "<Other ");
            let rels = rels.replace("</Relationships>", "</Other>");
            parts.insert(SHEET_RELS.into(), rels);
        },
        SHEET_RELS,
    );
}

#[test]
fn unreadable_table_parts_name_the_part_with_their_own_variant() {
    for (part, damage, expected) in [
        (TABLE_PATH, "truncated", Error::Xml(String::new())),
        (SHEET_RELS, "truncated", Error::Xml(String::new())),
        (TABLE_PATH, "doctype", Error::Unsupported(String::new())),
        (SHEET_RELS, "doctype", Error::Unsupported(String::new())),
    ] {
        let mut parts = parts("ref='A1:B3'");
        let xml = &parts[part];
        let damaged = match damage {
            "truncated" => xml[..xml.len() / 2].to_owned(),
            _ => format!("<!DOCTYPE x>{xml}"),
        };
        parts.insert(part.into(), damaged);
        let mut book = book(&parts);
        let error = book.set_cell("Data", "B2", 7).unwrap_err();
        assert_eq!(
            std::mem::discriminant(&error),
            std::mem::discriminant(&expected),
            "{part} {damage}: {error:?}"
        );
        let message = match &error {
            Error::Xml(message) | Error::Unsupported(message) => message,
            other => panic!("{other:?}"),
        };
        assert!(
            message.starts_with(&format!("part {part:?}: ")),
            "{message}"
        );
        assert!(!message.contains("worksheet \""), "{message}");
        assert!(!book.has_changes());
        // The worksheet itself stays readable.
        assert_eq!(
            book.get_cell("Data", "B2").unwrap().value,
            CellValue::Number(5.0)
        );
    }
}
