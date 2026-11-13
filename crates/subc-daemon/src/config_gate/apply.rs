//! Reduce a module's field policies to one whole-generation transition.

use std::collections::BTreeMap;

use serde::Serialize;
use subc_config::{ApplyKind, Changeset};

use super::{GateError, Result};

/// Declaration order is the increasing transition strength. Every stronger
/// transition must still perform the route/readiness work of the weaker ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Transition {
    Live,
    RouteRebind,
    NextSupervision,
    ModuleRestart,
    DaemonRestart,
}

impl Transition {
    fn from_apply(kind: ApplyKind) -> Result<Self> {
        match kind {
            ApplyKind::Live => Ok(Self::Live),
            ApplyKind::RouteRebind => Ok(Self::RouteRebind),
            ApplyKind::NextSupervision => Ok(Self::NextSupervision),
            ApplyKind::ModuleRestart => Ok(Self::ModuleRestart),
            ApplyKind::DaemonRestart => Ok(Self::DaemonRestart),
            _ => Err(GateError::new(
                "apply_kind_unsupported",
                "the daemon cannot enforce this field's apply policy",
            )),
        }
    }

    pub(crate) fn for_module(changeset: &Changeset, module: &str) -> Result<Self> {
        let floor = if changes_program(changeset, module) {
            Self::ModuleRestart
        } else {
            Self::Live
        };
        changeset
            .fields
            .iter()
            .filter(|field| field.module_id == module)
            .try_fold(floor, |current, field| {
                Ok(current.max(Self::from_apply(field.apply)?))
            })
    }

    pub(crate) fn requires_spawn(self) -> bool {
        self >= Self::NextSupervision
    }
}

/// Whether an ordinary change or rollback replaces the module's approved
/// program. The process already running was launched from the previous program,
/// so it must not acknowledge this generation: only a replacement child started
/// from the newly approved program, after warm-up, can make it active.
///
/// The daemon reviewer names each changed program file `program:<path>` in the
/// module's launch manifest, and a rollback marks a program that differs from
/// its target with `program_changed_since_target`. A stamp or unstamp records
/// the programs already running rather than replacing them, so it is excluded.
fn changes_program(changeset: &Changeset, module: &str) -> bool {
    let replaces_programs = matches!(
        changeset
            .apply_plan
            .get("operation")
            .and_then(serde_json::Value::as_str),
        Some("change" | "rollback")
    );
    replaces_programs
        && changeset
            .launch_manifests
            .get(module)
            .and_then(serde_json::Value::as_object)
            .is_some_and(|manifest| {
                manifest.keys().any(|key| {
                    key.starts_with("program:") || key == "program_changed_since_target"
                })
            })
}

/// The resulting transition is approval input, not an execution-time guess.
/// Preserve the existing apply plan alongside the module transition table.
pub(super) fn bind_plan(changeset: &mut Changeset) -> Result<()> {
    let mut policies: BTreeMap<&str, (Transition, Vec<String>)> = BTreeMap::new();
    for field in &changeset.fields {
        let (kind, live_fields) = policies
            .entry(&field.module_id)
            .or_insert((Transition::Live, vec![]));
        *kind = (*kind).max(Transition::from_apply(field.apply)?);
        if field.apply == ApplyKind::Live {
            live_fields.push(field.path.clone());
        }
    }
    for module in changeset.launch_manifests.keys() {
        if changes_program(changeset, module) {
            let (kind, _) = policies
                .entry(module.as_str())
                .or_insert((Transition::Live, vec![]));
            *kind = (*kind).max(Transition::ModuleRestart);
        }
    }
    let mut transitions = BTreeMap::new();
    for (module, (kind, live_fields)) in policies {
        let waits_for_next_start = kind == Transition::NextSupervision;
        let deferred_live_fields = if waits_for_next_start {
            live_fields
        } else {
            vec![]
        };
        transitions.insert(module, {
            serde_json::json!({
                "transition":kind,
                "whole_generation":true,
                "waits_for_next_start":waits_for_next_start,
                "waits_for_daemon_restart":kind == Transition::DaemonRestart,
                "deferred_live_fields":deferred_live_fields,
                "explanation":if kind == Transition::DaemonRestart {
                    "The complete generation takes effect when the daemon next restarts. Approval does not restart the daemon or interrupt running modules."
                } else if waits_for_next_start {
                    "The complete generation waits for the module's next start; fields that would apply live alone do not activate early."
                } else {
                    "The complete generation activates only after every required transition and readiness check finishes."
                },
            })
        });
    }
    let mut plan = match std::mem::take(&mut changeset.apply_plan) {
        serde_json::Value::Object(plan) => plan,
        serde_json::Value::Null => serde_json::Map::new(),
        review => serde_json::Map::from_iter([("review".into(), review)]),
    };
    plan.insert(
        "module_transitions".into(),
        serde_json::to_value(transitions).map_err(GateError::invalid)?,
    );
    changeset.apply_plan = serde_json::Value::Object(plan);
    Ok(())
}
