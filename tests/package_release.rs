use sheetpatch::{CellValue, Workbook};
use std::collections::BTreeMap;

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const SHEET: &str = "<sheet name='Data' sheetId='1' r:id='one'/>";
const RELATIONSHIP: &str = "<Relationship Id='one' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet' Target='worksheets/sheet1.xml'/>";

fn fixture(sheets: &str, relationships: &str, replacements: &[(&str, &str)]) -> Vec<u8> {
    let worksheet = format!(
        "<worksheet xmlns='{MAIN_NS}'><sheetData><row r='1'><c r='A1'><v>7</v></c></row></sheetData></worksheet>"
    );
    let mut parts = BTreeMap::from([
        (
            "[Content_Types].xml",
            "<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='rels' ContentType='application/vnd.openxmlformats-package.relationships+xml'/><Default Extension='xml' ContentType='application/xml'/><Override PartName='/xl/workbook.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml'/><Override PartName='/xl/worksheets/sheet1.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml'/><Override PartName='/xl/worksheets/sheet2.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml'/></Types>".to_owned(),
        ),
        (
            "_rels/.rels",
            format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='office' Type='{OFFICE_NS}/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ),
        (
            "xl/workbook.xml",
            format!("<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'>{sheets}</workbook>"),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            format!("<Relationships xmlns='{PACKAGE_NS}'>{relationships}</Relationships>"),
        ),
        ("xl/worksheets/sheet1.xml", worksheet.clone()),
        ("xl/worksheets/sheet2.xml", worksheet),
    ]);
    for &(name, xml) in replacements {
        parts.insert(name, xml.to_owned());
    }
    stored_archive(&parts)
}

// Independent, minimal ZIP writer: package tests do not use the crate's writer
// to construct the archive that its package discovery must accept or reject.
fn stored_archive(parts: &BTreeMap<&str, String>) -> Vec<u8> {
    let mut output = Vec::new();
    let mut central = Vec::new();
    for (&name, xml) in parts {
        let bytes = xml.as_bytes();
        let offset = output.len() as u32;
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        let crc = !crc;
        output.extend_from_slice(&0x04034b50u32.to_le_bytes());
        for value in [20u16, 0, 0, 0, 0] {
            output.extend_from_slice(&value.to_le_bytes());
        }
        for value in [crc, bytes.len() as u32, bytes.len() as u32] {
            output.extend_from_slice(&value.to_le_bytes());
        }
        output.extend_from_slice(&(name.len() as u16).to_le_bytes());
        output.extend_from_slice(&0u16.to_le_bytes());
        output.extend_from_slice(name.as_bytes());
        output.extend_from_slice(bytes);

        central.extend_from_slice(&0x02014b50u32.to_le_bytes());
        for value in [20u16, 20, 0, 0, 0, 0] {
            central.extend_from_slice(&value.to_le_bytes());
        }
        for value in [crc, bytes.len() as u32, bytes.len() as u32] {
            central.extend_from_slice(&value.to_le_bytes());
        }
        for value in [name.len() as u16, 0, 0, 0, 0] {
            central.extend_from_slice(&value.to_le_bytes());
        }
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let offset = output.len() as u32;
    output.extend_from_slice(&central);
    output.extend_from_slice(&0x06054b50u32.to_le_bytes());
    for value in [0u16, 0, parts.len() as u16, parts.len() as u16] {
        output.extend_from_slice(&value.to_le_bytes());
    }
    output.extend_from_slice(&(central.len() as u32).to_le_bytes());
    output.extend_from_slice(&offset.to_le_bytes());
    output.extend_from_slice(&0u16.to_le_bytes());
    output
}

#[test]
fn worksheet_names_decode_excel_escapes_and_preserve_archive_bytes() {
    let original = fixture(
        "<sheets><sheet name='Data_x0020_&amp;_xD83D__xDE00_' sheetId='1' r:id='one'/><sheet name='literal_x005F_x0041_' sheetId='2' r:id='two'/></sheets>",
        &format!(
            "{RELATIONSHIP}<Relationship Id='two' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet2.xml'/>"
        ),
        &[],
    );
    let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
    assert_eq!(workbook.sheets()[0].name(), "Data &😀");
    assert_eq!(workbook.sheets()[1].name(), "literal_x0041_");
    assert_eq!(workbook.to_bytes().unwrap(), original);
    assert_eq!(
        workbook.get_cell("Data &😀", "A1").unwrap().value,
        CellValue::Number(7.0)
    );
    workbook.set_cell("literal_x0041_", "A1", 12).unwrap();
    let reopened = Workbook::from_bytes(workbook.to_bytes().unwrap()).unwrap();
    assert_eq!(
        reopened.get_cell("literal_x0041_", "A1").unwrap().value,
        CellValue::Number(12.0)
    );
    assert!(reopened.get_cell("Literal_x0041_", "A1").is_err());
}

#[test]
fn workbook_discovery_rejects_ambiguous_sheet_locations() {
    for sheets in [
        String::new(),
        format!("<sheets>{SHEET}</sheets><sheets/>"),
        format!("<extension><sheets>{SHEET}</sheets></extension>"),
        format!("<sheets/><extension>{SHEET}</extension>"),
        format!("<sheets><extension>{SHEET}</extension></sheets>"),
    ] {
        assert!(
            Workbook::from_bytes(fixture(&sheets, RELATIONSHIP, &[])).is_err(),
            "{sheets}"
        );
    }
}

#[test]
fn duplicate_decoded_names_are_rejected_for_all_sheet_types() {
    for kind in ["worksheet", "chartsheet", "macrosheet"] {
        let sheets =
            format!("<sheets>{SHEET}<sheet name='_x0044_ata' sheetId='2' r:id='two'/></sheets>");
        let rels = format!(
            "{RELATIONSHIP}<Relationship Id='two' Type='{OFFICE_NS}/{kind}' Target='worksheets/sheet2.xml'/>"
        );
        assert!(Workbook::from_bytes(fixture(&sheets, &rels, &[])).is_err());
    }
    let invalid = "<sheets><sheet name='_xD800_' sheetId='1' r:id='one'/></sheets>";
    assert!(Workbook::from_bytes(fixture(invalid, RELATIONSHIP, &[])).is_err());
}

#[test]
fn unrelated_sheet_types_and_foreign_elements_remain_opaque() {
    let sheets = format!(
        "<sheets xmlns:x='urn:vendor'>{SHEET}<x:sheet name='Opaque'/><sheet name='Chart' sheetId='2' r:id='two'/></sheets>"
    );
    let rels = format!(
        "{RELATIONSHIP}<Relationship Id='two' Type='{OFFICE_NS}/chartsheet' Target='worksheets/sheet2.xml'/>"
    );
    let original = fixture(&sheets, &rels, &[]);
    let workbook = Workbook::from_bytes(original.clone()).unwrap();
    assert_eq!(workbook.sheets().len(), 1);
    assert_eq!(workbook.sheets()[0].name(), "Data");
    assert_eq!(workbook.to_bytes().unwrap(), original);
}

#[test]
fn accessed_package_metadata_must_be_complete_xml_of_the_expected_type() {
    for xml in [
        "<Types>",
        "<Types/>",
        "<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'/>",
    ] {
        let bytes = fixture(
            &format!("<sheets>{SHEET}</sheets>"),
            RELATIONSHIP,
            &[("[Content_Types].xml", xml)],
        );
        assert!(Workbook::from_bytes(bytes).is_err(), "{xml}");
    }
    let hidden = format!("<extension>{RELATIONSHIP}</extension>");
    assert!(
        Workbook::from_bytes(fixture(&format!("<sheets>{SHEET}</sheets>"), &hidden, &[])).is_err()
    );
}

#[test]
fn binary_workbooks_are_classified_independently_of_zip_name_case() {
    for name in ["xl/workbook.bin", "xl/workbook.BIN"] {
        let roots = format!(
            "<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='office' Type='{OFFICE_NS}/officeDocument' Target='xl/workbook.bin'/></Relationships>"
        );
        let original = fixture(
            &format!("<sheets>{SHEET}</sheets>"),
            RELATIONSHIP,
            &[("_rels/.rels", &roots), (name, "binary\0workbook")],
        );
        assert!(matches!(
            Workbook::from_bytes(original),
            Err(sheetpatch::Error::Unsupported(message)) if message.contains(".xlsb")
        ));
    }
}

#[test]
fn namespace_character_references_are_normalized_before_discovery_and_editing() {
    let encoded_main = MAIN_NS.replace("/main", "/&#109;ain");
    let encoded_office = OFFICE_NS.replace("/relationships", "/relation&#115;hips");
    let encoded_package = PACKAGE_NS.replace("/relationships", "/relation&#115;hips");
    let workbook_xml = format!(
        "<workbook xmlns='{encoded_main}' xmlns:r='{encoded_office}'><sheets>{SHEET}</sheets></workbook>"
    );
    let root_rels = format!(
        "<Relationships xmlns='{encoded_package}'><Relationship Id='office' Type='{OFFICE_NS}/officeDocument' Target='xl/workbook.xml'/></Relationships>"
    );
    let worksheet_xml = format!(
        "<worksheet xmlns='{encoded_main}'><sheetData><row r='1'><c r='A1'><v>7</v></c></row></sheetData></worksheet>"
    );
    let original = fixture(
        &format!("<sheets>{SHEET}</sheets>"),
        RELATIONSHIP,
        &[
            ("xl/workbook.xml", &workbook_xml),
            ("_rels/.rels", &root_rels),
            ("xl/worksheets/sheet1.xml", &worksheet_xml),
        ],
    );
    let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
    assert_eq!(workbook.to_bytes().unwrap(), original);
    assert_eq!(
        workbook.get_cell("Data", "A1").unwrap().value,
        CellValue::Number(7.0)
    );
    workbook.set_cell("Data", "A1", "normalized").unwrap();
    let reopened = Workbook::from_bytes(workbook.to_bytes().unwrap()).unwrap();
    assert_eq!(
        reopened.get_cell("Data", "A1").unwrap().value,
        CellValue::Text("normalized".into())
    );
}

#[test]
fn duplicate_expanded_attributes_use_normalized_namespace_names() {
    let workbook_xml = format!(
        "<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}' xmlns:s='{}'><sheets><sheet name='Data' sheetId='1' r:id='one' s:id='one'/></sheets></workbook>",
        OFFICE_NS.replace("/relationships", "/relation&#115;hips")
    );
    let bytes = fixture(
        &format!("<sheets>{SHEET}</sheets>"),
        RELATIONSHIP,
        &[("xl/workbook.xml", &workbook_xml)],
    );
    assert!(Workbook::from_bytes(bytes).is_err());
}

#[test]
fn empty_batches_require_an_existing_worksheet_without_creating_changes() {
    let original = fixture(&format!("<sheets>{SHEET}</sheets>"), RELATIONSHIP, &[]);
    let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
    workbook
        .set_cells("Data", std::iter::empty::<(&str, i32)>())
        .unwrap();
    assert!(
        workbook
            .set_cells("missing", std::iter::empty::<(&str, i32)>())
            .is_err()
    );
    assert!(
        workbook
            .get_cells("missing", std::iter::empty::<&str>())
            .is_err()
    );
    assert!(!workbook.has_changes());
    assert_eq!(workbook.to_bytes().unwrap(), original);
}

#[test]
fn duplicate_batch_addresses_validate_every_input_and_keep_pending_changes_on_failure() {
    let original = fixture(&format!("<sheets>{SHEET}</sheets>"), RELATIONSHIP, &[]);
    let mut workbook = Workbook::from_bytes(original).unwrap();
    workbook.set_cell("Data", "B2", "pending").unwrap();
    let before = workbook.to_bytes().unwrap();
    for invalid in [
        CellValue::Number(f64::NAN),
        CellValue::Number(f64::INFINITY),
        CellValue::Text("invalid\0text".into()),
    ] {
        assert!(
            workbook
                .set_cells("Data", [("A1", invalid), ("a1", CellValue::Number(12.0))])
                .is_err()
        );
        assert_eq!(workbook.to_bytes().unwrap(), before);
    }
    workbook
        .set_cells("Data", [("A1", 11), ("a1", 12)])
        .unwrap();
    let reopened = Workbook::from_bytes(workbook.to_bytes().unwrap()).unwrap();
    assert_eq!(
        reopened.get_cell("Data", "A1").unwrap().value,
        CellValue::Number(12.0)
    );
    assert_eq!(
        reopened.get_cell("Data", "B2").unwrap().value,
        CellValue::Text("pending".into())
    );
}

fn escaped_part_fixture(decoded_names: bool, include_decoys: bool) -> Vec<u8> {
    let workbook_name = if decoded_names {
        "books/Workbook Main.xml"
    } else {
        "books/Workbook%20Main.xml"
    };
    let rels_name = if decoded_names {
        "books/_rels/Workbook Main.xml.rels"
    } else {
        "books/_rels/Workbook%20Main.xml.rels"
    };
    let sheet_name = if decoded_names {
        "tabs/Sheet ?.xml"
    } else {
        "tabs/Sheet%20%3F.xml"
    };
    let strings_name = if decoded_names {
        "strings/Shared Text.xml"
    } else {
        "strings/Shared%20Text.xml"
    };
    let worksheet = format!(
        "<worksheet xmlns='{MAIN_NS}'><sheetData><row r='1'><c r='A1' t='s'><v>0</v></c><c r='B1'><v>7</v></c></row></sheetData></worksheet>"
    );
    let mut parts = BTreeMap::from([
        ("[Content_Types].xml", "<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='rels' ContentType='application/vnd.openxmlformats-package.relationships+xml'/><Default Extension='xml' ContentType='application/xml'/><Override PartName='/books/Workbook%20Main.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml'/><Override PartName='/tabs/Sheet%20%3F.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml'/><Override PartName='/strings/Shared%20Text.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml'/></Types>".into()),
        ("_rels/.rels", format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='office' Type='{OFFICE_NS}/officeDocument' Target='BOOKS/workbook%20main.xml'/></Relationships>")),
        (workbook_name, format!("<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheets><sheet name='Data' sheetId='1' r:id='one'/></sheets></workbook>")),
        (rels_name, format!("<Relationships xmlns='{PACKAGE_NS}'><Relationship Id='one' Type='{OFFICE_NS}/worksheet' Target='../tabs/sheet%20%3f.xml'/><Relationship Id='strings' Type='{OFFICE_NS}/sharedStrings' Target='/strings/shared%20text.xml'/></Relationships>")),
        (sheet_name, worksheet),
        (strings_name, format!("<sst xmlns='{MAIN_NS}'><si><t>Canonical</t></si></sst>")),
    ]);
    if include_decoys {
        parts.insert("tabs/Sheet ?.xml", format!("<worksheet xmlns='{MAIN_NS}'><sheetData><row r='1'><c r='A1'><v>99</v></c></row></sheetData></worksheet>"));
        parts.insert(
            "strings/Shared Text.xml",
            format!("<sst xmlns='{MAIN_NS}'><si><t>Wrong</t></si></sst>"),
        );
    }
    stored_archive(&parts)
}

#[test]
fn escaped_opc_part_names_discover_read_edit_and_compact_without_decoding_zip_names() {
    for include_decoys in [false, true] {
        let original = escaped_part_fixture(false, include_decoys);
        let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
        assert_eq!(workbook.sheets()[0].path(), "tabs/Sheet%20%3F.xml");
        assert_eq!(workbook.to_bytes().unwrap(), original);
        assert_eq!(
            workbook.get_cell("Data", "A1").unwrap().value,
            CellValue::Text("Canonical".into())
        );
        workbook.set_cell("Data", "B1", 12).unwrap();
        for output in [
            workbook.to_bytes().unwrap(),
            workbook.to_bytes_compact().unwrap(),
        ] {
            let reopened = Workbook::from_bytes(output).unwrap();
            assert_eq!(reopened.sheets()[0].path(), "tabs/Sheet%20%3F.xml");
            assert_eq!(
                reopened.get_cell("Data", "A1").unwrap().value,
                CellValue::Text("Canonical".into())
            );
            assert_eq!(
                reopened.get_cell("Data", "B1").unwrap().value,
                CellValue::Number(12.0)
            );
        }
    }
}

#[test]
fn legacy_decoded_zip_part_names_remain_readable_when_encoded_parts_are_absent() {
    let original = escaped_part_fixture(true, false);
    let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
    assert_eq!(workbook.sheets()[0].path(), "tabs/Sheet ?.xml");
    assert_eq!(workbook.to_bytes().unwrap(), original);
    assert_eq!(
        workbook.get_cell("Data", "A1").unwrap().value,
        CellValue::Text("Canonical".into())
    );
    workbook.set_cell("Data", "B1", 12).unwrap();
    let reopened = Workbook::from_bytes(workbook.to_bytes().unwrap()).unwrap();
    assert_eq!(
        reopened.get_cell("Data", "B1").unwrap().value,
        CellValue::Number(12.0)
    );
}

#[test]
fn equivalent_worksheet_zip_names_are_rejected_before_editing() {
    let duplicate = format!("<worksheet xmlns='{MAIN_NS}'><sheetData/></worksheet>");
    let original = fixture(
        &format!("<sheets>{SHEET}</sheets>"),
        RELATIONSHIP,
        &[("XL/WORKSHEETS/SHEET1.XML", &duplicate)],
    );
    assert!(matches!(
        Workbook::from_bytes(original),
        Err(sheetpatch::Error::InvalidWorkbook(message)) if message.contains("ambiguous equivalent OPC part names")
    ));
}
