//! The first-party mark's activation and lowering, decided while the gate
//! holds its delivery state. The mark tells providers that a route's caller
//! or target is a supervised spawn of a program the operator admitted, so
//! they may apply grants its approved provider declarations cover.
//!
//! The gate is where a spawn's acknowledgement of its module's active
//! generation is observed, so it decides the mark there; readiness comes from
//! the supervisor's healthy probe through [`RuntimeOwner::mark_ready`]. The
//! decision runs under the delivery state, takes the spawn's authority record
//! state, and publishes through [`MarkPublisher`] into the forwarding table:
//! delivery, then record, then forwarding. The publisher sends the catalog
//! notice and any route closes only after those locks are released.
//!
//! # Activation
//!
//! A spawn is activated once, at the first moment all of these hold:
//! - it was admitted while the stamped store gated its module, and both still
//!   hold (a stamp never marks a child already running);
//! - its program's origin qualifies on this daemon: `release`, or
//!   `development` on a development daemon;
//! - it runs the active generation: a module the active generation delivers
//!   configuration to acknowledged that generation, and any other module was
//!   admitted under the active program record;
//! - it reported ready.
//!
//! Activation publishes the mark without bumping the epoch and closes no
//! route. A spawn promoted early, because the incumbent it was replacing
//! died, stays unmarked until it has acknowledged its own generation as the
//! active one.
//!
//! # Lowering
//!
//! A spawn is lowered for good when the store is unstamped, when its module is
//! no longer gated, when its own delivery fails (a refused or timed-out
//! acknowledgement of a pending generation), or when its module's active
//! generation moves to one the spawn never acknowledged (a rollback or a
//! restore). Lowering bumps the spawn's epoch and closes its routes, each with
//! the close reason its opener can decode. Revoked origin evidence lowers
//! through [`RuntimeOwner::lower_mark`].

use std::sync::Arc;

use super::admission::MarkView;
use super::*;
use crate::spawn_authority::{MarkStatus, SpawnAuthorityRecord};

/// Publishes mark changes beyond the record: the forwarding table's mark, the
/// epoch fence and route closes, and the catalog notice. Called while the gate
/// holds its delivery state; implementations take the record's state, then
/// the forwarding write lock, and defer everything else until both are
/// released.
pub(crate) trait MarkPublisher: Send + Sync {
    /// Raise `record`'s mark. `true` when it was raised.
    fn activate(&self, record: &Arc<SpawnAuthorityRecord>) -> bool;
    /// Lower `record`'s mark for good.
    fn lower(&self, record: &Arc<SpawnAuthorityRecord>);
}

/// The publisher used before the daemon installs one, and by gate tests that
/// have no forwarding table: the mark lives on the record alone.
struct RecordOnly;

impl MarkPublisher for RecordOnly {
    fn activate(&self, record: &Arc<SpawnAuthorityRecord>) -> bool {
        record.activate(|| Ok::<_, ()>(())).unwrap_or(false)
    }

    fn lower(&self, record: &Arc<SpawnAuthorityRecord>) {
        record.lower(|_| ());
    }
}

impl RuntimeOwner {
    /// Install the daemon's publisher. Without one the mark lives on the
    /// record alone.
    pub(crate) fn install_mark_publisher(&self, publisher: Arc<dyn MarkPublisher>) {
        let _ = self.mark_publisher.set(publisher);
    }

    fn mark_publisher(&self) -> &dyn MarkPublisher {
        self.mark_publisher
            .get()
            .map_or(&RecordOnly as &dyn MarkPublisher, |publisher| {
                publisher.as_ref()
            })
    }

    /// The supervisor saw `spawn` report ready (a healthy probe of its own
    /// registration). Activates the mark if that was the last condition.
    pub(crate) fn mark_ready(&self, spawn: &DeliverySpawn) -> Result<()> {
        let Some(record) = spawn.authority() else {
            return Ok(());
        };
        let mut state = self.lock()?;
        state.delivery.track_mark(spawn);
        record.note_ready();
        evaluate(self, &state, spawn);
        Ok(())
    }

    /// The supervisor saw `spawn` answer a probe without reporting ready.
    /// Records, and logs once, that a spawn that has met every other
    /// condition is held back only by its readiness: a module must reach
    /// ready without first-party grants, or it is never marked.
    pub(crate) fn mark_not_ready(&self, spawn: &DeliverySpawn) -> Result<()> {
        if spawn.authority().is_none() {
            return Ok(());
        }
        let mut state = self.lock()?;
        state.delivery.track_mark(spawn);
        evaluate(self, &state, spawn);
        Ok(())
    }

    /// Lower `record`'s mark for a reason the gate does not observe itself,
    /// such as revoked or tampered origin evidence.
    #[allow(dead_code)]
    pub(crate) fn lower_mark(&self, record: &Arc<SpawnAuthorityRecord>) -> Result<()> {
        let _state = self.lock()?;
        self.mark_publisher().lower(record);
        Ok(())
    }
}

/// `spawn` acknowledged `generation`, which is now its module's active
/// generation.
pub(super) fn acknowledged(
    owner: &RuntimeOwner,
    state: &OwnerState,
    spawn: Option<&DeliverySpawn>,
    generation: u64,
) {
    let Some(spawn) = spawn else {
        return;
    };
    if let Some(record) = spawn.authority() {
        record.note_active_acknowledgement(generation);
        evaluate(owner, state, spawn);
    }
}

/// `spawn` refused, or let time out, the acknowledgement it was asked for
/// only for its mark.
pub(super) fn acknowledgement_refused(
    _owner: &RuntimeOwner,
    _state: &OwnerState,
    spawn: Option<&DeliverySpawn>,
) {
    let Some(record) = spawn.and_then(DeliverySpawn::authority) else {
        return;
    };
    if record.mark_facts().unmarked {
        record.set_unmarked_status(MarkStatus::AcknowledgementRefused);
        tracing::warn!(
            module_id = record.module_id(),
            spawn = record.spawn_number(),
            "spawn is not marked first-party: it did not acknowledge its module's active configuration"
        );
    }
}

/// `spawn`'s own delivery failed, so it no longer runs the active generation
/// for sure.
pub(super) fn delivery_failed(owner: &RuntimeOwner, spawn: Option<&DeliverySpawn>) {
    if let Some(record) = spawn.and_then(DeliverySpawn::authority) {
        if record.mark_facts().marked {
            owner.mark_publisher().lower(record);
        }
    }
}

/// Activate `spawn` if every condition holds now; otherwise record why not.
fn evaluate(owner: &RuntimeOwner, state: &OwnerState, spawn: &DeliverySpawn) {
    let Some(record) = spawn.authority() else {
        return;
    };
    let facts = record.mark_facts();
    if !facts.unmarked {
        return;
    }
    let view = owner
        .published
        .read()
        .map(|published| published.admission.mark_view(spawn.module()))
        .unwrap_or(MarkView::Undecided);
    let status = blocker(state, spawn, record, view);
    match status {
        None => {
            if owner.mark_publisher().activate(record) {
                tracing::info!(
                    module_id = record.module_id(),
                    spawn = record.spawn_number(),
                    "spawn marked first-party"
                );
            }
        }
        Some(status) => {
            if record.set_unmarked_status(status) {
                tracing::warn!(
                    module_id = record.module_id(),
                    spawn = record.spawn_number(),
                    "spawn is not marked first-party: it has not reported ready. A module must \
                     reach ready without first-party grants and use them once they work"
                );
            }
        }
    }
}

/// What still keeps `spawn` from the mark, or `None` when nothing does.
fn blocker(
    state: &OwnerState,
    spawn: &DeliverySpawn,
    record: &SpawnAuthorityRecord,
    view: MarkView,
) -> Option<MarkStatus> {
    let facts = record.mark_facts();
    let (store, body) = state.store.loaded_admission_state();
    if !spawn.stamped() || store != StoreState::Stamped || !facts.origin_qualifies {
        return Some(MarkStatus::NotEligible);
    }
    let MarkView::Gated {
        generation,
        delivers_config,
    } = view
    else {
        return Some(MarkStatus::NotEligible);
    };
    if delivers_config {
        let active = active_generation(body, spawn.module());
        if active.is_none() || facts.acknowledged_active != active {
            return Some(match record.mark_status() {
                MarkStatus::AcknowledgementRefused => MarkStatus::AcknowledgementRefused,
                _ => MarkStatus::AwaitingAcknowledgement,
            });
        }
    } else if spawn.generation() != generation {
        // Admitted under a program record that is no longer the active one.
        return Some(MarkStatus::NotEligible);
    }
    if !facts.ready {
        return Some(MarkStatus::AwaitingReadiness);
    }
    None
}

fn active_generation(body: &crate::config_store::StoreBody, module: &str) -> Option<u64> {
    body.versions
        .iter()
        .find(|(tuple, _)| tuple.module == module && tuple.project.is_none())
        .and_then(|(_, versions)| versions.active)
}

/// Lower every tracked spawn this state rules out. Runs as the gate releases
/// its delivery state, with the admission view it is about to publish.
pub(super) fn sweep(
    owner: &RuntimeOwner,
    state: &mut OwnerState,
    view: &super::admission::AdmissionCache,
) {
    let records = state.delivery.mark_records();
    let (store, body) = state.store.loaded_admission_state();
    for record in &records {
        let facts = record.mark_facts();
        if !facts.marked && !facts.unmarked {
            continue;
        }
        let module = record.module_id();
        // A committed unstamp lowers the mark immediately, regardless of the
        // current launch-admission view.
        let view = if store == StoreState::Unstamped {
            MarkView::NotGated
        } else {
            view.mark_view(module)
        };
        let lower = match view {
            MarkView::Undecided => false,
            MarkView::NotGated => true,
            MarkView::Gated {
                delivers_config, ..
            } => {
                facts.marked
                    && delivers_config
                    && facts.acknowledged_active != active_generation(body, module)
            }
        };
        if lower {
            owner.mark_publisher().lower(record);
        }
    }
}
