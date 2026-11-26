//! Bounded, read-only diagnostics for the local stack and its daemon.

mod fix;
mod host;
mod module_checks;
mod program_checks;
mod program_fixes;
mod report;
pub mod rescue;
mod run;
mod stack_checks;
mod window_checks;

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "test-support"))]
pub(crate) use tests::run_with_blocked_describe;

#[cfg(unix)]
pub use program_checks::run_program_pool;
pub use program_checks::CheckedProgram;
pub use program_fixes::{Guard, Interrupt, Programs, Prompter, WaitStatus};
pub use report::*;
pub use run::{run, run_with_programs, ProgramRun};

pub(crate) use host::{DescribeOutcome, DoctorHost};
pub(crate) use stack_checks::{classify_service_and_daemon, ProcessEntry};

/// Controls which daemon to inspect and whether to attempt an offered revert.
#[derive(Debug, Default)]
pub struct DoctorOptions {
    /// An explicitly selected connection file, if any.
    pub connection_override: Option<std::path::PathBuf>,
    /// Attempt the offered revert through the normal approval flow.
    pub fix: bool,
}

/// Reads one configured module's local log tail when no daemon answers.
pub trait LogFallback: Send + Sync {
    /// Read the bounded tail of the module's active log files without printing.
    fn tail(&self, module_id: &str) -> LogFallbackRead;
}

/// The result of reading a module's local logs and resolving its log locations.
pub struct LogFallbackRead {
    /// The tail, no source (`Ok(None)`), or the read error.
    pub tail: Result<Option<LogTail>, String>,
    /// A non-fatal warning from resolving the run folder, if any.
    pub run_folder_warning: Option<String>,
}

/// Lines read from a configured module's local logs.
pub struct LogTail {
    /// The module whose logs were read.
    pub module_id: String,
    /// Complete lines with their original timestamps and text intact.
    pub entries: Vec<String>,
    /// Whether the byte window excluded file content.
    pub truncated: bool,
}
