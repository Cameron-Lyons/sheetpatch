//! Write a complete replacement beside the destination before renaming it.
use crate::{Error, Result};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

struct Temporary {
    path: PathBuf,
    file: Option<File>,
}

impl Drop for Temporary {
    fn drop(&mut self) {
        // Windows cannot unlink an open file. Close it before cleanup on every
        // error path, including errors from serialization and flushing.
        self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) fn write(
    path: &Path,
    serialize: impl FnOnce(&mut BufWriter<&File>) -> Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let filename = path
        .file_name()
        .ok_or_else(|| Error::InvalidValue("destination must name a file".into()))?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut temp = loop {
        let mut name = OsString::from(".");
        name.push(filename);
        name.push(format!(
            ".sheetpatch-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let candidate = parent.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                break Temporary {
                    path: candidate,
                    file: Some(file),
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    let file = temp.file.as_ref().expect("temporary file is open");
    {
        let mut writer = BufWriter::new(file);
        serialize(&mut writer)?;
        writer.flush()?;
    }
    if let Ok(metadata) = fs::metadata(path) {
        file.set_permissions(metadata.permissions())?;
    }
    file.sync_all()?;
    temp.file.take();
    fs::rename(&temp.path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialization_failure_keeps_destination_and_closes_temp_before_cleanup() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "sheetpatch-atomic-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let destination = directory.join("book.xlsx");
        fs::write(&destination, b"original").unwrap();
        let result = write(&destination, |writer| {
            writer.write_all(b"partial replacement")?;
            Err(std::io::Error::other("intentional serialization failure").into())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"original");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
