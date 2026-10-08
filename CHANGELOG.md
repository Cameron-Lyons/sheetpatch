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
- Atomic saves preserve private destination permissions before writing temporary
  contents and support long destination filenames.
- Native CLI filesystem paths work independently of Unicode text arguments;
  closed output pipes are handled without a panic.
- Clearing an empty typed cell removes its type while retaining formatting and
  unfamiliar sibling content.
- Workbook sheet names decode Excel escape sequences. Ambiguous workbook layouts,
  invalid package XML, and duplicate expanded XML attributes are rejected.
- Empty worksheet batches validate the sheet name consistently with reads and
  individual edits.
- Public API documentation is required at compile time. `Error` is now
  non-exhaustive, so downstream matches must include a fallback arm.
- An independently exported LibreOffice workbook and optional LibreOfficeKit
  round-trip check cover application interoperability.

Compatibility: Rust 1.88 or later; ZIP32 `.xlsx`, `.xlsm`, `.xltx`, and `.xltm`.
Formula creation/calculation, structural edits, ZIP64, encrypted workbooks, and
editing signed packages remain outside the supported scope.
