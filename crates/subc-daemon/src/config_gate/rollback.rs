use super::{CapturedConfiguration, GateError, OwnerState, Result, RuntimeOwner};
use crate::config_gate::GateSnapshot;

pub(super) fn prepare(
    owner: &RuntimeOwner,
    state: &mut OwnerState,
    id: u64,
) -> Result<(CapturedConfiguration, GateSnapshot)> {
    let mut target = GateSnapshot::from_store(&state.store.rollback_configuration(id)?)?;
    // A rollback restores user-level settings only; project layers stay as they are.
    target.inputs.retain(|key, _| !key.is_project());
    let delivery = state
        .store
        .body()?
        .generations
        .get(&id)
        .and_then(|entry| entry.delivery.as_ref())
        .ok_or_else(|| {
            GateError::new(
                "rollback_snapshot_unavailable",
                "retained raw configuration is absent",
            )
        })?;
    let incompatible: Vec<_> = state
        .active_declarations
        .iter()
        .filter_map(|(module, connection)| {
            let (_, admitted) = state.declarations.get(connection)?;
            (delivery.declarations.get(module) != Some(admitted)).then_some(module.as_str())
        })
        .collect();
    if !incompatible.is_empty() {
        return Err(GateError::new(
            "rollback_declaration_mismatch",
            format!(
                "Rollback target {id} was approved for a different build of {}. Change the settings values with an ordinary edit instead.",
                incompatible.join(", ")
            ),
        ));
    }
    let mut candidate = owner.capturer.capture_retained(&target, delivery)?;
    let approved = GateSnapshot::from_store(&state.store.body()?.approved)?;
    let root = super::policy_root::section_value(&approved);
    if super::policy_root::section_value(&target) != root {
        super::policy_root::patch_daemon(
            &mut candidate,
            super::policy_root::edit_for(root.cloned()),
        )?;
    }
    Ok((candidate, target))
}
