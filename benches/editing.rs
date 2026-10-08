//! Reproducible benchmarks without a benchmarking dependency.
//! Run `SHEETPATCH_BENCH_SAMPLES=3 cargo bench --bench editing`.
//! Samples include workbook loading and edits, but exclude fixture construction.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    hint::black_box,
    io::{self, Read, Write},
    time::{Duration, Instant},
};

use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};
use sheetpatch::{CellValue, Workbook};

const MAIN_NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const SHEET_PATH: &str = "xl/worksheets/sheet1.xml";
const CASES: &[(usize, usize)] = &[(10_000, 100), (50_000, 500)];

fn updates(cells: usize, count: usize) -> Vec<(String, CellValue)> {
    (0..count)
        .map(|index| {
            (
                format!("A{}", 1 + index * cells / count),
                CellValue::Number(-((index + 1) as f64)),
            )
        })
        .collect()
}

fn worksheet(cells: usize, values: &BTreeMap<usize, i32>) -> Vec<u8> {
    let mut xml = format!(
        "<worksheet xmlns=\"{MAIN_NS}\" xmlns:v=\"urn:vendor\"><dimension ref=\"A1:A{cells}\"/><sheetData>"
    );
    for row in 1..=cells {
        let value = values.get(&row).copied().unwrap_or(row as i32);
        write!(
            xml,
            "<row r=\"{row}\"><c r=\"A{row}\" s=\"0\" v:tag=\"keep\"><v>{value}</v></c></row>"
        )
        .unwrap();
    }
    xml.push_str("</sheetData><v:opaque flag='retained'>future XML</v:opaque></worksheet>");
    xml.into_bytes()
}

fn fixture(cells: usize) -> Vec<u8> {
    let sheet = worksheet(cells, &BTreeMap::new());
    let workbook = format!(
        "<workbook xmlns=\"{MAIN_NS}\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet name=\"Data\" sheetId=\"1\" r:id=\"r1\"/></sheets></workbook>"
    );
    // An incompressible opaque part makes serialization exercise retained data,
    // instead of benchmarking only an unusually small, compressible worksheet.
    let mut payload = Vec::with_capacity(1 << 20);
    let mut state = 0x1234_5678u32;
    for _ in 0..(1 << 20) {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        payload.push(state as u8);
    }
    zip(&[
        ("[Content_Types].xml", b"<Types xmlns='http://schemas.openxmlformats.org/package/2006/content-types'><Default Extension='rels' ContentType='application/vnd.openxmlformats-package.relationships+xml'/><Default Extension='xml' ContentType='application/xml'/><Default Extension='bin' ContentType='application/octet-stream'/><Override PartName='/xl/workbook.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml'/><Override PartName='/xl/worksheets/sheet1.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml'/><Override PartName='/xl/styles.xml' ContentType='application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml'/></Types>"),
        ("_rels/.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='root' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument' Target='xl/workbook.xml'/></Relationships>"),
        ("xl/workbook.xml", workbook.as_bytes()),
        ("xl/_rels/workbook.xml.rels", b"<Relationships xmlns='http://schemas.openxmlformats.org/package/2006/relationships'><Relationship Id='r1' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet' Target='worksheets/sheet1.xml'/><Relationship Id='s1' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles' Target='styles.xml'/></Relationships>"),
        (SHEET_PATH, &sheet),
        ("xl/styles.xml", b"<styleSheet xmlns='http://schemas.openxmlformats.org/spreadsheetml/2006/main'><fonts count='1'><font/></fonts><fills count='2'><fill><patternFill patternType='none'/></fill><fill><patternFill patternType='gray125'/></fill></fills><borders count='1'><border/></borders><cellStyleXfs count='1'><xf/></cellStyleXfs><cellXfs count='1'><xf xfId='0'/></cellXfs></styleSheet>"),
        ("customXml/item1.xml", b"<opaque xmlns='urn:vendor' flag='keep'>unfamiliar XML</opaque>"),
        ("vendor/opaque.bin", &payload),
    ])
}

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut central = Vec::new();
    for &(name, bytes) in entries {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
        encoder.write_all(bytes).unwrap();
        let compressed = encoder.finish().unwrap();
        let crc = crc32(bytes);
        let offset = output.len() as u32;
        put32(&mut output, 0x0403_4b50);
        for value in [20, 0, 8, 0, 0] {
            put16(&mut output, value);
        }
        for value in [crc, compressed.len() as u32, bytes.len() as u32] {
            put32(&mut output, value);
        }
        put16(&mut output, name.len() as u16);
        put16(&mut output, 0);
        output.extend_from_slice(name.as_bytes());
        output.extend_from_slice(&compressed);
        put32(&mut central, 0x0201_4b50);
        for value in [20, 20, 0, 8, 0, 0] {
            put16(&mut central, value);
        }
        for value in [crc, compressed.len() as u32, bytes.len() as u32] {
            put32(&mut central, value);
        }
        put16(&mut central, name.len() as u16);
        for _ in 0..4 {
            put16(&mut central, 0);
        }
        put32(&mut central, 0);
        put32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let offset = output.len() as u32;
    output.extend_from_slice(&central);
    put32(&mut output, 0x0605_4b50);
    put16(&mut output, 0);
    put16(&mut output, 0);
    put16(&mut output, entries.len() as u16);
    put16(&mut output, entries.len() as u16);
    put32(&mut output, central.len() as u32);
    put32(&mut output, offset);
    put16(&mut output, 0);
    output
}

fn put16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn read16(bytes: &[u8], at: usize) -> usize {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize
}

fn read32(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

struct Entry<'a> {
    compressed: &'a [u8],
    central_prefix: &'a [u8],
    central_suffix: &'a [u8],
}

fn entries(bytes: &[u8]) -> BTreeMap<String, Entry<'_>> {
    let end = bytes.len() - 22;
    assert_eq!(&bytes[end..end + 4], b"PK\x05\x06");
    let mut offset = read32(bytes, end + 16);
    let mut entries = BTreeMap::new();
    for _ in 0..read16(bytes, end + 10) {
        assert_eq!(&bytes[offset..offset + 4], b"PK\x01\x02");
        let local = read32(bytes, offset + 42);
        let start = local + 30 + read16(bytes, local + 26) + read16(bytes, local + 28);
        let size = read32(bytes, offset + 20);
        let name_size = read16(bytes, offset + 28);
        let central_size = 46 + name_size + read16(bytes, offset + 30) + read16(bytes, offset + 32);
        let name = String::from_utf8(bytes[offset + 46..offset + 46 + name_size].to_vec()).unwrap();
        entries.insert(
            name,
            Entry {
                compressed: &bytes[start..start + size],
                central_prefix: &bytes[offset..offset + 42],
                central_suffix: &bytes[offset + 46..offset + central_size],
            },
        );
        offset += central_size;
    }
    entries
}

fn verify_output(input: &[u8], output: &[u8], expected: &[u8]) {
    let originals = entries(input);
    let replacements = entries(output);
    assert_eq!(originals.len(), replacements.len());
    let mut decoded = Vec::new();
    DeflateDecoder::new(replacements[SHEET_PATH].compressed)
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, expected, "effective worksheet XML must match");
    for (name, original) in originals {
        if name != SHEET_PATH {
            let replacement = &replacements[&name];
            assert_eq!(original.compressed, replacement.compressed, "{name}");
            assert_eq!(
                original.central_prefix, replacement.central_prefix,
                "{name}"
            );
            assert_eq!(
                original.central_suffix, replacement.central_suffix,
                "{name}"
            );
        }
    }
}

fn expected(cells: usize, count: usize) -> Vec<u8> {
    worksheet(
        cells,
        &(0..count)
            .map(|index| (1 + index * cells / count, -((index + 1) as i32)))
            .collect(),
    )
}

fn sequential(input: &[u8], edits: &[(String, CellValue)]) -> Workbook {
    let mut book = Workbook::from_bytes(input.to_vec()).unwrap();
    for (address, value) in edits {
        book.set_cell("Data", address, value.clone()).unwrap();
    }
    book
}

fn measure<T>(samples: usize, mut operation: impl FnMut() -> T) -> Duration {
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        let result = black_box(operation());
        times.push(started.elapsed());
        drop(result);
    }
    times.sort_unstable();
    times[times.len() / 2]
}

fn samples() -> usize {
    std::env::var("SHEETPATCH_BENCH_SAMPLES")
        .map(|value| {
            value
                .parse::<usize>()
                .ok()
                .filter(|&samples| samples > 0)
                .expect("SHEETPATCH_BENCH_SAMPLES must be a positive integer")
        })
        .unwrap_or(3)
}

fn report(label: &str, cells: usize, count: usize, samples: usize, elapsed: Duration) {
    println!("{label},{cells},{count},{samples},{}", elapsed.as_micros());
}

// New APIs begin here; the helpers above also run against the original baseline.
fn bulk(input: &[u8], edits: &[(String, CellValue)]) -> Workbook {
    let mut book = Workbook::from_bytes(input.to_vec()).unwrap();
    book.set_cells(
        "Data",
        edits
            .iter()
            .map(|(address, value)| (address.as_str(), value.clone())),
    )
    .unwrap();
    book
}

fn main() {
    if cfg!(debug_assertions) {
        println!("Run cargo bench --bench editing for release-profile measurements.");
        return;
    }
    let samples = samples();
    println!("operation,cells,edits,samples,median_us");
    for &(cells, count) in CASES {
        let input = fixture(cells);
        let edits = updates(cells, count);
        let expected = expected(cells, count);
        let sequential = sequential(&input, &edits);
        let batched = bulk(&input, &edits);
        let output = batched.to_bytes().unwrap();
        verify_output(&input, &sequential.to_bytes().unwrap(), &expected);
        verify_output(&input, &output, &expected);
        let mut streamed = Vec::new();
        batched.write_to(&mut streamed).unwrap();
        assert_eq!(streamed, output);
        let compact = batched.to_bytes_compact().unwrap();
        verify_output(&input, &compact, &expected);

        report(
            "sequential",
            cells,
            count,
            samples,
            measure(samples, || self::sequential(&input, &edits)),
        );
        report(
            "bulk",
            cells,
            count,
            samples,
            measure(samples, || bulk(&input, &edits)),
        );
        report(
            "to_bytes",
            cells,
            count,
            samples,
            measure(samples, || batched.to_bytes().unwrap()),
        );
        report(
            "write_to_sink",
            cells,
            count,
            samples,
            measure(samples, || batched.write_to(io::sink()).unwrap()),
        );
        report(
            "compact_to_bytes",
            cells,
            count,
            samples,
            measure(samples, || batched.to_bytes_compact().unwrap()),
        );
        report(
            "compact_to_sink",
            cells,
            count,
            samples,
            measure(samples, || batched.write_compact_to(io::sink()).unwrap()),
        );
        println!(
            "sizes,{cells},{count},input={},edited={},compact={}",
            input.len(),
            output.len(),
            compact.len()
        );

        let mut repeated = input.clone();
        let mut values = BTreeMap::new();
        for value in 1..=5 {
            let mut book = Workbook::from_bytes(repeated).unwrap();
            book.set_cell("Data", "A1", -(value as f64)).unwrap();
            repeated = book.to_bytes().unwrap();
            values.insert(1, -value);
        }
        let compacted = Workbook::from_bytes(repeated.clone())
            .unwrap()
            .to_bytes_compact()
            .unwrap();
        verify_output(&input, &compacted, &worksheet(cells, &values));
        assert_eq!(compacted, repeated);
        println!(
            "growth,{cells},saves=5,normal={},compact={}",
            repeated.len(),
            compacted.len()
        );
    }
}
