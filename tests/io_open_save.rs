//! File-system behavior of `Workbook::open`, `save`, and `save_compact`.
use sheetpatch::{Error, Workbook};
use std::{
    fs, io,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/libreoffice.xlsx"
);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sheetpatch-io-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn entries(&self) -> usize {
        fs::read_dir(&self.0).unwrap().count()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_invalid_input<T: std::fmt::Debug>(result: Result<T, Error>, message: &str) {
    match result {
        Err(Error::Io(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
            assert!(error.to_string().contains(message), "{error}");
        }
        other => panic!("expected an invalid-input I/O error, got {other:?}"),
    }
}

#[test]
fn destinations_without_a_file_name_are_reported_as_io_errors() {
    let workbook = Workbook::open(FIXTURE).unwrap();
    for destination in ["", ".", ".."] {
        assert_invalid_input(workbook.save(destination), "destination must name a file");
        assert_invalid_input(
            workbook.save_compact(destination),
            "destination must name a file",
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_sheetpatch"))
        .args(["set", FIXTURE, ".", "Data", "A1", "text", "x"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "sheetpatch: I/O error: destination must name a file\n"
    );
}

#[test]
fn directory_destinations_are_rejected_without_writing() {
    let directory = TempDir::new();
    let destination = directory.join("output.xlsx");
    fs::create_dir(&destination).unwrap();
    let mut workbook = Workbook::open(FIXTURE).unwrap();
    workbook.set_cell("Data", "A1", "new value").unwrap();
    assert_invalid_input(
        workbook.save(&destination),
        "destination is not a regular file",
    );
    assert_invalid_input(
        workbook.save_compact(&destination),
        "destination is not a regular file",
    );
    assert!(fs::metadata(&destination).unwrap().is_dir());
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    assert_eq!(directory.entries(), 1);
}

#[cfg(unix)]
#[test]
fn symlinked_device_destinations_are_rejected_without_writing() {
    use std::os::unix::fs::FileTypeExt;

    let directory = TempDir::new();
    let destination = directory.join("output.xlsx");
    std::os::unix::fs::symlink("/dev/null", &destination).unwrap();
    let workbook = Workbook::open(FIXTURE).unwrap();
    assert_invalid_input(
        workbook.save(&destination),
        "destination is not a regular file",
    );
    assert!(fs::symlink_metadata(&destination).unwrap().is_symlink());
    assert!(
        fs::metadata("/dev/null")
            .unwrap()
            .file_type()
            .is_char_device()
    );
    assert_eq!(directory.entries(), 1);
}

#[cfg(unix)]
#[test]
fn oversized_inputs_are_rejected_before_reading() {
    let directory = TempDir::new();
    let path = directory.join("huge.xlsx");
    // A sparse file one byte larger than any ZIP32 archive can be: rejected
    // from its metadata, without allocating or reading eight gibibytes.
    let limit = 2 * u64::from(u32::MAX) + 22 + u64::from(u16::MAX);
    fs::File::create(&path).unwrap().set_len(limit + 1).unwrap();
    match Workbook::open(&path) {
        Err(Error::Unsupported(message)) => assert!(message.contains("ZIP32"), "{message}"),
        other => panic!("expected an unsupported-input error, got {:?}", other.err()),
    }
}

#[cfg(unix)]
mod permissions {
    use super::*;
    use std::{
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        path::Path,
    };

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn umask(directory: &TempDir) -> u32 {
        let probe = directory.join("umask-probe");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o777)
            .open(&probe)
            .unwrap();
        let umask = !mode(&probe) & 0o777;
        fs::remove_file(probe).unwrap();
        umask
    }

    fn source(directory: &TempDir, permissions: u32) -> PathBuf {
        let path = directory.join(&format!("source-{permissions:o}.xlsx"));
        fs::copy(FIXTURE, &path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(permissions)).unwrap();
        path
    }

    #[test]
    fn new_destinations_are_no_more_permissive_than_the_source_file() {
        let directory = TempDir::new();
        let umask = umask(&directory);
        for permissions in [0o600, 0o640, 0o644] {
            let mut workbook = Workbook::open(source(&directory, permissions)).unwrap();
            workbook.set_cell("Data", "A1", "edited").unwrap();
            let saved = directory.join(&format!("saved-{permissions:o}.xlsx"));
            workbook.save(&saved).unwrap();
            assert_eq!(mode(&saved), permissions & !umask);
            let compact = directory.join(&format!("compact-{permissions:o}.xlsx"));
            workbook.save_compact(&compact).unwrap();
            assert_eq!(mode(&compact), permissions & !umask);
            let reopened = Workbook::open(&saved).unwrap();
            assert_eq!(
                reopened.get_cell("Data", "A1").unwrap().value,
                "edited".into()
            );
        }
    }

    #[test]
    fn cli_outputs_of_private_inputs_stay_private() {
        let directory = TempDir::new();
        let umask = umask(&directory);
        let input = source(&directory, 0o600);
        for (command, output) in [("set", "set.xlsx"), ("compact", "compact.xlsx")] {
            let output = directory.join(output);
            let mut cli = Command::new(env!("CARGO_BIN_EXE_sheetpatch"));
            cli.arg(command).arg(&input).arg(&output);
            if command == "set" {
                cli.args(["Data", "A1", "number", "1"]);
            }
            let result = cli.output().unwrap();
            assert!(result.status.success(), "{result:?}");
            assert_eq!(mode(&output), 0o600 & !umask, "{command}");
        }
    }

    #[test]
    fn in_memory_workbooks_create_destinations_with_default_permissions() {
        let directory = TempDir::new();
        let umask = umask(&directory);
        let workbook = Workbook::from_bytes(fs::read(FIXTURE).unwrap()).unwrap();
        let saved = directory.join("saved.xlsx");
        workbook.save(&saved).unwrap();
        assert_eq!(mode(&saved), 0o666 & !umask);
    }

    #[test]
    fn existing_destinations_keep_their_own_permissions() {
        let directory = TempDir::new();
        let workbook = Workbook::open(source(&directory, 0o600)).unwrap();
        let destination = directory.join("existing.xlsx");
        fs::copy(FIXTURE, &destination).unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o644)).unwrap();
        workbook.save(&destination).unwrap();
        assert_eq!(mode(&destination), 0o644);

        // An in-place save keeps the source's private mode.
        let input = source(&directory, 0o640);
        Workbook::open(&input).unwrap().save(&input).unwrap();
        assert_eq!(mode(&input), 0o640);
    }
}
