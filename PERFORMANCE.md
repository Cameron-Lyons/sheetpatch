# Performance

Batch edits parse each affected worksheet once and apply all XML changes in one
pass. Formula-cell guards inspect each edited cell's own markup instead of
recording every formula cell while indexing. Formula-range and merged-range
guards use direct membership checks for small collections of edits or ranges.
Larger batches exclude ranges outside the edited cells' bounding rectangle
before allocating a sweep index. Table-header guards coalesce the rows that
headers span and test the edits in those rows against every header in one row
sweep, so overlapping headers do not repeat work for each table; current header
values are read through the edit's own worksheet index and compared one at a
time. A worksheet's tables are resolved only by a batch that changes it, after
its edits are planned and before the edited XML is written. Table targets are
normalized against the worksheet's directory once, so spellings that differ
only in letter case, escapes of unreserved characters, or dot segments share
one resolution, and each distinct table part is decompressed and parsed once
per workbook, even when several worksheets relate to it. Inserted cells resolve
their row's default style once, while column defaults remain specific to each
column. Row defaults are validated only when needed.
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
restores cached parent bindings after nested declarations. XML 1.0 character
checks scan each part's UTF-8 bytes in vectorizable chunks rather than decoding
every character. Package parts are checked by the same validating parser core
as worksheets, with the same compact namespace identifiers, so a tag in a
package part briefly costs what it does in a worksheet: a 24-byte record per
attribute, each value that XML normalization changes, and past 16 attributes a
set of their names. Package discovery validates whole parts but retains only
the elements it needs, and clears its namespace table after 64 distinct URI
spellings, so namespace declarations in markup it does not retain do not grow
its memory. Relationships parts are checked as they stream: only the
relationships that discovery uses are copied, and duplicate IDs are compared as
spans of the part.

One patch plan applies payload and attribute changes directly to worksheet spans;
unchanged cell extensions are copied only into the final output. Edits that
leave a worksheet unchanged retain its current XML without copying it. Shared
strings are decoded while streaming, releasing each entry's XML as it completes,
and Excel text escapes are decoded without converting ordinary UTF-8 to a
temporary UTF-16 buffer. Numeric and predefined XML text references are decoded
directly, avoiding temporary strings for each reference. Writing XML-forbidden
characters appends Excel escapes directly to the output buffer. The
shared-string table is decompressed and parsed at most once per workbook, and
the outcome is kept even when the table is damaged, so later reads and edits
neither parse it again nor change its error. Its decoded text is held in one
buffer with a 32-bit end offset per entry, rather than one allocation per
string. Prepared worksheet views reuse a validated index across reads, with
retention controlled by the view's lifetime; single-cell reads avoid
allocating a batch result. Simple scalar payloads borrow the validated XML
instead of allocating a temporary string before parsing a number, boolean,
date, or shared-string index. CLI `cells` traverses indexed records directly,
without collecting addresses or searching for them again. CLI reads buffer up to 4 MiB of
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
eight threads by default including the calling thread, and never more than
`std::thread::available_parallelism` or the segment count. Its bytes do not
depend on the thread count and are up to about 0.25% larger than a single
stream's. Applications saving workbooks concurrently can cap each workbook's
workers with `book.set_compression_threads(NonZeroUsize::new(1))`, importing
`std::num::NonZeroUsize`. The cap applies to every normal and compact save,
including streams; `None` restores automatic selection, and
`compression_threads()` reports the configured cap. This setting does not
change saved bytes or pending edits. Where a thread cannot be started, for example under a
limit on the user's processes, the calling thread compresses its segments
instead. Untouched parts keep their original compressed bytes.

Run the dependency-free benchmark with:

```sh
SHEETPATCH_BENCH_SAMPLES=7 cargo bench --locked --bench editing
```

The harness independently checks expected worksheet XML and untouched compressed
ZIP parts before measuring, and compares sequential edits, batching, streaming,
compaction, batch reads, prepared reads, and unchanged batches. It also exercises
wide rows, unfamiliar cell extensions, shared strings, new-row insertion,
multiple worksheets, bulk edits across thousands of tables, row-style inheritance,
and text requiring many Excel escapes.
Measurements calibrate repeated operations to at least 50 ms per sample,
include destruction of returned results, and report iteration count, minimum,
median, 90th percentile, and maximum in microseconds. Fixture construction is
excluded; the reported distribution is over per-operation averages of timed
batches, rather than individual request latencies.
`SHEETPATCH_BENCH_SAMPLE_MS` changes the calibration target; zero
selects one operation per sample for quick correctness checks. These timing
boundaries differ from the historical measurements below.
The `read_paths` case exercises narrow columns in a tall, wide sheet and ordered,
sparse, shuffled, duplicate, and blank-heavy batches. The `empty_rows` case
checks range reads over 20,000 styled rows containing zero or three cells.
Both check expected values before timing, including request order and blanks.
Use `SHEETPATCH_BENCH_CASES` to select comma-separated cases
(`narrow_10000`, `narrow_50000`, `guarded`, `wide`, `opaque`, `shared_strings`,
`insertion`, `multisheet`, `tables`, `row_style`, `control_text`, `read_paths`, `empty_rows`) and
`SHEETPATCH_BENCH_OPERATIONS` to select exact operation names printed by the
harness. For example:

```sh
SHEETPATCH_BENCH_CASES=narrow_50000 \
SHEETPATCH_BENCH_OPERATIONS=bulk_at,get_cells_at,prepared_single_reads,prepared_range_reads \
SHEETPATCH_BENCH_SAMPLES=5 cargo bench --locked --bench editing
```

For CLI wall time and peak resident memory on Linux or macOS, use Python 3
and GNU `/usr/bin/time` on Linux or BSD `/usr/bin/time` on macOS:

```sh
python3 scripts/benchmark.py --candidate target/release/sheetpatch --samples 7 --output target/cli-performance.json
python3 scripts/benchmark.py --candidate target/release/sheetpatch --baseline /path/to/previous/sheetpatch --samples 7 --output target/comparison.json
```

The script measures numeric and shared-string whole-sheet reads, a large-sheet
save, and row insertions. Untimed warmups validate read output, edits, ZIP
checksums, and untouched records. Each CLI measurement runs in a fresh worker;
fixture generation and Python startup are excluded, while the native time
wrapper's startup/exit overhead is included. Native time measures the CLI's
peak RSS without Python's inherited memory footprint. Use `--time PATH` or
`SHEETPATCH_TIME` when the executable lives elsewhere. Comparisons alternate binary order
on the same host and include raw samples, variability, executable hashes, and
candidate/baseline ratios. Weekly and manually dispatched CI runs retain
library timings, CLI memory results, and compiler/runner details for 90 days.
Use a quiet host for acceptance decisions: shared CI runners are useful for
history, but their noise is unsuitable for a hard performance gate.

### Prepared-read measurements

On 2026-11-10, the original and updated read algorithms were compared using the
same benchmark harness on an Apple M4, macOS arm64, Rust 1.99.1, with release
builds. Four pairs alternated baseline and candidate order. Each run took five
samples calibrated to 30 ms; the table reports the median of those four run
medians, in microseconds. The fixture contains 2,000 rows and 128 columns;
view construction is excluded and returned values are dropped inside timing.
These are local synthetic measurements, not thresholds or guarantees.

| Operation | Original | Updated |
| --- | ---: | ---: |
| One column across 1,000 rows | 134.022 | 95.685 |
| Ordered dense batch | 398.193 | 275.026 |
| Ordered sparse batch | 660.773 | 528.082 |
| Ordered batch with duplicates | 2,438.235 | 1,653.296 |
| Batch including missing cells | 117.347 | 71.526 |
| Full-width range (control) | 5,685.461 | 5,732.396 |
| Whole-sheet iteration (control) | 11,161.661 | 11,413.260 |
| Shuffled batch (control) | 1,041.702 | 1,005.056 |

The existing small-range cases on one-column sheets stayed between 6 and 7
microseconds. Whole-sheet, full-width, shuffled, and small-range controls
varied by about 4% or less; these small differences are not claimed as gains.
The empty-row case returns without scanning row records, and its three-cell
control took 0.116 versus 0.118 microseconds. Sparse sheets filter existing
cells directly so empty rows do not add work; dense rows seek column bounds.
Ordered batches retain a row-local cursor, while unordered batches keep the
ordinary lookup path. Reductions fold contiguous slices without rechecking
the traversal mode per cell. No additional index is retained, and request and
error order stay unchanged.

To rerun the new workloads:

```sh
SHEETPATCH_BENCH_CASES=read_paths,empty_rows SHEETPATCH_BENCH_SAMPLES=7 \
cargo bench --locked --bench editing
```

### Historical 1.0 measurements

The following measurements compare 1.0.0 with an earlier release candidate,
commit `32b1d6c`, on an Intel Core Ultra 5 325 (8 cores), Linux x86_64, Rust
1.99.1, using release builds and synthetic workloads. Benchmark rows are harness
medians. The other rows report the median CLI wall time and peak resident
memory of 15 to 30 interleaved runs, except the earlier candidate's 3.2 GiB,
which comes from an earlier run with Rust 1.99.0. They describe these
workloads, not a guarantee for every workbook, and vary between runs and
machines.

| Workload | Earlier candidate | 1.0.0 |
| --- | ---: | ---: |
| `narrow_50000` `bulk_at`, 500 edits | 50 ms | 26 ms |
| `narrow_50000` `get_cells_at`, 500 reads | 47 ms | 24 ms |
| `narrow_50000` `cold_single_read` | 49 ms | 27 ms |
| `guarded_bulk_at` | 25 ms | 14 ms |
| `multisheet_apply`, 128 edits across 4 worksheets | 6.8 ms | 4.2 ms |
| CLI `get` of one cell in a 200,000-cell worksheet | 155 ms, 95.5 MiB | 86 ms, 48.7 MiB |
| Reading from a 61 MB `sharedStrings.xml` | 1,004 ms, 731 MiB | 543 ms, 94.0 MiB |
| Opening with a padded 16 MiB `[Content_Types].xml` | 819 MiB | 19 MiB |
| Opening with a 63 MiB `workbook.xml` | 3.2 GiB | 66 MiB |
| CLI `set` of one cell in an 800,000-cell, 36 MB worksheet | 1,031 ms, 405 MiB | 390 ms, 217 MiB |

Worksheet parsing alone was about 40-49% faster and index construction 44-55%
faster. Recompressing replacement parts at Deflate level 4 instead of the default
level 6, in 1 MiB segments on several threads for parts over 2 MiB, made saving
an edited worksheet about 3 times faster for a 0.7 MB worksheet and 5 to 17
times faster for worksheets of 3.5 to 42 MB, with recompressed parts within 0.6%
of their level 6 size.

### Historical 0.2 measurements

Initial 0.2 measurements used one release-mode sample per synthetic workload on
the same machine with Rust 1.99.0:

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
