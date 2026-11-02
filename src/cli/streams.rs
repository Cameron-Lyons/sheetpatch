use super::args::Command;
use super::error::Failure;
use sheetpatch::Workbook;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;

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
            Self::Stdin => Workbook::from_reader(io::stdin().lock()).map_err(|error| {
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
