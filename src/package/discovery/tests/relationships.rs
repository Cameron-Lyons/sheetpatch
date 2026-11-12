use super::*;
use crate::package::discovery::namespaces::OFFICE_NAMESPACE;
use crate::package::discovery::part::{NAMESPACE_LIMIT, Node};
use std::collections::BTreeSet;
use std::result::Result as StdResult;

// Relationship elements wherever they occur, so that misplaced ones can
// be rejected.
fn relationship_node(_: usize, namespace: &str, local: &str, _: Option<&Node>) -> bool {
    namespace == PACKAGE_NAMESPACE && local == "Relationship"
}

// The retaining reference parser, with the same schema normalization rules:
// it retains every relationship element, then checks them.
fn reference_relationships(bytes: &[u8]) -> Result<Vec<Relationship>> {
    let nodes = part::xml_nodes(bytes, relationship_node)?;
    if nodes[0].local != "Relationships" || &*nodes[0].namespace != PACKAGE_NAMESPACE {
        return Err(Error::InvalidWorkbook("invalid relationships root".into()));
    }
    let relationships = &nodes[1..];
    if relationships.iter().any(|node| node.parent != Some(0)) {
        return Err(Error::InvalidWorkbook(
            "relationship outside relationships root".into(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut result = Vec::new();
    for node in relationships {
        let required = |key| {
            node.attribute("", key)
                .map(str::to_owned)
                .ok_or_else(|| Error::InvalidWorkbook(format!("relationship lacks {key}")))
        };
        let id = required("Id")?;
        if !ids.insert(id.clone()) {
            return Err(Error::InvalidWorkbook("duplicate relationship id".into()));
        }
        let mode = node.attribute("", "TargetMode").unwrap_or("Internal");
        if !matches!(mode, "Internal" | "External") {
            return Err(Error::InvalidWorkbook(
                "invalid relationship TargetMode".into(),
            ));
        }
        result.push(Relationship {
            id,
            kind: xml::trim_whitespace(&required("Type")?).to_owned(),
            target: required("Target")?,
            external: mode == "External",
        });
    }
    Ok(result)
}

// A parse result with its error's debug form, so that results compare.
type Summary = StdResult<Vec<Relationship>, String>;

fn summary(relationships: Result<Vec<Relationship>>) -> Summary {
    relationships.map_err(|error| format!("{error:?}"))
}

#[test]
fn relationship_type_matching_trims_only_surrounding_xml_whitespace() {
    let table = format!("{OFFICE_NAMESPACE}/table");
    for (kind, selected) in [
        (table.clone(), true),
        (format!(" &#x9;{table}&#xD;&#xA; "), true),
        (format!("\u{85}{table}\u{a0}"), false),
        (format!("{OFFICE_NAMESPACE}/TABLE"), false),
        (format!("{OFFICE_NAMESPACE}/%74able"), false),
        (format!("{OFFICE_NAMESPACE}/ta&#x9;ble"), false),
        ("urn:vendor:table".into(), false),
    ] {
        let xml = format!(
            "<Relationships xmlns='{PACKAGE_NAMESPACE}'><Relationship Id='a' Type='{kind}' Target='table.xml'/></Relationships>"
        );
        let relationships = parse(xml.as_bytes(), &|_, kind| kind == table).unwrap();
        assert_eq!(relationships.len(), usize::from(selected), "{kind}");
        if selected {
            assert_eq!(relationships[0].kind, table);
        }
    }
}

#[test]
fn streamed_relationships_match_the_retaining_reference() {
    let roots = [
        (
            format!("<Relationships xmlns='{PACKAGE_NAMESPACE}'>"),
            "</Relationships>",
        ),
        (
            format!(
                "<?xml version='1.0'?><r:Relationships xmlns:r='{PACKAGE_NAMESPACE}' xmlns='{PACKAGE_NAMESPACE}'>"
            ),
            "</r:Relationships>",
        ),
        // Unprefixed children of these are in no namespace or another.
        (
            format!("<r:Relationships xmlns:r='{PACKAGE_NAMESPACE}'>"),
            "</r:Relationships>",
        ),
        (
            "<Relationships xmlns='urn:x'>".to_owned(),
            "</Relationships>",
        ),
        (
            format!("<Relations xmlns='{PACKAGE_NAMESPACE}'>"),
            "</Relations>",
        ),
        (
            format!("<Relationship xmlns='{PACKAGE_NAMESPACE}'>"),
            "</Relationship>",
        ),
    ];
    let restart: String = (0..NAMESPACE_LIMIT + 8)
        .map(|n| format!("<v:a xmlns:v='urn:{n}'/>"))
        .collect();
    let children = [
        format!("<Relationship Id='a' Type='{OFFICE_NAMESPACE}/table' Target='t.xml'/>"),
        format!("<Relationship Id='padded' Type=' &#x9;{OFFICE_NAMESPACE}/table&#xA; ' Target='t.xml'/>"),
        format!("<Relationship Id='b' Type='{OFFICE_NAMESPACE}/hyperlink' Target='https://example.com/' TargetMode='External'/>"),
        format!("<Relationship Id='c' Type='{OFFICE_NAMESPACE}/worksheet' Target='s.xml' TargetMode='Internal'></Relationship>"),
        "<Relationship Id='a&#98;' Type='t' Target='&#120;&amp;'/>".to_owned(),
        "<Relationship Id='ab' Type='t' Target='y'/>".to_owned(),
        "<Relationship Id='a\tb' Type='t\n' Target='z'/>".to_owned(),
        "<Relationship Id='a b' Type='t' Target='z'/>".to_owned(),
        "<Relationship Type='t' Target='x'/>".to_owned(),
        "<Relationship Id='m' Target='x'/>".to_owned(),
        "<Relationship Id='n' Type='t'/>".to_owned(),
        "<Relationship Id='e' Type='t' Target='x' TargetMode='external'/>".to_owned(),
        "<Relationship Id='f' Type='t' Target='x' TargetMode=' External'/>".to_owned(),
        "<Relationship Id='g' TargetMode='bad'/>".to_owned(),
        "<Relationship xmlns:p='urn:p' p:Id='h' Type='t' Target='x'/>".to_owned(),
        format!("<Relationship xmlns:p='{PACKAGE_NAMESPACE}' p:Id='i' Id='j' p:Type='t' Type='u' Target='x'/>"),
        format!("<q:Relationship xmlns:q='{PACKAGE_NAMESPACE}' Id='k' Type='t' Target='x'/>"),
        "<x:Relationship xmlns:x='urn:x' Id='a'/>".to_owned(),
        "<Relationship xmlns='urn:other' Id='a'/>".to_owned(),
        "<other Id='a'/>".to_owned(),
        "<x:wrap xmlns:x='urn:x'><Relationship Id='w' Type='t' Target='x'/></x:wrap>".to_owned(),
        "<Relationship Id='o' Type='t' Target='x'><Relationship Id='p' Type='t' Target='y'/></Relationship>".to_owned(),
        "<!-- comment --> ".to_owned(),
        restart,
        // Errors in the XML itself, which every structural error defers to.
        "<Relationship Id='a' Id='b'/>".to_owned(),
        "<u:x/>".to_owned(),
        "<a>&unknown;</a>".to_owned(),
        "<a></b>".to_owned(),
        "]]>".to_owned(),
    ];
    let mut state: u64 = 0x5eed_1234;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as usize
    };
    let selected = |id: &str, kind: &str| id.starts_with('a') || kind.ends_with("/table");
    let mut outcomes = BTreeSet::new();
    for _ in 0..4_000 {
        let (open, close) = &roots[next() % roots.len()];
        let mut xml = open.clone();
        for _ in 0..next() % 6 {
            xml.push_str(&children[next() % children.len()]);
        }
        xml.push_str(close);
        let reference = summary(reference_relationships(xml.as_bytes()));
        assert_eq!(
            summary(parse(xml.as_bytes(), &|_, _| true)),
            reference,
            "{xml}"
        );
        let filtered = reference.clone().map(|relationships| {
            relationships
                .into_iter()
                .filter(|relationship| selected(&relationship.id, &relationship.kind))
                .collect()
        });
        assert_eq!(summary(parse(xml.as_bytes(), &selected)), filtered, "{xml}");
        outcomes.insert(match reference {
            Ok(relationships) => format!("Ok({})", relationships.len().min(2)),
            Err(error) => error,
        });
    }
    // Every structural refusal and an XML error occur among the cases.
    for expected in [
        "invalid relationships root",
        "relationship outside relationships root",
        "relationship lacks Id",
        "relationship lacks Type",
        "relationship lacks Target",
        "duplicate relationship id",
        "invalid relationship TargetMode",
        "Xml(",
        "Ok(0)",
        "Ok(2)",
    ] {
        assert!(
            outcomes.iter().any(|outcome| outcome.contains(expected)),
            "{expected}: {outcomes:?}"
        );
    }
}
