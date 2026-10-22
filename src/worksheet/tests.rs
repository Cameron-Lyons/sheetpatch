use super::parse::{self, STRICT, TRANSITIONAL, XmlDocument};
use super::prepared_worksheet::{self, PreparedWorksheet};
use super::read::{self, ArenaPeaks, ReadError, SharedStrings};
use super::validation::{self, RangeKind};
use super::{date, patch, protection};
use crate::{CellContent, CellRange, CellRef, CellValue, Error, Result};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::iter;

fn patch_cells<'xml, 'strings>(
    xml: &'xml [u8],
    cells: &BTreeMap<CellRef, CellValue>,
    shared_strings: impl FnMut() -> Option<&'strings SharedStrings>,
    date1904: bool,
) -> Result<Cow<'xml, [u8]>> {
    PreparedWorksheet::parse(xml)?.patch_cells(cells, shared_strings, date1904)
}

fn patch_cell(xml: &[u8], cell: CellRef, value: &CellValue) -> Result<Vec<u8>> {
    patch_cells(
        xml,
        &BTreeMap::from([(cell, value.clone())]),
        || None,
        /* date1904: */ false,
    )
    .map(Cow::into_owned)
}

fn read_cell(
    xml: &[u8],
    cell: CellRef,
    shared_strings: Option<&SharedStrings>,
) -> Result<CellContent> {
    prepared_worksheet::read_cells(
        xml,
        &[cell],
        || Ok(shared_strings),
        /* date1904: */ false,
    )
    .map(|mut cells| cells.remove(0))
    .map_err(|error| match error {
        ReadError::Worksheet(error)
        | ReadError::SharedStrings(error)
        | ReadError::UndecodableString { error, .. } => error,
    })
}

fn sheet(body: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><worksheet xmlns="{TRANSITIONAL}" xmlns:x="urn:custom">{body}</worksheet>"#
    )
}
fn edit(xml: &str, cell: &str, value: CellValue) -> Result<String> {
    Ok(String::from_utf8(patch_cell(xml.as_bytes(), cell.parse().unwrap(), &value)?).unwrap())
}

#[test]
fn retains_raw_markup_and_unknown_cell_content() {
    let input = sheet(
        r#"<dimension ref='A1:C5'/><sheetData><row r='2' spans='1:3'><c r='B2' s='7' t = 'n' x:flag='yes'><x:v>untouched</x:v><!--keep--><v>12</v><extLst><x:future/></extLst></c></row></sheetData><x:opaque arbitrary='true'/>"#,
    );
    let expected = input.replace("t = 'n'", "t = 'inlineStr'").replace(
        "<v>12</v>",
        "<is><t xml:space=\"preserve\"> &amp;&lt;&gt; </t></is>",
    );
    assert_eq!(
        edit(&input, "B2", CellValue::Text(" &<> ".into())).unwrap(),
        expected
    );
}

#[test]
fn inserts_rows_and_cells_in_order_and_extends_hints() {
    let input = sheet(
        r#"<dimension ref="B2:B2"/><sheetData><row r="2" spans="2:2"><c r="B2"><v>9</v></c></row><row r="4"/></sheetData>"#,
    );
    let changed = edit(&input, "A2", CellValue::Number(3.0)).unwrap();
    assert!(changed.contains(r#"<dimension ref="A2:B2"/>"#));
    assert!(changed.contains(r#"<row r="2" spans="1:2"><c r="A2"><v>3</v></c><c r="B2">"#));
    let changed = edit(&changed, "C3", CellValue::Bool(true)).unwrap();
    assert!(changed.contains(r#"</row><row r="3"><c r="C3" t="b"><v>1</v></c></row><row r="4"/>"#));
}

#[test]
fn expands_empty_sheet_row_and_cell() {
    let empty = sheet("<sheetData />");
    let changed = edit(&empty, "D9", CellValue::Number(1.5)).unwrap();
    assert!(
        changed.contains(r#"<sheetData ><row r="9"><c r="D9"><v>1.5</v></c></row></sheetData>"#)
    );
    let empty = sheet(r#"<sheetData><row r="1" spans="1:1" /></sheetData>"#);
    let changed = edit(&empty, "B1", CellValue::Number(2.0)).unwrap();
    assert!(changed.contains(r#"<row r="1" spans="1:2" ><c r="B1"><v>2</v></c></row>"#));
    let empty = sheet(r#"<sheetData><row r="1"><c r="A1" s="8" /></row></sheetData>"#);
    assert!(
        edit(&empty, "A1", CellValue::Bool(false))
            .unwrap()
            .contains(r#"<c r="A1" s="8"  t="b"><v>0</v></c>"#)
    );
}

#[test]
fn understands_prefixed_strict_namespace_and_ignores_foreign_tags() {
    let input = format!(
        r#"<s:worksheet xmlns:s="{STRICT}" xmlns:x="urn:x"><x:sheetData/><s:sheetData><s:row r="1"><s:c r="A1" t="s"><x:v>keep</x:v><s:v>0</s:v></s:c></s:row></s:sheetData></s:worksheet>"#
    );
    let changed = edit(&input, "A1", CellValue::Text("a".into())).unwrap();
    assert!(changed.contains(r#"<x:v>keep</x:v><s:is><s:t xml:space="preserve">a</s:t></s:is>"#));
    assert!(
        edit(&changed, "B1", CellValue::Number(2.0))
            .unwrap()
            .contains(r#"<s:c r="B1"><s:v>2</s:v></s:c>"#)
    );
}

#[test]
fn indexes_direct_children_across_nested_extensions_and_empty_elements() {
    let foreign = "<x:extension><x:empty/><x:nested><x:leaf/></x:nested></x:extension>";
    let rich = "<is><r><rPr><b/></rPr><t>one</t></r><r><t>two</t></r></is>";
    let input = sheet(&format!(
        "{foreign}<sheetData>{foreign}<row r='1'>{foreign}<c r='A1' t='inlineStr'>{rich}{foreign}</c>{foreign}<c r='C1'/>{foreign}</row>{foreign}<row r='3'/>{foreign}</sheetData>{foreign}"
    ));
    let addresses: Vec<CellRef> = ["A1", "C1", "A2", "B3"]
        .iter()
        .map(|address| address.parse().unwrap())
        .collect();
    assert_eq!(
        prepared_worksheet::read_cells(
            input.as_bytes(),
            &addresses,
            || panic!("inline strings should not load shared strings"),
            /* date1904: */ false
        )
        .unwrap()
        .into_iter()
        .map(|cell| cell.value)
        .collect::<Vec<_>>(),
        vec![
            "onetwo".into(),
            CellValue::Blank,
            CellValue::Blank,
            CellValue::Blank
        ]
    );
    let changed = batch(
        &input,
        &[
            ("A1", "new".into()),
            ("B1", 2.0.into()),
            ("C1", 3.0.into()),
            ("A2", 4.0.into()),
            ("B3", 5.0.into()),
        ],
    )
    .unwrap();
    let expected = input
        .replace(rich, "<is><t xml:space=\"preserve\">new</t></is>")
        .replace(
            "<c r='C1'/>",
            "<c r=\"B1\"><v>2</v></c><c r='C1'><v>3</v></c>",
        )
        .replace(
            "<row r='3'/>",
            "<row r=\"2\"><c r=\"A2\"><v>4</v></c></row><row r='3'><c r=\"B3\"><v>5</v></c></row>",
        );
    assert_eq!(changed, expected);
    assert_eq!(
        prepared_worksheet::read_cells(
            changed.as_bytes(),
            &addresses,
            || panic!("inline strings should not load shared strings"),
            /* date1904: */ false
        )
        .unwrap()
        .into_iter()
        .map(|cell| cell.value)
        .collect::<Vec<_>>(),
        vec!["new".into(), 3.0.into(), 4.0.into(), 5.0.into()]
    );
}

#[test]
fn resolves_escaped_namespace_names_and_rejects_expanded_attribute_duplicates() {
    for namespace in [TRANSITIONAL, STRICT] {
        let encoded = namespace.replace('/', "&#47;");
        let input = format!(
            r#"<s:worksheet xmlns:s="{encoded}"><s:sheetData><s:row r="1"><s:c r="A1"><s:v>1</s:v></s:c></s:row></s:sheetData></s:worksheet>"#
        );
        let changed = edit(&input, "A1", 2.0.into()).unwrap();
        assert_eq!(changed, input.replace("<s:v>1</s:v>", "<s:v>2</s:v>"));
        assert_eq!(
            read_cell(changed.as_bytes(), "A1".parse().unwrap(), None)
                .unwrap()
                .value,
            CellValue::Number(2.0)
        );
    }
    let duplicate = sheet(
        r#"<sheetData><row r="1"><c r="A1" xmlns:a="urn:x" xmlns:b="urn&#58;x" a:flag="one" b:flag="two"/></row></sheetData>"#,
    );
    assert!(matches!(
        edit(&duplicate, "A1", 2.0.into()),
        Err(Error::Xml(_))
    ));
    for attributes in [
        "a:first='one' a:flag='two' b:flag='three'",
        "a:flag='one' a:other='two' b:flag='three'",
    ] {
        let duplicate = sheet(&format!(
            "<sheetData><row r='1'><c r='A1' xmlns:a='urn:x' xmlns:b='urn&#58;x' {attributes}/></row></sheetData>"
        ));
        assert!(matches!(
            edit(&duplicate, "A1", 2.0.into()),
            Err(Error::Xml(_))
        ));
    }
    let unbound = sheet("<sheetData><row r='1'><c r='A1' missing:flag='one'/></row></sheetData>");
    assert!(matches!(
        edit(&unbound, "A1", 2.0.into()),
        Err(Error::Xml(_))
    ));
}

#[test]
fn normalizes_literal_attribute_whitespace_before_character_references() {
    let input = sheet(
        "<sheetData><row r=\"1\"><c r=\"A1\" x:flag=\"a\r\nb\rc\nd\te&#13;f&#10;g&#9;h\"/></row></sheetData>",
    );
    let XmlDocument {
        source,
        elements,
        attributes,
        ..
    } = parse::parse(input.as_bytes()).unwrap();
    let cell = elements
        .iter()
        .find(|element| element.is(source, elements[0].namespace, "c"))
        .unwrap();
    assert_eq!(
        cell.attribute(&attributes, "x:flag").unwrap().value,
        "a b c d e\rf\ng\th"
    );
    let changed = edit(&input, "A1", 2.0.into()).unwrap();
    assert!(changed.contains("x:flag=\"a\r\nb\rc\nd\te&#13;f&#10;g&#9;h\""));
}

// A protected-cell refusal names its cell and reason; the workbook, which
// knows the worksheet's name, fills in the sheet.
fn protected_cell(result: Result<String>, address: &str, expected: &str) -> bool {
    matches!(
        result,
        Err(Error::ProtectedCell { sheet, cell, reason, .. })
            if sheet.is_empty() && cell.to_string() == address && reason == expected
    )
}

#[test]
fn rejects_formula_cells_and_formula_followers() {
    for kind in ["shared", "array", "dataTable"] {
        let si = if kind == "shared" { " si=\"0\"" } else { "" };
        let input = sheet(&format!(
            r#"<sheetData><row r="1"><c r="A1"><f t="{kind}"{si} ref="A1:C2">1</f><v>1</v></c></row></sheetData>"#
        ));
        assert!(protected_cell(
            edit(&input, "A1", CellValue::Blank),
            "A1",
            "contains a formula"
        ));
        // Even the cell's current value is refused.
        assert!(protected_cell(
            edit(&input, "A1", CellValue::Number(1.0)),
            "A1",
            "contains a formula"
        ));
        assert!(protected_cell(
            edit(&input, "B2", CellValue::Number(8.0)),
            "B2",
            "belongs to a formula range"
        ));
        assert!(edit(&input, "D2", CellValue::Number(8.0)).is_ok());
    }
}

#[test]
fn formula_cells_are_those_with_a_main_namespace_formula_child() {
    let input = sheet(&format!(
        "<sheetData><row r='1'><c r='A1'><v>1</v></c><c r='B1'><x:f>foreign</x:f><v>2</v></c><c r='C1'><v>3</v><f>C2</f></c></row><row r='3'><c r='A3' xmlns:s='{TRANSITIONAL}'><s:f>1</s:f></c><c r='B3'/></row></sheetData>"
    ));
    let edits = |addresses: &[&str]| {
        let cells: BTreeMap<CellRef, CellValue> = addresses
            .iter()
            .map(|address| (address.parse().unwrap(), CellValue::Number(9.0)))
            .collect();
        patch_cells(
            input.as_bytes(),
            &cells,
            || None,
            /* date1904: */ false,
        )
        .map(Cow::into_owned)
    };
    for address in ["A1", "B1", "B3", "A2", "D1", "C3"] {
        assert!(edits(&[address]).is_ok(), "{address}");
    }
    // Edits are checked in row-major order, so the first formula cell is named.
    for (addresses, formula) in [
        (&["A1", "C1", "A3"][..], "C1"),
        (&["B3", "A3", "B1"], "A3"),
        (&["D9", "A3", "A2", "C1"], "C1"),
    ] {
        let result = edits(addresses).map(|xml| String::from_utf8(xml).unwrap());
        assert!(
            protected_cell(result, formula, "contains a formula"),
            "{addresses:?}"
        );
    }
}

#[test]
fn shared_formula_indices_use_their_numeric_identity() {
    let input = sheet(
        "<sheetData><row r='1'><c r='A1'><f t='shared' si='1' ref='A1:B1'>1</f><v>1</v></c><c r='B1'><f t='shared' si='01'/><v>1</v></c><c r='C1'><v>1</v></c></row></sheetData>",
    );
    assert_eq!(
        edit(&input, "C1", 2.0.into()).unwrap(),
        input.replacen("<c r='C1'><v>1</v>", "<c r='C1'><v>2</v>", 1)
    );
    assert!(edit(&input, "B1", 2.0.into()).is_err());
    for index in ["not-a-number", "-1", "4294967296"] {
        let invalid = input.replace("si='01'", &format!("si='{index}'"));
        assert!(matches!(
            edit(&invalid, "C1", 2.0.into()),
            Err(Error::InvalidWorkbook(_))
        ));
    }
}

#[test]
fn numeric_attributes_collapse_xml_whitespace_without_rewriting_it() {
    let input = sheet(
        "<sheetData><row r=' &#9;1&#13;&#10; '><c r='A1'><f t='shared' si=' 1 ' ref='A1:B1'>1</f><v>1</v></c><c r='B1'><f t='shared' si='&#9;01&#10;'/><v>1</v></c><c r='C1' s=' &#9;3&#13; '><v> 7 </v></c></row></sheetData>",
    );
    let content = read_cell(input.as_bytes(), "C1".parse().unwrap(), None).unwrap();
    assert_eq!(content.value, CellValue::Number(7.0));
    assert_eq!(content.style_index, Some(3));
    assert_eq!(
        edit(&input, "C1", 8.0.into()).unwrap(),
        input.replace("<v> 7 </v>", "<v>8</v>")
    );
    for address in ["A1", "B1"] {
        assert!(edit(&input, address, 2.0.into()).is_err());
    }
    for invalid in ["1 2", "\u{a0}1\u{a0}"] {
        let bad_row = input.replace(" &#9;1&#13;&#10; ", invalid);
        assert!(edit(&bad_row, "C1", 8.0.into()).is_err());
        let bad_index = input.replace("si=' 1 '", &format!("si='{invalid}'"));
        assert!(edit(&bad_index, "C1", 8.0.into()).is_err());
        let bad_style = input.replace("s=' &#9;3&#13; '", &format!("s='{invalid}'"));
        assert!(read_cell(bad_style.as_bytes(), "C1".parse().unwrap(), None).is_err());
    }
}

#[test]
fn metadata_for_an_existing_value_cannot_survive_a_value_change() {
    let input = sheet(
        r#"<sheetData><row r="1"><c r="A1" t="e" vm="1" cm="2" ph="1"><v>#VALUE!</v></c><c r="B1"><v>3</v></c></row></sheetData>"#,
    );
    for value in [
        CellValue::from("replacement"),
        CellValue::Number(5.0),
        CellValue::Bool(true),
        CellValue::Error("#N/A".into()),
        CellValue::Blank,
    ] {
        assert!(matches!(
            edit(&input, "A1", value),
            Err(Error::ProtectedCell { cell, reason, .. })
                if cell.to_string() == "A1"
                    && reason == "has value metadata (a linked or rich data type)"
        ));
    }
    assert_eq!(
        edit(&input, "A1", CellValue::Error("#VALUE!".into())).unwrap(),
        input
    );
    let output = edit(&input, "B1", CellValue::Number(4.0)).unwrap();
    assert_eq!(output, input.replace("<v>3</v>", "<v>4</v>"));
    let cell_metadata = input.replace(" vm=\"1\"", "");
    assert!(matches!(
        edit(&cell_metadata, "A1", CellValue::from("replacement")),
        Err(Error::ProtectedCell { cell, reason, .. })
            if cell.to_string() == "A1"
                && reason == "has cell metadata with unsupported edit semantics"
    ));
    assert_eq!(
        edit(&cell_metadata, "A1", CellValue::Error("#VALUE!".into())).unwrap(),
        cell_metadata
    );
}

#[test]
fn cell_metadata_guard_preserves_blank_no_ops_and_ignores_vendor_attributes() {
    for attribute in ["cm='0'", "cm='1'", "cm=' &#9;1&#13; '"] {
        let blank = sheet(&format!(
            "<sheetData><row r='1'><c r='A1' {attribute}/></row></sheetData>"
        ));
        assert_eq!(edit(&blank, "A1", CellValue::Blank).unwrap(), blank);
        assert!(matches!(
            edit(&blank, "A1", CellValue::from("new")),
            Err(Error::ProtectedCell { reason, .. }) if reason == protection::CELL_METADATA
        ));
        // Clearing an existing empty payload still changes its representation.
        let payload = blank.replace("/></row>", "><v/></c></row>");
        assert!(matches!(
            edit(&payload, "A1", CellValue::Blank),
            Err(Error::ProtectedCell { reason, .. }) if reason == protection::CELL_METADATA
        ));
    }
    let vendor = sheet(
        "<sheetData><row r='1'><c r='A1' xmlns:x='urn:custom' x:cm='1'><v>1</v></c></row></sheetData>",
    );
    assert_eq!(
        edit(&vendor, "A1", CellValue::Number(2.0)).unwrap(),
        vendor.replace("<v>1</v>", "<v>2</v>")
    );
}

#[test]
fn unsupported_containers_cannot_hide_formulas_or_merged_ranges() {
    let prefix = r#"<sheetData><row r="1"><c r="A1"><v>1</v>"#;
    for body in [
        format!("{prefix}<x:alternate><f>1+1</f></x:alternate></c></row></sheetData>"),
        format!(
            "{prefix}</c></row></sheetData><x:alternate><mergeCells><mergeCell ref=\"A1:C2\"/></mergeCells></x:alternate>"
        ),
        format!(
            "{prefix}</c></row></sheetData><mergeCells><x:alternate><mergeCell ref=\"A1:C2\"/></x:alternate></mergeCells>"
        ),
    ] {
        let input = sheet(&body);
        assert!(
            matches!(edit(&input, "B2", 8.0.into()), Err(Error::Unsupported(_))),
            "{input}"
        );
    }
    // A whole cell hidden the same way is refused, even as the edit's target.
    for body in [
        r#"<sheetData><row r="1"><x:alternate><c r="A1"><f>1+1</f><v>2</v></c></x:alternate></row></sheetData>"#,
        r#"<sheetData><c r="A1"><f>1+1</f><v>2</v></c></sheetData>"#,
        r#"<sheetData/><c r="A1"><f>1+1</f><v>2</v></c>"#,
    ] {
        let input = sheet(body);
        for address in ["A1", "B2"] {
            assert!(
                matches!(
                    edit(&input, address, 1.0.into()),
                    Err(Error::Unsupported(m)) if m == "cell outside a worksheet row"
                ),
                "{address}: {input}"
            );
        }
    }
    let input = sheet(&format!(
        "{prefix}</c></row></sheetData><x:mergeCells><x:mergeCell ref=\"A1:C2\"/></x:mergeCells>"
    ));
    assert!(edit(&input, "B2", 8.0.into()).is_ok());
}

#[test]
fn markup_compatibility_branches_cannot_bypass_value_or_range_guards() {
    let mc = "http://schemas.openxmlformats.org/markup-compatibility/2006";
    let alternative = |contents: &str| {
        format!(
            "<mc:AlternateContent xmlns:mc=\"{mc}\"><mc:Choice Requires=\"x\">{contents}</mc:Choice><mc:Fallback/></mc:AlternateContent>"
        )
    };
    for body in [
        format!(
            "<sheetData><row r=\"1\"><c r=\"A1\">{}</c></row></sheetData>",
            alternative("<v>7</v>")
        ),
        format!(
            "<sheetData><row r=\"1\"><c r=\"A1\">{}<v>7</v></c></row></sheetData>",
            alternative("<f>1+6</f>")
        ),
        format!(
            "<sheetData/>{}",
            alternative("<mergeCells><mergeCell ref=\"A1:C2\"/></mergeCells>")
        ),
        format!(
            "<sheetData><row r=\"1\">{}</row></sheetData>",
            alternative("<c r=\"A1\"><f>1+6</f><v>7</v></c>")
        ),
        // A worksheet's only sheetData in a branch hides every cell.
        alternative("<sheetData><row r=\"1\"><c r=\"A1\"><v>7</v></c></row></sheetData>"),
        format!(
            "<mc:AlternateContent xmlns:mc=\"{mc}\"><mc:Choice Requires=\"x\"><sheetData/></mc:Choice><mc:Fallback><sheetData><row r=\"1\"><c r=\"A1\"><v>7</v></c></row></sheetData></mc:Fallback></mc:AlternateContent>"
        ),
    ] {
        let input = sheet(&body);
        assert!(
            matches!(edit(&input, "B2", 8.0.into()), Err(Error::Unsupported(_))),
            "{input}"
        );
        assert!(
            matches!(
                read_cell(input.as_bytes(), "A1".parse().unwrap(), None),
                Err(Error::Unsupported(_))
            ),
            "{input}"
        );
    }
}

#[test]
fn generated_elements_use_the_context_prefix_after_default_namespace_rebinding() {
    let input = format!(
        r#"<s:worksheet xmlns:s="{TRANSITIONAL}" xmlns="urn:foreign"><s:sheetData><s:row r="1"><s:c r="A1"><s:v>1</s:v></s:c></s:row></s:sheetData></s:worksheet>"#
    );
    let output = batch(
        &input,
        &[
            ("A1", CellValue::from("new")),
            ("B1", CellValue::Number(2.0)),
            ("C2", CellValue::Bool(true)),
        ],
    )
    .unwrap();
    let contents = prepared_worksheet::read_cells(
        output.as_bytes(),
        &[
            "A1".parse().unwrap(),
            "B1".parse().unwrap(),
            "C2".parse().unwrap(),
        ],
        || Ok(None),
        /* date1904: */ false,
    )
    .unwrap();
    assert_eq!(
        contents
            .iter()
            .map(|content| &content.value)
            .collect::<Vec<_>>(),
        vec![
            &CellValue::from("new"),
            &CellValue::Number(2.0),
            &CellValue::Bool(true)
        ]
    );
    assert!(output.contains("<s:is><s:t xml:space=\"preserve\">new</s:t></s:is>"));
    assert!(output.contains("<s:row r=\"2\"><s:c r=\"C2\" t=\"b\"><s:v>1</s:v></s:c></s:row>"));
}

#[test]
fn clears_only_payload_and_type_and_missing_blank_is_noop() {
    let input = sheet(
        r#"<sheetData><row r="1"><c r="A1" t="inlineStr" s="4"><is><t>hello</t></is><x:future/></c></row></sheetData>"#,
    );
    assert_eq!(
        edit(&input, "A1", CellValue::Blank).unwrap(),
        input
            .replace(r#" t="inlineStr""#, "")
            .replace("<is><t>hello</t></is>", "")
    );
    assert_eq!(edit(&input, "XFD1048576", CellValue::Blank).unwrap(), input);
}

#[test]
fn clearing_empty_typed_cells_removes_type_and_retains_unknown_content() {
    for cell in [
        r#"<c r="A1" s="4" t = 'inlineStr' x:flag="keep" />"#,
        r#"<c r="A1" s="4" t = 's' x:flag="keep"><x:future/><!--keep--></c>"#,
        r#"<c r="A1" s="4" t = 'n' x:flag="keep"></c>"#,
    ] {
        let input = sheet(&format!("<sheetData><row r=\"1\">{cell}</row></sheetData>"));
        let typed = cell.find(" t = '").unwrap();
        let type_end = typed + cell[typed + 6..].find('\'').unwrap() + 7;
        let mut expected = cell.to_owned();
        expected.replace_range(typed..type_end, "");
        let output = edit(&input, "A1", CellValue::Blank).unwrap();
        assert_eq!(output, input.replace(cell, &expected));
        assert_eq!(edit(&output, "A1", CellValue::Blank).unwrap(), output);
        assert_eq!(
            read_cell(output.as_bytes(), "A1".parse().unwrap(), None)
                .unwrap()
                .value,
            CellValue::Blank
        );
    }
}

#[test]
fn escapes_excel_tokens_and_carriage_returns() {
    assert_eq!(
        patch::text_escaped("_x0041_\r&_Xabcd_"),
        "_x005F_x0041_&#13;&amp;_Xabcd_"
    );
    assert_eq!(
        patch::text_escaped("<\0\u{b}_x000B\u{b}\u{ffff}>"),
        "&lt;_x0000__x000B__x005F_x000B_x000B__xFFFF_&gt;"
    );
}

#[test]
fn xml_forbidden_text_round_trips_and_rewriting_it_is_a_no_op() {
    let text = "\0a\u{1}_x0001\u{1f}_x0041_\u{fffe}\r\n\u{ffff}";
    let changed = edit(&sheet("<sheetData/>"), "A1", text.into()).unwrap();
    assert!(changed.contains(
        "<t xml:space=\"preserve\">_x0000_a_x0001__x005F_x0001_x001F__x005F_x0041__xFFFE_&#13;\n_xFFFF_</t>"
    ));
    assert_eq!(
        read_cell(changed.as_bytes(), "A1".parse().unwrap(), None)
            .unwrap()
            .value,
        CellValue::from(text)
    );
    assert_eq!(edit(&changed, "A1", text.into()).unwrap(), changed);
    let typed = changed
        .replace(
            "<c r=\"A1\" t=\"inlineStr\"><is><t xml:space=\"preserve\">",
            "<c r=\"A1\" t=\"str\"><v>",
        )
        .replace("</t></is>", "</v>");
    assert_eq!(edit(&typed, "A1", text.into()).unwrap(), typed);
}

#[test]
fn leading_byte_order_marks_are_text_for_reads_and_unchanged_value_checks() {
    let input = sheet(
        "<sheetData><row r='1'><c r='A1' t='inlineStr'><is><t>\u{feff}Name</t></is></c><c r='B1'><v>\u{feff}5</v></c><c r='C1' t='str'><v>\u{feff}\u{feff}x</v></c></row></sheetData>",
    );
    assert_eq!(
        read_cell(input.as_bytes(), "C1".parse().unwrap(), None)
            .unwrap()
            .value,
        CellValue::from("\u{feff}\u{feff}x")
    );
    assert!(matches!(
        read_cell(input.as_bytes(), "B1".parse().unwrap(), None),
        Err(Error::InvalidWorkbook(message)) if message == "B1: invalid numeric cell value"
    ));
    // Values that differ only by a leading U+FEFF are real edits.
    for (address, value, old, new) in [
        (
            "A1",
            CellValue::from("Name"),
            "<is><t>\u{feff}Name</t></is>",
            "<is><t xml:space=\"preserve\">Name</t></is>",
        ),
        ("B1", CellValue::Number(5.0), "<v>\u{feff}5</v>", "<v>5</v>"),
        (
            "C1",
            CellValue::from("\u{feff}x"),
            "t='str'><v>\u{feff}\u{feff}x</v>",
            "t='inlineStr'><is><t xml:space=\"preserve\">\u{feff}x</t></is>",
        ),
    ] {
        let changed = edit(&input, address, value.clone()).unwrap();
        assert_eq!(changed, input.replace(old, new), "{address}");
        assert_eq!(edit(&changed, address, value).unwrap(), changed);
    }
    assert_eq!(edit(&input, "A1", "\u{feff}Name".into()).unwrap(), input);
}

#[test]
fn incomplete_formula_groups_block_every_edit_and_name_the_formula_cell() {
    for formula in [
        "<f t='shared' si='0'/>",
        "<f t='array'>1</f>",
        "<f t='dataTable' r1='A1'/>",
    ] {
        let input = sheet(&format!(
            "<sheetData><row r='1'><c r='A1'><v>1</v></c></row><row r='2'><c r='B2'>{formula}<v>1</v></c></row></sheetData>"
        ));
        for address in ["A1", "Z9", "B2"] {
            assert!(
                matches!(
                    edit(&input, address, 2.0.into()),
                    Err(Error::Unsupported(message))
                        if message == "B2 belongs to a formula group without a reference range"
                ),
                "{formula} {address}"
            );
        }
        // Positive control: the same group with its range protects only it.
        let complete = input
            .replace("/>", " ref='B2:C3'/>")
            .replace(">1</f>", " ref='B2:C3'>1</f>");
        assert!(edit(&complete, "A1", 2.0.into()).is_ok(), "{complete}");
        assert!(edit(&complete, "Z9", 2.0.into()).is_ok());
        assert!(matches!(
            edit(&complete, "C3", 2.0.into()),
            Err(Error::ProtectedCell { sheet, cell, reason, .. })
                if sheet.is_empty() && cell.to_string() == "C3" && reason == "belongs to a formula range"
        ));
    }
    // A shared group is complete when any member carries its range, and an
    // incomplete group is reported at its first cell.
    let followers = sheet(
        "<sheetData><row r='1'><c r='A1'><f t='shared' si='3'/><v>1</v></c><c r='B1'><f t='shared' si='3' ref='A1:B1'>1</f><v>1</v></c><c r='C1'><f t='shared' si='4'/><v>1</v></c><c r='D1'><f t='shared' si='4'/><v>1</v></c></row></sheetData>",
    );
    assert!(matches!(
        edit(&followers, "E5", 2.0.into()),
        Err(Error::Unsupported(message))
            if message == "C1 belongs to a formula group without a reference range"
    ));
    let complete = followers.replace(
        "<c r='C1'><f t='shared' si='4'/>",
        "<c r='C1'><f t='shared' si='4' ref='C1:D1'>1</f>",
    );
    assert!(edit(&complete, "E5", 2.0.into()).is_ok());
    for (cell, message) in [
        (
            "<c r='A1'><f t='shared'/></c>",
            "A1: shared formula without an index",
        ),
        (
            "<c r='A1'><f t='shared' si='x'/></c>",
            "A1: invalid shared formula index",
        ),
        (
            "<c r='A1'><f t='array' ref='B2:A1'/></c>",
            "A1: invalid cell range \"B2:A1\"",
        ),
    ] {
        let input = sheet(&format!("<sheetData><row r='1'>{cell}</row></sheetData>"));
        assert!(
            matches!(edit(&input, "C3", 2.0.into()), Err(Error::InvalidWorkbook(m)) if m == message),
            "{cell}"
        );
    }
}

#[test]
fn unsorted_or_duplicate_rows_and_cells_are_rejected_with_their_location() {
    // Duplicate, unsorted, or malformed coordinates leave a cell's position
    // ambiguous and make the workbook invalid. Absent coordinates are valid
    // but cannot be indexed.
    for (rows, expected) in [
        (
            "<row r='2'/><row r='1'/>",
            "invalid workbook: row 1: duplicate, unsorted, or invalid row numbers",
        ),
        (
            "<row r='1'/><row r='1'/>",
            "invalid workbook: row 1: duplicate, unsorted, or invalid row numbers",
        ),
        (
            "<row r='0'/>",
            "invalid workbook: row 0: duplicate, unsorted, or invalid row numbers",
        ),
        (
            "<row r='1048577'/>",
            "invalid workbook: row 1048577: duplicate, unsorted, or invalid row numbers",
        ),
        ("<row r='x'/>", "invalid workbook: invalid row number"),
        (
            "<row r='1'><c r='B1'/><c r='A1'/></row>",
            "invalid workbook: A1: duplicate, unsorted, or inconsistent cell references",
        ),
        (
            "<row r='1'><c r='A1'/><c r='A1'/></row>",
            "invalid workbook: A1: duplicate, unsorted, or inconsistent cell references",
        ),
        (
            "<row r='1'><c r='A2'/></row>",
            "invalid workbook: A2: duplicate, unsorted, or inconsistent cell references",
        ),
        (
            "<row r='1'><c r='A0'/></row>",
            "invalid workbook: row 1: invalid cell reference in worksheet",
        ),
        (
            "<row r='1'><c r='XFE1'/></row>",
            "invalid workbook: row 1: invalid cell reference in worksheet",
        ),
        (
            "<row r='3'><c/></row>",
            "unsupported: row 3: cells without explicit references",
        ),
        (
            "<row><c r='A1'/></row>",
            "unsupported: rows without explicit row numbers",
        ),
    ] {
        let input = sheet(&format!("<sheetData>{rows}</sheetData>"));
        for result in [
            edit(&input, "C5", 2.0.into()).map(drop),
            read_cell(input.as_bytes(), "C5".parse().unwrap(), None).map(drop),
        ] {
            assert_eq!(result.unwrap_err().to_string(), expected, "{rows}");
        }
    }
    // Positive control: sorted, distinct rows edit and read normally.
    let input = sheet("<sheetData><row r='1'><c r='A1'/><c r='B1'/></row><row r='2'/></sheetData>");
    let changed = edit(&input, "A2", 2.0.into()).unwrap();
    assert_eq!(
        read_cell(changed.as_bytes(), "A2".parse().unwrap(), None)
            .unwrap()
            .value,
        CellValue::Number(2.0)
    );
}

#[test]
fn malformed_worksheet_structure_makes_the_workbook_invalid() {
    for (input, message) in [
        (
            format!("<chartsheet xmlns='{TRANSITIONAL}'><sheetData/></chartsheet>"),
            "expected an OOXML worksheet",
        ),
        (
            "<worksheet><sheetData/></worksheet>".into(),
            "worksheet has no OOXML namespace",
        ),
        (
            sheet(""),
            "worksheet must contain exactly one sheetData element",
        ),
        (
            sheet("<sheetData/><sheetData/>"),
            "worksheet must contain exactly one sheetData element",
        ),
        (
            sheet("<sheetData/><mergeCells><mergeCell/></mergeCells>"),
            "merged cells without a reference",
        ),
    ] {
        for result in [
            edit(&input, "A1", 2.0.into()).map(drop),
            read_cell(input.as_bytes(), "A1".parse().unwrap(), None).map(drop),
        ] {
            assert!(
                matches!(&result, Err(Error::InvalidWorkbook(m)) if m == message),
                "{input}: {result:?}"
            );
        }
    }
    // A sheetData only inside another element is valid content this crate
    // does not index.
    let input = sheet("<x:extension xmlns:x='urn:x'><sheetData/></x:extension>");
    for result in [
        edit(&input, "A1", 2.0.into()).map(drop),
        read_cell(input.as_bytes(), "A1".parse().unwrap(), None).map(drop),
    ] {
        assert!(
            matches!(&result, Err(Error::Unsupported(m)) if m == "sheetData inside an unsupported XML container"),
            "{input}: {result:?}"
        );
    }
    // A repeated or malformed dimension only refuses the edits that may need
    // to widen it.
    for (dimension, message) in [
        (
            "<dimension ref='A1'/><dimension ref='A1'/>",
            "multiple worksheet dimensions",
        ),
        ("<dimension ref='nope'/>", "invalid cell range \"nope\""),
        ("<dimension/>", "dimension without a reference"),
    ] {
        let input = sheet(&format!("{dimension}<sheetData/>"));
        assert!(read_cell(input.as_bytes(), "A1".parse().unwrap(), None).is_ok());
        assert!(
            matches!(
                edit(&input, "B2", 2.0.into()),
                Err(Error::Unsupported(m)) if m == message
            ),
            "{dimension}"
        );
    }
}

#[test]
fn values_are_inserted_before_an_existing_cells_extension_list() {
    for (cell, value, expected) in [
        (
            "<c r=\"A1\" s=\"2\"></c>",
            CellValue::Number(5.0),
            "<c r=\"A1\" s=\"2\"><v>5</v></c>",
        ),
        (
            "<c r=\"A1\"><extLst><ext uri=\"u\"/></extLst></c>",
            CellValue::Number(5.0),
            "<c r=\"A1\"><v>5</v><extLst><ext uri=\"u\"/></extLst></c>",
        ),
        (
            "<c r=\"A1\" t=\"n\"><x:before/><extLst/><x:after/></c>",
            CellValue::from("text"),
            "<c r=\"A1\" t=\"inlineStr\"><x:before/><is><t xml:space=\"preserve\">text</t></is><extLst/><x:after/></c>",
        ),
    ] {
        let input = sheet(&format!("<sheetData><row r=\"1\">{cell}</row></sheetData>"));
        let changed = edit(&input, "A1", value.clone()).unwrap();
        assert_eq!(changed, input.replace(cell, expected));
        assert_eq!(
            read_cell(changed.as_bytes(), "A1".parse().unwrap(), None)
                .unwrap()
                .value,
            value
        );
    }
}

#[test]
fn only_xml_version_10_is_supported() {
    let body = "<sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData>";
    for version in ["1.1", "2.0"] {
        let input = sheet(body).replace("version=\"1.0\"", &format!("version=\"{version}\""));
        assert!(matches!(
            edit(&input, "A1", 2.0.into()),
            Err(Error::Unsupported(message)) if message == "only XML 1.0 is supported"
        ));
        assert!(matches!(
            read_cell(input.as_bytes(), "A1".parse().unwrap(), None),
            Err(Error::Unsupported(message)) if message == "only XML 1.0 is supported"
        ));
    }
    let input = sheet(body);
    assert_eq!(
        edit(&input, "A1", 2.0.into()).unwrap(),
        input.replace("<v>1</v>", "<v>2</v>")
    );
}

#[test]
fn shared_strings_are_loaded_only_for_text_over_shared_cells() {
    let input = sheet(
        "<sheetData><row r='1'><c r='A1' t='s'><v>0</v></c><c r='B1' t='s'><v>1</v></c><c r='C1'><v>3</v></c></row></sheetData>",
    );
    let cells = |values: &[(&str, CellValue)]| -> BTreeMap<CellRef, CellValue> {
        values
            .iter()
            .map(|(address, value)| (address.parse().unwrap(), value.clone()))
            .collect()
    };
    let strings = SharedStrings::from_iter(["first", "second"]);
    // The loader is asked for each comparison; the workbook keeps the table.
    let mut loads = 0;
    let unchanged = patch_cells(
        input.as_bytes(),
        &cells(&[
            ("A1", "first".into()),
            ("B1", "second".into()),
            ("C1", 3.0.into()),
        ]),
        || {
            loads += 1;
            Some(&strings)
        },
        /* date1904: */ false,
    )
    .unwrap();
    assert!(matches!(unchanged, Cow::Borrowed(_)));
    assert_eq!(loads, 2);
    // Numbers, blanks, and text over other cells never need the table.
    let changed = patch_cells(
        input.as_bytes(),
        &cells(&[
            ("A1", 1.0.into()),
            ("B1", CellValue::Blank),
            ("C1", "x".into()),
        ]),
        || panic!("shared strings are not needed"),
        /* date1904: */ false,
    )
    .unwrap();
    assert!(matches!(changed, Cow::Owned(_)));
    // A table that is absent or fails to load leaves each edit as a change.
    let changed = patch_cells(
        input.as_bytes(),
        &cells(&[("A1", "first".into()), ("B1", "second".into())]),
        || None,
        /* date1904: */ false,
    )
    .unwrap();
    assert_eq!(
        changed.as_ref(),
        edit(&input, "A1", "first".into())
            .and_then(|one| edit(&one, "B1", "second".into()))
            .unwrap()
            .as_bytes()
    );
}

#[test]
fn new_cells_use_row_or_column_default_styles_with_the_context_prefix() {
    let input = format!(
        r#"<s:worksheet xmlns:s="{STRICT}"><s:cols><s:col min="1" max="2" style="5"/></s:cols><s:sheetData><s:row r="1" s="6" customFormat="1"><s:c r="C1"><s:v>1</s:v></s:c></s:row><s:row r="2" s="6"/></s:sheetData></s:worksheet>"#
    );
    let changed = batch(
        &input,
        &[
            ("A1", 1.0.into()),
            ("A2", "two".into()),
            ("C2", 2.0.into()),
            ("B3", true.into()),
            ("C3", 3.0.into()),
        ],
    )
    .unwrap();
    assert!(changed.contains(r#"<s:c r="A1" s="6"><s:v>1</s:v></s:c><s:c r="C1">"#));
    assert!(changed.contains(r#"<s:row r="2" s="6"><s:c r="A2" s="5" t="inlineStr">"#));
    assert!(changed.contains(r#"<s:c r="C2"><s:v>2</s:v></s:c></s:row>"#));
    assert!(changed.contains(
        r#"<s:row r="3"><s:c r="B3" s="5" t="b"><s:v>1</s:v></s:c><s:c r="C3"><s:v>3</s:v></s:c></s:row>"#
    ));
    // Hidden column definitions make a new cell's style ambiguous.
    let mc = "http://schemas.openxmlformats.org/markup-compatibility/2006";
    let hidden = sheet(&format!(
        "<mc:AlternateContent xmlns:mc='{mc}'><mc:Choice Requires='x'><cols><col min='1' max='1' style='3'/></cols></mc:Choice></mc:AlternateContent><sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData>"
    ));
    assert!(matches!(
        edit(&hidden, "A2", 2.0.into()),
        Err(Error::Unsupported(message))
            if message == "A2: column definitions inside an unsupported XML container"
    ));
    assert!(edit(&hidden, "A1", 2.0.into()).is_ok());
}

#[test]
fn iso_dates_convert_in_both_date_systems() {
    use super::date::DateError::{BeforeEpoch, Invalid, TimeZoneOffset};
    for (text, serial_1900, serial_1904) in [
        ("1900-01-01", Ok(1.0), Err(BeforeEpoch)),
        ("1900-02-28", Ok(59.0), Err(BeforeEpoch)),
        ("1900-03-01", Ok(61.0), Err(BeforeEpoch)),
        ("1903-12-31", Ok(1_461.0), Err(BeforeEpoch)),
        ("1904-01-01", Ok(1_462.0), Ok(0.0)),
        ("1904-01-02T12:00", Ok(1_463.5), Ok(1.5)),
        ("2024-01-15T00:00:00Z", Ok(45_306.0), Ok(43_844.0)),
        ("2000-02-29", Ok(36_585.0), Ok(35_123.0)),
        ("18:00:00.000", Ok(0.75), Ok(0.75)),
        ("06:00Z", Ok(0.25), Ok(0.25)),
        // Excel's day zero holds times without a date.
        ("1899-12-31", Ok(0.0), Err(BeforeEpoch)),
        ("1899-12-31T12:00:00", Ok(0.5), Err(BeforeEpoch)),
        ("1899-12-30", Err(BeforeEpoch), Err(BeforeEpoch)),
        ("1899-12-30T23:59", Err(BeforeEpoch), Err(BeforeEpoch)),
        ("0000-01-01", Err(BeforeEpoch), Err(BeforeEpoch)),
        // Valid values with an offset other than Z would need a time zone.
        (
            "2024-01-15T00:00:00-00:00",
            Err(TimeZoneOffset),
            Err(TimeZoneOffset),
        ),
        (
            "2024-01-15T12:00:00.5+05:30",
            Err(TimeZoneOffset),
            Err(TimeZoneOffset),
        ),
        ("2024-01-15+01:00", Err(TimeZoneOffset), Err(TimeZoneOffset)),
        ("12:00-03:00", Err(TimeZoneOffset), Err(TimeZoneOffset)),
        (
            "1899-12-30T00:00+01:00",
            Err(TimeZoneOffset),
            Err(TimeZoneOffset),
        ),
        // Malformed values, with or without an offset.
        ("2024-02-30+01:00", Err(Invalid), Err(Invalid)),
        ("2024-01-15T12:00+5:00", Err(Invalid), Err(Invalid)),
        ("2024-01-15T12:00+15:00", Err(Invalid), Err(Invalid)),
        ("2024-01-15T12:00Z+01:00", Err(Invalid), Err(Invalid)),
        ("+01:00", Err(Invalid), Err(Invalid)),
        ("1900-02-29", Err(Invalid), Err(Invalid)),
        ("2024-01-15T00:00:00z", Err(Invalid), Err(Invalid)),
        ("2024-01-15 00:00:00", Err(Invalid), Err(Invalid)),
        ("2024-01-15T", Err(Invalid), Err(Invalid)),
        ("2024-01-15T24:00", Err(Invalid), Err(Invalid)),
        ("T00:00", Err(Invalid), Err(Invalid)),
        ("12:3\u{e9}", Err(Invalid), Err(Invalid)),
        ("2024-01-1\u{e9}", Err(Invalid), Err(Invalid)),
        ("12:00:5\u{e9}", Err(Invalid), Err(Invalid)),
        ("\u{e9}12:00:00", Err(Invalid), Err(Invalid)),
        ("+2024-01-15", Err(Invalid), Err(Invalid)),
        ("12:00:00.+1", Err(Invalid), Err(Invalid)),
        ("", Err(Invalid), Err(Invalid)),
    ] {
        assert_eq!(
            date::excel_serial(text, /* date1904: */ false),
            serial_1900,
            "{text}"
        );
        assert_eq!(
            date::excel_serial(text, /* date1904: */ true),
            serial_1904,
            "{text}"
        );
    }
    // A time before midnight never rounds into the next day, even where the
    // fraction of a second rounds up to 60 seconds or the day's serial leaves
    // too little precision for it.
    for (text, day) in [
        ("23:59:59.9999999999999999", 0.0),
        ("23:59:59.9999999999", 0.0),
        ("1899-12-31T23:59:59.9999999999999999", 0.0),
        ("2024-01-15T23:59:59.9999999999", 45_306.0),
        ("2024-01-15T23:59:59.9999999999999999Z", 45_306.0),
        ("9999-12-31T23:59:59.999999999", 2_958_465.0),
    ] {
        let serial = date::excel_serial(text, /* date1904: */ false).unwrap();
        assert!(serial >= day && serial < day + 1.0, "{text}: {serial}");
        assert!(serial > day + 0.9999, "{text}: {serial}");
    }
    let input = sheet(
        "<sheetData><row r='1'><c r='A1' t='d' s='3'><v>2024-01-15</v></c><c r='B1' t='d'><v>1899-12-31T12:00:00</v></c></row></sheetData>",
    );
    for (date1904, serial) in [(false, 45_306.0), (true, 43_844.0)] {
        let content = prepared_worksheet::read_cells(
            input.as_bytes(),
            &["A1".parse().unwrap()],
            || panic!("no strings"),
            date1904,
        )
        .unwrap()
        .remove(0);
        assert_eq!(content.value, CellValue::Number(serial));
        assert_eq!(content.style_index, Some(3));
    }
    assert_eq!(
        read_cell(input.as_bytes(), "B1".parse().unwrap(), None)
            .unwrap()
            .value,
        CellValue::Number(0.5)
    );
    let date_cell = |date1904: bool, value: f64| {
        patch_cells(
            input.as_bytes(),
            &BTreeMap::from([("A1".parse().unwrap(), value.into())]),
            || panic!("no strings"),
            date1904,
        )
        .unwrap()
    };
    // Rewriting a date cell's own serial, in the workbook's date system,
    // leaves its exact XML.
    assert!(matches!(
        date_cell(/* date1904: */ false, 45_306.0),
        Cow::Borrowed(_)
    ));
    assert!(matches!(
        date_cell(/* date1904: */ true, 43_844.0),
        Cow::Borrowed(_)
    ));
    // Any other number replaces the date with a plain number.
    assert_eq!(
        date_cell(/* date1904: */ true, 45_306.0).as_ref(),
        input
            .replace(" t='d' s='3'><v>2024-01-15", " s='3'><v>45306")
            .as_bytes()
    );
    assert_eq!(
        edit(&input, "A1", 45_307.0.into()).unwrap(),
        input.replace(" t='d' s='3'><v>2024-01-15", " s='3'><v>45307")
    );
}

#[test]
fn rejects_unknown_payload_markup_but_can_replace_known_rich_text() {
    let rich = sheet(
        r#"<sheetData><row r="1"><c r="A1" t="inlineStr"><is><r><rPr><b/><color rgb="FF000000"/></rPr><t xml:space="preserve">old</t></r></is></c></row></sheetData>"#,
    );
    assert!(edit(&rich, "A1", CellValue::Text("new".into())).is_ok());
    let foreign = rich.replace("<b/>", "<x:future/>");
    assert!(edit(&foreign, "A1", CellValue::Blank).is_err());
    let attribute = rich.replace("<is>", "<is x:flag='keep'>");
    assert!(edit(&attribute, "A1", CellValue::Text("new".into())).is_err());
}

#[test]
fn merged_range_anchor_can_change_but_followers_cannot() {
    let input = sheet(
        r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:C2"/></mergeCells>"#,
    );
    assert!(edit(&input, "A1", CellValue::Number(2.0)).is_ok());
    for (address, value) in [
        ("B2", CellValue::Number(2.0)),
        ("B2", CellValue::Blank),
        ("C1", CellValue::from("x")),
    ] {
        assert!(protected_cell(
            edit(&input, address, value),
            address,
            "is merged into another cell's range"
        ));
    }
    assert!(edit(&input, "D2", CellValue::Number(2.0)).is_ok());
}

#[test]
fn preserves_utf8_bom_and_rejects_non_utf8_encoding() {
    let input = format!(
        "\u{feff}{}",
        sheet(r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData>"#)
    );
    let changed = edit(&input, "A1", CellValue::Number(2.0)).unwrap();
    assert_eq!(changed, input.replace("<v>1</v>", "<v>2</v>"));
    // U+FEFF after the byte order mark is text before the root element.
    let doubled = format!("\u{feff}{input}");
    assert!(matches!(
        edit(&doubled, "A1", CellValue::Number(2.0)),
        Err(Error::Xml(message)) if message == "text outside the XML root"
    ));
    let input = input.replace("version=\"1.0\"", "version=\"1.0\" encoding=\"UTF-16\"");
    assert!(edit(&input, "A1", CellValue::Number(2.0)).is_err());
}

#[test]
fn rejects_ambiguous_or_invalid_xml() {
    for body in [
        r#"<sheetData><row><c r="A1"/></row></sheetData>"#,
        r#"<sheetData><row r="1"><c r="B1"/><c r="A1"/></row></sheetData>"#,
        r#"<sheetData><row r="1"><c r="A1"><v>1</v><is><t>x</t></is></c></row></sheetData>"#,
        r#"<sheetData><row r="1"><c r="A1" t="n" t="b"/></row></sheetData>"#,
        r#"<sheetData><row r="1"><c r="A1"t="n"/></row></sheetData>"#,
        r#"<sheetData><row r="1"><c r="A1"><v>&unknown;</v></c></row></sheetData>"#,
    ] {
        assert!(
            edit(&sheet(body), "A1", CellValue::Number(2.0)).is_err(),
            "{body}"
        );
    }
}

#[test]
fn rejects_invalid_xml_names_and_non_xml_whitespace_outside_root() {
    for input in [
        sheet(r#"<sheetData 0bad="one"/>"#),
        sheet("<sheetData><0bad/></sheetData>"),
        sheet("<sheetData><x:bad:name/></sheetData>"),
        sheet("<sheetData><?0bad data?></sheetData>"),
        sheet("<sheetData><?XML data?></sheetData>"),
        format!("\u{a0}{}", sheet("<sheetData/>")),
        format!("{}\u{a0}", sheet("<sheetData/>")),
        format!("{}<worksheet/>", sheet("<sheetData/>")),
        sheet("<sheetData/><x:a>]]></x:a>"),
        sheet("<sheetData/><x:a v='a<b'/>"),
        format!("{}&amp;", sheet("<sheetData/>")),
        format!("{}<![CDATA[x]]>", sheet("<sheetData/>")),
        sheet("<sheetData/><x:a v='&#1;'/>"),
        sheet("<sheetData/><x:a>\u{1}</x:a>"),
    ] {
        assert!(
            matches!(edit(&input, "A1", 2.0.into()), Err(Error::Xml(_))),
            "accepted malformed XML: {input}"
        );
        assert!(
            matches!(
                read_cell(input.as_bytes(), "A1".parse().unwrap(), None),
                Err(Error::Xml(_))
            ),
            "read malformed XML: {input}"
        );
    }
}

#[test]
fn rejects_illegal_namespace_declarations_even_when_unused_or_escaped() {
    for body in [
        "<sheetData xmlns:p='' />",
        "<sheetData xmlns:p='http://www.w3.org/XML/1998/namespac&#101;' />",
        "<sheetData xmlns:p='http://www.w3.org/2000/xmlns&#47;' />",
        "<sheetData/><x:foreign xmlns='http://www.w3.org/XML/1998/namespac&#101;'/>",
        "<sheetData/><x:foreign xmlns='http://www.w3.org/2000/xmlns&#47;'/>",
        "<sheetData/><xmlns:invalid/>",
    ] {
        let input = sheet(body);
        assert!(
            matches!(edit(&input, "A1", 2.0.into()), Err(Error::Xml(_))),
            "{input}"
        );
    }
}

// quick-xml's escape-html feature, which another dependency can enable,
// makes its default resolver accept HTML entities. XML 1.0 predefines five.
#[test]
fn only_xml_10_entities_are_resolved_whatever_quick_xml_features_are_enabled() {
    let a1: CellRef = "A1".parse().unwrap();
    let cell = |attributes: &str, text: &str| {
        sheet(&format!(
            "<sheetData><row r='1'><c r='A1' t='inlineStr'{attributes}><is><t>{text}</t></is></c></row></sheetData>"
        ))
    };
    let input = cell(" x:flag='&amp;&#160;'", "a&#160;b&amp;&quot;");
    assert_eq!(
        read_cell(input.as_bytes(), a1, None).unwrap().value,
        CellValue::Text("a\u{a0}b&\"".into())
    );
    for (input, entity) in [
        (cell("", "a&nbsp;b"), "nbsp"),
        (cell(" x:flag='&nbsp;'", "a"), "nbsp"),
        (sheet("<sheetData xmlns:p='urn:&nbsp;'/>"), "nbsp"),
        (
            sheet("<sheetData xmlns:xml='http:&sol;&sol;www.w3.org/XML/1998/namespace'/>"),
            "sol",
        ),
    ] {
        let read = read_cell(input.as_bytes(), a1, None).map(|_| ());
        let edited = edit(&input, "A1", 2.0.into()).map(|_| ());
        for result in [read, edited] {
            assert!(
                matches!(&result, Err(Error::Xml(message)) if message.contains(entity)),
                "{input}: {result:?}"
            );
        }
    }
    // A reference in text is reported by name, in cells and shared strings.
    const UNDEFINED: &str = "undefined XML entity &nbsp;";
    let undefined =
        |result: Result<()>| matches!(result, Err(Error::Xml(message)) if message == UNDEFINED);
    assert!(undefined(
        read_cell(cell("", "&nbsp;").as_bytes(), a1, None).map(|_| ())
    ));
    let table = format!("<sst xmlns='{TRANSITIONAL}'><si><t>a&nbsp;b</t></si></sst>");
    assert!(undefined(
        read::read_shared_strings(table.as_bytes()).map(|_| ())
    ));
}

#[test]
fn validates_xml_declarations_and_retains_valid_unicode_names() {
    for declaration in [
        " <?xml version='1.0'?>",
        "<!--first--><?xml version='1.0'?>",
        "<?xml version='1.0' version='1.0'?>",
        "<?xml version='1.0' standalone='maybe'?>",
        "<?xml version='1.0' standalone='yes' encoding='UTF-8'?>",
        "<?xml version='1.0' unknown='value'?>",
    ] {
        let input = sheet("<sheetData/>").replace("<?xml version=\"1.0\"?>", declaration);
        assert!(edit(&input, "A1", 2.0.into()).is_err(), "{input}");
    }
    let input = sheet(
        "<?vendor:tool untouched?><sheetData/><x:外 x:À·=\"keep\"/><x:\u{10000} x:\u{200c}start=\"keep\"/>",
    )
    .replace(
        "<?xml version=\"1.0\"?>",
        "<?xml version='1.0' encoding='utf-8' standalone='yes'?>\r\n",
    );
    let changed = edit(&input, "A1", 2.0.into()).unwrap();
    assert_eq!(
        changed,
        input.replace(
            "<sheetData/>",
            "<sheetData><row r=\"1\"><c r=\"A1\"><v>2</v></c></row></sheetData>"
        )
    );
}

fn batch(xml: &str, changes: &[(&str, CellValue)]) -> Result<String> {
    let cells: BTreeMap<CellRef, CellValue> = changes
        .iter()
        .map(|(address, value)| (address.parse().unwrap(), value.clone()))
        .collect();
    Ok(String::from_utf8(
        patch_cells(xml.as_bytes(), &cells, || None, /* date1904: */ false)?.into_owned(),
    )
    .unwrap())
}

#[test]
fn batch_inserts_at_shared_gaps_and_extends_metadata_once() {
    let input = sheet(
        r#"<dimension ref='C4'/><sheetData><!--keep--><row r='4' spans='3:3'><c r='C4' s='7'><v>3</v></c><extLst><x:future/></extLst></row><row r='8' spans='2:2' /></sheetData>"#,
    );
    let changed = batch(
        &input,
        &[
            ("B8", 8.0.into()),
            ("E4", 5.0.into()),
            ("B2", 2.0.into()),
            ("B4", 2.0.into()),
            ("D4", 4.0.into()),
            ("A4", 1.0.into()),
            ("A2", 1.0.into()),
            ("D3", 3.0.into()),
            ("A8", 1.0.into()),
            ("XFD1048576", CellValue::Blank),
        ],
    )
    .unwrap();
    assert!(changed.contains("<dimension ref='A2:E8'/><sheetData><!--keep--><row r=\"2\"><c r=\"A2\"><v>1</v></c><c r=\"B2\"><v>2</v></c></row><row r=\"3\"><c r=\"D3\"><v>3</v></c></row><row r='4' spans='1:5'>"));
    assert!(changed.contains("<c r=\"A4\"><v>1</v></c><c r=\"B4\"><v>2</v></c><c r='C4' s='7'><v>3</v></c><c r=\"D4\"><v>4</v></c><c r=\"E4\"><v>5</v></c><extLst><x:future/></extLst>"));
    assert!(changed.contains(
        "<row r='8' spans='1:2' ><c r=\"A8\"><v>1</v></c><c r=\"B8\"><v>8</v></c></row>"
    ));
}

#[test]
fn batch_expands_one_empty_data_element_for_many_rows() {
    let input = sheet("<sheetData />");
    let changed = batch(
        &input,
        &[
            ("C7", true.into()),
            ("A1", 1.0.into()),
            ("B1", 2.0.into()),
            ("A5", "five".into()),
        ],
    )
    .unwrap();
    assert!(changed.contains("<sheetData ><row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"B1\"><v>2</v></c></row><row r=\"5\">"));
    assert_eq!(changed.matches("<sheetData").count(), 1);
    assert_eq!(changed.matches("</sheetData>").count(), 1);
    assert_eq!(
        read_cell(changed.as_bytes(), "C7".parse().unwrap(), None)
            .unwrap()
            .value,
        CellValue::Bool(true)
    );
}

#[test]
fn batch_replaces_and_inserts_at_the_same_byte_position() {
    let input = sheet(
        r#"<sheetData><row r="1"><c r="B1" t="inlineStr"><is><t>old</t></is></c><c r="D1"><v>4</v></c></row></sheetData>"#,
    );
    let changed = batch(
        &input,
        &[
            ("A1", 1.0.into()),
            ("B1", CellValue::Blank),
            ("C1", 3.0.into()),
            ("D1", 8.0.into()),
        ],
    )
    .unwrap();
    assert!(changed.contains(
        r#"<c r="A1"><v>1</v></c><c r="B1"></c><c r="C1"><v>3</v></c><c r="D1"><v>8</v></c>"#
    ));
}

#[test]
fn batch_guards_include_all_edits_and_accept_merged_anchors() {
    let input = sheet(
        r#"<sheetData><row r="1"><c r="A1"><f t="array" ref="A1:C2">1</f><v>1</v></c></row></sheetData><mergeCells><mergeCell ref="E3:G5"/><mergeCell ref="A7:XFD1048576"/></mergeCells>"#,
    );
    assert!(batch(&input, &[("D1", 2.0.into()), ("C2", 8.0.into())]).is_err());
    assert!(batch(&input, &[("D1", 2.0.into()), ("E4", CellValue::Blank)]).is_err());
    assert!(batch(&input, &[("E3", 2.0.into()), ("A7", 8.0.into())]).is_ok());
    assert!(batch(&input, &[("XFD1048576", 8.0.into())]).is_err());
    assert!(batch(&input, &[("G2", 2.0.into()), ("H6", 8.0.into())]).is_ok());
    // Large batches over many ranges use a sweep, which reports the same
    // refusals as direct membership checks.
    let merges: String = (0..20)
        .map(|index| format!("<mergeCell ref='A{row}:B{row}'/>", row = 2 * index + 1))
        .collect();
    let formulas: String = (0..20)
        .map(|index| {
            let row = 2 * index + 1;
            format!("<row r='{row}'><c r='D{row}'><f t='array' ref='D{row}:E{row}'>1</f><v>1</v></c></row>")
        })
        .collect();
    let input = sheet(&format!(
        "<sheetData>{formulas}</sheetData><mergeCells>{merges}</mergeCells>"
    ));
    for (extra, reason) in [
        ("B5", "is merged into another cell's range"),
        ("E7", "belongs to a formula range"),
    ] {
        let mut changes: Vec<(String, CellValue)> = (1..=40)
            .map(|row| (format!("A{row}"), CellValue::from(row)))
            .collect();
        changes.push((extra.into(), 1.into()));
        let changes: Vec<(&str, CellValue)> = changes
            .iter()
            .map(|(address, value)| (address.as_str(), value.clone()))
            .collect();
        assert!(
            protected_cell(batch(&input, &changes), extra, reason),
            "{extra}"
        );
    }
}

#[test]
fn reads_values_formulas_styles_and_preserves_requested_order() {
    let input = sheet(
        r#"<sheetData><row r="1"><c r="A1" s="7"><f>SUM(B1,C1)&amp;2</f><v>42.5</v></c><c r="B1" t="b"><v>1</v></c><c r="C1" t="inlineStr"><is><r><rPr><b/></rPr><t>rich &amp; </t></r><r><t>text</t></r><rPh sb="0" eb="1"><t>phonetic</t></rPh></is></c><c r="D1" t="s"><v>1</v></c><c r="E1" t="e"><v>#DIV/0!</v></c><c r="F1" t="str"><f>"cached"</f><v>cached</v></c><c r="G1" s="9"/></row></sheetData>"#,
    );
    let addresses: Vec<CellRef> = ["C1", "A1", "D1", "B1", "E1", "F1", "G1", "A2", "C1"]
        .iter()
        .map(|address| address.parse().unwrap())
        .collect();
    let strings = SharedStrings::from_iter(["first", "second"]);
    let cells = prepared_worksheet::read_cells(
        input.as_bytes(),
        &addresses,
        || Ok(Some(&strings)),
        /* date1904: */ false,
    )
    .unwrap();
    assert_eq!(
        cells
            .iter()
            .map(|cell| cell.value.clone())
            .collect::<Vec<_>>(),
        vec![
            "rich & text".into(),
            42.5.into(),
            "second".into(),
            true.into(),
            CellValue::Error("#DIV/0!".into()),
            "cached".into(),
            CellValue::Blank,
            CellValue::Blank,
            "rich & text".into(),
        ]
    );
    assert_eq!(cells[1].formula.as_deref(), Some("SUM(B1,C1)&2"));
    assert_eq!(cells[1].style_index, Some(7));
    assert_eq!(cells[6].style_index, Some(9));
    assert_eq!(cells[7].style_index, None);
}

#[test]
fn shared_strings_are_loaded_only_for_requested_shared_cells() {
    let input = sheet(
        r#"<sheetData><row r="1"><c r="A1"><v>7</v></c><c r="B1" t="s"><v>0</v></c><c r="C1" t="s"><v>1</v></c></row></sheetData>"#,
    );
    let numeric = prepared_worksheet::read_cells(
        input.as_bytes(),
        &["A1".parse().unwrap()],
        || panic!("reading a number should not access shared strings"),
        /* date1904: */ false,
    )
    .unwrap();
    assert_eq!(numeric[0].value, CellValue::Number(7.0));

    // The loader is asked for each shared cell; the workbook keeps the table.
    let strings = SharedStrings::from_iter(["first", "second"]);
    let mut loads = 0;
    let contents = prepared_worksheet::read_cells(
        input.as_bytes(),
        &[
            "B1".parse().unwrap(),
            "A1".parse().unwrap(),
            "C1".parse().unwrap(),
            "B1".parse().unwrap(),
        ],
        || {
            loads += 1;
            Ok(Some(&strings))
        },
        /* date1904: */ false,
    )
    .unwrap();
    assert_eq!(loads, 3);
    assert_eq!(
        contents
            .into_iter()
            .map(|cell| cell.value)
            .collect::<Vec<_>>(),
        vec!["first".into(), 7.0.into(), "second".into(), "first".into()]
    );
}

#[test]
fn shared_string_tables_return_each_decoded_string_exactly() {
    use std::fmt::Write as _;

    // Empty, multi-byte and escaped text, split into runs at varying points,
    // so that strings of every length meet at every kind of boundary.
    let pieces = [
        "",
        "a",
        "雪",
        "😀",
        "_x0041_",
        "&<>",
        "\r\n",
        " ",
        "\u{1}",
        "_xD83D__xDE00_",
    ];
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move |bound: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % bound as u64) as usize
    };
    let mut expected = Vec::new();
    let mut body = String::new();
    for _ in 0..2_000 {
        let runs: Vec<String> = (0..next(4))
            .map(|_| (0..next(4)).map(|_| pieces[next(pieces.len())]).collect())
            .collect();
        let t = |text: &str| format!("<t xml:space='preserve'>{}</t>", patch::text_escaped(text));
        match runs.as_slice() {
            [] => body.push_str("<si/>"),
            [text] if next(2) == 0 => write!(body, "<si>{}</si>", t(text)).unwrap(),
            runs => {
                let markup: String = runs
                    .iter()
                    .map(|text| format!("<r>{}</r>", t(text)))
                    .collect();
                write!(body, "<si>{markup}</si>").unwrap();
            }
        }
        expected.push(runs.concat());
    }
    let table =
        read::read_shared_strings(format!("<sst xmlns='{TRANSITIONAL}'>{body}</sst>").as_bytes())
            .unwrap();
    for (index, text) in expected.iter().enumerate() {
        assert!(
            matches!(table.get(index), Some(Ok(entry)) if entry == text),
            "{index}"
        );
    }
    assert!(table.get(expected.len()).is_none());
    assert_eq!(table, expected);
}

#[test]
fn reads_shared_rich_strings_excel_escapes_and_xml_newlines() {
    let table = format!(r#"<sst xmlns="{STRICT}"><si><t>first</t></si><si><r><t>_x005F_x0041_</t></r><r><t>_xD83D__xDE00_&#13;<![CDATA[\r\nline\r]]></t></r><rPh sb="0" eb="1"><t>ignore</t></rPh></si><si><r><t>_</t></r><r><t>x0041_</t></r></si></sst>"#).replace("\\r", "\r").replace("\\n", "\n");
    assert_eq!(
        read::read_shared_strings(table.as_bytes()).unwrap(),
        vec!["first", "_x0041_😀\r\nline\n", "_x0041_"]
    );
    let input = sheet(
        "<sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>a\r\nb\rc&#13;d&amp;_x000A_</t></is></c></row></sheetData>",
    );
    assert_eq!(
        read_cell(input.as_bytes(), "A1".parse().unwrap(), None)
            .unwrap()
            .value,
        CellValue::Text("a\nb\nc\rd&\n".into())
    );
    let original = "_x0041_\r😀&_Xabcd_";
    let changed = edit(&sheet("<sheetData/>"), "A1", original.into()).unwrap();
    assert_eq!(
        read_cell(changed.as_bytes(), "A1".parse().unwrap(), None)
            .unwrap()
            .value,
        CellValue::Text(original.into())
    );
}

#[test]
fn hidden_shared_strings_cannot_shift_visible_string_indices() {
    let mc = "http://schemas.openxmlformats.org/markup-compatibility/2006";
    for namespace in [TRANSITIONAL, STRICT] {
        let table = format!(
            "<sst xmlns='{namespace}' xmlns:mc='{mc}' xmlns:x='urn:custom'><mc:AlternateContent><mc:Choice Requires='x'><si><t>first</t></si></mc:Choice><mc:Fallback><si><t>first fallback</t></si></mc:Fallback></mc:AlternateContent><si><t>second</t></si></sst>"
        );
        assert!(matches!(
            read::read_shared_strings(table.as_bytes()),
            Err(Error::Unsupported(_))
        ));
        let foreign = format!(
            "<sst xmlns='{namespace}' xmlns:x='urn:custom'><x:si><x:t>extension</x:t></x:si><si><t>first</t></si><si><t>second</t></si></sst>"
        );
        assert_eq!(
            read::read_shared_strings(foreign.as_bytes()).unwrap(),
            vec!["first", "second"]
        );
    }
}

#[test]
fn hidden_rich_text_cannot_produce_a_partial_scalar_read() {
    for payload in [
        "<t>visible</t><x:alternate><t>hidden</t></x:alternate>",
        "<t>visible</t><x:alternate><r><t>hidden</t></r></x:alternate>",
        "<r><t>visible</t><x:alternate><t>hidden</t></x:alternate></r>",
        "<t>visible</t><rPh sb='0' eb='1'><x:alternate><t>hidden</t></x:alternate></rPh>",
    ] {
        let inline = sheet(&format!(
            "<sheetData><row r='1'><c r='A1' t='inlineStr'><is>{payload}</is></c></row></sheetData>"
        ));
        assert!(matches!(
            read_cell(inline.as_bytes(), "A1".parse().unwrap(), None),
            Err(Error::Unsupported(_))
        ));
        // In the shared-string table, only that entry is refused.
        let table = format!(
            "<sst xmlns='{TRANSITIONAL}' xmlns:x='urn:custom'><si>{payload}</si><si><t>next</t></si></sst>"
        );
        let table = read::read_shared_strings(table.as_bytes()).unwrap();
        assert!(matches!(table.get(0), Some(Err(Error::Unsupported(_)))));
        assert!(matches!(table.get(1), Some(Ok("next"))));
    }
}

#[test]
fn rejects_invalid_read_values_and_missing_shared_strings() {
    for (kind, payload) in [
        ("n", "NaN"),
        ("n", "infinity"),
        ("n", "\u{a0}1\u{a0}"),
        ("b", "2"),
        ("b", "\u{a0}true\u{a0}"),
        ("s", "-1"),
        ("s", "\u{a0}0\u{a0}"),
        ("d", "2026-01-01T00:00:00+01:00"),
        ("d", "2026-02-29"),
        ("x", "1"),
    ] {
        let input = sheet(&format!(
            r#"<sheetData><row r="1"><c r="A1" t="{kind}"><v>{payload}</v></c></row></sheetData>"#
        ));
        assert!(
            read_cell(
                input.as_bytes(),
                "A1".parse().unwrap(),
                Some(&SharedStrings::default())
            )
            .is_err()
        );
    }
    let input = sheet(r#"<sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData>"#);
    assert!(matches!(
        read_cell(input.as_bytes(), "A1".parse().unwrap(), None),
        Err(Error::InvalidWorkbook(message))
            if message == "A1: shared-string cell, but the workbook has no shared-string table"
    ));
    assert!(matches!(
        read_cell(
            input.as_bytes(),
            "A1".parse().unwrap(),
            Some(&SharedStrings::default())
        ),
        Err(Error::InvalidWorkbook(_))
    ));
}

#[test]
fn can_write_and_read_excel_error_values() {
    let input = sheet(r#"<sheetData><row r="1"><c r="A1" s="8"><v>1</v></c></row></sheetData>"#);
    let changed = edit(&input, "A1", CellValue::Error("#SPILL!".into())).unwrap();
    let cell = read_cell(changed.as_bytes(), "A1".parse().unwrap(), None).unwrap();
    assert_eq!(cell.value, CellValue::Error("#SPILL!".into()));
    assert_eq!(cell.style_index, Some(8));
}

#[test]
fn matching_values_retain_exact_xml_and_do_not_change_metadata() {
    let input = sheet(
        r#"<dimension ref='C3'/><sheetData><row r='1' spans='2:3'><c r='A1' t='n'><v> 1.00 </v></c><c r='B1' t='b'><v>true</v></c><c r='C1' t='inlineStr'><is><r><rPr><b/></rPr><t>same</t></r></is></c><c r='D1' t='str'><v>_x005F_x0041_</v></c><c r='E1' t='e'><v>#REF!</v></c><c r='F1' s='2' /></row></sheetData>"#,
    );
    assert_eq!(
        batch(
            &input,
            &[
                ("A1", 1.0.into()),
                ("B1", true.into()),
                ("C1", "same".into()),
                ("D1", "_x0041_".into()),
                ("E1", CellValue::Error("#REF!".into())),
                ("F1", CellValue::Blank)
            ]
        )
        .unwrap(),
        input
    );
    let unknown = input.replace("<rPr><b/></rPr>", "<rPr><x:future/></rPr>");
    assert!(batch(&unknown, &[("C1", "same".into())]).is_err());
}

#[test]
fn no_op_patches_borrow_the_original_xml() {
    let input = sheet(
        "<sheetData><row r='1'><c r='A1'><v>1.00</v></c><c r='B1' t='inlineStr'><is><r><t>same</t></r></is></c></row></sheetData>",
    );
    for changes in [
        BTreeMap::new(),
        BTreeMap::from([("A1".parse().unwrap(), 1.0.into())]),
        BTreeMap::from([("B1".parse().unwrap(), "same".into())]),
        BTreeMap::from([("C1".parse().unwrap(), CellValue::Blank)]),
    ] {
        assert!(matches!(
            patch_cells(
                input.as_bytes(),
                &changes,
                || None,
                /* date1904: */ false
            )
            .unwrap(),
            Cow::Borrowed(_)
        ));
    }
}

#[test]
fn edits_are_no_ops_exactly_when_the_cell_already_reads_as_the_value() {
    // Clearing an existing payload is a change even when it reads as blank;
    // every other edit is a no-op only if a read returns the same value.
    let strings = read::read_shared_strings(
        format!(
            "<sst xmlns='{TRANSITIONAL}'><si><t>first</t></si><si><t xml:space='preserve'> 2</t></si><si><t>_xD800_</t></si></sst>"
        )
        .as_bytes(),
    )
    .unwrap();
    let address: CellRef = "A1".parse().unwrap();
    let values = [
        CellValue::Blank,
        2.5.into(),
        2.0.into(),
        3.0.into(),
        0.0.into(),
        true.into(),
        false.into(),
        "first".into(),
        " 2".into(),
        " 2A".into(),
        "2".into(),
        "".into(),
        CellValue::Error("#N/A".into()),
    ];
    for cell in [
        "<c r='A1'><v> 2.50 </v></c>",
        "<c r='A1' t='n'><v/></c>",
        "<c r='A1'><v>2 x</v></c>",
        "<c r='A1' t='b'><v>true</v></c>",
        "<c r='A1' t='b'><v> 0 </v></c>",
        "<c r='A1' t='b'><v>yes</v></c>",
        "<c r='A1' t='d'><v>1900-01-03</v></c>",
        "<c r='A1' t='d'><v>1904-01-03</v></c>",
        "<c r='A1' t='d'><v>2024-02-30</v></c>",
        "<c r='A1' t='str'><v> 2_x0041_</v></c>",
        "<c r='A1' t='str'><v>2</v></c>",
        "<c r='A1' t='e'><v>#N/A</v></c>",
        "<c r='A1' t='inlineStr'><is><r><t>fi</t></r><r><t>rst</t></r></is></c>",
        "<c r='A1' t='inlineStr'><v>first</v></c>",
        "<c r='A1'><is><t>first</t></is></c>",
        "<c r='A1' t='s'><v> 0 </v></c>",
        "<c r='A1' t='s'><v>1</v></c>",
        "<c r='A1' t='s'><v>2</v></c>",
        "<c r='A1' t='s'><v>3</v></c>",
        "<c r='A1' t='x'><v>2</v></c>",
    ] {
        let input = sheet(&format!("<sheetData><row r='1'>{cell}</row></sheetData>"));
        for date1904 in [false, true] {
            let read = prepared_worksheet::read_cells(
                input.as_bytes(),
                &[address],
                || Ok(Some(&strings)),
                date1904,
            )
            .ok()
            .map(|mut cells| cells.remove(0).value);
            for value in &values {
                let patched = patch_cells(
                    input.as_bytes(),
                    &BTreeMap::from([(address, value.clone())]),
                    || Some(&strings),
                    date1904,
                )
                .unwrap();
                let no_op = *value != CellValue::Blank && read.as_ref() == Some(value);
                assert_eq!(
                    matches!(patched, Cow::Borrowed(_)),
                    no_op,
                    "{cell} {value:?} date1904={date1904}"
                );
            }
        }
    }
}

#[test]
fn range_guards_match_rectangle_membership_with_overlapping_ranges() {
    let mut seed = 17u32;
    let mut next = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        seed
    };
    let mut ranges = Vec::new();
    for _ in 0..100 {
        let first_row = 1 + next() % 30;
        let first_column = 1 + (next() % 10) as u16;
        ranges.push(CellRange::new(
            CellRef {
                row: first_row,
                column: first_column,
            },
            CellRef {
                row: first_row + next() % 5,
                column: first_column + (next() % 3) as u16,
            },
        ));
    }
    for kind in [RangeKind::Formula, RangeKind::Merged] {
        for row in 1..=35 {
            for column in 1..=14 {
                let cell = CellRef { row, column };
                let blocked = ranges.iter().any(|range| {
                    range.contains(cell) && (kind == RangeKind::Formula || cell != range.first())
                });
                assert_eq!(
                    validation::validate_ranges(
                        &BTreeMap::from([(cell, CellValue::Blank)]),
                        &ranges,
                        kind
                    )
                    .is_err(),
                    blocked
                );
                // The same target in a large batch exercises the event
                // sweep. Other cells lie past every generated rectangle.
                let mut batch: BTreeMap<CellRef, CellValue> = (40..=70)
                    .map(|row| (CellRef { row, column: 1 }, CellValue::Blank))
                    .collect();
                batch.insert(cell, CellValue::Blank);
                assert_eq!(
                    validation::validate_ranges(&batch, &ranges, kind).is_err(),
                    blocked
                );
            }
        }
    }
    let boundary = CellRange::new(
        CellRef {
            row: 1,
            column: CellRef::MAX_COLUMN,
        },
        CellRef {
            row: CellRef::MAX_ROW,
            column: CellRef::MAX_COLUMN,
        },
    );
    assert!(
        validation::validate_ranges(
            &BTreeMap::from([("XFD1".parse().unwrap(), CellValue::Blank)]),
            &[boundary],
            RangeKind::Merged
        )
        .is_ok()
    );
    assert!(
        validation::validate_ranges(
            &BTreeMap::from([("XFD1048576".parse().unwrap(), CellValue::Blank)]),
            &[boundary],
            RangeKind::Merged
        )
        .is_err()
    );
}

#[test]
fn range_envelope_includes_interior_edits_and_keeps_overlapping_anchor_guards() {
    let mut ranges = vec![CellRange::new("AF1".parse().unwrap(), "AG30".parse().unwrap()); 40];
    ranges.push(CellRange::new(
        "B15".parse().unwrap(),
        "F15".parse().unwrap(),
    ));
    let mut cells: BTreeMap<CellRef, CellValue> = (1..=10)
        .chain([30])
        .map(|row| (CellRef::try_new(row, 26).unwrap(), CellValue::Blank))
        .collect();
    cells.insert("C15".parse().unwrap(), CellValue::Blank);
    for kind in [RangeKind::Formula, RangeKind::Merged] {
        let error = validation::validate_ranges(&cells, &ranges, kind).unwrap_err();
        assert!(error.to_string().contains("C15"));
    }
    cells.remove(&"C15".parse().unwrap());
    cells.insert("B15".parse().unwrap(), CellValue::Blank);
    assert!(validation::validate_ranges(&cells, &ranges, RangeKind::Merged).is_ok());
    assert!(validation::validate_ranges(&cells, &ranges, RangeKind::Formula).is_err());
    ranges.push(CellRange::new(
        "A15".parse().unwrap(),
        "D15".parse().unwrap(),
    ));
    let error = validation::validate_ranges(&cells, &ranges, RangeKind::Merged).unwrap_err();
    assert!(error.to_string().contains("B15"));
}

#[test]
fn worksheet_range_references_accept_markers_anywhere_but_not_reversed_corners() {
    // Worksheet references differ from `CellRange`'s syntax on purpose: `$`
    // markers may appear anywhere, and reversed corners are refused rather
    // than normalized.
    let merged = |reference: &str| {
        sheet(&format!(
            "<sheetData><row r='1'><c r='A1'><v>1</v></c></row></sheetData><mergeCells><mergeCell ref='{reference}'/></mergeCells>"
        ))
    };
    for reference in ["C1:D2", "c1:d2", "$C$1:$D$2", "C$$1:D2$", "$C1:D$2"] {
        let input = merged(reference);
        assert!(edit(&input, "C1", 2.0.into()).is_ok(), "{reference}");
        assert!(edit(&input, "E3", 2.0.into()).is_ok(), "{reference}");
        assert!(
            matches!(
                edit(&input, "D2", 2.0.into()),
                Err(Error::ProtectedCell { cell, reason, .. })
                    if cell.to_string() == "D2" && reason == protection::MERGED
            ),
            "{reference}"
        );
    }
    for reference in [
        "D2:C1", "C2:D1", "D1:C2", " C1:D2", "C1:D2 ", "C1:D2:E3", "C1::D2", "C1:", ":D2", "",
        "C:D", "1:2", "C1:XFE1", "$", "C1:$",
    ] {
        let message = format!("invalid cell range {reference:?}");
        assert!(
            matches!(
                edit(&merged(reference), "A1", 2.0.into()),
                Err(Error::InvalidWorkbook(m)) if m == message
            ),
            "{reference:?}"
        );
    }
    let formula = sheet(
        "<sheetData><row r='1'><c r='A1'><f t='array' ref='$b$2:C$3'>1</f><v>1</v></c></row></sheetData>",
    );
    assert!(edit(&formula, "D4", 2.0.into()).is_ok());
    assert!(matches!(
        edit(&formula, "B2", 2.0.into()),
        Err(Error::ProtectedCell { reason, .. }) if reason == protection::FORMULA_RANGE
    ));
    // A dimension grows to the edited cells and is written without markers.
    for (reference, cell, grown) in [
        ("$B$2", "B2", None),
        ("b2:c3", "C3", None),
        ("$B$2", "C3", Some("B2:C3")),
        ("B$2:c3", "A1", Some("A1:C3")),
        ("B2", "B9", Some("B2:B9")),
    ] {
        let input = sheet(&format!(
            "<dimension ref='{reference}'/><sheetData><row r='2'><c r='B2'><v>1</v></c></row></sheetData>"
        ));
        let changed = edit(&input, cell, 2.0.into()).unwrap();
        let expected = format!("<dimension ref='{}'/>", grown.unwrap_or(reference));
        assert!(changed.contains(&expected), "{reference} {cell}: {changed}");
    }
    assert!(matches!(
        edit(&sheet("<dimension ref='C3:B2'/><sheetData/>"), "A1", 2.0.into()),
        Err(Error::Unsupported(m)) if m == "invalid cell range \"C3:B2\""
    ));
}

#[test]
fn attribute_lists_end_only_at_the_tag_terminator() {
    for tag in [
        "<x:t /xml:space='preserve'>text</x:t>",
        "<x:t xml:space='preserve'/ >text</x:t>",
        "<x:t xml:space='preserve' / >text</x:t>",
        "<x:t xml:space='preserve'/x:a='1'>text</x:t>",
        "<x:t xml:space='preserve' /x:a='1'/>",
        "<x:t xml:space='preserve'x:a='1'/>",
        "<x:t x:a='1' x:a='2'/>",
        "<x:t x:a=1/>",
        "<x:t x:a/>",
    ] {
        let input = sheet(&format!("<sheetData/>{tag}"));
        assert!(
            matches!(parse::parse(input.as_bytes()), Err(Error::Xml(_))),
            "{tag}"
        );
    }
    for tag in [
        "<x:t xml:space='preserve' />",
        "<x:t\txml:space = 'preserve'\r\n/>",
        "<x:t x:a='1/>'>text</x:t>",
        "<x:t\n>text</x:t\n>",
    ] {
        let input = sheet(&format!("<sheetData/>{tag}"));
        let XmlDocument {
            elements,
            attributes,
            ..
        } = parse::parse(input.as_bytes()).unwrap();
        let element = elements.last().unwrap();
        assert!(element.attributes(&attributes).all(|attribute| {
            input.as_bytes()[attribute.end() - 1] == input.as_bytes()[attribute.value_start() - 1]
        }));
        assert_eq!(
            &input[element.start()..element.start() + 4],
            "<x:t",
            "{tag}"
        );
    }
}

#[test]
fn duplicate_attributes_are_found_in_tags_of_any_length() {
    let tag = |count: usize, duplicate: &str| {
        let attributes: String = (0..count)
            .map(|index| {
                let prefix = if index % 2 == 0 { "a" } else { "b" };
                format!(" {prefix}:n{index}='{index}' n{index}='{index}'")
            })
            .collect();
        sheet(&format!(
            "<sheetData/><x:t xmlns:a='urn:same' xmlns:b='urn&#58;same'{attributes}{duplicate}/>"
        ))
    };
    for count in [1, 7, 8, 9, 20, 40] {
        let input = tag(count, "");
        assert!(parse::parse(input.as_bytes()).is_ok(), "{count}");
        for duplicate in [
            " n0='again'".to_owned(),
            format!(" n{}='again'", count - 1),
            // Different prefixes bound to one namespace repeat an expanded name.
            " b:n0='again'".to_owned(),
            format!(" a:n{}='again'", count - 1),
        ] {
            let input = tag(count, &duplicate);
            assert!(
                matches!(parse::parse(input.as_bytes()), Err(Error::Xml(_))),
                "{count}{duplicate}"
            );
        }
    }
}

#[test]
#[cfg(target_pointer_width = "64")]
fn parse_records_stay_compact() {
    use super::parse::Element;
    use crate::xml::Attribute;
    assert_eq!(size_of::<Element>(), 48);
    assert_eq!(size_of::<Attribute>(), 24);
}

#[test]
fn element_names_match_whole_local_names() {
    let input = sheet(&format!(
        "<sheetData/><c/><s:c xmlns:s='{TRANSITIONAL}'/><x:c/><cc/><xc/><s:xc xmlns:s='{TRANSITIONAL}'/><x:sheetData/>"
    ));
    let XmlDocument {
        source, elements, ..
    } = parse::parse(input.as_bytes()).unwrap();
    let main = elements[0].namespace;
    let mut matches = Vec::new();
    for element in &elements {
        for name in ["c", "xc", "sheetData", "worksheet"] {
            let local = element.namespace == main && element.local_name(source) == name;
            assert_eq!(element.is(source, main, name), local, "{name}");
            if local {
                matches.push(format!("{}={name}", element.name(source)));
            }
        }
    }
    assert_eq!(
        matches,
        [
            "worksheet=worksheet",
            "sheetData=sheetData",
            "c=c",
            "s:c=c",
            "xc=xc",
            "s:xc=xc"
        ]
    );
}

#[test]
fn released_subtrees_take_their_normalized_attribute_values() {
    // Every attribute but `n` has a value that normalization changes.
    let input = format!(
        "<sst xmlns='{TRANSITIONAL}' a='root&#9;value'>{}</sst>",
        (0..6)
            .map(|n| format!(
                "<si n='{n}' a='&#{};' b='x&amp;{n}'><t c='one&#10;{n}'/></si>",
                65 + n
            ))
            .collect::<String>()
    );
    let values = |document: &XmlDocument, index: usize| {
        iter::once(index)
            .chain(document.elements[index].descendants())
            .flat_map(|element| document.elements[element].attributes(&document.attributes))
            .map(|attribute| attribute.value.to_owned())
            .collect::<Vec<_>>()
    };
    let expected = |n: u8| {
        [
            n.to_string(),
            char::from(65 + n).to_string(),
            format!("x&{n}"),
            format!("one\n{n}"),
        ]
    };
    let mut entry = 0;
    let document = parse::parse_with(input.as_bytes(), |document, index| {
        assert_eq!(values(document, index), expected(entry));
        // Keep every other string and release the rest.
        let release = entry % 2 == 1;
        entry += 1;
        release
    })
    .unwrap();
    let kept: Vec<Vec<String>> = document.elements[0]
        .children(&document.elements)
        .map(|index| values(&document, index))
        .collect();
    assert_eq!(kept, [expected(0), expected(2), expected(4)]);
    assert_eq!(
        document.elements[0]
            .attribute(&document.attributes, "a")
            .unwrap()
            .value,
        "root\tvalue"
    );
    // The root's value and three for each kept string.
    assert_eq!(document.attributes.normalized_len(), 1 + 3 * 3);
}

#[test]
fn xml_variants_follow_the_shared_policy() {
    let nested = |depth: usize, root: &str| {
        format!(
            "<{root} xmlns='{TRANSITIONAL}'>{}{}</{root}>",
            "<x>".repeat(depth),
            "</x>".repeat(depth)
        )
    };
    // The root and 255 nested elements are accepted; one more is refused.
    let accepted = nested(255, "sst");
    assert_eq!(
        read::read_shared_strings(accepted.as_bytes()).unwrap(),
        Vec::<String>::new()
    );
    for root in ["worksheet", "sst"] {
        let input = nested(256, root);
        let error = if root == "sst" {
            read::read_shared_strings(input.as_bytes()).unwrap_err()
        } else {
            read_cell(input.as_bytes(), "A1".parse().unwrap(), None).unwrap_err()
        };
        assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    }
    let mut invalid = sheet("<sheetData/>").into_bytes();
    invalid.insert(invalid.len() - 2, 0xff);
    assert!(matches!(
        read_cell(&invalid, "A1".parse().unwrap(), None),
        Err(Error::Xml(_))
    ));
    let encoded =
        sheet("<sheetData/>").replace("version=\"1.0\"", "version=\"1.0\" encoding=\"UTF-16\"");
    assert!(matches!(
        read_cell(encoded.as_bytes(), "A1".parse().unwrap(), None),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn read_errors_name_the_cell() {
    // Malformed cell data makes the workbook invalid.
    for (kind, payload, message) in [
        ("n", "x", "B2: invalid numeric cell value"),
        ("n", "1e999", "B2: non-finite numeric cell value"),
        ("b", "2", "B2: invalid boolean cell value"),
        ("d", "2026-02-29", "B2: invalid ISO 8601 date cell value"),
        ("s", "-1", "B2: invalid shared-string index"),
        ("s", "0x1", "B2: invalid shared-string index"),
        ("q", "1", "B2: invalid cell value type \"q\""),
        (
            "inlinestr",
            "1",
            "B2: invalid cell value type \"inlinestr\"",
        ),
    ] {
        let input = sheet(&format!(
            "<sheetData><row r='2'><c r='B2' t='{kind}'><v>{payload}</v></c></row></sheetData>"
        ));
        let error = read_cell(
            input.as_bytes(),
            "B2".parse().unwrap(),
            Some(&SharedStrings::default()),
        )
        .unwrap_err();
        assert!(
            matches!(&error, Error::InvalidWorkbook(error) if error == message),
            "{message}: {error:?}"
        );
    }
    let input = sheet("<sheetData><row r='2'><c r='B2' s='x'/></row></sheetData>");
    assert!(matches!(
        read_cell(input.as_bytes(), "B2".parse().unwrap(), None),
        Err(Error::InvalidWorkbook(error)) if error == "B2: invalid cell style index"
    ));
    // So do payloads that do not match their cell type or each other.
    for (cell, message) in [
        (
            "<c r='B2' t='inlineStr'><v>1</v></c>",
            "B2: inline string cell without an inline payload",
        ),
        (
            "<c r='B2' t='s'><is><t>x</t></is></c>",
            "B2: non-inline cell has an inline payload",
        ),
        (
            "<c r='B2'><is><t>x</t></is></c>",
            "B2: non-inline cell has an inline payload",
        ),
        (
            "<c r='B2'><v>1</v><v>2</v></c>",
            "B2: cell has multiple value payloads",
        ),
        (
            "<c r='B2' t='inlineStr'><v>1</v><is><t>x</t></is></c>",
            "B2: cell has multiple value payloads",
        ),
        (
            "<c r='B2'><f>1</f><f>2</f><v>2</v></c>",
            "B2: cell has multiple formulas",
        ),
    ] {
        let input = sheet(&format!("<sheetData><row r='2'>{cell}</row></sheetData>"));
        let error = read_cell(input.as_bytes(), "B2".parse().unwrap(), None).unwrap_err();
        assert!(
            matches!(&error, Error::InvalidWorkbook(error) if error == message),
            "{message}: {error:?}"
        );
    }
    // Valid ISO 8601 values outside the supported forms are unsupported.
    for (payload, message) in [
        (
            "2026-01-01T00:00:00+01:00",
            "B2: ISO 8601 date cell value with a time-zone offset",
        ),
        (
            "1899-12-30",
            "B2: ISO 8601 date cell value before the start of the workbook's date system",
        ),
    ] {
        let input = sheet(&format!(
            "<sheetData><row r='2'><c r='B2' t='d'><v>{payload}</v></c></row></sheetData>"
        ));
        let error = read_cell(input.as_bytes(), "B2".parse().unwrap(), None).unwrap_err();
        assert!(
            matches!(&error, Error::Unsupported(error) if error == message),
            "{message}: {error:?}"
        );
    }
    let input =
        sheet("<sheetData><row r='2'><c r='B2' t='str'><v>_xD800_</v></c></row></sheetData>");
    assert!(matches!(
        read_cell(input.as_bytes(), "B2".parse().unwrap(), None),
        Err(Error::Unsupported(error))
            if error == "B2: unpaired UTF-16 surrogate in Excel text escape"
    ));
    let input = sheet("<sheetData><row r='2'><c r='B2' t='s'><v>5</v></c></row></sheetData>");
    assert!(matches!(
        read_cell(input.as_bytes(), "B2".parse().unwrap(), Some(&SharedStrings::from_iter(["only"]))),
        Err(Error::InvalidWorkbook(error)) if error == "B2: shared-string index 5 is out of bounds"
    ));
    // A failure to load the table concerns that part, not the cell.
    let loaded = prepared_worksheet::read_cells(
        input.as_bytes(),
        &["B2".parse().unwrap()],
        || Err(Error::Xml("shared strings".into())),
        /* date1904: */ false,
    );
    assert!(matches!(
        loaded,
        Err(ReadError::SharedStrings(Error::Xml(error))) if error == "shared strings"
    ));
    // So does a string the table cannot decode, which only cells that use it
    // fail to read. The table's loader names its part.
    let strings = read::read_shared_strings(
        format!("<sst xmlns='{TRANSITIONAL}'><si><t>Name</t></si><si><t>_xD83D_</t></si></sst>")
            .as_bytes(),
    )
    .unwrap();
    let input = sheet(
        "<sheetData><row r='2'><c r='A2' t='s'><v>0</v></c><c r='B2' t='s'><v>1</v></c></row></sheetData>",
    );
    let read = |address: &str| {
        prepared_worksheet::read_cells(
            input.as_bytes(),
            &[address.parse().unwrap()],
            || Ok(Some(&strings)),
            /* date1904: */ false,
        )
        .map(|mut cells| cells.remove(0).value)
    };
    assert_eq!(read("A2").unwrap(), CellValue::from("Name"));
    assert!(matches!(
        read("B2"),
        Err(ReadError::UndecodableString {
            index: 1,
            error: Error::Unsupported(error),
        }) if error == "unpaired UTF-16 surrogate in Excel text escape"
    ));
    // A cell's own problems are the worksheet's, even when they concern the
    // table: here, its absence.
    let loaded = prepared_worksheet::read_cells(
        input.as_bytes(),
        &["B2".parse().unwrap()],
        || Ok(None),
        /* date1904: */ false,
    );
    assert!(matches!(
        loaded,
        Err(ReadError::Worksheet(Error::InvalidWorkbook(error)))
            if error == "B2: shared-string cell, but the workbook has no shared-string table"
    ));
}

#[test]
fn shared_strings_stream_without_changing_error_precedence() {
    let table = |body: &str| format!("<sst xmlns='{TRANSITIONAL}' xmlns:x='urn:x'>{body}</sst>");
    let strings: Vec<String> = (0..500).map(|index| format!("s{index}_x0041_")).collect();
    let body: String = strings
        .iter()
        .enumerate()
        .map(|(index, text)| {
            if index % 3 == 0 {
                format!("<si><r><rPr><b/></rPr><t>{text}</t></r><rPh><t>p</t></rPh></si><x:e/>")
            } else {
                format!("<si><t>{text}</t></si>")
            }
        })
        .collect();
    let expected: Vec<String> = strings
        .iter()
        .map(|text| text.replace("_x0041_", "A"))
        .collect();
    assert_eq!(
        read::read_shared_strings(table(&body).as_bytes()).unwrap(),
        expected
    );
    let undecodable = "<si><t>_xD800_</t></si>";
    let unsupported = "<si><t><x:e/></t></si>";
    let hidden = "shared string outside the shared-string table";
    for (body, expected) in [
        // A string hidden anywhere outranks a decoding error before it.
        (format!("{undecodable}<x:e><si/></x:e>"), hidden),
        (format!("{unsupported}<si><x:e><si/></x:e></si>"), hidden),
        // A malformed or unsupported document outranks both.
        (format!("{unsupported}<si><t>x</t></sx>"), "XML error"),
        (
            format!("{unsupported}<x:e><si/></x:e><!DOCTYPE x>"),
            "document type",
        ),
    ] {
        let error = read::read_shared_strings(table(&body).as_bytes()).unwrap_err();
        assert!(error.to_string().contains(expected), "{body}: {error:?}");
    }
    // Otherwise strings that cannot be decoded fail only their own reads,
    // each with its own error, and later strings keep their indexes.
    let surrogate = "unpaired UTF-16 surrogate in Excel text escape";
    let elements = "elements inside a simple cell text value";
    for (body, errors) in [
        (
            format!("{undecodable}{unsupported}<si><t>after</t></si>"),
            [surrogate, elements],
        ),
        (
            format!("{unsupported}{undecodable}<si><t>after</t></si>"),
            [elements, surrogate],
        ),
    ] {
        let strings = read::read_shared_strings(table(&body).as_bytes()).unwrap();
        for (index, expected) in errors.into_iter().enumerate() {
            assert!(
                matches!(
                    strings.get(index),
                    Some(Err(Error::Unsupported(message))) if message == expected
                ),
                "{body}: {index}"
            );
        }
        assert!(matches!(strings.get(2), Some(Ok("after"))), "{body}");
        assert!(strings.get(3).is_none(), "{body}");
    }
    // Each distinct error is kept once, however many entries share it.
    let many = format!("{undecodable}{unsupported}").repeat(1_000);
    let strings = read::read_shared_strings(table(&many).as_bytes()).unwrap();
    assert_eq!(strings.error_count(), 2);
    assert!(matches!(
        strings.get(1_999),
        Some(Err(Error::Unsupported(message))) if message == elements
    ));
    let wrong_root =
        format!("<worksheet xmlns='{TRANSITIONAL}' xmlns:x='urn:x'>{unsupported}</worksheet>");
    assert!(matches!(
        read::read_shared_strings(wrong_root.as_bytes()),
        Err(Error::InvalidWorkbook(error)) if error == "expected an OOXML shared-string table"
    ));
}

#[test]
fn row_spans_preserve_unsorted_overlapping_intervals_until_a_gap_is_filled() {
    let spans = "8:10&#9;2:4&#10;3:7&#13;12:12";
    let input = sheet(&format!(
        "<sheetData><row r='1' spans='{spans}'/></sheetData>"
    ));
    let covered = batch(
        &input,
        &[
            ("B1", 1.0.into()),
            ("E1", 2.0.into()),
            ("J1", 3.0.into()),
            ("L1", 4.0.into()),
        ],
    )
    .unwrap();
    assert!(covered.contains(&format!("spans='{spans}'")));
    let gap = edit(&covered, "K1", 5.0.into()).unwrap();
    assert!(gap.contains("spans='2:12'"));
    let widened = batch(&covered, &[("A1", 6.0.into()), ("M1", 7.0.into())]).unwrap();
    assert!(widened.contains("spans='1:13'"));
}

#[test]
fn fragmented_row_spans_support_large_batches() {
    let spans = (1..2048)
        .rev()
        .filter(|column| column % 2 == 1)
        .map(|column| format!("{column}:{column}"))
        .collect::<Vec<_>>()
        .join(" ");
    let input = sheet(&format!(
        "<sheetData><row r='1' spans='{spans}'/></sheetData>"
    ));
    for parity in [0, 1] {
        let changes: BTreeMap<CellRef, CellValue> = (1..=2048)
            .filter(|column| column % 2 == parity)
            .map(|column| (CellRef { row: 1, column }, CellValue::Number(1.0)))
            .collect();
        let output = patch_cells(
            input.as_bytes(),
            &changes,
            || None,
            /* date1904: */ false,
        )
        .unwrap();
        let expected = if parity == 0 { "1:2048" } else { &spans };
        assert!(
            str::from_utf8(&output)
                .unwrap()
                .contains(&format!("spans='{expected}'"))
        );
        assert_eq!(
            PreparedWorksheet::parse(&output).unwrap().cells().len(),
            1024
        );
    }
}

#[test]
fn row_spans_reject_non_xml_whitespace() {
    for separator in ['\u{85}', '\u{a0}', '\u{2000}'] {
        let input = sheet(&format!(
            "<sheetData><row r='1' spans='1:1{separator}3:3'/></sheetData>"
        ));
        assert!(matches!(
            edit(&input, "B1", 1.0.into()),
            Err(Error::Unsupported(message)) if message == "row 1: invalid row spans"
        ));
    }
}

#[test]
fn prepared_worksheets_read_their_cells_as_they_were_before_a_patch() {
    let input = sheet(
        "<dimension ref='A2:B2'/><sheetData><row r='2'><c r='A2' t='s'><v>0</v></c><c r='B2'><v>5</v></c></row></sheetData>",
    );
    let strings = SharedStrings::from_iter(["Name"]);
    let cells = |worksheet: &PreparedWorksheet<'_>| {
        let addresses: [CellRef; 3] = ["A1", "A2", "B2"].map(|address| address.parse().unwrap());
        worksheet
            .read_cells(
                &addresses,
                || Ok(Some(&strings)),
                /* date1904: */ false,
            )
            .unwrap()
            .into_iter()
            .map(|content| content.value)
            .collect::<Vec<_>>()
    };
    let worksheet = PreparedWorksheet::parse(input.as_bytes()).unwrap();
    // The new row and the widened dimension move every original cell.
    let patched = worksheet
        .patch_cells(
            &BTreeMap::from([
                ("A1".parse().unwrap(), "new".into()),
                ("B2".parse().unwrap(), 8.0.into()),
            ]),
            || Some(&strings),
            /* date1904: */ false,
        )
        .unwrap();
    assert_eq!(
        cells(&worksheet),
        [CellValue::Blank, "Name".into(), 5.0.into()]
    );
    assert_eq!(
        cells(&PreparedWorksheet::parse(&patched).unwrap()),
        ["new".into(), "Name".into(), 8.0.into()]
    );
}

#[test]
fn shared_string_tables_are_parsed_one_entry_at_a_time() {
    // Each entry holds a normalized attribute value, and so does the root.
    let entries = "<si><r><t a='&#49;'>a</t></r></si>".repeat(1000);
    let input = format!("<sst xmlns='{TRANSITIONAL}' a='&#9;'>{entries}</sst>");
    read::SHARED_STRING_PEAKS.set(ArenaPeaks::default());
    let strings = read::read_shared_strings(input.as_bytes()).unwrap();
    assert_eq!(strings.get(999).unwrap().unwrap(), "a");
    // The root, then one entry's si, r and t elements and t's value.
    assert_eq!(
        read::SHARED_STRING_PEAKS.get(),
        ArenaPeaks {
            elements: 4,
            values: 2
        }
    );
}
