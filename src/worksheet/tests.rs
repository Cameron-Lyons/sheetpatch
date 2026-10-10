use super::index::Range;
use super::parse::{XmlDocument, parse};
use super::patch::text_escaped;
use super::validation::validate_ranges;
use super::*;

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
    let addresses: Vec<_> = ["A1", "C1", "A2", "B3"]
        .iter()
        .map(|address| address.parse().unwrap())
        .collect();
    assert_eq!(
        read_cells(input.as_bytes(), &addresses, || {
            panic!("inline strings should not load shared strings")
        })
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
        read_cells(changed.as_bytes(), &addresses, || {
            panic!("inline strings should not load shared strings")
        })
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
        elements,
        attributes,
        ..
    } = parse(input.as_bytes()).unwrap();
    let cell = elements
        .iter()
        .find(|element| element.is(elements[0].namespace, "c"))
        .unwrap();
    assert_eq!(
        cell.attr(&attributes, "x:flag").unwrap().value,
        "a b c d e\rf\ng\th"
    );
    let changed = edit(&input, "A1", 2.0.into()).unwrap();
    assert!(changed.contains("x:flag=\"a\r\nb\rc\nd\te&#13;f&#10;g&#9;h\""));
}

#[test]
fn rejects_formula_cells_and_formula_followers() {
    for kind in ["shared", "array", "dataTable"] {
        let si = if kind == "shared" { " si=\"0\"" } else { "" };
        let input = sheet(&format!(
            r#"<sheetData><row r="1"><c r="A1"><f t="{kind}"{si} ref="A1:C2">1</f><v>1</v></c></row></sheetData>"#
        ));
        assert!(edit(&input, "A1", CellValue::Blank).is_err());
        assert!(edit(&input, "B2", CellValue::Number(8.0)).is_err());
        assert!(edit(&input, "D2", CellValue::Number(8.0)).is_ok());
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
            Err(Error::Unsupported(_))
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
            Err(Error::Unsupported(_))
        ));
    }
    assert_eq!(
        edit(&input, "A1", CellValue::Error("#VALUE!".into())).unwrap(),
        input
    );
    let output = edit(&input, "B1", CellValue::Number(4.0)).unwrap();
    assert_eq!(output, input.replace("<v>3</v>", "<v>4</v>"));
    let cell_metadata = input.replace(" vm=\"1\"", "");
    assert!(edit(&cell_metadata, "A1", CellValue::from("replacement")).is_ok());
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
    let contents = read_cells(
        output.as_bytes(),
        &[
            "A1".parse().unwrap(),
            "B1".parse().unwrap(),
            "C2".parse().unwrap(),
        ],
        || Err(Error::SharedStringsUnavailable),
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
        text_escaped("_x0041_\r&_Xabcd_"),
        "_x005F_x0041_&#13;&amp;_x005F_Xabcd_"
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
    assert!(edit(&input, "B2", CellValue::Number(2.0)).is_err());
    assert!(edit(&input, "B2", CellValue::Blank).is_err());
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
    ] {
        assert!(
            matches!(edit(&input, "A1", 2.0.into()), Err(Error::Xml(_))),
            "accepted malformed XML: {input}"
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
    let cells = changes
        .iter()
        .map(|(address, value)| (address.parse().unwrap(), value.clone()))
        .collect();
    Ok(String::from_utf8(patch_cells(xml.as_bytes(), &cells)?.into_owned()).unwrap())
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
}

#[test]
fn reads_values_formulas_styles_and_preserves_requested_order() {
    let input = sheet(
        r#"<sheetData><row r="1"><c r="A1" s="7"><f>SUM(B1,C1)&amp;2</f><v>42.5</v></c><c r="B1" t="b"><v>1</v></c><c r="C1" t="inlineStr"><is><r><rPr><b/></rPr><t>rich &amp; </t></r><r><t>text</t></r><rPh sb="0" eb="1"><t>phonetic</t></rPh></is></c><c r="D1" t="s"><v>1</v></c><c r="E1" t="e"><v>#DIV/0!</v></c><c r="F1" t="str"><f>"cached"</f><v>cached</v></c><c r="G1" s="9"/></row></sheetData>"#,
    );
    let addresses: Vec<_> = ["C1", "A1", "D1", "B1", "E1", "F1", "G1", "A2", "C1"]
        .iter()
        .map(|address| address.parse().unwrap())
        .collect();
    let strings = ["first".into(), "second".into()];
    let cells = read_cells(input.as_bytes(), &addresses, || Ok(&strings)).unwrap();
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
fn shared_strings_load_only_for_requested_cells_and_once_per_read() {
    let input = sheet(
        r#"<sheetData><row r="1"><c r="A1"><v>7</v></c><c r="B1" t="s"><v>0</v></c><c r="C1" t="s"><v>1</v></c></row></sheetData>"#,
    );
    let numeric = read_cells(input.as_bytes(), &["A1".parse().unwrap()], || {
        panic!("reading a number should not access shared strings")
    })
    .unwrap();
    assert_eq!(numeric[0].value, CellValue::Number(7.0));

    let strings = ["first".into(), "second".into()];
    let mut loads = 0;
    let contents = read_cells(
        input.as_bytes(),
        &[
            "B1".parse().unwrap(),
            "C1".parse().unwrap(),
            "B1".parse().unwrap(),
        ],
        || {
            loads += 1;
            Ok(&strings)
        },
    )
    .unwrap();
    assert_eq!(loads, 1);
    assert_eq!(
        contents
            .into_iter()
            .map(|cell| cell.value)
            .collect::<Vec<_>>(),
        vec!["first".into(), "second".into(), "first".into()]
    );
}

#[test]
fn reads_shared_rich_strings_excel_escapes_and_xml_newlines() {
    let table = format!(r#"<sst xmlns="{STRICT}"><si><t>first</t></si><si><r><t>_x005F_x0041_</t></r><r><t>_xD83D__xDE00_&#13;<![CDATA[\r\nline\r]]></t></r><rPh sb="0" eb="1"><t>ignore</t></rPh></si><si><r><t>_</t></r><r><t>x0041_</t></r></si></sst>"#).replace("\\r", "\r").replace("\\n", "\n");
    assert_eq!(
        read_shared_strings(table.as_bytes()).unwrap(),
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
            read_shared_strings(table.as_bytes()),
            Err(Error::Unsupported(_))
        ));
        let foreign = format!(
            "<sst xmlns='{namespace}' xmlns:x='urn:custom'><x:si><x:t>extension</x:t></x:si><si><t>first</t></si><si><t>second</t></si></sst>"
        );
        assert_eq!(
            read_shared_strings(foreign.as_bytes()).unwrap(),
            ["first", "second"]
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
        let table =
            format!("<sst xmlns='{TRANSITIONAL}' xmlns:x='urn:custom'><si>{payload}</si></sst>");
        assert!(matches!(
            read_shared_strings(table.as_bytes()),
            Err(Error::Unsupported(_))
        ));
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
        ("d", "2026-01-01"),
    ] {
        let input = sheet(&format!(
            r#"<sheetData><row r="1"><c r="A1" t="{kind}"><v>{payload}</v></c></row></sheetData>"#
        ));
        assert!(read_cell(input.as_bytes(), "A1".parse().unwrap(), Some(&[])).is_err());
    }
    let input = sheet(r#"<sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData>"#);
    assert!(matches!(
        read_cell(input.as_bytes(), "A1".parse().unwrap(), None),
        Err(Error::SharedStringsUnavailable)
    ));
    assert!(matches!(
        read_cell(input.as_bytes(), "A1".parse().unwrap(), Some(&[])),
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
            patch_cells(input.as_bytes(), &changes).unwrap(),
            Cow::Borrowed(_)
        ));
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
        ranges.push(Range {
            first: CellRef {
                row: first_row,
                column: first_column,
            },
            last: CellRef {
                row: first_row + next() % 5,
                column: first_column + (next() % 3) as u16,
            },
        });
    }
    for merged in [false, true] {
        for row in 1..=35 {
            for column in 1..=14 {
                let cell = CellRef { row, column };
                let blocked = ranges
                    .iter()
                    .any(|range| range.contains(cell) && (!merged || cell != range.first));
                assert_eq!(
                    validate_ranges(&BTreeMap::from([(cell, CellValue::Blank)]), &ranges, merged)
                        .is_err(),
                    blocked
                );
                // The same target in a large batch exercises the event
                // sweep. Other cells lie past every generated rectangle.
                let mut batch: BTreeMap<_, _> = (40..=70)
                    .map(|row| (CellRef { row, column: 1 }, CellValue::Blank))
                    .collect();
                batch.insert(cell, CellValue::Blank);
                assert_eq!(validate_ranges(&batch, &ranges, merged).is_err(), blocked);
            }
        }
    }
    let boundary = Range {
        first: CellRef {
            row: 1,
            column: 16_384,
        },
        last: CellRef {
            row: 1_048_576,
            column: 16_384,
        },
    };
    assert!(
        validate_ranges(
            &BTreeMap::from([("XFD1".parse().unwrap(), CellValue::Blank)]),
            &[boundary],
            true
        )
        .is_ok()
    );
    assert!(
        validate_ranges(
            &BTreeMap::from([("XFD1048576".parse().unwrap(), CellValue::Blank)]),
            &[boundary],
            true
        )
        .is_err()
    );
}

#[test]
fn range_envelope_includes_interior_edits_and_keeps_overlapping_anchor_guards() {
    let mut ranges = vec![
        Range {
            first: "AF1".parse().unwrap(),
            last: "AG30".parse().unwrap(),
        };
        40
    ];
    ranges.push(Range {
        first: "B15".parse().unwrap(),
        last: "F15".parse().unwrap(),
    });
    let mut cells: BTreeMap<_, _> = (1..=10)
        .chain([30])
        .map(|row| (CellRef::new(row, 26).unwrap(), CellValue::Blank))
        .collect();
    cells.insert("C15".parse().unwrap(), CellValue::Blank);
    for merged in [false, true] {
        let error = validate_ranges(&cells, &ranges, merged).unwrap_err();
        assert!(error.to_string().contains("C15"));
    }
    cells.remove(&"C15".parse().unwrap());
    cells.insert("B15".parse().unwrap(), CellValue::Blank);
    assert!(validate_ranges(&cells, &ranges, true).is_ok());
    assert!(validate_ranges(&cells, &ranges, false).is_err());
    ranges.push(Range {
        first: "A15".parse().unwrap(),
        last: "D15".parse().unwrap(),
    });
    let error = validate_ranges(&cells, &ranges, true).unwrap_err();
    assert!(error.to_string().contains("B15"));
}
