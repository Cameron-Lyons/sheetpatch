use super::*;
use crate::Workbook;

#[test]
fn workbook_discovery_does_not_alias_invalid_relationship_paths() {
    let archive =
        Archive::try_new(include_bytes!("../../../../tests/fixtures/libreoffice.xlsx").to_vec())
            .unwrap();
    for (part, target, invalid_targets) in [
        (
            "_rels/.rels",
            "xl/workbook.xml",
            [
                "xl//workbook.xml",
                "xl/workbook.xml/",
                "xl/workbook.xml/.",
                "xl/workbook.xml/child/..",
            ],
        ),
        (
            "xl/_rels/workbook.xml.rels",
            "worksheets/sheet1.xml",
            [
                "worksheets//sheet1.xml",
                "worksheets/sheet1.xml/",
                "worksheets/sheet1.xml/.",
                "worksheets/sheet1.xml/child/..",
            ],
        ),
    ] {
        let xml = String::from_utf8(archive.read(part).unwrap()).unwrap();
        assert!(xml.contains(target));
        for invalid_target in invalid_targets {
            let changes = BTreeMap::from([(
                part.to_owned(),
                xml.replace(target, invalid_target).into_bytes(),
            )]);
            let bytes = archive.write(&changes).unwrap();
            assert!(
                Workbook::from_bytes(bytes).is_err(),
                "{part}: {invalid_target}"
            );
        }
    }
}
