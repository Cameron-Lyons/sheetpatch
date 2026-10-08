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
    renamed: bool,
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if self.renamed {
            return;
        }
        // Windows refuses to remove a readonly file, including a replacement
        // that inherited readonly permissions from its destination.
        #[cfg(windows)]
        {
            let metadata = self
                .file
                .as_ref()
                .map_or_else(|| fs::metadata(&self.path), File::metadata);
            if let Ok(metadata) = metadata {
                let mut permissions = metadata.permissions();
                if permissions.readonly() {
                    permissions.set_readonly(false);
                    if let Some(file) = &self.file {
                        let _ = file.set_permissions(permissions);
                    } else {
                        let _ = fs::set_permissions(&self.path, permissions);
                    }
                }
            }
        }
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
    path.file_name()
        .ok_or_else(|| Error::InvalidValue("destination must name a file".into()))?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut temp = loop {
        // Keep the temporary name independent of the destination's length.
        // A valid destination can already use the filesystem's full name limit.
        let name = OsString::from(format!(
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
                    renamed: false,
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    let file = temp.file.as_ref().expect("temporary file is open");
    // Restrict an existing private workbook's replacement before any contents
    // are written; applying permissions afterward would expose a temporary
    // copy under the process's default creation permissions.
    if let Ok(metadata) = fs::metadata(path) {
        file.set_permissions(metadata.permissions())?;
    }
    {
        let mut writer = BufWriter::new(file);
        serialize(&mut writer)?;
        writer.flush()?;
    }
    file.sync_all()?;
    temp.file.take();
    fs::rename(&temp.path, path)?;
    temp.renamed = true;
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

    #[cfg(unix)]
    #[test]
    fn restricts_private_replacements_before_serializing() {
        use std::os::unix::fs::PermissionsExt;

        let directory =
            std::env::temp_dir().join(format!("sheetpatch-private-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let destination = directory.join("book.xlsx");
        fs::write(&destination, b"private original").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).unwrap();
        write(&destination, |writer| {
            let temporary = fs::read_dir(&directory)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?
                .into_iter()
                .find(|path| path != &destination)
                .expect("replacement exists while serializing");
            assert_eq!(fs::metadata(temporary)?.permissions().mode() & 0o777, 0o600);
            writer.write_all(b"private replacement")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"private replacement");
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saves_destinations_with_long_filenames() {
        let directory =
            std::env::temp_dir().join(format!("sheetpatch-long-name-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let destination = directory.join(format!("{}.xlsx", "x".repeat(240)));
        fs::write(&destination, b"original").unwrap();
        write(&destination, |writer| {
            writer.write_all(b"replacement")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn failed_saves_remove_readonly_temporary_files() {
        let directory =
            std::env::temp_dir().join(format!("sheetpatch-readonly-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let destination = directory.join("book.xlsx");
        fs::write(&destination, b"original").unwrap();
        let mut permissions = fs::metadata(&destination).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&destination, permissions).unwrap();
        let result = write(&destination, |writer| {
            writer.write_all(b"partial replacement")?;
            Err(std::io::Error::other("intentional serialization failure").into())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"original");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        assert!(fs::metadata(&destination).unwrap().permissions().readonly());

        // Windows also rejects replacing a readonly destination at rename.
        let result = write(&destination, |writer| {
            writer.write_all(b"replacement")?;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"original");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        let mut permissions = fs::metadata(&destination).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&destination, permissions).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }
}
