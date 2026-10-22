//! Optional export/edit/reopen check using an installed LibreOfficeKit.
//!
//! LibreOffice exports `fixtures/libreoffice-source.ods` as XLSX, the CLI edits
//! it, and LibreOffice reopens the default and compacted saves and exports them
//! as ODS, whose cells are checked here. The test is ignored by default:
//!
//! ```sh
//! cargo test --test libreoffice -- --ignored
//! ```
//!
//! `SHEETPATCH_LIBREOFFICE_PROGRAM` selects the installation's `program`
//! directory; otherwise the `libreoffice` or `soffice` launcher on `PATH`
//! locates it. LibreOffice is loaded through the system's dynamic loader, so
//! the check runs on Unix only.
#![cfg(unix)]
mod common;

use common::TempDir;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, XmlVersion};
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

const SOURCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/libreoffice-source.ods"
);

#[test]
#[ignore = "needs an installed LibreOfficeKit"]
fn libreoffice_reopens_default_and_compacted_edits() {
    let program = program();
    let temp = TempDir::new();
    let directory = fs::canonicalize(temp.path()).unwrap();
    let office = Office::new(&program, &directory);
    let original = directory.join("original.xlsx");
    office.convert(Path::new(SOURCE), &original, "xlsx");
    let edited = directory.join("edited.xlsx");
    let mut source = original.as_path();
    for (address, kind, value) in [
        ("A1", "text", "=literal text"),
        ("B1", "number", "21"),
        ("C1", "bool", "false"),
        ("E1", "text", "Café ☕ _x0041_\nnext line"),
        ("F3", "text", "Inserted"),
        ("F1", "error", "#N/A"),
        // A blank is a no-op for an absent cell, so first give F2 a value.
        ("F2", "text", "replaced by a blank"),
        ("F2", "blank", ""),
    ] {
        run(sheetpatch()
            .arg("set")
            .arg(source)
            .arg(&edited)
            .args(["Data", address, kind, value]));
        source = &edited;
    }
    let compacted = directory.join("compact.xlsx");
    run(sheetpatch().arg("compact").arg(&edited).arg(&compacted));
    for workbook in [&edited, &compacted] {
        let output = workbook.with_extension("ods");
        office.convert(workbook, &output, "ods");
        check_ods(&output);
    }
}

fn sheetpatch() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
}

fn run(command: &mut Command) {
    let status = command.status().unwrap();
    assert!(status.success(), "{command:?} failed: {status}");
}

/// LibreOffice's `program` directory: `SHEETPATCH_LIBREOFFICE_PROGRAM`, or the
/// directory of the resolved `libreoffice` or `soffice` launcher on `PATH`.
fn program() -> PathBuf {
    if let Some(program) = env::var_os("SHEETPATCH_LIBREOFFICE_PROGRAM") {
        return fs::canonicalize(&program).unwrap_or_else(|error| {
            panic!(
                "SHEETPATCH_LIBREOFFICE_PROGRAM {}: {error}",
                Path::new(&program).display()
            )
        });
    }
    let path = env::var_os("PATH").unwrap_or_default();
    let launcher = ["libreoffice", "soffice"]
        .iter()
        .find_map(|name| {
            env::split_paths(&path)
                .map(|directory| directory.join(name))
                .find(|candidate| {
                    fs::metadata(candidate).is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                })
        })
        .expect("install LibreOffice or set SHEETPATCH_LIBREOFFICE_PROGRAM");
    let launcher = fs::canonicalize(launcher).unwrap();
    launcher.parent().unwrap().to_owned()
}

/// A `file:` URL for an absolute path, percent-encoding every byte that is not
/// unreserved.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            write!(uri, "%{byte:02X}").unwrap();
        }
    }
    uri
}

fn c_string(text: impl Into<Vec<u8>>) -> CString {
    CString::new(text).unwrap()
}

// RTLD_NOW has the same value on Linux and macOS.
const RTLD_NOW: c_int = 2;

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
}

/// The dynamic loader's message for its most recent failure.
fn loader_error() -> String {
    // SAFETY: dlerror returns null or a NUL-terminated message that stays
    // valid until the loader's next call on this thread.
    unsafe {
        let message = dlerror();
        if message.is_null() {
            "unknown dynamic loader error".into()
        } else {
            CStr::from_ptr(message).to_string_lossy().into_owned()
        }
    }
}

/// A LibreOfficeKit object (`LibreOfficeKit` or `LibreOfficeKitDocument`).
#[repr(C)]
struct Handle<Functions> {
    table: *const Table<Functions>,
}

/// A function table, which starts with its own size in bytes.
#[repr(C)]
struct Table<Functions> {
    size: usize,
    functions: Functions,
}

/// The stable prefix of `LibreOfficeKitClass` after its size, from
/// LibreOfficeKit/LibreOfficeKit.h.
#[repr(C)]
struct OfficeFunctions {
    destroy: Option<unsafe extern "C" fn(*mut Handle<OfficeFunctions>)>,
    load: Option<
        unsafe extern "C" fn(
            *mut Handle<OfficeFunctions>,
            *const c_char,
        ) -> *mut Handle<DocumentFunctions>,
    >,
    error: Option<unsafe extern "C" fn(*mut Handle<OfficeFunctions>) -> *mut c_char>,
    _load_with_options: *const c_void,
    free_error: Option<unsafe extern "C" fn(*mut c_char)>,
}

/// The stable prefix of `LibreOfficeKitDocumentClass` after its size.
#[repr(C)]
struct DocumentFunctions {
    destroy: Option<unsafe extern "C" fn(*mut Handle<DocumentFunctions>)>,
    save: Option<
        unsafe extern "C" fn(
            *mut Handle<DocumentFunctions>,
            *const c_char,
            *const c_char,
            *const c_char,
        ) -> c_int,
    >,
}

/// The functions of `handle`, after checking that its table has all of them.
///
/// # Safety
///
/// `handle` must point to a live LibreOfficeKit object whose function table
/// starts with `Functions`' fields, and the result must not outlive it.
unsafe fn functions<'a, Functions>(handle: *mut Handle<Functions>) -> &'a Functions {
    // SAFETY: the caller passes a live object, and every table begins with its
    // size, so the size is readable whatever the table's length.
    let (table, size) = unsafe {
        let table = (*handle).table;
        (table, (&raw const (*table).size).read())
    };
    assert!(
        size >= size_of::<Table<Functions>>(),
        "LibreOfficeKit's API is too old for this check"
    );
    // SAFETY: the table is long enough to hold every declared function.
    unsafe { &(*table).functions }
}

/// An initialized LibreOfficeKit, destroyed on drop.
struct Office(*mut Handle<OfficeFunctions>);

impl Office {
    fn new(program: &Path, directory: &Path) -> Self {
        // SAFETY: this is its binary's only test, so no other thread reads or
        // writes the environment while LibreOffice is configured.
        unsafe {
            env::set_var("SAL_USE_VCLPLUGIN", "svp");
            env::set_var("XDG_CACHE_HOME", directory.join("cache"));
        }
        let library = [
            "libsofficeapp.so",
            "libmergedlo.so",
            "libsofficeapp.dylib",
            "libmergedlo.dylib",
        ]
        .iter()
        .map(|name| program.join(name))
        .find(|library| {
            fs::metadata(library).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 1000)
        })
        .unwrap_or_else(|| panic!("LibreOfficeKit library not found in {}", program.display()));
        let name = c_string(library.as_os_str().as_bytes());
        // SAFETY: the library is LibreOffice's own, which supports being
        // loaded this way. It is never unloaded.
        let library = unsafe { dlopen(name.as_ptr(), RTLD_NOW) };
        assert!(!library.is_null(), "{}", loader_error());
        // SAFETY: the library handle is valid and the name is NUL-terminated.
        let hook = unsafe { dlsym(library, c"libreofficekit_hook_2".as_ptr()) };
        assert!(!hook.is_null(), "{}", loader_error());
        type Hook =
            unsafe extern "C" fn(*const c_char, *const c_char) -> *mut Handle<OfficeFunctions>;
        // SAFETY: the symbol has this signature in LibreOfficeKitInit.h.
        let hook = unsafe { std::mem::transmute::<*mut c_void, Hook>(hook) };
        let install = c_string(program.as_os_str().as_bytes());
        let profile = c_string(file_uri(&directory.join("profile")));
        // SAFETY: both arguments are NUL-terminated; LibreOffice copies them.
        let office = unsafe { hook(install.as_ptr(), profile.as_ptr()) };
        assert!(!office.is_null(), "LibreOfficeKit could not initialize");
        // SAFETY: LibreOfficeKit returned a live object.
        unsafe { functions(office) };
        Self(office)
    }

    fn functions(&self) -> &OfficeFunctions {
        // SAFETY: the object stays live until drop, and `new` checked its table.
        unsafe { functions(self.0) }
    }

    fn error(&self) -> String {
        let functions = self.functions();
        // SAFETY: the object is live; the message is null or NUL-terminated
        // and owned by the caller, who frees it with `free_error`.
        unsafe {
            let message = functions.error.unwrap()(self.0);
            if message.is_null() {
                return "unknown LibreOfficeKit error".into();
            }
            let text = CStr::from_ptr(message).to_string_lossy().into_owned();
            functions.free_error.unwrap()(message);
            text
        }
    }

    fn convert(&self, source: &Path, destination: &Path, format: &str) {
        let url = c_string(file_uri(source));
        // SAFETY: the object is live and the URL is NUL-terminated.
        let document = unsafe { self.functions().load.unwrap()(self.0, url.as_ptr()) };
        assert!(
            !document.is_null(),
            "LibreOffice could not open {}: {}",
            source.display(),
            self.error()
        );
        // SAFETY: LibreOfficeKit returned a live document.
        unsafe { functions(document) };
        let document = Document(document);
        let url = c_string(file_uri(destination));
        let format = c_string(format);
        // SAFETY: the document is live and the strings are NUL-terminated; a
        // null filter selects the format's default options.
        let saved = unsafe {
            document.functions().save.unwrap()(
                document.0,
                url.as_ptr(),
                format.as_ptr(),
                std::ptr::null(),
            )
        };
        assert!(
            saved != 0 && destination.is_file(),
            "LibreOffice could not save {}: {}",
            destination.display(),
            self.error()
        );
    }
}

impl Drop for Office {
    fn drop(&mut self) {
        // SAFETY: the object is live and is not used again.
        unsafe { self.functions().destroy.unwrap()(self.0) }
    }
}

/// A document loaded by [`Office::convert`], destroyed on drop.
struct Document(*mut Handle<DocumentFunctions>);

impl Document {
    fn functions(&self) -> &DocumentFunctions {
        // SAFETY: the document stays live until drop, and `convert` checked
        // its table before wrapping it.
        unsafe { functions(self.0) }
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        // SAFETY: the document is live and is not used again.
        unsafe { self.functions().destroy.unwrap()(self.0) }
    }
}

const OFFICE: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
const TEXT: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
const STYLE: &str = "urn:oasis:names:tc:opendocument:xmlns:style:1.0";
const FO: &str = "urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0";
const CALCEXT: &str = "urn:org:documentfoundation:names:experimental:calc:xmlns:calcext:1.0";

/// An XML element with namespace-resolved names.
#[derive(Default)]
struct Element {
    namespace: String,
    name: String,
    attributes: Vec<(String, String, String)>,
    children: Vec<Node>,
}

enum Node {
    Element(Element),
    Text(String),
}

impl Element {
    fn is(&self, namespace: &str, name: &str) -> bool {
        self.namespace == namespace && self.name == name
    }

    fn get(&self, namespace: &str, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(n, local, _)| n == namespace && local == name)
            .map(|(_, _, value)| value.as_str())
    }

    /// Child elements, in order.
    fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|node| match node {
            Node::Element(element) => Some(element),
            Node::Text(_) => None,
        })
    }

    fn children(&self, namespace: &str, name: &str) -> impl Iterator<Item = &Element> {
        self.elements()
            .filter(move |element| element.is(namespace, name))
    }

    /// Descendant elements named `namespace`/`name`, in document order.
    fn descendants<'a>(&'a self, namespace: &str, name: &str, found: &mut Vec<&'a Element>) {
        for element in self.elements() {
            if element.is(namespace, name) {
                found.push(element);
            }
            element.descendants(namespace, name, found);
        }
    }

    /// All text inside the element, in document order.
    fn text(&self) -> String {
        let mut text = String::new();
        for node in &self.children {
            match node {
                Node::Element(element) => text.push_str(&element.text()),
                Node::Text(content) => text.push_str(content),
            }
        }
        text
    }

    fn push_text(&mut self, text: &str) {
        if let Some(Node::Text(last)) = self.children.last_mut() {
            last.push_str(text);
        } else {
            self.children.push(Node::Text(text.to_owned()));
        }
    }
}

fn namespace(result: ResolveResult<'_>) -> String {
    match result {
        ResolveResult::Bound(namespace) => namespace.into_inner().to_owned(),
        ResolveResult::Unbound => String::new(),
        ResolveResult::Unknown(prefix) => panic!("undeclared XML prefix {prefix}"),
    }
}

/// The root element of an XML document.
fn parse(xml: &[u8]) -> Element {
    let mut reader = NsReader::from_str(std::str::from_utf8(xml).unwrap());
    let mut open = vec![Element::default()];
    loop {
        let (resolved, event) = reader.read_resolved_event().unwrap();
        let resolved = namespace(resolved);
        match event {
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                let mut element = Element {
                    namespace: resolved,
                    name: tag.local_name().as_ref().to_owned(),
                    ..Element::default()
                };
                for attribute in tag.attributes() {
                    let attribute = attribute.unwrap();
                    let (result, name) = reader.resolver().resolve_attribute(attribute.key);
                    let value = attribute.normalized_value(XmlVersion::Implicit1_0).unwrap();
                    element.attributes.push((
                        namespace(result),
                        name.as_ref().to_owned(),
                        value.into_owned(),
                    ));
                }
                if matches!(event, Event::Start(_)) {
                    open.push(element);
                } else {
                    open.last_mut()
                        .unwrap()
                        .children
                        .push(Node::Element(element));
                }
            }
            Event::End(_) => {
                let element = open.pop().unwrap();
                open.last_mut()
                    .unwrap()
                    .children
                    .push(Node::Element(element));
            }
            Event::Text(text) => open.last_mut().unwrap().push_text(&text.xml10_content()),
            Event::CData(data) => open.last_mut().unwrap().push_text(&data.xml10_content()),
            Event::GeneralRef(reference) => {
                let character = reference.resolve_char_ref().unwrap();
                let text = match character {
                    Some(character) => character.to_string(),
                    None => quick_xml::escape::resolve_xml_entity(&reference)
                        .unwrap_or_else(|| panic!("undefined XML entity {:?}", &*reference))
                        .to_owned(),
                };
                open.last_mut().unwrap().push_text(&text);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    assert_eq!(open.len(), 1, "unclosed XML element");
    open.pop()
        .unwrap()
        .children
        .into_iter()
        .find_map(|node| match node {
            Node::Element(element) => Some(element),
            Node::Text(_) => None,
        })
        .unwrap()
}

/// A cell value as LibreOffice exported it.
#[derive(Debug, PartialEq)]
enum Value {
    Number(f64),
    Bool(bool),
    Text(String),
}

fn text(value: &str) -> Value {
    Value::Text(value.into())
}

fn repeated(element: &Element, attribute: &str) -> usize {
    element
        .get(TABLE, attribute)
        .map_or(1, |count| count.parse().unwrap())
}

/// Inspects values exported by LibreOffice after it has reopened the XLSX.
fn check_ods(path: &Path) {
    let archive = fs::read(path).unwrap();
    let content = parse(&common::payload(&archive, "content.xml"));
    let styles = parse(&common::payload(&archive, "styles.xml"));
    let mut style_list = Vec::new();
    for root in [&styles, &content] {
        root.descendants(STYLE, "style", &mut style_list);
    }
    let style_names: HashMap<&str, &Element> = style_list
        .into_iter()
        .filter_map(|style| Some((style.get(STYLE, "name")?, style)))
        .collect();
    let sheets: Vec<&Element> = content
        .children(OFFICE, "body")
        .flat_map(|body| body.children(OFFICE, "spreadsheet"))
        .flat_map(|spreadsheet| spreadsheet.children(TABLE, "table"))
        .collect();
    assert_eq!(
        sheets
            .iter()
            .map(|sheet| sheet.get(TABLE, "name"))
            .collect::<Vec<_>>(),
        [Some("Data"), Some("Notes")]
    );
    let mut cells = HashMap::new();
    for sheet in &sheets {
        let name = sheet.get(TABLE, "name").unwrap();
        let mut row_index = 0;
        for row in sheet.children(TABLE, "table-row") {
            if row_index >= 3 {
                break;
            }
            let mut column = 0;
            for cell in row.elements() {
                if column >= 6 {
                    break;
                }
                let repeat = repeated(cell, "number-columns-repeated");
                for offset in 0..repeat.min(6 - column) {
                    cells.insert((name, row_index, column + offset), cell);
                }
                column += repeat;
            }
            row_index += repeated(row, "number-rows-repeated");
        }
    }

    let cell = |sheet: &'static str, row: usize, column: usize| {
        *cells
            .get(&(sheet, row, column))
            .unwrap_or_else(|| panic!("no cell at {sheet} row {row} column {column}"))
    };
    let value = |sheet: &'static str, row: usize, column: usize| {
        let cell = cell(sheet, row, column);
        match cell.get(OFFICE, "value-type") {
            Some("float") => Value::Number(cell.get(OFFICE, "value").unwrap().parse().unwrap()),
            Some("boolean") => Value::Bool(cell.get(OFFICE, "boolean-value") == Some("true")),
            _ => Value::Text(
                cell.children(TEXT, "p")
                    .map(Element::text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        }
    };
    let is_bold = |cell: &Element| {
        let mut name = cell.get(TABLE, "style-name");
        let mut seen = HashSet::new();
        while let Some(current) = name
            && seen.insert(current)
            && let Some(style) = style_names.get(current)
        {
            let weight = style
                .children(STYLE, "text-properties")
                .next()
                .and_then(|properties| properties.get(FO, "font-weight"));
            if let Some(weight) = weight {
                return weight == "bold";
            }
            name = style.get(STYLE, "parent-style-name");
        }
        false
    };

    assert_eq!(value("Data", 0, 0), text("=literal text"));
    assert!(
        is_bold(cell("Data", 0, 0)),
        "edited cell lost its bold formatting"
    );
    assert_eq!(value("Data", 0, 1), Value::Number(21.0));
    assert_eq!(value("Data", 0, 2), Value::Bool(false));
    // Sheetpatch retains the original cache; an independent application may
    // either keep it or recalculate on import according to its own settings.
    let cached = value("Data", 0, 3);
    assert!(
        [Value::Number(25.0), Value::Number(42.0)].contains(&cached),
        "{cached:?}"
    );
    assert_eq!(
        cell("Data", 0, 3).get(TABLE, "formula"),
        Some("of:=[.B1]*2")
    );
    assert_eq!(value("Data", 0, 4), text("Café ☕ _x0041_\nnext line"));
    assert_eq!(value("Data", 2, 5), text("Inserted"));
    assert_eq!(value("Notes", 0, 0), text("Keep this sheet"));
    // An XLSX error literal must reopen as an error (LibreOffice currently
    // imports it as the formula of:=#N/A), not as text that looks like one.
    let error = cell("Data", 0, 5);
    assert_eq!(value("Data", 0, 5), text("#N/A"));
    assert!(
        error.get(CALCEXT, "value-type") == Some("error")
            || error.get(TABLE, "formula") == Some("of:=#N/A"),
        "error value reopened as text"
    );
    let blank = cells.get(&("Data", 1, 5));
    assert!(
        blank.is_none_or(
            |blank| blank.get(OFFICE, "value-type").is_none() && blank.text().is_empty()
        ),
        "blanked cell kept a value"
    );
}
