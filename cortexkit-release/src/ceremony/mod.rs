//! Explicit operator ceremonies for declaration changes on durable release trains.
//!
//! Resume admission reads the journal before it lets callers acquire credentials or
//! execute phases. A changed declaration is therefore an operator decision rather
//! than a value an ordinary resume can silently adopt.

use crate::{
    approval::ApprovalStore,
    declaration::ParsedDeclaration,
    state::{DeclarationBinding, JournalStore, StateError, TrainTerminalState},
    DeclarationDigest,
};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};
use thiserror::Error;

/// Stable, machine-readable refusal codes for operator ceremonies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CeremonyRefusalCode {
    DeclarationDigestMismatch,
    DeclarationNotPinned,
    TrainAbandoned,
    RebindConfirmationRequired,
    RebindNotRequired,
    RebindPreviewStale,
}

impl CeremonyRefusalCode {
    /// Returns the stable serialization-safe spelling of this refusal code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeclarationDigestMismatch => "declaration_digest_mismatch",
            Self::DeclarationNotPinned => "declaration_not_pinned",
            Self::TrainAbandoned => "train_abandoned",
            Self::RebindConfirmationRequired => "rebind_confirmation_required",
            Self::RebindNotRequired => "rebind_not_required",
            Self::RebindPreviewStale => "rebind_preview_stale",
        }
    }
}

impl fmt::Display for CeremonyRefusalCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A typed refusal with an operator-facing explanation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CeremonyRefusal {
    pub code: CeremonyRefusalCode,
    pub train_journal_id: String,
    pub message: String,
}

impl fmt::Display for CeremonyRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

/// Errors emitted before or during a declaration-change ceremony.
#[derive(Debug, Error)]
pub enum CeremonyError {
    #[error("{0}")]
    Refusal(CeremonyRefusal),
    #[error("durable train state failed: {0}")]
    State(#[from] StateError),
    #[error("approval state failed: {0}")]
    Approval(#[from] crate::approval::ApprovalError),
}

/// A single structural difference between the prior and replacement declarations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclarationDiffEntry {
    /// JSON Pointer location of the changed declaration value.
    pub path: String,
    /// The value pinned by the train, or `None` when the replacement added it.
    pub pinned: Option<Value>,
    /// The active replacement value, or `None` when it was removed.
    pub replacement: Option<Value>,
}

/// An operator-visible declaration comparison that must be confirmed before rebinding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebindPreview {
    pub train_journal_id: String,
    pub pinned_digest: DeclarationDigest,
    pub replacement_digest: DeclarationDigest,
    pub differences: Vec<DeclarationDiffEntry>,
    pinned_normalized: Value,
    replacement_declaration: ParsedDeclaration,
}

impl RebindPreview {
    /// Renders the structural diff so a command-line caller can show it before confirmation.
    pub fn render_diff(&self) -> String {
        let mut output = format!(
            "--- pinned declaration ({})\n+++ active declaration ({})\n",
            self.pinned_digest, self.replacement_digest
        );
        for difference in &self.differences {
            output.push_str(&format!("@@ {} @@\n", difference.path));
            if let Some(pinned) = &difference.pinned {
                output.push_str(&format!("- {}\n", render_value(pinned)));
            }
            if let Some(replacement) = &difference.replacement {
                output.push_str(&format!("+ {}\n", render_value(replacement)));
            }
        }
        output
    }
}

/// An explicit response to the rebind confirmation prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebindConfirmation {
    Confirmed,
    Declined,
}

/// The durable result of a confirmed declaration rebind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebindOutcome {
    pub binding: DeclarationBinding,
    pub invalidated_approval: bool,
    /// A new plan must construct and receive a fresh approval before any public effect.
    pub approval_reconstruction_required: bool,
}

/// Checks resume preconditions before the caller may acquire credentials or run phases.
///
/// The function is intentionally separate from phase orchestration: callers invoke it
/// immediately after loading the declaration, while this operation is still limited to
/// local durable state. A mismatch produces recovery guidance instead of invoking a
/// provider, an executor, or an approval gate.
pub fn admit_resume(
    journal: &JournalStore,
    active_declaration: &ParsedDeclaration,
) -> Result<(), CeremonyError> {
    if let Some(terminal) = journal.terminal_state()? {
        return Err(terminal_refusal(journal, terminal));
    }
    let binding = journal.pinned_declaration()?.ok_or_else(|| {
        refusal(
            journal,
            CeremonyRefusalCode::DeclarationNotPinned,
            "the train has no pinned declaration",
        )
    })?;
    if binding.digest != active_declaration.digest {
        return Err(mismatch_refusal(
            journal,
            &binding.digest,
            &active_declaration.digest,
        ));
    }
    Ok(())
}

/// Admits resume, then invokes the continuation that may acquire credentials and execute phases.
///
/// A caller supplies the continuation so a declaration mismatch prevents any downstream
/// provider access by construction.
pub fn resume<T>(
    journal: &JournalStore,
    active_declaration: &ParsedDeclaration,
    continuation: impl FnOnce() -> Result<T, CeremonyError>,
) -> Result<T, CeremonyError> {
    admit_resume(journal, active_declaration)?;
    continuation()
}

/// Terminalizes a train journal while preserving every earlier journal record as evidence.
pub fn abandon(journal: &JournalStore) -> Result<TrainTerminalState, CeremonyError> {
    if let Some(terminal) = journal.terminal_state()? {
        return Err(terminal_refusal(journal, terminal));
    }
    journal.abandon().map_err(CeremonyError::State)
}

/// Loads the pinned declaration and calculates the replacement diff for operator display.
pub fn prepare_rebind(
    journal: &JournalStore,
    active_declaration: &ParsedDeclaration,
) -> Result<RebindPreview, CeremonyError> {
    if let Some(terminal) = journal.terminal_state()? {
        return Err(terminal_refusal(journal, terminal));
    }
    let binding = journal.pinned_declaration()?.ok_or_else(|| {
        refusal(
            journal,
            CeremonyRefusalCode::DeclarationNotPinned,
            "the train has no pinned declaration",
        )
    })?;
    if binding.digest == active_declaration.digest {
        return Err(refusal(
            journal,
            CeremonyRefusalCode::RebindNotRequired,
            "the active declaration already matches the pinned digest",
        ));
    }

    let mut differences = Vec::new();
    collect_differences(
        "",
        Some(&binding.normalized),
        Some(&active_declaration.normalized),
        &mut differences,
    );
    Ok(RebindPreview {
        train_journal_id: journal.train_journal_id(),
        pinned_digest: binding.digest,
        replacement_digest: active_declaration.digest.clone(),
        differences,
        pinned_normalized: binding.normalized,
        replacement_declaration: active_declaration.clone(),
    })
}

/// Durably replaces the pinned declaration after an explicit positive confirmation.
///
/// The old approval is removed before the replacement journal record is appended. If
/// durable journal writing then fails, the original declaration remains pinned but no
/// approval can be reused; callers must reconstruct and confirm a fresh subject.
pub fn confirm_rebind(
    journal: &JournalStore,
    approvals: &ApprovalStore,
    preview: RebindPreview,
    confirmation: RebindConfirmation,
) -> Result<RebindOutcome, CeremonyError> {
    if confirmation != RebindConfirmation::Confirmed {
        return Err(refusal(
            journal,
            CeremonyRefusalCode::RebindConfirmationRequired,
            "the declaration diff was not explicitly confirmed; no durable state changed",
        ));
    }
    if let Some(terminal) = journal.terminal_state()? {
        return Err(terminal_refusal(journal, terminal));
    }
    let current = journal.pinned_declaration()?.ok_or_else(|| {
        refusal(
            journal,
            CeremonyRefusalCode::DeclarationNotPinned,
            "the train has no pinned declaration",
        )
    })?;
    if current.digest != preview.pinned_digest || current.normalized != preview.pinned_normalized {
        return Err(refusal(
            journal,
            CeremonyRefusalCode::RebindPreviewStale,
            "the pinned declaration changed after the diff was displayed; display a new diff before rebinding",
        ));
    }

    let invalidated_approval = approvals.invalidate()?;
    let binding =
        journal.rebind_declaration(&preview.pinned_digest, &preview.replacement_declaration)?;
    Ok(RebindOutcome {
        binding,
        invalidated_approval,
        approval_reconstruction_required: true,
    })
}

fn mismatch_refusal(
    journal: &JournalStore,
    pinned_digest: &DeclarationDigest,
    active_digest: &DeclarationDigest,
) -> CeremonyError {
    let train_journal_id = journal.train_journal_id();
    refusal(
        journal,
        CeremonyRefusalCode::DeclarationDigestMismatch,
        format!(
            "active declaration digest `{active_digest}` differs from pinned digest `{pinned_digest}`; run `ck-release abandon {train_journal_id}` to terminalize this journal or `ck-release rebind {train_journal_id}` to display the declaration diff, confirm it, and pin the replacement digest"
        ),
    )
}

fn terminal_refusal(journal: &JournalStore, terminal: TrainTerminalState) -> CeremonyError {
    refusal(
        journal,
        CeremonyRefusalCode::TrainAbandoned,
        format!("this train journal is terminal ({terminal}) and cannot be resumed or rebound"),
    )
}

fn refusal(
    journal: &JournalStore,
    code: CeremonyRefusalCode,
    message: impl Into<String>,
) -> CeremonyError {
    CeremonyError::Refusal(CeremonyRefusal {
        code,
        train_journal_id: journal.train_journal_id(),
        message: message.into(),
    })
}

fn collect_differences(
    path: &str,
    pinned: Option<&Value>,
    replacement: Option<&Value>,
    differences: &mut Vec<DeclarationDiffEntry>,
) {
    match (pinned, replacement) {
        (Some(Value::Object(pinned)), Some(Value::Object(replacement))) => {
            let keys = pinned
                .keys()
                .chain(replacement.keys())
                .collect::<BTreeSet<_>>();
            for key in keys {
                let path = format!("{path}/{}", escape_json_pointer(key));
                collect_differences(&path, pinned.get(key), replacement.get(key), differences);
            }
        }
        (Some(Value::Array(pinned)), Some(Value::Array(replacement))) => {
            let length = pinned.len().max(replacement.len());
            for index in 0..length {
                collect_differences(
                    &format!("{path}/{index}"),
                    pinned.get(index),
                    replacement.get(index),
                    differences,
                );
            }
        }
        (pinned, replacement) if pinned != replacement => differences.push(DeclarationDiffEntry {
            path: if path.is_empty() {
                "/".to_owned()
            } else {
                path.to_owned()
            },
            pinned: pinned.cloned(),
            replacement: replacement.cloned(),
        }),
        _ => {}
    }
}

fn escape_json_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn render_value(value: &Value) -> String {
    serde_json::to_string(value).expect("JSON values always serialize")
}

#[cfg(test)]
#[path = "../../tests/ceremonies/mod.rs"]
mod tests;
