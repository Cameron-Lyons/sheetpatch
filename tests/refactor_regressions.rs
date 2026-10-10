//! Exercise transitions between original, pending, and prepared worksheet data.
use std::{collections::BTreeMap, io::Read};

use flate2::read::DeflateDecoder;
use sheetpatch::{CellEdit, CellRef, CellValue, Error, Workbook, WorksheetView};

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const DATA_PATH: &str = "xl/worksheets/sheet1.xml";
const DATA: &str = "<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' xmlns:v='urn:vendor'><dimension ref='A1:D1'/><sheetData><row r='1'><c r='A1' t='inlineStr' s='1'><is><t xml:space=\"preserve\">original</t></is><extLst><ext uri='keep'><v:tree><v:leaf a='1'>opaque</v:leaf></v:tree></ext></extLst></c><c r='B1'><v>2</v></c><c r='C1' t='s'><v>0</v></c><c r='D1'><f>B1*2</f><v>4</v></c></row></sheetData><v:future>keep</v:future></worksheet>";

fn fixture() -> Vec<u8> {
    let workbook = format!(
        "<workbook xmlns='{MAIN_NS}' xmlns:r='{OFFICE_NS}'><sheets><sheet name='Data' sheetId='1' r:id='data'/><sheet name='Other' sheetId='2' r:id='other'/></sheets></workbook>"
    );
    let relationships = format!(
        "<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='data' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet1.xml'/><Relationship Id='other' Type='{OFFICE_NS}/worksheet' Target='worksheets/sheet2.xml'/><Relationship Id='strings' Type='{OFFICE_NS}/sharedStrings' Target='sharedStrings.xml'/></Relationships>"
    );
    let other = format!(
        "<worksheet xmlns='{MAIN_NS}'><sheetData><row r='1'><c r='A1'><v>5</v></c></row></sheetData></worksheet>"
    );
    let shared =
        format!("<sst xmlns='{MAIN_NS}'><si><r><t>shared </t></r><r><t>☕</t></r></si></sst>");
    stored_archive(&[
        ("[Content_Types].xml", b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='rels' ContentType='application/vnd.openxmlformats-package.relationships+xml'/><Default Extension='xml' ContentType='application/xml'/></Types>"),
        ("_rels/.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='office' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", relationships.as_bytes()),
        (DATA_PATH, DATA.as_bytes()),
        ("xl/worksheets/sheet2.xml", other.as_bytes()),
        ("xl/sharedStrings.xml", shared.as_bytes()),
        ("vendor/opaque.bin", b"\0\xffuntouched\x13"),
    ])
}

fn stored_archive(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut directory = Vec::new();
    for &(name, content) in parts {
        let offset = archive.len() as u32;
        let crc = crc32fast::hash(content);
        archive.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        for value in [20u16, 0, 0, 0, 0] {
            archive.extend_from_slice(&value.to_le_bytes());
        }
        for value in [crc, content.len() as u32, content.len() as u32] {
            archive.extend_from_slice(&value.to_le_bytes());
        }
        archive.extend_from_slice(&(name.len() as u16).to_le_bytes());
        archive.extend_from_slice(&0u16.to_le_bytes());
        archive.extend_from_slice(name.as_bytes());
        archive.extend_from_slice(content);
        directory.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        for value in [20u16, 20, 0, 0, 0, 0] {
            directory.extend_from_slice(&value.to_le_bytes());
        }
        for value in [crc, content.len() as u32, content.len() as u32] {
            directory.extend_from_slice(&value.to_le_bytes());
        }
        directory.extend_from_slice(&(name.len() as u16).to_le_bytes());
        for _ in 0..4 {
            directory.extend_from_slice(&0u16.to_le_bytes());
        }
        directory.extend_from_slice(&0u32.to_le_bytes());
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name.as_bytes());
    }
    let offset = archive.len() as u32;
    archive.extend_from_slice(&directory);
    archive.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    for value in [0u16, 0, parts.len() as u16, parts.len() as u16] {
        archive.extend_from_slice(&value.to_le_bytes());
    }
    archive.extend_from_slice(&(directory.len() as u32).to_le_bytes());
    archive.extend_from_slice(&offset.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive
}

fn payloads(archive: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let u16_at = |at| u16::from_le_bytes(archive[at..at + 2].try_into().unwrap()) as usize;
    let u32_at = |at| u32::from_le_bytes(archive[at..at + 4].try_into().unwrap()) as usize;
    let end = archive.len() - 22;
    assert_eq!(&archive[end..end + 4], b"PK\x05\x06");
    let mut at = u32_at(end + 16);
    let mut parts = BTreeMap::new();
    for _ in 0..u16_at(end + 10) {
        assert_eq!(&archive[at..at + 4], b"PK\x01\x02");
        let name_len = u16_at(at + 28);
        let name = String::from_utf8(archive[at + 46..at + 46 + name_len].to_vec()).unwrap();
        let local = u32_at(at + 42);
        assert_eq!(&archive[local..local + 4], b"PK\x03\x04");
        let start = local + 30 + u16_at(local + 26) + u16_at(local + 28);
        let compressed = &archive[start..start + u32_at(at + 20)];
        let bytes = match u16_at(at + 10) {
            0 => compressed.to_vec(),
            8 => {
                let mut bytes = Vec::new();
                DeflateDecoder::new(compressed)
                    .read_to_end(&mut bytes)
                    .unwrap();
                bytes
            }
            method => panic!("unexpected compression: {method}"),
        };
        assert_eq!(bytes.len(), u32_at(at + 24), "uncompressed size: {name}");
        assert_eq!(
            crc32fast::hash(&bytes) as usize,
            u32_at(at + 16),
            "CRC: {name}"
        );
        assert!(parts.insert(name, bytes).is_none(), "duplicate ZIP member");
        at += 46 + name_len + u16_at(at + 30) + u16_at(at + 32);
    }
    parts
}

fn fixture_with_replacements(replacements: &[(&str, &[u8])]) -> Vec<u8> {
    let mut parts = payloads(&fixture());
    for &(path, bytes) in replacements {
        parts.insert(path.into(), bytes.to_vec());
    }
    let borrowed: Vec<_> = parts
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();
    stored_archive(&borrowed)
}

#[test]
fn prepared_reads_follow_committed_edits_and_reset_and_keep_exact_xml() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Workbook>();
    send_sync::<WorksheetView<'_>>();
    let original = fixture();
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    let addresses = ["C1", "A1", "D1", "Z99", "A1"];
    let original_values = book.get_cells("Data", addresses).unwrap();
    assert_eq!(original_values[0].value, CellValue::from("shared ☕"));
    {
        let view = book.prepare_sheet("Data").unwrap();
        assert_eq!(view.sheet().name(), "Data");
        assert_eq!(view.sheet().path(), DATA_PATH);
        assert_eq!(view.get_cells(addresses).unwrap(), original_values);
        assert_eq!(view.get_cell("a1").unwrap(), original_values[1]);
        assert_eq!(
            view.get_cell_at(CellRef::new(1, 4).unwrap()).unwrap(),
            original_values[2]
        );
        assert_eq!(
            view.get_cells_at([CellRef::new(1, 1).unwrap(); 2]).unwrap(),
            vec![original_values[1].clone(); 2]
        );
    }
    assert_eq!(book.to_bytes().unwrap(), original);
    book.set_cells("Data", [("B1", 7), ("B1", 9)]).unwrap();
    book.set_cell("Data", "A1", "updated").unwrap();
    let pending = book.to_bytes().unwrap();
    let expected = DATA
        .replace("<v>2</v>", "<v>9</v>")
        .replace(">original</t>", ">updated</t>");
    let mut expected_parts = payloads(&original);
    expected_parts.insert(DATA_PATH.into(), expected.into_bytes());
    assert_eq!(payloads(&pending), expected_parts);
    {
        let view = book.prepare_sheet("Data").unwrap();
        assert_eq!(view.get_cell("B1").unwrap().value, CellValue::Number(9.0));
        assert_eq!(
            view.get_cell("A1").unwrap().value,
            CellValue::from("updated")
        );
        assert_eq!(view.get_cell("D1").unwrap(), original_values[2]);
    }
    book.set_cells("Data", [("B1", 9)]).unwrap();
    assert_eq!(book.to_bytes().unwrap(), pending);
    assert!(matches!(
        book.apply_edits([
            CellEdit::new("Other", "A1", 10).unwrap(),
            CellEdit::new("Data", "D1", 11).unwrap(),
        ]),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(book.to_bytes().unwrap(), pending);
    assert_eq!(
        book.prepare_sheet("Other")
            .unwrap()
            .get_cell("A1")
            .unwrap()
            .value,
        CellValue::Number(5.0)
    );
    book.reset_changes();
    assert_eq!(
        book.prepare_sheet("Data")
            .unwrap()
            .get_cells(addresses)
            .unwrap(),
        original_values
    );
    assert_eq!(book.to_bytes().unwrap(), original);
    book.set_cell("Data", "B1", 20).unwrap();
    assert_eq!(
        book.prepare_sheet("Data")
            .unwrap()
            .get_cell("B1")
            .unwrap()
            .value,
        CellValue::Number(20.0)
    );
    book.set_cell("Data", "B1", 2).unwrap();
    assert!(!book.has_changes());
    assert_eq!(book.to_bytes().unwrap(), original);
    assert_eq!(
        book.prepare_sheet("Data")
            .unwrap()
            .get_cells(addresses)
            .unwrap(),
        original_values
    );
}

#[test]
fn prepared_view_keeps_address_errors_missing_sheet_and_read_order() {
    let book = Workbook::from_bytes(fixture()).unwrap();
    assert!(matches!(
        book.prepare_sheet("data"),
        Err(Error::SheetNotFound(_))
    ));
    let view = book.prepare_sheet("Data").unwrap();
    assert!(
        view.get_cells(std::iter::empty::<&str>())
            .unwrap()
            .is_empty()
    );
    assert!(view.get_cells_at([]).unwrap().is_empty());
    assert!(matches!(
        view.get_cell("A0"),
        Err(Error::InvalidCellReference(_))
    ));
    assert!(matches!(
        view.get_cells(["A1", "A0"]),
        Err(Error::InvalidCellReference(_))
    ));
    let values = view.get_cells(["Z99", "B1", "B1"]).unwrap();
    assert_eq!(values[0].value, CellValue::Blank);
    assert_eq!(values[1], values[2]);
    assert_eq!(values[1].value, CellValue::Number(2.0));
}

#[test]
fn sparse_edits_preserve_spacing_and_order_insertions_at_shared_gaps() {
    let sheet = format!(
        "<worksheet xmlns='{MAIN_NS}' xmlns:v='urn:vendor'><dimension ref = 'B2:B2' /><sheetData ><row r = '2' spans = '2:2' ><c r = 'B2' t = 'b' s = '1' /><extLst><ext uri='keep'><v:tail k='unchanged'/></ext></extLst></row><row r = '4' spans = '2:2' /></sheetData><v:future a='keep'/></worksheet>"
    );
    let original = fixture_with_replacements(&[(DATA_PATH, sheet.as_bytes())]);
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    book.set_cells(
        "Data",
        [
            ("C4", CellValue::Number(24.0)),
            ("A3", CellValue::from("three")),
            ("B2", CellValue::Bool(true)),
            ("A2", CellValue::Number(11.0)),
            ("C3", CellValue::Number(13.0)),
            ("A4", CellValue::Number(14.0)),
            ("C2", CellValue::Bool(false)),
        ],
    )
    .unwrap();
    let expected = format!(
        "<worksheet xmlns='{MAIN_NS}' xmlns:v='urn:vendor'><dimension ref = 'A2:C4' /><sheetData ><row r = '2' spans = '1:3' ><c r=\"A2\"><v>11</v></c><c r = 'B2' t = 'b' s = '1' ><v>1</v></c><c r=\"C2\" t=\"b\"><v>0</v></c><extLst><ext uri='keep'><v:tail k='unchanged'/></ext></extLst></row><row r=\"3\"><c r=\"A3\" t=\"inlineStr\"><is><t xml:space=\"preserve\">three</t></is></c><c r=\"C3\"><v>13</v></c></row><row r = '4' spans = '1:3' ><c r=\"A4\"><v>14</v></c><c r=\"C4\"><v>24</v></c></row></sheetData><v:future a='keep'/></worksheet>"
    );
    let mut expected_parts = payloads(&original);
    expected_parts.insert(DATA_PATH.into(), expected.into_bytes());
    for output in [book.to_bytes().unwrap(), book.to_bytes_compact().unwrap()] {
        assert_eq!(payloads(&output), expected_parts);
    }
    let addresses = ["C4", "B2", "A3", "C2", "B2", "Z99"];
    let values = book.get_cells("Data", addresses).unwrap();
    assert_eq!(values[0].value, CellValue::Number(24.0));
    assert_eq!(values[1].style_index, Some(1));
    assert_eq!(values[1].value, CellValue::Bool(true));
    assert_eq!(values[2].value, CellValue::from("three"));
    assert_eq!(values[3].value, CellValue::Bool(false));
    assert_eq!(values[1], values[4]);
    assert_eq!(values[5].value, CellValue::Blank);
    assert_eq!(
        book.prepare_sheet("Data")
            .unwrap()
            .get_cells(addresses)
            .unwrap(),
        values
    );
}

#[test]
fn empty_sheet_expansion_retains_whitespace_and_sorts_new_rows_and_cells() {
    let sheet = format!("<worksheet xmlns='{MAIN_NS}'><sheetData \t/></worksheet>");
    let original = fixture_with_replacements(&[(DATA_PATH, sheet.as_bytes())]);
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    book.set_cells(
        "Data",
        [
            ("C2", CellValue::Bool(true)),
            ("B1", CellValue::from("first")),
            ("A1", CellValue::Number(1.0)),
            ("B1", CellValue::from("last")),
            ("Z99", CellValue::Blank),
        ],
    )
    .unwrap();
    let expected = format!(
        "<worksheet xmlns='{MAIN_NS}'><sheetData \t><row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"B1\" t=\"inlineStr\"><is><t xml:space=\"preserve\">last</t></is></c></row><row r=\"2\"><c r=\"C2\" t=\"b\"><v>1</v></c></row></sheetData></worksheet>"
    );
    let mut expected_parts = payloads(&original);
    expected_parts.insert(DATA_PATH.into(), expected.into_bytes());
    assert_eq!(payloads(&book.to_bytes().unwrap()), expected_parts);
    assert_eq!(
        book.prepare_sheet("Data")
            .unwrap()
            .get_cell("B1")
            .unwrap()
            .value,
        CellValue::from("last")
    );
    book.reset_changes();
    assert_eq!(book.to_bytes().unwrap(), original);
    assert_eq!(
        book.prepare_sheet("Data")
            .unwrap()
            .get_cell("B1")
            .unwrap()
            .value,
        CellValue::Blank
    );
}

#[test]
fn prepared_reads_match_direct_errors_for_malformed_ranges_and_hidden_content() {
    let cases = [
        "<sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData><mergeCells><mergeCell ref='B1:A1'/></mergeCells>",
        "<sheetData><row r='1'><c r='A1'><f t='shared' ref='A1:B1'>1</f><v>1</v></c></row></sheetData>",
        "<sheetData><row r='1'><c r='A1'><f t='shared' si='bad' ref='A1:B1'>1</f><v>1</v></c></row></sheetData>",
        "<sheetData><row r='1'><c r='A1'><v>1</v><extLst><ext uri='keep'><v:foreign><f>1</f></v:foreign></ext></extLst></c></row></sheetData>",
        "<sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData><v:foreign><mergeCells><mergeCell ref='A1:B1'/></mergeCells></v:foreign>",
        "<sheetData><v:foreign><row r='1'><c r='A1'><v>1</v></c></row></v:foreign></sheetData>",
        "<sheetData><row r='1'><c r='A1' t='inlineStr'><is><v:foreign><t>hidden</t></v:foreign></is></c></row></sheetData>",
    ];
    for body in cases {
        let sheet = format!("<worksheet xmlns='{MAIN_NS}' xmlns:v='urn:vendor'>{body}</worksheet>");
        let original = fixture_with_replacements(&[(DATA_PATH, sheet.as_bytes())]);
        let book = Workbook::from_bytes(original.clone()).unwrap();
        let direct = book.get_cell("Data", "A1").unwrap_err();
        let prepared = match book.prepare_sheet("Data") {
            Ok(view) => view.get_cell("A1").unwrap_err(),
            Err(error) => error,
        };
        assert_eq!(prepared.to_string(), direct.to_string(), "{body}");
        assert!(matches!(direct, Error::Unsupported(_)), "{body}: {direct}");
        assert_eq!(book.to_bytes().unwrap(), original);
        assert!(!book.has_changes());
    }
}

#[test]
fn prepared_shared_string_errors_remain_lazy_and_match_direct_reads() {
    let strings = format!(
        "<sst xmlns='{MAIN_NS}' xmlns:v='urn:vendor'><v:hidden><si><t>hidden entry</t></si></v:hidden><si><t>visible entry</t></si></sst>"
    );
    let original = fixture_with_replacements(&[("xl/sharedStrings.xml", strings.as_bytes())]);
    let book = Workbook::from_bytes(original.clone()).unwrap();
    let view = book.prepare_sheet("Data").unwrap();
    assert_eq!(view.get_cell("B1").unwrap().value, CellValue::Number(2.0));
    assert_eq!(
        view.get_cell("D1").unwrap().formula.as_deref(),
        Some("B1*2")
    );
    let direct = book.get_cell("Data", "C1").unwrap_err();
    let prepared = view.get_cell("C1").unwrap_err();
    assert_eq!(prepared.to_string(), direct.to_string());
    assert!(matches!(direct, Error::Unsupported(_)));
    assert_eq!(book.to_bytes().unwrap(), original);
}
