use super::date::{self, DateError};
use super::errors;
use super::index::WorksheetIndex;
use super::parse::{self, Element, XmlDocument};
use crate::xml::{self, NamespaceId};
use crate::{CellContent, CellRef, CellValue, Error, Result};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::borrow::Cow;
#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::result::Result as StdResult;
use std::{io, iter};

/// Why a read failed: a problem in the worksheet itself, a failure to load
/// the shared-string table a `t="s"` cell needed, or an entry of that table
/// that cannot be decoded. The table's load errors come from its loader,
/// which names that part, so only worksheet errors need the worksheet's name.
/// An entry's error concerns the table too, and needs its part and index.
#[derive(Debug)]
pub(crate) enum ReadError {
    Worksheet(Error),
    SharedStrings(Error),
    UndecodableString { index: usize, error: Error },
}

impl From<Error> for ReadError {
    fn from(error: Error) -> Self {
        Self::Worksheet(error)
    }
}

fn owned_text(text: Cow<'_, str>) -> Result<String> {
    match text {
        Cow::Borrowed(text) => xml::copy_text(text),
        Cow::Owned(text) => Ok(text),
    }
}

fn append_text(output: &mut String, text: &str) -> Result<()> {
    output
        .try_reserve(text.len())
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    output.push_str(text);
    Ok(())
}

// Normalize XML 1.0 literal line endings directly into the result, avoiding
// an infallibly allocated intermediate string from the event decoder.
fn append_xml_text(output: &mut String, text: &str) -> Result<()> {
    let mut pieces = text.split('\r');
    append_text(output, pieces.next().unwrap_or_default())?;
    for piece in pieces {
        append_text(output, "\n")?;
        append_text(output, piece.strip_prefix('\n').unwrap_or(piece))?;
    }
    Ok(())
}

// XML normalizes literal CR/CRLF before expanding character references. Doing
// that in the opposite order would silently turn a deliberate &#13; into LF.
fn element_text<'xml>(source: &'xml str, element: &Element) -> Result<Cow<'xml, str>> {
    if element.has_children() {
        return Err(errors::unsupported(
            "elements inside a simple cell text value",
        ));
    }
    if element.empty {
        return Ok(Cow::Borrowed(""));
    }
    let content = &source[element.open_end()..element.close_start()];
    // Text without markup, references or carriage returns is its own value.
    // Tiny scalars are cheaper to check inline. Search longer text in blocks.
    let plain = if content.len() < 16 {
        !content
            .bytes()
            .any(|byte| matches!(byte, b'<' | b'&' | b'\r'))
    } else {
        memchr::memchr3(b'<', b'&', b'\r', content.as_bytes()).is_none()
    };
    if plain {
        return Ok(Cow::Borrowed(content));
    }
    let mut content = content.as_bytes();
    let mut text = String::new();
    // A new reader discards a leading UTF-8 byte order mark, which inside an
    // element is the ordinary character U+FEFF. Consume each leading copy, so
    // the reader never starts at one.
    while let Some(rest) = content.strip_prefix("\u{feff}".as_bytes()) {
        append_text(&mut text, "\u{feff}")?;
        content = rest;
    }
    let mut reader = Reader::from_reader(content);
    loop {
        match reader.read_event().map_err(Error::xml)? {
            Event::Text(value) => {
                append_xml_text(&mut text, &value)?;
            }
            Event::CData(value) => {
                append_xml_text(&mut text, &value)?;
            }
            Event::GeneralRef(reference) => {
                let mut buffer = [0; 4];
                append_text(
                    &mut text,
                    xml::reference(&reference)?.encode_utf8(&mut buffer),
                )?;
            }
            Event::Comment(_) | Event::PI(_) => {}
            Event::Eof => return Ok(Cow::Owned(text)),
            _ => {
                return Err(errors::unsupported(
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
            return Err(errors::unsupported(
                "rich text inside an unsupported XML container",
            ));
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
            return Err(errors::unsupported(
                "rich text run has multiple text elements",
            ));
        }
    }
    Ok(())
}

// Visits the text elements of a rich-text element, such as a shared string's
// `si` or an inline string's `is`, in document order. Callers validate it with
// `validate_rich_text_structure` first.
fn visit_rich_text(
    source: &str,
    elements: &[Element],
    index: usize,
    namespace: NamespaceId,
    mut visit: impl FnMut(&Element) -> Result<()>,
) -> Result<()> {
    for child in elements[index].children(elements) {
        let element = &elements[child];
        if element.is(source, namespace, "t") {
            visit(element)?;
        } else if element.is(source, namespace, "r") {
            for child in element.children(elements) {
                let element = &elements[child];
                if element.is(source, namespace, "t") {
                    visit(element)?;
                }
            }
        }
    }
    Ok(())
}

// The text of a rich-text element appended to `text`, such as a shared string
// appended to its table's buffer. On an error, `text` may hold part of it.
fn append_rich_text(
    text: &mut String,
    source: &str,
    elements: &[Element],
    index: usize,
    namespace: NamespaceId,
) -> Result<()> {
    // The buffer grows as for pushing each entry decoded and whole. Decoding
    // never lengthens text, so an entry whose markup fits the buffer's spare
    // capacity is decoded straight into it, which cannot grow it, and pushing
    // it whole would not have either. An entry that may not fit is read to a
    // string of its own first, and the buffer grows for that.
    let element = &elements[index];
    if text.capacity() - text.len() < element.end() - element.start() {
        let entry = rich_text(source, elements, index, namespace)?;
        return append_text(text, &entry);
    }
    validate_rich_text_structure(source, elements, index, namespace)?;
    let capacity = text.capacity();
    visit_rich_text(source, elements, index, namespace, |element| {
        let piece = element_text(source, element)?;
        // Only an underscore can start an Excel escape.
        if piece.contains('_') {
            xml::append_excel_text(text, &piece)
        } else {
            text.push_str(&piece);
            Ok(())
        }
    })?;
    debug_assert_eq!(
        text.capacity(),
        capacity,
        "an entry is decoded within the markup that bounds it"
    );
    Ok(())
}

// The text of a rich-text element as a value of its own, such as an inline
// string. Its first nonempty piece becomes the value, so a decoded piece is
// not copied again. Escapes are decoded piece by piece into compact text:
// decoding them in place would keep their longer spelling's capacity.
fn rich_text(
    source: &str,
    elements: &[Element],
    index: usize,
    namespace: NamespaceId,
) -> Result<String> {
    validate_rich_text_structure(source, elements, index, namespace)?;
    let mut text = String::new();
    visit_rich_text(source, elements, index, namespace, |element| {
        let piece = element_text(source, element)?;
        // Only an underscore can start an Excel escape.
        let piece = if piece.contains('_') {
            Cow::Owned(xml::excel_text(&piece)?)
        } else {
            piece
        };
        if text.is_empty() {
            text = owned_text(piece)?;
            Ok(())
        } else {
            append_text(&mut text, &piece)
        }
    })?;
    Ok(text)
}

fn is_string_table(document: &XmlDocument) -> bool {
    let root = &document.elements[0];
    root.local_name(document.source) == "sst" && document.is_spreadsheet_namespace(root.namespace)
}

// The most elements and normalized attribute values that a parse has held at
// once.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ArenaPeaks {
    pub(super) elements: usize,
    pub(super) values: usize,
}

// The decoded text of every entry, in order, for tests.
#[cfg(test)]
impl SharedStrings {
    pub(super) fn text(&self) -> &str {
        &self.text
    }
}

// The peaks of the streaming shared-string parse on this thread, for tests,
// and the capacity of the table's text after each entry of the last table.
#[cfg(test)]
thread_local! {
    pub(super) static SHARED_STRING_PEAKS: Cell<ArenaPeaks> =
        const { Cell::new(ArenaPeaks { elements: 0, values: 0 }) };
    pub(super) static SHARED_STRING_CAPACITIES: RefCell<Vec<usize>> =
        const { RefCell::new(Vec::new()) };
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
    #[cfg(test)]
    SHARED_STRING_CAPACITIES.with_borrow_mut(Vec::clear);
    let mut strings = SharedStrings::default();
    let mut distinct_errors: HashMap<String, u32> = HashMap::new();
    let mut hidden_string = false;
    let mut mixed_namespace = false;
    let mut table_error: Option<Error> = None;
    let document = parse::parse_with(xml, |document, index| {
        #[cfg(test)]
        SHARED_STRING_PEAKS.with(|peaks| {
            let ArenaPeaks { elements, values } = peaks.get();
            peaks.set(ArenaPeaks {
                elements: elements.max(document.elements.len()),
                values: values.max(document.attributes.normalized_len()),
            });
        });
        if hidden_string || mixed_namespace || !is_string_table(document) {
            return true;
        }
        let elements = &document.elements;
        let source = document.source;
        let namespace = elements[0].namespace;
        // Every element below the root arrives in exactly one released
        // subtree, as the child itself or one of its descendants.
        let subtree = iter::once(index).chain(elements[index].descendants());
        for current in subtree {
            let element = &elements[current];
            mixed_namespace |= element.namespace != namespace
                && document.is_spreadsheet_namespace(element.namespace);
            hidden_string |= current != index && element.is(source, namespace, "si");
        }
        if !hidden_string
            && !mixed_namespace
            && table_error.is_none()
            && elements[index].is(source, namespace, "si")
        {
            if let Err(error) = strings.push_with(&mut distinct_errors, |text| {
                append_rich_text(text, source, elements, index, namespace)
            }) {
                table_error = Some(error);
            }
            #[cfg(test)]
            SHARED_STRING_CAPACITIES.with_borrow_mut(|capacities| {
                capacities.push(strings.text.capacity());
            });
        }
        true
    })?;
    if !is_string_table(&document) {
        return Err(errors::invalid("expected an OOXML shared-string table"));
    }
    if mixed_namespace {
        return Err(errors::unsupported(
            "shared-string table mixes strict and transitional SpreadsheetML namespaces",
        ));
    }
    if hidden_string {
        return Err(errors::unsupported(
            "shared string outside the shared-string table",
        ));
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
    // The entries that cannot be decoded, in index order. Such an entry
    // holds no text.
    undecodable: Vec<UndecodableEntry>,
    // Each distinct decoding error once, so that a table of many damaged
    // entries costs little more than their indexes. `Error` is not
    // `RefUnwindSafe` only because an `io::Error` can box any source. A kept
    // error never changes and is never an I/O error, which `push_with`
    // returns instead, so the table and workbook stay unwind-safe.
    errors: Vec<AssertUnwindSafe<Error>>,
}

// An entry of a shared-string table that cannot be decoded.
#[derive(Debug)]
struct UndecodableEntry {
    // The entry's index in the table.
    index: u32,
    // The position of the entry's error in `SharedStrings::errors`.
    error: u32,
}

impl SharedStrings {
    /// The string at `index`, or the error that kept that entry from being
    /// decoded. `None` past the end of the table.
    pub(crate) fn get(&self, index: usize) -> Option<StdResult<&str, &Error>> {
        let end = *self.ends.get(index)? as usize;
        if let Ok(position) = self
            .undecodable
            .binary_search_by_key(&index, |entry| entry.index as usize)
        {
            let error = &self.errors[self.undecodable[position].error as usize];
            return Some(Err(&error.0));
        }
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| self.ends[previous] as usize);
        // Each offset ends a whole string, so it is a character boundary.
        Some(Ok(&self.text[start..end]))
    }

    // Records the next entry, which `decode` appends to the table's text, so
    // that no entry needs a string of its own. An entry that cannot be
    // decoded is recorded as undecodable; `distinct` holds the position of
    // each error already recorded, by its display.
    fn push_with(
        &mut self,
        distinct: &mut HashMap<String, u32>,
        decode: impl FnOnce(&mut String) -> Result<()>,
    ) -> Result<()> {
        let start = self.text.len();
        match decode(&mut self.text) {
            Ok(()) => self.end_entry(),
            // Memory exhaustion is not an undecodable string entry.
            Err(error @ Error::Io(_)) => Err(error),
            Err(error) => {
                self.text.truncate(start);
                self.push_undecodable(error, distinct)
            }
        }
    }

    // Ends the entry whose text was appended last.
    fn end_entry(&mut self) -> Result<()> {
        let end = index_limit(self.text.len())?;
        self.ends.push(end);
        Ok(())
    }

    // Records the next entry as undecodable, with no text.
    fn push_undecodable(
        &mut self,
        error: Error,
        distinct: &mut HashMap<String, u32>,
    ) -> Result<()> {
        let index = index_limit(self.ends.len())?;
        let next = index_limit(self.errors.len())?;
        self.end_entry()?;
        let errors = &mut self.errors;
        let position = *distinct.entry(error.to_string()).or_insert_with(|| {
            errors.push(AssertUnwindSafe(error));
            next
        });
        self.undecodable.push(UndecodableEntry {
            index,
            error: position,
        });
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
        .map_err(|_| errors::unsupported("shared-string table exceeds the 4 GiB index limit"))
}

#[cfg(test)]
impl<'text> FromIterator<&'text str> for SharedStrings {
    fn from_iter<I: IntoIterator<Item = &'text str>>(strings: I) -> Self {
        let mut table = Self::default();
        for text in strings {
            table
                .push_with(&mut HashMap::new(), |output| append_text(output, text))
                .unwrap();
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

fn is_value_payload(index: &WorksheetIndex, element: &Element) -> bool {
    element.is(index.source(), index.namespace, "v")
        || element.is(index.source(), index.namespace, "is")
}

fn multiple_value_payloads() -> Error {
    errors::invalid("cell has multiple value payloads")
}

// The cell's `v` or `is` payload, if it has one.
pub(super) fn value_payload(index: &WorksheetIndex, cell: &Element) -> Result<Option<usize>> {
    let elements = &index.elements;
    errors::unique(
        cell.children(elements)
            .filter(|&child| is_value_payload(index, &elements[child])),
        multiple_value_payloads,
    )
}

// Decodes the cell's payload as reads see it; no-op detection compares an
// edit against the same value.
pub(super) fn decode_payload(
    index: &WorksheetIndex,
    cell: &Element,
    payload_index: usize,
    date1904: bool,
) -> Result<StoredValue> {
    let elements = &index.elements;
    let source = index.source();
    let namespace = index.namespace;
    let payload = &elements[payload_index];
    let kind = cell
        .attribute(&index.attributes, "t")
        .map_or("n", |attribute| attribute.value);
    if kind == "inlineStr" {
        if !payload.is(source, namespace, "is") {
            return Err(errors::invalid(
                "inline string cell without an inline payload",
            ));
        }
        let text = rich_text(source, elements, payload_index, namespace)?;
        return Ok(StoredValue::Value(CellValue::Text(text)));
    }
    if !payload.is(source, namespace, "v") {
        return Err(errors::invalid("non-inline cell has an inline payload"));
    }
    let text = element_text(source, payload)?;
    // Values that their cell type cannot hold make the workbook malformed;
    // only valid ISO 8601 dates outside the supported forms are unsupported.
    let value = match kind {
        "n" if xml::trim_whitespace(&text).is_empty() => CellValue::Blank,
        "n" => {
            let number: f64 = xml::trim_whitespace(&text)
                .parse()
                .map_err(|_| errors::invalid("invalid numeric cell value"))?;
            if !number.is_finite() {
                return Err(errors::invalid("non-finite numeric cell value"));
            }
            CellValue::Number(number)
        }
        "b" => CellValue::Bool(match xml::trim_whitespace(&text) {
            "0" | "false" => false,
            "1" | "true" => true,
            _ => return Err(errors::invalid("invalid boolean cell value")),
        }),
        "str" => CellValue::Text(xml::excel_text(&text)?),
        "e" => CellValue::Error(xml::excel_text(&text)?),
        // Dates read as the serial number Excel would store for them.
        "d" => {
            let serial = date::excel_serial(xml::trim_whitespace(&text), date1904).map_err(
                |error| match error {
                    DateError::Invalid => errors::invalid("invalid ISO 8601 date cell value"),
                    DateError::TimeZoneOffset => {
                        errors::unsupported("ISO 8601 date cell value with a time-zone offset")
                    }
                    DateError::BeforeEpoch => errors::unsupported(
                        "ISO 8601 date cell value before the start of the workbook's date system",
                    ),
                },
            )?;
            CellValue::Number(serial)
        }
        "s" => {
            return xml::trim_whitespace(&text)
                .parse::<usize>()
                .map(StoredValue::SharedString)
                .map_err(|_| errors::invalid("invalid shared-string index"));
        }
        _ => return Err(errors::invalid(format!("invalid cell value type {kind:?}"))),
    };
    Ok(StoredValue::Value(value))
}

// A cell's style, formula and stored value.
struct StoredCell {
    style_index: Option<u32>,
    formula: Option<String>,
    value: StoredValue,
}

// The cell's style index, if it has one. Inlined, as it was inside
// `stored_cell`, which every read of a cell runs.
#[inline]
pub(super) fn style_index(index: &WorksheetIndex, cell: &Element) -> Result<Option<u32>> {
    cell.attribute(&index.attributes, "s")
        .map(|attribute| {
            xml::trim_whitespace(attribute.value)
                .parse()
                .map_err(|_| errors::invalid("invalid cell style index"))
        })
        .transpose()
}

// The cell's style, formula and stored value, read in that order.
fn stored_cell(index: &WorksheetIndex, cell: &Element, date1904: bool) -> Result<StoredCell> {
    let elements = &index.elements;
    let style_index = style_index(index, cell)?;
    // Locate the formula and payload in one pass. A malformed formula takes
    // precedence even when repeated payloads appear earlier in the XML.
    let mut formula = None;
    let mut payload = None;
    let mut multiple_values = false;
    for child in cell.children(elements) {
        let element = &elements[child];
        if is_value_payload(index, element) {
            multiple_values = payload.is_some();
            payload = Some(child);
        } else if element.is(index.source(), index.namespace, "f") {
            if formula.is_some() {
                return Err(errors::invalid("cell has multiple formulas"));
            }
            formula = Some(element);
        }
    }
    let formula = formula
        .map(|formula| element_text(index.source(), formula).and_then(owned_text))
        .transpose()?;
    if multiple_values {
        return Err(multiple_value_payloads());
    }
    let value = match payload {
        Some(payload) => decode_payload(index, cell, payload, date1904)?,
        None => StoredValue::Value(CellValue::Blank),
    };
    Ok(StoredCell {
        style_index,
        formula,
        value,
    })
}

#[inline]
pub(super) fn cell_content<'a>(
    index: &WorksheetIndex,
    address: CellRef,
    element: Option<usize>,
    shared_strings: &mut impl FnMut() -> Result<Option<&'a SharedStrings>>,
    date1904: bool,
) -> StdResult<CellContent, ReadError> {
    let Some(cell_index) = element else {
        return Ok(CellContent {
            value: CellValue::Blank,
            formula: None,
            style_index: None,
        });
    };
    indexed_cell_content(index, address, cell_index, shared_strings, date1904)
}

// The address and element come from one entry in the validated cell index.
// Sequential reads can use it directly without searching that index again.
pub(super) fn indexed_cell_content<'a>(
    index: &WorksheetIndex,
    address: CellRef,
    cell_index: usize,
    shared_strings: &mut impl FnMut() -> Result<Option<&'a SharedStrings>>,
    date1904: bool,
) -> StdResult<CellContent, ReadError> {
    let StoredCell {
        style_index,
        formula,
        value,
    } = stored_cell(index, &index.elements[cell_index], date1904)
        .map_err(|error| error.context(address))?;
    let value = match value {
        StoredValue::Value(value) => value,
        StoredValue::SharedString(position) => {
            // A table that exists but cannot be loaded concerns that part;
            // a missing table or index is the cell's problem.
            let strings = shared_strings()
                .map_err(ReadError::SharedStrings)?
                .ok_or_else(|| {
                    errors::invalid(format!(
                        "{address}: shared-string cell, but the workbook has no shared-string table"
                    ))
                })?;
            match strings.get(position) {
                Some(Ok(text)) => CellValue::Text(xml::copy_text(text)?),
                // Only the cells that use an undecodable entry fail.
                Some(Err(error)) => {
                    return Err(ReadError::UndecodableString {
                        index: position,
                        error: error.duplicate(),
                    });
                }
                None => {
                    return Err(errors::invalid(format!(
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
    index: &WorksheetIndex,
    cells: &[CellRef],
    mut shared_strings: impl FnMut() -> Result<Option<&'a SharedStrings>>,
    date1904: bool,
) -> StdResult<Vec<CellContent>, ReadError> {
    if cells.is_empty() {
        return Ok(Vec::new());
    }
    // Reserve the result slots fallibly. Text and formula copies also use
    // fallible growth; the cell-count limit does not cap their total bytes.
    let mut contents = Vec::new();
    contents
        .try_reserve_exact(cells.len())
        .map_err(|_| Error::Io(io::ErrorKind::OutOfMemory.into()))?;
    if cells.windows(2).all(|pair| pair[0] <= pair[1]) {
        let mut cursor = index.cell_cursor();
        for &cell in cells {
            contents.push(cell_content(
                index,
                cell,
                cursor.cell(cell),
                &mut shared_strings,
                date1904,
            )?);
        }
    } else {
        for &cell in cells {
            contents.push(cell_content(
                index,
                cell,
                index.cell(cell),
                &mut shared_strings,
                date1904,
            )?);
        }
    }
    Ok(contents)
}
