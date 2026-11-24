use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{
    super::{
        journal::{self, content, paths, Entry, Liveness, Owner, State},
        json_string, WindowError,
    },
    Row, MANUAL,
};
use crate::stack::{RosterError, Survivor};

#[derive(Debug, Serialize)]
pub struct List {
    schema: &'static str,
    pub windows: Vec<Summary>,
}

impl List {
    pub(super) fn new(windows: Vec<Summary>) -> Self {
        Self {
            schema: "ck.window.list/v1",
            windows,
        }
    }

    pub fn human(&self) -> String {
        if self.windows.is_empty() {
            return "No windows.".into();
        }
        self.windows
            .iter()
            .map(Summary::human)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub id: String,
    pub note: Option<String>,
    pub state: State,
    pub step_count: usize,
    pub disk_bytes: Option<u64>,
    pub stopped_at: Option<CurrentStep>,
    pub busy: Option<CurrentStep>,
    pub cleanup_pending: bool,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert_status: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CurrentStep {
    pub index: usize,
    pub total: usize,
}

impl Summary {
    pub(super) fn new(root: &Path, row: Row, liveness: impl Fn(&Owner) -> Liveness) -> Self {
        match row {
            Err((id, error)) => {
                let disk_bytes = content::tree_size(&root.join("windows").join(&id)).ok();
                Self {
                    id,
                    note: None,
                    state: State::Unreadable,
                    step_count: 0,
                    disk_bytes,
                    stopped_at: None,
                    busy: None,
                    cleanup_pending: false,
                    error: Some(format!("{error}; {MANUAL}")),
                    revert_status: None,
                }
            }
            Ok(snapshot) => {
                let state = snapshot.state(liveness);
                let step_count = snapshot.steps.len();
                let pending = snapshot.unfinished().map(|(index, _)| CurrentStep {
                    index,
                    total: step_count,
                });
                let dir = journal::dir(root, &snapshot.id).expect("decoded window id");
                Self {
                    id: snapshot.id,
                    note: snapshot.note,
                    state,
                    step_count,
                    disk_bytes: content::tree_size(&dir).ok(),
                    stopped_at: if state == State::Interrupted {
                        pending
                    } else {
                        None
                    },
                    busy: if state == State::Open { pending } else { None },
                    cleanup_pending: state == State::Kept
                        && !matches!(paths::info(&dir.join("saved")), Ok(None)),
                    error: None,
                    revert_status: match state {
                        State::Reverted => Some("reverted"),
                        State::RevertInterrupted => Some("stopped_part_way"),
                        _ => None,
                    },
                }
            }
        }
    }

    fn human(&self) -> String {
        let mut state = self.state.to_string();
        if let Some(at) = self.stopped_at {
            state.push_str(&format!(": stopped at step {} of {}", at.index, at.total));
        }
        if let Some(at) = self.busy {
            state.push_str(&format!(": step {} of {} busy", at.index, at.total));
        }
        if self.cleanup_pending {
            state.push_str(", cleanup pending");
        }
        if let Some(error) = &self.error {
            state.push_str(&format!(": {error}"));
        }
        let disk = self.disk_bytes.map_or_else(
            || "disk use unavailable".into(),
            |bytes| format!("{bytes} bytes"),
        );
        let note = self
            .note
            .as_ref()
            .map_or_else(String::new, |note| format!(" {}", dialog_note(note)));
        format!(
            "{}: {state} ({} steps, {disk}){note}",
            self.id, self.step_count
        )
    }
}

/// Notes can be written by automation. Keep invisible directional and formatting
/// characters out of the prompt itself, without hiding ordinary non-ASCII text.
pub(super) fn dialog_note(note: &str) -> String {
    let mut escaped = String::new();
    for ch in json_string(note).chars() {
        if ch.is_control()
            || matches!(ch as u32,
            0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2 |
            0x180E | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB |
            0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
        {
            use std::fmt::Write as _;
            for unit in ch.encode_utf16(&mut [0; 2]) {
                write!(escaped, "\\u{unit:04x}").expect("writing to a string");
            }
        } else {
            escaped.push(ch);
        }
    }
    escaped
}

#[derive(Clone, Debug, Serialize)]
pub struct StepSummary {
    pub index: usize,
    pub kind: &'static str,
    pub target: PathBuf,
}

impl StepSummary {
    pub(super) fn new(index: usize, entry: &Entry) -> Self {
        Self {
            index: index + 1,
            kind: entry.step.action.kind(),
            target: entry.step.target.clone(),
        }
    }
}

#[derive(Debug)]
pub struct Close {
    pub id: String,
    pub outstanding: Vec<StepSummary>,
    pub warnings: Vec<String>,
}

impl Close {
    pub fn into_result(self) -> WindowResult {
        WindowResult {
            window: Some(self.id),
            outstanding: self.outstanding,
            warnings: self.warnings,
            ..WindowResult::default()
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WindowResult {
    schema: &'static str,
    pub exit: i32,
    pub survivors: Vec<Survivor>,
    pub unhealthy: Vec<String>,
    pub not_health_checked: Vec<String>,
    pub restored: Vec<PathBuf>,
    pub marker_removed: bool,
    pub daemon_unreachable: bool,
    pub daemon_past_bound: bool,
    pub relaunched_pid: Option<u32>,
    pub manager_error: Option<String>,
    pub roster_error: Option<RosterError>,
    pub window: Option<String>,
    pub outstanding: Vec<StepSummary>,
    pub message: Option<String>,
    pub warnings: Vec<String>,
}

impl Default for WindowResult {
    fn default() -> Self {
        Self {
            schema: "ck.window.result/v1",
            exit: 0,
            survivors: Vec::new(),
            unhealthy: Vec::new(),
            not_health_checked: Vec::new(),
            restored: Vec::new(),
            marker_removed: false,
            daemon_unreachable: false,
            daemon_past_bound: false,
            relaunched_pid: None,
            manager_error: None,
            roster_error: None,
            window: None,
            outstanding: Vec::new(),
            message: None,
            warnings: Vec::new(),
        }
    }
}

impl From<WindowError> for WindowResult {
    fn from(error: WindowError) -> Self {
        Self {
            exit: error.exit,
            message: Some(error.message),
            ..Self::default()
        }
    }
}
