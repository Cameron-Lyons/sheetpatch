//! Namespace-aware XML edits against byte spans, without reserializing the sheet.
use std::collections::{HashMap, HashSet};

use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};

use crate::{CellRef, CellValue, Error, Result};

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

fn valid_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
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
    let mut result = Vec::with_capacity(xml.len());
    let mut position = 0;
    for edit in edits {
        if edit.start < position || edit.end < edit.start || edit.end > xml.len() {
            return Err(unsupported("overlapping XML edits"));
        }
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

fn spans_edit(row: &Element, column: u16) -> Result<Option<Edit>> {
    let Some(attribute) = row.attr("spans") else {
        return Ok(None);
    };
    let mut spans: Vec<(u16, u16)> = Vec::new();
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
    if spans
        .iter()
        .any(|&(first, last)| (first..=last).contains(&column))
    {
        return Ok(None);
    }
    spans.push((column, column));
    spans.sort_unstable();
    // Row spans are hints; widen the closest span while retaining every old
    // interval. The common single interval stays a single interval.
    if spans.len() == 2 {
        spans = vec![(spans[0].0, spans[1].1)];
    }
    let value = spans
        .iter()
        .map(|(first, last)| format!("{first}:{last}"))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(attribute_edit(row, "spans", Some(&value)))
}

pub(crate) fn patch_cell(xml: &[u8], cell: CellRef, value: &CellValue) -> Result<Vec<u8>> {
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
    let data = &elements[data_index];
    let mut rows: Vec<RowCells> = Vec::new();
    let mut previous_row = 0;
    let mut shared: HashMap<String, bool> = HashMap::new();
    let mut protected = Vec::new();
    for &index in &data.children {
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
                if address == cell {
                    return Err(unsupported(format!("{cell} contains a formula")));
                }
                let range = formula
                    .attr("ref")
                    .map(|attribute| reference_range(&attribute.value))
                    .transpose()?;
                if let Some(range) = range {
                    protected.push(range);
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
                        return Err(unsupported("formula group without a reference range"));
                    }
                    _ => {}
                }
            }
        }
        rows.push((row_number, index, cells));
    }
    if shared.values().any(|has_range| !has_range) {
        return Err(unsupported(
            "shared formula group without a reference range",
        ));
    }
    if protected.iter().any(|range| range.contains(cell)) {
        return Err(unsupported(format!("{cell} belongs to a formula range")));
    }
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
            let range = reference_range(&reference.value)?;
            if range.contains(cell) && cell != range.first {
                return Err(unsupported(format!(
                    "{cell} is not the anchor of its merged range"
                )));
            }
        }
    }
    // A main-namespace row or cell hidden in another container is ambiguous
    // (e.g. an AlternateContent layout); leave it for an OOXML-aware editor.
    for element in &elements {
        if element.is(namespace, "row") && element.parent != Some(data_index) {
            return Err(unsupported("row outside sheetData"));
        }
        if element.is(namespace, "c")
            && !element.parent.is_some_and(|parent| {
                elements[parent].is(namespace, "row") && elements[parent].parent == Some(data_index)
            })
        {
            return Err(unsupported("cell outside a worksheet row"));
        }
    }
    let existing_row = rows.iter().find(|(number, _, _)| *number == cell.row);
    let existing_cell =
        existing_row.and_then(|(_, _, cells)| cells.iter().find(|(address, _)| *address == cell));
    if existing_cell.is_none() && matches!(value, CellValue::Blank) {
        return Ok(xml.to_vec());
    }
    let mut edits = Vec::new();
    if let Some(&(_, index)) = existing_cell {
        edits.push(Edit {
            start: elements[index].start,
            end: elements[index].end,
            bytes: patched_cell(xml, &elements, index, namespace, value)?,
        });
    } else if let Some((_, row_index, cells)) = existing_row {
        let row = &elements[*row_index];
        let new_cell = new_cell(row, cell, value);
        if row.empty {
            let mut bytes = opening(xml, row, spans_edit(row, cell.column)?, true)?;
            bytes.extend(new_cell);
            bytes.extend_from_slice(format!("</{}>", row.name).as_bytes());
            edits.push(Edit {
                start: row.start,
                end: row.end,
                bytes,
            });
        } else {
            let insertion = cells
                .iter()
                .find(|(address, _)| address.column > cell.column)
                .map(|(_, index)| elements[*index].start)
                .unwrap_or_else(|| {
                    row.children
                        .iter()
                        .map(|&index| &elements[index])
                        .find(|child| child.is(namespace, "extLst"))
                        .map_or(row.close_start, |child| child.start)
                });
            edits.push(Edit {
                start: insertion,
                end: insertion,
                bytes: new_cell,
            });
            if let Some(edit) = spans_edit(row, cell.column)? {
                edits.push(edit);
            }
        }
    } else {
        let row_name = data.qualified("row");
        let mut bytes = format!("<{row_name} r=\"{}\">", cell.row).into_bytes();
        bytes.extend(new_cell(data, cell, value));
        bytes.extend_from_slice(format!("</{row_name}>").as_bytes());
        if data.empty {
            let mut expanded = opening(xml, data, None, true)?;
            expanded.extend(bytes);
            expanded.extend_from_slice(format!("</{}>", data.name).as_bytes());
            edits.push(Edit {
                start: data.start,
                end: data.end,
                bytes: expanded,
            });
        } else {
            let insertion = rows
                .iter()
                .find(|(number, _, _)| *number > cell.row)
                .map(|(_, index, _)| elements[*index].start)
                .unwrap_or(data.close_start);
            edits.push(Edit {
                start: insertion,
                end: insertion,
                bytes,
            });
        }
    }
    if existing_cell.is_some()
        && let Some((_, row_index, _)) = existing_row
        && let Some(edit) = spans_edit(&elements[*row_index], cell.column)?
    {
        edits.push(edit);
    }
    let dimensions: Vec<_> = root
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
        let range = reference_range(&reference.value)?;
        if !range.contains(cell)
            && let Some(edit) =
                attribute_edit(dimension, "ref", Some(&range.including(cell).reference()))
        {
            edits.push(edit);
        }
    }
    edits_applied(xml, edits)
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
}
