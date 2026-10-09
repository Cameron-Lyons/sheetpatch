# Releasing sheetpatch

Version 1.0 stabilizes the documented scalar editing and preservation API in
README.md. It does not add formula calculation, structural editing, or other
spreadsheet object-model operations. Public API removals or signature changes
require a new major version. New `Error` variants remain compatible because the
enum is non-exhaustive. Keep the stated Rust minimum supported version accurate
and explicitly document any future increase.

## Candidate checks

Run these checks on the exact candidate source. A passing aggregate `CI` job
must cover stable Rust on Linux, macOS, and Windows, and Rust 1.88.0 on Linux.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps
cargo build --locked --release --all-targets --all-features
cargo package --locked
cargo publish --locked --dry-run
```

`cargo package` verifies the extracted distributable, including both the library
and CLI. Inspect `cargo package --list` to ensure source, tests, fixtures, README,
license, and release notes are present and local build artifacts are excluded.
With all dependencies already cached, build, test, documentation, and package
checks can also use `--offline`. The publishing dry run still requires network
access. Offline packaging does not verify registry access, publishing credentials,
or crate ownership.

Check representative workbooks produced by a spreadsheet application, including
any `.xlsm` files critical to the release. Byte preservation tests cover macros
as opaque data; they do not execute VBA or validate Excel's UI or recalculation.
Retain that distinction in release claims. Review current dependency advisories
before publishing, including transitive dependencies in Cargo.lock.

The committed LibreOffice fixture runs in the ordinary Rust test suite. With
LibreOfficeKit installed, also run the independent export/edit/reopen check:

```sh
python3 scripts/check_interoperability.py
```

This checks normal and compacted output, including styles, literal text, Unicode,
and formula fidelity. The Rust fixture separately verifies unchanged stored
formula caches; LibreOffice may recalculate on import. It does not verify Excel's
UI or execute VBA.

## Publication

Local candidate verification on 2026-10-08 passed with Rust 1.99.0 on Linux:

- Formatting, strict Clippy, all 102 tests, both documentation examples,
  documentation with warnings denied, and release builds.
- Offline packaging and the complete test suite in the extracted crate.
- Installation from the extracted crate, version output, and a fixture read.
- LibreOffice export/edit/reopen checks for default and compact saves, plus
  independent reopens of edits to a workbook with URI-escaped worksheet names.

These results cover the local candidate. Git metadata and remote CI results are
unavailable in this workspace. Rust 1.88.0 is not installed, and its download and
the online publishing dry run failed because network hostnames could not resolve.
Before publication, require the configured Windows/macOS/MSRV CI jobs and the
online dry run to pass for the final candidate commit.

1. Confirm Cargo.toml and Cargo.lock both contain `1.0.0`, and update the changelog
   from release candidate to the actual release date.
2. Commit the candidate in a normal Git checkout and require the complete CI
   matrix to pass for that exact commit. Do not release from an unverified tree.
3. Run the online dry run above, confirm crates.io ownership and credentials,
   then run `cargo publish --locked` when publication is authorized.
4. Tag that same commit `v1.0.0` and attach the changelog to the GitHub release.
5. Confirm the published crate installs with
   `cargo install sheetpatch --version 1.0.0 --locked`, reports
   `sheetpatch 1.0.0`, and that its docs.rs documentation builds successfully.

Preparing the package does not publish it, create a release, or run remote CI.
