#![forbid(unsafe_code)]
#![warn(clippy::unwrap_used)]

mod cli;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::main()
}
