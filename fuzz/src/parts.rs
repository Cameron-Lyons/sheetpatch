//! Valid packages around independently mutated metadata and shared strings.

use crate::fixture_with_parts;

const WORKSHEET: &[u8] = b"<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' xmlns:r='http://schemas.openxmlformats.org/officeDocument/2006/relationships'><sheetData><row r='1'><c r='A1' t='inlineStr'><is><t>Header</t></is></c><c r='C1'><v>7</v></c></row><row r='2'><c r='A2'><v>3</v></c></row></sheetData><tableParts count='1'><tablePart r:id='table'/></tableParts></worksheet>";
const WORKBOOK_RELATIONSHIPS: &[u8] = b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='sheet' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet' Target='worksheets/sheet1.xml'/></Relationships>";
const SHEET_RELATIONSHIPS: &[u8] = b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='table' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/table' Target='../tables/table1.xml'/></Relationships>";
const TABLE: &[u8] = b"<table xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' id='1' name='DataTable' displayName='DataTable' ref='A1:A2' headerRowCount='1'><tableColumns count='1'><tableColumn id='1' name='Header'/></tableColumns></table>";

/// One parser selection and the wrapper used for XML-fragment mutation.
pub struct Part {
    pub path: &'static str,
    pub open: &'static [u8],
    pub close: &'static [u8],
}

pub const PACKAGE_PARTS: &[Part] = &[
    Part {
        path: "[Content_Types].xml",
        open: b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'>",
        close: b"</Types>",
    },
    Part {
        path: "_rels/.rels",
        open: b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'>",
        close: b"</Relationships>",
    },
    Part {
        path: "xl/workbook.xml",
        open: b"<workbook xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' xmlns:r='http://schemas.openxmlformats.org/officeDocument/2006/relationships'>",
        close: b"</workbook>",
    },
    Part {
        path: "xl/_rels/workbook.xml.rels",
        open: b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'>",
        close: b"</Relationships>",
    },
    Part {
        path: "xl/worksheets/_rels/sheet1.xml.rels",
        open: b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'>",
        close: b"</Relationships>",
    },
    Part {
        path: "xl/tables/table1.xml",
        open: b"<table xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' id='1' name='DataTable' displayName='DataTable' ref='A1:A2' headerRowCount='1'>",
        close: b"</table>",
    },
];

pub fn package_part(part: &Part, xml: &[u8]) -> Vec<u8> {
    fixture_with_parts(
        WORKSHEET,
        b"unchanged package-part fuzz payload",
        &[
            ("xl/_rels/workbook.xml.rels", WORKBOOK_RELATIONSHIPS),
            ("xl/worksheets/_rels/sheet1.xml.rels", SHEET_RELATIONSHIPS),
            ("xl/tables/table1.xml", TABLE),
            (part.path, xml),
        ],
    )
}

pub fn shared_strings(xml: &[u8]) -> Vec<u8> {
    // The unused numeric/style-only cells exercise ranges that must not load
    // broken strings, and several indices reach per-entry decode failures.
    let worksheet = b"<worksheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><sheetData><row r='1'><c r='A1' t='s'><v>0</v></c><c r='B1'><v>7</v></c><c r='C1' t='s'><v>1</v></c><c r='D1' t='s'><v>2</v></c><c r='E1' t='s'><v>3</v></c></row><row r='2'><c r='B2' s='1'/></row></sheetData></worksheet>";
    let relationships = b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='sheet' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet' Target='worksheets/sheet1.xml'/><Relationship Id='strings' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings' Target='sharedStrings.xml'/></Relationships>";
    fixture_with_parts(
        worksheet,
        b"unchanged shared-string fuzz payload",
        &[
            ("xl/_rels/workbook.xml.rels", relationships),
            ("xl/sharedStrings.xml", xml),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sheetpatch::{CellValue, Error, Workbook};

    #[test]
    fn wrappers_reach_metadata_and_lazy_table_validation() {
        let xml = b"<workbook xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main' xmlns:r='http://schemas.openxmlformats.org/officeDocument/2006/relationships'><workbookPr date1904='1'/><sheets><sheet name='Renamed' sheetId='1' r:id='sheet'/></sheets></workbook>";
        let mut book = Workbook::from_bytes(package_part(&PACKAGE_PARTS[2], xml)).unwrap();
        assert!(book.uses_1904_dates());
        assert_eq!(book.sheets()[0].name(), "Renamed");
        assert!(matches!(
            book.set_cell("Renamed", "A1", "changed"),
            Err(Error::ProtectedCell { .. })
        ));
        book.set_cell("Renamed", "A2", 11).unwrap();
        crate::exercise(book);

        // A valid worksheet and workbook still open when only table metadata
        // is malformed; changing a value must then reach that lazy parser.
        for index in [4, 5] {
            let mut book =
                Workbook::from_bytes(package_part(&PACKAGE_PARTS[index], b"<broken")).unwrap();
            assert_eq!(
                book.get_cell("Data", "A2").unwrap().value,
                CellValue::Number(3.0)
            );
            assert!(book.set_cell("Data", "A2", 11).is_err());
        }
    }

    #[test]
    fn shared_string_wrapper_reaches_valid_and_invalid_entries() {
        let xml = b"<sst xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><si><t>first</t></si><si><t>_xD800_</t></si><si><r><t>last</t></r></si></sst>";
        let book = Workbook::from_bytes(shared_strings(xml)).unwrap();
        assert_eq!(
            book.get_cell("Data", "A1").unwrap().value.as_text(),
            Some("first")
        );
        assert!(matches!(
            book.get_cell("Data", "C1"),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(
            book.get_cell("Data", "D1").unwrap().value.as_text(),
            Some("last")
        );
        crate::exercise(book);
    }
}
