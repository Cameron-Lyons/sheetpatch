# Releasing sheetpatch

The 1.x series keeps the documented scalar editing and preservation API in
README.md stable. The next release is 1.2.0, which adds cell iterators and a
compression thread limit while retaining Rust 1.88 as the minimum supported
version. Formula calculation and structural editing remain outside its scope.
Public API removals or signature changes require a new major version.
New `Error` and `SheetVisibility` variants, and new
`CellContent` and `Error::ProtectedCell` fields, remain compatible because they
are non-exhaustive. Error message wording is not part of the stable API.
Keep the stated Rust minimum supported version accurate and explicitly document
any future increase.

## Candidate checks

Run these checks on the exact candidate commit:

```sh
cargo fmt --all -- --check
cargo fmt --all --manifest-path fuzz/Cargo.toml -- --check
cargo +nightly-2026-10-22 fmt --all -- --check --config imports_granularity=Module,format_macro_matchers=true
cargo +nightly-2026-10-22 fmt --all --manifest-path fuzz/Cargo.toml -- --check --config imports_granularity=Module,format_macro_matchers=true
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo clippy --locked --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings
rustup target add x86_64-pc-windows-msvc
cargo clippy --locked --target x86_64-pc-windows-msvc --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo test --locked --manifest-path fuzz/Cargo.toml --lib
cargo test --locked --lib --features quick-xml/escape-html
cargo test --locked --doc --all-features
cargo +1.88.0 test --locked --all-targets --all-features
cargo +1.88.0 test --locked --lib --features quick-xml/escape-html
cargo +1.88.0 test --locked --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps
cargo build --locked --release --all-targets --all-features
SHEETPATCH_BENCH_SAMPLES=1 \
SHEETPATCH_BENCH_SAMPLE_MS=0 \
SHEETPATCH_BENCH_OPERATIONS=bulk_at,prepared_single_reads,prepared_range_reads,indexed_cell_reads,shared_prepared_reads,narrow_column_reads,ordered_batch_reads \
cargo bench --locked --bench editing
cargo audit --deny unsound --deny yanked
cargo audit --file fuzz/Cargo.lock --deny unsound --deny yanked
cargo package --locked
cargo publish --locked --dry-run
```

The Windows-target Clippy pass lints `cfg(windows)` code, which a Unix build
never compiles; it type-checks without linking. The benchmark subset runs every
correctness check in the harness except the slow sequential-edit comparison and
does not compare timings; run it with more samples and no operation filter when
performance claims change. `cargo audit` (from the `cargo-audit` crate) fails on
vulnerable, unsound, or yanked locked dependencies and reports unmaintained
crates as warnings; review those warnings, including transitive dependencies in
both lockfiles. The `cargo +1.88.0` run requires that toolchain to be installed.
The nightly rustfmt run checks the Google Rust Style Guide's import grouping and
macro-matcher formatting, whose rustfmt options are unstable; it requires
`rustup toolchain install nightly-2026-10-22 --profile minimal --component rustfmt`.

GitHub CI repeats these checks. It runs for pull requests, pushes to `main` and
`v*` tags, manual dispatch, and a weekly schedule. A newer push to a pull request
cancels the superseded run; runs for `main`, tags, schedules, and manual dispatch
always complete. The aggregate `CI` job requires formatting, including the
nightly rustfmt import check, both Clippy passes, and documentation; tests with
stable Rust on Linux, macOS, and Windows and with Rust 1.88.0 on Linux; the
release build, benchmark correctness checks, packaging, publishing dry run, and
tests, installation, and JSON inspection of the extracted package; the
LibreOffice interoperability check; randomized property tests and bounded
fuzzing; and dependency audits of both lockfiles. Weekly scheduled runs repeat
the audits, extend the property and fuzz checks, and retain performance and
peak-memory measurements. Manual runs also record performance.

The fuzz runner and reproducible local commands are documented in README.md.
Retain the property/fuzz seed and any failure artifacts. When performance
changes, compare stable baseline and candidate binaries on the same quiet host
with `scripts/benchmark.py`; shared CI runner timings are history, not an
acceptance threshold.

`cargo package` verifies the extracted distributable, including both the library
and CLI. Inspect `cargo package --list` to ensure source, tests, fixtures, README,
license, and release notes are present and local build artifacts are excluded.
Test and install the extracted package, as CI does, rather than relying only
on the source checkout:

```sh
candidate=target/package/sheetpatch-1.2.0
cargo test --locked --all-targets --all-features --manifest-path "$candidate/Cargo.toml"
cargo test --locked --doc --all-features --manifest-path "$candidate/Cargo.toml"
cargo install --locked --path "$candidate" --root target/release-install
SHEETPATCH_INSTALLED_BINARY="$PWD/target/release-install/bin/sheetpatch" \
  cargo test --locked --manifest-path "$candidate/Cargo.toml" --test installed_cli -- --ignored
```

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

1. Confirm Cargo.toml, Cargo.lock, and the sheetpatch entry in fuzz/Cargo.lock
   contain `1.2.0`, and update the changelog from release candidate to the actual
   release date.
2. Commit the candidate in a normal Git checkout and require the complete CI
   matrix to pass for that exact commit. Do not release from an unverified tree.
3. Run the online dry run above, confirm crates.io ownership and credentials,
   then run `cargo publish --locked` when publication is authorized.
4. Tag that same commit `v1.2.0` and attach the changelog to the GitHub release.
5. Confirm the published crate installs with
   `cargo install sheetpatch --version 1.2.0 --locked`, reports
   `sheetpatch 1.2.0`, and that its docs.rs documentation builds successfully.

Preparing the package does not publish it, create a release, or run remote CI.
