//! Immutable config planning and approval, independent of transport dispatch.
//!
//! The daemon owner captures files, validates schemas/bounds and runs module
//! review before handing snapshots here. It serializes this engine with the
//! store and supervisor commit locks, recaptures files at completion, and uses
//! only the committed snapshot for delivery. No client text is approval input.

mod anchor_retry;
mod apply;
mod approval;
pub mod capture;
pub(crate) mod program;
pub(crate) mod review;
mod reviewed_build;
pub mod runtime;
mod snapshot;
mod strict_json;

pub use approval::*;
pub use snapshot::{CapturedInput, GateSnapshot, InputKey};

use std::collections::{BTreeMap, BTreeSet};

use subc_config::{Changeset, Digest, Generation, Plan};
use subc_control::config::ConfigPlanView;

use crate::config_store::{CommitKind, ConfigStore, HistoryEntry, StoreError, StoreState, Tuple};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateError {
    pub code: String,
    pub detail: String,
}
type Result<T> = std::result::Result<T, GateError>;

impl GateError {
    pub fn new(code: &str, detail: impl ToString) -> Self {
        Self {
            code: code.into(),
            detail: detail.to_string(),
        }
    }
    fn invalid(detail: impl ToString) -> Self {
        Self::new("value_invalid", detail)
    }
    fn stale() -> Self {
        Self::new(
            "plan_stale",
            "reviewed inputs or generation changed; plan again",
        )
    }

    pub(crate) fn commit_timeout() -> Self {
        Self::new("config_commit_timeout", "the post-answer commit window expired before durable publication; plan remains unapproved")
    }
}
impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}
impl std::error::Error for GateError {}
impl From<StoreError> for GateError {
    fn from(error: StoreError) -> Self {
        use crate::config_store::StoreErrorCode as Code;
        let code = match error.code {
            Code::StoreUnverified => "store_unverified",
            Code::StoreMissing => "store_missing",
            Code::StoreTampered => "store_tampered",
            Code::StoreTooLarge => "store_too_large",
            Code::AnchorPending => "anchor_pending",
            Code::GenerationPruned => "generation_pruned",
            Code::GenerationMissing => "generation_missing",
            Code::RevisionExhausted => "revision_exhausted",
            Code::InvalidState => "config_gated",
            Code::Io => "config_store_io",
        };
        Self::new(code, error.detail)
    }
}

/// The supervisor's program-admission adapter supplies this authority. A module
/// registration and a schema declaration alone never admit its running code.
pub trait ReviewAdmission: Send + Sync {
    fn check(&self, registration: &crate::registry::ModuleRegistration) -> Result<()>;
}

pub(crate) struct MissingReviewAdmission;
impl ReviewAdmission for MissingReviewAdmission {
    fn check(&self, registration: &crate::registry::ModuleRegistration) -> Result<()> {
        Err(GateError::new(
            "program_admission_unavailable",
            format!(
                "{}: running program review admission is not connected",
                registration.manifest.module_id
            ),
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanKind {
    Change,
    Stamp,
    Unstamp,
    Rollback(u64),
}
impl PlanKind {
    fn commit_kind(self) -> CommitKind {
        match self {
            Self::Change => CommitKind::Approved,
            Self::Stamp => CommitKind::Stamp,
            Self::Unstamp => CommitKind::Unstamp,
            Self::Rollback(_) => CommitKind::Rollback,
        }
    }
}

struct PendingPlan {
    view: ConfigPlanView,
    kind: PlanKind,
    snapshot: GateSnapshot,
    modules: BTreeSet<String>,
    changed: BTreeSet<InputKey>,
    callosum_entry_changed: bool,
    recovery: bool,
    delivery: Option<crate::config_store::DeliverySnapshot>,
}

/// Memory-only plans cannot be replayed across a daemon restart. A successful
/// generation commit changes the base revision and consumes its approval once.
pub struct ConfigGate {
    incarnation: String,
    plans: BTreeMap<Digest, PendingPlan>,
    attempts: BTreeMap<u64, approval::Attempt>,
    next_attempt: u64,
}

impl ConfigGate {
    pub(crate) fn plan_modules(&self, digest: &Digest) -> Result<BTreeSet<String>> {
        self.plans
            .get(digest)
            .map(|plan| plan.modules.clone())
            .ok_or_else(|| GateError::new("plan_unknown", "plan is absent or already applied"))
    }

    pub(crate) fn cancel_attempt(&mut self, attempt: u64) {
        self.attempts.remove(&attempt);
    }

    pub(crate) fn expire_plan(&mut self, digest: &Digest) -> bool {
        self.plans.remove(digest).is_some()
    }
    pub fn new(incarnation: String) -> Self {
        Self {
            incarnation,
            plans: BTreeMap::new(),
            attempts: BTreeMap::new(),
            next_attempt: 0,
        }
    }

    /// `review` enforces the approved schema and every affected project's bounds
    /// before asking admitted module reviewers. Refusals produce no plan.
    pub fn plan(
        &mut self,
        store: &mut ConfigStore,
        kind: PlanKind,
        candidate: GateSnapshot,
        observed: &GateSnapshot,
        review: impl FnOnce(&GateSnapshot, &GateSnapshot) -> Result<Changeset>,
    ) -> Result<ConfigPlanView> {
        self.plan_reviewed(store, kind, candidate, observed, None, review)
    }

    /// As [`Self::plan`], also recording which modules' admitted reviewers
    /// answered `config.review` for this plan, and which build each module's
    /// approved configuration now rests on. Without that record the plan
    /// carries none, and the next reader counts every module as reviewed by
    /// the programs the plan approved.
    pub(crate) fn plan_reviewed(
        &mut self,
        store: &mut ConfigStore,
        kind: PlanKind,
        candidate: GateSnapshot,
        observed: &GateSnapshot,
        reviewed: Option<&BTreeSet<String>>,
        review: impl FnOnce(&GateSnapshot, &GateSnapshot) -> Result<Changeset>,
    ) -> Result<ConfigPlanView> {
        let status = store.status();
        let recovery = matches!(
            status.state,
            StoreState::StoreUnverified | StoreState::StoreMissing
        );
        if (recovery && !matches!(kind, PlanKind::Stamp | PlanKind::Unstamp))
            || (status.state == StoreState::Unstamped && kind != PlanKind::Stamp)
        {
            return Err(GateError::new(
                "config_gated",
                "stamp is required before planning changes",
            ));
        }
        if self.plans.len() >= 64 {
            return Err(GateError::new("config_busy", "too many pending plans"));
        }
        let mut rollback_programs = Vec::new();
        if let PlanKind::Rollback(id) = kind {
            let target = GateSnapshot::from_store(&store.rollback_configuration(id)?)?;
            if target
                .inputs
                .iter()
                .filter(|(key, _)| !key.is_program())
                .ne(candidate.inputs.iter().filter(|(key, _)| !key.is_program()))
            {
                return Err(GateError::invalid(
                    "rollback must reverse the target configuration",
                ));
            }
            for (key, now) in candidate.inputs.iter().filter(|(key, _)| key.is_program()) {
                if target.inputs.get(key) != Some(now) {
                    rollback_programs.push(serde_json::json!({
                        "module_id":key.module(),
                        "input":key,
                        "reason":"program changed since that version",
                        "approved_then":target.inputs.get(key),
                        "on_disk_now":now,
                    }));
                }
            }
        }
        let (base_revision, base_generation, approved, active) = if recovery {
            (0, 0, GateSnapshot::default(), BTreeMap::new())
        } else {
            store.delivery_ready()?;
            let body = store.body()?;
            let active = body
                .versions
                .iter()
                .filter_map(|(tuple, versions)| {
                    versions
                        .active
                        .map(|id| (serde_json::to_string(tuple).expect("tuple"), id))
                })
                .collect();
            (
                body.base_revision,
                body.last_generation,
                GateSnapshot::from_store(&body.approved)?,
                active,
            )
        };
        let changed: BTreeSet<InputKey> = approved
            .changed_keys(&candidate)
            .into_iter()
            .cloned()
            .collect();
        let callosum_entry_changed = if changed.contains(&InputKey::Daemon) {
            match (
                approved.inputs.get(&InputKey::Daemon),
                candidate.inputs.get(&InputKey::Daemon),
            ) {
                (
                    Some(CapturedInput {
                        value: subc_config::RecordedValue::Value { value: old },
                        ..
                    }),
                    Some(CapturedInput {
                        value: subc_config::RecordedValue::Value { value: new },
                        ..
                    }),
                ) => old.pointer("/modules/callosum") != new.pointer("/modules/callosum"),
                _ => true,
            }
        } else {
            false
        };
        let mut modules: BTreeSet<String> = changed
            .iter()
            .filter_map(|key| key.module().map(str::to_owned))
            .collect();
        let mut changeset = review(&approved, &candidate)?;
        for difference in &rollback_programs {
            if let Some(module) = difference["module_id"].as_str() {
                modules.insert(module.to_owned());
                changeset
                    .launch_manifests
                    .entry(module.to_owned())
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .ok_or_else(|| GateError::invalid("launch manifest must be an object"))?
                    .insert(
                        "program_changed_since_target".into(),
                        serde_json::Value::Bool(true),
                    );
            }
        }
        modules.extend(changeset.fields.iter().map(|field| field.module_id.clone()));
        modules.extend(changeset.launch_manifests.keys().cloned());
        if matches!(kind, PlanKind::Stamp | PlanKind::Unstamp) {
            modules.extend(observed.modules());
            modules.extend(approved.modules());
        }
        // Bind the full retained view and operation, including identity-only and
        // declaration-only changes a module's scalar diff would not describe.
        changeset.apply_plan = serde_json::json!({
            "operation": match kind { PlanKind::Change => "change", PlanKind::Stamp => "stamp", PlanKind::Unstamp => "unstamp", PlanKind::Rollback(_) => "rollback" },
            "rollback_to": if let PlanKind::Rollback(id) = kind { Some(id) } else { None },
            "review": changeset.apply_plan,
            "previous": approved.to_store()?,
            "snapshot": candidate.to_store()?,
        });
        if matches!(kind, PlanKind::Rollback(_)) {
            changeset.apply_plan["rollback_programs"] = serde_json::Value::Array(rollback_programs);
        }
        let replaces_programs =
            !recovery && matches!(kind, PlanKind::Change | PlanKind::Rollback(_));
        if replaces_programs {
            // A module whose active generation ran an older program than this
            // plan's (an earlier program change not yet active, say) still has
            // that older process running. It must not acknowledge this
            // generation either, even though the program matches the last
            // approval.
            let body = store.body()?;
            for module in &modules {
                if reviewed_build::active(body, module)?
                    .is_some_and(|active| active != reviewed_build::identity(&candidate, module))
                {
                    changeset
                        .launch_manifests
                        .entry(module.clone())
                        .or_insert_with(|| serde_json::json!({}))
                        .as_object_mut()
                        .ok_or_else(|| GateError::invalid("launch manifest must be an object"))?
                        .insert(
                            "program_changed_since_active".into(),
                            serde_json::Value::Bool(true),
                        );
                }
            }
        }
        if let Some(reviewed) = reviewed {
            let mut record = if replaces_programs {
                reviewed_build::recorded(store.body()?)?
            } else {
                BTreeMap::new()
            };
            let present = candidate.modules();
            record.retain(|module, _| present.contains(module));
            for module in reviewed {
                // A stamp records the programs already running. Otherwise the
                // reviewer is the process that acknowledged the active
                // generation, or one admitted from the last approval when the
                // module has never acknowledged one.
                let program = if replaces_programs {
                    match reviewed_build::active(store.body()?, module)? {
                        Some(active) => active,
                        None => reviewed_build::identity(&approved, module),
                    }
                } else {
                    reviewed_build::identity(&candidate, module)
                };
                if let Some(previous) = record.get(module).filter(|previous| **previous != program)
                {
                    changeset.policy_changes.push(serde_json::json!({
                        "module_id": module,
                        "path": "reviewed_program",
                        "old": previous,
                        "new": program,
                        "reason": "approved configuration re-reviewed by the module's newly approved program",
                    }));
                }
                record.insert(module.clone(), program);
            }
            changeset.apply_plan[reviewed_build::REVIEWED_PROGRAMS] = serde_json::json!(record);
        }
        apply::bind_plan(&mut changeset)?;
        let mut plan = Plan::new(
            base_revision,
            base_generation,
            &self.incarnation,
            observed.read_set(&modules)?,
            changeset,
        );
        plan.active_generations = active;
        let view = ConfigPlanView::new(plan).map_err(GateError::invalid)?;
        self.plans.insert(
            view.digest.clone(),
            PendingPlan {
                view: view.clone(),
                kind,
                snapshot: candidate,
                modules,
                changed,
                callosum_entry_changed,
                recovery,
                delivery: None,
            },
        );
        Ok(view)
    }

    fn plan_program_keys(&self, digest: &Digest) -> Result<Vec<InputKey>> {
        let plan = self
            .plans
            .get(digest)
            .ok_or_else(|| GateError::new("plan_unknown", "plan is absent or already applied"))?;
        Ok(plan
            .snapshot
            .inputs
            .keys()
            .filter(|key| {
                key.is_program()
                    && key
                        .module()
                        .is_some_and(|module| plan.modules.contains(module))
            })
            .cloned()
            .collect())
    }

    /// Attach daemon-reviewed raw inputs without exposing them in diagnostic
    /// plans. The saved file identities and byte digests still bind the bytes
    /// captured for review.
    pub(crate) fn bind_delivery(
        &mut self,
        digest: &Digest,
        delivery: crate::config_store::DeliverySnapshot,
    ) -> Result<()> {
        self.plans
            .get_mut(digest)
            .ok_or_else(|| GateError::new("plan_unknown", "plan is absent or already applied"))?
            .delivery = Some(delivery);
        Ok(())
    }

    /// Capture program identities for the reverse configuration's launch specs
    /// at the restored paths at plan time. No executable bytes or historical
    /// identities are restored.
    pub fn rollback(
        &mut self,
        store: &mut ConfigStore,
        generation: u64,
        observed: &GateSnapshot,
        current_programs: &GateSnapshot,
        review: impl FnOnce(&GateSnapshot, &GateSnapshot) -> Result<Changeset>,
    ) -> Result<ConfigPlanView> {
        let mut target = GateSnapshot::from_store(&store.rollback_configuration(generation)?)?;
        target.restore_programs_from(current_programs)?;
        self.plan(
            store,
            PlanKind::Rollback(generation),
            target,
            observed,
            review,
        )
    }

    fn fresh(plan: &PendingPlan, store: &mut ConfigStore, current: &GateSnapshot) -> Result<()> {
        let status = store.status();
        let recovery = matches!(
            status.state,
            StoreState::StoreUnverified | StoreState::StoreMissing
        );
        if recovery != plan.recovery {
            return Err(GateError::stale());
        }
        let base = if recovery {
            0
        } else {
            store.delivery_ready()?;
            store.body()?.base_revision
        };
        plan.view
            .plan
            .check_fresh(base, &current.read_set(&plan.modules)?)
            .map_err(|_| GateError::stale())
    }

    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        digest: &Digest,
        approval_channel: subc_config::ApprovalChannel,
        store: &mut ConfigStore,
        current: &GateSnapshot,
        now_ms: u64,
        before_publication: impl FnOnce() -> Result<()>,
    ) -> Result<u64> {
        let plan = self
            .plans
            .get(digest)
            .ok_or_else(|| GateError::new("plan_unknown", "plan is absent or already applied"))?;
        Self::fresh(plan, store, current)?;
        let mut generation = Generation::new(
            0,
            approval_channel,
            digest.clone(),
            plan.view.plan.input_vector.clone(),
            plan.view.plan.changeset.clone(),
        );
        generation.active_generations = plan.view.plan.active_generations.clone();
        for module in &plan.modules {
            generation
                .module_states
                .insert(module.clone(), subc_config::GenerationState::Pending);
            if let Some(CapturedInput {
                state: subc_config::InputState::Present { digest },
                ..
            }) = plan
                .snapshot
                .inputs
                .get(&InputKey::Declaration(module.clone()))
            {
                generation
                    .declaration_digests
                    .insert(module.clone(), digest.clone());
            }
        }
        let tuples: BTreeSet<Tuple> = plan
            .modules
            .iter()
            .map(|module| Tuple {
                module: module.clone(),
                project: None,
            })
            .chain(plan.view.plan.changeset.fields.iter().map(|field| Tuple {
                module: field.module_id.clone(),
                project: field.project_id.clone(),
            }))
            .chain(plan.modules.iter().flat_map(|module| {
                plan.view
                    .plan
                    .changeset
                    .affected_projects
                    .iter()
                    .map(move |project| Tuple {
                        module: module.clone(),
                        project: Some(project.clone()),
                    })
            }))
            .collect();
        let snapshot = plan.snapshot.to_store()?;
        let entry = HistoryEntry {
            generation,
            kind: plan.kind.commit_kind(),
            approval_id: digest.to_string(),
            outcome: "approved".into(),
            tuples,
            configuration: snapshot.clone(),
            delivery: plan.delivery.clone(),
        };
        let base_before = store.status().base_revision;
        let observed = current.to_store()?;
        before_publication()?;
        let result = if plan.recovery {
            store.recovery_exit(entry, snapshot, now_ms)
        } else {
            store.commit_generation(entry, snapshot, observed, now_ms)
        };
        // The sealed store's external revision/digest anchor may fail to update
        // after a durable commit. The committed base makes the old plan stale;
        // never repeat that generation while retrying the anchor update.
        if result.is_ok() || store.status().base_revision != base_before {
            self.plans.remove(digest);
        }
        let id = result?;
        store.delivery_ready()?;
        Ok(id)
    }
}

mod registration;
#[cfg(test)]
mod tests;
