//! The config owner serializes pin planning, approved reads and ledger publication.

use std::path::PathBuf;

use serde_json::{json, Value};
use subc_config::{raw_digest, InputState, RecordedValue};
use subc_control::config::{ConfigEdit, ConfigEditAction, ConfigPlanView};
use subc_protocol::policy_root::{
    PolicyRoot, PolicyRootChange, PolicyRootChangeState, PolicyRootPin,
};

use super::{daemon_review, edits, OwnerState, Result, RuntimeOwner, StoreState};
use crate::config_gate::{
    capture::CapturedConfiguration, CapturedInput, GateError, GateSnapshot, InputKey,
};
use crate::policy_root::{ledger, refuse, Change, Move, Section, POINTER, SECTION};

pub(crate) const DAEMON: &str = "subc";

pub(super) struct StampLedger {
    previous: Option<crate::policy_root::Key>,
    candidate: Option<crate::policy_root::Key>,
}

impl RuntimeOwner {
    pub(crate) fn with_policy_root_data(mut self, data: PathBuf) -> Self {
        self.policy_root_ledger = ledger::ledger_path(data);
        *self
            .policy_root_unstamped
            .get_mut()
            .expect("new config owner") = crate::daemon_config::load(&self.capturer.daemon_path)
            .ok()
            .flatten()
            .map_or(Ok(None), |config| config.agent_policy_root);
        self
    }

    pub(super) fn replace_unstamped_root(&self, section: Section) {
        *self
            .policy_root_unstamped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = section;
    }

    pub(super) fn unstamped_root(&self) -> Section {
        self.policy_root_unstamped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn rescan_policy_root(&self, section: Section) -> Result<()> {
        let mut state = self.lock()?;
        if state.store.status().state == StoreState::Unstamped {
            self.replace_unstamped_root(section);
            if let Some(problem) = self.root_view(&mut state)?.1 {
                tracing::warn!(%problem, "daemon configuration problem");
            }
        }
        Ok(())
    }

    fn root_view(&self, state: &mut OwnerState) -> Result<(PolicyRoot, Option<String>)> {
        let (section, generation, checkpoint) = match state.store.status().state {
            StoreState::Unstamped => (self.unstamped_root(), 0, None),
            StoreState::Stamped if !state.recovery_exit_hold => {
                state.store.delivery_ready()?;
                let body = state.store.body()?;
                let generation = super::placement::last_approved_generation(body);
                let delivery = body
                    .generations
                    .get(&generation)
                    .and_then(|entry| entry.delivery.as_ref())
                    .ok_or_else(GateError::stale)?;
                (
                    super::daemon_readers::parse_daemon(delivery)?.agent_policy_root,
                    generation,
                    ledger::last_checkpoint(body).map(|(_, value)| value.clone()),
                )
            }
            _ => (Ok(None), 0, None),
        };
        let (pin, problem) = self.resolve_section(section, checkpoint.as_ref());
        Ok((PolicyRoot { pin, generation }, problem))
    }

    pub(crate) fn policy_root_problem(
        &self,
        published: &super::ApprovedDaemonView,
    ) -> Option<String> {
        // Status must not join the owner while an apply holds it. The daemon
        // snapshot is already published, and the ledger is replaced atomically.
        let (section, checkpoint) = match published {
            super::ApprovedDaemonView::NotStamped => (self.unstamped_root(), None),
            super::ApprovedDaemonView::Approved {
                config,
                policy_root_checkpoint,
                ..
            } => (
                config.agent_policy_root.clone(),
                policy_root_checkpoint.as_ref(),
            ),
            super::ApprovedDaemonView::Unavailable => return None,
        };
        self.resolve_section(section, checkpoint).1
    }

    fn resolve_section(
        &self,
        section: Section,
        checkpoint: Option<&Value>,
    ) -> (Option<PolicyRootPin>, Option<String>) {
        let ledger = if checkpoint.is_some() || matches!(section, Ok(Some(_))) {
            match self.read_root_ledger(checkpoint) {
                Ok(ledger) => ledger,
                Err(error) => return (None, Some(error.detail)),
            }
        } else {
            None
        };
        let mut problem = None;
        let pin = match section {
            Ok(None) => None,
            Err(code) => {
                problem = Some(format!("{SECTION}: {}", refuse(code).detail));
                None
            }
            Ok(Some(key)) => match ledger {
                Some(ledger) if !ledger.contains(&key.key_id) => Some(key.pin(ledger.retired())),
                Some(ledger) => {
                    let destination = ledger
                        .interrupted_destination(&key.key_id)
                        .and_then(subc_protocol::policy_root::decode_public_key)
                        .map(|raw| subc_protocol::policy_root::fingerprint(&raw));
                    problem = Some(match destination {
                        Some(fingerprint) => {
                            format!("{SECTION}: pin move to {fingerprint} interrupted")
                        }
                        None => format!("{SECTION}: the configured key has already left the pin"),
                    });
                    None
                }
                _ => {
                    problem = Some(ledger::problem().detail);
                    None
                }
            },
        };
        (pin, problem)
    }

    pub(crate) fn read_policy_root(&self) -> Result<(PolicyRoot, Option<String>)> {
        let mut state = self.lock()?;
        Self::check_authority(&mut state, false)?;
        self.root_view(&mut state)
    }

    fn read_root_ledger(&self, checkpoint: Option<&Value>) -> Result<Option<ledger::Ledger>> {
        let checkpoint = parse_checkpoint(checkpoint)?;
        ledger::Ledger::read_checked(
            self.policy_root_ledger
                .as_deref()
                .ok_or_else(ledger::problem)?,
            checkpoint.as_ref(),
        )
    }

    pub(super) fn check_policy_root_move(
        &self,
        state: &OwnerState,
        baseline: &CapturedConfiguration,
        movement: Move,
        unstamped: bool,
    ) -> Result<Change> {
        Self::no_pending_policy_root(state)?;
        let section = captured_section(baseline)?;
        if unstamped && section != self.unstamped_root() {
            return Err(GateError::new(
                "config_changed_since_review",
                "rescan subc.jsonc before moving the policy root",
            ));
        }
        let current = section.ok().flatten();
        let setting = matches!(movement, Move::Set(_));
        let (_, body) = state.store.loaded_admission_state();
        let checkpoint = (!unstamped)
            .then(|| ledger::last_checkpoint(body))
            .flatten()
            .map(|(_, value)| value);
        let ledger = self.read_root_ledger(checkpoint)?;
        if ledger.is_none() {
            let path = self
                .policy_root_ledger
                .as_deref()
                .ok_or_else(ledger::problem)?;
            if current.is_some()
                || (setting
                    && (ledger::directory_exists(path)? || (!unstamped && has_pin_history(body))))
            {
                return Err(ledger::problem());
            }
        }
        let ledger = ledger.unwrap_or_default();
        let departing = current
            .as_ref()
            .is_some_and(|key| ledger.contains(&key.key_id));
        if setting && departing {
            if current
                .as_ref()
                .is_some_and(|key| ledger.interrupted_destination(&key.key_id).is_some())
            {
                return Err(refuse("policy_root_key_reused"));
            }
            return movement.check(None, &ledger);
        }
        if departing
            && current
                .as_ref()
                .is_some_and(|key| ledger.interrupted_destination(&key.key_id).is_none())
        {
            return Err(refuse("policy_root_absent"));
        }
        let change = movement.check(current.as_ref(), &ledger)?;
        if departing && !ledger.matches_interrupted(&change) {
            return Err(refuse("policy_root_key_reused"));
        }
        Ok(change)
    }

    pub(super) fn no_pending_policy_root(state: &OwnerState) -> Result<()> {
        if state.gate.plans.values().any(|plan| {
            plan.edits
                .as_ref()
                .is_some_and(|edit| edit.policy_root.is_some())
        }) {
            return Err(refuse("policy_root_change_pending"));
        }
        Ok(())
    }

    pub(crate) fn plan_policy_root(&self, movement: Move) -> Result<PolicyRootChange> {
        let prepared = self.prepare_policy_root_edit(movement)?;
        let change = prepared
            .edits
            .as_ref()
            .and_then(|edit| edit.policy_root.as_ref())
            .expect("policy root edit")
            .clone();
        let ConfigPlanView { digest, .. } = self.finish_plan(prepared, |old, new| {
            let mut changeset = daemon_review(old, new)?;
            changeset.policy_changes =
                vec![json!({"path": POINTER, "summary": change.review, "change": change})];
            Ok(changeset)
        })?;
        Ok(PolicyRootChange {
            digest,
            state: PolicyRootChangeState::Pending,
        })
    }

    pub(crate) fn unstamped_policy_root_review(
        &self,
        digest: &subc_config::Digest,
    ) -> Result<Option<String>> {
        let state = self.lock()?;
        Ok(state
            .gate
            .plans
            .get(digest)
            .and_then(|plan| plan.edits.as_ref())
            .filter(|edit| edit.unstamped)
            .and_then(|edit| edit.policy_root.as_ref())
            .map(|change| change.review.clone()))
    }

    pub(super) fn publish_policy_root_ledger(
        &self,
        change: &Change,
        now_ms: u64,
        checkpoint: Option<&Value>,
        require_existing: bool,
    ) -> Result<ledger::Checkpoint> {
        let path = self
            .policy_root_ledger
            .as_deref()
            .ok_or_else(ledger::problem)?;
        // The old key fails closed if publication stops after this step. The
        // opposite order could expose the new pin with no record of its predecessor.
        let checkpoint = parse_checkpoint(checkpoint)?;
        let next =
            ledger::Ledger::apply(path, change, now_ms, checkpoint.as_ref(), require_existing)?;
        #[cfg(test)]
        if change.previous.is_some()
            && self
                .policy_root_fail_after_append
                .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(GateError::new(
                "publication_failed",
                "publication interrupted after the ledger append",
            ));
        }
        Ok(next)
    }

    pub(super) fn stamp_ledger_intent(
        &self,
        body: &crate::config_store::StoreBody,
        plan: &crate::config_gate::PendingPlan,
    ) -> Option<StampLedger> {
        if plan.kind != super::PlanKind::Stamp && !plan.recovery {
            return None;
        }
        // An Unstamp hands authority back to disk but does not erase the last
        // approved stamped key. A cold recovery has no trusted history to use.
        let trusted = body.generations.values().rev().find(|entry| {
            matches!(
                entry.kind,
                crate::config_store::CommitKind::Stamp
                    | crate::config_store::CommitKind::Approved
                    | crate::config_store::CommitKind::Rollback
            )
        });
        let previous = trusted
            .and_then(generation_section)
            .cloned()
            .and_then(|raw| crate::policy_root::parse_section(Some(raw)).ok().flatten())
            .or_else(|| {
                (!plan.recovery)
                    .then(|| self.unstamped_root().ok().flatten())
                    .flatten()
            });
        let candidate = crate::policy_root::parse_section(section_value(&plan.snapshot).cloned())
            .ok()
            .flatten();
        Some(StampLedger {
            previous,
            candidate,
        })
    }

    pub(super) fn publish_stamp_ledger(
        &self,
        intent: StampLedger,
        now_ms: u64,
        checkpoint: Option<&Value>,
    ) -> Result<Option<ledger::Checkpoint>> {
        let previous = intent
            .previous
            .filter(|previous| intent.candidate.as_ref() != Some(previous));
        match self.read_root_ledger(checkpoint) {
            Ok(Some(mut ledger)) => {
                if let Some(previous) = previous {
                    let path = self
                        .policy_root_ledger
                        .as_deref()
                        .ok_or_else(ledger::problem)?;
                    ledger.retire_for_stamp(path, previous.key_id, now_ms)?;
                }
                Ok(Some(ledger.checkpoint()?))
            }
            Err(error) if checkpoint.is_some() && previous.is_some() => Err(error),
            // Missing or untrusted ledger bytes authorize no pin. A recovery
            // can still publish the daemon settings without trusting them.
            _ => parse_checkpoint(checkpoint),
        }
    }

    pub(super) fn unstamped_root_matches(&self, change: &Change) -> Result<bool> {
        let current = self.unstamped_root().ok().flatten();
        match (current, change.previous.as_ref()) {
            (Some(current), Some(previous)) => Ok(current.key_id == *previous),
            (None, None) => Ok(true),
            (Some(current), None) => Ok(self.read_root_ledger(None)?.is_some_and(|ledger| {
                ledger.contains(&current.key_id)
                    && ledger.interrupted_destination(&current.key_id).is_none()
            })),
            (None, Some(_)) => Ok(false),
        }
    }
}

fn generation_section(entry: &crate::config_store::HistoryEntry) -> Option<&Value> {
    let RecordedValue::Value { value } = entry.configuration.get("\"Daemon\"")? else {
        return None;
    };
    value.pointer("/value/value/agent_policy_root")
}

pub(super) fn has_pin_history(body: &crate::config_store::StoreBody) -> bool {
    body.generations
        .values()
        .any(|entry| generation_section(entry).is_some())
}

fn parse_checkpoint(value: Option<&Value>) -> Result<Option<ledger::Checkpoint>> {
    value
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| ledger::problem())
}

pub(super) fn captured_section(captured: &CapturedConfiguration) -> Result<Section> {
    let bytes = captured
        .daemon
        .bytes
        .as_deref()
        .ok_or_else(|| GateError::new("invalid_daemon_config", "subc.jsonc is absent"))?;
    crate::daemon_config::parse_with_activity(
        std::str::from_utf8(bytes).map_err(GateError::invalid)?,
        &captured.daemon.path,
    )
    .map(|(config, _)| config.agent_policy_root)
    .map_err(GateError::invalid)
}

pub(super) fn edit_for(value: Option<Value>) -> ConfigEdit {
    let action = match value {
        Some(set) => ConfigEditAction::Set { set },
        None => ConfigEditAction::Remove { remove: true },
    };
    ConfigEdit {
        module_id: DAEMON.into(),
        layer: "user".into(),
        path: POINTER.into(),
        workspace_id: None,
        project_id: None,
        action,
    }
}

pub(super) fn patch_daemon(candidate: &mut CapturedConfiguration, edit: ConfigEdit) -> Result<()> {
    let captured = candidate
        .diagnostic
        .inputs
        .get_mut(&InputKey::Daemon)
        .ok_or_else(GateError::stale)?;
    let bytes = patch_captured_daemon(captured, candidate.daemon.bytes.as_deref(), edit)?;
    candidate.daemon.state = captured.state.clone();
    candidate.daemon.bytes = Some(bytes);
    Ok(())
}

fn patch_captured_daemon(
    captured: &mut CapturedInput,
    original: Option<&[u8]>,
    edit: ConfigEdit,
) -> Result<Vec<u8>> {
    let bytes = edits::edit_text(original, std::slice::from_ref(&edit))?;
    if original == Some(bytes.as_slice()) {
        return Ok(bytes);
    }
    let RecordedValue::Value { value } = &mut captured.value else {
        return Err(GateError::stale());
    };
    let doc = value.as_object_mut().ok_or_else(GateError::stale)?;
    match edit.action {
        ConfigEditAction::Set { set } => {
            doc.insert(SECTION.into(), set);
        }
        ConfigEditAction::Remove { .. } => {
            doc.remove(SECTION);
        }
    }
    captured.state = InputState::Present {
        digest: raw_digest(&bytes),
    };
    Ok(bytes)
}

pub(in crate::config_gate) fn section_value(snapshot: &GateSnapshot) -> Option<&Value> {
    let RecordedValue::Value { value } = &snapshot.inputs.get(&InputKey::Daemon)?.value else {
        return None;
    };
    value.get(SECTION)
}

pub(in crate::config_gate) fn preserve_root_on_rollback(
    body: &crate::config_store::StoreBody,
    generation: u64,
    target: &mut GateSnapshot,
) -> Result<()> {
    let approved = GateSnapshot::from_store(&body.approved)?;
    let root = section_value(&approved);
    if section_value(target) == root {
        return Ok(());
    }
    let original = body
        .generations
        .get(&generation)
        .and_then(|entry| entry.delivery.as_ref())
        .and_then(|delivery| delivery.daemon.as_ref())
        .and_then(|layer| layer.bytes.as_deref())
        .ok_or_else(GateError::stale)?;
    let daemon = target
        .inputs
        .get_mut(&InputKey::Daemon)
        .ok_or_else(GateError::stale)?;
    patch_captured_daemon(daemon, Some(original), edit_for(root.cloned()))?;
    Ok(())
}
