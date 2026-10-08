#!/usr/bin/env python3
"""Optional XLSX export/edit/reopen check using an installed LibreOfficeKit.

Run from a checkout: python3 scripts/check_interoperability.py
Pass --libreoffice-program /path/to/libreoffice/program for another installation.
Uses Python's standard library; LibreOffice is the independent producer/reader.
"""

import argparse
import ctypes
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import xml.etree.ElementTree as ET
import zipfile


class OfficeFunctions(ctypes.Structure):
    # Stable C API prefix from LibreOfficeKit/LibreOfficeKit.h.
    _fields_ = [("size", ctypes.c_size_t)] + [
        (name, ctypes.c_void_p)
        for name in ("destroy", "load", "error", "load_options", "free_error")
    ]


class DocumentFunctions(ctypes.Structure):
    _fields_ = [("size", ctypes.c_size_t)] + [
        (name, ctypes.c_void_p) for name in ("destroy", "save")
    ]


def function(result, *arguments):
    return ctypes.CFUNCTYPE(result, *arguments)


def table(pointer, structure):
    address = ctypes.cast(pointer, ctypes.POINTER(ctypes.c_void_p))[0]
    value = ctypes.cast(address, ctypes.POINTER(structure)).contents
    if value.size < ctypes.sizeof(structure):
        raise RuntimeError("LibreOfficeKit's API is too old for this check")
    return value


class Office:
    def __init__(self, program, directory):
        os.environ["SAL_USE_VCLPLUGIN"] = "svp"
        os.environ["XDG_CACHE_HOME"] = str(directory / "cache")
        library = next(
            (program / name for name in (
                "libsofficeapp.so", "libmergedlo.so", "libsofficeapp.dylib",
                "libmergedlo.dylib", "sofficeapp.dll", "mergedlo.dll",
            ) if (program / name).is_file() and (program / name).stat().st_size > 1000),
            None,
        )
        if library is None:
            raise RuntimeError(f"LibreOfficeKit library not found in {program}")
        self.library = ctypes.CDLL(str(library))
        hook = self.library.libreofficekit_hook_2
        hook.argtypes = [ctypes.c_char_p, ctypes.c_char_p]
        hook.restype = ctypes.c_void_p
        self.pointer = hook(
            os.fsencode(program), (directory / "profile").as_uri().encode()
        )
        if not self.pointer:
            raise RuntimeError("LibreOfficeKit could not initialize")
        self.api = table(self.pointer, OfficeFunctions)

    def error(self):
        pointer = function(ctypes.c_void_p, ctypes.c_void_p)(self.api.error)(
            self.pointer
        )
        if not pointer:
            return "unknown LibreOfficeKit error"
        message = ctypes.string_at(pointer).decode("utf-8", "replace")
        function(None, ctypes.c_void_p)(self.api.free_error)(pointer)
        return message

    def convert(self, source, destination, file_format):
        document = function(
            ctypes.c_void_p, ctypes.c_void_p, ctypes.c_char_p
        )(self.api.load)(self.pointer, source.as_uri().encode())
        if not document:
            raise RuntimeError(f"LibreOffice could not open {source}: {self.error()}")
        api = table(document, DocumentFunctions)
        try:
            saved = function(
                ctypes.c_int, ctypes.c_void_p, ctypes.c_char_p,
                ctypes.c_char_p, ctypes.c_char_p,
            )(api.save)(
                document, destination.as_uri().encode(), file_format.encode(),
                None,
            )
            if not saved or not destination.is_file():
                raise RuntimeError(f"LibreOffice could not save {destination}: {self.error()}")
        finally:
            function(None, ctypes.c_void_p)(api.destroy)(document)

    def close(self):
        function(None, ctypes.c_void_p)(self.api.destroy)(self.pointer)


def check_ods(path):
    # Inspect values exported by LibreOffice after it has reopened the XLSX.
    namespaces = {
        name: f"urn:oasis:names:tc:opendocument:xmlns:{name}:1.0"
        for name in ("office", "table", "text", "style")
    }
    with zipfile.ZipFile(path) as archive:
        content = ET.fromstring(archive.read("content.xml"))
        styles = ET.fromstring(archive.read("styles.xml"))
    style_names = {
        style.get(f"{{{namespaces['style']}}}name"): style
        for root in (styles, content)
        for style in root.findall(".//style:style", namespaces)
    }
    sheets = content.findall("office:body/office:spreadsheet/table:table", namespaces)
    assert [sheet.get(f"{{{namespaces['table']}}}name") for sheet in sheets] == ["Data", "Notes"]
    cells = {}
    for sheet in sheets:
        name = sheet.get(f"{{{namespaces['table']}}}name")
        row_index = 0
        for row in sheet.findall("table:table-row", namespaces):
            if row_index >= 3:
                break
            column = 0
            for cell in row:
                if column >= 6:
                    break
                repeated = int(cell.get(f"{{{namespaces['table']}}}number-columns-repeated", "1"))
                for offset in range(min(repeated, 6 - column)):
                    cells[(name, row_index, column + offset)] = cell
                column += repeated
            row_index += int(row.get(f"{{{namespaces['table']}}}number-rows-repeated", "1"))

    def value(sheet, row, column):
        cell = cells[(sheet, row, column)]
        kind = cell.get(f"{{{namespaces['office']}}}value-type")
        if kind == "float":
            return float(cell.get(f"{{{namespaces['office']}}}value"))
        if kind == "boolean":
            return cell.get(f"{{{namespaces['office']}}}boolean-value") == "true"
        return "\n".join("".join(p.itertext()) for p in cell.findall("text:p", namespaces))

    def is_bold(cell):
        name = cell.get(f"{{{namespaces['table']}}}style-name")
        seen = set()
        while name in style_names and name not in seen:
            seen.add(name)
            style = style_names[name]
            properties = style.find("style:text-properties", namespaces)
            if properties is not None:
                weight = properties.get("{urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0}font-weight")
                if weight is not None:
                    return weight == "bold"
            name = style.get(f"{{{namespaces['style']}}}parent-style-name")
        return False

    assert value("Data", 0, 0) == "=literal text"
    assert is_bold(cells[("Data", 0, 0)]), "edited cell lost its bold formatting"
    assert value("Data", 0, 1) == 21
    assert value("Data", 0, 2) is False
    # Sheetpatch retains the original cache; an independent application may
    # either keep it or recalculate on import according to its own settings.
    assert value("Data", 0, 3) in (25, 42)
    assert cells[("Data", 0, 3)].get(f"{{{namespaces['table']}}}formula") == "of:=[.B1]*2"
    assert value("Data", 0, 4) == "Café ☕ _x0041_\nnext line"
    assert value("Data", 2, 5) == "Inserted"
    assert value("Notes", 0, 0) == "Keep this sheet"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--libreoffice-program", type=Path)
    parser.add_argument("--sheetpatch", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    program = args.libreoffice_program
    if program is None:
        launcher = shutil.which("libreoffice") or shutil.which("soffice")
        if launcher is None:
            parser.error("install LibreOffice or pass --libreoffice-program")
        program = Path(launcher).resolve().parent
    binary = args.sheetpatch
    if binary is None:
        subprocess.run(["cargo", "build", "--locked"], cwd=root, check=True)
        binary = root / "target/debug" / ("sheetpatch.exe" if os.name == "nt" else "sheetpatch")
    binary = binary.resolve()
    with tempfile.TemporaryDirectory(prefix="sheetpatch-interoperability-") as temp:
        directory = Path(temp).resolve()
        office = Office(program.resolve(), directory)
        try:
            original = directory / "original.xlsx"
            office.convert(root / "tests/fixtures/libreoffice-source.ods", original, "xlsx")
            edited = directory / "edited.xlsx"
            source = original
            for address, kind, value in (
                ("A1", "text", "=literal text"),
                ("B1", "number", "21"),
                ("C1", "bool", "false"),
                ("E1", "text", "Café ☕ _x0041_\nnext line"),
                ("F3", "text", "Inserted"),
            ):
                subprocess.run(
                    [str(binary), "set", str(source), str(edited), "Data", address, kind, value],
                    check=True,
                )
                source = edited
            compacted = directory / "compact.xlsx"
            subprocess.run([str(binary), "compact", str(edited), str(compacted)], check=True)
            for workbook in (edited, compacted):
                output = workbook.with_suffix(".ods")
                office.convert(workbook, output, "ods")
                check_ods(output)
            print("PASS: LibreOffice exported and reopened default and compacted XLSX files; values, formulas, and both sheets verified.")
        finally:
            office.close()


if __name__ == "__main__":
    main()
