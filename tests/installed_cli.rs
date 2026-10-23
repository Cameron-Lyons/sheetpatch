//! Smoke test for an installed executable, such as the one `cargo install`
//! builds from the extracted release package. The test is ignored by default;
//! `SHEETPATCH_INSTALLED_BINARY` names the executable to check:
//!
//! ```sh
//! SHEETPATCH_INSTALLED_BINARY=/path/to/sheetpatch cargo test --test installed_cli -- --ignored
//! ```
use std::env;
use std::process::{Command, Stdio};

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
