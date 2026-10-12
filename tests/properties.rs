//! Deterministic randomized properties for edits, reads, and preservation.
//!
//! Every test runs a fixed number of cases from a fixed seed, using a small
//! xorshift64* generator, so a failure reproduces exactly. Failure messages name
//! the test, seed, and case. Set `SHEETPATCH_PROPERTY_SEED` (decimal or `0x`
//! hexadecimal) to explore other inputs, and `SHEETPATCH_PROPERTY_CASES` to
//! change the number of cases each test runs.
//!
//! Workbooks are generated together with a model of every cell's expected read
//! (value, formula, and style), the worksheet's protected cells, and the exact
//! XML source of each cell, so edits can be checked against an independent
//! oracle rather than against the crate's own parser.
mod common;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    panic::{self, AssertUnwindSafe},
};

use common::{
    ZipLayout, central_directory_offset, data_descriptor_len, payload, with_data_descriptors,
    write_zip, zip_entries,
};
use sheetpatch::{CellContent, CellEdit, CellRange, CellRef, CellValue, Error, Workbook};

const DEFAULT_SEED: u64 = 0x5EE7_5EED_2026_1011;

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const STRICT_NS: &str = "http://purl.oclc.org/ooxml/spreadsheetml/main";
const OFFICE_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const STRICT_OFFICE_NS: &str = "http://purl.oclc.org/ooxml/officeDocument/relationships";
const PACKAGE_NS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const CONTENT_TYPES_NS: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const VENDOR_NS: &str = "urn:sheetpatch:vendor";

const MAX_TEXT_UNITS: usize = 32_767;

const ERROR_CODES: [&str; 17] = [
    "#NULL!",
    "#DIV/0!",
    "#VALUE!",
    "#REF!",
    "#NAME?",
    "#NUM!",
    "#N/A",
    "#GETTING_DATA",
    "#SPILL!",
    "#CALC!",
    "#FIELD!",
    "#BLOCKED!",
    "#CONNECT!",
    "#BUSY!",
    "#UNKNOWN!",
    "#PYTHON!",
    "#EXTERNAL!",
];

// ---------------------------------------------------------------- harness

/// xorshift64*, seeded through splitmix64 so nearby seeds diverge at once.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Self((z ^ (z >> 31)) | 1)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        assert!(bound > 0, "empty random range");
        (self.next_u64() % bound as u64) as usize
    }

    fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        self.next_u64() % denominator < numerator
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    fn inclusive(&mut self, low: u32, high: u32) -> u32 {
        low + (self.next_u64() % u64::from(high - low + 1)) as u32
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            items.swap(index, self.below(index + 1));
        }
    }
}

/// Identifies one generated case in every failure message.
#[derive(Clone, Copy)]
struct Case {
    test: &'static str,
    seed: u64,
    index: usize,
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{} SHEETPATCH_PROPERTY_SEED={:#x} case {}]",
            self.test, self.seed, self.index
        )
    }
}

fn env_number(name: &str) -> Option<u64> {
    let text = std::env::var(name).ok()?;
    let text = text.trim();
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(&hex.replace('_', ""), 16),
        None => text.replace('_', "").parse(),
    };
    Some(parsed.unwrap_or_else(|_| panic!("{name} must be a decimal or 0x-prefixed u64: {text:?}")))
}

/// Runs `body` for each case with an independent generator. A panic anywhere,
/// including inside the crate, is reported with the seed and case number.
fn run_cases(test: &'static str, default_cases: usize, mut body: impl FnMut(Case, &mut Rng)) {
    let seed = env_number("SHEETPATCH_PROPERTY_SEED").unwrap_or(DEFAULT_SEED);
    let cases = env_number("SHEETPATCH_PROPERTY_CASES").map_or(default_cases, |n| n as usize);
    let stream = test.bytes().fold(0xCBF2_9CE4_8422_2325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
    });
    for index in 0..cases {
        let case = Case { test, seed, index };
        let mut rng = Rng::new(seed ^ stream ^ (index as u64).wrapping_mul(0xD605_BBB5_8C8A_BBFD));
        if let Err(panic) = panic::catch_unwind(AssertUnwindSafe(|| body(case, &mut rng))) {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("non-text panic payload");
            if message.contains(&case.to_string()) {
                panic::resume_unwind(panic);
            }
            panic!("{case} failed: {message}");
        }
    }
}

trait OrFail<T> {
    fn or_fail(self, case: Case, what: impl fmt::Display) -> T;
}

impl<T, E: fmt::Debug> OrFail<T> for Result<T, E> {
    #[track_caller]
    fn or_fail(self, case: Case, what: impl fmt::Display) -> T {
        match self {
            Ok(value) => value,
            Err(error) => panic!("{case} {what}: {error:?}"),
        }
    }
}

/// Debug output, truncated so 32,767-unit strings keep messages readable.
fn brief(value: &impl fmt::Debug) -> String {
    let text = format!("{value:?}");
    if text.len() <= 240 {
        return text;
    }
    let mut end = 200;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... ({} bytes)", &text[..end], text.len())
}

fn cell(row: u32, column: u16) -> CellRef {
    CellRef::new(row, column).expect("generated address is within the grid")
}

// ---------------------------------------------------------------- values

/// Text pieces combined into written values: XML specials, whitespace, line
/// breaks, invisible characters, literal Excel escape tokens, and formulas.
const TEXT_PIECES: &[&str] = &[
    "plain",
    "Total",
    " ",
    "  lead",
    "trail  ",
    "&",
    "<",
    ">",
    "\"",
    "'",
    "&amp;",
    "<b>x</b>",
    "]]>",
    "\r",
    "\n",
    "\r\n",
    "\t",
    "\u{feff}",
    "\u{85}",
    "\u{2028}",
    "\u{a0}",
    "\u{fffd}",
    "\u{1f600}",
    "雪",
    "e\u{301}",
    "_x0041_",
    "_X0041_",
    "_x005F_",
    "_x005F_x0041_",
    "_x0001",
    "_x",
    "_x00",
    "_xD83D_",
    "_",
    "__",
    "=SUM(A1)",
    "#N/A",
    "1e5",
    "-0",
    "TRUE",
];

/// A character that XML 1.0 cannot carry: U+0000-U+001F, U+FFFE, or U+FFFF.
/// Tab, LF, and CR are included too; they are valid but handled specially.
fn control_character(rng: &mut Rng) -> char {
    match rng.below(10) {
        0 => '\u{fffe}',
        1 => '\u{ffff}',
        _ => char::from_u32(rng.below(0x20) as u32).unwrap(),
    }
}

fn text_fragment(rng: &mut Rng) -> String {
    match rng.below(10) {
        0 | 1 => control_character(rng).to_string(),
        // A literal escape prefix followed by a character that must itself
        // be escaped, which would otherwise read back as one token.
        2 => format!(
            "_{}{:04X}{}",
            rng.pick(&["x", "X"]),
            rng.below(0x1_0000),
            control_character(rng)
        ),
        // A literal, well-formed escape token in either case.
        3 => format!("_x{:04x}_", rng.below(0x1_0000)),
        _ => rng.pick(TEXT_PIECES).to_string(),
    }
}

/// Text of exactly 32,767 UTF-16 code units, built from adversarial pieces.
fn longest_text(rng: &mut Rng) -> String {
    // Fragments are never empty, so the unit has at least one code unit.
    let unit: String = (0..1 + rng.below(3)).map(|_| text_fragment(rng)).collect();
    let unit_len = unit.encode_utf16().count();
    let repeats = if rng.chance(1, 2) {
        MAX_TEXT_UNITS / unit_len
    } else {
        1
    };
    let mut text = unit.repeat(repeats);
    text.extend(std::iter::repeat_n(
        'z',
        MAX_TEXT_UNITS - repeats * unit_len,
    ));
    assert_eq!(text.encode_utf16().count(), MAX_TEXT_UNITS);
    text
}

fn text_value(rng: &mut Rng, allow_long: bool) -> String {
    match rng.below(40) {
        0 | 1 => String::new(),
        2 if allow_long => longest_text(rng),
        _ => {
            let mut text = String::new();
            if rng.chance(1, 6) {
                text.push('\u{feff}');
            }
            for _ in 0..1 + rng.below(5) {
                text.push_str(&text_fragment(rng));
            }
            text
        }
    }
}

fn number_value(rng: &mut Rng) -> f64 {
    match rng.below(5) {
        0 => loop {
            let number = f64::from_bits(rng.next_u64());
            if number.is_finite() {
                break number;
            }
        },
        1 => {
            let magnitude = (rng.next_u64() >> 11) as f64 / (1u64 << rng.below(60)) as f64;
            if rng.chance(1, 2) {
                -magnitude
            } else {
                magnitude
            }
        }
        _ => *rng.pick(&[
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.5,
            0.1 + 0.2,
            1e21,
            -1e21,
            1e-7,
            123_456_789_012_345_680_000.0,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            f64::EPSILON,
            // The smallest and largest subnormal numbers.
            f64::from_bits(1),
            f64::from_bits(0x000F_FFFF_FFFF_FFFF),
            -f64::from_bits(1),
            45_306.75,
            1e300,
            -1e-300,
        ]),
    }
}

fn random_value(rng: &mut Rng, allow_long: bool) -> CellValue {
    match rng.below(20) {
        0..=7 => CellValue::Text(text_value(rng, allow_long)),
        8..=12 => CellValue::Number(number_value(rng)),
        13 | 14 => CellValue::Bool(rng.chance(1, 2)),
        15 | 16 => CellValue::Error(rng.pick(&ERROR_CODES).to_string()),
        _ => CellValue::Blank,
    }
}

fn invalid_value(rng: &mut Rng) -> CellValue {
    match rng.below(9) {
        0 => CellValue::Number(f64::NAN),
        1 => CellValue::Number(f64::INFINITY),
        2 => CellValue::Number(f64::NEG_INFINITY),
        3 => CellValue::Text("a".repeat(MAX_TEXT_UNITS + 1)),
        4 => CellValue::Text("é".repeat(MAX_TEXT_UNITS + 1)),
        5 => CellValue::Text(format!("{}\u{1f600}", "z".repeat(MAX_TEXT_UNITS - 1))),
        _ => CellValue::Error(
            rng.pick(&[
                "#N/A!",
                "#n/a",
                "#DIV/0",
                "",
                "#FUTURE!",
                "#N/A\u{1}",
                " #N/A",
                "N/A",
                "#REF!\0",
            ])
            .to_string(),
        ),
    }
}

/// An independent statement of the documented value rules.
fn valid_value(value: &CellValue) -> bool {
    match value {
        CellValue::Number(number) => number.is_finite(),
        CellValue::Text(text) => text.encode_utf16().count() <= MAX_TEXT_UNITS,
        CellValue::Error(code) => ERROR_CODES.contains(&code.as_str()),
        _ => true,
    }
}

fn is_long(value: &CellValue) -> bool {
    matches!(value, CellValue::Text(text) if text.len() > 1_000)
}

// ---------------------------------------------------------------- the model

/// What a read of one cell must return.
#[derive(Clone, Debug, PartialEq)]
struct Expected {
    value: CellValue,
    formula: Option<String>,
    style: Option<u32>,
}

impl Expected {
    fn value(value: CellValue) -> Self {
        Self {
            value,
            formula: None,
            style: None,
        }
    }

    fn formula(value: CellValue, formula: &str) -> Self {
        Self {
            value,
            formula: Some(formula.to_owned()),
            style: None,
        }
    }

    fn matches(&self, content: &CellContent) -> bool {
        content.value == self.value
            && content.formula == self.formula
            && content.style_index == self.style
    }
}

#[derive(Clone, Copy, Default)]
struct RowFormat {
    style: Option<u32>,
    custom: bool,
}

/// A generated worksheet and the oracle for reading and editing it.
#[derive(Clone)]
struct SheetModel {
    name: String,
    path: String,
    /// Every cell element in the worksheet, by address.
    cells: BTreeMap<CellRef, Expected>,
    rows: BTreeMap<u32, RowFormat>,
    /// Column default styles as `(min, max, style)`; style 0 is no style.
    columns: Vec<(u16, u16, u32)>,
    formula_cells: BTreeSet<CellRef>,
    formula_ranges: Vec<CellRange>,
    merges: Vec<CellRange>,
    /// Cells with value metadata, which may only be "rewritten" unchanged.
    metadata_cells: BTreeSet<CellRef>,
    /// Cells whose value payload holds a comment, which no edit may discard.
    opaque_cells: BTreeSet<CellRef>,
    /// The generated grid's last row and column, before far-away cells.
    extent: (u32, u16),
    xml: String,
    /// Exact source of each cell element and row start tag.
    cell_sources: BTreeMap<CellRef, String>,
    row_sources: BTreeMap<u32, String>,
    /// Exact source of all markup outside `sheetData` except the dimension.
    fixed_sources: Vec<String>,
    /// Cell and row extension lists followed by their parent's end tag, as
    /// the schema requires them to stay last.
    tail_sources: Vec<String>,
}

impl SheetModel {
    fn expected(&self, cell: CellRef) -> Expected {
        self.cells
            .get(&cell)
            .cloned()
            .unwrap_or_else(|| Expected::value(CellValue::Blank))
    }

    fn protected(&self, cell: CellRef) -> bool {
        self.guarded(cell) || self.opaque_cells.contains(&cell)
    }

    /// Formula cells, cells in formula ranges, and merged followers: the
    /// guards checked for a whole batch before any cell is patched.
    fn guarded(&self, cell: CellRef) -> bool {
        self.formula_cells.contains(&cell)
            || self.formula_ranges.iter().any(|range| range.contains(cell))
            || self
                .merges
                .iter()
                .any(|range| range.contains(cell) && cell != range.first())
    }

    fn accepts(&self, cell: CellRef, value: &CellValue) -> bool {
        !self.protected(cell)
            && (!self.metadata_cells.contains(&cell) || self.expected(cell).value == *value)
    }

    fn protected_cells(&self) -> Vec<CellRef> {
        let mut cells: BTreeSet<CellRef> = &self.formula_cells | &self.opaque_cells;
        for range in self.formula_ranges.iter().chain(&self.merges) {
            cells.extend(range.cells().filter(|&cell| self.protected(cell)));
        }
        cells.into_iter().collect()
    }

    /// A new cell takes its row's style when the row has a custom format, and
    /// otherwise its column's default style. Style zero needs no attribute.
    fn default_style(&self, cell: CellRef) -> Option<u32> {
        if let Some(row) = self.rows.get(&cell.row())
            && row.custom
        {
            return row.style.filter(|&style| style != 0);
        }
        self.columns
            .iter()
            .find(|&&(min, max, _)| (min..=max).contains(&cell.column()))
            .map(|&(_, _, style)| style)
            .filter(|&style| style != 0)
    }

    fn apply(&mut self, cell: CellRef, value: &CellValue) {
        if let Some(existing) = self.cells.get_mut(&cell) {
            assert!(existing.formula.is_none(), "formula cells are protected");
            existing.value = value.clone();
        } else if !value.is_blank() {
            let style = self.default_style(cell);
            self.cells.insert(
                cell,
                Expected {
                    value: value.clone(),
                    formula: None,
                    style,
                },
            );
            self.rows.entry(cell.row()).or_default();
        }
    }
}

// ---------------------------------------------------------------- workbook generation

/// Worksheet or shared-string `<t>` contents and the text they decode to.
const SOURCE_TEXTS: &[(&str, &str)] = &[
    ("Revenue", "Revenue"),
    (
        "a &amp; b &lt;c&gt; &quot;d&quot; &apos;e&apos;",
        "a & b <c> \"d\" 'e'",
    ),
    ("Page_x000C_Break_x0009_", "Page\u{c}Break\t"),
    ("_x005F_x0041_ stays literal", "_x0041_ stays literal"),
    ("_x0041__X0042_", "AB"),
    ("_xD83D__xDE00_ pair", "\u{1f600} pair"),
    ("line&#10;break&#13;", "line\nbreak\r"),
    ("crlf\r\nand cr\rbecome lf", "crlf\nand cr\nbecome lf"),
    ("tab\there", "tab\there"),
    ("雪 \u{1f600} &#x1F600;", "雪 \u{1f600} \u{1f600}"),
    ("\u{feff}bom", "\u{feff}bom"),
    ("  padded  ", "  padded  "),
    ("<![CDATA[x < y & z]]>", "x < y & z"),
    ("be<!-- note -->fore", "before"),
    ("=SUM(A1)", "=SUM(A1)"),
    ("", ""),
];

/// Numeric `<v>` spellings and their values.
const NUMBER_SOURCES: &[(&str, f64)] = &[
    ("1", 1.0),
    ("-2.5", -2.5),
    ("1.5E-3", 0.0015),
    (" 42 ", 42.0),
    ("0.1", 0.1),
    ("1e21", 1e21),
    ("-0", -0.0),
    ("+1", 1.0),
    ("&#49;&#x32;", 12.0),
    (".5", 0.5),
    ("4.9406564584124654E-324", 5e-324),
    ("1.7976931348623157E+308", f64::MAX),
];

const DATE_SOURCES: &[(&str, f64)] = &[
    ("2024-01-15", 45_306.0),
    ("2024-01-15T18:00Z", 45_306.75),
    ("1900-03-01", 61.0),
    ("12:00", 0.5),
];

/// Element names in the worksheet's SpreadsheetML namespace.
#[derive(Clone, Copy)]
struct Names {
    prefix: &'static str,
}

impl Names {
    fn el(self, local: &str) -> String {
        format!("{}{local}", self.prefix)
    }

    /// The declaration binding this prefix (or the default namespace).
    fn declaration(self, namespace: &str) -> String {
        match self.prefix.strip_suffix(':') {
            Some(prefix) => format!("xmlns:{prefix}=\"{namespace}\""),
            None => format!("xmlns=\"{namespace}\""),
        }
    }
}

fn escape_attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn t_element(rng: &mut Rng, names: Names, source: &str) -> String {
    let t = names.el("t");
    if source.is_empty() && rng.chance(1, 2) {
        return format!("<{t}/>");
    }
    let preserve = source.starts_with(' ') || source.ends_with(' ') || rng.chance(1, 3);
    let space = if preserve {
        " xml:space=\"preserve\""
    } else {
        ""
    };
    format!("<{t}{space}>{source}</{t}>")
}

/// Rich text for an `<is>` or `<si>` element: a plain `<t>` or formatted runs,
/// optionally followed by phonetic runs that are not part of the cell text.
fn rich_text(rng: &mut Rng, names: Names) -> (String, String) {
    let mut xml = String::new();
    let mut text = String::new();
    if rng.chance(1, 2) {
        let (source, decoded) = *rng.pick(SOURCE_TEXTS);
        xml.push_str(&t_element(rng, names, source));
        text.push_str(decoded);
    } else {
        for _ in 0..1 + rng.below(3) {
            let (source, decoded) = *rng.pick(SOURCE_TEXTS);
            let properties = if rng.chance(1, 2) {
                format!(
                    "<{rpr}><{b}/><{sz} val=\"11\"/><{color} rgb=\"FFFF0000\"/><{font} val=\"Calibri\"/></{rpr}>",
                    rpr = names.el("rPr"),
                    b = names.el("b"),
                    sz = names.el("sz"),
                    color = names.el("color"),
                    font = names.el("rFont"),
                )
            } else {
                String::new()
            };
            let r = names.el("r");
            let t = t_element(rng, names, source);
            xml.push_str(&format!("<{r}>{properties}{t}</{r}>"));
            text.push_str(decoded);
        }
    }
    if rng.chance(1, 3) {
        let rph = names.el("rPh");
        for _ in 0..1 + rng.below(2) {
            let t = t_element(rng, names, "ふりがな");
            xml.push_str(&format!("<{rph} sb=\"0\" eb=\"1\">{t}</{rph}>"));
        }
        xml.push_str(&format!(
            "<{} fontId=\"1\" type=\"noConversion\"/>",
            names.el("phoneticPr")
        ));
    }
    (xml, text)
}

struct SharedStrings {
    xml: String,
    texts: Vec<String>,
}

fn shared_strings(rng: &mut Rng, namespace: &str) -> SharedStrings {
    let names = Names {
        prefix: if rng.chance(1, 4) { "ss:" } else { "" },
    };
    let si = names.el("si");
    let count = 1 + rng.below(8);
    let mut items = String::new();
    let mut texts = Vec::new();
    for _ in 0..count {
        let (xml, text) = rich_text(rng, names);
        items.push_str(&format!("<{si}>{xml}</{si}>"));
        texts.push(text);
    }
    let sst = names.el("sst");
    SharedStrings {
        xml: format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<{sst} {} count=\"{count}\" uniqueCount=\"{count}\">{items}</{sst}>",
            names.declaration(namespace)
        ),
        texts,
    }
}

/// One generated cell element before its address and style are assigned.
struct CellSpec {
    kind: Option<&'static str>,
    children: Vec<String>,
    expected: Expected,
    metadata: bool,
    opaque: bool,
    formula_range: Option<CellRange>,
}

impl CellSpec {
    fn new(kind: Option<&'static str>, children: Vec<String>, expected: Expected) -> Self {
        Self {
            kind,
            children,
            expected,
            metadata: false,
            opaque: false,
            formula_range: None,
        }
    }
}

fn value_element(names: Names, text: &str) -> String {
    let v = names.el("v");
    format!("<{v}>{text}</{v}>")
}

fn formula_element(names: Names, attributes: &str, text: &str) -> String {
    let f = names.el("f");
    if text.is_empty() {
        format!("<{f}{attributes}/>")
    } else {
        format!("<{f}{attributes}>{text}</{f}>")
    }
}

fn plain_cell(rng: &mut Rng, names: Names, strings: &[String]) -> CellSpec {
    let v = |text: &str| vec![value_element(names, text)];
    match rng.below(13) {
        0 | 1 => {
            let (source, number) = *rng.pick(NUMBER_SOURCES);
            let kind = rng.chance(1, 6).then_some("n");
            CellSpec::new(kind, v(source), Expected::value(CellValue::Number(number)))
        }
        2 | 3 if !strings.is_empty() => {
            let index = rng.below(strings.len());
            let source = if rng.chance(1, 5) {
                format!(" {index} ")
            } else {
                index.to_string()
            };
            let text = CellValue::Text(strings[index].clone());
            CellSpec::new(Some("s"), v(&source), Expected::value(text))
        }
        2..=5 => {
            let is = names.el("is");
            let (payload, text) = if rng.chance(1, 12) {
                (format!("<{is}/>"), String::new())
            } else {
                let (xml, text) = rich_text(rng, names);
                (format!("<{is}>{xml}</{is}>"), text)
            };
            let mut spec = CellSpec::new(
                Some("inlineStr"),
                vec![payload],
                Expected::value(CellValue::Text(text)),
            );
            // A comment in the old value is markup an edit would discard.
            spec.opaque = spec.children[0].contains("<!--");
            spec
        }
        6 => {
            let (source, value) = *rng.pick(&[
                ("1", true),
                ("0", false),
                ("true", true),
                ("false", false),
                (" 1 ", true),
            ]);
            CellSpec::new(
                Some("b"),
                v(source),
                Expected::value(CellValue::Bool(value)),
            )
        }
        7 => {
            let code = if rng.chance(1, 6) {
                "#FUTURE!"
            } else {
                rng.pick(&ERROR_CODES)
            };
            let error = CellValue::Error(code.to_owned());
            CellSpec::new(Some("e"), v(code), Expected::value(error))
        }
        8 => {
            let (source, serial) = *rng.pick(DATE_SOURCES);
            CellSpec::new(
                Some("d"),
                v(source),
                Expected::value(CellValue::Number(serial)),
            )
        }
        9 => CellSpec::new(None, Vec::new(), Expected::value(CellValue::Blank)),
        10 => {
            let empty = if rng.chance(1, 2) {
                format!("<{}/>", names.el("v"))
            } else {
                value_element(names, "")
            };
            CellSpec::new(None, vec![empty], Expected::value(CellValue::Blank))
        }
        _ => {
            let number = number_value(rng);
            let source = if rng.chance(1, 3) {
                format!("{number:E}")
            } else {
                number.to_string()
            };
            CellSpec::new(None, v(&source), Expected::value(CellValue::Number(number)))
        }
    }
}

fn formula_cell(rng: &mut Rng, names: Names) -> CellSpec {
    let f = |text| formula_element(names, "", text);
    let v = |text| value_element(names, text);
    match rng.below(5) {
        0 => CellSpec::new(
            None,
            vec![f("SUM(A1:B1)"), v("3")],
            Expected::formula(CellValue::Number(3.0), "SUM(A1:B1)"),
        ),
        1 => CellSpec::new(
            Some("str"),
            vec![f("\"x\"&amp;CHAR(65)"), v("x_x0041_")],
            Expected::formula(CellValue::Text("xA".into()), "\"x\"&CHAR(65)"),
        ),
        2 => CellSpec::new(
            Some("b"),
            vec![f("ISNUMBER(A1)"), v("1")],
            Expected::formula(CellValue::Bool(true), "ISNUMBER(A1)"),
        ),
        3 => CellSpec::new(
            Some("e"),
            vec![f("1/0"), v("#DIV/0!")],
            Expected::formula(CellValue::Error("#DIV/0!".into()), "1/0"),
        ),
        _ => CellSpec::new(
            None,
            vec![f("NOW()")],
            Expected::formula(CellValue::Blank, "NOW()"),
        ),
    }
}

fn metadata_cell(names: Names) -> CellSpec {
    let mut spec = CellSpec::new(
        Some("e"),
        vec![value_element(names, "#VALUE!")],
        Expected::value(CellValue::Error("#VALUE!".into())),
    );
    spec.metadata = true;
    spec
}

/// A rectangle anchored in the grid, at most three cells on each side.
fn small_range(rng: &mut Rng, rows: u32, columns: u16) -> CellRange {
    let first = cell(
        rng.inclusive(1, rows),
        rng.inclusive(1, u32::from(columns)) as u16,
    );
    let last = cell(
        first.row() + rng.inclusive(0, 2),
        first.column() + rng.inclusive(0, 2) as u16,
    );
    CellRange::new(first, last)
}

/// Shared, array, and data-table formula groups and merged ranges. Each claims
/// a free rectangle, so the protections never overlap one another.
fn place_ranges(
    rng: &mut Rng,
    crowded: bool,
    names: Names,
    grid: &mut BTreeMap<CellRef, CellSpec>,
    rows: u32,
    columns: u16,
) -> Vec<CellRange> {
    let mut reserved: BTreeSet<CellRef> = BTreeSet::new();
    let mut merges = Vec::new();
    let attempts = if crowded { 48 } else { 6 };
    for group in 0..rng.below(attempts) {
        let range = small_range(rng, rows, columns);
        if range.cells().any(|cell| reserved.contains(&cell)) {
            continue;
        }
        reserved.extend(range.cells());
        let reference = range.to_string();
        let (anchor, members) = (range.first(), range.cells().skip(1));
        let mut formula = |attributes: String, text: &str, cached: f64| {
            let mut spec = CellSpec::new(
                None,
                vec![
                    formula_element(names, &attributes, text),
                    value_element(names, &cached.to_string()),
                ],
                Expected::formula(CellValue::Number(cached), text),
            );
            spec.formula_range = Some(range);
            grid.insert(anchor, spec);
        };
        match rng.below(8) {
            0 | 1 => {
                formula(
                    format!(" t=\"shared\" ref=\"{reference}\" si=\"{group}\""),
                    "A1*2",
                    2.0,
                );
                // Followers carry only the group index.
                for member in members {
                    if rng.chance(2, 3) {
                        let attributes = format!(" t=\"shared\" si=\"{group}\"");
                        let spec = CellSpec::new(
                            None,
                            vec![
                                formula_element(names, &attributes, ""),
                                value_element(names, "4"),
                            ],
                            Expected::formula(CellValue::Number(4.0), ""),
                        );
                        grid.insert(member, spec);
                    } else {
                        grid.remove(&member);
                    }
                }
            }
            2 => {
                formula(
                    format!(" t=\"array\" ref=\"{reference}\""),
                    "TRANSPOSE(A1:C3)",
                    1.0,
                );
                // Other array members hold plain cached values.
                for member in members {
                    if rng.chance(1, 2) {
                        let spec = CellSpec::new(
                            None,
                            vec![value_element(names, "7")],
                            Expected::value(CellValue::Number(7.0)),
                        );
                        grid.insert(member, spec);
                    } else {
                        grid.remove(&member);
                    }
                }
            }
            3 => formula(
                format!(" t=\"dataTable\" ref=\"{reference}\" dt2D=\"0\" dtr=\"0\" r1=\"A1\""),
                "",
                5.0,
            ),
            _ if range.cell_count() > 1 => {
                // Merged followers are usually absent or empty formatted cells.
                for member in members {
                    match rng.below(3) {
                        0 => {
                            grid.remove(&member);
                        }
                        1 => {
                            let blank = Expected::value(CellValue::Blank);
                            grid.insert(member, CellSpec::new(None, Vec::new(), blank));
                        }
                        _ => {}
                    }
                }
                merges.push(range);
            }
            _ => {}
        }
    }
    merges
}

/// An attribute in any valid XML spelling: either quote, and optional
/// whitespace around `=` or line breaks before the name.
fn quoted(rng: &mut Rng, name: &str, value: &str) -> String {
    let space = if rng.chance(1, 8) { "\n\t" } else { " " };
    let equals = if rng.chance(1, 8) { " = " } else { "=" };
    let quote = if rng.chance(1, 5) { '\'' } else { '"' };
    format!("{space}{name}{equals}{quote}{value}{quote}")
}

/// The end of an empty element, with or without a space before it.
fn empty_end(rng: &mut Rng) -> &'static str {
    if rng.chance(1, 4) { " />" } else { "/>" }
}

fn generate_sheet(
    rng: &mut Rng,
    crowded: bool,
    name: &str,
    path: &str,
    strict: bool,
    strings: &[String],
) -> SheetModel {
    let namespace = if strict { STRICT_NS } else { MAIN_NS };
    let prefixes: [&'static str; 4] = ["", "x:", "s:", "main:"];
    let names = Names {
        prefix: prefixes[if rng.chance(1, 3) {
            rng.inclusive(1, 3) as usize
        } else {
            0
        }],
    };
    // Crowded sheets are larger and hold many protected ranges, so that large
    // batches are guarded by the sweep rather than direct membership checks.
    let (rows, columns) = if crowded {
        (rng.inclusive(10, 24), rng.inclusive(8, 16) as u16)
    } else {
        (rng.inclusive(2, 13), rng.inclusive(2, 9) as u16)
    };

    // Column default styles: sorted, nonoverlapping, some without a style.
    let mut column_styles = Vec::new();
    let mut next = rng.inclusive(1, 3) as u16;
    while next <= columns + 2 && rng.chance(2, 3) {
        let last = next + rng.inclusive(0, 2) as u16;
        let style = *rng.pick(&[0, 0, 1, 4, 9]);
        column_styles.push((next, last, style));
        next = last + 1 + rng.inclusive(0, 2) as u16;
    }
    if rng.chance(1, 6) {
        column_styles.push((CellRef::MAX_COLUMN, CellRef::MAX_COLUMN, 7));
    }

    // Cells, then protected ranges that overwrite part of the grid.
    let mut grid: BTreeMap<CellRef, CellSpec> = BTreeMap::new();
    let density = rng.inclusive(1, 4);
    for row in 1..=rows {
        for column in 1..=columns {
            if rng.inclusive(1, 5) > density {
                continue;
            }
            let spec = match rng.below(14) {
                0 => formula_cell(rng, names),
                1 if rng.chance(1, 3) => metadata_cell(names),
                _ => plain_cell(rng, names, strings),
            };
            grid.insert(cell(row, column), spec);
        }
    }
    let merges = place_ranges(rng, crowded, names, &mut grid, rows, columns);
    if rng.chance(1, 6) {
        let far = *rng.pick(&[
            (CellRef::MAX_ROW, CellRef::MAX_COLUMN),
            (CellRef::MAX_ROW, 1),
            (1_000, 27),
        ]);
        grid.insert(cell(far.0, far.1), plain_cell(rng, names, strings));
    }
    let mut row_numbers: BTreeSet<u32> = grid.keys().map(|cell| cell.row()).collect();
    for _ in 0..rng.below(3) {
        row_numbers.insert(rng.inclusive(1, rows + 3));
    }

    let mut model = SheetModel {
        name: name.to_owned(),
        path: path.to_owned(),
        cells: BTreeMap::new(),
        rows: BTreeMap::new(),
        columns: column_styles.clone(),
        formula_cells: BTreeSet::new(),
        formula_ranges: Vec::new(),
        merges: merges.clone(),
        metadata_cells: BTreeSet::new(),
        opaque_cells: BTreeSet::new(),
        extent: (rows, columns),
        xml: String::new(),
        cell_sources: BTreeMap::new(),
        row_sources: BTreeMap::new(),
        fixed_sources: Vec::new(),
        tail_sources: Vec::new(),
    };

    // Separators between rows and cells: none, line breaks, or markup that
    // carries no content.
    let (row_space, cell_space) = *rng.pick(&[
        ("", ""),
        ("", ""),
        ("\n", "\n  "),
        ("\r\n", "\r\n\t"),
        ("<!-- row -->", "<?keep cell?>"),
    ]);
    let child_space = if cell_space.is_empty() || rng.chance(1, 2) {
        ""
    } else {
        cell_space
    };
    let el = |local: &str| names.el(local);
    let mut sheet_data = String::new();
    for &row in &row_numbers {
        let mut format = RowFormat::default();
        let mut open = format!("<{} r=\"{row}\"", el("row"));
        let row_cells: Vec<CellRef> = grid
            .range(cell(row, 1)..=cell(row, CellRef::MAX_COLUMN))
            .map(|(&address, _)| address)
            .collect();
        if rng.chance(1, 2) {
            let spans = match (row_cells.first(), row_cells.last()) {
                (Some(first), Some(last)) if rng.chance(2, 3) => {
                    format!("{}:{}", first.column(), last.column())
                }
                _ => format!("1:{}", rng.inclusive(1, u32::from(columns))),
            };
            open.push_str(&quoted(rng, "spans", &spans));
        }
        if rng.chance(1, 3) {
            let style = *rng.pick(&[0, 3, 6]);
            let spelling = if rng.chance(1, 6) {
                format!(" {style} ")
            } else {
                style.to_string()
            };
            open.push_str(&quoted(rng, "s", &spelling));
            format.style = Some(style);
            if rng.chance(3, 4) {
                let flag = *rng.pick(&["1", "true", "0", "false"]);
                format.custom = matches!(flag, "1" | "true");
                open.push_str(&quoted(rng, "customFormat", flag));
            }
        } else if rng.chance(1, 8) {
            open.push_str(" customFormat=\"1\"");
            format.custom = true;
        }
        if rng.chance(1, 5) {
            open.push_str(" ht=\"24\" customHeight=\"1\" v:row=\"keep\"");
        }
        model.rows.insert(row, format);
        let extension = rng.chance(1, 8).then(|| {
            format!(
                "<{ext_list}><{ext} uri=\"urn:row\"><v:rowFeature on=\"1\"/></{ext}></{ext_list}>",
                ext_list = el("extLst"),
                ext = el("ext"),
            )
        });
        sheet_data.push_str(row_space);
        if row_cells.is_empty() && extension.is_none() && rng.chance(1, 2) {
            open.push_str(empty_end(rng));
            sheet_data.push_str(&open);
            model.row_sources.insert(row, open);
            continue;
        }
        open.push('>');
        sheet_data.push_str(&open);
        model.row_sources.insert(row, open);
        for address in row_cells {
            let spec = grid.remove(&address).unwrap();
            let mut expected = spec.expected;
            let mut attributes = vec![(String::from("r"), address.to_string())];
            if rng.chance(1, 3) {
                let style = *rng.pick(&[0, 1, 2, 3, 10, 200]);
                expected.style = Some(style);
                attributes.push(("s".into(), style.to_string()));
            }
            if let Some(kind) = spec.kind {
                attributes.push(("t".into(), kind.into()));
            }
            if spec.opaque {
                model.opaque_cells.insert(address);
            }
            if spec.metadata {
                attributes.push(("vm".into(), "1".into()));
                model.metadata_cells.insert(address);
            }
            if rng.chance(1, 10) {
                attributes.push(("cm".into(), "1".into()));
            }
            if rng.chance(1, 10) {
                attributes.push(("v:keep".into(), "yes".into()));
            }
            if rng.chance(1, 4) {
                rng.shuffle(&mut attributes);
            }
            let mut children = spec.children;
            if rng.chance(1, 10) {
                let extension = format!(
                    "<{ext_list}><{ext} uri=\"urn:cell\"><v:cellFeature flag=\"1\"/></{ext}></{ext_list}>",
                    ext_list = el("extLst"),
                    ext = el("ext"),
                );
                model
                    .tail_sources
                    .push(format!("{extension}</{}>", el("c")));
                children.push(extension);
            }
            let mut source = format!("<{}", el("c"));
            for (name, value) in &attributes {
                source.push_str(&quoted(rng, name, value));
            }
            if children.is_empty() && rng.chance(1, 2) {
                source.push_str(empty_end(rng));
            } else {
                source.push('>');
                for child in &children {
                    source.push_str(child_space);
                    source.push_str(child);
                }
                source.push_str(&format!("</{}>", el("c")));
            }
            if expected.formula.is_some() {
                model.formula_cells.insert(address);
            }
            if let Some(range) = spec.formula_range {
                model.formula_ranges.push(range);
            }
            sheet_data.push_str(cell_space);
            sheet_data.push_str(&source);
            model.cell_sources.insert(address, source);
            model.cells.insert(address, expected);
        }
        if let Some(extension) = extension {
            sheet_data.push_str(&extension);
            model
                .tail_sources
                .push(format!("{extension}</{}>", el("row")));
        }
        sheet_data.push_str(&format!("</{}>", el("row")));
    }
    assert!(grid.is_empty(), "every generated cell belongs to a row");

    // Markup outside sheetData, in schema order.
    let declaration = if rng.chance(2, 3) {
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n"
    } else {
        ""
    };
    let office = if strict { STRICT_OFFICE_NS } else { OFFICE_NS };
    let root = format!(
        "{declaration}<{} {} xmlns:r=\"{office}\" xmlns:v=\"{VENDOR_NS}\" v:mode=\"keep\">",
        el("worksheet"),
        names.declaration(namespace),
    );
    let mut before = vec![root.clone()];
    if rng.chance(1, 3) {
        before.push(format!(
            "<{pr} filterMode=\"0\"><{setup} fitToPage=\"1\"/></{pr}>",
            pr = el("sheetPr"),
            setup = el("pageSetUpPr"),
        ));
    }
    let dimension = rng.chance(3, 4).then(|| {
        let reference = match (model.cells.keys().next(), model.cells.keys().next_back()) {
            (Some(&first), Some(&last)) if rng.chance(1, 2) => {
                let columns = model.cells.keys().map(|cell| cell.column());
                let (low, high) = (columns.clone().min().unwrap(), columns.max().unwrap());
                CellRange::new(cell(first.row(), low), cell(last.row(), high)).to_string()
            }
            _ => rng.pick(&["A1", "A1:C3", "B2:Z99"]).to_string(),
        };
        format!("<{} ref=\"{reference}\"/>", el("dimension"))
    });
    let mut middle = Vec::new();
    if rng.chance(1, 3) {
        middle.push(format!(
            "<{views}><{view} workbookViewId=\"0\"><{selection} activeCell=\"B2\" sqref=\"B2\"/></{view}></{views}>",
            views = el("sheetViews"),
            view = el("sheetView"),
            selection = el("selection"),
        ));
    }
    if rng.chance(1, 3) {
        middle.push(format!(
            "<{} defaultRowHeight=\"15\"/>",
            el("sheetFormatPr")
        ));
    }
    if !column_styles.is_empty() {
        let mut cols = format!("<{}>", el("cols"));
        for &(min, max, style) in &column_styles {
            let style = if style == 0 && rng.chance(1, 2) {
                String::new()
            } else {
                format!(" style=\"{style}\"")
            };
            cols.push_str(&format!(
                "<{} min=\"{min}\" max=\"{max}\" width=\"9.5\"{style}/>",
                el("col")
            ));
        }
        cols.push_str(&format!("</{}>", el("cols")));
        middle.push(cols);
    }
    let mut after = Vec::new();
    if !merges.is_empty() {
        let mut merge_cells = format!("<{} count=\"{}\">", el("mergeCells"), merges.len());
        for range in &merges {
            merge_cells.push_str(&format!("<{} ref=\"{range}\"/>", el("mergeCell")));
        }
        merge_cells.push_str(&format!("</{}>", el("mergeCells")));
        after.push(merge_cells);
    }
    if rng.chance(1, 3) {
        after.push("<v:future id=\"42\"><v:payload>keep me exactly</v:payload></v:future>".into());
    }
    if rng.chance(1, 2) {
        after.push(format!(
            "<{} left=\"0.7\" right=\"0.7\" top=\"0.75\" bottom=\"0.75\" header=\"0.3\" footer=\"0.3\"/>",
            el("pageMargins")
        ));
    }
    if rng.chance(1, 3) {
        after.push(format!(
            "<{ext_list}><{ext} uri=\"{{78C0D931-6437-407d-A8EE-F0AAD7539E65}}\"><v:conditionalFormattings/></{ext}></{ext_list}>",
            ext_list = el("extLst"),
            ext = el("ext"),
        ));
    }
    let sheet_data = if sheet_data.is_empty() && rng.chance(1, 2) {
        format!("<{}{}", el("sheetData"), empty_end(rng))
    } else {
        format!(
            "<{data}>{sheet_data}{row_space}</{data}>",
            data = el("sheetData")
        )
    };
    let close = format!("</{}>", el("worksheet"));
    let mut xml = before.concat();
    xml.push_str(dimension.as_deref().unwrap_or_default());
    xml.push_str(&middle.concat());
    xml.push_str(&sheet_data);
    xml.push_str(&after.concat());
    xml.push_str(&close);
    model.xml = xml;
    model.fixed_sources = before
        .into_iter()
        .chain(middle)
        .chain(after)
        .chain([close])
        .collect();
    model
}

/// How the archive records its members.
struct Layout {
    zip: ZipLayout<'static>,
    /// `None` for no data descriptors, or the chance (in 4) that an entry has
    /// one and whether they all carry the optional signature (`None`: each
    /// descriptor decides).
    descriptors: Option<(u64, Option<bool>)>,
}

fn random_layout(rng: &mut Rng) -> Layout {
    let zip = ZipLayout {
        deflate_level: if rng.chance(1, 3) {
            None
        } else {
            Some(rng.inclusive(0, 9))
        },
        store_larger_than: *rng.pick(&[usize::MAX, usize::MAX, 600, 8_000]),
        entry_comment: rng.chance(1, 3).then_some(b"entry comment kept".as_slice()),
        archive_comment: if rng.chance(1, 3) {
            b"archive comment kept".as_slice()
        } else {
            b"".as_slice()
        },
    };
    let descriptors = rng.chance(1, 2).then(|| {
        (
            rng.inclusive(1, 4) as u64,
            *rng.pick(&[Some(true), Some(false), None]),
        )
    });
    Layout { zip, descriptors }
}

struct Package {
    bytes: Vec<u8>,
    sheets: Vec<SheetModel>,
}

/// A workbook with one to three worksheets; `crowded` forces every worksheet
/// to hold many protected ranges, which otherwise happens for one in six.
fn generate_package(rng: &mut Rng, crowded: bool) -> Package {
    let strict = rng.chance(1, 4);
    let namespace = if strict { STRICT_NS } else { MAIN_NS };
    let office = if strict { STRICT_OFFICE_NS } else { OFFICE_NS };
    let strings = rng.chance(3, 4).then(|| shared_strings(rng, namespace));
    let texts = strings.as_ref().map_or(&[][..], |strings| &strings.texts);

    let mut sheet_names = vec!["Data", "Notes & <Q>", "雪 Ünï", "Q1 'Plan'", "Sheet \"4\""];
    rng.shuffle(&mut sheet_names);
    let sheet_count = rng.inclusive(1, 3) as usize;
    let sheets: Vec<SheetModel> = sheet_names[..sheet_count]
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let path = format!("xl/worksheets/sheet{}.xml", index + 1);
            let crowded = crowded || rng.chance(1, 6);
            generate_sheet(rng, crowded, name, &path, strict, texts)
        })
        .collect();

    let mut workbook = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<workbook xmlns=\"{namespace}\" xmlns:r=\"{office}\"><bookViews><workbookView/></bookViews><sheets>"
    );
    let mut relationships = format!("<Relationships xmlns=\"{PACKAGE_NS}\">");
    for (index, sheet) in sheets.iter().enumerate() {
        let id = index + 1;
        workbook.push_str(&format!(
            "<sheet name=\"{}\" sheetId=\"{id}\" r:id=\"rId{id}\"/>",
            escape_attribute(&sheet.name)
        ));
        let target = if rng.chance(1, 4) {
            format!("/{}", sheet.path)
        } else {
            format!("worksheets/sheet{id}.xml")
        };
        relationships.push_str(&format!(
            "<Relationship Id=\"rId{id}\" Type=\"{office}/worksheet\" Target=\"{target}\"/>"
        ));
    }
    workbook.push_str("</sheets><calcPr calcId=\"191029\"/></workbook>");
    if strings.is_some() {
        relationships.push_str(&format!(
            "<Relationship Id=\"rIdStrings\" Type=\"{office}/sharedStrings\" Target=\"sharedStrings.xml\"/>"
        ));
    }
    relationships.push_str(&format!(
        "<Relationship Id=\"rIdStyles\" Type=\"{office}/styles\" Target=\"styles.xml\"/></Relationships>"
    ));

    let mut entries: Vec<(String, Vec<u8>)> = vec![
        (
            "[Content_Types].xml".into(),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Types xmlns=\"{CONTENT_TYPES_NS}\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Default Extension=\"bin\" ContentType=\"application/vnd.ms-office.vbaProject\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.ms-excel.sheet.macroEnabled.main+xml\"/></Types>"
            )
            .into_bytes(),
        ),
        (
            "_rels/.rels".into(),
            format!(
                "<Relationships xmlns=\"{PACKAGE_NS}\"><Relationship Id=\"rId1\" Type=\"{office}/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>"
            )
            .into_bytes(),
        ),
        ("xl/workbook.xml".into(), workbook.into_bytes()),
        ("xl/_rels/workbook.xml.rels".into(), relationships.into_bytes()),
        (
            "xl/styles.xml".into(),
            format!("<styleSheet xmlns=\"{namespace}\"><cellXfs count=\"1\"><xf/></cellXfs></styleSheet>")
                .into_bytes(),
        ),
    ];
    if let Some(strings) = &strings {
        entries.push((
            "xl/sharedStrings.xml".into(),
            strings.xml.clone().into_bytes(),
        ));
    }
    for sheet in &sheets {
        entries.push((sheet.path.clone(), sheet.xml.clone().into_bytes()));
    }
    // Opaque parts: compressible XML, and incompressible binary data.
    if rng.chance(1, 2) {
        let mut macro_bytes = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1".to_vec();
        macro_bytes.extend((0..rng.below(3_000)).map(|_| rng.next_u64() as u8));
        entries.push(("xl/vbaProject.bin".into(), macro_bytes));
    }
    if rng.chance(1, 2) {
        entries.push((
            "xl/charts/chart1.xml".into(),
            "<chartSpace xmlns='urn:chart'><cache>1 2 3</cache></chartSpace>"
                .repeat(1 + rng.below(40))
                .into_bytes(),
        ));
    }
    if rng.chance(1, 3) {
        entries.push((
            "customXml/item1.xml".into(),
            b"<?xml version='1.0'?><mystery xmlns='urn:unknown'>Opaque</mystery>".to_vec(),
        ));
    }
    rng.shuffle(&mut entries);

    let layout = random_layout(rng);
    let borrowed: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    let mut bytes = write_zip(&borrowed, &layout.zip);
    if let Some((frequency, signed)) = layout.descriptors {
        let choices: BTreeMap<&str, Option<bool>> = entries
            .iter()
            .map(|(name, _)| {
                let choice = rng
                    .chance(frequency, 4)
                    .then(|| signed.unwrap_or_else(|| rng.chance(1, 2)));
                (name.as_str(), choice)
            })
            .collect();
        bytes = with_data_descriptors(&bytes, |name| choices[name]);
    }
    Package { bytes, sheets }
}

// ---------------------------------------------------------------- batches

#[derive(Clone, Copy, Debug)]
enum Spelling {
    Typed,
    Upper,
    Lower,
}

#[derive(Clone, Debug)]
struct Edit {
    sheet: usize,
    cell: CellRef,
    value: CellValue,
    spelling: Spelling,
}

impl Edit {
    fn address(&self) -> String {
        match self.spelling {
            Spelling::Lower => self.cell.to_string().to_ascii_lowercase(),
            _ => self.cell.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Api {
    SetCell,
    SetCellAt,
    SetCells,
    SetCellsAt,
    /// `set_cells_at` over a range's cells in row-major order.
    Block(CellRange),
    ApplyEdits,
}

#[derive(Clone, Debug)]
struct Batch {
    api: Api,
    edits: Vec<Edit>,
    /// Replaces the target sheet name (or the sheet of one `apply_edits`
    /// edit) with a name no worksheet has.
    missing_sheet: Option<(usize, String)>,
}

impl Batch {
    /// The value each edited cell must hold afterward: the last one written.
    fn net(&self) -> BTreeMap<(usize, CellRef), CellValue> {
        self.edits
            .iter()
            .map(|edit| ((edit.sheet, edit.cell), edit.value.clone()))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    Applied,
    InvalidValue,
    SheetNotFound,
    /// A protected cell, refused with its worksheet and address.
    Protected,
    /// A value payload with unfamiliar markup, which no edit may discard.
    Refused,
}

/// The outcome of a result, and for a protected cell, its worksheet and cell.
fn outcome_of(result: &sheetpatch::Result<()>) -> Option<(Outcome, Option<(&str, CellRef)>)> {
    match result {
        Ok(()) => Some((Outcome::Applied, None)),
        Err(Error::InvalidValue(_)) => Some((Outcome::InvalidValue, None)),
        Err(Error::SheetNotFound(_)) => Some((Outcome::SheetNotFound, None)),
        Err(Error::ProtectedCell { sheet, cell, .. }) => {
            Some((Outcome::Protected, Some((sheet.as_str(), *cell))))
        }
        Err(Error::Unsupported(_)) => Some((Outcome::Refused, None)),
        Err(_) => None,
    }
}

/// The documented validation order: single-cell setters and `CellEdit`
/// validate values before finding the sheet, while batch setters find the
/// sheet first. Worksheet protections are checked only for valid input.
fn expected_outcome(sheets: &[SheetModel], batch: &Batch) -> Outcome {
    let invalid = batch.edits.iter().any(|edit| !valid_value(&edit.value));
    let missing = batch.missing_sheet.is_some();
    let values_first = matches!(batch.api, Api::SetCell | Api::SetCellAt | Api::ApplyEdits);
    if invalid && (values_first || !missing) {
        return Outcome::InvalidValue;
    }
    if missing {
        return Outcome::SheetNotFound;
    }
    refusal(sheets, batch).map_or(Outcome::Applied, |(outcome, _)| outcome)
}

/// The first refused edit of a batch with valid input, and its outcome.
/// Worksheets are patched in workbook order. Within one, the formula and
/// merged-range guards cover the whole batch before any cell is patched; the
/// cells are then patched in row-major order, where unfamiliar payload markup
/// and changed values with metadata are refused.
fn refusal(sheets: &[SheetModel], batch: &Batch) -> Option<(Outcome, (usize, CellRef))> {
    let net = batch.net();
    for (index, sheet) in sheets.iter().enumerate() {
        let edits = || {
            net.iter()
                .filter(move |((edited, _), _)| *edited == index)
                .map(|(&(_, cell), value)| (cell, value))
        };
        // Formula cells are checked first, then formula ranges, then merged
        // ranges, each in row-major order.
        let first = |guard: &dyn Fn(CellRef) -> bool| {
            edits().map(|(cell, _)| cell).find(|&cell| guard(cell))
        };
        let guarded = first(&|cell| sheet.formula_cells.contains(&cell))
            .or_else(|| {
                first(&|cell| {
                    sheet
                        .formula_ranges
                        .iter()
                        .any(|range| range.contains(cell))
                })
            })
            .or_else(|| {
                first(&|cell| {
                    sheet
                        .merges
                        .iter()
                        .any(|range| range.contains(cell) && cell != range.first())
                })
            });
        if let Some(cell) = guarded {
            debug_assert!(sheet.guarded(cell));
            return Some((Outcome::Protected, (index, cell)));
        }
        if let Some((cell, _)) = edits().find(|&(cell, value)| !sheet.accepts(cell, value)) {
            let outcome = if sheet.opaque_cells.contains(&cell) {
                Outcome::Refused
            } else {
                Outcome::Protected
            };
            return Some((outcome, (index, cell)));
        }
    }
    None
}

fn apply_batch(
    book: &mut Workbook,
    sheets: &[SheetModel],
    batch: &Batch,
) -> sheetpatch::Result<()> {
    let sheet_name = |position: usize| -> &str {
        match &batch.missing_sheet {
            Some((index, name)) if *index == position => name,
            _ => &sheets[batch.edits[position].sheet].name,
        }
    };
    let first = &batch.edits[0];
    match batch.api {
        Api::SetCell => book.set_cell(sheet_name(0), &first.address(), first.value.clone()),
        Api::SetCellAt => book.set_cell_at(sheet_name(0), first.cell, first.value.clone()),
        Api::SetCells => book.set_cells(
            sheet_name(0),
            batch
                .edits
                .iter()
                .map(|edit| (edit.address(), edit.value.clone())),
        ),
        Api::SetCellsAt => book.set_cells_at(
            sheet_name(0),
            batch
                .edits
                .iter()
                .map(|edit| (edit.cell, edit.value.clone())),
        ),
        Api::Block(range) => book.set_cells_at(
            sheet_name(0),
            range
                .cells()
                .zip(batch.edits.iter().map(|edit| edit.value.clone())),
        ),
        Api::ApplyEdits => {
            let mut edits = Vec::with_capacity(batch.edits.len());
            for (position, edit) in batch.edits.iter().enumerate() {
                let sheet = sheet_name(position);
                let value = edit.value.clone();
                edits.push(match edit.spelling {
                    Spelling::Typed => CellEdit::at(sheet, edit.cell, value)?,
                    _ => CellEdit::new(sheet, &edit.address(), value)?,
                });
            }
            book.apply_edits(edits)
        }
    }
}

/// A target cell biased toward existing cells, their neighbors, the grid's
/// edge, and Excel's extreme addresses.
fn pick_cell(rng: &mut Rng, sheet: &SheetModel) -> CellRef {
    let (rows, columns) = sheet.extent;
    let count = sheet.cells.len();
    let existing = |rng: &mut Rng| *sheet.cells.keys().nth(rng.below(count)).unwrap();
    match rng.below(10) {
        0..=4 if count > 0 => existing(rng),
        5 if count > 0 => {
            let near = existing(rng);
            let row = (near.row() + rng.inclusive(0, 2)).saturating_sub(1).max(1);
            let column = (u32::from(near.column()) + rng.inclusive(0, 2)).saturating_sub(1);
            cell(row.min(CellRef::MAX_ROW), column.clamp(1, 16_384) as u16)
        }
        6 => {
            let (row, column) = *rng.pick(&[
                (1, CellRef::MAX_COLUMN),
                (CellRef::MAX_ROW, 1),
                (CellRef::MAX_ROW, CellRef::MAX_COLUMN),
                (rows + 20, 3),
                (1_000, 27),
            ]);
            cell(row, column)
        }
        _ => cell(
            rng.inclusive(1, rows + 3),
            rng.inclusive(1, u32::from(columns) + 3) as u16,
        ),
    }
}

fn spelling(rng: &mut Rng) -> Spelling {
    *rng.pick(&[Spelling::Typed, Spelling::Upper, Spelling::Lower])
}

/// An edit the worksheet accepts: an unprotected cell, with metadata cells
/// only "rewritten" with their current value.
fn accepted_edit(rng: &mut Rng, sheets: &[SheetModel], sheet: usize, allow_long: bool) -> Edit {
    let model = &sheets[sheet];
    let mut target = pick_cell(rng, model);
    for _ in 0..32 {
        if !model.protected(target) {
            break;
        }
        target = pick_cell(rng, model);
    }
    if model.protected(target) {
        // Ranges never extend more than two rows past the generated grid.
        target = cell(model.extent.0 + 3, 1);
    }
    // Rewriting the current value exercises no-op detection, as do the
    // metadata cells, which accept nothing else. Reads accept error codes
    // that writes refuse, such as `#FUTURE!`.
    let current = model.expected(target).value;
    let value =
        if model.metadata_cells.contains(&target) || (valid_value(&current) && rng.chance(1, 8)) {
            current
        } else {
            random_value(rng, allow_long)
        };
    Edit {
        sheet,
        cell: target,
        value,
        spelling: spelling(rng),
    }
}

/// Mostly small batches. Larger ones, more frequent on sheets with many
/// protected ranges, are checked by the sweep guard instead of a direct scan.
fn batch_size(rng: &mut Rng, sheet: &SheetModel) -> u32 {
    let crowded = sheet.formula_ranges.len() > 8 || sheet.merges.len() > 8;
    if rng.chance(if crowded { 3 } else { 1 }, 5) {
        rng.inclusive(13, 40)
    } else {
        rng.inclusive(1, 12)
    }
}

/// A batch every worksheet accepts, through a randomly chosen API. Batches
/// may repeat a cell; the last value wins. At most one value is a 32,767-unit
/// string, which keeps the suite fast.
fn accepted_batch(rng: &mut Rng, sheets: &[SheetModel]) -> Batch {
    let sheet = rng.below(sheets.len());
    let mut long_budget = rng.chance(1, 6);
    let mut edit = |rng: &mut Rng, sheet: usize| {
        let edit = accepted_edit(rng, sheets, sheet, long_budget);
        if is_long(&edit.value) {
            long_budget = false;
        }
        edit
    };
    let (api, mut edits) = match rng.below(12) {
        0 | 1 => (Api::SetCell, vec![edit(rng, sheet)]),
        2 | 3 => (Api::SetCellAt, vec![edit(rng, sheet)]),
        4 | 5 => {
            let count = batch_size(rng, &sheets[sheet]);
            (
                Api::SetCells,
                (0..count).map(|_| edit(rng, sheet)).collect(),
            )
        }
        6 | 7 => {
            let count = batch_size(rng, &sheets[sheet]);
            (
                Api::SetCellsAt,
                (0..count).map(|_| edit(rng, sheet)).collect(),
            )
        }
        8 | 9 => {
            let model = &sheets[sheet];
            let editable = |range: &CellRange| {
                range
                    .cells()
                    .all(|cell| !model.protected(cell) && !model.metadata_cells.contains(&cell))
            };
            let mut ranges =
                (0..4).map(|_| small_range(rng, model.extent.0 + 2, model.extent.1 + 2));
            if let Some(range) = ranges.find(editable) {
                let edits = range
                    .cells()
                    .map(|cell| Edit {
                        sheet,
                        cell,
                        value: random_value(rng, false),
                        spelling: Spelling::Typed,
                    })
                    .collect();
                return Batch {
                    api: Api::Block(range),
                    edits,
                    missing_sheet: None,
                };
            }
            (Api::SetCellsAt, vec![edit(rng, sheet)])
        }
        _ => {
            let count = batch_size(rng, &sheets[sheet]);
            let edits = (0..count)
                .map(|_| {
                    let sheet = rng.below(sheets.len());
                    edit(rng, sheet)
                })
                .collect();
            (Api::ApplyEdits, edits)
        }
    };
    if !matches!(api, Api::SetCell | Api::SetCellAt) && rng.chance(1, 3) {
        // Repeat earlier cells with different values; the last one wins.
        for _ in 0..rng.inclusive(1, 3) {
            let earlier = edits[rng.below(edits.len())].clone();
            let model = &sheets[earlier.sheet];
            if model.metadata_cells.contains(&earlier.cell) {
                continue;
            }
            edits.push(Edit {
                value: random_value(rng, false),
                spelling: spelling(rng),
                ..earlier
            });
        }
    }
    Batch {
        api,
        edits,
        missing_sheet: None,
    }
}

/// A batch with exactly one reason to fail: a protected cell, a changed
/// metadata cell, an invalid value, or a sheet name that matches no
/// worksheet exactly.
fn rejected_batch(rng: &mut Rng, sheets: &[SheetModel]) -> Batch {
    let mut batch = accepted_batch(rng, sheets);
    if matches!(batch.api, Api::Block(_)) {
        batch.api = Api::SetCellsAt;
    }
    let single = matches!(batch.api, Api::SetCell | Api::SetCellAt);
    let target = batch.edits[0].sheet;
    let position = if single {
        0
    } else {
        rng.below(batch.edits.len() + 1)
    };
    let candidates = |sheet: usize| -> Vec<(usize, CellRef)> {
        let model = &sheets[sheet];
        model
            .protected_cells()
            .into_iter()
            .chain(model.metadata_cells.iter().copied())
            .map(|cell| (sheet, cell))
            .collect()
    };
    let protected: Vec<(usize, CellRef)> = if matches!(batch.api, Api::ApplyEdits) {
        (0..sheets.len()).flat_map(candidates).collect()
    } else {
        candidates(target)
    };
    match rng.below(4) {
        0 | 1 if !protected.is_empty() => {
            let (sheet, target) = *rng.pick(&protected);
            let mut value = random_value(rng, false);
            if sheets[sheet].metadata_cells.contains(&target) {
                while value == sheets[sheet].expected(target).value {
                    value = random_value(rng, false);
                }
            }
            let edit = Edit {
                sheet,
                cell: target,
                value,
                spelling: spelling(rng),
            };
            // A later accepted rewrite of the same metadata cell would win.
            batch
                .edits
                .retain(|other| (other.sheet, other.cell) != (sheet, target));
            if single {
                batch.edits = vec![edit];
            } else {
                batch.edits.insert(position.min(batch.edits.len()), edit);
            }
        }
        2 => {
            let sheet = &sheets[batch.edits[position.min(batch.edits.len() - 1)].sheet];
            let variants = [
                sheet.name.to_lowercase(),
                sheet.name.to_uppercase(),
                format!("{} ", sheet.name),
                format!(" {}", sheet.name),
                "Missing".to_owned(),
            ];
            let name = variants
                .into_iter()
                .find(|name| sheets.iter().all(|sheet| sheet.name != *name))
                .unwrap();
            let index = if matches!(batch.api, Api::ApplyEdits) {
                position.min(batch.edits.len() - 1)
            } else {
                0
            };
            batch.missing_sheet = Some((index, name));
        }
        _ => {
            let index = rng.below(batch.edits.len());
            batch.edits[index].value = invalid_value(rng);
        }
    }
    batch
}

// ---------------------------------------------------------------- checks

/// Reads every modelled cell plus probes, in shuffled order with repeats,
/// through each read API, and compares them with the model.
fn check_reads(case: Case, stage: &str, book: &Workbook, sheets: &[SheetModel], rng: &mut Rng) {
    let listed: Vec<(&str, &str)> = book
        .sheets()
        .iter()
        .map(|sheet| (sheet.name(), sheet.path()))
        .collect();
    let modelled: Vec<(&str, &str)> = sheets
        .iter()
        .map(|sheet| (sheet.name.as_str(), sheet.path.as_str()))
        .collect();
    assert_eq!(listed, modelled, "{case} {stage}: worksheet list");
    for sheet in sheets {
        let name = sheet.name.as_str();
        let mut cells: Vec<CellRef> = sheet.cells.keys().copied().collect();
        for _ in 0..4 {
            cells.push(pick_cell(rng, sheet));
        }
        let protected = sheet.protected_cells();
        if !protected.is_empty() {
            cells.push(*rng.pick(&protected));
        }
        if !cells.is_empty() {
            let repeat = *rng.pick(&cells);
            cells.push(repeat);
        }
        rng.shuffle(&mut cells);
        let got = book
            .get_cells_at(name, cells.iter().copied())
            .or_fail(case, format_args!("{stage}: get_cells_at on {name:?}"));
        assert_eq!(got.len(), cells.len(), "{case} {stage}: read count");
        for (address, content) in cells.iter().zip(&got) {
            let expected = sheet.expected(*address);
            assert!(
                expected.matches(content),
                "{case} {stage}: {name}!{address} read {}, expected {}",
                brief(content),
                brief(&expected)
            );
        }
        let spelled: Vec<String> = cells
            .iter()
            .map(|cell| {
                if rng.chance(1, 2) {
                    cell.to_string().to_ascii_lowercase()
                } else {
                    cell.to_string()
                }
            })
            .collect();
        let by_name = book
            .get_cells(name, &spelled)
            .or_fail(case, format_args!("{stage}: get_cells on {name:?}"));
        assert!(
            by_name == got,
            "{case} {stage}: get_cells differs from get_cells_at on {name}"
        );
        let view = book
            .prepare_sheet(name)
            .or_fail(case, format_args!("{stage}: prepare_sheet({name:?})"));
        assert_eq!(view.sheet().name(), name, "{case} {stage}: view name");
        assert_eq!(view.sheet().path(), sheet.path, "{case} {stage}: view path");
        let viewed = view
            .get_cells_at(cells.iter().copied())
            .or_fail(case, format_args!("{stage}: view.get_cells_at on {name:?}"));
        assert!(
            viewed == got,
            "{case} {stage}: prepared view differs from direct reads on {name}"
        );
        let viewed = view
            .get_cells(&spelled)
            .or_fail(case, format_args!("{stage}: view.get_cells on {name:?}"));
        assert!(
            viewed == got,
            "{case} {stage}: prepared A1 view differs on {name}"
        );
        if !cells.is_empty() {
            let index = rng.below(cells.len());
            let single = [
                book.get_cell(name, &spelled[index]),
                book.get_cell_at(name, cells[index]),
                view.get_cell(&spelled[index]),
                view.get_cell_at(cells[index]),
            ];
            for content in single {
                let content = content.or_fail(case, format_args!("{stage}: single read"));
                assert!(
                    content == got[index],
                    "{case} {stage}: single read of {name}!{} differs",
                    cells[index]
                );
            }
        }
    }
}

/// End offset of each entry's local record, including any data descriptor.
fn record_end(bytes: &[u8], entry: &common::ZipEntry<'_>) -> usize {
    entry.local_offset + entry.local.len() + data_descriptor_len(bytes, entry)
}

fn assert_contiguous(case: Case, stage: &str, bytes: &[u8]) {
    let mut records: Vec<(usize, usize)> = zip_entries(bytes)
        .iter()
        .map(|entry| (entry.local_offset, record_end(bytes, entry)))
        .collect();
    records.sort_unstable();
    let mut cursor = 0;
    for (start, end) in records {
        assert_eq!(
            start, cursor,
            "{case} {stage}: gap or overlap before a local record"
        );
        cursor = end;
    }
    assert_eq!(
        cursor,
        central_directory_offset(bytes),
        "{case} {stage}: gap before the central directory"
    );
}

/// Untouched members keep their local header, compressed bytes, descriptor,
/// and central record except its offset; member order and the archive
/// comment are unchanged too.
fn assert_untouched_records(
    case: Case,
    stage: &str,
    input: &[u8],
    output: &[u8],
    changed: &BTreeSet<&str>,
) {
    let before = zip_entries(input);
    let after = zip_entries(output);
    let names = |entries: &[common::ZipEntry<'_>]| -> Vec<String> {
        entries.iter().map(|entry| entry.name.clone()).collect()
    };
    assert_eq!(
        names(&before),
        names(&after),
        "{case} {stage}: member order"
    );
    for (old, new) in before.iter().zip(&after) {
        // Every member, changed or not, decompresses to its recorded size and CRC.
        new.payload();
        if changed.contains(old.name.as_str()) {
            continue;
        }
        let old_record = &input[old.local_offset..record_end(input, old)];
        let new_record = &output[new.local_offset..record_end(output, new)];
        assert!(
            old_record == new_record,
            "{case} {stage}: local record of untouched {} changed",
            old.name
        );
        assert_eq!(
            (old.central_before_offset(), old.central_after_offset()),
            (new.central_before_offset(), new.central_after_offset()),
            "{case} {stage}: central record of untouched {} changed",
            old.name
        );
    }
    assert_eq!(
        common::archive_comment(input),
        common::archive_comment(output),
        "{case} {stage}: archive comment"
    );
}

/// A worksheet's dimension reference, when it has one.
fn dimension(xml: &str) -> Option<&str> {
    let start = xml.find("dimension ref=\"")? + "dimension ref=\"".len();
    let length = xml[start..].find('"')?;
    Some(&xml[start..start + length])
}

/// The column intervals of a row's `spans` hint, when the row has one.
fn row_spans(xml: &str, row: u32) -> Option<Vec<(u16, u16)>> {
    let start = xml.find(&format!("row r=\"{row}\""))?;
    let tag = &xml[start..start + xml[start..].find('>')?];
    let name = tag.find("spans")?;
    let value = tag[name + "spans".len()..].trim_start_matches([' ', '\t', '\n', '=']);
    let quote = value.chars().next()?;
    let value = &value[1..1 + value[1..].find(quote)?];
    let spans = value
        .split_whitespace()
        .map(|span| {
            let (first, last) = span.split_once(':').expect("span interval");
            (
                first.parse().expect("span start"),
                last.parse().expect("span end"),
            )
        })
        .collect();
    Some(spans)
}

fn assert_same_bytes(case: Case, what: &str, actual: &[u8], expected: &[u8]) {
    assert!(
        actual == expected,
        "{case} {what}: {} bytes differ from the expected {} bytes",
        actual.len(),
        expected.len()
    );
}

/// Applies a batch that must fail with the documented error, and requires
/// that the pending state, including earlier accepted edits, is unchanged.
fn assert_refused(case: Case, book: &mut Workbook, sheets: &[SheetModel], batch: &Batch) {
    let expected = expected_outcome(sheets, batch);
    assert_ne!(
        expected,
        Outcome::Applied,
        "{case}: rejected batch generator"
    );
    let before = book.to_bytes().or_fail(case, "to_bytes");
    let had_changes = book.has_changes();
    let result = apply_batch(book, sheets, batch);
    let protected: Vec<String> = batch
        .net()
        .iter()
        .filter(|&(&(sheet, cell), value)| !sheets[sheet].accepts(cell, value))
        .map(|(&(sheet, cell), _)| format!("{}!{cell}", sheets[sheet].name))
        .collect();
    // A protected cell is reported with its own worksheet and address.
    let expected_cell = refusal(sheets, batch)
        .filter(|_| expected == Outcome::Protected)
        .map(|(_, (sheet, cell))| (sheets[sheet].name.as_str(), cell));
    assert_eq!(
        outcome_of(&result),
        Some((expected, expected_cell)),
        "{case}: {result:?}; protected cells {protected:?} in {}",
        brief(batch)
    );
    assert_eq!(
        book.has_changes(),
        had_changes,
        "{case}: a refused batch changed has_changes"
    );
    let after = book.to_bytes().or_fail(case, "to_bytes");
    assert_same_bytes(case, "refused batch output", &after, &before);
}

// ---------------------------------------------------------------- properties

/// Writes random batches through every editing API and requires that reads
/// return the written values in memory, after a normal save, and after a
/// compact save; that untouched cells and styles are unchanged; that refused
/// batches change nothing; and that writing the same values again is a no-op.
#[test]
fn reads_return_written_values_before_and_after_saving() {
    run_cases("read_after_write", 40, |case, rng| {
        let package = generate_package(rng, false);
        let mut sheets = package.sheets;
        let mut book = Workbook::from_bytes(package.bytes.clone()).or_fail(case, "open package");
        check_reads(case, "original", &book, &sheets, rng);
        let mut written: BTreeMap<(usize, CellRef), CellValue> = BTreeMap::new();
        let mut last_batch = None;
        for round in 0..rng.inclusive(1, 4) {
            if rng.chance(1, 2) {
                let batch = rejected_batch(rng, &sheets);
                assert_refused(case, &mut book, &sheets, &batch);
            }
            let batch = accepted_batch(rng, &sheets);
            assert_eq!(
                expected_outcome(&sheets, &batch),
                Outcome::Applied,
                "{case}"
            );
            apply_batch(&mut book, &sheets, &batch).or_fail(
                case,
                format_args!("round {round}: applying {}", brief(&batch)),
            );
            for ((sheet, cell), value) in batch.net() {
                sheets[sheet].apply(cell, &value);
                written.insert((sheet, cell), value);
            }
            check_reads(
                case,
                &format!("round {round} in memory"),
                &book,
                &sheets,
                rng,
            );
            let saved = book.to_bytes().or_fail(case, "to_bytes");
            let had_changes = book.has_changes();
            apply_batch(&mut book, &sheets, &batch)
                .or_fail(case, format_args!("round {round}: repeating the batch"));
            assert_eq!(
                book.has_changes(),
                had_changes,
                "{case} round {round}: repeat"
            );
            let repeated = book.to_bytes().or_fail(case, "to_bytes");
            assert_same_bytes(case, "repeating a batch", &repeated, &saved);
            last_batch = Some(batch);
        }

        let output = book.to_bytes().or_fail(case, "to_bytes");
        let mut streamed = Vec::new();
        book.write_to(&mut streamed).or_fail(case, "write_to");
        assert_same_bytes(case, "write_to", &streamed, &output);
        let mut reopened = Workbook::from_bytes(output.clone()).or_fail(case, "reopen");
        check_reads(case, "reopened", &reopened, &sheets, rng);
        let compact = book.to_bytes_compact().or_fail(case, "to_bytes_compact");
        // Generated archives are contiguous, so there is nothing to remove.
        assert_same_bytes(case, "compact output", &compact, &output);
        let compacted = Workbook::from_bytes(compact).or_fail(case, "reopen compact");
        check_reads(case, "reopened compact", &compacted, &sheets, rng);

        // Every value already holds its final content, so writing it again
        // to the reopened workbook changes nothing: neither the last batch,
        // duplicates included, nor the final value of every edited cell.
        let last_batch = last_batch.expect("at least one round");
        apply_batch(&mut reopened, &sheets, &last_batch).or_fail(case, "repeating the last batch");
        assert!(
            !reopened.has_changes(),
            "{case}: repeating the last batch after reopening reported changes"
        );
        let names: Vec<&str> = sheets.iter().map(|sheet| sheet.name.as_str()).collect();
        if rng.chance(1, 2) {
            let edits = written
                .iter()
                .map(|(&(sheet, cell), value)| CellEdit::at(names[sheet], cell, value.clone()))
                .collect::<sheetpatch::Result<Vec<_>>>()
                .or_fail(case, "CellEdit::at");
            reopened
                .apply_edits(edits)
                .or_fail(case, "rewriting values");
        } else {
            for (index, name) in names.iter().enumerate() {
                reopened
                    .set_cells_at(
                        name,
                        written
                            .range(
                                (index, cell(1, 1))
                                    ..=(index, cell(CellRef::MAX_ROW, CellRef::MAX_COLUMN)),
                            )
                            .map(|(&(_, cell), value)| (cell, value.clone())),
                    )
                    .or_fail(case, "rewriting values");
            }
        }
        assert!(
            !reopened.has_changes(),
            "{case}: rewriting saved values reported changes"
        );
        let rewritten = reopened.to_bytes().or_fail(case, "to_bytes");
        assert_same_bytes(case, "rewritten output", &rewritten, &output);
    });
}

/// Saves keep every untouched ZIP record and every unedited byte of edited
/// worksheets, return the original archive without edits, and restore it
/// after `reset_changes`. Output is a deterministic function of the edits.
#[test]
fn saves_preserve_untouched_records_and_worksheet_markup() {
    run_cases("preservation", 40, |case, rng| {
        let package = generate_package(rng, false);
        let input = package.bytes;
        let original = package.sheets;
        let mut sheets = original.clone();
        let mut book = Workbook::from_bytes(input.clone()).or_fail(case, "open package");
        assert_same_bytes(
            case,
            "unedited to_bytes",
            &book.to_bytes().or_fail(case, "to_bytes"),
            &input,
        );
        let mut streamed = Vec::new();
        book.write_to(&mut streamed).or_fail(case, "write_to");
        assert_same_bytes(case, "unedited write_to", &streamed, &input);
        let compact = book.to_bytes_compact().or_fail(case, "to_bytes_compact");
        assert_same_bytes(case, "unedited compact output", &compact, &input);

        let mut batches = Vec::new();
        let mut touched: BTreeSet<(usize, CellRef)> = BTreeSet::new();
        for round in 0..rng.inclusive(1, 3) {
            if rng.chance(1, 3) {
                let batch = rejected_batch(rng, &sheets);
                assert_refused(case, &mut book, &sheets, &batch);
            }
            let batch = accepted_batch(rng, &sheets);
            apply_batch(&mut book, &sheets, &batch).or_fail(
                case,
                format_args!("round {round}: applying {}", brief(&batch)),
            );
            for ((sheet, cell), value) in batch.net() {
                sheets[sheet].apply(cell, &value);
                touched.insert((sheet, cell));
            }
            batches.push(batch);
        }
        let output = book.to_bytes().or_fail(case, "to_bytes");
        assert_eq!(
            book.has_changes(),
            output != input,
            "{case}: has_changes disagrees with the saved bytes"
        );
        let edited: BTreeSet<&str> = touched
            .iter()
            .map(|&(sheet, _)| sheets[sheet].path.as_str())
            .collect();
        assert_untouched_records(case, "save", &input, &output, &edited);
        assert_contiguous(case, "save", &output);
        let compact = book.to_bytes_compact().or_fail(case, "to_bytes_compact");
        assert_same_bytes(case, "compact output", &compact, &output);

        for (index, sheet) in original.iter().enumerate() {
            let xml = String::from_utf8(payload(&output, &sheet.path))
                .or_fail(case, "worksheet is UTF-8");
            let edited_cells: BTreeSet<CellRef> = touched
                .iter()
                .filter(|&&(edited, _)| edited == index)
                .map(|&(_, cell)| cell)
                .collect();
            if edited_cells.is_empty() {
                assert!(
                    xml == sheet.xml,
                    "{case}: unedited worksheet {} changed",
                    sheet.path
                );
                continue;
            }
            for (address, source) in &sheet.cell_sources {
                if !edited_cells.contains(address) {
                    assert!(
                        xml.contains(source.as_str()),
                        "{case}: unedited cell {}!{address} lost its source {}",
                        sheet.name,
                        brief(source)
                    );
                }
            }
            for (row, source) in &sheet.row_sources {
                if !edited_cells.iter().any(|cell| cell.row() == *row) {
                    assert!(
                        xml.contains(source.as_str()),
                        "{case}: unedited row {row} of {} changed: {source}",
                        sheet.name
                    );
                }
            }
            for source in &sheet.fixed_sources {
                assert!(
                    xml.contains(source.as_str()),
                    "{case}: markup outside sheetData changed in {}: {}",
                    sheet.name,
                    brief(source)
                );
            }
            for source in &sheet.tail_sources {
                assert!(
                    xml.contains(source.as_str()),
                    "{case}: an extension list is no longer last in {}: {source}",
                    sheet.name
                );
            }
            // Existing dimension and row-span hints cover every new cell.
            let had_dimension = dimension(&sheet.xml).is_some();
            for &created in edited_cells.iter().filter(|cell| {
                !sheet.cells.contains_key(cell) && sheets[index].cells.contains_key(cell)
            }) {
                if had_dimension {
                    let reference = dimension(&xml).unwrap_or_default();
                    let range: CellRange = reference
                        .parse()
                        .or_fail(case, format_args!("dimension {reference:?} after an edit"));
                    assert!(
                        range.contains(created),
                        "{case}: dimension {range} of {} misses new cell {created}",
                        sheet.name
                    );
                }
                if let Some(spans) = row_spans(&xml, created.row()) {
                    assert!(
                        spans
                            .iter()
                            .any(|&(first, last)| (first..=last).contains(&created.column())),
                        "{case}: spans {spans:?} of {} row {} miss new cell {created}",
                        sheet.name,
                        created.row()
                    );
                }
            }
        }

        book.reset_changes();
        assert!(!book.has_changes(), "{case}: reset_changes left changes");
        assert_same_bytes(
            case,
            "after reset_changes",
            &book.to_bytes().or_fail(case, "to_bytes"),
            &input,
        );
        check_reads(case, "after reset_changes", &book, &original, rng);
        for batch in &batches {
            apply_batch(&mut book, &original, batch).or_fail(case, "reapplying after reset");
        }
        assert_same_bytes(
            case,
            "reapplied output",
            &book.to_bytes().or_fail(case, "to_bytes"),
            &output,
        );
        book.reset_changes();
        assert_same_bytes(
            case,
            "second reset",
            &book.to_bytes().or_fail(case, "to_bytes"),
            &input,
        );
    });
}

/// Large batches against worksheets with many formula groups and merged ranges
/// are accepted exactly when no cell is protected, matching a direct oracle,
/// so the sweep used for large batches agrees with the direct scan.
#[test]
fn protection_guards_match_an_independent_oracle() {
    run_cases("guards", 30, |case, rng| {
        let package = generate_package(rng, true);
        let sheets = package.sheets;
        let mut book = Workbook::from_bytes(package.bytes).or_fail(case, "open package");
        for round in 0..4 {
            let multiple = rng.chance(1, 3);
            let target = rng.below(sheets.len());
            let mut edits: Vec<Edit> = (0..rng.inclusive(1, 60))
                .map(|_| {
                    let sheet = if multiple {
                        rng.below(sheets.len())
                    } else {
                        target
                    };
                    accepted_edit(rng, &sheets, sheet, false)
                })
                .collect();
            // Half of the batches add one protected cell, usually a range
            // member that only the range guards, not the formula check, catch.
            if rng.chance(1, 2) {
                let sheet = if multiple {
                    rng.below(sheets.len())
                } else {
                    target
                };
                let model = &sheets[sheet];
                let protected = model.protected_cells();
                let ranged: Vec<CellRef> = protected
                    .iter()
                    .copied()
                    .filter(|cell| {
                        !model.formula_cells.contains(cell) && !model.opaque_cells.contains(cell)
                    })
                    .collect();
                let choices = if ranged.is_empty() || rng.chance(1, 4) {
                    &protected
                } else {
                    &ranged
                };
                if !choices.is_empty() {
                    let edit = Edit {
                        sheet,
                        cell: *rng.pick(choices),
                        value: CellValue::Number(f64::from(rng.inclusive(0, 999))),
                        spelling: Spelling::Typed,
                    };
                    edits.insert(rng.below(edits.len() + 1), edit);
                }
            }
            let batch = Batch {
                api: if multiple {
                    Api::ApplyEdits
                } else {
                    Api::SetCellsAt
                },
                edits,
                missing_sheet: None,
            };
            if expected_outcome(&sheets, &batch) != Outcome::Applied {
                assert_refused(case, &mut book, &sheets, &batch);
                continue;
            }
            apply_batch(&mut book, &sheets, &batch).or_fail(
                case,
                format_args!("round {round}: unprotected batch {}", brief(&batch)),
            );
            let net = batch.net();
            for (index, sheet) in sheets.iter().enumerate() {
                let written: Vec<(CellRef, &CellValue)> = net
                    .iter()
                    .filter(|&(&(edited, _), _)| edited == index)
                    .map(|(&(_, cell), value)| (cell, value))
                    .collect();
                let read = book
                    .get_cells_at(&sheet.name, written.iter().map(|&(cell, _)| cell))
                    .or_fail(case, "get_cells_at");
                for ((cell, value), content) in written.iter().zip(read) {
                    assert_eq!(&content.value, *value, "{case} round {round}: {cell}");
                }
            }
            // Later rounds start from the generated worksheets again.
            book.reset_changes();
        }
    });
}

/// Column letters computed by digit ranges, independent of the crate's loop.
fn column_letters(column: u16) -> String {
    let letter = |index: u32| char::from(b'A' + index as u8);
    let column = u32::from(column);
    match column {
        1..=26 => letter(column - 1).to_string(),
        27..=702 => [letter((column - 27) / 26), letter((column - 27) % 26)]
            .iter()
            .collect(),
        _ => {
            let offset = column - 703;
            [
                letter(offset / 676),
                letter(offset / 26 % 26),
                letter(offset % 26),
            ]
            .iter()
            .collect()
        }
    }
}

fn random_row(rng: &mut Rng) -> u32 {
    match rng.below(4) {
        0 => *rng.pick(&[1, 2, 9, 10, 99, 100, CellRef::MAX_ROW - 1, CellRef::MAX_ROW]),
        1 => rng.inclusive(1, 30),
        2 => rng.inclusive(CellRef::MAX_ROW - 30, CellRef::MAX_ROW),
        _ => rng.inclusive(1, CellRef::MAX_ROW),
    }
}

fn random_column(rng: &mut Rng) -> u16 {
    match rng.below(4) {
        0 => *rng.pick(&[1, 2, 26, 27, 52, 53, 702, 703, 16_383, CellRef::MAX_COLUMN]),
        1 => rng.inclusive(1, 30) as u16,
        2 => rng.inclusive(16_354, 16_384) as u16,
        _ => rng.inclusive(1, 16_384) as u16,
    }
}

/// The size hint `CellRangeIter` documents: exact whenever it fits in `usize`.
fn exact_hint(remaining: u64) -> (usize, Option<usize>) {
    usize::try_from(remaining).map_or((usize::MAX, None), |n| (n, Some(n)))
}

/// `CellRef` and `CellRange` text round trips, normalization, and iteration.
#[test]
fn cell_references_and_ranges_round_trip() {
    run_cases("ranges", 400, |case, rng| {
        let a = cell(random_row(rng), random_column(rng));
        let b = if rng.chance(2, 3) {
            // A nearby corner keeps most ranges small enough to iterate.
            let row = (i64::from(a.row()) + rng.inclusive(0, 40) as i64 - 20)
                .clamp(1, i64::from(CellRef::MAX_ROW)) as u32;
            let column = (i64::from(a.column()) + rng.inclusive(0, 40) as i64 - 20)
                .clamp(1, i64::from(CellRef::MAX_COLUMN)) as u16;
            cell(row, column)
        } else {
            cell(random_row(rng), random_column(rng))
        };

        for reference in [a, b] {
            let text = reference.to_string();
            let expected = format!("{}{}", column_letters(reference.column()), reference.row());
            assert_eq!(text, expected, "{case}: Display of {reference:?}");
            for spelling in [text.clone(), text.to_ascii_lowercase()] {
                assert_eq!(
                    spelling.parse::<CellRef>().or_fail(case, &spelling),
                    reference,
                    "{case}: parse {spelling}"
                );
            }
            assert_eq!(
                CellRef::new(reference.row(), reference.column()).or_fail(case, "CellRef::new"),
                reference,
                "{case}"
            );
            let letters = column_letters(reference.column());
            for invalid in [
                format!("{letters}0"),
                format!("{letters}{}", u64::from(CellRef::MAX_ROW) + 1),
                format!("${text}"),
                format!("{letters}${}", reference.row()),
                format!(" {text}"),
                format!("{text} "),
                letters.clone(),
                reference.row().to_string(),
            ] {
                assert!(
                    invalid.parse::<CellRef>().is_err(),
                    "{case}: accepted {invalid:?}"
                );
            }
            assert!(
                CellRef::new(0, reference.column()).is_err(),
                "{case}: row 0"
            );
            assert!(
                CellRef::new(reference.row(), 0).is_err(),
                "{case}: column 0"
            );
            assert!(
                CellRef::new(CellRef::MAX_ROW + 1, reference.column()).is_err(),
                "{case}: row past the limit"
            );
            assert!(
                CellRef::new(reference.row(), CellRef::MAX_COLUMN + 1).is_err(),
                "{case}: column past the limit"
            );
        }

        let range = CellRange::new(a, b);
        assert_eq!(range, CellRange::new(b, a), "{case}: corner order");
        let first = cell(a.row().min(b.row()), a.column().min(b.column()));
        let last = cell(a.row().max(b.row()), a.column().max(b.column()));
        assert_eq!(
            (range.first(), range.last()),
            (first, last),
            "{case}: corners"
        );
        assert_eq!(
            range.height(),
            last.row() - first.row() + 1,
            "{case}: height"
        );
        assert_eq!(
            range.width(),
            last.column() - first.column() + 1,
            "{case}: width"
        );
        assert_eq!(
            range.cell_count(),
            u64::from(range.height()) * u64::from(range.width()),
            "{case}: cell_count"
        );
        let text = range.to_string();
        if first == last {
            assert_eq!(text, first.to_string(), "{case}: single-cell Display");
            assert_eq!(range, CellRange::from(first), "{case}: From<CellRef>");
        } else {
            assert_eq!(text, format!("{first}:{last}"), "{case}: Display");
        }
        for spelling in [
            text.clone(),
            text.to_ascii_lowercase(),
            format!("{a}:{b}"),
            format!("{b}:{a}"),
            format!(
                "{}:{}",
                cell(first.row(), last.column()),
                cell(last.row(), first.column())
            ),
        ] {
            assert_eq!(
                spelling.parse::<CellRange>().or_fail(case, &spelling),
                range,
                "{case}: parse {spelling}"
            );
        }
        for invalid in [
            format!("{text}:"),
            format!(":{text}"),
            format!("{a}:{b}:{a}"),
            format!(
                "{}:{}",
                column_letters(first.column()),
                column_letters(last.column())
            ),
            format!("{}:{}", first.row(), last.row()),
            format!("${first}:{last}"),
            format!(" {text}"),
            format!("Sheet1!{text}"),
        ] {
            assert!(
                invalid.parse::<CellRange>().is_err(),
                "{case}: accepted {invalid:?}"
            );
        }

        let mut cells = range.cells();
        let count = range.cell_count();
        assert_eq!(
            cells.size_hint(),
            exact_hint(count),
            "{case}: initial size_hint"
        );
        if count > 5_000 {
            // Too large to collect: check the first row and the wrap to the next.
            let width = usize::from(range.width());
            let head: Vec<CellRef> = cells.by_ref().take(width + 1).collect();
            assert_eq!(head[0], first, "{case}: first cell");
            for (offset, item) in head.iter().take(width).enumerate() {
                assert_eq!(
                    *item,
                    cell(first.row(), first.column() + offset as u16),
                    "{case}: first row"
                );
            }
            if range.height() > 1 {
                assert_eq!(
                    head[width],
                    cell(first.row() + 1, first.column()),
                    "{case}: wrap"
                );
            }
            let remaining = count - head.len() as u64;
            assert_eq!(
                cells.size_hint(),
                exact_hint(remaining),
                "{case}: size_hint"
            );
            return;
        }
        let mut visited = Vec::new();
        while let Some(item) = cells.next() {
            visited.push(item);
            let remaining = count - visited.len() as u64;
            assert_eq!(
                cells.size_hint(),
                exact_hint(remaining),
                "{case}: size_hint"
            );
        }
        assert_eq!(cells.next(), None, "{case}: fused");
        assert_eq!(visited.len() as u64, count, "{case}: cells() count");
        assert_eq!(visited.first(), Some(&first), "{case}: first cell");
        assert_eq!(visited.last(), Some(&last), "{case}: last cell");
        assert!(
            visited
                .windows(2)
                .all(|pair| (pair[0].row(), pair[0].column()) < (pair[1].row(), pair[1].column())),
            "{case}: cells() is not in row-major order"
        );
        let set: BTreeSet<CellRef> = visited.iter().copied().collect();
        assert!(
            visited.iter().all(|&item| range.contains(item)),
            "{case}: contains"
        );
        for _ in 0..64 {
            let row = (i64::from(first.row())
                + rng.below(usize::try_from(range.height()).unwrap() + 4) as i64
                - 2)
            .clamp(1, i64::from(CellRef::MAX_ROW)) as u32;
            let column =
                (i64::from(first.column()) + rng.below(usize::from(range.width()) + 4) as i64 - 2)
                    .clamp(1, i64::from(CellRef::MAX_COLUMN)) as u16;
            let probe = cell(row, column);
            assert_eq!(
                range.contains(probe),
                set.contains(&probe),
                "{case}: contains({probe}) disagrees with cells()"
            );
        }
    });
}
