//! Placement-window commands and the journal used by explicit revert.

mod commands;
pub mod journal;
mod revert;

pub use commands::{
    Close, CurrentStep, List, RecordRequest, StepSummary, Summary, WindowResult, RECORD_HELP,
};
pub use revert::{Outcome as RevertOutcome, RevertResult};

use std::{
    fmt,
    path::{Path, PathBuf},
};

use crate::{setup, stack::WindowCheck};
use journal::{Locked, Snapshot};

/// Resolved installation and daemon connection. Run these synchronous commands
/// off the async executor, including the presence and daemon queries.
#[derive(Clone, Debug)]
pub struct Windows {
    root: PathBuf,
    connection: Option<PathBuf>,
}

impl Windows {
    pub fn discover(connection: Option<&Path>) -> Result<Self, WindowError> {
        Self::at(&setup::data_directory()?, connection)
    }

    pub(crate) fn at(root: &Path, connection: Option<&Path>) -> Result<Self, WindowError> {
        Ok(Self {
            root: std::fs::canonicalize(root)
                .map_err(|error| journal::paths::error_at(root, error))?,
            connection: connection.map(Path::to_owned),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read a window for planning without taking or creating its lock.
    /// Unreadable windows block other operations even when they name a different id.
    pub fn snapshot(&self, id: &str) -> Result<Snapshot, WindowError> {
        journal::dir(&self.root, id)?;
        self.ensure_readable()?;
        journal::snapshot(&self.root, id).map_err(Into::into)
    }

    /// Resolve a revert plan. Revert itself checks whether the chosen state allows it.
    pub fn revert_candidate(&self, id: Option<&str>) -> Result<Snapshot, WindowError> {
        if let Some(id) = id {
            return self.snapshot(id);
        }
        self.ensure_readable()?
            .into_iter()
            .find_map(|row| {
                let row = row.ok()?;
                (!row.state(journal::Owner::liveness).terminal()).then_some(row)
            })
            .ok_or_else(|| "no non-terminal window".into())
    }

    /// Lock and re-read after prompting or preflight. The count pins the plan the
    /// operator approved, not a possibly older unlocked snapshot.
    pub fn lock(&self, id: &str, record_count: usize) -> Result<Locked, WindowError> {
        journal::dir(&self.root, id)?;
        self.ensure_readable()?;
        let locked = Locked::open(&self.root, id)?;
        if locked.snapshot().record_count != record_count {
            return Err(format!("window {id} changed while confirming the plan").into());
        }
        Ok(locked)
    }

    pub fn stack_check(&self, except: Option<&str>) -> WindowCheck {
        self.check_stack(except)
    }

    /// Recheck protection when the daemon answers. An offline command can rely
    /// on the verified unprotected observation persisted in the begin record.
    pub fn recheck_protection(&self) -> Result<(), WindowError> {
        self.check_protection(false)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowError {
    pub exit: i32,
    pub message: String,
}

impl WindowError {
    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self {
            exit: 1,
            message: message.into(),
        }
    }
}

impl From<String> for WindowError {
    fn from(message: String) -> Self {
        Self { exit: 2, message }
    }
}

impl From<&str> for WindowError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

impl fmt::Display for WindowError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(out)
    }
}

impl std::error::Error for WindowError {}

fn valid_module(id: &str) -> bool {
    subc_protocol::manifest::validate_module_id(id).is_ok()
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string always serializes")
}
