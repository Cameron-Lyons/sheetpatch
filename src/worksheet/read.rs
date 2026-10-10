use super::index::WorksheetIndex;
use super::parse::{Element, NamespaceId, parse};
use super::{decoded, unique, unsupported};
use crate::xml::{excel_text, trim_whitespace as trim_xml_whitespace};
use crate::{CellContent, CellRef, CellValue, Error, Result};
use quick_xml::events::Event;

// XML normalizes literal CR/CRLF before expanding character references. Doing
// that in the opposite order would silently turn a deliberate &#13; into LF.
pub(super) fn element_text(xml: &[u8], element: &Element) -> Result<String> {
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

pub(super) fn rich_text(
    xml: &[u8],
    elements: &[Element],
    index: usize,
    namespace: NamespaceId,
) -> Result<String> {
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
    let document = parse(xml)?;
    let root = &document.elements[0];
    let namespace = root.namespace;
    if root.local_name() != "sst" || !document.is_spreadsheet_namespace(namespace) {
        return Err(unsupported("expected an OOXML shared-string table"));
    }
    let elements = document.elements;
    let root = &elements[0];
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

pub(super) fn cell_content<'a>(
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
    let attributes = &index.attributes;
    let namespace = index.namespace;
    let cell = &elements[cell_index];
    let style_index = cell
        .attr(attributes, "s")
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
            .attr(attributes, "t")
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

pub(super) fn read_cells<'a>(
    xml: &[u8],
    index: &WorksheetIndex,
    cells: &[CellRef],
    mut shared_strings: impl FnMut() -> Result<&'a [String]>,
) -> Result<Vec<CellContent>> {
    if cells.is_empty() {
        return Ok(Vec::new());
    }
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
        .map(|&cell| cell_content(xml, index, cell, &mut load_strings))
        .collect()
}
