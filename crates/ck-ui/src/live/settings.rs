//! Read only the file the daemon reports, and only with candidate-preview support.
//! Also the shapes of a candidate preview's request and reply, which the
//! Save overlay uses to show what saving would do.

use std::path::Path;

use subc_client_rs::CallError;
use subc_control::SupervisorRescanResult;

pub const CAPABILITY: &str = "rescan-candidate-preview/v1";

/// The refusal code the daemon gives a candidate it can't load. Its message
/// says what is wrong with the text, so the overlay shows it as it is.
pub const INVALID_CONFIG: &str = "invalid_daemon_config";

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
