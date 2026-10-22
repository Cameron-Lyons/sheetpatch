//! Write a complete replacement beside the destination before renaming it.
use crate::Result;
#[cfg(target_os = "linux")]
use rustix::fs as rustix_fs;
#[cfg(target_os = "linux")]
use rustix::io as rustix_io;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

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
        self.file
            .as_ref()
            .expect("the temporary file stays open until `write` closes it to rename it")
    }
}

/// What a new save destination inherits from the regular file a workbook was
/// read from: Unix mode bits and group, without its owner or ACL.
#[derive(Clone, Copy)]
pub(crate) struct SourcePermissions {
    #[cfg(unix)]
    mode: Mode,
    #[cfg(unix)]
    gid: u32,
}

impl SourcePermissions {
    /// The permissions of `file`, an open regular file with `metadata`.
    pub(crate) fn new(file: &File, metadata: &Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            #[cfg(target_os = "linux")]
            let mode = file_mode(metadata, |value| {
                rustix_fs::fgetxattr(file, ACCESS_ACL, value)
            });
            #[cfg(not(target_os = "linux"))]
            let mode = {
                let _ = file;
                Mode::from(metadata.mode())
            };
            Self {
                mode,
                gid: metadata.gid(),
            }
        }
        // Other platforms create new files with default permissions.
        #[cfg(not(unix))]
        {
            let _ = (file, metadata);
            Self {}
        }
    }
}

/// A file's mode bits, as a new file in its owning group receives them.
///
/// With an access ACL, Linux keeps the ACL's mask in the group mode bits. The
/// mask only limits the owning group's own entry, which can grant less. A new
/// file that inherits an access ACL from its directory's default ACL takes its
/// group mode bits as that ACL's mask, so it keeps the mask; any other new
/// file takes them as its owning group's permissions.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mode {
    /// The mode bits, with the group bits the owning group actually has.
    plain: u32,
    /// The mode bits, with an access ACL's mask as the group bits.
    masked: u32,
}

#[cfg(unix)]
impl From<u32> for Mode {
    /// The mode of a file without an access ACL.
    fn from(mode: u32) -> Self {
        Self {
            plain: mode,
            masked: mode,
        }
    }
}

#[cfg(unix)]
impl Mode {
    /// The mode bits for `file`, a new file in the owning group.
    fn of(self, file: &File) -> u32 {
        if self.inherited_acl(file) {
            self.masked
        } else {
            self.plain
        }
    }

    /// Whether `file`, a new file, has an access ACL whose mask matters: one
    /// it inherits from its directory's default ACL.
    fn inherited_acl(self, file: &File) -> bool {
        // An empty buffer asks only whether the file has an access ACL.
        #[cfg(target_os = "linux")]
        let has_acl = self.masked != self.plain
            && rustix_fs::fgetxattr(file, ACCESS_ACL, &mut [0; 0]).is_ok();
        // Elsewhere both modes are the file's mode bits as they are.
        #[cfg(not(target_os = "linux"))]
        let has_acl = {
            let _ = file;
            false
        };
        has_acl
    }
}

/// The extended attribute that holds a Linux file's access ACL.
#[cfg(target_os = "linux")]
const ACCESS_ACL: &str = "system.posix_acl_access";

/// The mode of a file with `metadata`. `read_acl` reads the file's access ACL
/// into a buffer.
///
/// A file without an ACL, or on a filesystem without them, keeps its mode. An
/// ACL that cannot be read leaves the owning group's access unknown, so group
/// and other bits are cleared.
#[cfg(target_os = "linux")]
fn file_mode(
    metadata: &Metadata,
    read_acl: impl FnOnce(&mut [u8]) -> rustix_io::Result<usize>,
) -> Mode {
    use rustix::io::Errno;
    use std::os::unix::fs::MetadataExt;

    // No extended attribute value is larger than XATTR_SIZE_MAX (64 KiB).
    let mut value: Vec<u8> = vec![0; 1 << 16];
    match read_acl(&mut value) {
        Ok(len) => effective_mode(metadata.mode(), Some(&value[..len])),
        Err(Errno::NODATA | Errno::NOTSUP) => effective_mode(metadata.mode(), None),
        Err(_) => Mode::from(metadata.mode() & !0o077),
    }
}

/// The mode of a file with `st_mode`, whose group bits hold the mask of the
/// access ACL `acl`: its plain group bits are the mask ANDed with the ACL's
/// entry for the owning group. Without an ACL the mode is unchanged. An ACL
/// that cannot be parsed clears the group and other bits.
#[cfg(target_os = "linux")]
fn effective_mode(st_mode: u32, acl: Option<&[u8]>) -> Mode {
    let Some(acl) = acl else {
        return Mode::from(st_mode);
    };
    match owning_group_entry(acl) {
        Some(permissions) => Mode {
            plain: (st_mode & !0o070) | (st_mode & (permissions << 3)),
            masked: st_mode,
        },
        None => Mode::from(st_mode & !0o077),
    }
}

/// The permission bits of the owning group's entry (`ACL_GROUP_OBJ`) in a
/// `system.posix_acl_access` value: a little-endian version 2, then 8-byte
/// entries of a tag, permissions and an id. `None` unless the value has that
/// layout and exactly one such entry.
#[cfg(target_os = "linux")]
fn owning_group_entry(acl: &[u8]) -> Option<u32> {
    const VERSION: u32 = 2;
    const GROUP_OBJ: u16 = 0x04;

    let (&version, entries) = acl.split_first_chunk()?;
    let (entries, []) = entries.as_chunks::<8>() else {
        return None;
    };
    if u32::from_le_bytes(version) != VERSION {
        return None;
    }
    let mut owning_group = entries
        .iter()
        .filter(|entry| u16::from_le_bytes([entry[0], entry[1]]) == GROUP_OBJ);
    match (owning_group.next(), owning_group.next()) {
        (Some(entry), None) => Some(u32::from(u16::from_le_bytes([entry[2], entry[3]])) & 0o7),
        _ => None,
    }
}

/// Create a new, exclusively named file beside `destination`. On Unix `mode`
/// supplies the creation mode's permission bits. The OS applies the parent
/// directory's default ACL or, without one, the process umask.
///
/// The hidden name, `.{prefix}.sheetpatch-{pid}-{n}.tmp`, starts with at most
/// 32 bytes of the destination's file name, up to any colon, so a file left
/// behind by a killed process can be traced to its workbook.
fn create_temporary(destination: &Path, mode: Option<u32>) -> Result<Temporary> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let file_name = destination
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    // On Windows a colon starts the name of an alternate data stream: a prefix
    // with one would write the temporary into a stream of another file, and
    // removing it would leave that file behind. Every platform cuts there, so
    // temporary names do not depend on the platform.
    let name = file_name.split(':').next().unwrap_or_default();
    // Bound the prefix to keep the temporary name short: a valid destination
    // can already use the filesystem's full name limit.
    let mut end = name.len().min(32);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = &name[..end];
    loop {
        let candidate = destination.with_file_name(format!(
            ".{prefix}.sheetpatch-{}-{}.tmp",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
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
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

/// The destination's resolved metadata when it already exists.
///
/// Symlinks are followed, so a link to a regular file is accepted and later
/// replaced by the new file. Anything else that already exists (a directory,
/// FIFO, device, or socket) cannot be replaced by renaming a workbook over
/// it, so it is rejected before any output is generated.
fn existing_destination(path: &Path) -> io::Result<Option<Metadata>> {
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

/// The permission bits applied to the replacement of a destination with
/// `mode`, once it is known whether the replacement has the destination's
/// group and whether it inherited an access ACL. In that group it keeps the
/// mode that a new file there receives (see [`Mode::of`]). In another group
/// the owning group's actual permissions, not an ACL's mask, limit others.
#[cfg(unix)]
fn replacement_bits(mode: Mode, same_group: bool, inherited_acl: bool) -> u32 {
    let mode = if same_group && inherited_acl {
        mode.masked
    } else {
        mode.plain
    };
    replacement_mode(mode, same_group)
}

/// Give `file` the group `gid` when the process may, and report whether it
/// has that group. The result is checked, because some filesystems accept a
/// group change without applying it.
#[cfg(unix)]
fn assign_group(file: &File, gid: u32) -> io::Result<bool> {
    use std::os::unix::fs::{self as unix_fs, MetadataExt};

    Ok(file.metadata()?.gid() == gid
        || (unix_fs::fchown(file, None, Some(gid)).is_ok() && file.metadata()?.gid() == gid))
}

/// Create the replacement of `path`, an existing destination with resolved
/// metadata `destination`, with the destination's group and mode bits before
/// any contents are written.
#[cfg(unix)]
fn create_replacement(path: &Path, destination: &Metadata) -> Result<Temporary> {
    use std::fs::Permissions;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // Like the metadata, the ACL is read through symlinks.
    #[cfg(target_os = "linux")]
    let mode = file_mode(destination, |value| {
        rustix_fs::getxattr(path, ACCESS_ACL, value)
    });
    #[cfg(not(target_os = "linux"))]
    let mode = Mode::from(destination.mode());
    // Owner-only until the replacement has the destination's group, so no
    // group can open it yet.
    let temp = create_temporary(path, Some(mode.plain & 0o700))?;
    let same_group = assign_group(temp.file(), destination.gid())?;
    let inherited_acl = same_group && mode.inherited_acl(temp.file());
    temp.file()
        .set_permissions(Permissions::from_mode(replacement_bits(
            mode,
            same_group,
            inherited_acl,
        )))?;
    Ok(temp)
}

#[cfg(not(unix))]
fn create_replacement(path: &Path, destination: &Metadata) -> Result<Temporary> {
    let temp = create_temporary(path, None)?;
    temp.file().set_permissions(destination.permissions())?;
    Ok(temp)
}

/// Create the temporary file of `path`, a new destination.
///
/// On Unix, the source's mode bits supply the creation mode, subject to the
/// OS's default ACL or umask rules, without setuid, setgid or sticky bits. Like
/// a replacement, it keeps group mode bits only when it has the source's group,
/// assigning that group when the process may.
#[cfg(unix)]
fn create_new(path: &Path, source: Option<SourcePermissions>) -> Result<Temporary> {
    use std::fs::Permissions;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Some(source) = source else {
        return create_temporary(path, None);
    };
    // A file that inherits an access ACL takes the source's mask as its own.
    // Any other file would grant the mask to its owning group, so it is
    // discarded unwritten and created with the group's actual permissions.
    let mut temp = create_temporary(path, Some(source.mode.masked & 0o777))?;
    let mode = source.mode.of(temp.file());
    if mode != source.mode.masked {
        drop(temp);
        temp = create_temporary(path, Some(mode & 0o777))?;
    }
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
        create_temporary(path, Some(mode & 0o700))?
    };
    let same_group = assign_group(temp.file(), source.gid)?;
    // As for a replacement, the source's owning group's actual permissions
    // limit others in another group.
    let mode = if same_group {
        mode
    } else {
        mode & source.mode.plain
    };
    temp.file()
        .set_permissions(Permissions::from_mode(replacement_mode(mode, same_group)))?;
    Ok(temp)
}

#[cfg(not(unix))]
fn create_new(path: &Path, _: Option<SourcePermissions>) -> Result<Temporary> {
    create_temporary(path, None)
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
/// On Linux, the group bits of a file with an access ACL hold the ACL's mask.
/// A new file that inherits an access ACL from its directory keeps that mask;
/// any other gets the owning group's own entry as limited by the mask.
/// Setuid, setgid and sticky bits are never applied.
pub(crate) fn write(
    path: &Path,
    source: Option<SourcePermissions>,
    serialize: impl FnOnce(&mut BufWriter<&File>) -> Result<()>,
) -> Result<()> {
    use std::io::Write as _;

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
        Some(existing) => create_replacement(path, &existing)?,
        None => create_new(path, source)?,
    };
    let file = temp.file();
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
    use std::env;
    use std::io::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs as unix_fs;
    #[cfg(unix)]
    use std::process::Command;

    /// A uniquely named directory that is removed even when a test fails.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = env::temp_dir().join(format!(
                "sheetpatch-atomic-{label}-{}-{}",
                process::id(),
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
        let output = Command::new("id").arg("-G").output();
        let Some(groups) = output
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
        else {
            return Vec::new();
        };
        groups
            .split_whitespace()
            .filter_map(|gid| gid.parse().ok())
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
                && unix_fs::chown(&probe, None, Some(gid)).is_err()
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
        use std::os::unix::net::UnixListener;

        let directory = TempDir::new("special");
        let fifo = directory.join("fifo.xlsx");
        let status = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo is available");
        assert!(status.success());
        // Opening the FIFO would block without a reader; it must not be opened.
        let result = write(&fifo, None, |_| panic!("rejected before serializing"));
        assert_invalid_input(result, "destination is not a regular file");
        assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());

        let socket = directory.join("socket.xlsx");
        let _listener = UnixListener::bind(&socket).unwrap();
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
        use std::os::unix::fs::FileTypeExt;

        let directory = TempDir::new("symlink");
        let device = directory.join("null.xlsx");
        unix_fs::symlink("/dev/null", &device).unwrap();
        let result = write(&device, None, |_| panic!("rejected before serializing"));
        assert_invalid_input(result, "destination is not a regular file");
        assert!(fs::symlink_metadata(&device).unwrap().is_symlink());
        assert!(fs::metadata(&device).unwrap().file_type().is_char_device());

        // A link to a regular file is replaced by the new file; the original
        // target is left unchanged.
        let target = directory.join("target.xlsx");
        fs::write(&target, b"original").unwrap();
        let link = directory.join("link.xlsx");
        unix_fs::symlink(&target, &link).unwrap();
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
        let temporary = create_temporary(&directory.join("book.xlsx"), Some(0o600)).unwrap();
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
                mode: Mode::from(0o600),
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
        let other_group = other_groups(default_gid)
            .into_iter()
            .find(|&gid| unix_fs::chown(&destination, None, Some(gid)).is_ok());
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
        assert_eq!(replacement_mode(0o640, /* same_group: */ true), 0o640);
        assert_eq!(replacement_mode(0o640, /* same_group: */ false), 0o600);
        assert_eq!(replacement_mode(0o664, /* same_group: */ false), 0o604);
        assert_eq!(replacement_mode(0o2775, /* same_group: */ true), 0o775);
        assert_eq!(replacement_mode(0o2775, /* same_group: */ false), 0o705);
        assert_eq!(replacement_mode(0o4755, /* same_group: */ false), 0o705);
        assert_eq!(replacement_mode(0o7777, /* same_group: */ true), 0o777);
        assert_eq!(replacement_mode(0o100_644, /* same_group: */ true), 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn changing_groups_does_not_grant_other_access_to_the_original_group() {
        // Unix checks group permissions before other permissions. Members of
        // the old group denied read access by 0604 must not gain it when they
        // become "other" after the replacement gets a different group.
        for (original, expected) in [(0o604, 0o600), (0o646, 0o604), (0o624, 0o600)] {
            assert_eq!(
                replacement_mode(original, /* same_group: */ false),
                expected
            );
            assert_eq!(replacement_mode(original, /* same_group: */ true), original);
        }

        let directory = TempDir::new("other-access");
        let Some(foreign) = foreign_gid(&directory.0) else {
            return;
        };
        let umask = umask(&directory);
        let destination = directory.join("private-to-group.xlsx");
        let source = SourcePermissions {
            mode: Mode::from(0o604),
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
            let source = permissions.map(|mode| SourcePermissions {
                mode: Mode::from(mode),
                gid,
            });
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
            mode: Mode::from(0o640),
            gid: default,
        };
        assert_eq!(create(&destination, source), expected);
        assert_eq!((gid(&destination), mode(&destination)), (default, expected));

        // A group the process cannot assign never receives group access, not
        // even while the file is empty.
        if let Some(foreign) = foreign_gid(&directory.0) {
            let destination = directory.join("foreign.xlsx");
            let source = SourcePermissions {
                mode: Mode::from(0o640),
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
            mode: Mode::from(0o640),
            gid: other,
        };
        create(&destination, source);
        assert_eq!((gid(&destination), mode(&destination)), (other, expected));

        // A setgid directory gives new files its group; the source's group is
        // assigned instead, and a group the process cannot assign gets no
        // group access.
        let shared = directory.join("shared");
        fs::create_dir(&shared).unwrap();
        unix_fs::chown(&shared, None, Some(other)).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o2775)).unwrap();
        assert_eq!(default_gid(&shared), other);
        let destination = shared.join("inherited.xlsx");
        let source = SourcePermissions {
            mode: Mode::from(0o640),
            gid: default,
        };
        assert_eq!(create(&destination, source), expected);
        assert_eq!((gid(&destination), mode(&destination)), (default, expected));
        if let Some(foreign) = foreign_gid(&shared) {
            let destination = shared.join("foreign.xlsx");
            let source = SourcePermissions {
                mode: Mode::from(0o640),
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

    /// Share `path` with one other user through an access ACL. The ACL's mask,
    /// rw-, then takes the place of the group mode bits, while the owning
    /// group's own entry keeps the old group bits. Returns false when
    /// `setfacl` is missing or the filesystem refuses ACLs.
    #[cfg(target_os = "linux")]
    fn share_with_one_user(path: &Path) -> bool {
        Command::new("setfacl")
            .args(["-m", "u:65534:rw"])
            .arg(path)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn access_acl_masks_do_not_grant_the_owning_group_access() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new("acl");
        let umask = umask(&directory);
        let mut shared = None;
        for permissions in [0o600, 0o604] {
            let destination = directory.join(&format!("book-{permissions:o}.xlsx"));
            fs::write(&destination, b"original").unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(permissions)).unwrap();
            if !share_with_one_user(&destination) {
                return;
            }
            assert_eq!(mode(&destination), permissions | 0o060);
            let file = File::open(&destination).unwrap();
            let source = SourcePermissions::new(&file, &file.metadata().unwrap());
            drop(file);

            // The owning group's entry is ---, so its replacement and a new
            // copy give that group no access.
            replace(&destination, b"replacement").unwrap();
            assert_eq!(mode(&destination), permissions);
            let created = directory.join(&format!("new-{permissions:o}.xlsx"));
            assert_eq!(create(&created, source), permissions & !umask);
            assert_eq!(mode(&created), permissions & !umask);
            shared = Some(source);
        }

        // Members of the owning group were denied, even though other::r--
        // allows reading. A copy in a group the process cannot assign puts
        // them among others, so others get no access either.
        let Some(foreign) = foreign_gid(&directory.0) else {
            return;
        };
        let source = SourcePermissions {
            gid: foreign,
            ..shared.unwrap()
        };
        let destination = directory.join("foreign.xlsx");
        assert_eq!(create(&destination, source), 0o600 & !umask);
        assert_eq!(mode(&destination), 0o600 & !umask);
        assert_eq!(directory.entries(), 5);
    }

    /// Give `directory` a default ACL that shares new files with one other
    /// user, but not with their owning group: new files inherit user:65534:rw-,
    /// group::--- and other::r--, and their group mode bits set the inherited
    /// ACL's mask. Returns false when `setfacl` is missing or the filesystem
    /// refuses ACLs.
    #[cfg(target_os = "linux")]
    fn share_new_files_with_one_user(directory: &Path) -> bool {
        Command::new("setfacl")
            .args(["-d", "-m", "u:65534:rw,g::-,o::r"])
            .arg(directory)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_access_acls_keep_their_mask() {
        let directory = TempDir::new("default-acl");
        if !share_new_files_with_one_user(&directory.0) {
            return;
        }
        // The default ACL replaces the umask: the mask is rw-, so the other
        // user can write, while the owning group has no access.
        let destination = directory.join("book.xlsx");
        fs::write(&destination, b"original").unwrap();
        assert_eq!(mode(&destination), 0o664);
        let file = File::open(&destination).unwrap();
        let source = SourcePermissions::new(&file, &file.metadata().unwrap());
        drop(file);

        // The replacement and a new copy inherit the same ACL, so their group
        // bits are its mask again. The owning group's own entry, ---, would
        // take the other user's write access away.
        replace(&destination, b"replacement").unwrap();
        assert_eq!(mode(&destination), 0o664);
        let file = File::open(&destination).unwrap();
        let replaced = SourcePermissions::new(&file, &file.metadata().unwrap());
        assert_eq!(replaced.mode.plain & 0o777, 0o604);
        let created = directory.join("new.xlsx");
        assert_eq!(create(&created, source), 0o664);
        assert_eq!(mode(&created), 0o664);

        // A copy in a group the process cannot assign drops the mask, and the
        // owning group's own entry still limits others.
        let Some(foreign) = foreign_gid(&directory.0) else {
            return;
        };
        let source = SourcePermissions {
            gid: foreign,
            ..source
        };
        let destination = directory.join("foreign.xlsx");
        assert_eq!(create(&destination, source), 0o600);
        assert_eq!(mode(&destination), 0o600);
        assert_eq!(directory.entries(), 3);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn symlinked_destinations_use_the_access_acl_of_their_target() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new("acl-symlink");
        let target = directory.join("target.xlsx");
        fs::write(&target, b"original").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        if !share_with_one_user(&target) {
            return;
        }
        // The link has no ACL of its own. Like the mode bits, the owning
        // group's entry comes from the target, so the group gets no access.
        let link = directory.join("link.xlsx");
        unix_fs::symlink(&target, &link).unwrap();
        replace(&link, b"replacement").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_file());
        assert_eq!(mode(&link), 0o600);
        assert_eq!(mode(&target), 0o660);
        assert_eq!(directory.entries(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn replacements_in_another_group_limit_others_by_the_owning_group_entry() {
        // A 0604 file shared with one user through an access ACL whose mask
        // is rw- while the owning group's own entry grants nothing.
        let mode = Mode {
            plain: 0o604,
            masked: 0o664,
        };
        // Each case is a destination's mode, whether its replacement has the
        // destination's group, whether the replacement inherited an access
        // ACL, and the replacement's permission bits.
        let cases = [
            // In another group, others keep only what the owning group had.
            (mode, false, false, 0o600),
            (mode, false, true, 0o600),
            // In the destination's group, an inherited ACL keeps the mask.
            (mode, true, false, 0o604),
            (mode, true, true, 0o664),
            (Mode::from(0o4775), false, false, 0o705),
        ];
        for (mode, same_group, inherited_acl, expected) in cases {
            assert_eq!(
                replacement_bits(mode, same_group, inherited_acl),
                expected,
                "{mode:?}, same group: {same_group}, inherited ACL: {inherited_acl}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn access_acls_limit_group_bits_to_the_owning_group_entry() {
        const USER_OBJ: u16 = 0x01;
        const USER: u16 = 0x02;
        const GROUP_OBJ: u16 = 0x04;
        const GROUP: u16 = 0x08;
        const MASK: u16 = 0x10;
        const OTHER: u16 = 0x20;
        const UNDEFINED: u32 = u32::MAX;
        /// A `system.posix_acl_access` value of `version` with `entries`.
        fn acl(version: u32, entries: &[(u16, u16, u32)]) -> Vec<u8> {
            let mut value = version.to_le_bytes().to_vec();
            for &(tag, permissions, id) in entries {
                value.extend(tag.to_le_bytes());
                value.extend(permissions.to_le_bytes());
                value.extend(id.to_le_bytes());
            }
            value
        }

        assert_eq!(effective_mode(0o100_664, None), Mode::from(0o100_664));
        // `chmod 604; setfacl -m u:65534:rw` stores mask::rw- in the group
        // bits while group:: stays ---.
        let shared = acl(
            2,
            &[
                (USER_OBJ, 6, UNDEFINED),
                (USER, 6, 65534),
                (GROUP_OBJ, 0, UNDEFINED),
                (MASK, 6, UNDEFINED),
                (OTHER, 4, UNDEFINED),
            ],
        );
        assert_eq!(
            effective_mode(0o100_664, Some(&shared)),
            Mode {
                plain: 0o100_604,
                masked: 0o100_664
            }
        );
        // group::rw- with mask::r-x leaves the group only r--.
        let narrow = acl(
            2,
            &[
                (USER_OBJ, 7, UNDEFINED),
                (GROUP_OBJ, 6, UNDEFINED),
                (GROUP, 7, 100),
                (MASK, 5, UNDEFINED),
                (OTHER, 0, UNDEFINED),
            ],
        );
        assert_eq!(
            effective_mode(0o100_750, Some(&narrow)),
            Mode {
                plain: 0o100_740,
                masked: 0o100_750
            }
        );
        // Without a mask the group bits already are group::.
        let minimal = acl(
            2,
            &[
                (USER_OBJ, 6, UNDEFINED),
                (GROUP_OBJ, 4, UNDEFINED),
                (OTHER, 4, UNDEFINED),
            ],
        );
        assert_eq!(
            effective_mode(0o100_644, Some(&minimal)),
            Mode::from(0o100_644)
        );

        // Anything unparsable leaves only the owner's bits.
        let wrong_version = acl(1, &[(GROUP_OBJ, 6, UNDEFINED)]);
        let no_group = acl(2, &[(USER_OBJ, 6, UNDEFINED), (MASK, 6, UNDEFINED)]);
        let two_groups = acl(2, &[(GROUP_OBJ, 0, UNDEFINED), (GROUP_OBJ, 6, UNDEFINED)]);
        for garbled in [
            &b""[..],
            &shared[..3],
            &shared[..shared.len() - 1],
            &wrong_version,
            &no_group,
            &two_groups,
        ] {
            assert_eq!(
                effective_mode(0o100_664, Some(garbled)),
                Mode::from(0o100_600)
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unreadable_access_acls_leave_only_owner_permissions() {
        use rustix::io::Errno;
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new("acl-errors");
        let path = directory.join("book.xlsx");
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        // No ACL, or no ACL support: the mode bits are the permissions.
        for error in [Errno::NODATA, Errno::NOTSUP, Errno::OPNOTSUPP] {
            assert_eq!(file_mode(&metadata, |_| Err(error)), Mode::from(0o100_664));
        }
        for error in [Errno::IO, Errno::ACCESS, Errno::NOENT, Errno::RANGE] {
            assert_eq!(file_mode(&metadata, |_| Err(error)), Mode::from(0o100_600));
        }
        // An ACL that grants the owning group read access only.
        let acl = [2, 0, 0, 0, 0x04, 0, 4, 0, 0xff, 0xff, 0xff, 0xff];
        let read = |value: &mut [u8]| {
            value[..acl.len()].copy_from_slice(&acl);
            Ok(acl.len())
        };
        assert_eq!(
            file_mode(&metadata, read),
            Mode {
                plain: 0o100_644,
                masked: 0o100_664
            }
        );
    }

    #[test]
    fn temporary_files_are_hidden_and_named_after_their_destination() {
        let directory = TempDir::new("temporary-name");
        let long = format!("{}.xlsx", "x".repeat(40));
        // 33 bytes, so the cut at 32 bytes falls inside the last character.
        let euros = "€".repeat(11);
        let mut names = vec![
            ("book.xlsx", "book.xlsx".to_owned()),
            (".hidden.xlsx", ".hidden.xlsx".to_owned()),
            (&long, "x".repeat(32)),
            (&euros, "€".repeat(10)),
        ];
        // On Windows a colon in a destination name starts the name of an
        // alternate data stream, so only Unix saves to this name.
        if cfg!(unix) {
            names.push(("v2:book.xlsx", "v2".to_owned()));
        }
        let saves = names.len();
        for (name, prefix) in names {
            let destination = directory.join(name);
            let mut temporary = None;
            write(&destination, None, |writer| {
                temporary = fs::read_dir(&directory.0)?
                    .map(|entry| entry.map(|entry| entry.file_name()))
                    .collect::<io::Result<Vec<_>>>()?
                    .into_iter()
                    .find(|entry| entry.to_string_lossy().ends_with(".tmp"));
                writer.write_all(b"new")?;
                Ok(())
            })
            .unwrap();
            let temporary = temporary.expect("temporary file exists while serializing");
            let temporary = temporary.to_str().unwrap();
            let counter = temporary
                .strip_prefix(&format!(".{prefix}.sheetpatch-{}-", process::id()))
                .and_then(|rest| rest.strip_suffix(".tmp"));
            assert!(
                counter.is_some_and(|counter| counter.parse::<u64>().is_ok()),
                "{temporary}"
            );
        }
        assert_eq!(directory.entries(), saves);
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
