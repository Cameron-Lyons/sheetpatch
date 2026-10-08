`libreoffice.xlsx` was exported by LibreOffice 26.8.0.3 (Linux x86_64,
680 Build 3) through LibreOfficeKit on 2026-10-07. It is independent of
sheetpatch's ZIP and XML fixture builders. The original, synthetic source is
`libreoffice-source.ods`; both files are provided under this project's MIT license.

The workbook has two sheets (`Data` and `Notes`), a bold cell, shared strings,
Unicode text, numeric values, and the formula `B1*2` with a cached value of 25.
The Rust interoperability test reads and edits this immutable export without
requiring LibreOffice or any additional Rust dependencies.

To create a fresh export and verify both default saves and compact saves through
an independent reopen and ODS export, run:

```sh
python3 scripts/check_interoperability.py
```

The optional script needs an installed LibreOfficeKit. Its
`--libreoffice-program` option selects the installation directory containing
`libsofficeapp.so`, `libmergedlo.so`, or the corresponding platform library;
`--sheetpatch` selects an already-built executable. Otherwise it locates
LibreOffice and builds sheetpatch with `cargo build --locked`. It uses only
Python's standard library, an isolated temporary profile, and synthetic data.
It verifies values, formatting, formula fidelity, both sheets, and string handling.
The Rust test separately checks stored formula cache preservation; LibreOffice
may retain that cache or recalculate on import. The script does not request
formula recalculation or test Microsoft Excel or VBA.

The C API prefixes used by the script are defined in the upstream
[LibreOfficeKit header](https://github.com/LibreOffice/core/blob/master/include/LibreOfficeKit/LibreOfficeKit.h).
