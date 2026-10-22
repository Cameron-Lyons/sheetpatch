use super::date::{DateError, excel_serial};
use super::index::WorksheetIndex;
use super::parse::{Element, XmlDocument, parse_with};
use super::{ReadError, invalid, unique, unsupported, xml_error};
use crate::xml::{NamespaceId, excel_text, trim_whitespace as trim_xml_whitespace};
use crate::{CellContent, CellRef, CellValue, Error, Result};
use quick_xml::events::Event;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;

// XML normalizes literal CR/CRLF before expanding character references. Doing
// that in the opposite order would silently turn a deliberate &#13; into LF.
fn element_text(xml: &[u8], element: &Element) -> Result<String> {
    if element.has_children() {
        return Err(unsupported("elements inside a simple cell text value"));
    }
    if element.empty {
        return Ok(String::new());
    }
    let mut content = &xml[element.open_end()..element.close_start()];
    // Text without markup, references or carriage returns is its own value.
    if !content
        .iter()
        .any(|&byte| matches!(byte, b'<' | b'&' | b'\r'))
    {
        return String::from_utf8(content.to_vec()).map_err(|e| xml_error(e.to_string()));
    }
    let mut text = String::new();
    // A new reader discards a leading UTF-8 byte order mark, which inside an
    // element is the ordinary character U+FEFF. Consume each leading copy, so
    // the reader never starts at one.
    while let Some(rest) = content.strip_prefix("\u{feff}".as_bytes()) {
        text.push('\u{feff}');
        content = rest;
    }
    let mut reader = quick_xml::Reader::from_reader(content);
    loop {
        match reader.read_event().map_err(Error::xml)? {
            Event::Text(value) => {
                text.push_str(&value.xml10_content());
            }
            Event::CData(value) => {
                text.push_str(&value.xml10_content());
            }
            Event::GeneralRef(reference) => {
                text.push(crate::xml::reference(&reference)?);
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

pub(super) fn validate_rich_text_structure(
    source: &str,
    elements: &[Element],
    index: usize,
    namespace: NamespaceId,
) -> Result<()> {
    // Ignoring text hidden in compatibility branches or other containers would
    // return an incomplete scalar value. Phonetic text is a known, separate
    // annotation and remains excluded from the displayed text.
    for element in &elements[elements[index].descendants()] {
        let known_container =
            element.is(source, namespace, "r") || element.is(source, namespace, "rPh");
        let known_text = element.is(source, namespace, "t");
        if (known_container && element.parent() != Some(index))
            || (known_text
                && !element.parent().is_some_and(|parent| {
                    parent == index
                        || ((elements[parent].is(source, namespace, "r")
                            || elements[parent].is(source, namespace, "rPh"))
                            && elements[parent].parent() == Some(index))
                }))
        {
            return Err(unsupported("rich text inside an unsupported XML container"));
        }
        // CT_RElt permits one text element. Consumers can keep only the last
        // duplicate, so concatenating them would give reads and no-op detection
        // a different value. Each run's immediate children are scanned once.
        if element.is(source, namespace, "r")
            && element
                .children(elements)
                .filter(|&child| elements[child].is(source, namespace, "t"))
                .nth(1)
                .is_some()
        {
            return Err(unsupported("rich text run has multiple text elements"));
        }
    }
    Ok(())
}

fn rich_text(
    source: &str,
    elements: &[Element],
    index: usize,
    namespace: NamespaceId,
) -> Result<String> {
    validate_rich_text_structure(source, elements, index, namespace)?;
    let mut text = String::new();
    let mut append = |element: &Element| -> Result<()> {
        let piece = element_text(source.as_bytes(), element)?;
        // Only an underscore can start an Excel escape.
        let piece = if piece.contains('_') {
            excel_text(&piece)?
        } else {
            piece
        };
        if text.is_empty() {
            text = piece;
        } else {
            text.push_str(&piece);
        }
        Ok(())
    };
    for child in elements[index].children(elements) {
        let element = &elements[child];
        if element.is(source, namespace, "t") {
            append(element)?;
        } else if element.is(source, namespace, "r") {
            for child in element.children(elements) {
                let element = &elements[child];
                if element.is(source, namespace, "t") {
                    append(element)?;
                }
            }
        }
    }
    Ok(text)
}

fn is_string_table(document: &XmlDocument) -> bool {
    let root = &document.elements[0];
    root.local_name(document.source) == "sst" && document.is_spreadsheet_namespace(root.namespace)
}

// The most elements and normalized attribute values that the streaming
// shared-string parse has held at once on this thread, for tests.
#[cfg(test)]
thread_local! {
    pub(super) static SHARED_STRING_PEAKS: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}

/// Decodes each string as its `si` element completes and then releases that
/// subtree, so the parse holds one string's markup rather than the whole
/// table while still validating every event.
///
/// An entry that cannot be decoded, such as one with an unpaired UTF-16
/// surrogate escape or with text in an unsupported container, is recorded
/// with its error, so that only reads of that entry fail. Problems that
/// concern the whole table still fail it: a malformed or unsupported
/// document, the wrong root, mixed dialects, or an `si` inside another
/// element, which could shift the indexes of the entries after it. A table
/// too large to index is reported only after the rest of the part is known
/// to be valid.
pub(crate) fn read_shared_strings(xml: &[u8]) -> Result<SharedStrings> {
    let mut strings = SharedStrings::default();
    let mut distinct_errors = HashMap::new();
    let mut hidden_string = false;
    let mut mixed_namespace = false;
    let mut table_error = None;
    let document = parse_with(xml, |document, index| {
        #[cfg(test)]
        SHARED_STRING_PEAKS.with(|peaks| {
            let (elements, values) = peaks.get();
            peaks.set((
                elements.max(document.elements.len()),
                values.max(document.attributes.normalized_len()),
            ));
        });
        if hidden_string || mixed_namespace || !is_string_table(document) {
            return true;
        }
        let elements = &document.elements;
        let source = document.source;
        let namespace = elements[0].namespace;
        // Every element below the root arrives in exactly one released
        // subtree, as the child itself or one of its descendants.
        for current in std::iter::once(index).chain(elements[index].descendants()) {
            let element = &elements[current];
            mixed_namespace |= element.namespace != namespace
                && document.is_spreadsheet_namespace(element.namespace);
            hidden_string |= current != index && element.is(source, namespace, "si");
        }
        if !hidden_string
            && !mixed_namespace
            && table_error.is_none()
            && elements[index].is(source, namespace, "si")
            && let Err(error) = match rich_text(source, elements, index, namespace) {
                Ok(text) => strings.push(&text),
                Err(error) => strings.push_undecodable(error, &mut distinct_errors),
            }
        {
            table_error = Some(error);
        }
        true
    })?;
    if !is_string_table(&document) {
        return Err(invalid("expected an OOXML shared-string table"));
    }
    if mixed_namespace {
        return Err(unsupported(
            "shared-string table mixes strict and transitional SpreadsheetML namespaces",
        ));
    }
    if hidden_string {
        return Err(unsupported("shared string outside the shared-string table"));
    }
    if let Some(error) = table_error {
        return Err(error);
    }
    strings.text.shrink_to_fit();
    strings.ends.shrink_to_fit();
    strings.undecodable.shrink_to_fit();
    strings.errors.shrink_to_fit();
    Ok(strings)
}

/// A decoded shared-string table: the text of every string, concatenated,
/// and the offset where each string ends. One buffer and four bytes per string
/// replace a separately allocated `String` for each, which costs more than the
/// XML of a short or empty entry. Decoded text is never longer than its XML,
/// which `parse_with` limits to less than 4 GiB, and every entry takes several
/// bytes of it, so offsets and indexes fit in 32 bits.
#[derive(Debug, Default)]
pub(crate) struct SharedStrings {
    text: String,
    ends: Vec<u32>,
    // The entries that cannot be decoded, in index order, each with the
    // position of its error in `errors`. Such an entry holds no text.
    undecodable: Vec<(u32, u32)>,
    // Each distinct decoding error once, so that a table of many damaged
    // entries costs little more than their indexes. `Error` is not
    // `RefUnwindSafe` only because an `io::Error` can box any source. A kept
    // error never changes, and decoding never fails with an I/O error, so
    // the table, and a workbook holding it, stay unwind-safe.
    errors: Vec<AssertUnwindSafe<Error>>,
}

impl SharedStrings {
    /// The string at `index`, or the error that kept that entry from being
    /// decoded. `None` past the end of the table.
    pub(crate) fn get(&self, index: usize) -> Option<std::result::Result<&str, &Error>> {
        let end = *self.ends.get(index)? as usize;
        if let Ok(position) = self
            .undecodable
            .binary_search_by_key(&index, |&(entry, _)| entry as usize)
        {
            return Some(Err(&self.errors[self.undecodable[position].1 as usize].0));
        }
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| self.ends[previous] as usize);
        // Each offset ends a whole string, so it is a character boundary.
        Some(Ok(&self.text[start..end]))
    }

    fn push(&mut self, text: &str) -> Result<()> {
        let end = index_limit(self.text.len() + text.len())?;
        self.text.push_str(text);
        self.ends.push(end);
        Ok(())
    }

    // Records the next entry as undecodable. `distinct` holds the position
    // of each error already recorded, by its display.
    fn push_undecodable(
        &mut self,
        error: Error,
        distinct: &mut HashMap<String, u32>,
    ) -> Result<()> {
        let entry = index_limit(self.ends.len())?;
        let next = index_limit(self.errors.len())?;
        self.push("")?;
        let errors = &mut self.errors;
        let position = *distinct.entry(error.to_string()).or_insert_with(|| {
            errors.push(AssertUnwindSafe(error));
            next
        });
        self.undecodable.push((entry, position));
        Ok(())
    }
}

#[cfg(test)]
impl SharedStrings {
    /// How many distinct decoding errors the table holds.
    pub(super) fn error_count(&self) -> usize {
        self.errors.len()
    }
}

fn index_limit(value: usize) -> Result<u32> {
    u32::try_from(value)
        .map_err(|_| unsupported("shared-string table exceeds the 4 GiB index limit"))
}

#[cfg(test)]
impl<'a> FromIterator<&'a str> for SharedStrings {
    fn from_iter<I: IntoIterator<Item = &'a str>>(strings: I) -> Self {
        let mut table = Self::default();
        for text in strings {
            table.push(text).unwrap();
        }
        table
    }
}

// Tests compare a table with the strings it should hold.
#[cfg(test)]
impl<S: AsRef<str>> PartialEq<Vec<S>> for SharedStrings {
    fn eq(&self, strings: &Vec<S>) -> bool {
        self.ends.len() == strings.len()
            && strings
                .iter()
                .enumerate()
                .all(|(index, text)| {
                    matches!(self.get(index), Some(Ok(entry)) if entry == text.as_ref())
                })
    }
}

// A cell's stored value. A shared string is resolved separately: failing to
// load the table concerns that part rather than the cell.
pub(super) enum StoredValue {
    Value(CellValue),
    SharedString(usize),
}

// The cell's `v` or `is` payload, if it has one.
pub(super) fn value_payload(index: &WorksheetIndex, cell: &Element) -> Result<Option<usize>> {
    let (source, elements, namespace) = (index.source, &index.elements, index.namespace);
    unique(
        cell.children(elements).filter(|&child| {
            elements[child].is(source, namespace, "v")
                || elements[child].is(source, namespace, "is")
        }),
        || invalid("cell has multiple value payloads"),
    )
}

fn stored_value(
    xml: &[u8],
    index: &WorksheetIndex,
    cell: &Element,
    date1904: bool,
) -> Result<StoredValue> {
    match value_payload(index, cell)? {
        None => Ok(StoredValue::Value(CellValue::Blank)),
        Some(payload_index) => decode_payload(xml, index, cell, payload_index, date1904),
    }
}

// Decodes the cell's payload as reads see it; no-op detection compares an
// edit against the same value.
pub(super) fn decode_payload(
    xml: &[u8],
    index: &WorksheetIndex,
    cell: &Element,
    payload_index: usize,
    date1904: bool,
) -> Result<StoredValue> {
    let elements = &index.elements;
    let source = index.source;
    let namespace = index.namespace;
    let payload = &elements[payload_index];
    let kind = cell
        .attr(&index.attributes, "t")
        .map_or("n", |attribute| attribute.value);
    if kind == "inlineStr" {
        if !payload.is(source, namespace, "is") {
            return Err(invalid("inline string cell without an inline payload"));
        }
        let text = rich_text(source, elements, payload_index, namespace)?;
        return Ok(StoredValue::Value(CellValue::Text(text)));
    }
    if !payload.is(source, namespace, "v") {
        return Err(invalid("non-inline cell has an inline payload"));
    }
    let text = element_text(xml, payload)?;
    // Values that their cell type cannot hold make the workbook malformed;
    // only valid ISO 8601 dates outside the supported forms are unsupported.
    let value = match kind {
        "n" if trim_xml_whitespace(&text).is_empty() => CellValue::Blank,
        "n" => {
            let number = trim_xml_whitespace(&text)
                .parse::<f64>()
                .map_err(|_| invalid("invalid numeric cell value"))?;
            if !number.is_finite() {
                return Err(invalid("non-finite numeric cell value"));
            }
            CellValue::Number(number)
        }
        "b" => CellValue::Bool(match trim_xml_whitespace(&text) {
            "0" | "false" => false,
            "1" | "true" => true,
            _ => return Err(invalid("invalid boolean cell value")),
        }),
        "str" => CellValue::Text(excel_text(&text)?),
        "e" => CellValue::Error(excel_text(&text)?),
        // Dates read as the serial number Excel would store for them.
        "d" => CellValue::Number(excel_serial(trim_xml_whitespace(&text), date1904).map_err(
            |error| match error {
                DateError::Invalid => invalid("invalid ISO 8601 date cell value"),
                DateError::TimeZoneOffset => {
                    unsupported("ISO 8601 date cell value with a time-zone offset")
                }
                DateError::BeforeEpoch => unsupported(
                    "ISO 8601 date cell value before the start of the workbook's date system",
                ),
            },
        )?),
        "s" => {
            return trim_xml_whitespace(&text)
                .parse::<usize>()
                .map(StoredValue::SharedString)
                .map_err(|_| invalid("invalid shared-string index"));
        }
        _ => return Err(invalid(format!("invalid cell value type {kind:?}"))),
    };
    Ok(StoredValue::Value(value))
}

// The cell's style, formula and stored value, read in that order.
fn stored_cell(
    xml: &[u8],
    index: &WorksheetIndex,
    cell: &Element,
    date1904: bool,
) -> Result<(Option<u32>, Option<String>, StoredValue)> {
    let elements = &index.elements;
    let style_index = cell
        .attr(&index.attributes, "s")
        .map(|attribute| {
            trim_xml_whitespace(attribute.value)
                .parse::<u32>()
                .map_err(|_| invalid("invalid cell style index"))
        })
        .transpose()?;
    let formulas = cell
        .children(elements)
        .filter(|&child| elements[child].is(index.source, index.namespace, "f"));
    let formula = unique(formulas, || invalid("cell has multiple formulas"))?
        .map(|formula| element_text(xml, &elements[formula]))
        .transpose()?;
    Ok((
        style_index,
        formula,
        stored_value(xml, index, cell, date1904)?,
    ))
}

pub(super) fn cell_content<'a>(
    xml: &[u8],
    index: &WorksheetIndex,
    address: CellRef,
    shared_strings: &mut impl FnMut() -> Result<Option<&'a SharedStrings>>,
    date1904: bool,
) -> std::result::Result<CellContent, ReadError> {
    let Some(cell_index) = index.cell(address) else {
        return Ok(CellContent {
            value: CellValue::Blank,
            formula: None,
            style_index: None,
        });
    };
    let (style_index, formula, value) =
        stored_cell(xml, index, &index.elements[cell_index], date1904)
            .map_err(|error| error.context(address))?;
    let value = match value {
        StoredValue::Value(value) => value,
        StoredValue::SharedString(position) => {
            // A table that exists but cannot be loaded concerns that part;
            // a missing table or index is the cell's problem.
            let strings = shared_strings()
                .map_err(ReadError::SharedStrings)?
                .ok_or_else(|| {
                    invalid(format!(
                        "{address}: shared-string cell, but the workbook has no shared-string table"
                    ))
                })?;
            match strings.get(position) {
                Some(Ok(text)) => CellValue::Text(text.to_owned()),
                // Only the cells that use an undecodable entry fail.
                Some(Err(error)) => {
                    return Err(ReadError::UndecodableString {
                        index: position,
                        error: error.duplicate(),
                    });
                }
                None => {
                    return Err(invalid(format!(
                        "{address}: shared-string index {position} is out of bounds"
                    ))
                    .into());
                }
            }
        }
    };
    Ok(CellContent {
        value,
        formula,
        style_index,
    })
}

pub(super) fn read_cells<'a>(
    xml: &[u8],
    index: &WorksheetIndex,
    cells: &[CellRef],
    mut shared_strings: impl FnMut() -> Result<Option<&'a SharedStrings>>,
    date1904: bool,
) -> std::result::Result<Vec<CellContent>, ReadError> {
    if cells.is_empty() {
        return Ok(Vec::new());
    }
    // Reserve every result at once, so that a batch too large for memory is
    // an error rather than an aborting allocation partway through.
    let mut contents = Vec::new();
    contents
        .try_reserve_exact(cells.len())
        .map_err(|_| Error::Io(std::io::ErrorKind::OutOfMemory.into()))?;
    for &cell in cells {
        contents.push(cell_content(
            xml,
            index,
            cell,
            &mut shared_strings,
            date1904,
        )?);
    }
    Ok(contents)
}
