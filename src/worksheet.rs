//! Namespace-aware XML edits against byte spans, without reserializing the sheet.
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
};

use quick_xml::{
    events::{Event, attributes::Attribute as XmlAttribute},
    name::{QName, ResolveResult},
    reader::NsReader,
};

use crate::xml::{
    excel_text, namespace as decoded_namespace, trim_whitespace as trim_xml_whitespace,
    valid_char as valid_xml_char, valid_name, valid_qname, validate_declaration,
    validate_namespaces, whitespace as xml_whitespace,
};
use crate::{CellContent, CellRef, CellValue, Error, Result};

const TRANSITIONAL: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const STRICT: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";

#[derive(Debug)]
struct Attribute<'a> {
    name: &'a str,
    value: Cow<'a, str>,
    start: usize,
    value_start: usize,
    value_end: usize,
    end: usize,
}

#[derive(Debug)]
struct Element<'a> {
    name: &'a str,
    namespace: Option<Rc<str>>,
    parent: Option<usize>,
    children_start: usize,
    children_end: usize,
    attributes: Vec<Attribute<'a>>,
    start: usize,
    open_end: usize,
    close_start: usize,
    end: usize,
    empty: bool,
    opaque_markup: bool,
}

impl<'xml> Element<'xml> {
    fn local_name(&self) -> &str {
        self.name.rsplit(':').next().unwrap()
    }
    fn is(&self, namespace: &str, name: &str) -> bool {
        self.namespace.as_deref() == Some(namespace) && self.local_name() == name
    }
    fn attr(&self, name: &str) -> Option<&Attribute<'_>> {
        self.attributes.iter().find(|attr| attr.name == name)
    }
    fn qualified(&self, name: &str) -> String {
        self.name
            .rsplit_once(':')
            .map_or_else(|| name.to_owned(), |(prefix, _)| format!("{prefix}:{name}"))
    }

    fn has_children(&self) -> bool {
        self.children_start < self.children_end
    }

    fn children<'a>(&self, elements: &'a [Element<'xml>]) -> Children<'a, 'xml> {
        Children {
            elements,
            next: self.children_start,
            end: self.children_end,
        }
    }
}

// Elements are recorded in preorder. A child's subtree ends at the next
// sibling, so the index needs no separately allocated list for each parent.
struct Children<'a, 'xml> {
    elements: &'a [Element<'xml>],
    next: usize,
    end: usize,
}

impl Iterator for Children<'_, '_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.end {
            return None;
        }
        let index = self.next;
        self.next = self.elements[index].children_end;
        Some(index)
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

fn unique<T>(mut items: impl Iterator<Item = T>, message: &'static str) -> Result<Option<T>> {
    let first = items.next();
    if items.next().is_some() {
        return Err(unsupported(message));
    }
    Ok(first)
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
) -> Result<Vec<Attribute<'_>>> {
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
        if position == whitespace_start {
            return Err(xml_error("missing XML attribute separator"));
        }
        let name_start = position;
        while position < open_end && !xml[position].is_ascii_whitespace() && xml[position] != b'=' {
            position += 1;
        }
        let name = std::str::from_utf8(&xml[name_start..position])
            .map_err(|e| xml_error(e.to_string()))?;
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
        let raw = &xml[value_start..value_end];
        if raw.contains(&b'<') {
            return Err(xml_error("less-than sign in an XML attribute"));
        }
        // XML normalizes literal attribute whitespace before expanding entity
        // references; referenced whitespace keeps its actual value.
        let value = XmlAttribute {
            key: QName(name),
            value: Cow::Borrowed(std::str::from_utf8(raw).map_err(|e| xml_error(e.to_string()))?),
        }
        .normalized_value(quick_xml::XmlVersion::Implicit1_0)?;
        if !value.chars().all(valid_xml_char) {
            return Err(xml_error("invalid XML character reference"));
        }
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

// Namespace URIs usually repeat for every row, cell, and value. Decode each raw
// spelling once and share it rather than allocating a URI per element.
fn cached_namespace(
    result: ResolveResult<'_>,
    cache: &mut HashMap<String, Rc<str>>,
) -> Result<Option<Rc<str>>> {
    match result {
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Bound(namespace) => {
            let value = if let Some(value) = cache.get(namespace.as_ref()) {
                Rc::clone(value)
            } else {
                let value: Rc<str> = decoded_namespace(ResolveResult::Bound(namespace))?.into();
                cache.insert(namespace.as_ref().to_owned(), Rc::clone(&value));
                value
            };
            Ok((!value.is_empty()).then_some(value))
        }
        unknown => decoded_namespace(unknown).map(|_| None),
    }
}

fn parse(xml: &[u8]) -> Result<Vec<Element<'_>>> {
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
    let mut namespaces = HashMap::new();
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
                let name =
                    std::str::from_utf8(&xml[start + 1..start + 1 + tag.name().as_ref().len()])
                        .map_err(|e| xml_error(e.to_string()))?;
                if !valid_qname(name) {
                    return Err(xml_error("invalid XML element name"));
                }
                validate_namespaces(tag)?;
                let (namespace, _) = reader.resolver().resolve_element(tag.name());
                let namespace = cached_namespace(namespace, &mut namespaces)?;
                // Let quick-xml check attribute syntax and duplicate names, and
                // independently reject duplicate expanded names/prefix errors.
                let mut first_expanded = None;
                let mut expanded = HashSet::new();
                for attribute in tag.attributes() {
                    let attribute = attribute.map_err(|e| xml_error(e.to_string()))?;
                    let attribute_name = attribute.key.as_ref();
                    if !valid_qname(attribute_name) {
                        return Err(xml_error("invalid XML attribute name"));
                    }
                    let (ns, local) = reader.resolver().resolve_attribute(attribute.key);
                    let ns = cached_namespace(ns, &mut namespaces)?;
                    // Unprefixed names and namespace declarations are already
                    // checked for exact duplicates by quick-xml. Only ordinary
                    // prefixed attributes can alias another expanded name.
                    if attribute_name.contains(':') && !attribute_name.starts_with("xmlns:") {
                        let name = (ns, local.into_inner());
                        if let Some(first) = &first_expanded {
                            if first == &name || !expanded.insert(name) {
                                return Err(xml_error("duplicate expanded attribute name"));
                            }
                        } else {
                            first_expanded = Some(name);
                        }
                    }
                }
                let attributes = attribute_spans(xml, start, end, name.len())?;
                let empty = matches!(event, Event::Empty(_));
                let parent = stack.last().copied();
                let index = elements.len();
                elements.push(Element {
                    name,
                    namespace,
                    parent,
                    children_start: index + 1,
                    children_end: index + 1,
                    attributes,
                    start,
                    open_end: end,
                    close_start: end,
                    end,
                    empty,
                    opaque_markup: false,
                });
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
                elements[index].children_end = elements.len();
            }
            Event::Text(text) => {
                let raw = text.as_ref();
                if raw.contains("]]>") {
                    return Err(xml_error("CDATA terminator in ordinary XML text"));
                }
                if stack.is_empty() && !xml_whitespace(raw) {
                    return Err(xml_error("text outside the XML root"));
                }
            }
            Event::GeneralRef(reference) => {
                if stack.is_empty() {
                    return Err(xml_error("entity reference outside the XML root"));
                }
                let name = reference.as_ref();
                decoded(&format!("&{name};"))?;
            }
            Event::CData(_) if stack.is_empty() => {
                return Err(xml_error("CDATA outside the XML root"));
            }
            Event::Comment(_) => {
                if let Some(&index) = stack.last() {
                    elements[index].opaque_markup = true;
                }
            }
            Event::PI(instruction) => {
                let target = instruction.target();
                if !valid_name(target) || target.eq_ignore_ascii_case("xml") {
                    return Err(xml_error("invalid XML processing instruction target"));
                }
                if let Some(&index) = stack.last() {
                    elements[index].opaque_markup = true;
                }
            }
            Event::DocType(_) => return Err(unsupported("XML document type declarations")),
            Event::Decl(declaration) => {
                if root_seen || declaration_seen || start != bom {
                    return Err(xml_error("misplaced XML declaration"));
                }
                declaration_seen = true;
                validate_declaration(&declaration)?;
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

fn patched_cell<'a>(
    xml: &'a [u8],
    elements: &[Element],
    index: usize,
    namespace: &str,
    value: &CellValue,
) -> Result<Cow<'a, [u8]>> {
    let cell = &elements[index];
    let value_child = unique(
        cell.children(elements).filter(|&child| {
            elements[child].is(namespace, "v") || elements[child].is(namespace, "is")
        }),
        "cell has multiple value payloads",
    )?;
    if let Some(child) = value_child {
        validate_payload(elements, child, namespace)?;
    }
    let kind = cell
        .attr("t")
        .map_or("n", |attribute| attribute.value.as_ref());
    let unchanged = if let Some(child) = value_child {
        let old = &elements[child];
        match value {
            CellValue::Number(number) if kind == "n" && old.is(namespace, "v") => {
                trim_xml_whitespace(&element_text(xml, old)?)
                    .parse::<f64>()
                    .ok()
                    == Some(*number)
            }
            CellValue::Bool(boolean) if kind == "b" && old.is(namespace, "v") => {
                match trim_xml_whitespace(&element_text(xml, old)?) {
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
        matches!(value, CellValue::Blank) && cell.attr("t").is_none()
    };
    if unchanged {
        return Ok(Cow::Borrowed(&xml[cell.start..cell.end]));
    }
    // Unlike cell metadata (`cm`), value metadata describes the old value and
    // can link to rich data in other package parts. Retaining that link after
    // replacing a scalar payload would silently leave stale value semantics.
    if cell.attr("vm").is_some() {
        return Err(unsupported("cell value has associated metadata"));
    }
    let value_bytes = payload(cell, value);
    let kind = attribute_edit(cell, "t", value_type(value));
    if cell.empty {
        if value_bytes.is_empty() {
            return opening(xml, cell, kind, false).map(Cow::Owned);
        }
        let mut output = opening(xml, cell, kind, true)?;
        output.extend(value_bytes);
        output.extend_from_slice(format!("</{}>", cell.name).as_bytes());
        return Ok(Cow::Owned(output));
    }
    let mut edits = Vec::new();
    if let Some(kind) = kind {
        edits.push(kind);
    }
    if let Some(child) = value_child {
        edits.push(Edit {
            start: elements[child].start,
            end: elements[child].end,
            bytes: value_bytes,
        });
    } else if !value_bytes.is_empty() {
        let position = cell
            .children(elements)
            .map(|child| &elements[child])
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
    edits_applied(&xml[cell.start..cell.end], edits).map(Cow::Owned)
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
            && !allowed_attributes.contains(&attribute.name)
    }) {
        return Err(unsupported(
            "unfamiliar attributes inside the cell value payload",
        ));
    }
    if matches!(element.local_name(), "v" | "t") && element.has_children() {
        return Err(unsupported("elements inside a simple cell value payload"));
    }
    for child in element.children(elements) {
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
struct WorksheetIndex<'a> {
    elements: Vec<Element<'a>>,
    namespace: String,
    data_index: usize,
    rows: Vec<RowCells>,
    formula_cells: HashSet<CellRef>,
    formula_ranges: Vec<Range>,
    merged_ranges: Vec<Range>,
    incomplete_formula_group: bool,
}

impl<'a> WorksheetIndex<'a> {
    fn parse(xml: &'a [u8]) -> Result<Self> {
        let elements = parse(xml)?;
        let root = &elements[0];
        let namespace = root
            .namespace
            .as_deref()
            .ok_or_else(|| unsupported("worksheet has no OOXML namespace"))?;
        if root.local_name() != "worksheet" || !matches!(namespace, TRANSITIONAL | STRICT) {
            return Err(unsupported("expected an OOXML worksheet"));
        }
        let data_index = unique(
            root.children(&elements)
                .filter(|&index| elements[index].is(namespace, "sheetData")),
            "worksheet must contain exactly one sheetData element",
        )?
        .ok_or_else(|| unsupported("worksheet must contain exactly one sheetData element"))?;
        let mut rows = Vec::new();
        let mut previous_row = 0;
        let mut shared: HashMap<u32, bool> = HashMap::new();
        let mut formula_cells = HashSet::new();
        let mut formula_ranges = Vec::new();
        let mut incomplete_formula_group = false;
        for index in elements[data_index].children(&elements) {
            let row = &elements[index];
            if !row.is(namespace, "row") {
                continue;
            }
            let row_reference = row
                .attr("r")
                .ok_or_else(|| unsupported("rows without explicit row numbers"))?;
            let row_number = trim_xml_whitespace(&row_reference.value)
                .parse::<u32>()
                .map_err(|_| unsupported("invalid row number"))?;
            if row_number <= previous_row || row_number > 1_048_576 {
                return Err(unsupported("duplicate, unsorted, or invalid row numbers"));
            }
            previous_row = row_number;
            let mut cells = Vec::new();
            let mut previous_column = 0;
            for index in row.children(&elements) {
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
                for formula in existing.children(&elements) {
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
                    match formula.attr("t").map(|attribute| attribute.value.as_ref()) {
                        Some("shared") => {
                            let index_reference = formula
                                .attr("si")
                                .ok_or_else(|| unsupported("shared formula without an index"))?;
                            let index = trim_xml_whitespace(&index_reference.value)
                                .parse::<u32>()
                                .map_err(|_| unsupported("invalid shared formula index"))?;
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
        for index in root.children(&elements) {
            let merges = &elements[index];
            if !merges.is(namespace, "mergeCells") {
                continue;
            }
            for index in merges.children(&elements) {
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
            if element.is(namespace, "f")
                && !element
                    .parent
                    .is_some_and(|parent| elements[parent].is(namespace, "c"))
            {
                return Err(unsupported("formula outside a worksheet cell"));
            }
            if (element.is(namespace, "v") || element.is(namespace, "is"))
                && !element
                    .parent
                    .is_some_and(|parent| elements[parent].is(namespace, "c"))
            {
                return Err(unsupported("value payload outside a worksheet cell"));
            }
            if element.is(namespace, "mergeCells") && element.parent != Some(0) {
                return Err(unsupported("merged ranges outside the worksheet root"));
            }
            if element.is(namespace, "mergeCell")
                && !element.parent.is_some_and(|parent| {
                    elements[parent].is(namespace, "mergeCells")
                        && elements[parent].parent == Some(0)
                })
            {
                return Err(unsupported("merged range outside mergeCells"));
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
    if cells.is_empty() || ranges.is_empty() {
        return Ok(());
    }
    let violation = |cell: CellRef| {
        unsupported(if merged {
            format!("{cell} is not the anchor of its merged range")
        } else {
            format!("{cell} belongs to a formula range")
        })
    };
    // With either dimension small, direct membership checks cost less than
    // sorting events and allocating a column index. Large batches still use
    // the sweep so their work does not grow as cells times ranges.
    if cells.len() <= 8 || ranges.len() <= 8 || cells.len().saturating_mul(ranges.len()) <= 256 {
        for &cell in cells.keys() {
            if ranges
                .iter()
                .any(|range| range.contains(cell) && (!merged || cell != range.first))
            {
                return Err(violation(cell));
            }
        }
        return Ok(());
    }
    let first_row = cells.first_key_value().unwrap().0.row;
    let last_row = cells.last_key_value().unwrap().0.row;
    let (first_column, last_column) = cells.keys().fold((u16::MAX, 0), |(first, last), cell| {
        (first.min(cell.column), last.max(cell.column))
    });
    // A rectangle outside the edited envelope cannot contain a requested
    // cell. Count relevant ranges before reserving event storage, so unrelated
    // worksheet ranges add no memory overhead to a batch.
    let relevant_ranges = ranges.iter().filter(|range| {
        range.first.row <= last_row
            && range.last.row >= first_row
            && range.first.column <= last_column
            && range.last.column >= first_column
    });
    let relevant_count = relevant_ranges.clone().count();
    if relevant_count == 0 {
        return Ok(());
    }
    let mut events = Vec::with_capacity(relevant_count * if merged { 4 } else { 2 });
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
    for range in relevant_ranges {
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
            return Err(violation(cell));
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

pub(crate) fn patch_cells<'a>(
    xml: &'a [u8],
    cells: &BTreeMap<CellRef, CellValue>,
) -> Result<Cow<'a, [u8]>> {
    if cells.is_empty() {
        return Ok(Cow::Borrowed(xml));
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
                .children(elements)
                .map(|index| &elements[index])
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
                    let Cow::Owned(bytes) =
                        patched_cell(xml, elements, cell_index, namespace, value)?
                    else {
                        continue;
                    };
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
        let dimension = unique(
            elements[0]
                .children(elements)
                .map(|index| &elements[index])
                .filter(|element| element.is(namespace, "dimension")),
            "multiple worksheet dimensions",
        )?;
        if let Some(dimension) = dimension {
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
    if edits.is_empty() {
        Ok(Cow::Borrowed(xml))
    } else {
        edits_applied(xml, edits).map(Cow::Owned)
    }
}

#[cfg(test)]
fn patch_cell(xml: &[u8], cell: CellRef, value: &CellValue) -> Result<Vec<u8>> {
    patch_cells(xml, &BTreeMap::from([(cell, value.clone())])).map(Cow::into_owned)
}

// XML normalizes literal CR/CRLF before expanding character references. Doing
// that in the opposite order would silently turn a deliberate &#13; into LF.
fn element_text(xml: &[u8], element: &Element) -> Result<String> {
    if element.has_children() {
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
                text.push_str(&value.xml10_content());
            }
            Event::CData(value) => {
                text.push_str(&value.xml10_content());
            }
            Event::GeneralRef(reference) => {
                let name = reference.as_ref();
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

fn rich_text(xml: &[u8], elements: &[Element], index: usize, namespace: &str) -> Result<String> {
    // Ignoring text hidden in compatibility branches or other containers would
    // return an incomplete scalar value. Phonetic text is a known, separate
    // annotation and remains excluded from the displayed text.
    for element in &elements[elements[index].children_start..elements[index].children_end] {
        let known_container = element.is(namespace, "r") || element.is(namespace, "rPh");
        let known_text = element.is(namespace, "t");
        if (known_container && element.parent != Some(index))
            || (known_text
                && !element.parent.is_some_and(|parent| {
                    parent == index
                        || ((elements[parent].is(namespace, "r")
                            || elements[parent].is(namespace, "rPh"))
                            && elements[parent].parent == Some(index))
                }))
        {
            return Err(unsupported("rich text inside an unsupported XML container"));
        }
    }
    let mut text = String::new();
    for child in elements[index].children(elements) {
        let element = &elements[child];
        if element.is(namespace, "t") {
            text.push_str(&excel_text(&element_text(xml, element)?)?);
        } else if element.is(namespace, "r") {
            for child in element.children(elements) {
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
    if elements
        .iter()
        .any(|element| element.is(namespace, "si") && element.parent != Some(0))
    {
        return Err(unsupported("shared string outside the shared-string table"));
    }
    root.children(&elements)
        .filter(|&index| elements[index].is(namespace, "si"))
        .map(|index| rich_text(xml, &elements, index, namespace))
        .collect()
}

fn cell_content<'a>(
    xml: &[u8],
    index: &WorksheetIndex,
    address: CellRef,
    shared_strings: &mut impl FnMut() -> Result<&'a [String]>,
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
            trim_xml_whitespace(&attribute.value)
                .parse::<u32>()
                .map_err(|_| unsupported("invalid cell style index"))
        })
        .transpose()?;
    let children = |name| {
        cell.children(elements)
            .filter(move |&child| elements[child].is(namespace, name))
    };
    let formula = unique(children("f"), "cell has multiple formulas")?
        .map(|index| element_text(xml, &elements[index]))
        .transpose()?;
    let payload_index = unique(
        cell.children(elements).filter(|&child| {
            elements[child].is(namespace, "v") || elements[child].is(namespace, "is")
        }),
        "cell has multiple value payloads",
    )?;
    let value = if let Some(payload_index) = payload_index {
        let payload = &elements[payload_index];
        let kind = cell
            .attr("t")
            .map_or("n", |attribute| attribute.value.as_ref());
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
                "n" if trim_xml_whitespace(&text).is_empty() => CellValue::Blank,
                "n" => {
                    let number = trim_xml_whitespace(&text)
                        .parse::<f64>()
                        .map_err(|_| unsupported("invalid numeric cell value"))?;
                    if !number.is_finite() {
                        return Err(unsupported("non-finite numeric cell value"));
                    }
                    CellValue::Number(number)
                }
                "b" => CellValue::Bool(match trim_xml_whitespace(&text) {
                    "0" | "false" => false,
                    "1" | "true" => true,
                    _ => return Err(unsupported("invalid boolean cell value")),
                }),
                "str" => CellValue::Text(excel_text(&text)?),
                "e" => CellValue::Error(excel_text(&text)?),
                "s" => {
                    let position = trim_xml_whitespace(&text)
                        .parse::<usize>()
                        .map_err(|_| unsupported("invalid shared-string index"))?;
                    let strings = shared_strings()?;
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

pub(crate) fn read_cells<'a>(
    xml: &[u8],
    cells: &[CellRef],
    mut shared_strings: impl FnMut() -> Result<&'a [String]>,
) -> Result<Vec<CellContent>> {
    if cells.is_empty() {
        return Ok(Vec::new());
    }
    let index = WorksheetIndex::parse(xml)?;
    let mut loaded_strings = None;
    let mut load_strings = || {
        if let Some(strings) = loaded_strings {
            return Ok(strings);
        }
        let strings = shared_strings()?;
        loaded_strings = Some(strings);
        Ok(strings)
    };
    cells
        .iter()
        .map(|&cell| cell_content(xml, &index, cell, &mut load_strings))
        .collect()
}

#[cfg(test)]
fn read_cell(xml: &[u8], cell: CellRef, shared_strings: Option<&[String]>) -> Result<CellContent> {
    read_cells(xml, &[cell], || {
        shared_strings.ok_or(Error::SharedStringsUnavailable)
    })
    .map(|mut cells| cells.remove(0))
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
            .replace("<c r='C1'/>", "<c r=\"B1\"><v>2</v></c><c r='C1'><v>3</v></c>")
            .replace("<row r='3'/>", "<row r=\"2\"><c r=\"A2\"><v>4</v></c></row><row r='3'><c r=\"B3\"><v>5</v></c></row>");
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
        let unbound =
            sheet("<sheetData><row r='1'><c r='A1' missing:flag='one'/></row></sheetData>");
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
        let elements = parse(input.as_bytes()).unwrap();
        let cell = elements
            .iter()
            .find(|element| element.is(TRANSITIONAL, "c"))
            .unwrap();
        assert_eq!(cell.attr("x:flag").unwrap().value, "a b c d e\rf\ng\th");
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
            let table = format!(
                "<sst xmlns='{TRANSITIONAL}' xmlns:x='urn:custom'><si>{payload}</si></sst>"
            );
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
                        validate_ranges(
                            &BTreeMap::from([(cell, CellValue::Blank)]),
                            &ranges,
                            merged
                        )
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
}
