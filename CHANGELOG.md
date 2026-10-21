# Changelog

## 1.0.0 — release candidate

The first stable release covers surgical scalar cell edits in existing OOXML
workbooks. Compared with 0.2, it stabilizes a typed Rust API, replaces changed
ZIP records instead of appending them, adds CLI inspection, validation, and
standard-stream support, and refuses more inputs whose edits could be ambiguous
or damage the workbook. See README.md for the preservation contract and
supported scope.

### Compatibility notes

Changes that can affect code, scripts, or workbooks written for 0.2 or for an
earlier 1.0.0 candidate:

- `Error` is `#[non_exhaustive]`; matches must include a fallback arm.
- `CellContent` is `#[non_exhaustive]`. Its fields remain public for reading, but
  it cannot be built with a struct literal outside the crate, and destructuring
  patterns need `..`.
- The public `From<quick_xml::Error> for Error` conversion is removed, keeping the
  pre-1.0 `quick-xml` dependency out of the stable API.
- Edits refused for a protected cell return the new non-exhaustive
  `Error::ProtectedCell { sheet, cell, reason }` variant instead of
  `Error::Unsupported`. Protected cells are formula cells, cells in a shared,
  array, or data-table formula range, and merged cells other than their range's
  anchor, as well as the newly protected table headers and cells carrying metadata.
- `Error::SharedStringsUnavailable` is removed. A shared-string cell in a workbook
  without a shared-string table returns `Error::InvalidWorkbook` naming the cell.
- Malformed or ambiguous worksheet and shared-string content returns
  `Error::InvalidWorkbook` instead of `Error::Unsupported`, as table and workbook
  parts already did. This covers invalid or non-finite numbers, booleans,
  shared-string and style indices, and unknown cell types; payloads that do not
  match their cell type (`t="inlineStr"` with `<v>`, or another type with
  `<is>`), and cells with two payloads or two formulas; row numbers and cell
  references that do not parse, lie outside the grid, repeat, are out of order,
  or name another row; merge and formula ranges that do not parse or have
  reversed corners, a `mergeCell` without `ref`, and a shared formula without a
  numeric `si`; and a worksheet without exactly one `sheetData`, or a worksheet
  or shared-string part whose root is not an OOXML `worksheet` or `sst`. Valid
  content the crate cannot index, such as rows or cells without `r` and content
  inside markup-compatibility containers, stays `Error::Unsupported`, as do the
  documented edit refusals, including a malformed, `ref`-less, or repeated
  `dimension`. These errors display as `invalid workbook: …` instead of
  `unsupported: …`, in CLI output too; exit codes are unchanged.
- One `get_cells_at` call reads at most 16,777,216 (2^24) cells, sixteen whole
  columns. A larger request, such as a `used_range()` that one distant
  style-only cell stretches over the whole grid, returns `Error::Unsupported`
  before any cell is read; the previous candidate aborted the process on the
  failed allocation. Every cell a worksheet holds fits in one call
  (`get_cells_at(view.cells())`); split larger ranges. Batch reads, including
  `get_cells`, whose results cannot be allocated return `Error::Io` of kind
  `OutOfMemory` instead of aborting.
- Error messages changed, including CLI messages. `Error::Unsupported` displays as
  `unsupported: …` instead of `unsupported edit: …`. `SheetNotFound` and
  `InvalidCellReference` quote their input, as in `worksheet not found: "Data"`.
  Errors raised inside a worksheet are prefixed with `worksheet "Name": ` and name
  the cell or row where one applies; errors in the shared-string table, a
  worksheet's relationships, or its table parts are prefixed with
  `part "path": ` instead. ZIP member-name errors say which rule failed and name
  the member. `Error::Io`'s `source()` returns the I/O error's own source, since
  the message already includes the I/O error.
- When several edited worksheets fail output validation, for example two over
  the 64 MiB part limit, the error names the first in workbook order rather
  than in part-path order.
- A save destination without a file name (such as `""`, `.`, or `dir/..`) returns
  `Error::Io` of kind `InvalidInput` instead of `Error::InvalidValue`. Existing
  directories, FIFOs, devices, and sockets are refused the same way before output
  is generated.
- Temporary save files are named `.{name}.sheetpatch-{pid}-{n}.tmp` after their
  destination instead of `.sheetpatch-{pid}-{n}.tmp`. Scripts that clean up
  leftovers of killed saves should match `.*.sheetpatch-*.tmp`.
- On Linux, saving a file with an access ACL no longer gives the owning group
  the ACL mask's permissions (see Saving), so new and replacement files can get
  fewer permission bits than before, never more. `rustix` is a new Linux-only
  dependency, used to read the ACL without unsafe code.
- A save that replaces a deflated part larger than 2 MiB starts up to seven
  scoped threads, besides the calling thread, to compress it in segments, so
  applications that save many large workbooks in parallel may oversubscribe the
  CPU. The compressed bytes of those parts, about 0.1% larger, differ from the
  previous candidate's; smaller replacements and untouched parts are unaffected.
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
- Newly refused inputs: archives with an additional end-of-central-directory
  signature that leaves room for a complete end record, such as one in the
  archive comment, with an earlier complete end record that libzip before 1.11
  could select instead of the real one, with member names containing a
  backslash, or with Info-ZIP Unicode Path fields of any version that rename a
  member; case-variant `[Content_Types].xml` duplicates; duplicate
  worksheet targets; invalid sheet `state` or `workbookPr date1904` values, or
  more than one `workbookPr`; edits that change an Excel table header cell; edits
  to worksheets with malformed or missing table relationships or parts; and
  `Workbook::open` input over the largest possible ZIP32 archive (about 8 GiB).
  The earlier end record and backslash names return `Error::InvalidWorkbook`.
- In rare cases, saves, compaction, and `patch --check` are refused with
  `Error::Unsupported` because the end of their output would be ambiguous to
  ZIP readers, as described under Package and ZIP handling. Such saves used to
  succeed, but neither sheetpatch nor, in the ZIP64-locator case, Python's
  `zipfile` could reopen their output.
- Compaction reports obsolete records it cannot verify (corrupt payloads, size
  or checksum mismatches, invalid or incomplete deflate streams, and missing,
  ambiguous, or conflicting data descriptors) as `Error::Unsupported` with a
  message starting `cannot compact`, whatever their encoding; Stored records
  with a data descriptor and Deflated records used to return
  `Error::InvalidWorkbook`. Name and extra-field rules shared with active
  members keep their variants.
- CLI options must come right after the command, and option-like arguments in
  path positions are usage errors; name such files `./-name`. `-` as INPUT or
  OUTPUT now means standard input or output. Patch lines containing a carriage
  return other than in a CRLF ending are rejected with exit code 2, including
  `\r\r\n` endings that 0.2 read as a value ending in a carriage return; only a
  final line without a line feed may still end with one.

### Rust API

- Stable API for typed addresses, scalar values, cell inspection, batch and
  cross-worksheet transactions, streaming output, and explicit ZIP compaction.
  Public API documentation is required at compile time; fallible functions
  document their errors, and `Workbook` documents its edit restrictions.
- `Workbook::from_reader` reads a workbook from any `std::io::Read`, refusing
  input beyond the largest possible ZIP32 archive (about 8 GiB).
- `Error::ProtectedCell` names the worksheet, cell, and reason for a refused
  edit of a protected cell, so callers can match fields instead of messages.
- `get_cell_at`, `get_cells_at`, `set_cell_at`, `set_cells_at`, and `CellEdit::at`
  accept validated `CellRef` values directly. `CellRef::MAX_ROW` and
  `CellRef::MAX_COLUMN` expose Excel's limits.
- `CellRange` parses, formats, and iterates rectangular ranges in row-major order
  for block reads and writes with the typed batch APIs. It implements
  `IntoIterator`, so a range can be passed directly where cells are expected, and
  `Ord`, ordering ranges by their top-left and then bottom-right cell.
  `CellRef` and `CellRange` formatting honors width, fill, and alignment.
- `CellValue` converts from `&String`, `Cow<str>`, `char`, the integer types that
  `f64` holds exactly, and `Option<T>` (`None` clears the cell). `i64`, `u64`,
  `isize`, and `usize` use `TryFrom`, rejecting magnitudes above 2^53. Values
  implement `Default` (blank) and `Display`, whose precision applies only to
  numbers, and offer `as_text`, `as_number`, `as_bool`, `as_error`, and
  `is_blank`. `CellEdit` implements `PartialEq`.
- `prepare_sheet` returns a read-only `WorksheetView` for repeated reads using one
  validated worksheet index, retained only for the view's lifetime. Its `cells`
  iterator (an `ExactSizeIterator`) lists every cell in row-major order and
  `used_range` returns the smallest range containing them.
- `Workbook::sheet`, `is_signed`, and `uses_1904_dates`, and `Sheet::visibility`
  with the non-exhaustive `SheetVisibility` enum and its `as_str` spelling,
  describe the package. `Sheet` implements `PartialEq`, `Eq`, and `Hash`;
  `Workbook` and `WorksheetView` implement `Debug` without printing contents.
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
- Usage and `help patch` state that patch fields are raw, so quotes are stored
  as written; the advice to quote values applies to shell arguments.
  `help cells` lists every row of its output that `patch` refuses.
- `-` reads the workbook from standard input, up to the largest possible ZIP32
  archive, or streams it to standard output; neither is used on a terminal, and
  INPUT and PATCH cannot both be `-`.
- Invalid syntax, addresses, ranges, values, patch rows, and non-Unicode text
  arguments exit with code 2 before a workbook is opened where possible; syntax
  errors print the usage and other invalid input prints one line with a help
  pointer. Workbook, missing-worksheet, and I/O errors exit with code 1, and I/O
  messages name the path or stream. Patch errors report their line number,
  including rows naming a missing worksheet and the last row for a protected
  cell.
- Text output ends quietly with code 0 when its reader closes
  the pipe and reports other write failures with code 1; a workbook that cannot
  be completely written to standard output always exits with code 1. On a
  terminal, plain output escapes control characters so cell text and sheet names
  cannot control the terminal; pipes and files receive exact text. JSON strings
  escape every control character, including DEL and C1 as `\u00XX`.
- Diagnostics on a terminal also escape control characters other than tab and
  line feed, including those from file paths and malformed workbook metadata;
  redirected diagnostics retain their original text.
- Native filesystem paths work independently of Unicode text arguments. Invalid
  UTF-8 patch rows report their line number for files and standard input.

### Editing and reading

- Writing the value a cell already stores, including a shared string's own
  text, leaves the worksheet unchanged. Clearing an empty typed cell removes its
  type while retaining formatting and unfamiliar sibling content.
- Text round-trips exactly: control characters use `_xHHHH_` escapes, literal
  escape sequences stay literal, and a leading U+FEFF is preserved on read.
- The escape marker requires lowercase `x`; uppercase `_XHHHH_` stays literal
  when reading cells and sheet names, preventing an edit from being incorrectly
  treated as unchanged.
- ISO 8601 date cells (`t="d"`) read as serial numbers in the workbook's 1900 or
  1904 date system, with 1899-12-31 as serial 0 in the 1900 system. Rewriting one
  with its own serial leaves it unchanged.
- Scalar changes to cells with value metadata are refused, preserving linked and
  rich data types. Unchanged values and unrelated cell edits retain metadata.
- Cells with cell metadata are protected too: their metadata types can require
  disassociation on edits or clears, which cannot safely be assumed to preserve
  the metadata. Reads and exact no-op writes remain available.
- Excel table header cells may be rewritten with their current value but not
  changed, keeping header text equal to the table's column names.
- Cell values, formulas, and merged ranges in unsupported XML containers cannot
  bypass worksheet edit protections, and shared-string and rich-text reads reject
  hidden content that would shift indices or truncate values. Inline-string
  replacements use the same structural check, preventing edits from silently
  discarding hidden rich text. Multiple text elements within one rich-text run
  are refused, since spreadsheet readers can interpret them differently and
  make an edit appear unchanged incorrectly.
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
- Valid character references in `xmlns:xml` declarations are accepted in
  worksheets, shared strings, and package metadata without changing their
  original spelling or byte offsets.
- Workbook properties inside markup-compatibility branches or matching
  `mc:ProcessContent` wrappers are refused as unsupported, avoiding an incorrect
  default date system when another spreadsheet reader processes those properties.
- Mixed strict and transitional SpreadsheetML is refused in workbook metadata,
  worksheets, and shared strings, preventing omitted data, incorrect date
  systems, and bypassed formula or merged-cell protections.
- ZIP archives that other readers could interpret differently are rejected,
  including an earlier end-of-central-directory record that libzip before 1.11
  would read and member names containing a backslash, which some readers treat
  as `/`. OLE compound files are recognized as unsupported formats. Binary
  workbook detection handles uppercase extensions.
- Saves check the end of their output with the reader's own end-record rules
  before writing the central directory. In the rare case that new offsets,
  checksums, or sizes, or records the save moves to a new distance from the end
  of the file, would form another end-of-directory record that readers could
  select, including an earlier one that libzip before 1.11 would read, or a
  ZIP64 locator signature before the end record, the save is refused with
  `Error::Unsupported`, as are compaction and `patch --check`.
- Non-ASCII ZIP member names require the UTF-8 flag in both headers, preventing
  filename aliases and worksheet selection differences between ZIP readers.
  Compaction also refuses to discard obsolete records with ambiguous encodings.
- ZIP patched-data entries stay opaque; reading, editing, and deleting obsolete
  patched records during compaction are refused. Compaction recognizes obsolete
  Stored records whose payload contains header- and descriptor-like bytes,
  including a header-like record with a ZIP64 size sentinel, which it used to
  refuse as an ambiguous stored data descriptor. Each obsolete record is parsed
  and inflated only within the gap that holds it.

### Saving

- Default saves replace changed ZIP records in physical order, so ordinary
  edited archives reopen in LibreOffice and repeated saves avoid accumulating
  obsolete worksheet copies. Untouched local records remain byte-identical;
  central-directory offsets may change. Existing opaque gaps remain protected.
  Explicit compaction can remove obsolete records from 0.2 append saves.
- Saves are atomic. Existing destination permissions are applied before contents
  are written; on Unix the replacement keeps the destination's group when it can
  be assigned, and otherwise loses group access. New destinations for workbooks
  opened from files use the input's permission bits as their creation mode,
  subject to the operating system's umask or default ACL rules, and the input's
  group under the same rule. ACLs are not copied; inherited directory ACLs can
  grant additional users access even when mode bits match. On Linux, a file's
  access-ACL mask does not become the owning group's permissions: a new or
  replacement file gets the owning group's own `group::` entry as limited by
  the mask, unless it inherits the destination directory's default ACL, which
  keeps the mask, and an ACL that cannot be read or parsed leaves owner-only
  bits. Setuid, setgid, and sticky bits are never kept. Unix saves sync the
  directory after the rename. Long destination filenames are supported.
  Temporary files are named `.{name}.sheetpatch-{pid}-{n}.tmp` after up to 32
  bytes of the destination's file name, up to any colon, so a copy left behind
  by a killed process can be traced to its workbook and deleted.

### Performance

- Worksheet parsing reads attributes in one pass into compact records that
  refer to the XML only by 32-bit offsets (48 bytes per element and 24 per
  attribute, down from 64 and 56), and shares namespace identifiers. Only
  attribute values that XML normalization changes are stored separately, and
  streaming shared-string parses release them with each string. Edits apply
  absolute spans without rebuilding cells. README.md lists measurements.
- Formula-cell protection checks each edited cell's own formula element instead
  of recording every formula cell during indexing.
- Shared strings are decoded while streaming, and package discovery retains only
  the elements it needs, greatly reducing peak memory for large parts.
- Multi-sheet edits remember failed shared-string loads within the transaction,
  avoiding repeated parsing of a damaged table during explicit cell repairs.
- Namespace resolution caches active prefix bindings and restores parent caches
  after shadowing. Package attribute and compatibility-rule lookups reuse URI
  fingerprints with exact equality checks, avoiding repeated long-URI scans.
- Excel text decoding copies ordinary UTF-8 directly and decodes only escape
  tokens, avoiding a temporary UTF-16 copy of values containing underscores.
- Numeric and predefined XML text references are validated and decoded without
  reconstructing and allocating each reference, while retaining invalid-reference
  diagnostics.
- Writing XML-forbidden characters appends their Excel escapes directly to the
  output buffer, avoiding a temporary string for every character.
- CLI `get` and `cells` buffer up to 4 MiB of formatted output while
  validating, so reads whose output fits decode each cell once, with no
  cell-count cap. A value whose stored text would overflow the buffer ends it
  before being copied, so repeated or oversized stored strings cannot multiply
  memory use. Cells beyond the buffer are validated before printing and reread
  individually.
- Prepared single-cell reads avoid allocating a batch result.
- CLI patches validate every row while retaining only each cell's final value,
  keeping repeated edits from accumulating discarded text. Diagnostic line
  numbers and the input row count remain unchanged.
- Table-header protection coalesces the rows that headers span and tests the
  edits in them against every header in one row sweep, so each edit is visited
  once however many tables overlap, and edits outside header rows are not
  examined.
- Table relationship targets are normalized against the worksheet's directory
  once. Spellings that differ only in letter case, unreserved-character
  escapes, or dot segments share one resolution, and each distinct table part
  is decompressed and parsed once per workbook, even when several worksheets
  relate to it. Every relationship's metadata is validated, and a table part
  that fails is reported again by each edit that needs it.
- Table-header checks compare values individually, avoiding a retained copy of
  every header value while preserving validation order.
- Insertions resolve each row's default style once, avoiding repeated attribute
  scans and numeric parsing. Existing-cell edits and missing-cell clears still
  skip row defaults, and column styles remain specific to each inserted cell.
- Compaction rejects impossible embedded ZIP header names before decoding and
  hashing them, avoiding repeated long-name scans inside obsolete stored data.
- Compaction searches for obsolete stored-stream descriptors only outside active
  ZIP records. Header-like bytes inside active payloads no longer cause false
  ambiguity errors or inflate the descriptor index.
- Replacement parts use Deflate level 4 instead of the default level 6, making
  saves of edited worksheets 2 to 3.4 times faster in the repository's
  measurements, with similar compressed size. A deflated replacement larger
  than 2 MiB is compressed in 1 MiB segments on up to eight threads, including
  the calling thread; its bytes do not depend on the thread count. ZIP saves
  borrow unchanged metadata, reuse validated physical entry order, and use
  optimized CRC32 checksums; in-memory output reserves archive-sized capacity.
- Benchmarks cover prepared and ordinary reads, wide rows, large cell extensions,
  shared strings, insertions, multi-sheet transactions, many-table batches,
  no-op batches, row-style inheritance, and control-heavy text. Workload filters
  select cases and operations, and every case verifies its output before measuring.

### Quality

- Deterministic randomized property tests, an independently exported LibreOffice
  workbook, and an optional LibreOfficeKit round-trip check complement the
  byte-preservation tests.
- CI lints the Windows MSVC target, runs the benchmark correctness checks, and
  audits Cargo.lock with `cargo audit` on every run and weekly.

Compatibility: Rust 1.88 or later; ZIP32 `.xlsx`, `.xlsm`, `.xltx`, and `.xltm`.
Formula creation/calculation, structural edits, ZIP64, encrypted workbooks, and
editing signed packages remain outside the supported scope.
