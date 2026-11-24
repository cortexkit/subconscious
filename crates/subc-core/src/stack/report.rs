use serde::Serialize;
use subc_daemon::{ExecutableIdentity, RecordReadError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerState {
    Loaded,
    Running,
    NotLoaded,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerState {
    Absent,
    Present,
    Stale,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleState {
    Running,
    Stopped,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Healthy,
    Unhealthy,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Source {
    #[serde(rename = "daemon")]
    Daemon,
    #[serde(rename = "manager+roster")]
    ManagerAndRoster,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SurvivorState {
    Alive,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SurvivorReason {
    IdentityUnreadable,
    ExecutableChanged,
    ContainerUnproven,
}

#[derive(Debug, Serialize)]
pub struct Survivor {
    pub name: String,
    pub pid: u32,
    pub start_time: Option<u64>,
    pub executable: Option<ExecutableIdentity>,
    pub state: SurvivorState,
    pub reason: Option<SurvivorReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RosterErrorKind {
    Unreadable,
    Malformed,
    UnknownVersion,
}

#[derive(Debug, Serialize)]
pub struct RosterError {
    pub kind: RosterErrorKind,
    pub version: Option<u32>,
    pub message: String,
}

impl From<RecordReadError> for RosterError {
    fn from(error: RecordReadError) -> Self {
        let (kind, version) = match &error {
            RecordReadError::Io(_) => (RosterErrorKind::Unreadable, None),
            RecordReadError::Malformed(_) => (RosterErrorKind::Malformed, None),
            RecordReadError::UnknownVersion(version) => {
                (RosterErrorKind::UnknownVersion, Some(*version))
            }
        };
        Self {
            kind,
            version,
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ModuleStatus {
    pub name: String,
    pub state: ModuleState,
    pub health: Health,
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StackStatus {
    schema: &'static str,
    pub source: Source,
    pub manager: ManagerState,
    pub stopped_marker: MarkerState,
    pub daemon_pid: Option<u32>,
    pub daemon_start_time: Option<u64>,
    pub daemon_version: String,
    pub roster_error: Option<RosterError>,
    pub manager_error: Option<String>,
    pub modules: Vec<ModuleStatus>,
}

impl StackStatus {
    pub(super) fn new(manager: ManagerState, stopped_marker: MarkerState) -> Self {
        Self {
            schema: "ck.stack.status/v1",
            source: Source::ManagerAndRoster,
            manager,
            stopped_marker,
            daemon_pid: None,
            daemon_start_time: None,
            daemon_version: "unknown".into(),
            roster_error: None,
            manager_error: None,
            modules: Vec::new(),
        }
    }

    pub fn human(&self) -> String {
        let mut lines = vec![format!(
            "Stack status ({})\nManager: {}; stopped marker: {}\nDaemon: pid {}, start time {}, version {}",
            label(self.source), label(self.manager), label(self.stopped_marker),
            self.daemon_pid.map_or_else(|| "unknown".into(), |pid| pid.to_string()),
            self.daemon_start_time.map_or_else(|| "unknown".into(), |time| time.to_string()),
            self.daemon_version,
        )];
        if let Some(error) = &self.manager_error {
            lines.push(format!("Manager query failed: {error}"));
        }
        if let Some(error) = &self.roster_error {
            lines.push(format!("Roster {}: {}", label(error.kind), error.message));
        }
        for module in &self.modules {
            let detail = module
                .detail
                .as_deref()
                .map_or_else(String::new, |detail| format!(" ({detail})"));
            lines.push(format!(
                "{}: {}, health {}{detail}",
                module.name,
                label(module.state),
                label(module.health)
            ));
        }
        lines.join("\n")
    }
}

#[derive(Debug, Serialize)]
pub struct DegradedModule {
    pub name: String,
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StackResult {
    pub(super) schema: &'static str,
    pub exit: u8,
    pub survivors: Vec<Survivor>,
    pub unhealthy: Vec<String>,
    pub not_health_checked: Vec<String>,
    pub degraded: Vec<DegradedModule>,
    pub restored: Vec<String>,
    pub marker_removed: bool,
    pub daemon_unreachable: bool,
    pub daemon_past_bound: bool,
    pub relaunched_pid: Option<u32>,
    pub manager_error: Option<String>,
    pub roster_error: Option<RosterError>,
    pub message: Option<String>,
}

impl Default for StackResult {
    fn default() -> Self {
        Self {
            schema: "ck.stack.result/v1",
            exit: 0,
            survivors: Vec::new(),
            unhealthy: Vec::new(),
            not_health_checked: Vec::new(),
            degraded: Vec::new(),
            restored: Vec::new(),
            marker_removed: false,
            daemon_unreachable: false,
            daemon_past_bound: false,
            relaunched_pid: None,
            manager_error: None,
            roster_error: None,
            message: None,
        }
    }
}

impl StackResult {
    pub fn refused(message: impl Into<String>) -> Self {
        Self {
            exit: 2,
            message: Some(message.into()),
            ..Self::default()
        }
    }

    pub(super) fn note(&mut self, message: impl Into<String>) {
        let message = message.into();
        if let Some(existing) = &mut self.message {
            existing.push('\n');
            existing.push_str(&message);
        } else {
            self.message = Some(message);
        }
    }

    pub fn human(&self, stopping: bool) -> String {
        let mut lines = Vec::new();
        if self.exit == 0 {
            lines.push(
                if stopping {
                    "Stack stopped."
                } else {
                    "Stack started."
                }
                .into(),
            );
        }
        if let Some(message) = &self.message {
            lines.push(message.clone());
        }
        if let Some(error) = &self.manager_error {
            lines.push(format!("Service manager: {error}"));
        }
        if let Some(error) = &self.roster_error {
            lines.push(format!("Roster {}: {}", label(error.kind), error.message));
        }
        for survivor in &self.survivors {
            let executable = survivor
                .executable
                .as_ref()
                .and_then(|identity| serde_json::to_string(identity).ok())
                .unwrap_or_else(|| "unknown".into());
            let reason = survivor
                .reason
                .map_or_else(String::new, |reason| format!(": {}", label(reason)));
            lines.push(format!(
                "{} (pid {}, executable {executable}): {}{reason}",
                survivor.name,
                survivor.pid,
                label(survivor.state)
            ));
        }
        if !self.unhealthy.is_empty() {
            lines.push(format!("Unhealthy: {}", self.unhealthy.join(", ")));
        }
        if !self.not_health_checked.is_empty() {
            lines.push(format!(
                "Running, not health-checked: {}",
                self.not_health_checked.join(", ")
            ));
        }
        for module in &self.degraded {
            let detail = module
                .detail
                .as_deref()
                .map_or_else(String::new, |detail| format!(": {detail}"));
            lines.push(format!("{}: running, degraded{detail}", module.name));
        }
        if self.marker_removed {
            lines.push("Removed the stopped marker.".into());
        }
        if self.daemon_unreachable {
            lines.push("The daemon did not answer supervisor.list by the deadline.".into());
        }
        if self.daemon_past_bound {
            lines.push("The daemon did not stop within its wait bound.".into());
        }
        if stopping {
            lines.push(super::STOP_DURATION.into());
        }
        lines.join("\n")
    }
}

fn label(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}
