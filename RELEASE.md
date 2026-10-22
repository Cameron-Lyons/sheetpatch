# Releasing sheetpatch

Version 1.0 stabilizes the documented scalar editing and preservation API in
README.md. It does not add formula calculation, structural editing, or other
spreadsheet object-model operations. Public API removals or signature changes
require a new major version. New `Error` and `SheetVisibility` variants, and new
`CellContent` and `Error::ProtectedCell` fields, remain compatible because they
are non-exhaustive. Error message wording is not part of the stable API.
Keep the stated Rust minimum supported version accurate and explicitly document
any future increase.

## Candidate checks

Run these checks on the exact candidate commit:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
rustup target add x86_64-pc-windows-msvc
cargo clippy --locked --target x86_64-pc-windows-msvc --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
cargo +1.88.0 test --locked --all-targets --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps
cargo build --locked --release --all-targets --all-features
SHEETPATCH_BENCH_SAMPLES=1 \
SHEETPATCH_BENCH_OPERATIONS=bulk_at,prepared_single_reads,shared_prepared_reads \
cargo bench --locked --bench editing
cargo audit --deny unsound --deny yanked
cargo package --locked
cargo publish --locked --dry-run
```

The Windows-target Clippy pass lints `cfg(windows)` code, which a Linux build
never compiles; it type-checks without linking. The benchmark subset runs every
correctness check in the harness except the slow sequential-edit comparison and
does not compare timings; run it with more samples and no operation filter when
performance claims change. `cargo audit` (from the `cargo-audit` crate) fails on
vulnerable, unsound, or yanked locked dependencies and reports unmaintained
crates as warnings; review those warnings, including transitive dependencies in
Cargo.lock. The `cargo +1.88.0` run requires that toolchain to be installed.

GitHub CI repeats these checks. It runs for pull requests, pushes to `main` and
`v*` tags, manual dispatch, and a weekly schedule. A newer push to a pull request
cancels the superseded run; runs for `main`, tags, schedules, and manual dispatch
always complete. The aggregate `CI` job requires formatting, both Clippy passes,
and documentation; tests with stable Rust on Linux, macOS, and Windows and with
Rust 1.88.0 on Linux; the release build, benchmark correctness checks, packaging,
publishing dry run, and tests, installation, and JSON inspection of the extracted
package; the LibreOffice interoperability check; and the dependency audit.
Scheduled runs only repeat the audit against the unchanged lockfile, so new
advisories surface between releases.

`cargo package` verifies the extracted distributable, including both the library
and CLI. Inspect `cargo package --list` to ensure source, tests, fixtures, README,
license, and release notes are present and local build artifacts are excluded.
With all dependencies already cached, build, test, documentation, and package
checks can also use `--offline`. The publishing dry run and audit database
update still require network access. Offline packaging does not verify registry
access, publishing credentials, or crate ownership.

Check representative workbooks produced by a spreadsheet application, including
any `.xlsm` files critical to the release. Byte preservation tests cover macros
as opaque data; they do not execute VBA or validate Excel's UI or recalculation.
Retain that distinction in release claims.

The committed LibreOffice fixture runs in the ordinary Rust test suite. With
LibreOfficeKit installed, also run the independent export/edit/reopen check:

```sh
cargo test --test libreoffice -- --ignored
```

This checks normal and compacted output, including styles, literal text, Unicode,
error values, blank cells, and formula fidelity. The Rust fixture separately
verifies unchanged stored formula caches; LibreOffice may recalculate on import.
It does not verify Excel's UI or execute VBA.

## Verification record

Record the verification of the exact candidate commit in the release notes or
pull request before publishing: the full commit hash, the link to its passing
`CI` run, the local toolchain and platform, which local checks ran and any that
could not (for example, a missing toolchain, registry access, or LibreOfficeKit),
and the date of the dependency audit. Describe results by check rather than by
hard-coded test counts, and repeat the record for any later commit; a record for
an earlier candidate does not carry over.

## Publication

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
