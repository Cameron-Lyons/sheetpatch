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
use sheetpatch::{CellEdit, CellRange, CellRef, CellValue, Workbook};

fn main() -> sheetpatch::Result<()> {
    let mut book = Workbook::open("report.xlsm")?;

    for sheet in book.sheets() {
        println!("{} ({:?})", sheet.name(), sheet.visibility());
    }

    book.set_cell("Summary", "B2", "Updated & reviewed")?;
    book.set_cell("Summary", "C2", 42.5)?;
    book.set_cell("Summary", "D2", true)?;
    book.set_cell("Summary", "E2", CellValue::Blank)?;

    // Typed addresses use one-based row and column numbers.
    let target = CellRef::new(5, 3)?;
    book.set_cell_at("Summary", target, 37.5)?;

    // A batch parses each affected worksheet once.
    book.set_cells("Summary", [("C3", 12.5), ("C4", 25.0)])?;

    // Ranges visit their cells row by row, pairing with the typed batches.
    let block: CellRange = "B6:D7".parse()?;
    book.set_cells_at("Summary", block.cells().zip([1, 2, 3, 4, 5, 6]))?;
    let block_values = book.get_cells_at("Summary", block.cells())?;
    let total: f64 = block_values.iter().filter_map(|cell| cell.value.as_number()).sum();
    println!("block total: {total}");

    // A transaction across worksheets commits only after all edits succeed.
    book.apply_edits([
        CellEdit::new("Summary", "B3", "Reviewed")?,
        CellEdit::new("Summary", "B4", "Approved")?,
    ])?;

    let cell = book.get_cell_at("Summary", target)?;
    println!("{} {:?}", cell.value, cell.formula);

    book.save("report-edited.xlsm")?;
    Ok(())
}
```

`Workbook::from_bytes(Vec<u8>)` and `Workbook::to_bytes()` support in-memory
workflows. Addresses use A1 notation, accept lowercase letters, and must fit
Excel's row and column limits (`CellRef::MAX_ROW`, `CellRef::MAX_COLUMN`).
`CellRange` parses `"A1:C3"` with corners in any order, or a single cell, and
provides `first`, `last`, `height`, `width`, `cell_count`, `contains`, and
`cells`.

### Values

Editing methods accept any `impl Into<CellValue>`. Strings (`&str`, `String`,
`&String`, `Cow<str>`) and `char` become text, and remain text even when they
start with `=`. `f64` and the exactly convertible integers (`i8`, `i16`, `i32`,
`u8`, `u16`, `u32`) become numbers, `bool` becomes a boolean, and `Option<T>`
maps `None` to `CellValue::Blank`, which clears the value. `i64`, `u64`, `isize`,
and `usize` convert with `CellValue::try_from`, which returns
`Error::InvalidValue` above 2^53 in magnitude, where `f64` stops representing
every integer; write longer identifiers as text. There is deliberately no `f32`
conversion. Error values must be Excel error codes, such as `#N/A` or `#DIV/0!`
(see [Supported scope](#supported-scope)). `as_text`, `as_number`, `as_bool`,
`as_error`, and `is_blank` return typed views; `Display` prints a value as the
CLI's `get` does, without applying number formats.

To edit dates, write the Excel numeric serial into a cell that already has a date
format, or a new cell whose row or column style supplies one. Serials depend on
the workbook's date system: `uses_1904_dates()` reports whether serials count
from 1904-01-01 (serial 0) instead of the 1900 system, in which 1900-01-01 is
serial 1. For example, 2024-01-15 is 45306 in the 1900 system and 43844 in the
1904 system. ISO 8601 date cells (`t="d"`) read as serials in the workbook's system.

### Batches and transactions

Use `set_cells` for many edits on one sheet and `apply_edits` for transactions
across sheets. Duplicate addresses use the last value; all inputs are validated.
Failed transactions leave previous pending edits intact. `has_changes()` reports
pending changes and `reset_changes()` restores the original archive.
`get_cell_at`, `get_cells_at`, `set_cell_at`, `set_cells_at`, and `CellEdit::at`
accept typed `CellRef` addresses directly, avoiding conversion through A1 strings.
Typed batches preserve input order for reads and use the last value for duplicate
edits, with the same transaction and value validation rules as A1 batches.
Saving does not clear pending changes or replace the in-memory original archive.
Reopen the saved file to use it as the new baseline.

### Workbooks and reads

`sheets()` lists editable worksheets in workbook order, hidden ones included;
chart sheets are retained as opaque parts. `sheet(name)` finds one worksheet,
`Sheet::visibility()` reports hidden and very hidden tabs, and `is_signed()`
reports digitally signed packages, whose edits are refused. Worksheet names
match exactly, including case. Package discovery resolves URI-escaped
relationship targets and compares part paths without ASCII case, retaining the
actual ZIP member names. Ambiguous equivalent names for an accessed part are rejected.

`get_cell` returns a `CellContent` with a scalar `value`, stored `formula` text,
and `style_index`. Its fields are public for reading; it is `#[non_exhaustive]`,
so it is not constructed outside the crate and destructuring needs `..`.
`get_cells` reads multiple addresses with one worksheet parse, preserving input
order. Shared strings and rich text are decoded without changing the workbook;
formula values are cached results. Shared-formula follower cells may contain
empty stored formula text.

For repeated reads from one worksheet, `prepare_sheet` returns a `WorksheetView`
that parses and validates the worksheet once:

```rust,no_run
let book = sheetpatch::Workbook::open("report.xlsx")?;
let sheet = book.prepare_sheet("Summary")?;
let total = sheet.get_cell("C2")?;
let details = sheet.get_cells(["C3", "C4", "C3"])?;
if let Some(range) = sheet.used_range() {
    let everything = sheet.get_cells_at(range.cells())?;
}
# Ok::<(), sheetpatch::Error>(())
```

The view includes pending edits, preserves read order and duplicates, and loads
shared strings only when requested values need them. `cell_refs()` lists every
cell in row-major order, and `used_range()` returns the smallest range containing
them, style-only cells included; both come from the cells themselves, not the
stored `<dimension>`. The view borrows the workbook; drop it before making edits.
Dropping the view releases the parsed index. Ordinary reads retain cached
decompressed XML but release their index after each call. Both workbooks and
prepared views can be shared across threads.

Errors raised inside a worksheet name it and, where one applies, the cell, as in
`unsupported: worksheet "Summary": B2 contains a formula`. `Error` is
non-exhaustive, so matches need a fallback arm.

`write_to` streams a workbook to any `std::io::Write` implementation and flushes
it before returning; a failing writer may hold a partial archive. File saves use
that same path, avoiding allocation of another complete ZIP. Worksheets are
decompressed lazily and cached; shared strings are loaded only when a requested
cell needs them.

## Use from the command line

After publication, install with `cargo install sheetpatch --version 1.0.0 --locked`.
From a checkout, use `cargo install --path . --locked` or build as shown below.

```sh
cargo build --release --locked
./target/release/sheetpatch --version
./target/release/sheetpatch list report.xlsm
./target/release/sheetpatch list --json report.xlsm
./target/release/sheetpatch get report.xlsm Summary B2
./target/release/sheetpatch get report.xlsm Summary B2 C2 D2
./target/release/sheetpatch get report.xlsm Summary B2:D4
./target/release/sheetpatch get --json report.xlsm Summary B2 C2:D3
./target/release/sheetpatch cells report.xlsm Summary
./target/release/sheetpatch cells --json report.xlsm Summary A1:F20
./target/release/sheetpatch set report.xlsm edited.xlsm Summary B2 text 'Revised total'
./target/release/sheetpatch set report.xlsm edited.xlsm Summary C2 number 42.5
./target/release/sheetpatch set report.xlsm edited.xlsm Summary D2 bool true
./target/release/sheetpatch set report.xlsm edited.xlsm Summary E2 blank
./target/release/sheetpatch patch report.xlsm edited.xlsm edits.tsv
./target/release/sheetpatch patch report.xlsm edited.xlsm - < edits.tsv
./target/release/sheetpatch patch --check report.xlsm edits.tsv
./target/release/sheetpatch patch --check report.xlsm - < edits.tsv
./target/release/sheetpatch compact edited.xlsm compact.xlsm
./target/release/sheetpatch help patch
```

### Reading

`get` accepts one or more addresses or ranges such as `B2:D4` (corners in any
order) and prints each current scalar value in argument order, with ranges
expanded row by row, or an empty line for a blank cell. One `get` reads at most
1,048,576 cells. The worksheet is parsed once; all reads must succeed before any
values are printed. Text can include embedded newlines or tabs. Use `get --json`
for unambiguous machine-readable output: a JSON array of objects with `sheet`,
normalized A1 `cell`, `type`, `value`, `formula`, and `style_index` fields. Types
are `text`, `number`, `bool`, `error`, and `blank`; blank values and absent
formula/style fields use `null`. Formula values are stored caches, and
shared-formula followers may have an empty formula string.

`cells` lists the cells that hold a value or formula, row by row, skipping cells
that hold only formatting. An optional RANGE limits the list without being
expanded, so a whole-sheet range is cheap. Plain output has one
`CELL<TAB>TYPE<TAB>VALUE` line per cell; prefixing each line with the sheet name
and a tab gives patch rows for single-line values of cells without formulas.
`cells --json` returns the same objects as `get --json`. An empty worksheet prints
nothing, or `[]`.

`list` prints worksheet names in workbook order, hidden ones included.
`list --json` returns objects with `name`, package `path`, and `visibility`
(`visible`, `hidden`, or `veryHidden`).

### Editing and patch files

Each `set` invocation starts from its input workbook. To accumulate CLI edits,
use the previous output as the next input, or use the same input and output path.
The Rust API can apply many edits before one save. Keep the appropriate workbook
extension, especially `.xlsm` for VBA workbooks.

A patch file contains tab-separated `sheet`, `cell`, `type`, and `value` fields, one edit
per line. Types are `text`, `number`, `bool`, `error`, and `blank`; `blank` may omit
its value. Text keeps trailing whitespace and additional tabs. Patch files must
use UTF-8, optionally with a BOM, and LF or CRLF line endings. A carriage return
followed by more data on the same line is rejected rather than merging classic
Mac lines into one value; one that ends the file, or directly precedes a CRLF
ending, stays in the value. Empty lines and tab-free lines beginning with `#` are ignored.
Literal multiline values use the Rust API. Invalid rows and rows naming a missing
worksheet report their line number; a refused edit names the last line for that
cell, as in `Patch line 5: unsupported: worksheet "Summary": D2 contains a formula`.
Any error leaves the destination unchanged.

`patch --check` validates the complete transaction and output generation,
including worksheet names, formula and table-header protection, merged ranges,
value metadata, and ZIP size limits, without saving. It reports the number of
input edit rows, including duplicates. The same patch can then be applied with
`patch INPUT OUTPUT PATCH.tsv`. Check mode returns the same validation errors as
an actual edit. Generated ZIP bytes are streamed to a sink; no complete output
archive is allocated or written to disk. An empty patch succeeds for a supported workbook.

### Arguments, streams, and exit codes

Options (`--json`, `--check`, `--help`/`-h`) must come right after the command.
Unknown or repeated options, and option-like arguments where a path belongs,
are usage errors, so a misplaced flag never becomes a file name; write a file
whose name starts with `-` as `./-name`. Sheet names and values may start with `-`.

Use `-` as INPUT to read the workbook from standard input, and as OUTPUT to write
it to standard output; each is refused on a terminal, and INPUT and PATCH cannot
both be `-`. Output to standard output is streamed, not atomic:

```sh
./target/release/sheetpatch set - - Summary B2 number 5 < report.xlsm > edited.xlsm
./target/release/sheetpatch patch report.xlsm - edits.tsv > edited.xlsm
```

`--help` (`-h`) and `--version` (`-V`) exit successfully. `help COMMAND` and
`COMMAND --help` show focused command instructions; `help` shows the full
command list. Invalid command syntax, addresses, ranges or values, patch syntax,
or non-Unicode text arguments exit with code 2: syntax errors print the usage,
and other invalid input prints one line pointing to `sheetpatch help COMMAND`.
Workbook errors, missing worksheets, and I/O errors exit with code 1; I/O
messages name the path or stream, as in `cannot save 'edited.xlsm': …`.
Native filesystem paths are accepted even when they cannot be represented as Unicode.

Text output (`list`, `get`, `cells`, help, and `patch --check`) ends quietly with
code 0 when its reader closes the pipe, as `head` does; other write failures, such
as a full disk, exit with code 1. A workbook written to standard output must be
complete, so any failure to write it, including a closed pipe, exits with code 1
and `cannot write workbook to standard output`. On a terminal, plain `get` and
`cells` output shows control characters other than tab and line feed (C0, DEL,
and C1) as `\u{..}` escapes, and `list` escapes every control character in sheet
names. Pipes, files, and `--json` output receive the exact text.

## Saving

`save`, `save_compact`, and CLI file outputs write a temporary file beside the
destination and rename it into place after a successful write. The original
destination is retained if generation or writing fails, and the input path can be
the destination. The destination must name a file: a path without a file name,
and an existing directory, FIFO, device, or socket (also behind a symlink),
return `Error::Io` of kind `InvalidInput` before any output is generated.

Existing destination permissions are applied to the temporary file before
workbook contents are written. On Unix the replacement also keeps the
destination's group when the user may assign it; otherwise it gets no group
permissions. Owner, ACLs, and extended attributes are not preserved, because the
replacement is a new file. On Unix, a new destination saved from a workbook
opened with `Workbook::open`, including CLI outputs from an input file, is created
with the input's permission bits restricted by the umask, as `cp` does, so it is
never more permissive than the input; `from_bytes` and standard input use the
default mode. Saves replace the destination directory entry: an output symlink
to a regular file is replaced rather than written through, and hard links to the
old file keep the old contents. On Unix, like `sed -i`, a save replaces a
read-only destination as long as its directory is writable; Windows refuses to
replace a read-only destination. After the rename, Unix saves sync the directory
(best effort) so a completed save survives a crash.

## What lossless means

| Content | Behavior |
| --- | --- |
| Untouched package parts | Original payload, compressed bytes, local headers, extra fields, and comments are retained byte for byte; directory offsets may change. |
| Edited worksheet | Original XML outside the necessary edits is retained byte for byte. |
| Existing cell formatting | Style, other attributes, and unfamiliar sibling content are retained. |
| New cells and rows | Inserted in coordinate order. A new cell takes its row's style when the row has a custom format, otherwise its column's style; existing cells are never restyled. Existing dimension and row-span hints are widened as needed. |
| Text | Written as inline strings; shared strings stay untouched. Whitespace, XML characters, and literal Excel escape sequences are preserved. Characters XML 1.0 forbids, such as NUL and other control characters, are stored as Excel's `_xHHHH_` escapes and read back exactly. |
| Unchanged values | Writing the value a cell already stores, including a shared string's own text, leaves the worksheet unchanged; edits may load the shared-string table to compare. Protections still apply. |
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
  be UTF-8 XML 1.0, and be at most 64 MiB, as must saved replacements. Pending
  worksheet XML of 4 GiB or more cannot be indexed and is refused. Other untouched
  compression methods are retained without decoding. ZIP patched-data entries can
  only be preserved untouched. Workbooks are held in memory; `Workbook::open`
  refuses input larger than the largest possible ZIP32 archive, about 8 GiB.
- Archives that other ZIP readers could interpret differently are rejected:
  an end-of-central-directory signature after the real record (for example,
  inside the archive comment), Info-ZIP Unicode Path extra fields that rename a
  member, conflicting local and central headers, and overlapping records.
  `[Content_Types].xml` is found regardless of ASCII case; case-variant duplicates
  are ambiguous.
- Explicit, sorted row and cell coordinates. Ambiguous or malformed worksheet
  layouts return an error before applying changes.
- Unambiguous workbook metadata: invalid sheet `state` or `date1904` values, more
  than one `workbookPr`, and duplicate worksheet targets are rejected when
  opening. XML nested deeper than 256 elements is unsupported.
- Formula cells and shared, array, or data-table formula ranges are protected
  against overwriting. Existing formulas remain untouched. Formula creation,
  structural edits, style creation, and calculation are outside this version's scope.
- Excel table header cells are protected: an edit may rewrite a header's current
  value but not change it, since Excel requires header text to match the table's
  column names. Table bodies stay editable. Edits read the worksheet's table
  relationships and parts; missing or malformed ones refuse edits to that worksheet.
- Written error values must be one of Excel's error codes: `#NULL!`, `#DIV/0!`,
  `#VALUE!`, `#REF!`, `#NAME?`, `#NUM!`, `#N/A`, `#GETTING_DATA`, `#SPILL!`,
  `#CALC!`, `#FIELD!`, `#BLOCKED!`, `#CONNECT!`, `#BUSY!`, `#UNKNOWN!`,
  `#PYTHON!`, or `#EXTERNAL!`. Reads return any stored error token.
- ISO 8601 date cells (`t="d"`) read as numeric serials; rewriting one, even with
  its current serial, stores a plain number. Values without a serial, such as
  other UTC offsets or dates before the workbook's epoch, are refused on read.
- Cells with value metadata (`vm`, including linked and rich data types) are
  preserved; edits that would change their scalar value or representation are
  refused so metadata cannot remain attached to a replacement value.
- Merged cells can be edited through their top-left anchor. Other cells in a
  merged range are protected.
- Cell values, formulas, and merged ranges inside unsupported XML containers
  are refused when accessing that worksheet, preventing hidden content from
  bypassing these protections.
- Unfamiliar XML inside a value payload is protected against replacement;
  unfamiliar cell attributes and sibling elements are preserved.
- String reads refuse hidden text or shared-string entries in unsupported XML
  containers, preventing partial values or shifted shared-string indices.
- Legacy `.xls` and password-encrypted workbooks (OLE compound files) are
  detected and return `Error::Unsupported`, as do binary `.xlsb`, encrypted ZIP
  entries, ZIP64, multi-disk ZIP, and XML DTDs. Digitally signed OOXML packages
  can be opened and copied unchanged, but editing is refused to avoid
  invalidating signatures.

## Dependencies and checks

Three direct dependencies; XML parsing and compression disable default features:

- [`quick-xml`](https://docs.rs/quick-xml/0.42.0/quick_xml/): namespace-aware parsing.
- [`flate2`](https://docs.rs/flate2/1.1.10/flate2/): pure Rust Deflate compression.
- [`crc32fast`](https://docs.rs/crc32fast/1.5.2/crc32fast/): optimized ZIP checksums.

There are no additional Rust CLI or test dependencies. ZIP records are handled
in the crate so untouched records can remain unchanged.

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps
cargo build --locked --release --all-targets --all-features
cargo package --locked
```

GitHub CI runs for pull requests, pushes to `main` and `v*` tags, manual
dispatch, and a weekly schedule; a newer push to a pull request cancels its
superseded run, while other runs always complete. It checks formatting, Clippy
for the host and the Windows MSVC target, and documentation, with warnings
treated as errors. Tests run on Linux, macOS, and Windows with stable Rust, and
on Linux with the minimum supported Rust 1.88.0. The package job builds release
targets, runs the benchmark's correctness checks with one sample of a fast
operation subset, verifies `cargo package` and a publishing dry run, then tests,
installs, and inspects the extracted package. An independent LibreOffice
export/edit/reopen check runs on Linux. `cargo audit` checks Cargo.lock against
the RustSec database on every run and weekly, failing on vulnerable, unsound, or
yanked dependencies; scheduled runs perform only this audit. The aggregate `CI`
check requires every job to succeed. Actions are pinned to full commit hashes,
workflow permissions are read-only, and Dependabot checks Cargo dependencies and
GitHub Actions weekly.

Preservation tests construct workbook archives and independently compare
decompressed payloads, compressed streams, and central-directory metadata for
charts, pivot caches, macros, styles, shared strings, and custom parts. They also
cover unknown XML within edited worksheets, formula protection, namespace handling,
invalid inputs, and atomic saves. Deterministic randomized property tests check
reads after writes, saved archives, and protection guards against an independent
model of each generated workbook; `SHEETPATCH_PROPERTY_SEED` and
`SHEETPATCH_PROPERTY_CASES` explore other inputs. These checks verify byte
preservation; they do not launch Excel or execute VBA. A checked-in
LibreOffice-exported workbook adds coverage for real worksheet layouts, styles,
shared strings, and cached formulas. An optional independent round-trip check
requires installed LibreOfficeKit:

```sh
python3 scripts/check_interoperability.py
```

The script exports a workbook, applies CLI edits, and verifies both normal and
compacted files through LibreOffice. It does not require an additional Rust or
Python package dependency.

## Performance

Batch edits parse each affected worksheet once and apply all XML changes in one
pass. Formula and merged-range guards use direct membership checks for small
collections of edits or ranges. Larger batches exclude ranges outside the edited
cells' bounding rectangle before allocating a sweep index. Indexed ZIP/sheet
lookup and cached decompression avoid repeated archive scans, and ZIP physical
order is cached for repeated saves.

Worksheet parsing borrows names and attribute values from the original XML and
reads each element's attributes in a single pass into compact records with
32-bit offsets. Direct children use ranges in the parse index, attributes and row
cells use shared buffers, and normalized namespace URIs use compact identifiers.
One patch plan applies payload and attribute changes directly to worksheet spans;
unchanged cell extensions are copied only into the final output. Edits that
leave a worksheet unchanged retain its current XML without copying it. Shared
strings are decoded while streaming, releasing each entry's XML as it completes,
and package discovery validates whole parts but retains only the elements it
needs. Prepared worksheet views reuse a validated index across reads, with
retention controlled by the view's lifetime.

Streaming saves borrow ZIP metadata and hold one replacement compressed part at
a time, in addition to the input archive, cached original XML, and pending
worksheet XML. Changed parts resolve to archive entry indexes once per save, and
normal and compact saves share one record emitter with their respective
gap-preservation rules. Replacement parts are recompressed with Deflate level 4,
the fastest level that keeps lazy matching; untouched parts keep their original
compressed bytes.

Run the dependency-free benchmark with:

```sh
SHEETPATCH_BENCH_SAMPLES=3 cargo bench --locked --bench editing
```

The harness independently checks expected worksheet XML and untouched compressed
ZIP parts before measuring, and compares sequential edits, batching, streaming,
compaction, batch reads, prepared reads, and unchanged batches. It also exercises
wide rows, unfamiliar cell extensions, shared strings, new-row insertion, and
multiple worksheets. Use `SHEETPATCH_BENCH_CASES` to select comma-separated cases
(`narrow_10000`, `narrow_50000`, `guarded`, `wide`, `opaque`, `shared_strings`,
`insertion`, `multisheet`) and `SHEETPATCH_BENCH_OPERATIONS` to select exact
operation names printed by the harness. For example:

```sh
SHEETPATCH_BENCH_CASES=narrow_50000 \
SHEETPATCH_BENCH_OPERATIONS=bulk_at,get_cells_at,prepared_single_reads \
SHEETPATCH_BENCH_SAMPLES=5 cargo bench --locked --bench editing
```

### Measurements

The following measurements compare 1.0.0 with the preceding release candidate
on an Intel Core Ultra 5 325, Linux x86_64, Rust 1.99.0, using release builds and
synthetic workloads. Benchmark rows are harness medians; the other rows report a
timed run and its peak resident memory. They describe these workloads, not a
guarantee for every workbook, and vary between runs and machines.

| Workload | Previous candidate | 1.0.0 |
| --- | ---: | ---: |
| `narrow_50000` `bulk_at`, 500 edits | 63 ms | 40 ms |
| `narrow_50000` `get_cells_at`, 500 reads | 59 ms | 38 ms |
| `narrow_50000` `cold_single_read` | 57 ms | 37 ms |
| `guarded_bulk_at` | 29 ms | 19 ms |
| `multisheet_apply`, 128 edits across 4 worksheets | 7.9 ms | 5.2 ms |
| CLI `get` of one cell in a 200,000-cell worksheet | 167 ms, 97.7 MiB | 105 ms, 75.1 MiB |
| Reading from a 61 MB `sharedStrings.xml` | 1,067 ms, 732 MiB | 790 ms, 197 MiB |
| Opening with a padded 16 MiB `[Content_Types].xml` | 819 MiB | 19 MiB |
| Opening with a 63 MiB `workbook.xml` | 3.2 GiB | 66 MiB |

Worksheet parsing alone was about 30-36% faster and index construction 38-42%
faster. Recompressing replacement parts at Deflate level 4 instead of the default
level 6 made saves of edited worksheets 2 to 3.4 times faster in these
measurements, with recompressed parts within a few percent of their level 6 size.

### Historical 0.2 measurements

Initial 0.2 measurements used one release-mode sample per synthetic workload on
the same machine and toolchain:

| Worksheet cells / edits | Original sequential editor | Batch editor |
| --- | ---: | ---: |
| 10,000 / 100 | 1.335 s | 14.185 ms |
| 50,000 / 500 | 43.954 s | 74.536 ms |

The original editor was the initial public implementation at commit
`532f27d6b3815b436b3ed1862f4a633001e595eb`, measured with the same generated fixtures.
The pre-1.0 append writer grew to 3,354,749 bytes after five saves of the 50,000-cell
fixture; compaction kept it at 1,434,574 bytes. The 1.0 writer replaces changed
records, avoiding that growth. Streaming avoids allocating the output archive in
memory.

MIT licensed.
