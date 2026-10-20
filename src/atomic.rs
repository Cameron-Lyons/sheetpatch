//! Write a complete replacement beside the destination before renaming it.
use crate::Result;
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
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
        // Windows-only: this clears the readonly attribute, not Unix world-write bits.
        #[allow(clippy::permissions_set_readonly_false)]
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

impl Temporary {
    fn file(&self) -> &File {
        self.file.as_ref().expect("temporary file is open")
    }
}

/// What a new save destination inherits from the regular file a workbook was
/// read from: Unix mode bits and group, without its owner or ACL.
#[derive(Clone, Copy)]
pub(crate) struct SourcePermissions {
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    gid: u32,
}

impl SourcePermissions {
    pub(crate) fn new(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            Self {
                mode: metadata.mode(),
                gid: metadata.gid(),
            }
        }
        // Other platforms create new files with default permissions.
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Self {}
        }
    }
}

/// Create a new, exclusively named file in `parent`. On Unix `mode` supplies
/// the creation mode's permission bits. The OS applies the parent directory's
/// default ACL or, without one, the process umask.
fn create_temporary(parent: &Path, mode: Option<u32>) -> Result<Temporary> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        // Keep the temporary name independent of the destination's length.
        // A valid destination can already use the filesystem's full name limit.
        let name = OsString::from(format!(
            ".sheetpatch-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let candidate = parent.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt;

            // Set the mode at creation: chmod afterward cannot revoke access
            // from a process that already opened the temporary file.
            options.mode(mode & 0o777);
        }
        #[cfg(not(unix))]
        let _ = mode;
        match options.open(&candidate) {
            Ok(file) => {
                return Ok(Temporary {
                    path: candidate,
                    file: Some(file),
                    renamed: false,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

/// The destination's resolved metadata when it already exists.
///
/// Symlinks are followed, so a link to a regular file is accepted and later
/// replaced by the new file. Anything else that already exists (a directory,
/// FIFO, device, or socket) cannot be replaced by renaming a workbook over
/// it, so it is rejected before any output is generated.
fn existing_destination(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("destination is not a regular file: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The permission bits applied to an existing destination's replacement.
///
/// The replacement is a new file, so its group is the one new files receive in
/// that directory unless it can be changed to the destination's group. Group
/// permission bits are kept only for that group. Other bits are restricted by
/// the old group bits because its members now fall into that class. These mode
/// checks do not preserve ACLs or prevent inherited ACL access. Setuid, setgid
/// and sticky bits are never carried over: writing the contents would clear
/// setuid and setgid anyway.
#[cfg(unix)]
fn replacement_mode(destination_mode: u32, same_group: bool) -> u32 {
    let mode = destination_mode & 0o777;
    if same_group {
        mode
    } else {
        (mode & 0o700) | (mode & (mode >> 3) & 0o007)
    }
}

/// Give `file` the group `gid` when the process may, and report whether it
/// has that group. The result is checked, because some filesystems accept a
/// group change without applying it.
#[cfg(unix)]
fn assign_group(file: &File, gid: u32) -> io::Result<bool> {
    use std::os::unix::fs::{MetadataExt, fchown};

    Ok(file.metadata()?.gid() == gid
        || (fchown(file, None, Some(gid)).is_ok() && file.metadata()?.gid() == gid))
}

/// Create an existing destination's replacement with the destination's group
/// and mode bits, before any contents are written.
#[cfg(unix)]
fn create_replacement(parent: &Path, destination: &fs::Metadata) -> Result<Temporary> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // Owner-only until the replacement has the destination's group, so no
    // group can open it yet.
    let temp = create_temporary(parent, Some(destination.mode() & 0o700))?;
    let same_group = assign_group(temp.file(), destination.gid())?;
    temp.file()
        .set_permissions(fs::Permissions::from_mode(replacement_mode(
            destination.mode(),
            same_group,
        )))?;
    Ok(temp)
}

#[cfg(not(unix))]
fn create_replacement(parent: &Path, destination: &fs::Metadata) -> Result<Temporary> {
    let temp = create_temporary(parent, None)?;
    temp.file().set_permissions(destination.permissions())?;
    Ok(temp)
}

/// Create a new destination's temporary file.
///
/// On Unix, the source's mode bits supply the creation mode, subject to the
/// OS's default ACL or umask rules, without setuid, setgid or sticky bits. Like
/// a replacement, it keeps group mode bits only when it has the source's group,
/// assigning that group when the process may.
#[cfg(unix)]
fn create_new(parent: &Path, source: Option<SourcePermissions>) -> Result<Temporary> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Some(source) = source else {
        return create_temporary(parent, None);
    };
    let temp = create_temporary(parent, Some(source.mode & 0o777))?;
    let created = temp.file().metadata()?;
    if created.gid() == source.gid {
        return Ok(temp);
    }
    // The file was created in another group, such as the group of a setgid
    // directory. Group or other permissions may already have let someone
    // open it who cannot access the source. Discard it unwritten and start
    // owner-only before assigning the group. Its mode reflects the OS's
    // default ACL or umask restrictions.
    let mode = created.mode() & 0o777;
    let temp = if mode & 0o077 == 0 {
        temp
    } else {
        drop(temp);
        create_temporary(parent, Some(mode & 0o700))?
    };
    let same_group = assign_group(temp.file(), source.gid)?;
    temp.file()
        .set_permissions(fs::Permissions::from_mode(replacement_mode(
            mode, same_group,
        )))?;
    Ok(temp)
}

#[cfg(not(unix))]
fn create_new(parent: &Path, _: Option<SourcePermissions>) -> Result<Temporary> {
    create_temporary(parent, None)
}

/// Make a completed rename durable.
///
/// On Unix a rename reaches the disk only when its directory does; syncing
/// the replacement file alone leaves the new directory entry to the next
/// filesystem commit. The destination has already been replaced when this
/// runs, so a failure cannot be reported as a failed save. Errors are ignored:
/// some filesystems cannot open or sync a directory.
#[cfg(unix)]
fn sync_directory(directory: &Path) {
    if let Ok(directory) = File::open(directory) {
        let _ = directory.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_directory(_: &Path) {}

/// Replace `path` with the output of `serialize`, or leave it untouched.
///
/// An existing destination's mode bits are applied to its replacement
/// before contents are written; on Unix its group is kept too when the process
/// may assign it, and otherwise group bits are cleared and other bits are
/// limited to those shared by the original group and others. On Unix a new
/// destination uses the `source` file's mode bits, under the OS's default ACL
/// or umask rules and the same group rule, or the default mode when it is
/// `None`. ACLs are not preserved; inherited ACLs can grant additional access.
/// Setuid, setgid and sticky bits are never applied.
pub(crate) fn write(
    path: &Path,
    source: Option<SourcePermissions>,
    serialize: impl FnOnce(&mut BufWriter<&File>) -> Result<()>,
) -> Result<()> {
    if path.file_name().is_none() {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "destination must name a file").into(),
        );
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // Restrict an existing private workbook's replacement before any contents
    // are written; applying permissions afterward would expose a temporary
    // copy under the process's default creation permissions.
    let mut temp = match existing_destination(path)? {
        Some(existing) => create_replacement(parent, &existing)?,
        None => create_new(parent, source)?,
    };
    let file = temp.file.as_ref().expect("temporary file is open");
    {
        let mut writer = BufWriter::new(file);
        serialize(&mut writer)?;
        writer.flush()?;
    }
    file.sync_all()?;
    temp.file.take();
    fs::rename(&temp.path, path)?;
    temp.renamed = true;
    sync_directory(parent);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    /// A uniquely named directory that is removed even when a test fails.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "sheetpatch-atomic-{label}-{}-{}",
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

    fn replace(destination: &Path, contents: &[u8]) -> Result<()> {
        write(destination, None, |writer| {
            writer.write_all(contents)?;
            Ok(())
        })
    }

    /// Assert that `result` is an invalid-input I/O error mentioning `message`.
    fn assert_invalid_input(result: Result<()>, message: &str) {
        match result {
            Err(Error::Io(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
                assert!(error.to_string().contains(message), "{error}");
            }
            other => panic!("expected an invalid-input I/O error, got {other:?}"),
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// The process umask, observed from a file created with mode 0o777.
    #[cfg(unix)]
    fn umask(directory: &TempDir) -> u32 {
        use std::os::unix::fs::OpenOptionsExt;

        let probe = directory.join("umask-probe");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o777)
            .open(&probe)
            .unwrap();
        let umask = !mode(&probe) & 0o777;
        fs::remove_file(probe).unwrap();
        umask
    }

    /// The group a new file in `directory` receives.
    #[cfg(unix)]
    fn default_gid(directory: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt;

        let probe = directory.join("group-probe");
        fs::write(&probe, b"").unwrap();
        let gid = fs::metadata(&probe).unwrap().gid();
        fs::remove_file(probe).unwrap();
        gid
    }

    /// The process's groups other than `default`, which it may assign.
    #[cfg(unix)]
    fn other_groups(default: u32) -> Vec<u32> {
        let output = std::process::Command::new("id").arg("-G").output();
        output
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .into_iter()
            .flat_map(|groups| {
                groups
                    .split_whitespace()
                    .filter_map(|gid| gid.parse::<u32>().ok())
                    .collect::<Vec<_>>()
            })
            .filter(|&gid| gid != default)
            .collect()
    }

    /// A group the process may not give its files, unless it is privileged.
    #[cfg(unix)]
    fn foreign_gid(directory: &Path) -> Option<u32> {
        let probe = directory.join("foreign-probe");
        fs::write(&probe, b"").unwrap();
        let default = default_gid(directory);
        let groups = other_groups(default);
        let foreign = [0, 65534, 54321].into_iter().find(|&gid| {
            gid != default
                && !groups.contains(&gid)
                && std::os::unix::fs::chown(&probe, None, Some(gid)).is_err()
        });
        fs::remove_file(probe).unwrap();
        foreign
    }

    /// Save a new `destination` created from `source`, and return the
    /// temporary file's mode while serializing.
    #[cfg(unix)]
    fn create(destination: &Path, source: SourcePermissions) -> u32 {
        let directory = destination.parent().unwrap();
        let mut temporary_mode = None;
        write(destination, Some(source), |writer| {
            // A temporary file discarded for its group is already removed.
            let entries = fs::read_dir(directory)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<_>>>()?;
            let [temporary] = entries
                .iter()
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .ends_with(".tmp")
                })
                .collect::<Vec<_>>()[..]
            else {
                panic!("expected one temporary file: {entries:?}");
            };
            temporary_mode = Some(mode(temporary));
            writer.write_all(b"new")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"new");
        temporary_mode.unwrap()
    }

    #[test]
    fn serialization_failure_keeps_destination_and_closes_temp_before_cleanup() {
        let directory = TempDir::new("serialization");
        let destination = directory.join("book.xlsx");
        fs::write(&destination, b"original").unwrap();
        let result = write(&destination, None, |writer| {
            writer.write_all(b"partial replacement")?;
            Err(io::Error::other("intentional serialization failure").into())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"original");
        assert_eq!(directory.entries(), 1);
    }

    #[test]
    fn failed_rename_keeps_destination_and_removes_temporary_file() {
        let directory = TempDir::new("rename");
        let destination = directory.join("book.xlsx");
        fs::write(&destination, b"original").unwrap();
        // Replace the regular destination with a non-empty directory after the
        // checks, so only the final rename can fail.
        let result = write(&destination, None, |writer| {
            fs::remove_file(&destination)?;
            fs::create_dir(&destination)?;
            fs::write(destination.join("kept.txt"), b"kept")?;
            writer.write_all(b"replacement")?;
            Ok(())
        });
        assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
        assert_eq!(fs::read(destination.join("kept.txt")).unwrap(), b"kept");
        assert_eq!(directory.entries(), 1);
    }

    #[test]
    fn destinations_without_a_file_name_are_invalid_input() {
        for destination in ["", ".", "..", "directory/.."] {
            let result = write(Path::new(destination), None, |_| {
                panic!("{destination:?} must be rejected before serializing")
            });
            assert_invalid_input(result, "destination must name a file");
        }
        let error = write(Path::new("."), None, |_| Ok(())).unwrap_err();
        assert_eq!(error.to_string(), "I/O error: destination must name a file");
    }

    #[test]
    fn directory_destinations_are_rejected_before_serializing() {
        let directory = TempDir::new("directory");
        let destination = directory.join("book.xlsx");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("kept.txt"), b"kept").unwrap();
        let result = write(&destination, None, |_| {
            panic!("a directory destination must be rejected before serializing")
        });
        assert_invalid_input(result, "destination is not a regular file");
        assert_eq!(fs::read(destination.join("kept.txt")).unwrap(), b"kept");
        assert_eq!(directory.entries(), 1);

        // A trailing separator or `.` component still names the directory.
        let dotted = destination.join(".");
        let result = write(&dotted, None, |_| panic!("rejected before serializing"));
        assert_invalid_input(result, "destination is not a regular file");
        assert_eq!(directory.entries(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_and_socket_destinations_are_rejected_without_being_replaced() {
        use std::os::unix::fs::FileTypeExt;

        let directory = TempDir::new("special");
        let fifo = directory.join("fifo.xlsx");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo is available");
        assert!(status.success());
        // Opening the FIFO would block without a reader; it must not be opened.
        let result = write(&fifo, None, |_| panic!("rejected before serializing"));
        assert_invalid_input(result, "destination is not a regular file");
        assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());

        let socket = directory.join("socket.xlsx");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let result = write(&socket, None, |_| panic!("rejected before serializing"));
        assert_invalid_input(result, "destination is not a regular file");
        assert!(
            fs::symlink_metadata(&socket)
                .unwrap()
                .file_type()
                .is_socket()
        );
        assert_eq!(directory.entries(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_to_devices_are_rejected_and_symlinks_to_files_are_replaced() {
        use std::os::unix::fs::{FileTypeExt, symlink};

        let directory = TempDir::new("symlink");
        let device = directory.join("null.xlsx");
        symlink("/dev/null", &device).unwrap();
        let result = write(&device, None, |_| panic!("rejected before serializing"));
        assert_invalid_input(result, "destination is not a regular file");
        assert!(fs::symlink_metadata(&device).unwrap().is_symlink());
        assert!(fs::metadata(&device).unwrap().file_type().is_char_device());

        // A link to a regular file is replaced by the new file; the original
        // target is left unchanged.
        let target = directory.join("target.xlsx");
        fs::write(&target, b"original").unwrap();
        let link = directory.join("link.xlsx");
        symlink(&target, &link).unwrap();
        replace(&link, b"replacement").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_file());
        assert_eq!(fs::read(&link).unwrap(), b"replacement");
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(directory.entries(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_temporary_files_with_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new("created-private");
        let temporary = create_temporary(&directory.0, Some(0o600)).unwrap();
        // Check immediately after opening, before write() restores the mode.
        // No group or other reader can open an existing private replacement.
        assert_eq!(
            temporary
                .file
                .as_ref()
                .unwrap()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
        drop(temporary);
        assert_eq!(directory.entries(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn restricts_private_replacements_before_serializing() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = TempDir::new("private");
        let destination = directory.join("book.xlsx");
        fs::write(&destination, b"private original").unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).unwrap();
        let gid = fs::metadata(&destination).unwrap().gid();
        write(&destination, None, |writer| {
            let temporary = fs::read_dir(&directory.0)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<_>>>()?
                .into_iter()
                .find(|path| path != &destination)
                .expect("replacement exists while serializing");
            assert_eq!(mode(&temporary), 0o600);
            writer.write_all(b"private replacement")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"private replacement");
        assert_eq!(mode(&destination), 0o600);
        assert_eq!(fs::metadata(&destination).unwrap().gid(), gid);
        assert_eq!(directory.entries(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn existing_destinations_keep_their_mode_and_group() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = TempDir::new("group");
        let destination = directory.join("book.xlsx");
        fs::write(&destination, b"original").unwrap();
        let default_gid = fs::metadata(&destination).unwrap().gid();
        // Like `sed -i`, a rename-based save also replaces a read-only (0o400)
        // destination on Unix; Windows refuses (see the Windows-only test).
        for permissions in [0o640, 0o604, 0o644, 0o400] {
            fs::set_permissions(&destination, fs::Permissions::from_mode(permissions)).unwrap();
            // A private source must not override an existing file's mode.
            let source = SourcePermissions {
                mode: 0o600,
                gid: default_gid,
            };
            write(&destination, Some(source), |writer| {
                writer.write_all(b"replacement")?;
                Ok(())
            })
            .unwrap();
            assert_eq!(mode(&destination), permissions);
            assert_eq!(fs::metadata(&destination).unwrap().gid(), default_gid);
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).unwrap();
        }

        // When the user belongs to another group, a destination in that group
        // keeps it, along with its group permission bits.
        let output = std::process::Command::new("id").arg("-G").output();
        let other_group = output
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .into_iter()
            .flat_map(|groups| {
                groups
                    .split_whitespace()
                    .filter_map(|gid| gid.parse::<u32>().ok())
                    .collect::<Vec<_>>()
            })
            .find(|&gid| {
                gid != default_gid
                    && std::os::unix::fs::chown(&destination, None, Some(gid)).is_ok()
            });
        if let Some(gid) = other_group {
            assert_eq!(fs::metadata(&destination).unwrap().gid(), gid);
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o640)).unwrap();
            replace(&destination, b"group replacement").unwrap();
            assert_eq!(fs::read(&destination).unwrap(), b"group replacement");
            assert_eq!(fs::metadata(&destination).unwrap().gid(), gid);
            assert_eq!(mode(&destination), 0o640);
        }
        assert_eq!(directory.entries(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn group_permissions_are_withheld_from_a_different_group() {
        assert_eq!(replacement_mode(0o640, true), 0o640);
        assert_eq!(replacement_mode(0o640, false), 0o600);
        assert_eq!(replacement_mode(0o664, false), 0o604);
        assert_eq!(replacement_mode(0o2775, true), 0o775);
        assert_eq!(replacement_mode(0o2775, false), 0o705);
        assert_eq!(replacement_mode(0o4755, false), 0o705);
        assert_eq!(replacement_mode(0o7777, true), 0o777);
        assert_eq!(replacement_mode(0o100_644, true), 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn changing_groups_does_not_grant_other_access_to_the_original_group() {
        // Unix checks group permissions before other permissions. Members of
        // the old group denied read access by 0604 must not gain it when they
        // become "other" after the replacement gets a different group.
        for (original, expected) in [(0o604, 0o600), (0o646, 0o604), (0o624, 0o600)] {
            assert_eq!(replacement_mode(original, false), expected);
            assert_eq!(replacement_mode(original, true), original);
        }

        let directory = TempDir::new("other-access");
        let Some(foreign) = foreign_gid(&directory.0) else {
            return;
        };
        let umask = umask(&directory);
        let destination = directory.join("private-to-group.xlsx");
        let source = SourcePermissions {
            mode: 0o604,
            gid: foreign,
        };
        // Check both the temporary during serialization and the saved file.
        assert_eq!(create(&destination, source), 0o600 & !umask);
        assert_eq!(mode(&destination), 0o600 & !umask);
        assert_eq!(directory.entries(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn replacements_drop_setuid_setgid_and_sticky_bits() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new("special-bits");
        let destination = directory.join("book.xlsx");
        fs::write(&destination, b"original").unwrap();
        // Writing the contents would clear setuid and setgid anyway; the
        // replacement deliberately has none of these bits.
        for permissions in [0o4755, 0o2755, 0o1755, 0o6750, 0o7777] {
            // Some systems refuse a sticky bit on a regular file.
            if fs::set_permissions(&destination, fs::Permissions::from_mode(permissions)).is_err()
                || mode(&destination) != permissions
            {
                continue;
            }
            replace(&destination, b"replacement").unwrap();
            assert_eq!(fs::read(&destination).unwrap(), b"replacement");
            assert_eq!(mode(&destination), permissions & 0o777, "{permissions:o}");
        }
        assert_eq!(directory.entries(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn new_destinations_use_the_source_mode_restricted_by_the_umask() {
        let directory = TempDir::new("create");
        let umask = umask(&directory);
        let gid = default_gid(&directory.0);
        for (name, permissions, expected) in [
            ("private.xlsx", Some(0o600), 0o600),
            ("shared.xlsx", Some(0o664), 0o664),
            ("executable.xlsx", Some(0o4777), 0o777),
            ("setgid.xlsx", Some(0o2750), 0o750),
            ("sticky.xlsx", Some(0o1644), 0o644),
            ("default.xlsx", None, 0o666),
        ] {
            let destination = directory.join(name);
            let source = permissions.map(|mode| SourcePermissions { mode, gid });
            write(&destination, source, |writer| {
                writer.write_all(b"new")?;
                Ok(())
            })
            .unwrap();
            assert_eq!(fs::read(&destination).unwrap(), b"new");
            assert_eq!(mode(&destination), expected & !umask, "{name}");
        }
        assert_eq!(directory.entries(), 6);
    }

    #[cfg(unix)]
    #[test]
    fn new_destinations_keep_group_bits_only_in_the_source_group() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = TempDir::new("new-group");
        let umask = umask(&directory);
        let default = default_gid(&directory.0);
        let gid = |path: &Path| fs::metadata(path).unwrap().gid();
        let expected = 0o640 & !umask;

        // The source's own group: the mode applies from creation.
        let destination = directory.join("same.xlsx");
        let source = SourcePermissions {
            mode: 0o640,
            gid: default,
        };
        assert_eq!(create(&destination, source), expected);
        assert_eq!((gid(&destination), mode(&destination)), (default, expected));

        // A group the process cannot assign never receives group access, not
        // even while the file is empty.
        if let Some(foreign) = foreign_gid(&directory.0) {
            let destination = directory.join("foreign.xlsx");
            let source = SourcePermissions {
                mode: 0o640,
                gid: foreign,
            };
            assert_eq!(create(&destination, source) & 0o070, 0);
            assert_eq!(
                (gid(&destination), mode(&destination)),
                (default, expected & !0o070)
            );
        }

        // Another group of the user's is assigned along with its group bits.
        let Some(&other) = other_groups(default).first() else {
            return;
        };
        let destination = directory.join("other.xlsx");
        let source = SourcePermissions {
            mode: 0o640,
            gid: other,
        };
        create(&destination, source);
        assert_eq!((gid(&destination), mode(&destination)), (other, expected));

        // A setgid directory gives new files its group; the source's group is
        // assigned instead, and a group the process cannot assign gets no
        // group access.
        let shared = directory.join("shared");
        fs::create_dir(&shared).unwrap();
        std::os::unix::fs::chown(&shared, None, Some(other)).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o2775)).unwrap();
        assert_eq!(default_gid(&shared), other);
        let destination = shared.join("inherited.xlsx");
        let source = SourcePermissions {
            mode: 0o640,
            gid: default,
        };
        assert_eq!(create(&destination, source), expected);
        assert_eq!((gid(&destination), mode(&destination)), (default, expected));
        if let Some(foreign) = foreign_gid(&shared) {
            let destination = shared.join("foreign.xlsx");
            let source = SourcePermissions {
                mode: 0o640,
                gid: foreign,
            };
            assert_eq!(create(&destination, source) & 0o070, 0);
            assert_eq!(
                (gid(&destination), mode(&destination)),
                (other, expected & !0o070)
            );
        }
        assert_eq!(fs::read_dir(&shared).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn saves_destinations_with_long_filenames() {
        let directory = TempDir::new("long-name");
        let destination = directory.join(&format!("{}.xlsx", "x".repeat(240)));
        fs::write(&destination, b"original").unwrap();
        replace(&destination, b"replacement").unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"replacement");
        assert_eq!(directory.entries(), 1);
    }

    #[cfg(windows)]
    #[test]
    // Windows-only: this clears the readonly attribute, not Unix world-write bits.
    #[allow(clippy::permissions_set_readonly_false)]
    fn failed_saves_remove_readonly_temporary_files() {
        let directory = TempDir::new("readonly");
        let destination = directory.join("book.xlsx");
        fs::write(&destination, b"original").unwrap();
        let mut permissions = fs::metadata(&destination).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&destination, permissions).unwrap();
        let result = write(&destination, None, |writer| {
            writer.write_all(b"partial replacement")?;
            Err(io::Error::other("intentional serialization failure").into())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"original");
        assert_eq!(directory.entries(), 1);
        assert!(fs::metadata(&destination).unwrap().permissions().readonly());

        // Windows also rejects replacing a readonly destination at rename.
        let result = replace(&destination, b"replacement");
        assert!(result.is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"original");
        assert_eq!(directory.entries(), 1);
        let mut permissions = fs::metadata(&destination).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&destination, permissions).unwrap();
    }
}
