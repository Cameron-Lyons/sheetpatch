# sheetpatch

Lossless cell editing for **existing Excel workbooks**, written in Rust. Change
text, numbers, booleans, errors, or clear cells while preserving charts, pivot tables,
macros, formatting, relationships, and unfamiliar XML.

The crate patches worksheet XML at byte offsets. It retains all untouched ZIP
entries, including their compressed bytes, local headers, extra fields, and comments.
Central-directory offsets are adjusted when changed records move later entries.
It never rebuilds the workbook from a spreadsheet object model.

## Use from Rust

Requires Rust 1.88 or later. Once 1.0 is published, add the crate dependency:

```toml
[dependencies]
sheetpatch = "1.0"
```

For an unpublished checkout, use `sheetpatch = { path = "/path/to/sheetpatch" }`.

```rust,no_run
use sheetpatch::{CellEdit, CellValue, Workbook};

fn main() -> sheetpatch::Result<()> {
    let mut book = Workbook::open("report.xlsm")?;

    for sheet in book.sheets() {
        println!("{}", sheet.name());
    }

    book.set_cell("Summary", "B2", "Updated & reviewed")?;
    book.set_cell("Summary", "C2", 42.5)?;
    book.set_cell("Summary", "D2", true)?;
    book.set_cell("Summary", "E2", CellValue::Blank)?;

    // A batch parses each affected worksheet once.
    book.set_cells("Summary", [("C3", 12.5), ("C4", 25.0)])?;

    // A transaction across worksheets commits only after all edits succeed.
    book.apply_edits([
        CellEdit::new("Summary", "B3", "Reviewed")?,
        CellEdit::new("Summary", "B4", "Approved")?,
    ])?;

    let cell = book.get_cell("Summary", "C3")?;
    println!("{:?}", cell.value);

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

Use `set_cells` for many edits on one sheet and `apply_edits` for transactions
across sheets. Duplicate addresses use the last value; all inputs are validated.
Failed transactions leave previous pending edits intact. `has_changes()` reports
pending changes and `reset_changes()` restores the original archive.
Worksheet names match exactly, including case, and `sheets()` lists editable
worksheets in workbook order; chart sheets are retained as opaque parts. Saving
does not clear pending changes or replace the in-memory original archive. Reopen
the saved file to use it as the new baseline.

`get_cell` returns a `CellContent` with a scalar `value`, stored `formula` text,
and `style_index`. `get_cells` reads multiple addresses with one worksheet parse,
preserving input order. Shared strings and rich text are decoded without changing
the workbook; formula values are cached results. Shared-formula follower cells
may contain empty stored formula text. `CellRef::new(row, column)` supports typed,
one-based addresses and `CellEdit::at` accepts them directly.

`write_to` streams a workbook to any `std::io::Write` implementation. File saves
use that same path, avoiding allocation of another complete ZIP. Worksheets are
decompressed lazily and cached; shared strings are loaded only when a requested
cell needs them.

## Use from the command line

After publication, install with `cargo install sheetpatch --version 1.0.0 --locked`.
From a checkout, use `cargo install --path . --locked` or build as shown below.

```sh
cargo build --release --locked
./target/release/sheetpatch --version
./target/release/sheetpatch list report.xlsm
./target/release/sheetpatch set report.xlsm edited.xlsm Summary B2 text 'Revised total'
./target/release/sheetpatch set report.xlsm edited.xlsm Summary C2 number 42.5
./target/release/sheetpatch set report.xlsm edited.xlsm Summary D2 bool true
./target/release/sheetpatch set report.xlsm edited.xlsm Summary E2 blank
./target/release/sheetpatch get report.xlsm Summary B2
./target/release/sheetpatch patch report.xlsm edited.xlsm edits.tsv
./target/release/sheetpatch patch report.xlsm edited.xlsm - < edits.tsv
./target/release/sheetpatch compact edited.xlsm compact.xlsm
```

`get` prints the current scalar value, or an empty line for a blank cell. A patch
file contains tab-separated `sheet`, `cell`, `type`, and `value` fields, one edit
per line. Types are `text`, `number`, `bool`, `error`, and `blank`; `blank` may omit
its value. Text keeps trailing whitespace and additional tabs. UTF-8 BOM and
CRLF files are supported. Empty lines and tab-free lines beginning with `#` are
ignored. Literal multiline values use the Rust API. Invalid rows report their
line number and leave the destination unchanged.

`--help` (`-h`) and `--version` (`-V`) exit successfully. Invalid command syntax,
patch syntax, or non-Unicode text arguments exit with code 2; workbook and I/O
errors exit with code 1. Native filesystem paths are accepted even when they
cannot be represented as Unicode. Output handles a closed pipe without a panic.

Each `set` invocation starts from its input workbook. To accumulate CLI edits,
use the previous output as the next input, or use the same input and output path.
The Rust API can apply many edits before one save. `save` writes a temporary file
beside the destination and renames it into place after a successful write.
Keep the appropriate workbook extension, especially `.xlsm` for VBA workbooks.
Existing destination permissions are applied to the temporary file before
workbook contents are written. Saves replace the destination directory entry;
an output symlink is replaced, rather than writing through to its target.

## What lossless means

| Content | Behavior |
| --- | --- |
| Untouched package parts | Original payload, compressed bytes, local headers, extra fields, and comments are retained byte for byte; directory offsets may change. |
| Edited worksheet | Original XML outside the necessary edits is retained byte for byte. |
| Existing cell formatting | Style, other attributes, and unfamiliar sibling content are retained. |
| New cells and rows | Inserted in coordinate order; existing dimension and row-span hints are widened as needed. |
| Text | Written as inline strings; shared strings stay untouched. Whitespace, XML characters, and literal Excel escape sequences are preserved. |
| Blank values | Value and type are cleared; the cell and its formatting remain. Clearing a missing cell changes nothing. |
| Save without edits | The entire original archive is returned unchanged. |

The ZIP writer replaces changed worksheet records in their original physical
order, without adding obsolete copies or gaps to a contiguous archive. Untouched
local records and any pre-existing opaque prefixes or gaps are retained. Repeated
saves do not accumulate old worksheet copies; the file size varies with the new
payloads. This API does not securely erase old values from shared strings, caches,
other package parts, copies, or filesystem history.

Explicit `save_compact`, `to_bytes_compact`, and `write_compact_to` remove obsolete
ZIP records. Untouched active local headers, compressed bytes, and descriptors
remain unchanged; their central-directory offsets are adjusted. Compaction
refuses opaque ZIP prefixes or gaps it cannot safely identify. It produces a
smaller workbook when obsolete records are present, while protecting unfamiliar
archive data. Use it to remove the unused records left by sheetpatch 0.2's append
writer before opening those files in applications that reject ZIP gaps. It is
not a secure erasure operation for copies or filesystem history.

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

There are no additional Rust CLI or test dependencies. ZIP records and CRC32 are
handled in the crate so untouched records can remain unchanged.

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps
cargo build --locked --release --all-targets --all-features
cargo package --locked
```

GitHub CI checks formatting, Clippy, and documentation with warnings treated as
errors. Tests run on Linux, macOS, and Windows with stable Rust, and on Linux with
the minimum supported Rust 1.88.0. Release builds and a clean package build must
also pass. An independent LibreOffice export/edit/reopen check runs on Linux.
The aggregate `CI` check requires every job to succeed. Actions are
pinned to full commit hashes, workflow permissions are read-only, and Dependabot
checks Cargo dependencies and GitHub Actions weekly.

## Performance

Batch edits parse each affected worksheet once and apply all XML changes in one
pass. Indexed ZIP/sheet lookup and cached decompression avoid repeated archive
scans. Streaming saves hold one replacement compressed part at a time, in
addition to the input archive, cached original XML, and pending worksheet XML.

Run the dependency-free benchmark with:

```sh
SHEETPATCH_BENCH_SAMPLES=3 cargo bench --locked --bench editing
```

The harness independently checks expected worksheet XML and untouched compressed
ZIP parts before measuring, and compares sequential edits, batching, streaming,
and compaction. Initial 0.2 measurements used one release-mode sample per synthetic
workload on an Intel Core Ultra 5 325, Linux x86_64, Rust 1.99.0:

| Worksheet cells / edits | Original sequential editor | Batch editor |
| --- | ---: | ---: |
| 10,000 / 100 | 1.335 s | 14.185 ms |
| 50,000 / 500 | 43.954 s | 74.536 ms |

The original editor was the initial public implementation at commit
`532f27d6b3815b436b3ed1862f4a633001e595eb`, measured with the same generated fixtures.
These timings describe those workloads, not a guarantee for every workbook.
The pre-1.0 append writer grew to 3,354,749 bytes after five saves of the 50,000-cell
fixture; compaction kept it at 1,434,574 bytes. The 1.0 writer replaces changed
records, avoiding that growth. Streaming avoids allocating the output archive in
memory.

Preservation tests construct workbook archives and independently compare
decompressed payloads, compressed streams, and central-directory metadata for
charts, pivot caches, macros, styles, shared strings, and custom parts. They also
cover unknown XML within edited worksheets, formula protection, namespace handling,
invalid inputs, and atomic saves. These checks verify byte preservation; they do
not launch Excel or execute VBA. A checked-in LibreOffice-exported workbook adds
coverage for real worksheet layouts, styles, shared strings, and cached formulas.
An optional independent round-trip check requires installed LibreOfficeKit:

```sh
python3 scripts/check_interoperability.py
```

The script exports a workbook, applies CLI edits, and verifies both normal and
compacted files through LibreOffice. It does not require an additional Rust or
Python package dependency.

MIT licensed.
