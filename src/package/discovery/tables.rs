//! Worksheet table-header guards, resolved through package relationships.
use super::targets::{self, TargetBase};
use super::{namespaces, part, relationships};
use crate::archive::Archive;
use crate::{CellRange, CellRef, Error, Result, xml};
use std::collections::{HashMap, HashSet};

/// The header rows of the Excel tables related to a worksheet part. Excel
/// requires each header cell's text to equal its table column's name. Errors
/// name the relationships or table part they concern, not the worksheet.
///
/// `parsed` holds the header of each table part already read, by its ZIP
/// member name, for any worksheet of the archive: a header depends only on its
/// table part, which no edit changes. Parts that fail are not recorded.
pub(in crate::package) fn table_headers(
    archive: &Archive,
    worksheet_uri: &str,
    parsed: &mut HashMap<String, Option<CellRange>>,
) -> Result<Vec<CellRange>> {
    let relationships_uri = targets::relationships_path(worksheet_uri);
    let relationships_part = targets::part_path(archive, &relationships_uri)
        .map_err(|error| error.in_part(&relationships_uri))?;
    if !archive.contains(&relationships_part) {
        return Ok(Vec::new());
    }
    let in_relationships = |error: Error| error.in_part(&relationships_part);
    let mut headers = Vec::new();
    let mut tables: HashSet<String> = HashSet::new();
    // Targets already resolved, keyed by the worksheet directory segments they
    // keep and their ASCII-lowercased appended path. Part lookup ignores ASCII
    // case, so spellings with one key name the same part.
    let mut resolved: HashSet<(usize, String)> = HashSet::new();
    let mut base = None;
    let relationships = archive
        .read(&relationships_part)
        .and_then(|xml| {
            relationships::parse(&xml, &|_, kind| namespaces::office_type(kind, "table"))
        })
        .map_err(in_relationships)?;
    for relationship in relationships {
        if relationship.external {
            return Err(in_relationships(Error::InvalidWorkbook(format!(
                "table relationship {} is external",
                relationship.id
            ))));
        }
        // This worksheet's source URI is fixed and can be long. Normalizing a
        // target against its segments costs only the target's length, so only
        // the first relationship with each key builds and looks up the path.
        // Check external mode first, even when the target repeats.
        let target = targets::escaped_target(&relationship.target).map_err(in_relationships)?;
        let base = match &base {
            Some(base) => base,
            None => base.insert(TargetBase::try_new(worksheet_uri).map_err(in_relationships)?),
        };
        let (kept, appended) = base.normalize(&target).map_err(in_relationships)?;
        let key = (kept, appended.to_ascii_lowercase());
        if resolved.contains(&key) {
            continue;
        }
        let path =
            targets::part_path(archive, &base.join(kept, &appended)).map_err(in_relationships)?;
        if !archive.contains(&path) {
            return Err(in_relationships(Error::InvalidWorkbook(format!(
                "missing table part {path}"
            ))));
        }
        resolved.insert(key);
        // Different relationship IDs and URI spellings can name one part.
        // Validate every relationship, but read each resolved table only once.
        if !tables.insert(path.clone()) {
            continue;
        }
        let header = match parsed.get(&path) {
            Some(&header) => header,
            None => {
                let header = archive
                    .read(&path)
                    .and_then(|xml| table_header(&xml))
                    .map_err(|error| error.in_part(&path))?;
                parsed.insert(path, header);
                header
            }
        };
        headers.extend(header);
    }
    Ok(headers)
}

fn table_header(xml: &[u8]) -> Result<Option<CellRange>> {
    let invalid = |problem: &str| Error::InvalidWorkbook(format!("table {problem}"));
    let nodes = part::xml_nodes(xml, part::root_only)?;
    let table = &nodes[0];
    if table.local != "table" || !namespaces::main_namespace(&table.namespace) {
        return Err(Error::InvalidWorkbook(
            "expected a SpreadsheetML table".into(),
        ));
    }
    let reference = table
        .attribute("", "ref")
        .ok_or_else(|| invalid("lacks a ref"))?;
    let range: CellRange = xml::trim_whitespace(reference)
        .parse()
        .map_err(|_| invalid("has an invalid ref"))?;
    let rows: u32 = match table.attribute("", "headerRowCount") {
        None => 1,
        Some(count) => xml::trim_whitespace(count)
            .parse()
            .map_err(|_| invalid("has an invalid headerRowCount"))?,
    };
    if rows > range.height() {
        return Err(invalid("has more header rows than rows"));
    }
    Ok((rows > 0).then(|| {
        let first = range.first();
        let last = CellRef {
            row: first.row + rows - 1,
            column: range.last().column,
        };
        CellRange::new(first, last)
    }))
}
