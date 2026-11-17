use super::args::Command;
use super::error::Failure;
use sheetpatch::Workbook;
use std::ffi::OsStr;
#[cfg(unix)]
use std::fs::File;
use std::io::{self, BufRead, Write};
#[cfg(unix)]
use std::os::fd::{AsFd as _, BorrowedFd};
use std::path::Path;

// On Unix, std's standard stream handles treat a descriptor that is not open
// for their direction, such as standard output under `1<file`, as one that
// silently accepts writes and reads as empty (`EBADF`). The CLI uses a
// duplicate of the descriptor instead, which reports that error, so a
// workbook is not discarded with exit code 0 and a patch does not read as
// empty. Only when no descriptor is free for the duplicate does it fall back
// to the std handle. A closed descriptor is unaffected: std reopens it as
// `/dev/null`.
#[cfg(unix)]
fn duplicate(stream: BorrowedFd<'_>) -> Option<File> {
    stream.try_clone_to_owned().ok().map(File::from)
}

/// Standard output, for every command's results. Its duplicate descriptor is
/// made at the first write, so a command that writes nothing there, such as
/// `set` with an OUTPUT file, never holds it, and one that does has already
/// closed its INPUT.
#[derive(Default)]
pub(super) struct Stdout(Option<Box<dyn Write>>);

impl Stdout {
    fn stream(&mut self) -> &mut dyn Write {
        &mut **self.0.get_or_insert_with(|| {
            #[cfg(unix)]
            if let Some(file) = duplicate(io::stdout().as_fd()) {
                return Box::new(file);
            }
            Box::new(io::stdout().lock())
        })
    }
}

impl Write for Stdout {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.as_mut().map_or(Ok(()), |stream| stream.flush())
    }
}

/// Standard input, for a workbook or a patch.
pub(super) fn stdin() -> Box<dyn BufRead> {
    #[cfg(unix)]
    if let Some(file) = duplicate(io::stdin().as_fd()) {
        return Box::new(io::BufReader::new(file));
    }
    Box::new(io::stdin().lock())
}

pub(super) fn quoted_path(path: &OsStr) -> String {
    format!("'{}'", Path::new(path).display())
}

/// Where a command reads its workbook from.
pub(super) enum Source<'args> {
    Stdin,
    File(&'args OsStr),
}

impl<'args> Source<'args> {
    pub(super) fn try_new(command: Command, arg: &'args OsStr) -> Result<Self, Failure> {
        use std::io::IsTerminal as _;

        if arg != "-" {
            return Ok(Self::File(arg));
        }
        if io::stdin().is_terminal() {
            return Err(Failure::invalid(
                command.name(),
                "refusing to read a workbook from a terminal; redirect standard input or name an INPUT file",
            ));
        }
        Ok(Self::Stdin)
    }

    pub(super) fn open(self) -> Result<Workbook, Failure> {
        match self {
            Self::File(path) => Workbook::open(path).map_err(|error| {
                Failure::Context(format!("cannot read workbook {}", quoted_path(path)), error)
            }),
            // Reading stops once the input exceeds the largest ZIP32 archive.
            Self::Stdin => Workbook::from_reader(stdin()).map_err(|error| {
                Failure::Context("cannot read workbook from standard input".into(), error)
            }),
        }
    }
}

/// What saving a workbook does with obsolete ZIP records already in its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ObsoleteRecords {
    /// Retain them, as `set` and `patch` do.
    Keep,
    /// Remove them, as `compact` does.
    Remove,
}

/// Where a command writes its workbook.
pub(super) enum Destination<'args> {
    Stdout,
    File(&'args OsStr),
}

impl<'args> Destination<'args> {
    pub(super) fn try_new(
        command: Command,
        arg: &'args OsStr,
        terminal: bool,
    ) -> Result<Self, Failure> {
        if arg != "-" {
            return Ok(Self::File(arg));
        }
        if terminal {
            return Err(Failure::invalid(
                command.name(),
                "refusing to write a workbook to a terminal; redirect standard output or name an OUTPUT file",
            ));
        }
        Ok(Self::Stdout)
    }

    pub(super) fn save(
        self,
        workbook: &Workbook,
        obsolete_records: ObsoleteRecords,
        output: &mut impl Write,
    ) -> Result<(), Failure> {
        match self {
            Self::File(path) => {
                let saved = match obsolete_records {
                    ObsoleteRecords::Keep => workbook.save(path),
                    ObsoleteRecords::Remove => workbook.save_compact(path),
                };
                // Generation errors, such as ZIP limits, match `patch --check`
                // output; only file system failures name the destination.
                saved.map_err(|error| match error {
                    sheetpatch::Error::Io(_) => {
                        Failure::Context(format!("cannot save {}", quoted_path(path)), error)
                    }
                    error => Failure::Workbook(error),
                })
            }
            Self::Stdout => {
                let written = match obsolete_records {
                    ObsoleteRecords::Keep => workbook.write_to(&mut *output),
                    ObsoleteRecords::Remove => workbook.write_compact_to(&mut *output),
                };
                // Archive generation reports I/O errors only from its writer.
                // Unlike text, a truncated archive is unusable, so even a pipe
                // closed by its reader is a failure.
                written.map_err(|error| match error {
                    sheetpatch::Error::Io(_) => {
                        Failure::Context("cannot write workbook to standard output".into(), error)
                    }
                    error => Failure::Workbook(error),
                })
            }
        }
    }
}
