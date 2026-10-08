//! Namespace-aware XML edits against byte spans, without reserializing the sheet.
use std::collections::{BTreeMap, HashMap, HashSet};

use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};

use crate::xml::valid_char as valid_xml_char;
use crate::{CellContent, CellRef, CellValue, Error, Result};

const TRANSITIONAL: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const STRICT: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";

#[derive(Debug)]
struct Attribute {
    name: String,
    value: String,
    start: usize,
    value_start: usize,
    value_end: usize,
    end: usize,
}

#[derive(Debug)]
struct Element {
    name: String,
    namespace: Option<String>,
    parent: Option<usize>,
    children: Vec<usize>,
    attributes: Vec<Attribute>,
    start: usize,
    open_end: usize,
    close_start: usize,
    end: usize,
    empty: bool,
    opaque_markup: bool,
}

impl Element {
    fn local_name(&self) -> &str {
        self.name.rsplit(':').next().unwrap()
    }
    fn is(&self, namespace: &str, name: &str) -> bool {
        self.namespace.as_deref() == Some(namespace) && self.local_name() == name
    }
    fn attr(&self, name: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|attr| attr.name == name)
    }
    fn qualified(&self, name: &str) -> String {
        self.name
            .rsplit_once(':')
            .map_or_else(|| name.to_owned(), |(prefix, _)| format!("{prefix}:{name}"))
    }
}

struct Edit {
    start: usize,
    end: usize,
    bytes: Vec<u8>,
}

type RowCells = (u32, usize, Vec<(CellRef, usize)>);

fn xml_error(message: impl Into<String>) -> Error {
    Error::Xml(message.into())
}
fn unsupported(message: impl Into<String>) -> Error {
    Error::Unsupported(message.into())
}

fn decoded(input: &str) -> Result<String> {
    let value = quick_xml::escape::unescape(input).map_err(|e| xml_error(e.to_string()))?;
    if !value.chars().all(valid_xml_char) {
        return Err(xml_error("invalid XML character reference"));
    }
    Ok(value.into_owned())
}

// Attribute spans include their leading whitespace so deleting one does not
// disturb the spelling or whitespace of any other attribute.
fn attribute_spans(
    xml: &[u8],
    start: usize,
    open_end: usize,
    name_len: usize,
) -> Result<Vec<Attribute>> {
    let mut position = start + 1 + name_len;
    let mut result = Vec::new();
    while position < open_end {
        let whitespace_start = position;
        while position < open_end && xml[position].is_ascii_whitespace() {
            position += 1;
        }
        if matches!(xml.get(position), Some(b'/' | b'>')) {
            break;
        }
        let name_start = position;
        while position < open_end && !xml[position].is_ascii_whitespace() && xml[position] != b'=' {
            position += 1;
        }
        let name = std::str::from_utf8(&xml[name_start..position])
            .map_err(|e| xml_error(e.to_string()))?
            .to_owned();
        while position < open_end && xml[position].is_ascii_whitespace() {
            position += 1;
        }
        if xml.get(position) != Some(&b'=') {
            return Err(xml_error("invalid XML attribute"));
        }
        position += 1;
        while position < open_end && xml[position].is_ascii_whitespace() {
            position += 1;
        }
        let quote = *xml
            .get(position)
            .ok_or_else(|| xml_error("unterminated XML attribute"))?;
        if quote != b'\'' && quote != b'"' {
            return Err(xml_error("unquoted XML attribute"));
        }
        position += 1;
        let value_start = position;
        while position < open_end && xml[position] != quote {
            position += 1;
        }
        if position >= open_end {
            return Err(xml_error("unterminated XML attribute"));
        }
        let value_end = position;
        let raw = std::str::from_utf8(&xml[value_start..value_end])
            .map_err(|e| xml_error(e.to_string()))?;
        if raw.contains('<') {
            return Err(xml_error("less-than sign in an XML attribute"));
        }
        let value = decoded(raw)?;
        position += 1;
        result.push(Attribute {
            name,
            value,
            start: whitespace_start,
            value_start,
            value_end,
            end: position,
        });
    }
    Ok(result)
}

fn parse(xml: &[u8]) -> Result<Vec<Element>> {
    let source =
        std::str::from_utf8(xml).map_err(|e| xml_error(format!("worksheet must be UTF-8: {e}")))?;
    if !source.chars().all(valid_xml_char) {
        return Err(xml_error("invalid XML character"));
    }
    let bom = if xml.starts_with(&[0xef, 0xbb, 0xbf]) {
        3
    } else {
        0
    };
    // quick-xml strips a BOM without including it in buffer_position(). Keep
    // offsets relative to the original input explicitly.
    let mut reader = NsReader::from_reader(&xml[bom..]);
    reader.config_mut().check_comments = true;
    let mut elements: Vec<Element> = Vec::new();
    let mut stack = Vec::new();
    let mut root_seen = false;
    let mut declaration_seen = false;
    loop {
        let start = reader.buffer_position() as usize + bom;
        let event = reader.read_event()?;
        let end = reader.buffer_position() as usize + bom;
        match event {
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                if stack.len() >= 256 {
                    return Err(unsupported(
                        "worksheet XML nesting deeper than 256 elements",
                    ));
                }
                if stack.is_empty() {
                    if root_seen {
                        return Err(xml_error("multiple XML root elements"));
                    }
                    root_seen = true;
                }
                let (namespace, _) = reader.resolver().resolve_element(tag.name());
                let namespace = match namespace {
                    ResolveResult::Bound(ns) => Some(
                        std::str::from_utf8(ns.as_ref())
                            .map_err(|e| xml_error(e.to_string()))?
                            .to_owned(),
                    ),
                    ResolveResult::Unbound => None,
                    ResolveResult::Unknown(prefix) => {
                        return Err(xml_error(format!(
                            "unbound namespace prefix {}",
                            String::from_utf8_lossy(&prefix)
                        )));
                    }
                };
                // Let quick-xml check attribute syntax and duplicate names, and
                // independently reject duplicate expanded names/prefix errors.
                let mut expanded = HashSet::new();
                for attribute in tag.attributes() {
                    let attribute = attribute.map_err(|e| xml_error(e.to_string()))?;
                    let (ns, local) = reader.resolver().resolve_attribute(attribute.key);
                    let ns = match ns {
                        ResolveResult::Bound(ns) => ns.as_ref().to_vec(),
                        ResolveResult::Unbound => Vec::new(),
                        ResolveResult::Unknown(prefix) => {
                            return Err(xml_error(format!(
                                "unbound attribute prefix {}",
                                String::from_utf8_lossy(&prefix)
                            )));
                        }
                    };
                    if !expanded.insert((ns, local.as_ref().to_vec())) {
                        return Err(xml_error("duplicate expanded attribute name"));
                    }
                }
                let name = std::str::from_utf8(tag.name().as_ref())
                    .map_err(|e| xml_error(e.to_string()))?
                    .to_owned();
                let attributes = attribute_spans(xml, start, end, name.len())?;
                let empty = matches!(event, Event::Empty(_));
                let parent = stack.last().copied();
                let index = elements.len();
                elements.push(Element {
                    name,
                    namespace,
                    parent,
                    children: Vec::new(),
                    attributes,
                    start,
                    open_end: end,
                    close_start: end,
                    end,
                    empty,
                    opaque_markup: false,
                });
                if let Some(parent) = parent {
                    elements[parent].children.push(index);
                }
                if !empty {
                    stack.push(index);
                }
            }
            Event::End(_) => {
                let index = stack
                    .pop()
                    .ok_or_else(|| xml_error("unmatched closing XML tag"))?;
                elements[index].close_start = start;
                elements[index].end = end;
            }
            Event::Text(text) => {
                let raw =
                    std::str::from_utf8(text.as_ref()).map_err(|e| xml_error(e.to_string()))?;
                if raw.contains("]]>") {
                    return Err(xml_error("CDATA terminator in ordinary XML text"));
                }
                if stack.is_empty() && !raw.trim().is_empty() {
                    return Err(xml_error("text outside the XML root"));
                }
            }
            Event::GeneralRef(reference) => {
                if stack.is_empty() {
                    return Err(xml_error("entity reference outside the XML root"));
                }
                let name = std::str::from_utf8(reference.as_ref())
                    .map_err(|e| xml_error(e.to_string()))?;
                decoded(&format!("&{name};"))?;
            }
            Event::CData(_) if stack.is_empty() => {
                return Err(xml_error("CDATA outside the XML root"));
            }
            Event::Comment(_) | Event::PI(_) => {
                if let Some(&index) = stack.last() {
                    elements[index].opaque_markup = true;
                }
            }
            Event::DocType(_) => return Err(unsupported("XML document type declarations")),
            Event::Decl(declaration) => {
                if root_seen || declaration_seen {
                    return Err(xml_error("misplaced XML declaration"));
                }
                declaration_seen = true;
                let version = declaration
                    .version()
                    .map_err(|e| xml_error(e.to_string()))?;
                if version.as_ref() != b"1.0" {
                    return Err(unsupported("XML versions other than 1.0"));
                }
                if let Some(encoding) = declaration.encoding() {
                    let encoding = encoding.map_err(|e| xml_error(e.to_string()))?;
                    if !encoding.eq_ignore_ascii_case(b"UTF-8") {
                        return Err(unsupported("non-UTF-8 worksheet XML"));
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() || !root_seen {
        return Err(xml_error("incomplete XML document"));
    }
    Ok(elements)
}

fn edits_applied(xml: &[u8], mut edits: Vec<Edit>) -> Result<Vec<u8>> {
    edits.sort_by_key(|edit| (edit.start, edit.end));
    let mut position = 0;
    let mut length = 0usize;
    for edit in &edits {
        if edit.start < position || edit.end < edit.start || edit.end > xml.len() {
            return Err(unsupported("overlapping XML edits"));
        }
        length = length
            .checked_add(edit.start - position)
            .and_then(|length| length.checked_add(edit.bytes.len()))
            .ok_or_else(|| unsupported("patched XML exceeds addressable memory"))?;
        position = edit.end;
    }
    length = length
        .checked_add(xml.len() - position)
        .ok_or_else(|| unsupported("patched XML exceeds addressable memory"))?;
    let mut result = Vec::with_capacity(length);
    position = 0;
    for edit in edits {
        result.extend_from_slice(&xml[position..edit.start]);
        result.extend_from_slice(&edit.bytes);
        position = edit.end;
    }
    result.extend_from_slice(&xml[position..]);
    Ok(result)
}

fn attribute_edit(element: &Element, name: &str, value: Option<&str>) -> Option<Edit> {
    if let Some(attribute) = element.attr(name) {
        match value {
            Some(value) if value != attribute.value => Some(Edit {
                start: attribute.value_start,
                end: attribute.value_end,
                bytes: value.as_bytes().to_vec(),
            }),
            None => Some(Edit {
                start: attribute.start,
                end: attribute.end,
                bytes: Vec::new(),
            }),
            _ => None,
        }
    } else {
        value.map(|value| Edit {
            start: element.open_end - if element.empty { 2 } else { 1 },
            end: element.open_end - if element.empty { 2 } else { 1 },
            bytes: format!(" {name}=\"{value}\"").into_bytes(),
        })
    }
}

fn opening(
    xml: &[u8],
    element: &Element,
    attribute: Option<Edit>,
    expand: bool,
) -> Result<Vec<u8>> {
    let mut edits = Vec::new();
    if let Some(mut edit) = attribute {
        edit.start -= element.start;
        edit.end -= element.start;
        edits.push(edit);
    }
    if expand && element.empty {
        edits.push(Edit {
            start: element.open_end - element.start - 2,
            end: element.open_end - element.start - 1,
            bytes: Vec::new(),
        });
    }
    edits_applied(&xml[element.start..element.open_end], edits)
}

fn text_escaped(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    for (position, character) in text.char_indices() {
        // Escape literal Excel control-escape tokens so Excel shows the exact
        // user string rather than interpreting, for example, _x0041_ as A.
        if character == '_'
            && position + 7 <= bytes.len()
            && matches!(bytes[position + 1], b'x' | b'X')
            && bytes[position + 2..position + 6]
                .iter()
                .all(u8::is_ascii_hexdigit)
            && bytes[position + 6] == b'_'
        {
            output.push_str("_x005F_");
            continue;
        }
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '\r' => output.push_str("&#13;"),
            _ => output.push(character),
        }
    }
    output
}

fn value_type(value: &CellValue) -> Option<&'static str> {
    match value {
        CellValue::Text(_) => Some("inlineStr"),
        CellValue::Bool(_) => Some("b"),
        CellValue::Error(_) => Some("e"),
        _ => None,
    }
}

fn payload(prefix: &Element, value: &CellValue) -> Vec<u8> {
    let v = prefix.qualified("v");
    match value {
        CellValue::Text(text) => {
            let is = prefix.qualified("is");
            let t = prefix.qualified("t");
            format!(
                "<{is}><{t} xml:space=\"preserve\">{}</{t}></{is}>",
                text_escaped(text)
            )
            .into_bytes()
        }
        CellValue::Number(number) => format!("<{v}>{number}</{v}>").into_bytes(),
        CellValue::Bool(boolean) => format!("<{v}>{}</{v}>", u8::from(*boolean)).into_bytes(),
        CellValue::Error(error) => format!("<{v}>{}</{v}>", text_escaped(error)).into_bytes(),
        CellValue::Blank => Vec::new(),
    }
}

fn new_cell(context: &Element, cell: CellRef, value: &CellValue) -> Vec<u8> {
    let c = context.qualified("c");
    let kind = value_type(value).map_or(String::new(), |kind| format!(" t=\"{kind}\""));
    let mut output = format!("<{c} r=\"{cell}\"{kind}>").into_bytes();
    output.extend(payload(context, value));
    output.extend_from_slice(format!("</{c}>").as_bytes());
    output
}

fn patched_cell(
    xml: &[u8],
    elements: &[Element],
    index: usize,
    namespace: &str,
    value: &CellValue,
) -> Result<Vec<u8>> {
    let cell = &elements[index];
    let value_children: Vec<_> = cell
        .children
        .iter()
        .copied()
        .filter(|&child| elements[child].is(namespace, "v") || elements[child].is(namespace, "is"))
        .collect();
    if value_children.len() > 1 {
        return Err(unsupported("cell has multiple value payloads"));
    }
    if let Some(&child) = value_children.first() {
        validate_payload(elements, child, namespace)?;
    }
    let kind = cell
        .attr("t")
        .map_or("n", |attribute| attribute.value.as_str());
    let unchanged = if let Some(&child) = value_children.first() {
        let old = &elements[child];
        match value {
            CellValue::Number(number) if kind == "n" && old.is(namespace, "v") => {
                element_text(xml, old)?.trim().parse::<f64>().ok() == Some(*number)
            }
            CellValue::Bool(boolean) if kind == "b" && old.is(namespace, "v") => {
                match element_text(xml, old)?.trim() {
                    "1" | "true" => *boolean,
                    "0" | "false" => !*boolean,
                    _ => false,
                }
            }
            CellValue::Text(text) if kind == "inlineStr" && old.is(namespace, "is") => {
                rich_text(xml, elements, child, namespace)? == *text
            }
            CellValue::Text(text) if kind == "str" && old.is(namespace, "v") => {
                excel_text(&element_text(xml, old)?)? == *text
            }
            CellValue::Error(error) if kind == "e" && old.is(namespace, "v") => {
                excel_text(&element_text(xml, old)?)? == *error
            }
            _ => false,
        }
    } else {
        matches!(value, CellValue::Blank)
    };
    if unchanged {
        return Ok(xml[cell.start..cell.end].to_vec());
    }
    let value_bytes = payload(cell, value);
    let kind = attribute_edit(cell, "t", value_type(value));
    if cell.empty {
        if value_bytes.is_empty() {
            return opening(xml, cell, kind, false);
        }
        let mut output = opening(xml, cell, kind, true)?;
        output.extend(value_bytes);
        output.extend_from_slice(format!("</{}>", cell.name).as_bytes());
        return Ok(output);
    }
    let mut edits = Vec::new();
    if let Some(kind) = kind {
        edits.push(kind);
    }
    if let Some(&child) = value_children.first() {
        edits.push(Edit {
            start: elements[child].start,
            end: elements[child].end,
            bytes: value_bytes,
        });
    } else if !value_bytes.is_empty() {
        let position = cell
            .children
            .iter()
            .map(|&child| &elements[child])
            .find(|child| child.is(namespace, "extLst"))
            .map_or(cell.close_start, |child| child.start);
        edits.push(Edit {
            start: position,
            end: position,
            bytes: value_bytes,
        });
    }
    for edit in &mut edits {
        edit.start -= cell.start;
        edit.end -= cell.start;
    }
    edits_applied(&xml[cell.start..cell.end], edits)
}

// Known rich text is the old value and may be replaced. Unfamiliar markup
// within that value could carry other meaning: refuse to discard it.
fn validate_payload(elements: &[Element], index: usize, namespace: &str) -> Result<()> {
    let element = &elements[index];
    if element.namespace.as_deref() != Some(namespace) || element.opaque_markup {
        return Err(unsupported("unfamiliar XML inside the cell value payload"));
    }
    let allowed_attributes: &[&str] = match element.local_name() {
        "v" | "is" | "r" | "rPr" => &[],
        "t" => &["xml:space"],
        "rPh" => &["sb", "eb"],
        "phoneticPr" => &["fontId", "type", "alignment"],
        "color" => &["auto", "indexed", "rgb", "theme", "tint"],
        "rFont" | "charset" | "family" | "b" | "i" | "strike" | "outline" | "shadow"
        | "condense" | "extend" | "sz" | "u" | "vertAlign" | "scheme" => &["val"],
        _ => return Err(unsupported("unfamiliar XML inside the cell value payload")),
    };
    if element.attributes.iter().any(|attribute| {
        attribute.name != "xmlns"
            && !attribute.name.starts_with("xmlns:")
            && !allowed_attributes.contains(&attribute.name.as_str())
    }) {
        return Err(unsupported(
            "unfamiliar attributes inside the cell value payload",
        ));
    }
    if matches!(element.local_name(), "v" | "t") && !element.children.is_empty() {
        return Err(unsupported("elements inside a simple cell value payload"));
    }
    for &child in &element.children {
        validate_payload(elements, child, namespace)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Range {
    first: CellRef,
    last: CellRef,
}
impl Range {
    fn contains(self, cell: CellRef) -> bool {
        cell.row >= self.first.row
            && cell.row <= self.last.row
            && cell.column >= self.first.column
            && cell.column <= self.last.column
    }
    fn including(self, cell: CellRef) -> Self {
        Self {
            first: CellRef {
                row: self.first.row.min(cell.row),
                column: self.first.column.min(cell.column),
            },
            last: CellRef {
                row: self.last.row.max(cell.row),
                column: self.last.column.max(cell.column),
            },
        }
    }
    fn reference(self) -> String {
        if self.first == self.last {
            self.first.to_string()
        } else {
            format!("{}:{}", self.first, self.last)
        }
    }
}

fn reference_range(reference: &str) -> Result<Range> {
    let mut parts = reference.split(':');
    let first: CellRef = parts
        .next()
        .unwrap_or_default()
        .replace('$', "")
        .parse()
        .map_err(|_| unsupported(format!("invalid cell range {reference:?}")))?;
    let last = if let Some(last) = parts.next() {
        last.replace('$', "")
            .parse()
            .map_err(|_| unsupported(format!("invalid cell range {reference:?}")))?
    } else {
        first
    };
    if parts.next().is_some() || first.row > last.row || first.column > last.column {
        return Err(unsupported(format!("invalid cell range {reference:?}")));
    }
    Ok(Range { first, last })
}

// A worksheet's structure is indexed once. Byte spans remain relative to the
// original document, so unrelated whitespace, prefixes and markup never move.
struct WorksheetIndex {
    elements: Vec<Element>,
    namespace: String,
    data_index: usize,
    rows: Vec<RowCells>,
    formula_cells: HashSet<CellRef>,
    formula_ranges: Vec<Range>,
    merged_ranges: Vec<Range>,
    incomplete_formula_group: bool,
}

impl WorksheetIndex {
    fn parse(xml: &[u8]) -> Result<Self> {
        let elements = parse(xml)?;
        let root = &elements[0];
        let namespace = root
            .namespace
            .as_deref()
            .ok_or_else(|| unsupported("worksheet has no OOXML namespace"))?;
        if root.local_name() != "worksheet" || !matches!(namespace, TRANSITIONAL | STRICT) {
            return Err(unsupported("expected an OOXML worksheet"));
        }
        let data: Vec<_> = root
            .children
            .iter()
            .copied()
            .filter(|&index| elements[index].is(namespace, "sheetData"))
            .collect();
        if data.len() != 1 {
            return Err(unsupported(
                "worksheet must contain exactly one sheetData element",
            ));
        }
        let data_index = data[0];
        let mut rows = Vec::new();
        let mut previous_row = 0;
        let mut shared: HashMap<String, bool> = HashMap::new();
        let mut formula_cells = HashSet::new();
        let mut formula_ranges = Vec::new();
        let mut incomplete_formula_group = false;
        for &index in &elements[data_index].children {
            let row = &elements[index];
            if !row.is(namespace, "row") {
                continue;
            }
            let row_number = row
                .attr("r")
                .ok_or_else(|| unsupported("rows without explicit row numbers"))?
                .value
                .parse::<u32>()
                .map_err(|_| unsupported("invalid row number"))?;
            if row_number <= previous_row || row_number > 1_048_576 {
                return Err(unsupported("duplicate, unsorted, or invalid row numbers"));
            }
            previous_row = row_number;
            let mut cells = Vec::new();
            let mut previous_column = 0;
            for &index in &row.children {
                let existing = &elements[index];
                if !existing.is(namespace, "c") {
                    continue;
                }
                let address: CellRef = existing
                    .attr("r")
                    .ok_or_else(|| unsupported("cells without explicit references"))?
                    .value
                    .parse()
                    .map_err(|_| unsupported("invalid cell reference in worksheet"))?;
                if address.row != row_number || address.column <= previous_column {
                    return Err(unsupported(
                        "duplicate, unsorted, or inconsistent cell references",
                    ));
                }
                previous_column = address.column;
                cells.push((address, index));
                for &formula in &existing.children {
                    let formula = &elements[formula];
                    if !formula.is(namespace, "f") {
                        continue;
                    }
                    formula_cells.insert(address);
                    let range = formula
                        .attr("ref")
                        .map(|attribute| reference_range(&attribute.value))
                        .transpose()?;
                    if let Some(range) = range {
                        formula_ranges.push(range);
                    }
                    match formula.attr("t").map(|attribute| attribute.value.as_str()) {
                        Some("shared") => {
                            let index = formula
                                .attr("si")
                                .ok_or_else(|| unsupported("shared formula without an index"))?
                                .value
                                .clone();
                            let has_range = shared.entry(index).or_insert(false);
                            *has_range |= range.is_some();
                        }
                        Some("array" | "dataTable") if range.is_none() => {
                            incomplete_formula_group = true;
                        }
                        _ => {}
                    }
                }
            }
            rows.push((row_number, index, cells));
        }
        incomplete_formula_group |= shared.values().any(|has_range| !has_range);
        let mut merged_ranges = Vec::new();
        for &index in &root.children {
            let merges = &elements[index];
            if !merges.is(namespace, "mergeCells") {
                continue;
            }
            for &index in &merges.children {
                let merge = &elements[index];
                if !merge.is(namespace, "mergeCell") {
                    continue;
                }
                let reference = merge
                    .attr("ref")
                    .ok_or_else(|| unsupported("merged cells without a reference"))?;
                merged_ranges.push(reference_range(&reference.value)?);
            }
        }
        // Main-namespace rows/cells hidden in another container (such as
        // AlternateContent) would make the visible cell addresses ambiguous.
        for element in &elements {
            if element.is(namespace, "row") && element.parent != Some(data_index) {
                return Err(unsupported("row outside sheetData"));
            }
            if element.is(namespace, "c")
                && !element.parent.is_some_and(|parent| {
                    elements[parent].is(namespace, "row")
                        && elements[parent].parent == Some(data_index)
                })
            {
                return Err(unsupported("cell outside a worksheet row"));
            }
        }
        let namespace = namespace.to_owned();
        Ok(Self {
            elements,
            namespace,
            data_index,
            rows,
            formula_cells,
            formula_ranges,
            merged_ranges,
            incomplete_formula_group,
        })
    }

    fn cell(&self, address: CellRef) -> Option<usize> {
        let row = self
            .rows
            .binary_search_by_key(&address.row, |&(row, _, _)| row)
            .ok()?;
        let cells = &self.rows[row].2;
        let cell = cells
            .binary_search_by_key(&address.column, |&(cell, _)| cell.column)
            .ok()?;
        Some(cells[cell].1)
    }

    fn validate_edits(&self, cells: &BTreeMap<CellRef, CellValue>) -> Result<()> {
        if self.incomplete_formula_group {
            return Err(unsupported("formula group without a reference range"));
        }
        for &cell in cells.keys() {
            if self.formula_cells.contains(&cell) {
                return Err(unsupported(format!("{cell} contains a formula")));
            }
        }
        validate_ranges(cells, &self.formula_ranges, false)?;
        validate_ranges(cells, &self.merged_ranges, true)
    }
}

#[derive(Clone, Copy)]
struct GuardEvent {
    row: u32,
    first_column: u16,
    last_column: u16,
    delta: i32,
}

// Sorting integer row coordinates with a radix pass keeps large collections of
// formula/merged ranges linear in the worksheet size. A small Fenwick tree then
// tests each edited column against active ranges in at most 15 steps.
fn sort_guard_events(events: &mut Vec<GuardEvent>) {
    if events.len() < 2 {
        return;
    }
    let mut scratch = events.clone();
    for shift in [0, 8, 16] {
        let mut counts = [0usize; 256];
        for event in events.iter() {
            counts[((event.row >> shift) & 255) as usize] += 1;
        }
        let mut total = 0;
        for count in &mut counts {
            let size = *count;
            *count = total;
            total += size;
        }
        for &event in events.iter() {
            let bucket = ((event.row >> shift) & 255) as usize;
            scratch[counts[bucket]] = event;
            counts[bucket] += 1;
        }
        std::mem::swap(events, &mut scratch);
    }
}

fn validate_ranges(
    cells: &BTreeMap<CellRef, CellValue>,
    ranges: &[Range],
    merged: bool,
) -> Result<()> {
    let mut events = Vec::with_capacity(ranges.len() * if merged { 4 } else { 2 });
    let mut add_range = |first_row, last_row, first_column, last_column| {
        if first_row <= last_row && first_column <= last_column {
            events.push(GuardEvent {
                row: first_row,
                first_column,
                last_column,
                delta: 1,
            });
            events.push(GuardEvent {
                row: last_row + 1,
                first_column,
                last_column,
                delta: -1,
            });
        }
    };
    for range in ranges {
        if merged {
            // The anchor remains editable; the rest of its rectangle does not.
            add_range(
                range.first.row,
                range.first.row,
                range.first.column + 1,
                range.last.column,
            );
            add_range(
                range.first.row + 1,
                range.last.row,
                range.first.column,
                range.last.column,
            );
        } else {
            add_range(
                range.first.row,
                range.last.row,
                range.first.column,
                range.last.column,
            );
        }
    }
    if events.is_empty() {
        return Ok(());
    }
    sort_guard_events(&mut events);
    let mut active = vec![0i32; 16_386];
    let mut position = 0;
    for &cell in cells.keys() {
        while position < events.len() && events[position].row <= cell.row {
            let event = events[position];
            for (column, delta) in [
                (event.first_column as usize, event.delta),
                (event.last_column as usize + 1, -event.delta),
            ] {
                let mut index = column;
                while index < active.len() {
                    active[index] += delta;
                    index += index & index.wrapping_neg();
                }
            }
            position += 1;
        }
        let mut count = 0;
        let mut index = cell.column as usize;
        while index > 0 {
            count += active[index];
            index &= index - 1;
        }
        if count > 0 {
            return Err(unsupported(if merged {
                format!("{cell} is not the anchor of its merged range")
            } else {
                format!("{cell} belongs to a formula range")
            }));
        }
    }
    Ok(())
}

fn spans_edit(row: &Element, columns: impl Iterator<Item = u16>) -> Result<Option<Edit>> {
    let Some(attribute) = row.attr("spans") else {
        return Ok(None);
    };
    let mut spans = Vec::new();
    for span in attribute.value.split_whitespace() {
        let Some((first, last)) = span.split_once(':') else {
            return Err(unsupported("invalid row spans"));
        };
        let first = first
            .parse::<u16>()
            .map_err(|_| unsupported("invalid row spans"))?;
        let last = last
            .parse::<u16>()
            .map_err(|_| unsupported("invalid row spans"))?;
        if first == 0 || first > last || last > 16_384 {
            return Err(unsupported("invalid row spans"));
        }
        spans.push((first, last));
    }
    if spans.is_empty() {
        return Err(unsupported("empty row spans"));
    }
    let mut first = spans.iter().map(|span| span.0).min().unwrap();
    let mut last = spans.iter().map(|span| span.1).max().unwrap();
    let mut changed = false;
    for column in columns {
        changed |= !spans
            .iter()
            .any(|&(start, end)| (start..=end).contains(&column));
        first = first.min(column);
        last = last.max(column);
    }
    if !changed {
        return Ok(None);
    }
    // Spans are optional hints. Widen once for the whole batch, retaining every
    // old interval and covering all new columns with a conservative bound.
    Ok(attribute_edit(
        row,
        "spans",
        Some(&format!("{first}:{last}")),
    ))
}

pub(crate) fn patch_cells(xml: &[u8], cells: &BTreeMap<CellRef, CellValue>) -> Result<Vec<u8>> {
    if cells.is_empty() {
        return Ok(xml.to_vec());
    }
    for value in cells.values() {
        value.validate()?;
    }
    let index = WorksheetIndex::parse(xml)?;
    index.validate_edits(cells)?;
    let elements = &index.elements;
    let namespace = &index.namespace;
    let data = &elements[index.data_index];
    let mut edits = Vec::new();
    let mut changed_cells = Vec::new();
    let mut pending = cells.iter().peekable();
    let mut row_position = 0;
    let mut empty_data_rows = Vec::new();
    while let Some(&(&address, _)) = pending.peek() {
        let row_number = address.row;
        while row_position < index.rows.len() && index.rows[row_position].0 < row_number {
            row_position += 1;
        }
        let existing_row = index
            .rows
            .get(row_position)
            .filter(|&&(number, _, _)| number == row_number);
        let mut row_changes = Vec::new();
        while pending
            .peek()
            .is_some_and(|(address, _)| address.row == row_number)
        {
            row_changes.push(pending.next().unwrap());
        }
        if let Some((_, row_index, existing_cells)) = existing_row {
            let row = &elements[*row_index];
            let mut cell_position = 0;
            let mut changed_columns = Vec::new();
            let mut empty_row_cells = Vec::new();
            let fallback = row
                .children
                .iter()
                .map(|&index| &elements[index])
                .find(|child| child.is(namespace, "extLst"))
                .map_or(row.close_start, |child| child.start);
            for (&cell, value) in row_changes {
                while cell_position < existing_cells.len()
                    && existing_cells[cell_position].0.column < cell.column
                {
                    cell_position += 1;
                }
                let existing = existing_cells
                    .get(cell_position)
                    .filter(|&&(address, _)| address == cell);
                if let Some(&(_, cell_index)) = existing {
                    let bytes = patched_cell(xml, elements, cell_index, namespace, value)?;
                    if bytes == xml[elements[cell_index].start..elements[cell_index].end] {
                        continue;
                    }
                    edits.push(Edit {
                        start: elements[cell_index].start,
                        end: elements[cell_index].end,
                        bytes,
                    });
                } else if matches!(value, CellValue::Blank) {
                    continue;
                } else if row.empty {
                    empty_row_cells.extend(new_cell(row, cell, value));
                } else {
                    let insertion = existing_cells
                        .get(cell_position)
                        .map_or(fallback, |&(_, index)| elements[index].start);
                    edits.push(Edit {
                        start: insertion,
                        end: insertion,
                        bytes: new_cell(row, cell, value),
                    });
                }
                changed_cells.push(cell);
                changed_columns.push(cell.column);
            }
            if !changed_columns.is_empty() {
                let span = spans_edit(row, changed_columns.into_iter())?;
                if row.empty {
                    let mut bytes = opening(xml, row, span, true)?;
                    bytes.extend(empty_row_cells);
                    bytes.extend_from_slice(format!("</{}>", row.name).as_bytes());
                    edits.push(Edit {
                        start: row.start,
                        end: row.end,
                        bytes,
                    });
                } else if let Some(span) = span {
                    edits.push(span);
                }
            }
        } else {
            let mut new_cells = Vec::new();
            for (&cell, value) in row_changes {
                if !matches!(value, CellValue::Blank) {
                    new_cells.extend(new_cell(data, cell, value));
                    changed_cells.push(cell);
                }
            }
            if !new_cells.is_empty() {
                let row_name = data.qualified("row");
                let mut bytes = format!("<{row_name} r=\"{row_number}\">").into_bytes();
                bytes.extend(new_cells);
                bytes.extend_from_slice(format!("</{row_name}>").as_bytes());
                if data.empty {
                    empty_data_rows.extend(bytes);
                } else {
                    let insertion = index
                        .rows
                        .get(row_position)
                        .map_or(data.close_start, |&(_, index, _)| elements[index].start);
                    edits.push(Edit {
                        start: insertion,
                        end: insertion,
                        bytes,
                    });
                }
            }
        }
    }
    if !empty_data_rows.is_empty() {
        let mut bytes = opening(xml, data, None, true)?;
        bytes.extend(empty_data_rows);
        bytes.extend_from_slice(format!("</{}>", data.name).as_bytes());
        edits.push(Edit {
            start: data.start,
            end: data.end,
            bytes,
        });
    }
    if !changed_cells.is_empty() {
        let dimensions: Vec<_> = elements[0]
            .children
            .iter()
            .map(|&index| &elements[index])
            .filter(|element| element.is(namespace, "dimension"))
            .collect();
        if dimensions.len() > 1 {
            return Err(unsupported("multiple worksheet dimensions"));
        }
        if let Some(dimension) = dimensions.first() {
            let reference = dimension
                .attr("ref")
                .ok_or_else(|| unsupported("dimension without a reference"))?;
            let old = reference_range(&reference.value)?;
            let range = changed_cells
                .iter()
                .fold(old, |range, &cell| range.including(cell));
            if (!old.contains(range.first) || !old.contains(range.last))
                && let Some(edit) = attribute_edit(dimension, "ref", Some(&range.reference()))
            {
                edits.push(edit);
            }
        }
    }
    edits_applied(xml, edits)
}

#[cfg(test)]
fn patch_cell(xml: &[u8], cell: CellRef, value: &CellValue) -> Result<Vec<u8>> {
    patch_cells(xml, &BTreeMap::from([(cell, value.clone())]))
}

// XML normalizes literal CR/CRLF before expanding character references. Doing
// that in the opposite order would silently turn a deliberate &#13; into LF.
fn normalized_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn element_text(xml: &[u8], element: &Element) -> Result<String> {
    if !element.children.is_empty() {
        return Err(unsupported("elements inside a simple cell text value"));
    }
    if element.empty {
        return Ok(String::new());
    }
    let mut reader = quick_xml::Reader::from_reader(&xml[element.open_end..element.close_start]);
    let mut text = String::new();
    loop {
        match reader.read_event()? {
            Event::Text(value) => {
                let value =
                    std::str::from_utf8(value.as_ref()).map_err(|e| xml_error(e.to_string()))?;
                text.push_str(&normalized_text(value));
            }
            Event::CData(value) => {
                let value =
                    std::str::from_utf8(value.as_ref()).map_err(|e| xml_error(e.to_string()))?;
                text.push_str(&normalized_text(value));
            }
            Event::GeneralRef(reference) => {
                let name = std::str::from_utf8(reference.as_ref())
                    .map_err(|e| xml_error(e.to_string()))?;
                text.push_str(&decoded(&format!("&{name};"))?);
            }
            Event::Comment(_) | Event::PI(_) => {}
            Event::Eof => return Ok(text),
            _ => {
                return Err(unsupported(
                    "unexpected markup inside a simple cell text value",
                ));
            }
        }
    }
}

fn excel_text(text: &str) -> Result<String> {
    let bytes = text.as_bytes();
    let mut units = Vec::with_capacity(text.len());
    let mut position = 0;
    while position < bytes.len() {
        if bytes[position] == b'_'
            && position + 7 <= bytes.len()
            && matches!(bytes[position + 1], b'x' | b'X')
            && bytes[position + 2..position + 6]
                .iter()
                .all(u8::is_ascii_hexdigit)
            && bytes[position + 6] == b'_'
        {
            units.push(u16::from_str_radix(&text[position + 2..position + 6], 16).unwrap());
            position += 7;
        } else {
            let character = text[position..].chars().next().unwrap();
            let mut encoded = [0; 2];
            units.extend_from_slice(character.encode_utf16(&mut encoded));
            position += character.len_utf8();
        }
    }
    String::from_utf16(&units).map_err(|_| xml_error("invalid UTF-16 Excel text escape"))
}

fn rich_text(xml: &[u8], elements: &[Element], index: usize, namespace: &str) -> Result<String> {
    let mut text = String::new();
    for &child in &elements[index].children {
        let element = &elements[child];
        if element.is(namespace, "t") {
            text.push_str(&excel_text(&element_text(xml, element)?)?);
        } else if element.is(namespace, "r") {
            for &child in &element.children {
                let element = &elements[child];
                if element.is(namespace, "t") {
                    text.push_str(&excel_text(&element_text(xml, element)?)?);
                }
            }
        }
    }
    Ok(text)
}

pub(crate) fn read_shared_strings(xml: &[u8]) -> Result<Vec<String>> {
    let elements = parse(xml)?;
    let root = &elements[0];
    let namespace = root.namespace.as_deref().unwrap_or_default();
    if root.local_name() != "sst" || !matches!(namespace, TRANSITIONAL | STRICT) {
        return Err(unsupported("expected an OOXML shared-string table"));
    }
    root.children
        .iter()
        .copied()
        .filter(|&index| elements[index].is(namespace, "si"))
        .map(|index| rich_text(xml, &elements, index, namespace))
        .collect()
}

fn cell_content(
    xml: &[u8],
    index: &WorksheetIndex,
    address: CellRef,
    shared_strings: Option<&[String]>,
) -> Result<CellContent> {
    let Some(cell_index) = index.cell(address) else {
        return Ok(CellContent {
            value: CellValue::Blank,
            formula: None,
            style_index: None,
        });
    };
    let elements = &index.elements;
    let namespace = &index.namespace;
    let cell = &elements[cell_index];
    let style_index = cell
        .attr("s")
        .map(|attribute| {
            attribute
                .value
                .parse::<u32>()
                .map_err(|_| unsupported("invalid cell style index"))
        })
        .transpose()?;
    let children = |name| {
        cell.children
            .iter()
            .copied()
            .filter(move |&child| elements[child].is(namespace, name))
    };
    let formulas: Vec<_> = children("f").collect();
    if formulas.len() > 1 {
        return Err(unsupported("cell has multiple formulas"));
    }
    let formula = formulas
        .first()
        .map(|&index| element_text(xml, &elements[index]))
        .transpose()?;
    let values: Vec<_> = cell
        .children
        .iter()
        .copied()
        .filter(|&child| elements[child].is(namespace, "v") || elements[child].is(namespace, "is"))
        .collect();
    if values.len() > 1 {
        return Err(unsupported("cell has multiple value payloads"));
    }
    let value = if let Some(&payload_index) = values.first() {
        let payload = &elements[payload_index];
        let kind = cell
            .attr("t")
            .map_or("n", |attribute| attribute.value.as_str());
        if kind == "inlineStr" {
            if !payload.is(namespace, "is") {
                return Err(unsupported("inline string cell without an inline payload"));
            }
            CellValue::Text(rich_text(xml, elements, payload_index, namespace)?)
        } else {
            if !payload.is(namespace, "v") {
                return Err(unsupported("non-inline cell has an inline payload"));
            }
            let text = element_text(xml, payload)?;
            match kind {
                "n" if text.trim().is_empty() => CellValue::Blank,
                "n" => {
                    let number = text
                        .trim()
                        .parse::<f64>()
                        .map_err(|_| unsupported("invalid numeric cell value"))?;
                    if !number.is_finite() {
                        return Err(unsupported("non-finite numeric cell value"));
                    }
                    CellValue::Number(number)
                }
                "b" => CellValue::Bool(match text.trim() {
                    "0" | "false" => false,
                    "1" | "true" => true,
                    _ => return Err(unsupported("invalid boolean cell value")),
                }),
                "str" => CellValue::Text(excel_text(&text)?),
                "e" => CellValue::Error(excel_text(&text)?),
                "s" => {
                    let position = text
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| unsupported("invalid shared-string index"))?;
                    let strings = shared_strings.ok_or(Error::SharedStringsUnavailable)?;
                    CellValue::Text(
                        strings
                            .get(position)
                            .ok_or_else(|| {
                                Error::InvalidWorkbook(
                                    "shared-string index is out of bounds".into(),
                                )
                            })?
                            .clone(),
                    )
                }
                _ => return Err(unsupported(format!("unsupported cell value type {kind:?}"))),
            }
        }
    } else {
        CellValue::Blank
    };
    Ok(CellContent {
        value,
        formula,
        style_index,
    })
}

pub(crate) fn read_cells(
    xml: &[u8],
    cells: &[CellRef],
    shared_strings: Option<&[String]>,
) -> Result<Vec<CellContent>> {
    if cells.is_empty() {
        return Ok(Vec::new());
    }
    let index = WorksheetIndex::parse(xml)?;
    cells
        .iter()
        .map(|&cell| cell_content(xml, &index, cell, shared_strings))
        .collect()
}

#[cfg(test)]
fn read_cell(xml: &[u8], cell: CellRef, shared_strings: Option<&[String]>) -> Result<CellContent> {
    read_cells(xml, &[cell], shared_strings).map(|mut cells| cells.remove(0))
}

#[cfg(test)]
mod tests {
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
        assert!(
            changed.contains(r#"</row><row r="3"><c r="C3" t="b"><v>1</v></c></row><row r="4"/>"#)
        );
    }

    #[test]
    fn expands_empty_sheet_row_and_cell() {
        let empty = sheet("<sheetData />");
        let changed = edit(&empty, "D9", CellValue::Number(1.5)).unwrap();
        assert!(
            changed
                .contains(r#"<sheetData ><row r="9"><c r="D9"><v>1.5</v></c></row></sheetData>"#)
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
        assert!(
            changed.contains(r#"<x:v>keep</x:v><s:is><s:t xml:space="preserve">a</s:t></s:is>"#)
        );
        assert!(
            edit(&changed, "B1", CellValue::Number(2.0))
                .unwrap()
                .contains(r#"<s:c r="B1"><s:v>2</s:v></s:c>"#)
        );
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
            r#"<sheetData><row r="1"><c r="A1"><v>&unknown;</v></c></row></sheetData>"#,
        ] {
            assert!(
                edit(&sheet(body), "A1", CellValue::Number(2.0)).is_err(),
                "{body}"
            );
        }
    }

    fn batch(xml: &str, changes: &[(&str, CellValue)]) -> Result<String> {
        let cells = changes
            .iter()
            .map(|(address, value)| (address.parse().unwrap(), value.clone()))
            .collect();
        Ok(String::from_utf8(patch_cells(xml.as_bytes(), &cells)?).unwrap())
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
        let cells = read_cells(
            input.as_bytes(),
            &addresses,
            Some(&["first".into(), "second".into()]),
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
    fn rejects_invalid_read_values_and_missing_shared_strings() {
        for (kind, payload) in [
            ("n", "NaN"),
            ("n", "infinity"),
            ("b", "2"),
            ("s", "-1"),
            ("d", "2026-01-01"),
        ] {
            let input = sheet(&format!(
                r#"<sheetData><row r="1"><c r="A1" t="{kind}"><v>{payload}</v></c></row></sheetData>"#
            ));
            assert!(read_cell(input.as_bytes(), "A1".parse().unwrap(), Some(&[])).is_err());
        }
        let input =
            sheet(r#"<sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData>"#);
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
        let input =
            sheet(r#"<sheetData><row r="1"><c r="A1" s="8"><v>1</v></c></row></sheetData>"#);
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
    fn range_sweep_matches_rectangle_membership_with_overlapping_ranges() {
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
                        validate_ranges(
                            &BTreeMap::from([(cell, CellValue::Blank)]),
                            &ranges,
                            merged
                        )
                        .is_err(),
                        blocked
                    );
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
}
