# Changelog

## 1.0.0 — release candidate

The first stable release covers surgical scalar cell edits in existing OOXML
workbooks. Compared with 0.2, it stabilizes a typed Rust API, replaces changed
ZIP records instead of appending them, adds CLI inspection, validation, and
standard-stream support, and refuses more inputs whose edits could be ambiguous
or damage the workbook. See README.md for the preservation contract and
supported scope.

### Compatibility notes

Changes that can affect 0.2 code, scripts, or workbooks:

- `Error` is `#[non_exhaustive]`; matches must include a fallback arm.
- `CellContent` is `#[non_exhaustive]`. Its fields remain public for reading, but
  it cannot be built with a struct literal outside the crate, and destructuring
  patterns need `..`.
- The public `From<quick_xml::Error> for Error` conversion is removed, keeping the
  pre-1.0 `quick-xml` dependency out of the stable API.
- `Error::Unsupported` displays as `unsupported: …` instead of
  `unsupported edit: …`, which also changes CLI messages. Errors raised inside a
  worksheet are prefixed with `worksheet "Name": ` and name the cell or row where
  one applies; variants are unchanged.
- A save destination without a file name (such as `""`, `.`, or `dir/..`) returns
  `Error::Io` of kind `InvalidInput` instead of `Error::InvalidValue`. Existing
  directories, FIFOs, devices, and sockets are refused the same way before output
  is generated.
- OLE compound files (legacy `.xls` and password-encrypted workbooks) return
  `Error::Unsupported` instead of `Error::InvalidWorkbook`. Package XML that is
  not UTF-8 returns `Error::Xml` instead of `Error::Unsupported`, and package XML
  nested deeper than 256 elements returns `Error::Unsupported` instead of
  `Error::Xml`, matching worksheet parsing.
- Written error values must be one of Excel's 17 error codes, such as `#N/A` or
  `#DIV/0!`; 0.2 accepted any token-shaped `#…` string. Reads remain lenient.
- Text containing control characters that XML 1.0 forbids is now accepted and
  stored as Excel `_xHHHH_` escapes; 0.2 rejected it.
- New cells take their row's style when the row has a custom format, otherwise
  their column's style; 0.2 created them unstyled. Malformed column or row style
  definitions refuse only edits that create a cell needing them.
- Newly refused inputs: archives with additional end-of-central-directory
  signatures or Info-ZIP Unicode Path fields that rename a member; case-variant
  `[Content_Types].xml` duplicates; duplicate worksheet targets; invalid sheet
  `state` or `workbookPr date1904` values, or more than one `workbookPr`; edits
  that change an Excel table header cell; edits to worksheets with malformed or
  missing table relationships or parts; and `Workbook::open` input over the
  largest possible ZIP32 archive (about 8 GiB).
- CLI options must come right after the command, and option-like arguments in
  path positions are usage errors; name such files `./-name`. `-` as INPUT or
  OUTPUT now means standard input or output. Patch lines containing a carriage
  return followed by more data are rejected with exit code 2.

### Rust API

- Stable API for typed addresses, scalar values, cell inspection, batch and
  cross-worksheet transactions, streaming output, and explicit ZIP compaction.
  Public API documentation is required at compile time.
- `get_cell_at`, `get_cells_at`, `set_cell_at`, `set_cells_at`, and `CellEdit::at`
  accept validated `CellRef` values directly. `CellRef::MAX_ROW` and
  `CellRef::MAX_COLUMN` expose Excel's limits.
- `CellRange` parses, formats, and iterates rectangular ranges in row-major order
  for block reads and writes with the typed batch APIs.
- `CellValue` converts from `&String`, `Cow<str>`, `char`, the integer types that
  `f64` holds exactly, and `Option<T>` (`None` clears the cell). `i64`, `u64`,
  `isize`, and `usize` use `TryFrom`, rejecting magnitudes above 2^53. Values
  implement `Default` (blank) and `Display`, and offer `as_text`, `as_number`,
  `as_bool`, `as_error`, and `is_blank`. `CellEdit` implements `PartialEq`.
- `prepare_sheet` returns a read-only `WorksheetView` for repeated reads using one
  validated worksheet index, retained only for the view's lifetime. Its
  `cell_refs` lists every cell in row-major order and `used_range` returns the
  smallest range containing them.
- `Workbook::sheet`, `is_signed`, and `uses_1904_dates`, and `Sheet::visibility`
  with the non-exhaustive `SheetVisibility` enum, describe the package. `Sheet`
  implements `PartialEq`, `Eq`, and `Hash`; `Workbook` and `WorksheetView`
  implement `Debug` without printing contents.
- `write_to` and `write_compact_to` flush the writer before returning, so
  buffered writers report final write errors.
- `set_cell` and `set_cell_at` report a missing worksheet before an invalid
  address or value, consistently with batches and reads. Empty worksheet batches
  validate the sheet name.

### Command line

- Commands list worksheets, read cells, set values, apply TSV patches, and
  compact archives, with `--help`, `--version`, and per-command help through
  `help COMMAND` and `COMMAND --help`/`-h`.
- `get` reads multiple cells and ranges with one worksheet parse, in argument
  order, up to 1,048,576 cells per call. The new `cells` command lists the cells
  holding a value or formula, optionally within a range. `--json` output for
  both includes scalar types, values, stored formulas, and styles; `list --json`
  includes worksheet names, package paths, and visibility.
- `patch --check` validates a complete edit transaction and output generation
  against workbook protections and ZIP limits without writing a file, from a
  TSV file or standard input.
- `-` reads the workbook from standard input or streams it to standard output;
  neither is used on a terminal, and INPUT and PATCH cannot both be `-`.
- Invalid syntax, addresses, ranges, values, patch rows, and non-Unicode text
  arguments exit with code 2 before a workbook is opened where possible; syntax
  errors print the usage and other invalid input prints one line with a help
  pointer. Workbook, missing-worksheet, and I/O errors exit with code 1, and I/O
  messages name the path or stream. Patch errors report their line number,
  including rows naming a missing worksheet and the last row for a refused cell.
- Text output ends quietly with code 0 when its reader closes
  the pipe and reports other write failures with code 1; a workbook that cannot
  be completely written to standard output always exits with code 1. On a
  terminal, plain output escapes control characters so cell text and sheet names
  cannot control the terminal; pipes, files, and JSON receive exact text.
- Native filesystem paths work independently of Unicode text arguments. Invalid
  UTF-8 patch rows report their line number for files and standard input.

### Editing and reading

- Writing the value a cell already stores, including a shared string's own
  text, leaves the worksheet unchanged. Clearing an empty typed cell removes its
  type while retaining formatting and unfamiliar sibling content.
- Text round-trips exactly: control characters use `_xHHHH_` escapes, literal
  escape sequences stay literal, and a leading U+FEFF is preserved on read.
- ISO 8601 date cells (`t="d"`) read as serial numbers in the workbook's 1900 or
  1904 date system.
- Scalar changes to cells with value metadata are refused, preserving linked and
  rich data types. Unchanged values and unrelated cell edits retain metadata.
- Excel table header cells may be rewritten with their current value but not
  changed, keeping header text equal to the table's column names.
- Cell values, formulas, and merged ranges in unsupported XML containers cannot
  bypass worksheet edit protections, and shared-string and rich-text reads reject
  hidden content that would shift indices or truncate values.
- Worksheet numeric attributes and scalar values accept surrounding XML
  whitespace. Shared-formula indices compare numerically, so spellings such as
  `1` and `01` no longer prevent edits to unrelated cells; invalid indices are
  rejected.

### Package and ZIP handling

- Package relationships resolve URI-escaped and case-insensitive OPC part names
  without changing ZIP names. Canonical escaped names take precedence over legacy
  decoded names; ambiguous equivalent names for an accessed part are rejected.
  Relationship resolution rejects directory targets and unresolved empty path
  segments.
- Workbook sheet names decode Excel escape sequences. Ambiguous workbook layouts,
  invalid package XML, duplicate expanded XML attributes, and reserved namespace
  bindings (checked after character-reference normalization) are rejected.
- ZIP archives that other readers could interpret differently are rejected, and
  OLE compound files are recognized as unsupported formats. Binary workbook
  detection handles uppercase extensions.
- ZIP patched-data entries stay opaque; reading, editing, and deleting obsolete
  patched records during compaction are refused. Compaction recognizes obsolete
  Stored records whose payload contains header- and descriptor-like bytes.

### Saving

- Default saves replace changed ZIP records in physical order, so ordinary
  edited archives reopen in LibreOffice and repeated saves avoid accumulating
  obsolete worksheet copies. Untouched local records remain byte-identical;
  central-directory offsets may change. Existing opaque gaps remain protected.
  Explicit compaction can remove obsolete records from 0.2 append saves.
- Saves are atomic. Existing destination permissions are applied before contents
  are written; on Unix the replacement keeps the destination's group when it can
  be assigned, and otherwise loses group access. New destinations for workbooks
  opened from files are created with the input's permissions, restricted by the
  umask. Unix saves sync the directory after the rename. Long destination
  filenames are supported.

### Performance

- Worksheet parsing reads attributes in one pass into compact records, borrows
  names and values from the XML, and shares namespace identifiers; edits apply
  absolute spans without rebuilding cells. README.md lists measurements.
- Shared strings are decoded while streaming, and package discovery retains only
  the elements it needs, greatly reducing peak memory for large parts.
- Replacement parts use Deflate level 4 instead of the default level 6, making
  saves of edited worksheets 2 to 3.4 times faster in the repository's
  measurements, with similar compressed size. ZIP saves borrow unchanged
  metadata, reuse validated physical entry order, and use optimized CRC32
  checksums; in-memory output reserves archive-sized capacity.
- Benchmarks cover prepared and ordinary reads, wide rows, large cell extensions,
  shared strings, insertions, multi-sheet transactions, and no-op batches, with
  workload filters, and verify their output before measuring.

### Quality

- Deterministic randomized property tests, an independently exported LibreOffice
  workbook, and an optional LibreOfficeKit round-trip check complement the
  byte-preservation tests.
- CI lints the Windows MSVC target, runs the benchmark correctness checks, and
  audits Cargo.lock with `cargo audit` on every run and weekly.

Compatibility: Rust 1.88 or later; ZIP32 `.xlsx`, `.xlsm`, `.xltx`, and `.xltm`.
Formula creation/calculation, structural edits, ZIP64, encrypted workbooks, and
editing signed packages remain outside the supported scope.
