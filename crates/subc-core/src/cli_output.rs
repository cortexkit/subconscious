//! Checked stdout for operator commands, including setup's library renderers.

use std::io::{self, Write};

/// A reader that closes early has consumed all the output it wants on every OS.
pub fn stdout_result(result: io::Result<()>) -> io::Result<()> {
    if result
        .as_ref()
        .is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
    {
        std::process::exit(0);
    }
    result
}

/// Keep ordinary stdout errors visible without panicking on a closed pipe.
pub fn print(args: std::fmt::Arguments<'_>) {
    if let Err(error) = stdout_result(io::stdout().lock().write_fmt(args)) {
        panic!("failed printing to stdout: {error}");
    }
}

macro_rules! checked_println {
    () => { $crate::cli_output::print(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::cli_output::print(format_args!("{}\n", format_args!($($arg)*))) };
}

pub(crate) use checked_println as println;
