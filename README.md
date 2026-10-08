# sheetpatch

Lossless cell editing for **existing Excel workbooks**, written in Rust. Change
text, numbers, booleans, or clear cells while preserving charts, pivot tables,
macros, formatting, relationships, and unfamiliar XML.

The crate patches worksheet XML at byte offsets. It retains all untouched ZIP
entries, including their compressed bytes, headers, extra fields, and comments.
It never rebuilds the workbook from a spreadsheet object model.

## Use from Rust

Requires Rust 1.88 or later. Add `sheetpatch` as a local dependency:

```toml
[dependencies]
sheetpatch = { path = "/path/to/sheetpatch" }
```

```rust,no_run
use sheetpatch::{CellValue, Workbook};

fn main() -> sheetpatch::Result<()> {
    let mut book = Workbook::open("report.xlsm")?;

    for sheet in book.sheets() {
        println!("{}", sheet.name());
    }

    book.set_cell("Summary", "B2", "Updated & reviewed")?;
    book.set_cell("Summary", "C2", 42.5)?;
    book.set_cell("Summary", "D2", true)?;
    book.set_cell("Summary", "E2", CellValue::Blank)?;

    book.save("report-edited.xlsm")?;
    Ok(())
}
```

`Workbook::from_bytes(Vec<u8>)` and `Workbook::to_bytes()` support in-memory
workflows. Addresses use A1 notation, accept lowercase letters, and must fit
Excel's row and column limits. Strings remain text even when they start with `=`.
Numbers use `f64`; encode identifiers or integers requiring exact decimal digits
as text. To edit dates, write the Excel numeric serial into a cell that already
has the desired date format.

## Use from the command line

```sh
cargo build --release --locked
./target/release/sheetpatch list report.xlsm
./target/release/sheetpatch set report.xlsm edited.xlsm Summary B2 text 'Revised total'
./target/release/sheetpatch set report.xlsm edited.xlsm Summary C2 number 42.5
./target/release/sheetpatch set report.xlsm edited.xlsm Summary D2 bool true
./target/release/sheetpatch set report.xlsm edited.xlsm Summary E2 blank
```

Each `set` invocation starts from its input workbook. To accumulate CLI edits,
use the previous output as the next input, or use the same input and output path.
The Rust API can apply many edits before one save. `save` writes a temporary file
beside the destination and renames it into place after a successful write.
Keep the appropriate workbook extension, especially `.xlsm` for VBA workbooks.

## What lossless means

| Content | Behavior |
| --- | --- |
| Untouched package parts | Original payload, compressed bytes, and ZIP metadata are retained byte for byte. |
| Edited worksheet | Original XML outside the necessary edits is retained byte for byte. |
| Existing cell formatting | Style, other attributes, and unfamiliar sibling content are retained. |
| New cells and rows | Inserted in coordinate order; existing dimension and row-span hints are widened as needed. |
| Text | Written as inline strings; shared strings stay untouched. Whitespace, XML characters, and literal Excel escape sequences are preserved. |
| Blank values | Value and type are cleared; the cell and its formatting remain. Clearing a missing cell changes nothing. |
| Save without edits | The entire original archive is returned unchanged. |

The ZIP writer retains the original local-record area and appends replacements
for changed worksheets. Old worksheet bytes remain recoverable from unused ZIP
records, and repeated saves can increase the file size. This API does not erase
previous cell contents.

Charts, pivot tables, VBA projects, and other parts are preserved as opaque data.
Formula results and chart/pivot caches are not calculated or refreshed. Recalculate
formulas and refresh charts/pivots in Excel when changed inputs must be reflected
in cached results. The original workbook's calculation settings are retained.

## Supported scope

- Existing `.xlsx`, `.xlsm`, `.xltx`, and `.xltm` OOXML packages, including strict
  and transitional SpreadsheetML and namespace-prefixed worksheets.
- ZIP32 archives; accessed XML parts must use Stored or Deflate compression,
  be UTF-8 XML 1.0, and be at most 64 MiB. Other untouched compression methods
  are retained without decoding. Workbooks are held in memory.
- Explicit, sorted row and cell coordinates. Ambiguous or malformed worksheet
  layouts return an error before applying changes.
- Formula cells and shared, array, or data-table formula ranges are protected
  against overwriting. Existing formulas remain untouched. Formula creation,
  structural edits, style creation, and calculation are outside this version's scope.
- Merged cells can be edited through their top-left anchor. Other cells in a
  merged range are protected.
- Unfamiliar XML inside a value payload is protected against replacement;
  unfamiliar cell attributes and sibling elements are preserved.
- Legacy `.xls`, binary `.xlsb`, encrypted workbooks, ZIP64, multi-disk ZIP,
  and XML DTDs are unsupported. Digitally signed OOXML packages can be opened
  and copied unchanged, but editing is refused to avoid invalidating signatures.

## Dependencies and checks

Only two direct dependencies, both with default features disabled:

- [`quick-xml`](https://docs.rs/quick-xml/0.41.0/quick_xml/): namespace-aware parsing.
- [`flate2`](https://docs.rs/flate2/1.1.9/flate2/): pure Rust Deflate compression.

There are no additional CLI or test dependencies. ZIP records and CRC32 are
handled in the crate so untouched records can remain unchanged.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
cargo build --locked --release
cargo package --locked
```

GitHub CI checks formatting, Clippy, and documentation with warnings treated as
errors. Tests run on Linux, macOS, and Windows with stable Rust, and on Linux with
the minimum supported Rust 1.88.0. Release builds and a clean package build must
also pass. The aggregate `CI` check requires every job to succeed. Actions are
pinned to full commit hashes, workflow permissions are read-only, and Dependabot
checks Cargo dependencies and GitHub Actions weekly.

Preservation tests construct workbook archives and independently compare
decompressed payloads, compressed streams, and central-directory metadata for
charts, pivot caches, macros, styles, shared strings, and custom parts. They also
cover unknown XML within edited worksheets, formula protection, namespace handling,
invalid inputs, and atomic saves. These checks verify byte preservation; they do
not launch Excel or execute VBA.

MIT licensed.
