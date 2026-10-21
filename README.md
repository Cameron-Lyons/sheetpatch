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
    let block_values = book.get_cells_at("Summary", block)?;
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
workflows, and `Workbook::from_reader` reads a workbook from any `std::io::Read`,
such as standard input. Addresses use A1 notation, accept lowercase letters, and
must fit Excel's row and column limits (`CellRef::MAX_ROW`, `CellRef::MAX_COLUMN`).
`CellRange` parses `"A1:C3"` with corners in any order, or a single cell, and
provides `first`, `last`, `height`, `width`, `cell_count`, `contains`, and
`cells`. A range is itself iterable, row by row, so it can be passed directly to
`get_cells_at`, which returns one result per cell and reads at most 16,777,216
(2^24) cells per call; a larger request returns `Error::Unsupported` before any
cell is read. Ranges are ordered by their top-left and then bottom-right cell,
for use as sorted keys.

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
CLI's `get` does, without applying number formats. A precision such as `{:.2}`
rounds numbers and never truncates other values.

To edit dates, write the Excel numeric serial into a cell that already has a date
format, or a new cell whose row or column style supplies one. Serials depend on
the workbook's date system: `uses_1904_dates()` reports whether serials count
from 1904-01-01 (serial 0) instead of the 1900 system, in which 1900-01-01 is
serial 1. For example, 2024-01-15 is 45306 in the 1900 system and 43844 in the
1904 system. ISO 8601 date cells (`t="d"`) read as serials in the workbook's
system. In the 1900 system, Excel's day zero, 1899-12-31, is serial 0; a time
without a date reads as a fraction of serial 0 in either system.

### Batches and transactions

Use `set_cells` for many edits on one sheet and `apply_edits` for transactions
across sheets. Duplicate addresses use the last value; all inputs are validated.
Failed transactions leave previous pending edits intact. A refused batch or
transaction reports one refused edit, which need not be the first in input
order; the `set_cells` and `apply_edits` documentation gives the order of the
checks. `has_changes()` reports pending changes and `reset_changes()` restores
the original archive.
`get_cell_at`, `get_cells_at`, `set_cell_at`, `set_cells_at`, and `CellEdit::at`
accept typed `CellRef` addresses directly, avoiding conversion through A1 strings.
Typed batches preserve input order for reads and use the last value for duplicate
edits, with the same transaction and value validation rules as A1 batches.
Saving does not clear pending changes or replace the in-memory original archive.
Reopen the saved file to use it as the new baseline.

### Workbooks and reads

`sheets()` lists editable worksheets in workbook order, hidden ones included;
chart sheets are retained as opaque parts. `sheet(name)` finds one worksheet,
`Sheet::visibility()` reports hidden and very hidden tabs (`as_str` gives the
`list --json` spelling), and `is_signed()` reports digitally signed packages,
whose edits are refused. Worksheet names match exactly, including case. Package
discovery resolves URI-escaped relationship targets and compares part paths
without ASCII case, retaining the actual ZIP member names. Ambiguous equivalent
names for an accessed part are rejected.

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
let everything = sheet.get_cells_at(sheet.cells())?;
# Ok::<(), sheetpatch::Error>(())
```

The view includes pending edits, preserves read order and duplicates, and loads
shared strings only when requested values need them. `cells()` lists every
cell in row-major order as an `ExactSizeIterator`, and `used_range()` returns the
smallest range containing them, style-only cells included; both come from the
cells themselves, not the stored `<dimension>`. A single style-only cell far
from the data, such as one at XFD1048576, can stretch that range over all
17,179,869,184 cells of the grid, so check `CellRange::cell_count` before
reading a range, or read `cells()` as above. The view borrows the workbook;
drop it before making edits. Dropping the view releases the parsed index.
Ordinary reads retain cached decompressed XML but release their index after each
call. Both workbooks and prepared views can be shared across threads.

An edit of a protected cell (see [Supported scope](#supported-scope)) returns
`Error::ProtectedCell { sheet, cell, reason }`, displayed as
`protected cell: worksheet "Summary": D2 contains a formula`. Other errors raised
inside a worksheet name it and, where one applies, the cell. Errors in another
part, such as the shared-string table or a worksheet's relationships and table
parts, name that part instead, as in `part "xl/sharedStrings.xml": …`.
Malformed or ambiguous content, such as a numeric cell whose value is not a
number, a cell reference that is not an address, rows out of order, or a cell
with two values, returns `Error::InvalidWorkbook` in worksheets and shared
strings, as it does in workbook and table parts. `Error::Unsupported` is for
valid input and edits outside the supported scope, such as rows without explicit
row numbers or cells inside a markup-compatibility branch. `SheetNotFound` and
`InvalidCellReference` quote their input, as in `worksheet not found: "Summary"`.
Messages are for people and their wording may change; match on variants and
fields. `Error` is non-exhaustive, so matches need a fallback arm; so is the
`ProtectedCell` variant, whose patterns need `..`.

`write_to` streams a workbook to any `std::io::Write` implementation and flushes
it before returning; an error after output has started, such as a failing writer,
may leave a partial archive in it. File saves use that same path, avoiding
allocation of another complete ZIP. Worksheets are decompressed lazily and cached;
shared strings are loaded only when a requested cell needs them.

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
`CELL<TAB>TYPE<TAB>VALUE` line per cell. Prefixing each line with the sheet name
and a tab gives patch rows for single-line values, with these exceptions.
Wherever the rows are applied, `patch` refuses rows for error tokens outside
Excel's writable codes (see [Supported scope](#supported-scope)) and for text
longer than 32,767 UTF-16 code units. Applied back to the cells they were listed
from, it also refuses rows for formula cells, for the other cells of a shared,
array (including dynamic-array spill), or data-table formula range, and for
merged cells other than their range's top-left cell, even though the value is
unchanged. `cells --json` returns the same objects as `get --json`, in which
spill and other formula-range cells without a formula of their own have a `null`
formula. An empty worksheet prints nothing, or `[]`.

Both commands validate all requested values before printing anything. While
validating they buffer up to 4 MiB of formatted output, so a read whose output
fits decodes each cell once; cells after that prefix are read again
individually for output. A value whose stored text would overflow the buffer
ends the prefix before it is copied, so repeated or oversized stored strings
cannot multiply memory use.

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
its value. Text keeps trailing whitespace and additional tabs. Fields are raw:
there is no quoting or escaping, so quotes are stored as written, and a quoted
sheet name names a different worksheet. Patch files must use UTF-8, optionally
with a BOM, and LF or CRLF line endings. Any other carriage return is rejected,
including one before a CRLF ending, rather than merging classic Mac lines into
one value or leaving a stray carriage return; only a final line without a line
feed may end with one, which stays in the value. Empty lines
and tab-free lines beginning with `#` are ignored. Literal multiline values use
the Rust API. Invalid rows and rows naming a missing worksheet report their line
number; an edit refused for a protected cell names the last line for that cell,
as in `Patch line 5: protected cell: worksheet "Summary": D2 contains a formula`.
Every input row is validated, but only the final value for each cell is retained,
so repeated edits do not accumulate discarded text in memory. Any error leaves
the destination unchanged.

To generate a patch, join each row's fields with tabs (in Python,
`'\t'.join(fields) + '\n'`), which keeps tabs in the value, the last field. The
`csv` module also works with
`csv.writer(f, delimiter='\t', lineterminator='\n', quoting=csv.QUOTE_NONE, quotechar=None)`,
but raises `csv.Error` for values containing tabs or line breaks.
The default `csv` dialect quotes fields, and those quotes would be stored.

`patch --check` validates the complete transaction and output generation,
including worksheet names, formula and table-header protection, merged ranges,
cell and value metadata, and ZIP size limits, without saving. It reports the
number of input edit rows, including duplicates. The same patch can then be
applied with `patch INPUT OUTPUT PATCH.tsv`. Check mode returns the same
validation errors as an actual edit, reporting one refused edit per run.
Generated ZIP bytes are streamed to a sink; no complete output archive is
allocated or written to disk. An empty patch succeeds for a supported workbook.

### Arguments, streams, and exit codes

Options (`--json`, `--check`, `--help`/`-h`) must come right after the command.
Unknown or repeated options, and option-like arguments where a path belongs,
are usage errors, so a misplaced flag never becomes a file name; write a file
whose name starts with `-` as `./-name`. Sheet names and values may start with `-`.

Use `-` as INPUT to read the workbook from standard input, and as OUTPUT to write
it to standard output; each is refused on a terminal, and INPUT and PATCH cannot
both be `-`. Like `Workbook::from_reader`, standard input is read only up to the
largest possible ZIP32 archive. Output to standard output is streamed, not atomic:

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
Diagnostics written to a terminal escape control characters other than tab and
line feed, so paths and workbook metadata cannot send terminal-control commands.
Redirected diagnostics retain their original text.

Text output (`list`, `get`, `cells`, help, and `patch --check`) ends quietly with
code 0 when its reader closes the pipe, as `head` does; other write failures, such
as a full disk, exit with code 1. A workbook written to standard output must be
complete, so any failure to write it, including a closed pipe, exits with code 1
and `cannot write workbook to standard output`. On a terminal, plain `get` and
`cells` output shows control characters other than tab and line feed (C0, DEL,
and C1) as `\u{..}` escapes, and `list` escapes every control character in sheet
names. Pipes and files receive the exact text. `--json` strings decode to the
exact text and escape every control character, including DEL and C1 as `\u00XX`.

## Saving

`save`, `save_compact`, and CLI file outputs write a temporary file beside the
destination and rename it into place after a successful write. The original
destination is retained if generation or writing fails, and the input path can be
the destination. The destination must name a file: a path without a file name,
and an existing directory, FIFO, device, or socket (also behind a symlink),
return `Error::Io` of kind `InvalidInput` before any output is generated.

Existing destination permission bits are applied to the temporary file before
workbook contents are written. On Unix the replacement also keeps the
destination's group when the user may assign it; otherwise its group bits are
cleared, and its other bits are limited to those shared by the original group
and others. On Unix, a new destination saved from a workbook
opened with `Workbook::open`, including CLI outputs from an input file, is created
using the input's permission bits as its creation mode, subject to the operating
system's umask or default ACL rules. It uses the same group rule.
`from_bytes`, `from_reader`, and standard input use the default mode.

Owner, ACLs, and extended attributes are not copied. Both new and replacement
files can inherit the destination directory's default ACL, which can grant
access to additional users even when the permission bits match the original.
On Linux, a default ACL takes precedence over the umask; see
[`umask(2)`](https://man7.org/linux/man-pages/man2/umask.2.html).

On Linux, a file with an access ACL keeps the ACL's mask in its group
permission bits. A new or replacement file that inherits an access ACL from the
destination directory's default ACL gets that mask as its own. Any other new or
replacement file gets the permissions of the owning group's own entry
(`group::`), as limited by the mask, so saving a workbook shared with
`chmod 600; setfacl -m u:bob:rw` does not give its owning group `rw-` access.
An ACL that cannot be read or parsed leaves only the owner's permission bits.
Other platforms, including FreeBSD with POSIX.1e ACLs, use the mode bits as
they are.

Setuid, setgid, and sticky bits are never kept, on new or replaced files.
Saves replace the destination directory
entry: an output symlink to a regular file is replaced rather than written
through, and hard links to the old file keep the old contents. On Unix, like
`sed -i`, a save replaces a read-only destination as long as its directory is
writable; Windows refuses to replace a read-only destination. After the rename,
Unix saves sync the directory (best effort) so a completed save survives a crash.

If the process is killed during a save (Ctrl-C, SIGTERM, SIGKILL, power loss),
the destination is untouched, but a hidden temporary file can remain beside it.
It is named `.{name}.sheetpatch-{pid}-{n}.tmp`, where `{name}` is the
destination's file name up to any colon, cut to at most 32 bytes. It is an
incomplete copy and safe to delete.

## What lossless means

| Content | Behavior |
| --- | --- |
| Untouched package parts | Original payload, compressed bytes, local headers, extra fields, and comments are retained byte for byte; directory offsets may change. |
| Edited worksheet | Original XML outside the necessary edits is retained byte for byte. |
| Existing cell formatting | Style, other attributes, and unfamiliar sibling content are retained. |
| New cells and rows | Inserted in coordinate order. A new cell takes its row's style when the row has a custom format, otherwise its column's style; existing cells are never restyled. Existing dimension and row-span hints are widened as needed. |
| Text | Written as inline strings; shared strings stay untouched. Whitespace, XML characters, and literal Excel escape sequences are preserved. Characters XML 1.0 forbids, such as NUL and other control characters, are stored as Excel's `_xHHHH_` escapes and read back exactly. |
| Unchanged values | Writing the value a cell already stores, including a shared string's own text or an ISO 8601 date cell's serial, leaves the worksheet unchanged; edits may load the shared-string table to compare. Protections still apply. |
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
refuses, with `Error::Unsupported`, opaque ZIP prefixes or gaps and obsolete
records it cannot recognize or verify. It produces a smaller workbook when
obsolete records are present, while protecting unfamiliar archive data. Use it
to remove the unused records left by sheetpatch 0.2's append writer before
opening those files in applications that reject ZIP gaps. It is not a secure
erasure operation for copies or filesystem history.

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
  only be preserved untouched. Workbooks are held in memory; `Workbook::open`,
  `Workbook::from_reader`, and CLI standard input refuse input larger than the
  largest possible ZIP32 archive, about 8 GiB.
- ZIP member names must be UTF-8; non-ASCII names also require the ZIP UTF-8 flag
  in both headers. Without that flag, readers can interpret the same bytes as
  different names. Legacy filename encodings are unsupported; see the
  [ZIP encoding specification, Appendix D](https://pkware.cachefly.net/webdocs/casestudies/APPNOTE.TXT).
- Archives that other ZIP readers could interpret differently are rejected:
  an end-of-central-directory signature after the real record with room for a
  complete end record before the end of the file (for example, inside the
  archive comment); an earlier complete end record near the end of the file,
  for example in the last central entry's comment, whose directory starts at a
  central-directory header, which libzip before 1.11 can select instead;
  duplicate member names, and names that are empty or contain NUL or a
  backslash, which some readers treat as `/`; Info-ZIP Unicode Path extra
  fields of any version that rename a member; conflicting local and central
  headers; and overlapping records. A save that would itself produce such an
  archive is refused with `Error::Unsupported`, as are compaction and
  `patch --check`. This rare case arises when new offsets, checksums, or sizes,
  or records the save moves to a new distance from the end of the file, would
  form another end record that readers, including libzip before 1.11, could
  select, or a ZIP64 locator signature just before the end record. `write_to`
  and `write_compact_to` may already have written the local records.
  `[Content_Types].xml` is found regardless of ASCII case; case-variant
  duplicates are ambiguous.
- Explicit, sorted row and cell coordinates. Rows and cells without explicit
  coordinates return `Error::Unsupported`; duplicate, unsorted, or malformed
  coordinates return `Error::InvalidWorkbook`, as does other malformed or
  ambiguous worksheet structure, such as a cell with two values or a merge
  range that is not a range. Either error is returned before any change is
  applied.
- Unambiguous workbook metadata: invalid sheet `state` or `date1904` values, more
  than one `workbookPr`, and duplicate worksheet targets are rejected when
  opening. `workbookPr` inside a markup-compatibility branch or a matching
  `mc:ProcessContent` wrapper is unsupported, since processing that content
  could change the date system. Mixed strict and transitional SpreadsheetML in
  workbook metadata, worksheets, or shared strings is unsupported, since
  ignoring the other dialect could omit data or bypass edit protections.
  XML nested deeper than 256 elements is unsupported.
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
- ISO 8601 date cells (`t="d"`) read as numeric serials. Rewriting one with its
  current serial leaves it unchanged; a different number is stored as a plain
  number. Valid values without a serial, with a UTC offset other than `Z` or a
  date before the workbook's epoch, return `Error::Unsupported` on read;
  malformed values return `Error::InvalidWorkbook`.
- Cells with value metadata (`vm`, including linked and rich data types) are
  preserved; edits that would change their scalar value or representation are
  refused so metadata cannot remain attached to a replacement value.
- Cells with cell metadata (`cm`) are also protected against changes, since
  their metadata can require removal when values are edited or cleared.
  Metadata-specific edit rules are not interpreted. Reads and exact no-op
  writes remain available.
- Merged cells can be edited through their top-left anchor. Other cells in a
  merged range are protected.
- Cell values, formulas, and merged ranges inside unsupported XML containers
  are refused when accessing that worksheet, preventing hidden content from
  bypassing these protections.
- Unfamiliar XML inside a value payload is protected against replacement;
  unfamiliar cell attributes and sibling elements are preserved.
- String reads refuse hidden text or shared-string entries in unsupported XML
  containers, preventing partial values or shifted shared-string indices.
  Multiple text elements within a single rich-text run are also unsupported,
  since spreadsheet readers can interpret them differently.
  Edits also refuse inline strings with hidden rich text, preserving content
  that cannot be interpreted safely.
- Legacy `.xls` and password-encrypted workbooks (OLE compound files) are
  detected and return `Error::Unsupported`, as do binary `.xlsb`, encrypted ZIP
  entries, ZIP64, multi-disk ZIP, and XML DTDs. Digitally signed OOXML packages
  can be opened and copied unchanged, but editing is refused to avoid
  invalidating signatures.

## Dependencies and checks

Three direct dependencies, and a fourth on Linux; XML parsing, compression, and
`rustix` disable default features:

- [`quick-xml`](https://docs.rs/quick-xml/0.42.0/quick_xml/): namespace-aware parsing.
- [`flate2`](https://docs.rs/flate2/1.1.10/flate2/): pure Rust Deflate compression.
- [`crc32fast`](https://docs.rs/crc32fast/1.5.2/crc32fast/): optimized ZIP checksums.
- [`rustix`](https://docs.rs/rustix/1.1.5/rustix/) (Linux only, `fs` and `std`
  features): reads a file's access ACL for saves, so the crate keeps
  `forbid(unsafe_code)`.

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
pass. Formula-cell guards inspect each edited cell's own markup instead of
recording every formula cell while indexing. Formula-range and merged-range
guards use direct membership checks for small collections of edits or ranges.
Larger batches exclude ranges outside the edited cells' bounding rectangle
before allocating a sweep index. Table-header guards coalesce the rows that
headers span and test the edits in those rows against every header in one row
sweep, so overlapping headers do not repeat work for each table; current header
values are compared one at a time. Table targets are normalized against the
worksheet's directory once, so spellings that differ only in letter case,
escapes of unreserved characters, or dot segments share one resolution, and
each distinct table part is decompressed and parsed once per workbook, even
when several worksheets relate to it. Inserted cells resolve their row's
default style once, while column defaults remain specific to each column. Row
defaults are validated only when needed.
Indexed ZIP/sheet lookup and cached decompression avoid repeated archive scans,
and ZIP physical order is cached for repeated saves.

Worksheet parsing reads each element's attributes in a single pass into compact
records that refer to the original XML by 32-bit offsets, 48 bytes per element
and 24 per attribute. Names and raw attribute values are read back from those
spans; only values that XML normalization changes are stored separately, and
the streaming shared-string parse releases them with each string. Direct
children use ranges in the parse index, attributes and row cells use shared
buffers, and normalized namespace URIs use compact identifiers.
Namespace resolution reuses active prefix bindings across descendants and
restores cached parent bindings after nested declarations. Package attribute
checks reuse cached URI fingerprints and still compare exact namespace names.
One patch plan applies payload and attribute changes directly to worksheet spans;
unchanged cell extensions are copied only into the final output. Edits that
leave a worksheet unchanged retain its current XML without copying it. Shared
strings are decoded while streaming, releasing each entry's XML as it completes,
and Excel text escapes are decoded without converting ordinary UTF-8 to a
temporary UTF-16 buffer. Numeric and predefined XML text references are decoded
directly, avoiding temporary strings for each reference. Writing XML-forbidden
characters appends Excel escapes directly to the output buffer.
Package discovery validates whole parts but retains
only the elements it needs. For unchanged-value comparisons, multi-sheet edits
attempt shared-string loading once per transaction, including when the table is
damaged. Prepared worksheet views reuse a validated index
across reads, with retention controlled by the view's lifetime; single-cell
reads avoid allocating a batch result. CLI reads buffer up to 4 MiB of
formatted output while validating, with no cell-count cap, so most reads decode
each cell once. Cells beyond the buffer are validated before printing and then
read again individually for output.

Streaming saves borrow ZIP metadata and hold one replacement compressed part at
a time, in addition to the input archive, cached original XML, and pending
worksheet XML. Before writing the central directory, a save also holds the
rewritten directory and end record and up to the last 64 KiB of local-record
output, so that it can check the archive's end as a reader would. In the rare
case that an end-record-like structure in that window points before it (in
ordinary workbooks, only a Stored embedded ZIP's own end record), the local
records are generated a second time, recompressing replacements, to read those
bytes back. Changed parts resolve to archive entry indexes once per save, and
normal and compact saves share one record emitter with their respective
gap-preservation rules. Compaction searches for obsolete record boundaries only
in gaps between validated active records, so active payload bytes do not become
descriptor candidates, and parses and inflates each obsolete record only within
its gap.

Replacement parts keep their member's compression method. Deflated ones are
recompressed with Deflate level 4, the fastest level that keeps lazy matching.
A deflated replacement larger than 2 MiB is compressed in fixed 1 MiB segments,
each ending with a sync flush so that together they form one stream, on up to
eight threads including the calling thread, and never more than
`std::thread::available_parallelism` or the segment count. Its bytes do not
depend on the thread count and are about 0.1% larger than a single stream's.
Applications that save many large workbooks in parallel may therefore
oversubscribe the CPU. Untouched parts keep their original compressed bytes.

Run the dependency-free benchmark with:

```sh
SHEETPATCH_BENCH_SAMPLES=3 cargo bench --locked --bench editing
```

The harness independently checks expected worksheet XML and untouched compressed
ZIP parts before measuring, and compares sequential edits, batching, streaming,
compaction, batch reads, prepared reads, and unchanged batches. It also exercises
wide rows, unfamiliar cell extensions, shared strings, new-row insertion,
multiple worksheets, bulk edits across thousands of tables, row-style inheritance,
and text requiring many Excel escapes.
Use `SHEETPATCH_BENCH_CASES` to select comma-separated cases
(`narrow_10000`, `narrow_50000`, `guarded`, `wide`, `opaque`, `shared_strings`,
`insertion`, `multisheet`, `tables`, `row_style`, `control_text`) and
`SHEETPATCH_BENCH_OPERATIONS` to select exact operation names printed by the
harness. For example:

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
