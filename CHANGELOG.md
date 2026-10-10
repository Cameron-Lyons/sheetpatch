# Changelog

## 1.0.0 — release candidate

The first stable release covers surgical scalar cell edits in existing OOXML
workbooks. See README.md for the preservation contract and supported scope.

- Stable Rust API for typed addresses, scalar values, cell inspection, batch and
  cross-worksheet transactions, streaming output, and explicit ZIP compaction.
- Default saves replace changed ZIP records in physical order, so ordinary
  edited archives reopen in LibreOffice and repeated saves avoid accumulating
  obsolete worksheet copies. Untouched local records remain byte-identical;
  central-directory offsets may change. Existing opaque gaps remain protected.
  Explicit compaction can remove obsolete records from 0.2 append saves.
- CLI commands for listing sheets, reading cells, setting values, TSV patches,
  and compaction, plus `--help` and `--version`.
- CLI reads accept multiple cells with one worksheet parse and preserve input
  order. `get --json` includes scalar types, values, stored formulas, and styles;
  `list --json` exposes worksheet names and package paths for scripts.
- `patch --check` validates a complete edit transaction and output generation
  against workbook protections and ZIP limits without writing a file, with
  support for TSV files and stdin.
- Per-command help is available through `help COMMAND` and `COMMAND --help`/`-h`.
- Typed `get_cell_at`, `get_cells_at`, `set_cell_at`, and `set_cells_at` APIs accept
  validated `CellRef` values directly. CLI batch reads use the typed path, avoiding
  address conversion and reparsing; JSON addresses are formatted without temporary
  strings.
- `prepare_sheet` returns a read-only `WorksheetView` for repeated reads using
  one validated worksheet index. Index retention is limited to the view's lifetime;
  ordinary reads retain their existing memory behavior.
- Worksheet parsing, indexing, protection checks, value decoding, and patch
  planning have separate internal modules. Attributes and row cells use compact
  shared buffers, namespaces use canonical identifiers, and edits apply absolute
  spans without rebuilding each cell.
- Per-sheet state groups original and pending XML. ZIP writers resolve borrowed
  replacements to entry indexes once and share normal/compact record emission.
- Benchmarks cover prepared and ordinary reads, wide rows, large cell extensions,
  shared strings, insertions, and multi-sheet transactions, with workload filters.
- Invalid CLI addresses and values are reported as usage errors before opening
  a workbook; failed batch reads produce no partial output.
- Relationship URI resolution rejects directory targets and unresolved empty
  path segments, preventing malformed targets from aliasing valid package parts.
- ZIP patched-data entries stay opaque; reading, editing, and deleting obsolete
  patched records during compaction are refused.
- Compaction recognizes obsolete Stored records whose payload contains header
  and descriptor-like bytes, retaining neighboring active parts unchanged.
- Worksheet numeric attributes and scalar values accept surrounding XML
  whitespace, including shared-formula indices, styles, and shared-string indices.
- Shared-string and rich-text reads reject content hidden in unsupported XML
  containers, preventing shifted indices and incomplete text values.
- Atomic saves preserve private destination permissions before writing temporary
  contents and support long destination filenames. On Unix, temporary replacements
  use restricted permissions at creation, closing the window before chmod.
- Native CLI filesystem paths work independently of Unicode text arguments;
  closed output pipes are handled without a panic. Invalid Unicode text arguments
  are rejected before opening the input workbook, consistently using exit code 2.
- Invalid UTF-8 patch rows report their line number and exit with code 2 for
  both file and standard-input patches, leaving the destination unchanged.
- Scalar changes to cells with value metadata are refused, preserving linked
  and rich data types. Unchanged values and unrelated cell edits retain metadata.
- Cell values, formulas, and merged ranges in unsupported XML containers cannot
  bypass worksheet edit protections.
- Shared-formula indices compare numerically, so equivalent spellings such as
  `1` and `01` no longer prevent edits to unrelated cells. Invalid indices are
  rejected.
- Reserved XML namespace bindings are checked after character-reference
  normalization in both package metadata and worksheet XML.
- Clearing an empty typed cell removes its type while retaining formatting and
  unfamiliar sibling content.
- Workbook sheet names decode Excel escape sequences. Ambiguous workbook layouts,
  invalid package XML, and duplicate expanded XML attributes are rejected.
- Package relationships resolve URI-escaped and case-insensitive OPC part names
  without changing ZIP names. Canonical escaped names take precedence over legacy
  decoded names; ambiguous equivalent names for an accessed part are rejected.
- Empty worksheet batches validate the sheet name consistently with reads and
  individual edits.
- Binary workbook detection handles ZIP member names with uppercase extensions.
- Worksheet parsing borrows XML names and attribute values and shares decoded
  namespaces. Shared-string reads retain one worksheet parse; worksheet batches
  avoid cloning a sheet name for every edit. ZIP saves reuse validated physical
  entry order and use optimized CRC32 checksums.
- Worksheet child traversal uses ranges in the existing parse index, reducing
  allocations. Unchanged edits retain the current XML without a full copy or
  restaging pending changes. Single-sheet setters avoid a redundant grouping map.
- ZIP saves borrow unchanged metadata tails instead of cloning local headers,
  central records, and archive comments. In-memory output reserves archive-sized
  capacity to reduce buffer growth. Benchmarks also cover batch reads and no-ops.
- Public API documentation is required at compile time. `Error` is now
  non-exhaustive, so downstream matches must include a fallback arm.
- An independently exported LibreOffice workbook and optional LibreOfficeKit
  round-trip check cover application interoperability.

Compatibility: Rust 1.88 or later; ZIP32 `.xlsx`, `.xlsm`, `.xltx`, and `.xltm`.
Formula creation/calculation, structural edits, ZIP64, encrypted workbooks, and
editing signed packages remain outside the supported scope.
