#!/usr/bin/env python3
"""Measure CLI wall time and peak RSS, optionally interleaving two binaries.

Each measurement uses GNU time on Linux or BSD time on macOS to report the
CLI's peak resident memory without inheriting Python's startup memory. Wall
time includes the native time wrapper's startup/exit overhead. Fixture
creation, correctness validation and Python worker startup are excluded.
Requires Python's standard library and the platform's native time executable.
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import sys
import tempfile
import time
import zipfile
import xml.etree.ElementTree as ET


def worker(timer, command):
    with tempfile.TemporaryDirectory(prefix="sheetpatch-time-") as temporary:
        stats = Path(temporary) / "rss.txt"
        if sys.platform == "darwin":
            measured = [timer, "-l", *command]
        else:
            measured = [timer, "-f", "%M", "-o", str(stats), "--", *command]
        started = time.perf_counter_ns()
        process = subprocess.run(
            measured,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            env=dict(os.environ, LC_ALL="C"),
        )
        elapsed = (time.perf_counter_ns() - started) / 1e6
        if process.returncode:
            raise RuntimeError(
                f"measurement failed ({process.returncode}): {process.stderr.strip()}"
            )
        if sys.platform == "darwin":
            match = re.search(
                r"^\s*(\d+)\s+maximum resident set size\s*$",
                process.stderr,
                re.MULTILINE,
            )
            if not match:
                raise RuntimeError("BSD time did not report maximum resident set size")
            peak = int(match.group(1)) / 1024
        else:
            peak = int(stats.read_text().strip())
    print(json.dumps({"elapsed_ms": elapsed, "peak_rss_kib": peak}))


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest_file(file):
    digest = hashlib.sha256()
    while block := file.read(1024 * 1024):
        digest.update(block)
    return digest.digest()


def archive_parts(path):
    """Independently verify CRCs and retain each complete local ZIP record."""
    data = Path(path).read_bytes()
    with zipfile.ZipFile(path) as archive:
        entries = archive.infolist()
        require(
            len({entry.filename for entry in entries}) == len(entries),
            "duplicate ZIP members",
        )
        payloads = {entry.filename: archive.read(entry) for entry in entries}
        physical = sorted(entries, key=lambda entry: entry.header_offset)
        ends = [entry.header_offset for entry in physical[1:]] + [archive.start_dir]
        records = {
            entry.filename: data[entry.header_offset:end]
            for entry, end in zip(physical, ends)
        }
    return payloads, records


def validate_edit(name, arguments, cells):
    before, original_records = archive_parts(arguments[1])
    after, saved_records = archive_parts(arguments[2])
    sheet_path = "xl/worksheets/sheet1.xml"
    require(before.keys() == after.keys(), f"{name}: save added or dropped ZIP members")
    for part in before:
        if part != sheet_path:
            require(before[part] == after[part], f"{name}: changed untouched part {part}")
            require(
                original_records[part] == saved_records[part],
                f"{name}: changed untouched local ZIP record {part}",
            )
    namespace = "{http://schemas.openxmlformats.org/spreadsheetml/2006/main}"
    root = ET.fromstring(after[sheet_path])
    actual = root.findall(f"{namespace}sheetData/{namespace}row/{namespace}c")
    count = cells if name == "set_large_sheet" else 1000
    require(len(actual) == count, f"{name}: expected {count} cells, found {len(actual)}")
    for row, cell in enumerate(actual, 1):
        require(cell.get("r") == f"A{row}", f"{name}: wrong cell order/address at row {row}")
        require(cell.find(f"{namespace}f") is None, f"{name}: unexpected formula at A{row}")
        if name == "patch_insertion" or row == 1:
            expected = f"inserted {row}" if name == "patch_insertion" else "updated"
            require(cell.get("t") == "inlineStr", f"{name}: expected text at A{row}")
            text = "".join(
                element.text or ""
                for element in cell.findall(f"{namespace}is//{namespace}t")
            )
            require(text == expected, f"{name}: incorrect text at A{row}")
        else:
            require(cell.get("t") in (None, "n"), f"{name}: expected number at A{row}")
            require(
                cell.findtext(f"{namespace}v") == str(row),
                f"{name}: incorrect number at A{row}",
            )


def validate_warmup(name, binary, arguments, cells):
    """Validate exact read output and independently inspect saved ZIPs/XML."""
    with tempfile.TemporaryFile() as output:
        process = subprocess.run(
            [str(binary), *arguments], stdout=output, stderr=subprocess.PIPE
        )
        require(
            process.returncode == 0,
            f"{name}: {binary} failed: {process.stderr.decode(errors='replace')}",
        )
        require(not process.stderr, f"{name}: {binary} unexpectedly wrote to stderr")
        output.seek(0)
        if name.startswith("cells_"):
            expected = hashlib.sha256()
            count = cells if name == "cells_numeric" else min(cells, 20_000)
            for row in range(1, count + 1):
                if name == "cells_numeric":
                    record = f"A{row}\tnumber\t{row}\n"
                else:
                    record = f"A{row}\ttext\t{'text' * 128}\n"
                expected.update(record.encode())
            require(
                digest_file(output) == expected.digest(),
                f"{name}: {binary} returned incorrect cell output",
            )
        else:
            require(output.read(1) == b"", f"{name}: {binary} unexpectedly wrote to stdout")
            validate_edit(name, arguments, cells)


def write_workbook(path, sheet, shared=None):
    main = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
    office = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
    package = "http://schemas.openxmlformats.org/package/2006/relationships"
    strings = "" if shared is None else f'<Relationship Id="strings" Type="{office}/sharedStrings" Target="sharedStrings.xml"/>'
    parts = {
        "[Content_Types].xml": '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>',
        "_rels/.rels": f'<Relationships xmlns="{package}"><Relationship Id="office" Type="{office}/officeDocument" Target="xl/workbook.xml"/></Relationships>',
        "xl/workbook.xml": f'<workbook xmlns="{main}" xmlns:r="{office}"><sheets><sheet name="Data" sheetId="1" r:id="sheet"/></sheets></workbook>',
        "xl/_rels/workbook.xml.rels": f'<Relationships xmlns="{package}"><Relationship Id="sheet" Type="{office}/worksheet" Target="worksheets/sheet1.xml"/>{strings}</Relationships>',
        "xl/worksheets/sheet1.xml": f'<worksheet xmlns="{main}"><sheetData>{sheet}</sheetData></worksheet>',
    }
    if shared is not None:
        parts["xl/sharedStrings.xml"] = f'<sst xmlns="{main}"><si><t>{shared}</t></si></sst>'
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, text in parts.items():
            archive.writestr(name, text)


def fixtures(directory, cells):
    numeric = directory / "numeric.xlsx"
    shared = directory / "shared.xlsx"
    empty = directory / "empty.xlsx"
    write_workbook(numeric, "".join(f'<row r="{r}"><c r="A{r}"><v>{r}</v></c></row>' for r in range(1, cells + 1)))
    write_workbook(shared, "".join(f'<row r="{r}"><c r="A{r}" t="s"><v>0</v></c></row>' for r in range(1, min(cells, 20_000) + 1)), "text" * 128)
    write_workbook(empty, "")
    patch = directory / "edits.tsv"
    patch.write_text("".join(f"Data\tA{r}\ttext\tinserted {r}\n" for r in range(1, 1001)))
    output = directory / "output.xlsx"
    return {
        "cells_numeric": ["cells", str(numeric), "Data"],
        "cells_shared": ["cells", str(shared), "Data"],
        "set_large_sheet": ["set", str(numeric), str(output), "Data", "A1", "text", "updated"],
        "patch_insertion": ["patch", str(empty), str(output), str(patch)],
    }


def summary(samples):
    result = {"samples": samples}
    for metric in ("elapsed_ms", "peak_rss_kib"):
        values = sorted(sample[metric] for sample in samples)
        result[metric] = {
            "median": statistics.median(values),
            "min": values[0],
            "p90": values[math.ceil(len(values) * .9) - 1],
            "max": values[-1],
        }
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, help="optional baseline executable, timed on the same machine")
    parser.add_argument("--samples", type=int, default=7)
    parser.add_argument(
        "--time",
        type=Path,
        default=Path(os.environ.get("SHEETPATCH_TIME", "/usr/bin/time")),
        help="GNU time on Linux or BSD time on macOS (default: SHEETPATCH_TIME or /usr/bin/time)",
    )
    parser.add_argument("--cells", type=int, default=200_000)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.samples < 1 or not 1 <= args.cells <= 1_048_576:
        parser.error("samples must be positive and cells must fit Excel's row limit")
    if sys.platform not in ("linux", "darwin"):
        parser.error("peak RSS measurement requires Linux or macOS")
    timer = args.time.resolve()
    if not timer.is_file() or not os.access(timer, os.X_OK):
        parser.error(
            f"native time executable not found: {timer}; "
            "install GNU time on Linux or set --time/SHEETPATCH_TIME"
        )
    binaries = {"candidate": args.candidate.resolve()}
    if args.baseline:
        binaries["baseline"] = args.baseline.resolve()
    report = {
        "schema": 2,
        "platform": platform.platform(),
        "python": platform.python_version(),
        "cpu_count": os.cpu_count(),
        "measurement": {
            "wall_time": "CLI plus native time wrapper startup/exit; excludes Python worker startup, fixture generation and correctness checks",
            "peak_rss": "CLI process maximum resident set size, reported in KiB by native time",
            "time_executable": str(timer),
            "time_sha256": hashlib.sha256(timer.read_bytes()).hexdigest(),
            "correctness": "exact numeric/shared read output; edited cell values; ZIP CRCs and untouched local records",
        },
        "cells": args.cells,
        "binaries": {label: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()} for label, path in binaries.items()},
        "workloads": {},
    }
    with tempfile.TemporaryDirectory(prefix="sheetpatch-benchmark-") as temporary:
        for name, arguments in fixtures(Path(temporary), args.cells).items():
            measurements = {label: [] for label in binaries}
            # Untimed warm-up for both versions. Alternating measurement order
            # reduces systematic bias from caches and changing CPU frequency.
            for binary in binaries.values():
                validate_warmup(name, binary, arguments, args.cells)
            for sample in range(args.samples):
                order = list(binaries.items())
                if sample % 2:
                    order.reverse()
                for label, binary in order:
                    process = subprocess.run(
                        [sys.executable, __file__, "--worker", str(timer), str(binary), *arguments],
                        capture_output=True,
                        text=True,
                    )
                    require(
                        process.returncode == 0,
                        f"{name}: {label} measurement failed: {process.stderr.strip()}",
                    )
                    measurements[label].append(json.loads(process.stdout))
            results = {label: summary(values) for label, values in measurements.items()}
            if "baseline" in results:
                results["candidate_over_baseline"] = {
                    metric: results["candidate"][metric]["median"]
                    / results["baseline"][metric]["median"]
                    for metric in ("elapsed_ms", "peak_rss_kib")
                }
            report["workloads"][name] = results
            print(
                name,
                json.dumps({
                    label: value["elapsed_ms"]["median"]
                    for label, value in results.items() if label in binaries
                }),
                flush=True,
            )
    for label, binary in binaries.items():
        require(
            hashlib.sha256(binary.read_bytes()).hexdigest()
            == report["binaries"][label]["sha256"],
            f"{label} executable changed during the benchmark; rerun against stable binaries",
        )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--worker":
        worker(sys.argv[2], sys.argv[3:])
    else:
        main()
