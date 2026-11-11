//! Smoke test for an installed executable, such as the one `cargo install`
//! builds from the extracted release package. The test is ignored by default;
//! `SHEETPATCH_INSTALLED_BINARY` names the executable to check:
//!
//! ```sh
//! SHEETPATCH_INSTALLED_BINARY=/path/to/sheetpatch cargo test --test installed_cli -- --ignored
//! ```
mod common;

use common::TempDir;
use std::process::{Command, Stdio};
use std::{env, fs};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/libreoffice.xlsx"
);

/// Standard output of the installed executable, which must succeed without
/// writing to standard error.
fn installed(args: &[&str], input: &str) -> String {
    use std::io::Write as _;

    let binary = env::var_os("SHEETPATCH_INSTALLED_BINARY")
        .expect("set SHEETPATCH_INSTALLED_BINARY to the executable to check");
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{args:?}: {:?}", output.stderr);
    assert!(output.stderr.is_empty(), "{args:?}: {:?}", output.stderr);
    String::from_utf8(output.stdout).unwrap()
}

#[test]
#[ignore = "needs SHEETPATCH_INSTALLED_BINARY"]
fn installed_executable_reads_validates_and_documents_patches() {
    assert_eq!(
        installed(&["--version"], ""),
        concat!("sheetpatch ", env!("CARGO_PKG_VERSION"), "\n")
    );
    assert_eq!(
        installed(&["list", "--json", FIXTURE], ""),
        concat!(
            "[{\"name\":\"Data\",\"path\":\"xl/worksheets/sheet1.xml\",\"visibility\":\"visible\"},",
            "{\"name\":\"Notes\",\"path\":\"xl/worksheets/sheet2.xml\",\"visibility\":\"visible\"}]\n"
        )
    );
    assert_eq!(
        installed(
            &["get", "--json", FIXTURE, "Data", "a1", "D1", "XFD1048576"],
            ""
        ),
        concat!(
            "[{\"sheet\":\"Data\",\"cell\":\"A1\",\"type\":\"text\",",
            "\"value\":\"Original text\",\"formula\":null,\"style_index\":1},",
            "{\"sheet\":\"Data\",\"cell\":\"D1\",\"type\":\"number\",",
            "\"value\":25,\"formula\":\"B1*2\",\"style_index\":0},",
            "{\"sheet\":\"Data\",\"cell\":\"XFD1048576\",\"type\":\"blank\",",
            "\"value\":null,\"formula\":null,\"style_index\":null}]\n"
        )
    );
    assert_eq!(
        installed(
            &["patch", "--check", FIXTURE, "-"],
            "Data\tA1\ttext\tValidated through installed CLI\n"
        ),
        "Validated 1 patch row.\n"
    );
    assert!(installed(&["help", "patch"], "").contains("--check"));
}

#[test]
#[ignore = "needs SHEETPATCH_INSTALLED_BINARY"]
fn installed_executable_streams_ranges_and_saves_patches() {
    assert_eq!(
        installed(&["cells", FIXTURE, "Data", "B1:D2"], ""),
        "B1\tnumber\t12.5\nC1\tnumber\t1\nD1\tnumber\t25\n"
    );
    let directory = TempDir::new();
    let output = directory.join("patched.xlsx");
    let output = output.to_str().unwrap();
    assert_eq!(
        installed(
            &["patch", FIXTURE, output, "-"],
            "Data\tA1\ttext\tCafé & <ready>\nData\tB2\tnumber\t42.5\nData\tE1\tblank\n"
        ),
        ""
    );
    assert_eq!(
        installed(&["cells", output, "Data", "A1:E2"], ""),
        concat!(
            "A1\ttext\tCafé & <ready>\nB1\tnumber\t12.5\n",
            "C1\tnumber\t1\nD1\tnumber\t25\nB2\tnumber\t42.5\n"
        )
    );
    assert_eq!(
        installed(&["get", "--json", output, "Data", "D1"], ""),
        installed(&["get", "--json", FIXTURE, "Data", "D1"], "")
    );

    // Independently check ZIP integrity and preservation in the installed
    // binary's output, including parts the CLI never reads back.
    let before = fs::read(FIXTURE).unwrap();
    let after = fs::read(output).unwrap();
    let original = common::zip_entries(&before);
    let saved = common::zip_entries(&after);
    assert_eq!(original.len(), saved.len());
    for (original, saved) in original.iter().zip(&saved) {
        assert_eq!(original.name, saved.name);
        saved.payload();
        if original.name != "xl/worksheets/sheet1.xml" {
            assert_eq!(original.local, saved.local, "{}", original.name);
        }
    }
}
