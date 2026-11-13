//! Which program build reviewed each module's approved configuration.
//!
//! A module's `config.review` answer is valid only for the build that gave it.
//! Approving a new program for a module is a launch-only approval: the new
//! build cannot run before that approval, and the build still running is not
//! the one being approved. Every plan therefore records, per module, the
//! program identity of the build its approved configuration was reviewed by.
//! Once a newly approved program is active, the next plan re-reviews the
//! module's approved layers with that build and records it. Until then the new
//! build runs with the configuration approved last, and status names the
//! module as waiting for re-review.

use std::collections::BTreeMap;

use super::{program::approved_base, GateError, GateSnapshot, Result};
use crate::config_store::{CommitKind, StoreBody};

/// Key in a generation's `apply_plan`: module id to the program identity (see
/// [`approved_base`]) of the build that reviewed that module's configuration.
pub(super) const REVIEWED_PROGRAMS: &str = "reviewed_programs";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rereview {
    /// The approved program has not yet become active through a child started
    /// from it, so no running process is the build to review with.
    WaitingForProgram,
    /// The approved program is active; the next plan reviews with it.
    Ready,
}

pub(super) fn identity(snapshot: &GateSnapshot, module: &str) -> String {
    approved_base(snapshot, module).as_str().to_owned()
}

/// Program identities recorded by the newest approved generation. A generation
/// planned before this record existed counts each of its modules as reviewed
/// by the programs that generation approved.
pub(super) fn recorded(body: &StoreBody) -> Result<BTreeMap<String, String>> {
    let Some(entry) = body
        .generations
        .values()
        .rev()
        .find(|entry| entry.kind != CommitKind::Development)
    else {
        return Ok(BTreeMap::new());
    };
    if let Some(record) = entry.generation.changeset.apply_plan.get(REVIEWED_PROGRAMS) {
        return serde_json::from_value(record.clone()).map_err(GateError::invalid);
    }
    let snapshot = GateSnapshot::from_store(&entry.configuration)?;
    Ok(snapshot
        .modules()
        .into_iter()
        .map(|module| {
            let program = identity(&snapshot, &module);
            (module, program)
        })
        .collect())
}

/// Program identity of the module's active generation. Program changes become
/// active only through a child started from the new program, so this is the
/// build of the process that acknowledged the module's current configuration.
pub(super) fn active(body: &StoreBody, module: &str) -> Result<Option<String>> {
    let Some(entry) = body
        .versions
        .iter()
        .find(|(tuple, _)| tuple.module == module && tuple.project.is_none())
        .and_then(|(_, versions)| versions.active)
        .and_then(|id| body.generations.get(&id))
    else {
        return Ok(None);
    };
    let snapshot = GateSnapshot::from_store(&entry.configuration)?;
    Ok(Some(identity(&snapshot, module)))
}

/// Modules among `modules` whose approved program is not the build their
/// configuration was last reviewed by.
pub(super) fn pending<'a>(
    body: &StoreBody,
    modules: impl IntoIterator<Item = &'a String>,
) -> Result<BTreeMap<String, Rereview>> {
    let approved = GateSnapshot::from_store(&body.approved)?;
    let recorded = recorded(body)?;
    let mut pending = BTreeMap::new();
    for module in modules {
        let program = identity(&approved, module);
        if recorded.get(module) == Some(&program) {
            continue;
        }
        let state = if active(body, module)?.as_ref() == Some(&program) {
            Rereview::Ready
        } else {
            Rereview::WaitingForProgram
        };
        pending.insert(module.clone(), state);
    }
    Ok(pending)
}
