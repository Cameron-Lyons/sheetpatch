use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};
use sheetpatch::{CellEdit, CellRef, CellValue, Workbook};

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const WORKSHEET: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:x="urn:vendor" x:mode="keep"><dimension ref="A1:C2"/><sheetViews><sheetView workbookViewId="0"/></sheetViews><sheetData>
<row r="1" ht="24" customHeight="1" x:row="retain"><c r="A1" s="3" t="s" x:cell="retain"><v>0</v><extLst><ext uri="urn:vendor"><x:cellFeature flag="true"/></ext></extLst></c><c r="B1" t="inlineStr"><is><t>Unchanged &amp; literal</t></is></c><c r="C1"><v>99</v></c></row>
<row r="2"><c r="A2" t="b"><v>1</v></c><c r="C2"><v>22</v></c></row>
</sheetData><x:futureFeature x:id="42"><x:payload>keep me exactly</x:payload></x:futureFeature><drawing r:id="drawing1" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"/><extLst><ext uri="urn:vendor"><x:opaque a="1"/></ext></extLst></worksheet>"#;

fn fixture_at(
    worksheet: &str,
    workbook_path: &str,
    sheet_target: &str,
    sheet_path: &str,
) -> Vec<u8> {
    let content_types = format!(
        r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="bin" ContentType="application/vnd.ms-office.vbaProject"/><Override PartName="/{workbook_path}" ContentType="application/vnd.ms-excel.sheet.macroEnabled.main+xml"/><Override PartName="/{sheet_path}" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#
    );
    let root_rels = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="office" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{workbook_path}"/></Relationships>"#
    );
    let workbook = format!(
        r#"<workbook xmlns="{MAIN_NS}" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data &amp; Notes" sheetId="1" r:id="rId1"/></sheets><calcPr calcId="191029"/></workbook>"#
    );
    let workbook_rels = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="{sheet_target}"/><Relationship Id="strings" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="/xl/sharedStrings.xml"/></Relationships>"#
    );
    let (parent, file) = workbook_path.rsplit_once('/').unwrap();
    let workbook_rels_path = format!("{parent}/_rels/{file}.rels");
    let entries: Vec<(&str, &[u8])> = vec![
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", root_rels.as_bytes()),
        (workbook_path, workbook.as_bytes()),
        (&workbook_rels_path, workbook_rels.as_bytes()),
        (sheet_path, worksheet.as_bytes()),
        ("xl/charts/chart1.xml", b"<chartSpace xmlns='urn:chart'><opaque preserve='true'>chart cache</opaque></chartSpace>"),
        ("xl/pivotTables/pivotTable1.xml", b"<pivotTableDefinition name='ExistingPivot' cacheId='7'/><!--retain-->"),
        ("xl/pivotCache/pivotCacheDefinition1.xml", b"<pivotCacheDefinition refreshOnLoad='0'><cacheSource type='worksheet'/></pivotCacheDefinition>"),
        ("xl/pivotCache/pivotCacheRecords1.xml", b"<pivotCacheRecords count='1'><r><n v='17'/></r></pivotCacheRecords>"),
        ("xl/vbaProject.bin", b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1\0opaque macro bytes\xff\x13"),
        ("xl/sharedStrings.xml", b"<sst xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' count='1' uniqueCount='1'><si><t>Old text</t></si></sst>"),
        ("xl/styles.xml", b"<styleSheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><cellXfs count='4'><xf/><xf/><xf/><xf numFmtId='49'/></cellXfs></styleSheet>"),
        ("customXml/item1.xml", b"<?xml version='1.0'?><mystery xmlns='urn:unknown'><value strange='yes'>Opaque custom XML</value></mystery>"),
        ("xl/worksheets/_rels/sheet1.xml.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='drawing1' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing' Target='../drawings/drawing1.xml'/></Relationships>"),
        ("xl/drawings/drawing1.xml", b"<drawing xmlns='urn:drawing'><chart id='1'/></drawing>"),
        ("vendor/unknown.bin", b"\0\x01vendor\xff\xfeuntouched\x03"),
    ];
    archive(&entries)
}

fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut central = Vec::new();
    for &(name, bytes) in entries {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(7));
        encoder.write_all(bytes).unwrap();
        let compressed = encoder.finish().unwrap();
        let crc = crc32(bytes);
        let local_offset = output.len() as u32;
        let entry_comment = b"retain entry metadata";
        put_u32(&mut output, 0x04034b50);
        put_u16(&mut output, 20); // minimum extraction version
        put_u16(&mut output, 0); // flags
        put_u16(&mut output, 8); // raw deflate
        put_u16(&mut output, 0x4220); // DOS time
        put_u16(&mut output, 0x5d47); // DOS date
        put_u32(&mut output, crc);
        put_u32(&mut output, compressed.len() as u32);
        put_u32(&mut output, bytes.len() as u32);
        put_u16(&mut output, name.len() as u16);
        put_u16(&mut output, 0); // extra field length
        output.extend_from_slice(name.as_bytes());
        output.extend_from_slice(&compressed);

        put_u32(&mut central, 0x02014b50);
        put_u16(&mut central, 0x0314); // Unix producer
        put_u16(&mut central, 20);
        put_u16(&mut central, 0);
        put_u16(&mut central, 8);
        put_u16(&mut central, 0x4220);
        put_u16(&mut central, 0x5d47);
        put_u32(&mut central, crc);
        put_u32(&mut central, compressed.len() as u32);
        put_u32(&mut central, bytes.len() as u32);
        put_u16(&mut central, name.len() as u16);
        put_u16(&mut central, 0);
        put_u16(&mut central, entry_comment.len() as u16);
        put_u16(&mut central, 0); // starting disk
        put_u16(&mut central, 0); // internal attributes
        put_u32(&mut central, 0o100640 << 16); // Unix mode
        put_u32(&mut central, local_offset);
        central.extend_from_slice(name.as_bytes());
        central.extend_from_slice(entry_comment);
    }
    let central_offset = output.len();
    output.extend_from_slice(&central);
    put_u32(&mut output, 0x06054b50);
    put_u16(&mut output, 0);
    put_u16(&mut output, 0);
    put_u16(&mut output, entries.len() as u16);
    put_u16(&mut output, entries.len() as u16);
    put_u32(&mut output, central.len() as u32);
    put_u32(&mut output, central_offset as u32);
    let comment = b"Archive comment preserved by sheetpatch";
    put_u16(&mut output, comment.len() as u16);
    output.extend_from_slice(comment);
    output
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

struct ArchiveEntry {
    name: String,
    method: u16,
    crc: u32,
    uncompressed_size: usize,
    compressed_start: usize,
    compressed_size: usize,
    central_bytes: Vec<u8>,
}

fn archive_entries(bytes: &[u8]) -> Vec<ArchiveEntry> {
    let end = bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .unwrap();
    let count = u16_at(bytes, end + 10);
    let mut offset = u32_at(bytes, end + 16) as usize;
    let mut entries = Vec::new();
    for _ in 0..count {
        assert_eq!(u32_at(bytes, offset), 0x02014b50);
        let name_len = u16_at(bytes, offset + 28) as usize;
        let extra_len = u16_at(bytes, offset + 30) as usize;
        let comment_len = u16_at(bytes, offset + 32) as usize;
        let local = u32_at(bytes, offset + 42) as usize;
        assert_eq!(u32_at(bytes, local), 0x04034b50);
        let data_start =
            local + 30 + u16_at(bytes, local + 26) as usize + u16_at(bytes, local + 28) as usize;
        let entry_len = 46 + name_len + extra_len + comment_len;
        entries.push(ArchiveEntry {
            name: String::from_utf8(bytes[offset + 46..offset + 46 + name_len].to_vec()).unwrap(),
            method: u16_at(bytes, offset + 10),
            crc: u32_at(bytes, offset + 16),
            uncompressed_size: u32_at(bytes, offset + 24) as usize,
            compressed_start: data_start,
            compressed_size: u32_at(bytes, offset + 20) as usize,
            central_bytes: bytes[offset..offset + entry_len].to_vec(),
        });
        offset += entry_len;
    }
    entries
}

fn archive_comment(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .unwrap();
    let comment_len = u16_at(bytes, end + 20) as usize;
    &bytes[end + 22..end + 22 + comment_len]
}

fn fixture(worksheet: &str) -> Vec<u8> {
    fixture_at(
        worksheet,
        "xl/workbook.xml",
        "worksheets/sheet1.xml",
        "xl/worksheets/sheet1.xml",
    )
}

fn payload(bytes: &[u8], name: &str) -> Vec<u8> {
    let file = archive_entries(bytes)
        .into_iter()
        .find(|entry| entry.name == name)
        .unwrap();
    let compressed = &bytes[file.compressed_start..file.compressed_start + file.compressed_size];
    let output = match file.method {
        0 => compressed.to_vec(),
        8 => {
            let mut output = Vec::new();
            DeflateDecoder::new(compressed)
                .read_to_end(&mut output)
                .unwrap();
            output
        }
        method => panic!("unexpected compression method: {method}"),
    };
    assert_eq!(output.len(), file.uncompressed_size);
    assert_eq!(crc32(&output), file.crc);
    output
}

fn compressed_payload(bytes: &[u8], name: &str) -> Vec<u8> {
    let file = archive_entries(bytes)
        .into_iter()
        .find(|entry| entry.name == name)
        .unwrap();
    bytes[file.compressed_start..file.compressed_start + file.compressed_size].to_vec()
}

fn worksheet_xml(bytes: &[u8]) -> String {
    String::from_utf8(payload(bytes, "xl/worksheets/sheet1.xml")).unwrap()
}

fn assert_untouched_parts(original: &[u8], updated: &[u8]) {
    let names: Vec<String> = archive_entries(original)
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(archive_entries(updated).len(), names.len());
    for name in names
        .iter()
        .filter(|name| name.as_str() != "xl/worksheets/sheet1.xml")
    {
        assert_eq!(
            payload(updated, name),
            payload(original, name),
            "changed payload in {name}"
        );
        assert_eq!(
            compressed_payload(updated, name),
            compressed_payload(original, name),
            "recompressed {name}"
        );
    }
    assert_eq!(archive_comment(updated), archive_comment(original));
    for entry in archive_entries(original)
        .into_iter()
        .filter(|entry| entry.name != "xl/worksheets/sheet1.xml")
    {
        let updated_entry = archive_entries(updated)
            .into_iter()
            .find(|updated| updated.name == entry.name)
            .unwrap();
        assert_eq!(
            updated_entry.central_bytes, entry.central_bytes,
            "changed metadata in {}",
            entry.name
        );
    }
}

#[test]
fn untouched_parts_retain_original_payload_and_compressed_bytes() {
    let original = fixture(WORKSHEET);
    let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
    workbook
        .set_cell("Data & Notes", "A1", "Updated text")
        .unwrap();
    let updated = workbook.to_bytes().unwrap();
    assert_untouched_parts(&original, &updated);
}

#[test]
fn unknown_xml_and_untouched_cells_remain_byte_identical() {
    let mut workbook = Workbook::from_bytes(fixture(WORKSHEET)).unwrap();
    workbook
        .set_cell("Data & Notes", "A1", "New & <text>")
        .unwrap();
    let xml = worksheet_xml(&workbook.to_bytes().unwrap());
    for fragment in [
        "xmlns:x=\"urn:vendor\" x:mode=\"keep\"",
        "ht=\"24\" customHeight=\"1\" x:row=\"retain\"",
        "s=\"3\"",
        "x:cell=\"retain\"",
        "<extLst><ext uri=\"urn:vendor\"><x:cellFeature flag=\"true\"/></ext></extLst>",
        "<c r=\"B1\" t=\"inlineStr\"><is><t>Unchanged &amp; literal</t></is></c>",
        "<c r=\"C1\"><v>99</v></c>",
        "<row r=\"2\"><c r=\"A2\" t=\"b\"><v>1</v></c><c r=\"C2\"><v>22</v></c></row>",
        "<x:futureFeature x:id=\"42\"><x:payload>keep me exactly</x:payload></x:futureFeature>",
        "<extLst><ext uri=\"urn:vendor\"><x:opaque a=\"1\"/></ext></extLst>",
    ] {
        assert!(
            xml.contains(fragment),
            "lost XML fragment: {fragment}\n{xml}"
        );
    }
    assert!(xml.contains("New &amp; &lt;text&gt;"));
}

#[test]
fn no_op_returns_the_entire_original_archive_unchanged() {
    let original = fixture(WORKSHEET);
    let workbook = Workbook::from_bytes(original.clone()).unwrap();
    assert_eq!(workbook.to_bytes().unwrap(), original);
}

#[test]
fn resolves_workbook_and_sheet_paths_from_relationships() {
    let original = fixture_at(
        WORKSHEET,
        "custom/books/report.xml",
        "../../sheets/data.xml",
        "sheets/data.xml",
    );
    let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
    assert_eq!(workbook.sheets().len(), 1);
    assert_eq!(workbook.sheets()[0].name(), "Data & Notes");
    assert_eq!(workbook.sheets()[0].path(), "sheets/data.xml");
    workbook.set_cell("Data & Notes", "C1", 42).unwrap();
    let updated = workbook.to_bytes().unwrap();
    assert_ne!(
        payload(&updated, "sheets/data.xml"),
        payload(&original, "sheets/data.xml")
    );
    assert_eq!(
        payload(&updated, "custom/books/report.xml"),
        payload(&original, "custom/books/report.xml")
    );
}

#[test]
fn writes_text_numbers_booleans_and_blanks_without_shared_string_mutation() {
    let original = fixture(WORKSHEET);
    let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
    workbook
        .set_cell("Data & Notes", "A1", "  <&\"'>\n雪  ")
        .unwrap();
    workbook.set_cell("Data & Notes", "C1", -12.5).unwrap();
    workbook.set_cell("Data & Notes", "A2", false).unwrap();
    workbook
        .set_cell("Data & Notes", "C2", CellValue::Blank)
        .unwrap();
    let updated = workbook.to_bytes().unwrap();
    let xml = worksheet_xml(&updated);
    assert!(xml.contains("xml:space=\"preserve\""), "{xml}");
    assert!(xml.contains("&lt;&amp;"), "{xml}");
    assert!(xml.contains("雪"), "{xml}");
    assert!(xml.contains("<v>-12.5</v>"), "{xml}");
    assert!(xml.contains("<v>0</v>"), "{xml}");
    assert!(!xml.contains("<v>22</v>"), "{xml}");
    assert_eq!(
        payload(&updated, "xl/sharedStrings.xml"),
        payload(&original, "xl/sharedStrings.xml")
    );
    let mut reader = quick_xml::Reader::from_str(&xml);
    loop {
        if reader.read_event().unwrap() == quick_xml::events::Event::Eof {
            break;
        }
    }
}

#[test]
fn literal_ooxml_escape_sequences_and_carriage_returns_keep_their_text_meaning() {
    let mut workbook = Workbook::from_bytes(fixture(WORKSHEET)).unwrap();
    workbook
        .set_cell(
            "Data & Notes",
            "A1",
            "literal _x000A_ _Xabcd_ _x005F_\r\nend",
        )
        .unwrap();
    let xml = worksheet_xml(&workbook.to_bytes().unwrap());
    assert!(xml.contains("_x005F_x000A_"), "{xml}");
    assert!(xml.contains("_x005F_Xabcd_"), "{xml}");
    assert!(xml.contains("_x005F_x005F_"), "{xml}");
    assert!(xml.contains("&#13;\nend"), "{xml}");
}

#[test]
fn edits_prefixed_transitional_and_strict_worksheets_without_changing_namespaces() {
    for namespace in [MAIN_NS, "http://purl.oclc.org/ooxml/spreadsheetml/main"] {
        let xml = format!(
            r#"<s:worksheet xmlns:s="{namespace}" xmlns:x="urn:foreign"><s:dimension ref="A1:B1"/><s:sheetData><s:row r="1"><s:c r="A1" s="3"><s:v>7</s:v><x:keep exact="yes"/></s:c><s:c r="B1"><s:v>8</s:v></s:c></s:row></s:sheetData><x:feature>untouched</x:feature></s:worksheet>"#
        );
        let original = fixture(&xml);
        let original = if namespace != MAIN_NS {
            let entries: Vec<(String, Vec<u8>)> = archive_entries(&original)
                .into_iter()
                .map(|entry| {
                    let bytes = payload(&original, &entry.name);
                    let bytes = if entry.name.ends_with(".xml") || entry.name.ends_with(".rels") {
                        String::from_utf8(bytes).unwrap()
                            .replace(MAIN_NS, namespace)
                            .replace("http://schemas.openxmlformats.org/officeDocument/2006/relationships", "http://purl.oclc.org/ooxml/officeDocument/relationships")
                            .into_bytes()
                    } else { bytes };
                    (entry.name, bytes)
                }).collect();
            let borrowed: Vec<(&str, &[u8])> = entries
                .iter()
                .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
                .collect();
            archive(&borrowed)
        } else {
            original
        };
        let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
        workbook
            .set_cell("Data & Notes", "A1", "new value")
            .unwrap();
        workbook.set_cell("Data & Notes", "C3", true).unwrap();
        let updated = workbook.to_bytes().unwrap();
        let xml = worksheet_xml(&updated);
        assert!(xml.contains(&format!("xmlns:s=\"{namespace}\"")), "{xml}");
        assert!(xml.contains("<s:is><s:t"), "{xml}");
        assert!(xml.contains("<s:c r=\"B1\"><s:v>8</s:v></s:c>"), "{xml}");
        assert!(xml.contains("<x:keep exact=\"yes\"/>"), "{xml}");
        assert!(xml.contains("<x:feature>untouched</x:feature>"), "{xml}");
        assert_untouched_parts(&original, &updated);
        assert!(Workbook::from_bytes(updated).is_ok());
    }
}

#[test]
fn inserts_rows_and_cells_in_order_and_expands_dimension() {
    let mut workbook = Workbook::from_bytes(fixture(WORKSHEET)).unwrap();
    workbook.set_cell("Data & Notes", "D5", 5).unwrap();
    workbook.set_cell("Data & Notes", "B2", true).unwrap();
    workbook.set_cell("Data & Notes", "B4", "middle").unwrap();
    workbook.set_cell("Data & Notes", "A5", "first").unwrap();
    let xml = worksheet_xml(&workbook.to_bytes().unwrap());
    assert!(xml.contains("ref=\"A1:D5\""), "{xml}");
    assert!(xml.find("r=\"A2\"").unwrap() < xml.find("r=\"B2\"").unwrap());
    assert!(xml.find("r=\"B2\"").unwrap() < xml.find("r=\"C2\"").unwrap());
    assert!(xml.find("r=\"2\"").unwrap() < xml.find("r=\"4\"").unwrap());
    assert!(xml.find("r=\"4\"").unwrap() < xml.find("r=\"5\"").unwrap());
    assert!(xml.find("r=\"A5\"").unwrap() < xml.find("r=\"D5\"").unwrap());
}

#[test]
fn formula_cells_and_shared_or_array_formula_ranges_are_rejected() {
    let xml = format!(
        r#"<worksheet xmlns="{MAIN_NS}"><dimension ref="A1:D3"/><sheetData><row r="1"><c r="A1"><f t="shared" si="0" ref="A1:A3">SUM(D1:D3)</f><v>6</v></c><c r="B1"><f t="array" ref="B1:C2">TRANSPOSE(D1:D2)</f><v>1</v></c><c r="D1"><f>1+2</f><v>3</v></c></row><row r="2"><c r="A2"><f t="shared" si="0"/><v>6</v></c><c r="C2"><v>2</v></c></row></sheetData></worksheet>"#
    );
    let original = fixture(&xml);
    let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
    for cell in ["A1", "A2", "A3", "B1", "C2", "D1"] {
        assert!(
            workbook.set_cell("Data & Notes", cell, 9).is_err(),
            "accepted formula edit at {cell}"
        );
    }
    assert_eq!(workbook.to_bytes().unwrap(), original);
}

#[test]
fn invalid_values_and_addresses_fail_without_modifying_the_archive() {
    let original = fixture(WORKSHEET);
    let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
    for cell in ["A0", "XFE1", "A1048577", "", "1A", "A1:B2", "$A$1"] {
        assert!(
            workbook.set_cell("Data & Notes", cell, 1).is_err(),
            "accepted {cell}"
        );
    }
    assert!(workbook.set_cell("missing sheet", "A1", 1).is_err());
    for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(workbook.set_cell("Data & Notes", "A1", number).is_err());
    }
    assert!(
        workbook
            .set_cell("Data & Notes", "A1", "invalid\0text")
            .is_err()
    );
    assert_eq!(workbook.to_bytes().unwrap(), original);
}

#[test]
fn rejects_non_ooxml_input() {
    assert!(Workbook::from_bytes(b"Not an Excel package".to_vec()).is_err());
    assert!(Workbook::from_bytes(archive(&[("unrelated.txt", b"hello")])).is_err());
}

#[test]
fn signed_packages_can_be_read_but_cannot_be_edited() {
    let original = fixture(WORKSHEET);
    let mut entries: Vec<(String, Vec<u8>)> = archive_entries(&original)
        .into_iter()
        .map(|entry| {
            let bytes = payload(&original, &entry.name);
            (entry.name, bytes)
        })
        .collect();
    entries.push((
        "_xmlsignatures/sig1.xml".into(),
        b"<Signature xmlns='http://www.w3.org/2000/09/xmldsig#'/>".to_vec(),
    ));
    let borrowed: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    let signed = archive(&borrowed);
    let mut workbook = Workbook::from_bytes(signed.clone()).unwrap();
    assert!(workbook.set_cell("Data & Notes", "A1", 3).is_err());
    assert_eq!(workbook.to_bytes().unwrap(), signed);
}

#[test]
fn signature_relationships_protect_unconventional_signature_paths() {
    let original = fixture(WORKSHEET);
    let mut entries: Vec<(String, Vec<u8>)> = archive_entries(&original)
        .into_iter()
        .map(|entry| {
            let mut bytes = payload(&original, &entry.name);
            if entry.name == "_rels/.rels" {
                let xml = String::from_utf8(bytes).unwrap().replace(
                    "</Relationships>",
                    "<Relationship Id=\"signature\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin\" Target=\"vendor/origin.sigs\"/></Relationships>",
                );
                bytes = xml.into_bytes();
            }
            (entry.name, bytes)
        })
        .collect();
    entries.push(("vendor/origin.sigs".into(), Vec::new()));
    let borrowed: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    let signed = archive(&borrowed);
    let mut workbook = Workbook::from_bytes(signed.clone()).unwrap();
    assert!(workbook.set_cell("Data & Notes", "A1", 3).is_err());
    assert_eq!(workbook.to_bytes().unwrap(), signed);
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sheetpatch-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn cli_lists_sheets_and_can_atomically_replace_the_input() {
    let directory = TempDir::new();
    let input = directory.0.join("input.xlsm");
    fs::write(&input, fixture(WORKSHEET)).unwrap();
    let listed = Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
        .arg("list")
        .arg(&input)
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert_eq!(String::from_utf8(listed.stdout).unwrap(), "Data & Notes\n");
    let changed = Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
        .arg("set")
        .arg(&input)
        .arg(&input)
        .args(["Data & Notes", "A1", "text", "CLI value"])
        .output()
        .unwrap();
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let bytes = fs::read(&input).unwrap();
    assert!(worksheet_xml(&bytes).contains("CLI value"));
    assert!(Workbook::open(&input).is_ok());
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn failed_cli_edits_leave_existing_output_unchanged() {
    let directory = TempDir::new();
    let input = directory.0.join("input.xlsx");
    let output = directory.0.join("output.xlsx");
    fs::write(&input, fixture(WORKSHEET)).unwrap();
    fs::write(&output, b"existing output").unwrap();
    let changed = Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
        .arg("set")
        .arg(&input)
        .arg(&output)
        .args(["Data & Notes", "A0", "number", "3"])
        .output()
        .unwrap();
    assert!(!changed.status.success());
    assert_eq!(fs::read(&output).unwrap(), b"existing output");
    let invalid_bool = Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
        .arg("set")
        .arg(&input)
        .arg(&output)
        .args(["Data & Notes", "A1", "bool", "maybe"])
        .output()
        .unwrap();
    assert_eq!(invalid_bool.status.code(), Some(2));
    assert_eq!(fs::read(&output).unwrap(), b"existing output");
}

#[test]
fn repeated_saves_reopen_cleanly_and_keep_untouched_package_parts() {
    let directory = TempDir::new();
    let input = directory.0.join("repeated.xlsm");
    let original = fixture(WORKSHEET);
    fs::write(&input, &original).unwrap();
    for (cell, value) in [("A1", "first"), ("C1", "second"), ("A1", "third")] {
        let mut workbook = Workbook::open(&input).unwrap();
        workbook.set_cell("Data & Notes", cell, value).unwrap();
        workbook.save(&input).unwrap();
        let updated = fs::read(&input).unwrap();
        assert_untouched_parts(&original, &updated);
        let reopened = Workbook::from_bytes(updated.clone()).unwrap();
        assert_eq!(reopened.to_bytes().unwrap(), updated);
    }
    assert!(worksheet_xml(&fs::read(&input).unwrap()).contains("third"));
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn failed_atomic_rename_keeps_existing_destination_and_removes_temporary_files() {
    let directory = TempDir::new();
    let destination = directory.0.join("output.xlsx");
    fs::create_dir(&destination).unwrap();
    let sentinel = destination.join("existing.txt");
    fs::write(&sentinel, b"existing destination content").unwrap();
    let mut workbook = Workbook::from_bytes(fixture(WORKSHEET)).unwrap();
    workbook
        .set_cell("Data & Notes", "A1", "new value")
        .unwrap();
    assert!(workbook.save(&destination).is_err());
    assert_eq!(
        fs::read(&sentinel).unwrap(),
        b"existing destination content"
    );
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

fn replace_parts(original: &[u8], replacements: &[(&str, &[u8])]) -> Vec<u8> {
    let mut owned: Vec<(String, Vec<u8>)> = archive_entries(original)
        .into_iter()
        .map(|entry| {
            let bytes = replacements
                .iter()
                .find(|(name, _)| *name == entry.name)
                .map_or_else(
                    || payload(original, &entry.name),
                    |(_, bytes)| bytes.to_vec(),
                );
            (entry.name, bytes)
        })
        .collect();
    for &(name, bytes) in replacements {
        if !owned.iter().any(|(existing, _)| existing == name) {
            owned.push((name.to_owned(), bytes.to_vec()));
        }
    }
    archive(
        &owned
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
            .collect::<Vec<_>>(),
    )
}

#[test]
fn batch_edits_are_ordered_transactional_and_preserve_opaque_parts() {
    let original = fixture(WORKSHEET);
    let mut workbook = Workbook::from_bytes(original.clone()).unwrap();
    workbook
        .set_cells(
            "Data & Notes",
            [
                ("E3", CellValue::from("third row")),
                ("B2", CellValue::Number(12.5)),
                ("A1", CellValue::from("first update")),
                ("D3", CellValue::Bool(false)),
                ("D1", CellValue::Error("#N/A".into())),
                ("A1", CellValue::from("last wins")),
            ],
        )
        .unwrap();
    assert!(workbook.has_changes());
    let contents = workbook
        .get_cells("Data & Notes", ["E3", "A1", "B2", "D3", "D1", "Z99", "A1"])
        .unwrap();
    assert_eq!(
        contents.iter().map(|cell| &cell.value).collect::<Vec<_>>(),
        vec![
            &CellValue::from("third row"),
            &CellValue::from("last wins"),
            &CellValue::Number(12.5),
            &CellValue::Bool(false),
            &CellValue::Error("#N/A".into()),
            &CellValue::Blank,
            &CellValue::from("last wins"),
        ]
    );
    assert_eq!(contents[1].style_index, Some(3));
    let output = workbook.to_bytes().unwrap();
    assert_untouched_parts(&original, &output);
    assert!(
        worksheet_xml(&output).contains(
            "<extLst><ext uri=\"urn:vendor\"><x:cellFeature flag=\"true\"/></ext></extLst>"
        )
    );
    let before = workbook.to_bytes().unwrap();
    assert!(
        workbook
            .set_cells("Data & Notes", [("A1", 3), ("A0", 4)])
            .is_err()
    );
    assert_eq!(workbook.to_bytes().unwrap(), before);
    workbook.reset_changes();
    assert!(!workbook.has_changes());
    assert_eq!(workbook.to_bytes().unwrap(), original);
}

#[test]
fn multi_sheet_transaction_rolls_back_every_sheet_on_failure() {
    let original = fixture(WORKSHEET);
    let workbook_xml = String::from_utf8(payload(&original, "xl/workbook.xml"))
        .unwrap()
        .replace(
            "</sheets>",
            "<sheet name=\"Other\" sheetId=\"2\" r:id=\"rId2\"/></sheets>",
        );
    let rels = String::from_utf8(payload(&original, "xl/_rels/workbook.xml.rels")).unwrap().replace("</Relationships>", "<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/other.xml\"/></Relationships>");
    let other = format!(
        "<worksheet xmlns=\"{MAIN_NS}\"><sheetData><row r=\"1\"><c r=\"A1\"><f>1+1</f><v>2</v></c></row></sheetData></worksheet>"
    );
    let bytes = replace_parts(
        &original,
        &[
            ("xl/workbook.xml", workbook_xml.as_bytes()),
            ("xl/_rels/workbook.xml.rels", rels.as_bytes()),
            ("xl/worksheets/other.xml", other.as_bytes()),
        ],
    );
    let mut book = Workbook::from_bytes(bytes).unwrap();
    book.set_cell("Data & Notes", "A2", false).unwrap();
    let before = book.to_bytes().unwrap();
    let edits = vec![
        CellEdit::new("Data & Notes", "A1", "pending").unwrap(),
        CellEdit::new("Other", "A1", 8).unwrap(),
    ];
    assert!(book.apply_edits(edits).is_err());
    assert_eq!(book.to_bytes().unwrap(), before);
    book.apply_edits([
        CellEdit::new("Data & Notes", "A1", "committed").unwrap(),
        CellEdit::at("Other", CellRef::new(2, 2).unwrap(), 9).unwrap(),
    ])
    .unwrap();
    assert_eq!(
        book.get_cell("Data & Notes", "A1").unwrap().value,
        CellValue::from("committed")
    );
    assert_eq!(
        book.get_cell("Other", "B2").unwrap().value,
        CellValue::Number(9.0)
    );
    let formula = book.get_cell("Other", "A1").unwrap();
    assert_eq!(formula.formula.as_deref(), Some("1+1"));
    assert_eq!(formula.value, CellValue::Number(2.0));
}

#[test]
fn inspect_shared_strings_styles_and_pending_values_without_changes() {
    let bytes = fixture(WORKSHEET);
    let mut book = Workbook::from_bytes(bytes.clone()).unwrap();
    let cell = book.get_cell("Data & Notes", "A1").unwrap();
    assert_eq!(cell.value, CellValue::from("Old text"));
    assert_eq!(cell.style_index, Some(3));
    assert_eq!(
        book.get_cell("Data & Notes", "B1").unwrap().value,
        CellValue::from("Unchanged & literal")
    );
    assert!(!book.has_changes());
    assert_eq!(book.to_bytes().unwrap(), bytes);
    book.set_cell("Data & Notes", "A1", "new").unwrap();
    assert_eq!(
        book.get_cell("Data & Notes", "A1").unwrap().value,
        CellValue::from("new")
    );
    book.reset_changes();
    assert_eq!(
        book.get_cell("Data & Notes", "A1").unwrap().value,
        CellValue::from("Old text")
    );
    let broken = replace_parts(&bytes, &[("xl/sharedStrings.xml", b"<malformed>")]);
    let book = Workbook::from_bytes(broken).unwrap();
    assert_eq!(
        book.get_cell("Data & Notes", "C1").unwrap().value,
        CellValue::Number(99.0)
    );
    assert!(book.get_cell("Data & Notes", "A1").is_err());
}

#[test]
fn streaming_output_and_compaction_preserve_active_payloads() {
    let original = fixture(WORKSHEET);
    let mut current = original.clone();
    for index in 0..4 {
        let mut book = Workbook::from_bytes(current).unwrap();
        book.set_cell("Data & Notes", "A1", format!("iteration {index}"))
            .unwrap();
        let mut streamed = Vec::new();
        book.write_to(&mut streamed).unwrap();
        assert_eq!(streamed, book.to_bytes().unwrap());
        current = streamed;
    }
    let book = Workbook::from_bytes(current.clone()).unwrap();
    let compact = book.to_bytes_compact().unwrap();
    assert!(compact.len() < current.len());
    let reopened = Workbook::from_bytes(compact.clone()).unwrap();
    assert_eq!(
        reopened.get_cell("Data & Notes", "A1").unwrap().value,
        CellValue::from("iteration 3")
    );
    for entry in archive_entries(&original) {
        if entry.name != "xl/worksheets/sheet1.xml" {
            assert_eq!(
                payload(&compact, &entry.name),
                payload(&original, &entry.name)
            );
            assert_eq!(
                compressed_payload(&compact, &entry.name),
                compressed_payload(&original, &entry.name)
            );
        }
    }
    assert_eq!(archive_comment(&original), archive_comment(&compact));
    assert_eq!(reopened.to_bytes_compact().unwrap(), compact);
}

#[test]
fn cli_patch_get_and_compact_support_atomic_batch_workflows() {
    let directory = TempDir::new();
    let input = directory.0.join("input.xlsm");
    let output = directory.0.join("output.xlsm");
    let patch = directory.0.join("patch.tsv");
    fs::write(&input, fixture(WORKSHEET)).unwrap();
    fs::write(&patch, "Data & Notes\tA1\ttext\tfirst\nData & Notes\tD3\tnumber\t12.5\nData & Notes\tE3\terror\t#N/A\n").unwrap();
    let binary = env!("CARGO_BIN_EXE_sheetpatch");
    let patched = Command::new(binary)
        .arg("patch")
        .arg(&input)
        .arg(&output)
        .arg(&patch)
        .output()
        .unwrap();
    assert!(
        patched.status.success(),
        "{}",
        String::from_utf8_lossy(&patched.stderr)
    );
    let inspected = Command::new(binary)
        .arg("get")
        .arg(&output)
        .args(["Data & Notes", "D3"])
        .output()
        .unwrap();
    assert!(inspected.status.success());
    assert_eq!(inspected.stdout, b"12.5\n");
    let before = fs::read(&output).unwrap();
    fs::write(
        &patch,
        "Data & Notes\tA1\ttext\tshould not save\nData & Notes\tD3\tnumber\tnot a number\n",
    )
    .unwrap();
    let rejected = Command::new(binary)
        .arg("patch")
        .arg(&output)
        .arg(&output)
        .arg(&patch)
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(2));
    assert_eq!(fs::read(&output).unwrap(), before);
    let compacted = Command::new(binary)
        .arg("compact")
        .arg(&output)
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        compacted.status.success(),
        "{}",
        String::from_utf8_lossy(&compacted.stderr)
    );
    assert!(fs::read(&output).unwrap().len() < before.len());
    assert_eq!(
        Workbook::open(&output)
            .unwrap()
            .get_cell("Data & Notes", "A1")
            .unwrap()
            .value,
        CellValue::from("first")
    );
}
