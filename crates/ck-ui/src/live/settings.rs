//! Read only the file the daemon reports, and only with candidate-preview support.
//! Also the shapes of a candidate preview's request and reply, which the
//! Save overlay uses to show what saving would do.

use std::path::Path;

use std::time::Duration;
use subc_client_rs::CallError;
use subc_control::SupervisorEntry;
use subc_control::SupervisorRescanResult;

pub const CAPABILITY: &str = "rescan-candidate-preview/v1";

/// The refusal code the daemon gives a candidate it can't load. Its message
/// says what is wrong with the text, so the overlay shows it as it is.
pub const INVALID_CONFIG: &str = "invalid_daemon_config";

/// Rescans drain modules sequentially. Include every attested budget, the
/// daemon's 30-second fallback for older entries, and teardown/scheduling time;
/// keep the ordinary control window even when the fleet has no drains.
pub fn fleet_drain_response_timeout(modules: &[SupervisorEntry]) -> Duration {
    let budget = modules
        .iter()
        .map(|m| Duration::from_millis(m.drain_timeout_ms.unwrap_or(30_000)))
        .fold(Duration::ZERO, Duration::saturating_add);
    Duration::from_secs(10).max(budget.saturating_add(Duration::from_secs(2)))
}

#[derive(Clone, Debug, PartialEq)]
pub enum ApplyOutcome {
    Ready(Box<SupervisorRescanResult>),
    Refused(String),
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApplyReply {
    pub revision: u64,
    pub restarts: Vec<String>,
    pub outcome: ApplyOutcome,
}

pub fn apply_outcome(result: Result<SupervisorRescanResult, CallError>) -> ApplyOutcome {
    match result {
        Ok(result) if !result.preview => ApplyOutcome::Ready(Box::new(result)),
        Err(CallError::Module(body)) => ApplyOutcome::Refused(body.code),
        // Without a valid apply acknowledgement, replay could apply twice.
        _ => ApplyOutcome::Unknown,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsReading {
    pub config_path: Option<String>,
    pub capable: bool,
    pub text: Option<String>,
    pub error: Option<String>,
}

impl SettingsReading {
    pub fn read(config_path: Option<String>, capabilities: &[String]) -> Self {
        let mut reading = Self {
            config_path,
            capable: capabilities.iter().any(|c| c == CAPABILITY),
            ..Self::default()
        };
        if reading.editable() {
            let path = Path::new(reading.config_path.as_deref().unwrap());
            match subc_jsonc::read_document(path) {
                Ok((text, _)) => reading.text = Some(text),
                Err(error) => reading.error = Some(error),
            }
        }
        reading
    }

    pub fn editable(&self) -> bool {
        self.capable
            && self
                .config_path
                .as_deref()
                .is_some_and(|p| Path::new(p).is_absolute())
    }
}

/// Names one preview request: the candidate revision it was sent for, and a
/// number no other request shares. A reply counts only while both still
/// match what the editor waits for, so a reply for an older revision, or one
/// sent before a reconnect, is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewTicket {
    pub revision: u64,
    pub request: u64,
}

/// What a `supervisor.rescan_checked` preview of one candidate said.
#[derive(Clone, Debug, PartialEq)]
pub enum PreviewOutcome {
    /// What saving the candidate would do, with the `expect` value an apply
    /// must send. Boxed because the result is much larger than the other
    /// variants.
    Ready(Box<SupervisorRescanResult>),
    /// The daemon can't load the candidate; its message, as it gave it.
    Refused(String),
    /// The preview failed for another reason; the code says which.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreviewReply {
    pub ticket: PreviewTicket,
    pub outcome: PreviewOutcome,
}

/// Reads a preview call's result. Only a reply that says it is a preview
/// and carries the value an apply needs counts as ready: anything else
/// could describe a rescan that already ran, or leave nothing to apply with.
pub fn preview_outcome(result: Result<SupervisorRescanResult, CallError>) -> PreviewOutcome {
    match result {
        Ok(result) if !result.preview => PreviewOutcome::Failed("preview_not_honoured".into()),
        Ok(result) if result.expect.is_none() => {
            PreviewOutcome::Failed("preview_without_expect".into())
        }
        Ok(result) => PreviewOutcome::Ready(Box::new(result)),
        Err(CallError::Module(body)) if body.code == INVALID_CONFIG => {
            PreviewOutcome::Refused(body.message)
        }
        Err(CallError::Module(body)) => PreviewOutcome::Failed(body.code),
        // The request never left, or no answer came back before the call's
        // deadline or the connection ended.
        Err(CallError::NotSent(_)) => PreviewOutcome::Failed("daemon_unreachable".into()),
        Err(CallError::OutcomeUnknown(_)) => PreviewOutcome::Failed("no_reply".into()),
        Err(e) => PreviewOutcome::Failed(e.code().unwrap_or("failed").to_string()),
    }
}
