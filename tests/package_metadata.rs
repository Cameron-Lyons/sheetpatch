//! Workbook metadata discovered at open: sheet lookup and visibility,
//! signatures, the date system, and Debug output.
mod common;

use sheetpatch::{CellValue, Error, Sheet, SheetVisibility, Workbook};
use std::collections::{BTreeMap, HashSet};

const MAIN_NAMESPACE: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const STRICT_MAIN_NAMESPACE: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";
const OFFICE_NAMESPACE: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE_NAMESPACE: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const SIGNATURE_ORIGIN: &str =
    "http://schemas.openxmlformats.org/package/2006/relationships/digital-signature/origin";

/// A workbook with the given `<workbook>` children before `sheets`, and the
/// given extra sheet attributes for its worksheets `Data` and `Other`.
fn parts(properties: &str, data: &str, other: &str) -> BTreeMap<String, String> {
    let worksheet = format!(
        "<worksheet xmlns='{MAIN_NAMESPACE}'><sheetData><row r='1'><c r='A1' t='d' s='1'><v>2024-01-15</v></c><c r='B1'><v>45306</v></c><c r='C1' s='2'/></row></sheetData></worksheet>"
    );
    BTreeMap::from([
        (
            "[Content_Types].xml".to_owned(),
            "<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='xml' ContentType='application/xml'/></Types>".to_owned(),
        ),
        (
            "_rels/.rels".into(),
            format!("<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='office' Type='{OFFICE_NAMESPACE}/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ),
        (
            "xl/workbook.xml".into(),
            format!("<workbook xmlns='{MAIN_NAMESPACE}' xmlns:r='{OFFICE_NAMESPACE}'>{properties}<sheets><sheet name='Data' sheetId='1' r:id='one'{data}/><sheet name='Other' sheetId='2' r:id='two'{other}/></sheets></workbook>"),
        ),
        (
            "xl/_rels/workbook.xml.rels".into(),
            format!("<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='one' Type='{OFFICE_NAMESPACE}/worksheet' Target='worksheets/sheet1.xml'/><Relationship Id='two' Type='{OFFICE_NAMESPACE}/worksheet' Target='worksheets/sheet2.xml'/></Relationships>"),
        ),
        ("xl/worksheets/sheet1.xml".into(), worksheet.clone()),
        ("xl/worksheets/sheet2.xml".into(), worksheet),
    ])
}

fn stored(parts: &BTreeMap<String, String>) -> Vec<u8> {
    let entries: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(name, xml)| (name.as_str(), xml.as_bytes()))
        .collect();
    common::stored_archive(&entries)
}

fn archive(properties: &str, data: &str, other: &str) -> Vec<u8> {
    stored(&parts(properties, data, other))
}

#[test]
fn workbook_relationships_resolve_independently_of_legacy_zip_name_spelling() {
    for (workbook_path, rels_path) in [
        ("xl/book%20one.xml", "xl/_rels/book one.xml.rels"),
        ("xl/book one.xml", "xl/_rels/book%20one.xml.rels"),
    ] {
        let mut parts = parts("", "", "");
        let workbook = parts.remove("xl/workbook.xml").unwrap();
        parts.insert(workbook_path.into(), workbook);
        let relationships = parts.remove("xl/_rels/workbook.xml.rels").unwrap();
        parts.insert(rels_path.into(), relationships);
        let roots = parts.get_mut("_rels/.rels").unwrap();
        *roots = roots.replace("xl/workbook.xml", "XL/BOOK%20ONE.XML");
        if rels_path.contains("%20") {
            // The encoded relationship part wins even when the workbook's
            // ZIP name is decoded and a decoded relationship decoy exists.
            parts.insert("xl/_rels/book one.xml.rels".into(), "not XML".into());
        }

        let original = stored(&parts);
        let mut book = Workbook::from_bytes(original.clone()).unwrap();
        assert_eq!(book.sheets().len(), 2);
        assert_eq!(book.to_bytes().unwrap(), original);
        assert_eq!(book.get_cell("Data", "B1").unwrap().value, 45_306.into());
        book.set_cell("Data", "B1", 7).unwrap();
        let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
        assert_eq!(reopened.get_cell("Data", "B1").unwrap().value, 7.into());
    }
}

#[test]
fn iso_date_cells_read_as_serials_in_the_workbook_date_system() {
    for (properties, uses_1904, serial) in [
        ("", false, 45_306.0),
        ("<workbookPr/>", false, 45_306.0),
        ("<workbookPr date1904='false'/>", false, 45_306.0),
        ("<workbookPr date1904=' 0 '/>", false, 45_306.0),
        ("<workbookPr date1904='1'/>", true, 43_844.0),
        ("<workbookPr date1904='true'/>", true, 43_844.0),
        ("<workbookPr date1904='\ttrue\n'/>", true, 43_844.0),
        // Only the workbook root's own child sets the date system.
        (
            "<extension><workbookPr date1904='1'/></extension>",
            false,
            45_306.0,
        ),
        (
            "<workbookPr xmlns='urn:vendor' date1904='1'/>",
            false,
            45_306.0,
        ),
    ] {
        let book = Workbook::from_bytes(archive(properties, "", "")).unwrap();
        assert_eq!(book.uses_1904_dates(), uses_1904, "{properties}");
        let direct = book.get_cell("Data", "A1").unwrap();
        assert_eq!(direct.value, CellValue::Number(serial), "{properties}");
        assert_eq!(direct.style_index, Some(1));
        assert_eq!(
            book.prepare_sheet("Data").unwrap().get_cell("A1").unwrap(),
            direct,
            "{properties}"
        );
        assert_eq!(
            book.get_cells("Data", ["A1", "B1"]).unwrap()[1].value,
            CellValue::Number(45_306.0)
        );
    }
}

#[test]
fn strict_workbooks_report_their_date_system() {
    let mut parts = parts("<workbookPr date1904='1'/>", "", "");
    for xml in parts.values_mut() {
        *xml = xml.replace(MAIN_NAMESPACE, STRICT_MAIN_NAMESPACE);
    }
    let book = Workbook::from_bytes(stored(&parts)).unwrap();
    assert!(book.uses_1904_dates());
    assert_eq!(
        book.get_cell("Other", "A1").unwrap().value,
        CellValue::Number(43_844.0)
    );
}

#[test]
fn mixed_spreadsheetml_metadata_is_not_silently_ignored() {
    for (root, other) in [
        (MAIN_NAMESPACE, STRICT_MAIN_NAMESPACE),
        (STRICT_MAIN_NAMESPACE, MAIN_NAMESPACE),
    ] {
        let mut parts = parts("", "", "");
        let workbook = parts["xl/workbook.xml"].replacen(
            &format!("xmlns='{MAIN_NAMESPACE}'"),
            &format!("xmlns='{root}'"),
            1,
        );
        let properties = format!("<workbookPr xmlns='{other}' date1904='1'/>");
        for mixed in [
            workbook.replace("<sheets>", &format!("{properties}<sheets>")),
            workbook.replace("<sheets>", &format!("<sheets xmlns='{other}'>")),
            workbook.replace(
                "<sheet name='Data'",
                &format!("<sheet xmlns='{other}' name='Data'"),
            ),
            workbook.replace(
                "<sheets>",
                &format!("<workbookPr date1904='0'/>{properties}<sheets>"),
            ),
        ] {
            parts.insert("xl/workbook.xml".into(), mixed.clone());
            let error = Workbook::from_bytes(stored(&parts)).err();
            assert!(
                matches!(&error, Some(Error::Unsupported(message)) if message.contains("strict and transitional")),
                "{mixed}: {error:?}"
            );
        }
    }
}

#[test]
fn invalid_date_systems_are_rejected_at_open() {
    for properties in [
        "<workbookPr date1904='yes'/>",
        "<workbookPr date1904='TRUE'/>",
        "<workbookPr date1904=''/>",
        "<workbookPr date1904='2'/>",
        "<workbookPr/><workbookPr date1904='1'/>",
    ] {
        let error = Workbook::from_bytes(archive(properties, "", "")).err();
        assert!(
            matches!(&error, Some(Error::InvalidWorkbook(message)) if message.contains("workbookPr")),
            "{properties}: {error:?}"
        );
    }
}

#[test]
fn compatibility_branches_cannot_silently_change_the_date_system() {
    let compatibility = "http://schemas.openxmlformats.org/markup-compatibility/2006";
    for (direct, choice, fallback) in [
        (
            "",
            "<workbookPr date1904='0'/>",
            "<workbookPr date1904='1'/>",
        ),
        (
            "",
            "<workbookPr date1904='1'/>",
            "<workbookPr date1904='0'/>",
        ),
        (
            "<workbookPr date1904='0'/>",
            "<workbookPr date1904='1'/>",
            "",
        ),
        (
            "",
            "",
            "<mc:AlternateContent><mc:Choice Requires='x'><workbookPr date1904='0'/></mc:Choice><mc:Fallback><workbookPr date1904='1'/></mc:Fallback></mc:AlternateContent>",
        ),
    ] {
        let properties = format!(
            "{direct}<mc:AlternateContent xmlns:mc='{compatibility}' xmlns:x='urn:vendor'><mc:Choice Requires='x'>{choice}</mc:Choice><mc:Fallback>{fallback}</mc:Fallback></mc:AlternateContent>"
        );
        let error = Workbook::from_bytes(archive(&properties, "", "")).err();
        assert!(
            matches!(&error, Some(Error::Unsupported(message)) if message.contains("workbookPr") && message.contains("date system")),
            "{properties}: {error:?}"
        );
    }

    // Alternate data within ordinary opaque workbook extensions does not
    // substitute workbook properties; unrelated root alternatives are opaque.
    for properties in [
        format!(
            "<extension><mc:AlternateContent xmlns:mc='{compatibility}'><mc:Fallback><workbookPr date1904='1'/></mc:Fallback></mc:AlternateContent></extension>"
        ),
        format!(
            "<mc:AlternateContent xmlns:mc='{compatibility}'><mc:Fallback><extension><workbookPr date1904='1'/></extension></mc:Fallback></mc:AlternateContent>"
        ),
        format!(
            "<mc:AlternateContent xmlns:mc='{compatibility}'><mc:Fallback><bookViews/></mc:Fallback></mc:AlternateContent>"
        ),
    ] {
        let original = archive(&properties, "", "");
        let book = Workbook::from_bytes(original.clone()).unwrap();
        assert!(!book.uses_1904_dates());
        assert_eq!(book.to_bytes().unwrap(), original);
    }
}

#[test]
fn process_content_cannot_promote_hidden_workbook_date_properties() {
    let compatibility = "http://schemas.openxmlformats.org/markup-compatibility/2006";
    for (directives, properties) in [
        (
            "mc:Ignorable='x' mc:ProcessContent='x:wrapper'",
            "<x:wrapper><workbookPr date1904='1'/></x:wrapper>",
        ),
        (
            "",
            "<x:wrapper mc:ProcessContent='x:wrapper' mc:Ignorable='x'><workbookPr date1904='1'/></x:wrapper>",
        ),
        (
            "mc:Ignorable='alias' mc:ProcessContent='alias:wrapper'",
            "<x:wrapper><workbookPr date1904='1'/></x:wrapper>",
        ),
        (
            "mc:Ignorable='x' mc:ProcessContent='x:*'",
            "<x:outer><x:inner><workbookPr date1904='1'/></x:inner></x:outer>",
        ),
        (
            "mc:Ignorable='x' mc:ProcessContent='x:outer'",
            "<x:outer mc:ProcessContent='x:inner'><x:inner><workbookPr date1904='1'/></x:inner></x:outer>",
        ),
        // Prefix rebinding does not change the expanded inherited rule.
        (
            "mc:Ignorable='x' mc:ProcessContent='x:wrapper'",
            "<alias:wrapper xmlns:x='urn:other'><workbookPr date1904='1'/></alias:wrapper>",
        ),
        (
            "mc:Ignorable='x' mc:ProcessContent='x:wrapper'",
            "<mc:AlternateContent><mc:Choice Requires='x'><mc:AlternateContent><mc:Fallback><x:wrapper><workbookPr date1904='1'/></x:wrapper></mc:Fallback></mc:AlternateContent></mc:Choice></mc:AlternateContent>",
        ),
        (
            "mc:Ignorable='x' mc:ProcessContent='x:wrapper'",
            "<x:wrapper><mc:AlternateContent><mc:Fallback><workbookPr date1904='1'/></mc:Fallback></mc:AlternateContent></x:wrapper>",
        ),
        // Repeating an inherited rule in a child must not erase it on exit.
        (
            "mc:Ignorable='x' mc:ProcessContent='x:wrapper'",
            "<x:wrapper mc:Ignorable='x' mc:ProcessContent='x:wrapper'/><x:wrapper><workbookPr date1904='1'/></x:wrapper>",
        ),
        // PreserveElements does not prevent unwrapping under current ECMA
        // markup-compatibility rules, even though some SDKs preserve it.
        (
            "mc:Ignorable='x' mc:ProcessContent='x:*' mc:PreserveElements='alias:wrapper'",
            "<x:wrapper><workbookPr date1904='1'/></x:wrapper>",
        ),
        (
            "mc:Ignorable='x' mc:ProcessContent='x:wrapper' mc:PreserveElements='x:*'",
            "<x:wrapper><workbookPr date1904='1'/></x:wrapper>",
        ),
    ] {
        for main in [MAIN_NAMESPACE, STRICT_MAIN_NAMESPACE] {
            let mut parts = parts(properties, "", "");
            let workbook = parts.get_mut("xl/workbook.xml").unwrap();
            *workbook = workbook.replace(MAIN_NAMESPACE, main).replacen(
                "<workbook ",
                &format!(
                    "<workbook xmlns:mc='{compatibility}' xmlns:x='urn:vend&#111;r' xmlns:alias='urn:vendor' {directives} "
                ),
                1,
            );
            let error = Workbook::from_bytes(stored(&parts)).err();
            assert!(
                matches!(&error, Some(Error::Unsupported(message)) if message.contains("ProcessContent") && message.contains("date system")),
                "{directives}, {properties}, {main}: {error:?}"
            );
        }
    }
}

#[test]
fn process_content_preserves_opaque_paths_and_rule_scopes() {
    let compatibility = "http://schemas.openxmlformats.org/markup-compatibility/2006";
    for (directives, properties) in [
        (
            "mc:Ignorable='x' mc:ProcessContent='x:wrapper'",
            "<x:wrapper><bookViews/></x:wrapper>",
        ),
        (
            "mc:Ignorable='x' mc:ProcessContent='x:other'",
            "<x:wrapper><workbookPr date1904='1'/></x:wrapper>",
        ),
        (
            "mc:ProcessContent='x:wrapper'",
            "<x:wrapper><workbookPr date1904='1'/></x:wrapper>",
        ),
        (
            "mc:Ignorable='x' mc:ProcessContent='x:*'",
            "<extension><x:wrapper><workbookPr date1904='1'/></x:wrapper></extension>",
        ),
        (
            "mc:Ignorable='x' mc:ProcessContent='x:wrapper'",
            "<x:wrapper xmlns:x='urn:other'><workbookPr date1904='1'/></x:wrapper>",
        ),
        (
            "mc:Ignorable='x'",
            "<x:wrapper mc:ProcessContent='x:*'/><x:wrapper><workbookPr date1904='1'/></x:wrapper>",
        ),
        (
            "mc:ProcessContent='x:*'",
            "<x:wrapper mc:Ignorable='x'></x:wrapper><x:wrapper><workbookPr date1904='1'/></x:wrapper>",
        ),
        (
            "",
            "<extension mc:Ignorable='x' mc:ProcessContent='x:*'></extension><x:wrapper><workbookPr date1904='1'/></x:wrapper>",
        ),
        (
            "mc:Ignorable='x' mc:ProcessContent='x:outer'",
            "<x:outer mc:ProcessContent='x:inner'/><x:inner><workbookPr date1904='1'/></x:inner>",
        ),
    ] {
        let mut parts = parts(properties, "", "");
        let workbook = parts.get_mut("xl/workbook.xml").unwrap();
        *workbook = workbook.replacen(
            "<workbook ",
            &format!(
                "<workbook xmlns:mc='{compatibility}' xmlns:x='urn:vendor' xmlns:alias='urn:vendor' {directives} "
            ),
            1,
        );
        let original = stored(&parts);
        let book = Workbook::from_bytes(original.clone())
            .unwrap_or_else(|error| panic!("{directives}, {properties}: {error}"));
        assert!(!book.uses_1904_dates());
        assert_eq!(book.to_bytes().unwrap(), original);
    }
}

#[test]
fn package_metadata_accepts_character_references_in_the_reserved_xml_namespace() {
    let mut parts = parts("<workbookPr date1904='1'/>", "", "");
    for (name, xml) in &mut parts {
        if !name.starts_with("xl/worksheets/") {
            *xml = xml.replacen(
                " xmlns=",
                " xmlns:xml='http://www.w3.org/XML/1998/namespac&#101;' xml:space='preserve' xmlns=",
                1,
            );
        }
    }
    let original = stored(&parts);
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    assert!(book.uses_1904_dates());
    assert_eq!(book.sheets()[0].name(), "Data");
    assert_eq!(book.to_bytes().unwrap(), original);
    book.set_cell("Data", "B1", 7).unwrap();
    let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    assert_eq!(reopened.get_cell("Data", "B1").unwrap().value, 7.into());
}

#[test]
fn namespace_binding_caches_follow_shadowing_and_empty_element_scopes() {
    let properties = "\
        <p:workbookPr p:date1904='false' date1904='true'/>\
        <p:workbookPr xmlns:p='urn:vendor' date1904='false'/>\
        <extension xmlns:p='urn:vendor' xmlns:r='urn:other'>\
          <p:workbookPr date1904='false' r:id='ignored'/>\
          <workbookPr xmlns='' date1904='false'/>\
        </extension>\
        <workbookPr xmlns='' date1904='false'/>";
    let mut parts = parts(properties, " state='hidden'", " state='veryHidden'");
    let workbook = parts.get_mut("xl/workbook.xml").unwrap();
    *workbook = workbook
        .replacen(
            "<workbook ",
            &format!("<workbook xmlns:p='{MAIN_NAMESPACE}' "),
            1,
        )
        .replace("<sheets>", "<p:sheets>")
        .replace("</sheets>", "</p:sheets>")
        .replace("<sheet ", "<p:sheet ");
    parts.insert(
        "xl/_rels/workbook.xml.rels".into(),
        format!(
            "<Relationships xmlns='{PACKAGE_NAMESPACE}' xmlns:p='{PACKAGE_NAMESPACE}'>\
              <p:Relationship Id='one' Type='{OFFICE_NAMESPACE}/worksheet' Target='worksheets/sheet1.xml'/>\
              <p:Relationship xmlns:p='urn:vendor' Id='one' Type='ignored' Target='ignored'/>\
              <extension xmlns:p='urn:vendor'>\
                <p:Relationship Id='one' Type='ignored' Target='ignored'/>\
                <Relationship xmlns='' Id='one' Type='ignored' Target='ignored'/>\
              </extension>\
              <Relationship xmlns='' Id='one' Type='ignored' Target='ignored'/>\
              <p:Relationship Id='two' Type='{OFFICE_NAMESPACE}/worksheet' Target='worksheets/sheet2.xml'/>\
            </Relationships>"
        ),
    );
    let original = stored(&parts);
    let mut book = Workbook::from_bytes(original.clone()).unwrap();
    assert!(book.uses_1904_dates());
    assert_eq!(book.sheets().len(), 2);
    assert_eq!(book.sheets()[0].visibility(), SheetVisibility::Hidden);
    assert_eq!(book.sheets()[1].visibility(), SheetVisibility::VeryHidden);
    assert_eq!(book.to_bytes().unwrap(), original);
    book.set_cell("Other", "B1", 7).unwrap();
    let reopened = Workbook::from_bytes(book.to_bytes().unwrap()).unwrap();
    assert_eq!(reopened.get_cell("Other", "B1").unwrap().value, 7.into());
}

#[test]
fn sheet_visibility_comes_from_the_sheet_state() {
    for (state, visibility) in [
        ("", SheetVisibility::Visible),
        (" state='visible'", SheetVisibility::Visible),
        (" state='hidden'", SheetVisibility::Hidden),
        (" state='veryHidden'", SheetVisibility::VeryHidden),
        (" state=' veryHidden\n'", SheetVisibility::VeryHidden),
    ] {
        let mut book = Workbook::from_bytes(archive("", state, " state='hidden'")).unwrap();
        assert_eq!(book.sheets()[0].visibility(), visibility, "{state}");
        assert_eq!(
            book.sheet("Other").map(Sheet::visibility),
            Some(SheetVisibility::Hidden)
        );
        // Hidden worksheets are read and edited like visible ones.
        book.set_cell("Data", "B1", 7).unwrap();
        assert_eq!(
            book.get_cell("Data", "B1").unwrap().value,
            CellValue::Number(7.0)
        );
    }
    for state in ["Hidden", "very hidden", "", "visible hidden", "2"] {
        let error = Workbook::from_bytes(archive("", &format!(" state='{state}'"), "")).err();
        assert!(
            matches!(&error, Some(Error::InvalidWorkbook(message)) if message.contains("state")),
            "{state}: {error:?}"
        );
    }
}

#[test]
fn sheets_are_found_by_exact_name() {
    let book = Workbook::from_bytes(archive("", "", "")).unwrap();
    assert_eq!(book.sheet("Data"), Some(&book.sheets()[0]));
    assert_eq!(
        book.sheet("Other").map(Sheet::path),
        Some("xl/worksheets/sheet2.xml")
    );
    assert_eq!(book.sheet("data"), None);
    assert_eq!(book.sheet("Missing"), None);
    let distinct: HashSet<&Sheet> = book.sheets().iter().chain(book.sheet("Data")).collect();
    assert_eq!(distinct.len(), 2);
}

#[test]
fn sheets_in_compatibility_branches_are_unsupported_and_misplaced_ones_invalid() {
    let compatibility = "http://schemas.openxmlformats.org/markup-compatibility/2006";
    let data = "<sheet name='Data' sheetId='1' r:id='one'/>";
    let other = "<sheet name='Other' sheetId='2' r:id='two'/>";
    let sheets = format!("<sheets>{data}{other}</sheets>");
    let branches = |choice: &str, fallback: &str| {
        format!(
            "<mc:AlternateContent xmlns:mc='{compatibility}'><mc:Choice Requires='x'>{choice}</mc:Choice><mc:Fallback>{fallback}</mc:Fallback></mc:AlternateContent>"
        )
    };
    let open = |replacement: &str| {
        let mut parts = parts("", "", "");
        let workbook = parts.get_mut("xl/workbook.xml").unwrap();
        assert!(workbook.contains(&sheets));
        *workbook = workbook.replace(&sheets, replacement);
        Workbook::from_bytes(stored(&parts)).err()
    };
    // A reader that picks either branch sees a valid workbook, but which
    // sheets it lists depends on the branch.
    for (replacement, expected) in [
        (
            format!("<sheets>{data}{}</sheets>", branches(other, other)),
            "sheet inside a markup-compatibility branch",
        ),
        (
            format!("<sheets>{}{other}</sheets>", branches(data, "")),
            "sheet inside a markup-compatibility branch",
        ),
        (
            branches(&sheets, &sheets),
            "sheets inside a markup-compatibility branch",
        ),
        (
            format!("{sheets}{}", branches(&sheets, "")),
            "sheets inside a markup-compatibility branch",
        ),
        (
            format!("{sheets}{}", branches(data, "")),
            "sheet inside a markup-compatibility branch",
        ),
    ] {
        let error = open(&replacement);
        assert!(
            matches!(&error, Some(Error::Unsupported(message)) if message == expected),
            "{replacement}: {error:?}"
        );
    }
    // Outside a branch, a misplaced sheet or a second sheets element is
    // invalid whatever a reader selects.
    for (replacement, expected) in [
        (
            format!("<sheets>{data}</sheets>{other}"),
            "sheet outside workbook sheets",
        ),
        (
            format!(
                "<sheets>{data}<mc:AlternateContent xmlns:mc='{compatibility}'>{other}</mc:AlternateContent></sheets>"
            ),
            "sheet outside workbook sheets",
        ),
        (
            format!("<sheets>{data}</sheets><sheets>{other}</sheets>"),
            "workbook must contain exactly one direct sheets element",
        ),
    ] {
        let error = open(&replacement);
        assert!(
            matches!(&error, Some(Error::InvalidWorkbook(message)) if message == expected),
            "{replacement}: {error:?}"
        );
    }
}

#[test]
fn signed_packages_are_reported_before_any_edit() {
    let book = Workbook::from_bytes(archive("", "", "")).unwrap();
    assert!(!book.is_signed());

    let mut origin = parts("", "", "");
    origin.insert(
        "_rels/.rels".into(),
        format!("<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='office' Type='{OFFICE_NAMESPACE}/officeDocument' Target='xl/workbook.xml'/><Relationship Id='signature' Type='{SIGNATURE_ORIGIN}' Target='signatures/origin.sigs'/></Relationships>"),
    );
    let mut folder = parts("", "", "");
    folder.insert("_XmlSignatures/sig1.xml".into(), "<Signature/>".into());
    for parts in [origin, folder] {
        let mut book = Workbook::from_bytes(stored(&parts)).unwrap();
        assert!(book.is_signed());
        assert!(matches!(
            book.set_cell("Data", "B1", 1),
            Err(Error::Unsupported(message)) if message.contains("signed")
        ));
        assert_eq!(
            book.get_cell("Data", "B1").unwrap().value,
            CellValue::Number(45_306.0)
        );
        assert!(!book.has_changes());
    }
}

#[test]
fn debug_output_summarizes_the_workbook_without_its_xml() {
    let mut book =
        Workbook::from_bytes(archive("<workbookPr date1904='1'/>", "", " state='hidden'")).unwrap();
    assert_eq!(
        format!("{book:?}"),
        "Workbook { sheets: [\
         Sheet { name: \"Data\", path: \"xl/worksheets/sheet1.xml\", visibility: Visible }, \
         Sheet { name: \"Other\", path: \"xl/worksheets/sheet2.xml\", visibility: Hidden }], \
         signed: false, date1904: true, changed_parts: [], .. }"
    );
    book.set_cell("Other", "D1", 3).unwrap();
    let debug = format!("{book:#?}");
    assert!(
        debug.contains("changed_parts: [\n        \"xl/worksheets/sheet2.xml\",\n    ],"),
        "{debug}"
    );
    assert!(!debug.contains("sheetData"), "{debug}");

    let view = book.prepare_sheet("Other").unwrap();
    assert_eq!(
        format!("{view:?}"),
        "WorksheetView { sheet: Sheet { name: \"Other\", path: \"xl/worksheets/sheet2.xml\", visibility: Hidden }, cells: 4, .. }"
    );
    // Results holding workbooks and views support the Debug-based helpers.
    assert!(matches!(
        book.prepare_sheet("Missing").unwrap_err(),
        Error::SheetNotFound(_)
    ));
    assert!(!matches!(
        Workbook::from_bytes(Vec::new()).unwrap_err(),
        Error::Io(_)
    ));
}
