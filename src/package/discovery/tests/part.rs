use super::*;
use crate::package::discovery::namespaces::{
    CONTENT_TYPES_NAMESPACE, MAIN_NAMESPACE, PACKAGE_NAMESPACE,
};
use crate::package::discovery::relationships::{self, Relationship};
use crate::package::discovery::workbook;
use crate::worksheet;

// Relationship elements wherever they occur, so that misplaced ones can
// be rejected.
fn relationship_node(_: usize, namespace: &str, local: &str, _: Option<&Node>) -> bool {
    namespace == PACKAGE_NAMESPACE && local == "Relationship"
}

// Parts both parsers refuse.
const MALFORMED_XML: &[&str] = &[
    "<Relationships>",
    "<a/><b/>",
    "<!DOCTYPE a><a/>",
    "<a/><p:b/>",
    "text<a/>",
    "<a><?xml version='1.0'?></a>",
    "<a>&unknown;</a>",
    "<a>&#0;</a>",
    "<a x='&#0;'/>",
    "<a 0bad='one'/>",
    "<a x='<'/>",
    "<a x='one'y='two'/>",
    "<a><0bad/></a>",
    "\u{a0}<a/>",
    "<?0bad instruction?><a/>",
    "<?XML version='1.0'?><a/>",
    " <?xml version='1.0'?><a/>",
    "<!--comment--><?xml version='1.0'?><a/>",
    "<?xml version='1.0' standalone='invalid'?><a/>",
    "<?xml version='1.0' version='1.0'?><a/>",
    "<?xml version='1.0' ignored='one'?><a/>",
    "<?xml version='1.0' standalone='yes' encoding='UTF-8'?><a/>",
    "<?xml version='1.0'encoding='UTF-8'?><a/>",
    // Elements that are not retained are validated all the same.
    "<a xmlns:p='urn:u' xmlns:q='urn:u'><b><c p:x='1' q:x='2'/></b></a>",
    "<a><b><c q:x='1'/></b></a>",
    "<a><b>&#1;</b></a>",
    "\u{feff}\u{feff}<a/>",
    "<a>]]></a>",
    "<a/>&amp;",
    "<a/><![CDATA[x]]>",
    "<a x='&#1;'/>",
    "<a>\u{1}</a>",
    "<a></b>",
];

#[test]
fn malformed_xml_has_no_partial_document() {
    for xml in MALFORMED_XML.iter().chain(ROOTLESS_XML) {
        assert!(xml_nodes(xml.as_bytes(), root_only).is_err(), "{xml}");
    }
}

// Parts that end before any root element starts.
const ROOTLESS_XML: &[&str] = &[
    "",
    " ",
    "<!--comment-->",
    "<?xml version='1.0'?>",
    "<?target data?>",
    "\u{feff}",
];

#[test]
fn parts_without_a_root_element_are_incomplete() {
    for xml in ROOTLESS_XML {
        for result in [
            xml_nodes(xml.as_bytes(), root_only).map(drop),
            worksheet::parse_xml(xml.as_bytes()),
        ] {
            assert!(
                matches!(&result, Err(Error::Xml(message)) if message == "incomplete XML document"),
                "{xml:?}: {result:?}"
            );
        }
    }
}

// Namespace constraints apply to normalized values, with the rules of
// Namespaces in XML 1.0.
const NAMESPACE_VIOLATIONS: &[&str] = &[
    "<tag xmlns:p=''/>",
    "<tag xmlns:p='http://www.w3.org/XML/1998/namespac&#101;'/>",
    "<tag xmlns:p='http://www.w3.org/2000/xmlns&#47;'/>",
    "<tag xmlns='http://www.w3.org/XML/1998/namespac&#101;'/>",
    "<tag xmlns='http://www.w3.org/2000/xmlns&#47;'/>",
    "<tag xmlns:xml='urn:incorrect'/>",
    "<tag xmlns:xmlns='urn:incorrect'/>",
    "<xmlns:tag/>",
];
const NAMESPACE_DECLARATIONS: &[&str] = &[
    "<tag xmlns=''/>",
    "<tag xmlns:p='urn&#58;correct'/>",
    "<tag xmlns:xml='http://www.w3.org/XML/1998/namespac&#101;'/>",
];

#[test]
fn namespace_constraints_use_decoded_values_and_xml_10_rules() {
    for xml in NAMESPACE_VIOLATIONS {
        assert!(
            matches!(xml_nodes(xml.as_bytes(), root_only), Err(Error::Xml(_))),
            "{xml}"
        );
    }
    for xml in NAMESPACE_DECLARATIONS {
        xml_nodes(xml.as_bytes(), root_only).unwrap();
    }
}

fn nested(depth: usize) -> String {
    format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth))
}

// Parts refused with the variant the worksheet parser reports.
fn variant_cases() -> [(Vec<u8>, Error); 5] {
    [
        (nested(257).into_bytes(), Error::Unsupported(String::new())),
        (b"<a>\xff</a>".to_vec(), Error::Xml(String::new())),
        (
            "\u{feff}\u{feff}<a/>".as_bytes().to_vec(),
            Error::Xml(String::new()),
        ),
        (
            b"<?xml version='1.0' encoding='UTF-16'?><a/>".to_vec(),
            Error::Unsupported(String::new()),
        ),
        (
            b"<!DOCTYPE a><a/>".to_vec(),
            Error::Unsupported(String::new()),
        ),
    ]
}

#[test]
fn package_xml_errors_use_the_worksheet_parser_variants() {
    xml_nodes(nested(256).as_bytes(), root_only).unwrap();
    for (xml, expected) in variant_cases() {
        let error = xml_nodes(&xml, root_only).err().unwrap();
        assert_eq!(
            mem::discriminant(&error),
            mem::discriminant(&expected),
            "{error}"
        );
    }
}

// As in worksheets, whatever entities quick-xml's features recognize.
#[test]
fn package_xml_resolves_only_xml_10_entities() {
    let nodes = xml_nodes(b"<a x='&amp;&#160;&quot;'>&lt;&#xA0;</a>", root_only).unwrap();
    assert_eq!(nodes[0].attribute("", "x"), Some("&\u{a0}\""));
    for (xml, entity) in [
        ("<a x='&nbsp;'/>", "nbsp"),
        ("<a><b x='&nbsp;'/></a>", "nbsp"),
        ("<a xmlns:p='urn:&nbsp;'/>", "nbsp"),
        (
            "<a xmlns:xml='http:&sol;&sol;www.w3.org/XML/1998/namespace'/>",
            "sol",
        ),
        ("<a>&nbsp;</a>", "nbsp"),
    ] {
        let error = xml_nodes(xml.as_bytes(), root_only).err();
        assert!(
            matches!(&error, Some(Error::Xml(message)) if message.contains(entity)),
            "{xml}: {error:?}"
        );
    }
    assert!(matches!(
        xml_nodes(b"<a>&nbsp;</a>", root_only),
        Err(Error::Xml(message)) if message == "undefined XML entity &nbsp;"
    ));
}

#[test]
fn discovery_retains_only_the_elements_it_needs() {
    // Unrelated markup costs no retained memory, wherever it occurs.
    let padding = "<a/><x:sheet x:id='1'/><b><Relationship/></b>".repeat(5_000);
    let types =
        format!("<Types xmlns='{CONTENT_TYPES_NAMESPACE}' xmlns:x='urn:x'>{padding}</Types>");
    assert_eq!(xml_nodes(types.as_bytes(), root_only).unwrap().len(), 1);

    let relationships_xml = format!(
        "<Relationships xmlns='{PACKAGE_NAMESPACE}' xmlns:x='urn:x'>{padding}<Relationship Id='a' Type='t' Target='b'/>{padding}</Relationships>"
    )
    .replace("<b><Relationship/></b>", "<b/>");
    let nodes = xml_nodes(relationships_xml.as_bytes(), relationship_node).unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[1].attribute("", "Target"), Some("b"));
    let expected = Relationship {
        id: "a".into(),
        kind: "t".into(),
        target: "b".into(),
        external: false,
    };
    assert_eq!(
        relationships::parse(relationships_xml.as_bytes(), &|_, _| true)
            .map_err(|error| format!("{error:?}")),
        Ok(vec![expected])
    );
    // Relationships that the caller does not select are not kept at all.
    assert!(
        relationships::parse(relationships_xml.as_bytes(), &|_, _| false)
            .unwrap()
            .is_empty()
    );

    let workbook = format!(
        "<workbook xmlns='{MAIN_NAMESPACE}' xmlns:x='urn:x'>{padding}<workbookPr date1904='1'/><sheets>{padding}<sheet name='a'/></sheets><extLst><workbookPr/>{padding}<ext><sheet name='b'/></ext></extLst></workbook>"
    );
    let nodes = xml_nodes(workbook.as_bytes(), workbook::workbook_node).unwrap();
    let retained: Vec<_> = nodes
        .iter()
        .map(|node| (node.local.as_str(), node.parent))
        .collect();
    // A misplaced sheet is kept, without its unretained parent, so that
    // discovery can reject it.
    assert_eq!(
        retained,
        [
            ("workbook", None),
            ("workbookPr", Some(0)),
            ("sheets", Some(0)),
            ("sheet", Some(2)),
            ("sheet", None),
        ]
    );
}

// Parts both parsers accept.
const WELL_FORMED_XML: &[&str] = &[
    "\u{feff}<?xml version='1.0' encoding='UTF-8' standalone='yes'?><a/>",
    "<?xml version='1.0' standalone='no'?><a/>",
    "<?xml-stylesheet href='style.xsl'?><a xmlns:雪='urn:vendor' 雪:属性='value'/>",
];

#[test]
fn complete_xml_supports_bom_declarations_and_unicode_names() {
    for xml in WELL_FORMED_XML {
        assert!(xml_nodes(xml.as_bytes(), root_only).is_ok(), "{xml}");
    }
}

// The worksheet tests' XML cases, as worksheet bodies.
const WORKSHEET_BODIES: &[&str] = &[
    "<sheetData 0bad='one'/>",
    "<sheetData><0bad/></sheetData>",
    "<sheetData><x:bad:name/></sheetData>",
    "<sheetData><?0bad data?></sheetData>",
    "<sheetData><?XML data?></sheetData>",
    "<sheetData/><x:a>]]></x:a>",
    "<sheetData/><x:a v='a<b'/>",
    "<sheetData/><x:a v='&#1;'/>",
    "<sheetData/><x:a>\u{1}</x:a>",
    "<sheetData xmlns:p='' />",
    "<sheetData xmlns:p='http://www.w3.org/XML/1998/namespac&#101;' />",
    "<sheetData xmlns:p='http://www.w3.org/2000/xmlns&#47;' />",
    "<sheetData/><x:foreign xmlns='http://www.w3.org/XML/1998/namespac&#101;'/>",
    "<sheetData/><x:foreign xmlns='http://www.w3.org/2000/xmlns&#47;'/>",
    "<sheetData/><xmlns:invalid/>",
    "<sheetData xmlns:p='urn:&nbsp;'/>",
    "<sheetData xmlns:xml='http:&sol;&sol;www.w3.org/XML/1998/namespace'/>",
    "<sheetData><row r='1'><c r='A1' t='inlineStr'><is><t>a&nbsp;b</t></is></c></row></sheetData>",
    "<sheetData><row r='1'><c r='A1' x:flag='&amp;&#160;'/></row></sheetData>",
    "<sheetData><row r='1'><c r='A1' t='n' t='b'/></row></sheetData>",
    "<sheetData><row r='1'><c r='A1't='n'/></row></sheetData>",
    "<sheetData><row r='1'><c r='A1'><v>&unknown;</v></c></row></sheetData>",
    "<sheetData><row r='1'><c r='A1' xmlns:a='urn:x' xmlns:b='urn&#58;x' a:flag='one' b:flag='two'/></row></sheetData>",
    "<sheetData><row r='1'><c r='A1' xmlns:a='urn:x' xmlns:b='urn&#58;x' a:first='one' a:flag='two' b:flag='three'/></row></sheetData>",
    "<sheetData><row r='1'><c r='A1' missing:flag='one'/></row></sheetData>",
    "<sheetData/><x:t /xml:space='preserve'>text</x:t>",
    "<sheetData/><x:t xml:space='preserve'/ >text</x:t>",
    "<sheetData/><x:t xml:space='preserve' / >text</x:t>",
    "<sheetData/><x:t xml:space='preserve'/x:a='1'>text</x:t>",
    "<sheetData/><x:t xml:space='preserve' /x:a='1'/>",
    "<sheetData/><x:t xml:space='preserve'x:a='1'/>",
    "<sheetData/><x:t x:a='1' x:a='2'/>",
    "<sheetData/><x:t x:a=1/>",
    "<sheetData/><x:t x:a/>",
    "<sheetData/><x:t xml:space='preserve' />",
    "<sheetData/><x:t\txml:space = 'preserve'\r\n/>",
    "<sheetData/><x:t x:a='1/>'>text</x:t>",
    "<sheetData/><x:t\n>text</x:t\n>",
    "<?vendor:tool untouched?><sheetData/><x:外 x:À·='keep'/><x:\u{10000} x:\u{200c}start='keep'/>",
    "<sheetData/><sheetData/>",
];

// Declarations in place of the worksheet's `<?xml version="1.0"?>`.
const WORKSHEET_DECLARATIONS: &[&str] = &[
    " <?xml version='1.0'?>",
    "<!--first--><?xml version='1.0'?>",
    "<?xml version='1.0' version='1.0'?>",
    "<?xml version='1.0' standalone='maybe'?>",
    "<?xml version='1.0' standalone='yes' encoding='UTF-8'?>",
    "<?xml version='1.0' unknown='value'?>",
    "<?xml version='1.0' encoding='UTF-16'?>",
    "<?xml version='1.1'?>",
    "<?xml version='2.0'?>",
    "<?xml version='1.0' encoding='utf-8' standalone='yes'?>\r\n",
    "\u{feff}<?xml version='1.0'?>",
    "\u{feff}\u{feff}<?xml version='1.0'?>",
];

// The review's differential probe of attribute, declaration, namespace,
// reference and markup edge cases.
const EDGE_CASES: &[&str] = &[
    "<a b = 'x'/>",
    "<a xmlns:xml='http://www.w3.org/XML/1998/namespace'/>",
    "<p:a xmlns:p='u'><p:b xmlns:p=''/></p:a>",
    "<a x='&#x20;'/>",
    "<a\tb='1'/>",
    "<a b='1'\n/>",
    "<a b='&lt;'/>",
    "<a b=\"'\"/>",
    "<a:b/>",
    "<a xml:lang='en'/>",
    "<a xmlns:a='urn:x' a:xmlns='1'/>",
    "<a b='1' b='2'/>",
    "<a><!-- a -- b --></a>",
    "<a><![CDATA[x]]></a>",
    "<a>]]&gt;</a>",
    "<a>\r\n</a>",
    "<?xml-stylesheet?><a/>",
    "<?xmlfoo?><a/>",
    "<a/><?xml version='1.0'?>",
    "<a/><!-- c -->",
    "<a/><?pi?>",
    "<a/>\n",
    "<a b='x'/c='y'/>",
    "<a b='x' / >",
    "<a / >",
    "<a b='x'/ >",
    "<a =\"x\"/>",
    "<a xmlns:p='urn:a' xmlns:q='urn:%61'><b p:x='1' q:x='2'/></a>",
    "<a xmlns:p='urn:a' xmlns:q='urn:&#97;'><b p:x='1' q:x='2'/></a>",
    "<a xmlns:p='&#x20;urn:a' xmlns:q='urn:a'><b p:x='1' q:x='2'/></a>",
    "<a xmlns:p=' urn:a' xmlns:q='urn:a'><b p:x='1' q:x='2'/></a>",
    "<a xmlns:p='urn:a'><b xmlns:p='urn:b' p:x='1'/></a>",
    "<a xmlns='urn:a' xmlns=''/>",
    "<a xmlns:p='urn:a' p:p='1' xmlns:p='urn:b'/>",
    "<a xmlns='&#0;'/>",
    "<a xmlns='&#1;'/>",
    "<a>&#xD800;</a>",
    "<a>&#65536;</a>",
    "<a>&#x110000;</a>",
    "<a>&amp;&lt;&gt;&quot;&apos;</a>",
    "<a b='&unknown;'/>",
    "<a b='&#38;'/>",
    "<a b='&#38;#0;'/>",
    "<a b='&#60;'/>",
    "<a>&#60;</a>",
    "<a><b/></a >",
    "<a ><b/></a>",
    "< a/>",
    "<a></ a>",
    "<a>\u{fffe}</a>",
    "<a b='\u{fffe}'/>",
    "<a\u{a0}b='1'/>",
    "<a b\u{a0}='1'/>",
    "<?xml  version='1.0'  ?><a/>",
    "<?xml\tversion='1.0'?><a/>",
    "<?xml version = '1.0'?><a/>",
    "\u{feff} <a/>",
    "<a><?xml-x?></a>",
    "<a><?x y?></a>",
    "<a><!---></a>",
    "<a><!-- --->",
    "<a><!-- - --></a>",
    "<a xmlns:b='urn:b'><b:c xmlns:b='urn:c'/><b:c/></a>",
    "<a>x<!DOCTYPE a></a>",
    "<a><![CDATA[]]]]><![CDATA[>]]></a>",
    "<a>]]]]></a>",
    "<a>]]&#62;</a>",
    "<a>&#93;]></a>",
    "<a b='>'/>",
    "<a b=\"/>\" />",
    "<a b='&#9;&#10;&#13;'/>",
    "<a b='\t\n\r'/>",
    "<a xmlns:a='urn:a' xmlns:A='urn:a' a:x='1' A:x='2'/>",
    "<a xmlns:a='http://www.w3.org/XML/1998/namespace'/>",
    "<a xml:x='1' xmlns:p='http://www.w3.org/XML/1998/namespace' />",
    "<a xmlns=' http://www.w3.org/2000/xmlns/'/>",
    "<a:b xmlns:a='urn:a'></a:b>",
    "<a:b xmlns:a='urn:a'></b>",
    "<a>\u{85}\u{2028}</a>",
    "<a/>\u{feff}",
    "<a>\u{feff}</a>",
    "<a b='1' c='2' d='3' e='4' f='5' g='6' h='7' i='8' j='9' k='10' l='11' m='12' n='13' o='14' p='15' q='16' r='17' b='18'/>",
    "<a xmlns:x='urn:x' xmlns:y='urn:x' x:a1='' x:a2='' x:a3='' x:a4='' x:a5='' x:a6='' x:a7='' x:a8='' x:a9='' x:a10='' x:a11='' x:a12='' x:a13='' x:a14='' x:a15='' x:a16='' x:a17='' y:a1=''/>",
];

// Package parts and worksheets share one validating parser core, so every
// part gets the same result, variant and message from either parser.
#[test]
fn package_and_worksheet_parsers_agree_on_every_case() {
    let worksheet = |body: &str| {
        format!(
            "<?xml version=\"1.0\"?><worksheet xmlns=\"{MAIN_NAMESPACE}\" xmlns:x=\"urn:custom\">{body}</worksheet>"
        )
    };
    let sheet = worksheet("<sheetData/>");
    let mut inputs: Vec<Vec<u8>> = [
        MALFORMED_XML,
        ROOTLESS_XML,
        WELL_FORMED_XML,
        NAMESPACE_VIOLATIONS,
        NAMESPACE_DECLARATIONS,
        EDGE_CASES,
    ]
    .concat()
    .iter()
    .map(|xml| xml.as_bytes().to_vec())
    .collect();
    inputs.extend(variant_cases().map(|(xml, _)| xml));
    inputs.push(nested(256).into_bytes());
    inputs.extend(
        WORKSHEET_BODIES
            .iter()
            .map(|body| worksheet(body).into_bytes()),
    );
    inputs.extend(WORKSHEET_DECLARATIONS.iter().map(|declaration| {
        sheet
            .replace("<?xml version=\"1.0\"?>", declaration)
            .into_bytes()
    }));
    for (prefix, suffix) in [
        ("\u{a0}", ""),
        ("", "\u{a0}"),
        ("", "<worksheet/>"),
        ("", "&amp;"),
        ("", "<![CDATA[x]]>"),
    ] {
        inputs.push(format!("{prefix}{sheet}{suffix}").into_bytes());
    }
    let deep = format!("{}{}", "<x:a>".repeat(256), "</x:a>".repeat(256));
    inputs.push(worksheet(&deep).into_bytes());
    let mut invalid = sheet.clone().into_bytes();
    invalid.insert(invalid.len() - 2, 0xff);
    inputs.push(invalid);
    for xml in &inputs {
        let from_package = xml_nodes(xml, root_only).map(drop);
        let from_worksheet = worksheet::parse_xml(xml);
        assert_eq!(
            from_package.map_err(|error| format!("{error:?}")),
            from_worksheet.map_err(|error| format!("{error:?}")),
            "{}",
            String::from_utf8_lossy(xml)
        );
    }
}

#[test]
fn duplicate_checks_of_long_attribute_lists_start_over_for_each_tag() {
    // Past 16 attributes the checks use sets, reused across tags. Each
    // later tag's 17th attribute repeats an earlier tag's name, plain or
    // expanded, which is no duplicate.
    let attributes = |name: &str, count: usize| -> String {
        (1..=count).map(|n| format!(" {name}{n}='1'")).collect()
    };
    let xml = format!(
        "<r xmlns:p='urn:p'><a{}/><b{} a1='1'/><c{}/><d{} p:a1='1'/></r>",
        attributes("a", 17),
        attributes("b", 16),
        attributes("p:a", 17),
        attributes("p:b", 16),
    );
    xml_nodes(xml.as_bytes(), root_only).unwrap();
    worksheet::parse_xml(xml.as_bytes()).unwrap();
}

#[test]
fn namespace_pool_restarts_keep_every_check_and_retained_namespace() {
    let padding: String = (0..3 * NAMESPACE_LIMIT)
        .map(|n| format!("<x:a xmlns:x='urn:{n}' x:n='{n}'/>"))
        .collect();
    // Retained elements that declare namespaces of their own make the
    // pool start over inside their tags, too.
    let relationships: String = (0..3 * NAMESPACE_LIMIT)
        .map(|n| {
            format!(
                "<p:Relationship xmlns:p='{PACKAGE_NAMESPACE}' xmlns:z='urn:z{n}' z:n='{n}' Id='r{n}' Type='t' Target='{n}'/>"
            )
        })
        .collect();
    let relationships_xml = format!(
        "<Relationships xmlns='{PACKAGE_NAMESPACE}'>{padding}<Relationship Id='a' Type='t' Target='b'/>{relationships}</Relationships>"
    );
    let nodes = xml_nodes(relationships_xml.as_bytes(), relationship_node).unwrap();
    let retained: Vec<_> = nodes
        .iter()
        .map(|node| {
            let vendor = node
                .attributes
                .iter()
                .find(|attribute| attribute.local == "n")
                .map(|attribute| (&*attribute.namespace, attribute.value.as_str()));
            (&*node.namespace, node.attribute("", "Target"), vendor)
        })
        .collect();
    let targets: Vec<_> = (0..3 * NAMESPACE_LIMIT).map(|n| n.to_string()).collect();
    let vendors: Vec<_> = (0..3 * NAMESPACE_LIMIT)
        .map(|n| format!("urn:z{n}"))
        .collect();
    let expected: Vec<_> = [
        (PACKAGE_NAMESPACE, None, None),
        (PACKAGE_NAMESPACE, Some("b"), None),
    ]
    .into_iter()
    .chain((0..3 * NAMESPACE_LIMIT).map(|n| {
        (
            PACKAGE_NAMESPACE,
            Some(targets[n].as_str()),
            Some((vendors[n].as_str(), targets[n].as_str())),
        )
    }))
    .collect();
    assert_eq!(retained, expected);
    let streamed = relationships::parse(relationships_xml.as_bytes(), &|_, _| true).unwrap();
    let streamed: Vec<_> = streamed
        .iter()
        .map(|r| (r.id.as_str(), r.target.as_str()))
        .collect();
    let ids: Vec<_> = (0..3 * NAMESPACE_LIMIT).map(|n| format!("r{n}")).collect();
    let expected: Vec<_> = [("a", "b")]
        .into_iter()
        .chain((0..3 * NAMESPACE_LIMIT).map(|n| (ids[n].as_str(), targets[n].as_str())))
        .collect();
    assert_eq!(streamed, expected);
    // An inherited binding still expands to the same name afterwards.
    let duplicate =
        format!("<a xmlns:p='urn:same'>{padding}<b xmlns:q='urn:same' p:x='1' q:x='2'/></a>");
    assert!(matches!(
        xml_nodes(duplicate.as_bytes(), root_only),
        Err(Error::Xml(message)) if message == "duplicate expanded attribute name"
    ));
    let process_content =
        format!("<a xmlns:m='{COMPATIBILITY_NAMESPACE}'>{padding}<b m:ProcessContent='x:y'/></a>");
    assert!(
        part_xml(process_content.as_bytes(), root_only)
            .unwrap()
            .process_content
    );
}

#[test]
fn package_parsing_holds_one_tag_of_attributes_and_a_bounded_namespace_pool() {
    // Each tag binds a new namespace and has three attributes.
    let padding: String = (0..10 * NAMESPACE_LIMIT)
        .map(|n| format!("<x:a xmlns:x='urn:{n}' b='1' c='2'/>"))
        .collect();
    let xml = format!("<Types xmlns='{CONTENT_TYPES_NAMESPACE}'>{padding}</Types>");
    ATTRIBUTE_PEAK.set(0);
    SPELLING_PEAK.set(0);
    xml_nodes(xml.as_bytes(), root_only).unwrap();
    assert_eq!(ATTRIBUTE_PEAK.get(), 3);
    // The pool starts over once a tag takes it past the limit.
    assert_eq!(SPELLING_PEAK.get(), NAMESPACE_LIMIT + 1);

    // Each child of a workbook using mc:ProcessContent resolves a new
    // namespace. The limit grows by the two rules in scope.
    let padding: String = (0..10 * NAMESPACE_LIMIT)
        .map(|n| format!("<a xmlns='urn:{n}'/>"))
        .collect();
    let workbook = format!(
        "<workbook xmlns='{MAIN_NAMESPACE}' xmlns:mc='{COMPATIBILITY_NAMESPACE}' xmlns:v='urn:v' mc:Ignorable='v' mc:ProcessContent='v:w'>{padding}</workbook>"
    );
    SPELLING_PEAK.set(0);
    reject_promoted_workbook_properties(workbook.as_bytes()).unwrap();
    assert_eq!(SPELLING_PEAK.get(), NAMESPACE_LIMIT + 2 + 1);
}

#[test]
fn promoted_workbook_properties_are_found_after_the_namespace_pool_restarts() {
    // Each direct child of the workbook resolves a new namespace, and the
    // rules declared on the root must survive every restart.
    let padding: String = (0..3 * NAMESPACE_LIMIT)
        .map(|n| format!("<a xmlns='urn:{n}'/>"))
        .collect();
    let workbook = format!(
        "<workbook xmlns='{MAIN_NAMESPACE}' xmlns:mc='{COMPATIBILITY_NAMESPACE}' xmlns:v='urn:v' mc:Ignorable='v' mc:ProcessContent='v:w'>{padding}<v:w><workbookPr date1904='1'/></v:w></workbook>"
    );
    assert!(matches!(
        reject_promoted_workbook_properties(workbook.as_bytes()),
        Err(Error::Unsupported(message)) if message.contains("ProcessContent")
    ));
    reject_promoted_workbook_properties(workbook.replace("v:w>", "v:x>").as_bytes()).unwrap();
}
