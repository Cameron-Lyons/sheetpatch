use super::args::USAGE;
use super::output::{Escape, Escaped};
use std::fmt::{self, Display, Formatter};
use std::io::{self, Write};

/// Quote ordinary tokens exactly as before, but bound diagnostics derived
/// from malformed input. Truncate on a UTF-8 boundary before escaping.
pub(super) struct DiagnosticText<'text>(pub(super) &'text str);

impl Display for DiagnosticText<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        const MAX_BYTES: usize = 256;
        let text = self.0;
        let mut end = text.len().min(MAX_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        write!(f, "{:?}", &text[..end])?;
        if end < text.len() {
            write!(f, "… ({} bytes)", text.len())?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum Failure {
    /// Invalid command syntax; the full usage follows the message.
    Usage(String),
    /// An invalid argument value or patch row; a pointer to the command's help
    /// follows the message.
    Invalid {
        command: &'static str,
        message: String,
    },
    /// A workbook could not be read, edited, or written.
    Workbook(sheetpatch::Error),
    /// A workbook error, prefixed with what failed: an operation and its path
    /// or stream, or the patch line whose edit was refused.
    Context(String, sheetpatch::Error),
    /// Writing text to standard output failed. A workbook that cannot be
    /// written to standard output is a `Context` failure instead, so a closed
    /// pipe is never mistaken for a complete archive.
    Output(io::Error),
}

impl Failure {
    pub(super) fn invalid(command: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            command,
            message: message.into(),
        }
    }

    pub(super) fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) | Self::Invalid { .. } => 2,
            Self::Workbook(_) | Self::Context(..) | Self::Output(_) => 1,
        }
    }
}

impl Display for Failure {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(f, "{message}\n\n{USAGE}"),
            Self::Invalid { command, message } => {
                let topic = if *command == "patch" {
                    "the format"
                } else {
                    "details"
                };
                write!(f, "{message}\nRun 'sheetpatch help {command}' for {topic}.")
            }
            Self::Workbook(error) => write!(f, "{error}"),
            // The context already says what failed; print the bare OS message.
            Self::Context(context, sheetpatch::Error::Io(error)) => write!(f, "{context}: {error}"),
            Self::Context(context, error) => write!(f, "{context}: {error}"),
            Self::Output(error) => write!(f, "cannot write output: {error}"),
        }
    }
}

impl From<sheetpatch::Error> for Failure {
    fn from(error: sheetpatch::Error) -> Self {
        Self::Workbook(error)
    }
}

/// Writes to standard output. Other I/O errors are converted with context.
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Output(error)
    }
}

pub(super) fn write_failure(
    output: &mut impl Write,
    failure: &Failure,
    terminal: bool,
) -> io::Result<()> {
    // Diagnostics can contain paths and workbook metadata. Keep their control
    // characters from reaching a terminal, while retaining help line breaks.
    let failure = Escaped {
        value: failure,
        escape: Escape::values(terminal),
    };
    writeln!(output, "sheetpatch: {failure}")
}
