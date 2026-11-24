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
mod capture_plan;
pub(crate) mod program;
pub mod project_layers;
pub mod project_registry;
mod refusal_rule;
pub(crate) mod review;
mod reviewed_build;
mod root_io;
pub mod runtime;
mod snapshot;
mod strict_json;
pub(crate) mod targets;

pub(crate) use apply::Transition;
pub use approval::*;
pub use snapshot::{CapturedInput, GateSnapshot, InputKey};

/// The settings edit base advances only when a retained generation names the module.
pub(crate) fn module_base_generation(body: &crate::config_store::StoreBody, module: &str) -> u64 {
    if !body.stamped {
        return 0;
    }
    body.generations
        .values()
        .rev()
        .find(|entry| entry.tuples.iter().any(|tuple| tuple.module == module))
        .map_or(0, |entry| entry.generation.id)
}

use std::collections::{BTreeMap, BTreeSet};

use subc_config::{Changeset, Digest, Generation, Layer, Plan};
use subc_control::config::{ConfigFileError, ConfigFileErrorLayer, ConfigPlanView};

use crate::config_store::{CommitKind, ConfigStore, HistoryEntry, StoreError, StoreState, Tuple};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateError {
    pub code: String,
    pub detail: String,
    capture_race: bool,
    settings_file: Option<Box<SettingsFile>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SettingsFile {
    layer: Layer,
    path: Option<String>,
    content_digest: Option<Digest>,
}
type Result<T> = std::result::Result<T, GateError>;

impl GateError {
    pub fn new(code: &str, detail: impl ToString) -> Self {
        Self {
            code: code.into(),
            detail: detail.to_string(),
            capture_race: false,
            settings_file: None,
        }
    }

    fn at_settings_layer(mut self, layer: Layer) -> Self {
        if matches!(layer, Layer::Project | Layer::Workspace) {
            self.settings_file = Some(Box::new(SettingsFile {
                layer,
                path: None,
                content_digest: None,
            }));
        }
        self
    }

    fn at_user_file(mut self, path: &std::path::Path, bytes: Option<&[u8]>) -> Self {
        self.settings_file = Some(Box::new(SettingsFile {
            layer: Layer::User,
            path: Some(path.to_string_lossy().into_owned()),
            content_digest: bytes.map(subc_config::raw_digest),
        }));
        self
    }

    fn with_settings_path(mut self, path_for: impl FnOnce(Layer) -> Option<String>) -> Self {
        if let Some(file) = self.settings_file.as_mut() {
            file.path =
                path_for(file.layer).filter(|path| std::path::Path::new(path).is_absolute());
        }
        self
    }

    fn into_settings_file_error(self) -> ConfigFileError {
        let mut error = ConfigFileError::new(self.code, self.detail);
        if let Some(file) = self.settings_file {
            error.layer = match file.layer {
                Layer::Project => Some(ConfigFileErrorLayer::Project),
                Layer::Workspace => Some(ConfigFileErrorLayer::Workspace),
                _ => None,
            };
            error.path = file.path;
        }
        error
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

    // Observational retries must not also retry stale review or plan decisions
    // just because those refusals use the same wire code and message.
    pub(crate) fn capture_race() -> Self {
        Self {
            capture_race: true,
            ..Self::stale()
        }
    }

    pub(crate) fn is_capture_race(&self) -> bool {
        self.capture_race
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

/// Which build each module's approved configuration was reviewed by, as a
/// plan writes it into its generation.
pub(crate) enum ReviewRecord<'a> {
    /// Write no record. Later plans then treat every module this plan approves
    /// as owing a re-review.
    Absent,
    /// These modules' admitted reviewers answered `config.review` for this
    /// plan. Their builds are recorded; other modules keep their entries from
    /// the newest approved generation.
    Reviewed(&'a BTreeSet<String>),
    /// Copy a record verbatim, for a rollback to a generation whose reviewers
    /// are known. `None` (that generation had no record) writes none.
    Copied(Option<serde_json::Value>),
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
    creation_sequence: u64,
    edits: Option<runtime::edits::EditPlan>,
    development_approval: Option<crate::program_origin::DevelopmentApproval>,
    view: ConfigPlanView,
    kind: PlanKind,
    snapshot: GateSnapshot,
    modules: BTreeSet<String>,
    changed: BTreeSet<InputKey>,
    callosum_entry_changed: bool,
    recovery: bool,
    delivery: Option<crate::config_store::DeliverySnapshot>,
    project_files: Vec<crate::config_store::ProjectFileApproval>,
    project_capture: Option<capture_plan::ProjectCapture>,
}

impl PendingPlan {
    fn targets(&self) -> Result<BTreeSet<Tuple>> {
        let targets = targets::checked(&self.view.plan)?;
        // Approving more provider declarations for a development build does
        // not start config delivery. The module read set still detects changes
        // while the operator reviews those declarations.
        Ok(if self.development_approval.is_some() {
            BTreeSet::new()
        } else {
            targets
        })
    }
}

/// Memory-only plans cannot be replayed across a daemon restart. A successful
/// generation commit changes the base revision and consumes its approval once.
pub struct ConfigGate {
    incarnation: String,
    plans: BTreeMap<Digest, PendingPlan>,
    attempts: BTreeMap<u64, approval::Attempt>,
    next_attempt: u64,
    next_plan_sequence: u64,
}

impl ConfigGate {
    fn pending_file_plan(&self, module: &str) -> Option<Digest> {
        self.plans
            .values()
            .filter(|plan| {
                plan.edits.is_none()
                    && plan.modules.contains(module)
                    && plan.targets().is_ok_and(|targets| {
                        targets
                            .iter()
                            .any(|tuple| tuple.module == module && tuple.project.is_none())
                            || !targets.iter().any(|tuple| tuple.module == module)
                    })
            })
            .max_by_key(|plan| plan.creation_sequence)
            .map(|plan| plan.view.digest.clone())
    }

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
            next_plan_sequence: 0,
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
        self.plan_reviewed(
            store,
            kind,
            candidate,
            observed,
            ReviewRecord::Absent,
            review,
        )
    }

    /// As [`Self::plan`], also writing the record of which build each module's
    /// approved configuration was reviewed by. A generation planned without that
    /// record vouches for no review, so later plans treat each module it
    /// approved as needing re-review.
    pub(crate) fn plan_reviewed(
        &mut self,
        store: &mut ConfigStore,
        kind: PlanKind,
        candidate: GateSnapshot,
        observed: &GateSnapshot,
        record: ReviewRecord<'_>,
        review: impl FnOnce(&GateSnapshot, &GateSnapshot) -> Result<Changeset>,
    ) -> Result<ConfigPlanView> {
        self.plan_reviewed_inner(store, kind, candidate, observed, record, None, None, review)
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_reviewed_inner(
        &mut self,
        store: &mut ConfigStore,
        kind: PlanKind,
        mut candidate: GateSnapshot,
        observed: &GateSnapshot,
        record: ReviewRecord<'_>,
        edits: Option<runtime::edits::EditPlan>,
        projects: Option<targets::ProjectReviews>,
        review: impl FnOnce(&GateSnapshot, &GateSnapshot) -> Result<Changeset>,
    ) -> Result<ConfigPlanView> {
        let status = store.status();
        let recovery = matches!(
            status.state,
            StoreState::StoreUnverified | StoreState::StoreMissing
        );
        if (recovery && !matches!(kind, PlanKind::Stamp | PlanKind::Unstamp))
            || (status.state == StoreState::Unstamped && kind != PlanKind::Stamp && edits.is_none())
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
            let mut target = GateSnapshot::from_store(&store.rollback_configuration(id)?)?;
            runtime::policy_root::preserve_root_on_rollback(store.body()?, id, &mut target)?;
            if target
                .inputs
                .iter()
                .filter(|(key, _)| !key.is_program() && !key.is_project())
                .ne(candidate
                    .inputs
                    .iter()
                    .filter(|(key, _)| !key.is_program() && !key.is_project()))
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
        let (base_revision, base_generation, approved, active) =
            if let Some(edit) = edits.as_ref().filter(|edit| edit.unstamped) {
                (
                    status.base_revision,
                    0,
                    edit.baseline.clone(),
                    BTreeMap::new(),
                )
            } else if recovery {
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
        if !recovery {
            candidate.carry_project_inputs(&approved);
        }
        if status.state == StoreState::Stamped
            && kind == PlanKind::Change
            && edits.as_ref().is_none_or(|edit| edit.policy_root.is_none())
            && runtime::policy_root::section_value(&candidate)
                != runtime::policy_root::section_value(&approved)
        {
            return Err(GateError::new(
                "value_invalid",
                "use a policy_root operation to change the stamped agent policy trust root",
            ));
        }
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
        let mut user_modules: BTreeSet<String> = changed
            .iter()
            .filter(|key| !key.is_project())
            .filter_map(|key| key.module().map(str::to_owned))
            .collect();
        let mut changeset = review(&approved, &candidate)?;
        for difference in &rollback_programs {
            if let Some(module) = difference["module_id"].as_str() {
                user_modules.insert(module.to_owned());
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
        user_modules.extend(
            changeset
                .fields
                .iter()
                .filter(|field| field.project_id.is_none())
                .map(|field| field.module_id.clone()),
        );
        user_modules.extend(changeset.launch_manifests.keys().cloned());
        if matches!(kind, PlanKind::Stamp | PlanKind::Unstamp) {
            user_modules.extend(
                observed
                    .inputs
                    .keys()
                    .chain(approved.inputs.keys())
                    .filter(|key| !key.is_project())
                    .filter_map(|key| key.module().map(str::to_owned)),
            );
        }
        // Bind the full retained view and operation, including identity-only and
        // declaration-only changes a module's scalar diff would not describe.
        // Keep activation policy out of reviewer metadata. `small_prompt` in
        // approval.rs offers the compact one-change prompt only when `review`
        // is null, so leaving the transition table in it would deny that
        // prompt to every one-field change.
        let transitions = changeset
            .apply_plan
            .as_object_mut()
            .and_then(|review| review.remove("module_transitions"));
        if transitions.is_some()
            && changeset
                .apply_plan
                .as_object()
                .is_some_and(|review| review.is_empty())
        {
            changeset.apply_plan = serde_json::Value::Null;
        }
        changeset.apply_plan = serde_json::json!({
            "operation": match kind { PlanKind::Change => "change", PlanKind::Stamp => "stamp", PlanKind::Unstamp => "unstamp", PlanKind::Rollback(_) => "rollback" },
            "rollback_to": if let PlanKind::Rollback(id) = kind { Some(id) } else { None },
            "review": changeset.apply_plan,
            "previous": approved.to_store()?,
            "snapshot": candidate.to_store()?,
        });
        if let Some(mut transitions) = transitions {
            if let Some(table) = transitions.as_object_mut() {
                if projects.is_some() {
                    table.retain(|module, _| user_modules.contains(module));
                } else {
                    user_modules.extend(table.keys().cloned());
                }
            }
            changeset.apply_plan["module_transitions"] = transitions;
        }
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
            for module in &user_modules {
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
        let reviewed = match record {
            ReviewRecord::Absent | ReviewRecord::Copied(None) => None,
            ReviewRecord::Copied(Some(copied)) => {
                changeset.apply_plan[reviewed_build::REVIEWED_PROGRAMS] = copied;
                None
            }
            ReviewRecord::Reviewed(reviewed) => Some(reviewed),
        };
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
                    if projects.is_some() {
                        user_modules.insert(module.clone());
                    }
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
        if let Some(edit) = &edits {
            changeset.apply_plan["edit_plan"] = serde_json::json!({
                "unstamped": edit.unstamped,
                "write_back": edit.files.iter().map(runtime::edits::WriteBack::intent).collect::<Vec<_>>(),
            });
            if !edit.refusals.is_empty() {
                // Keep refusals bound to the plan digest so apply can reject it;
                // settings clients read the keyed rows from view.refusals.
                changeset.apply_plan["edit_refusals"] = serde_json::json!(edit.refusals);
            }
        }
        let mut modules = user_modules.clone();
        modules.extend(changed.iter().filter_map(|key| match key {
            InputKey::ProjectTuple { module, .. } => Some(module.clone()),
            _ => None,
        }));
        if let Some(projects) = &projects {
            modules.extend(projects.tuples.keys().map(|tuple| tuple.module.clone()));
        }
        let inputs = observed.read_set(&modules)?;
        let (delivery, project_files) = if let Some(projects) = projects {
            let body = if recovery { None } else { Some(store.body()?) };
            let (delivery, files) = projects.bind(body, &inputs, &mut changeset, &user_modules)?;
            if recovery && !files.is_empty() {
                return Err(GateError::new(
                    "apply_plan_invalid",
                    "recovery cannot approve project file write-back",
                ));
            }
            (Some(delivery), files)
        } else {
            (None, Vec::new())
        };
        let mut plan = Plan::new(
            base_revision,
            base_generation,
            &self.incarnation,
            inputs,
            changeset,
        );
        plan.active_generations = active;
        targets::checked(&plan)?;
        let mut view = ConfigPlanView::new(plan).map_err(GateError::invalid)?;
        if let Some(edit) = &edits {
            view.refusals = edit.refusals.clone();
        }
        self.next_plan_sequence = self
            .next_plan_sequence
            .checked_add(1)
            .ok_or_else(|| GateError::new("config_busy", "plan creation sequence exhausted"))?;
        self.plans.insert(
            view.digest.clone(),
            PendingPlan {
                creation_sequence: self.next_plan_sequence,
                edits,
                development_approval: None,
                view: view.clone(),
                kind,
                snapshot: candidate,
                modules,
                changed,
                callosum_entry_changed,
                recovery,
                delivery,
                project_files,
                project_capture: None,
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
        mut delivery: crate::config_store::DeliverySnapshot,
    ) -> Result<()> {
        let plan = self
            .plans
            .get_mut(digest)
            .ok_or_else(|| GateError::new("plan_unknown", "plan is absent or already applied"))?;
        if let Some(projects) = &plan.delivery {
            for (module, declaration) in &projects.declarations {
                if delivery
                    .declarations
                    .get(module)
                    .is_some_and(|user| user != declaration)
                {
                    return Err(GateError::new(
                        "apply_plan_invalid",
                        "user and project views name different declarations of the same module",
                    ));
                }
                delivery
                    .declarations
                    .insert(module.clone(), declaration.clone());
            }
            delivery.project_views.clone_from(&projects.project_views);
            delivery
                .project_cutoffs
                .clone_from(&projects.project_cutoffs);
            delivery
                .project_approval_digests
                .clone_from(&projects.project_approval_digests);
        }
        plan.delivery = Some(delivery);
        Ok(())
    }

    /// Capture program identities for the reverse configuration's launch specs
    /// at the restored paths at plan time. No executable bytes or historical
    /// identities are restored. The restored generation's reviewed-program
    /// record is copied: its reviewers are known. If that generation has no
    /// record, the rollback has none either, so its modules owe re-review.
    pub fn rollback(
        &mut self,
        store: &mut ConfigStore,
        generation: u64,
        observed: &GateSnapshot,
        current_programs: &GateSnapshot,
        review: impl FnOnce(&GateSnapshot, &GateSnapshot) -> Result<Changeset>,
    ) -> Result<ConfigPlanView> {
        let mut target = GateSnapshot::from_store(&store.rollback_configuration(generation)?)?;
        runtime::policy_root::preserve_root_on_rollback(store.body()?, generation, &mut target)?;
        target.inputs.retain(|key, _| !key.is_project());
        target.restore_programs_from(current_programs)?;
        let copied = store
            .body()?
            .generations
            .get(&generation)
            .and_then(|entry| {
                entry
                    .generation
                    .changeset
                    .apply_plan
                    .get(reviewed_build::REVIEWED_PROGRAMS)
            })
            .cloned();
        self.plan_reviewed(
            store,
            PlanKind::Rollback(generation),
            target,
            observed,
            ReviewRecord::Copied(copied),
            review,
        )
    }

    fn fresh(plan: &PendingPlan, store: &mut ConfigStore, current: &GateSnapshot) -> Result<()> {
        plan.targets()?;
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
            .map_err(|_| {
                if plan.project_capture.is_some() {
                    capture_plan::changed()
                } else {
                    GateError::stale()
                }
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        digest: &Digest,
        approval_channel: subc_config::ApprovalChannel,
        store: &mut ConfigStore,
        current: &GateSnapshot,
        now_ms: u64,
        before_publication: impl FnOnce() -> Result<Option<crate::policy_root::ledger::Checkpoint>>,
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
        let declarations_only = plan.development_approval.is_some();
        generation.active_generations = plan.view.plan.active_generations.clone();
        let tuples = plan.targets()?;
        for tuple in &tuples {
            let module = &tuple.module;
            if tuple.project.is_none() {
                generation
                    .module_states
                    .insert(module.clone(), subc_config::GenerationState::Pending);
            }
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
        let snapshot = plan.snapshot.to_store()?;
        let mut entry = HistoryEntry {
            generation,
            kind: if declarations_only {
                CommitKind::DeclarationApproval
            } else {
                plan.kind.commit_kind()
            },
            approval_id: digest.to_string(),
            outcome: "approved".into(),
            tuples,
            project_outcomes: BTreeMap::new(),
            configuration: snapshot.clone(),
            delivery: plan.delivery.clone(),
        };
        let base_before = store.status().base_revision;
        let mut observed = current.clone();
        if !plan.recovery {
            let body = store.body()?;
            let previous = body.observed.as_ref().unwrap_or(&body.approved);
            let previous = GateSnapshot::from_store(previous)?;
            // Project file observations are maintained by the project capture,
            // not by the user-only file capturer. Keep deferred observations.
            observed.inputs.retain(|key, _| !key.is_project());
            observed.carry_project_inputs(&previous);
            observed.carry_project_inputs(&plan.snapshot);
            if let Some(capture) = &plan.project_capture {
                observed
                    .inputs
                    .extend(capture.edit_observations.inputs.clone());
                for (key, input) in &plan.snapshot.inputs {
                    if matches!(key, InputKey::ProjectTuple { .. }) {
                        observed.inputs.insert(key.clone(), input.clone());
                    }
                }
                for key in observed.inputs.keys().cloned().collect::<Vec<_>>() {
                    if matches!(&key, InputKey::Layer { module, .. } if capture.held_user.contains(module))
                    {
                        if let Some(input) = previous.inputs.get(&key) {
                            observed.inputs.insert(key, input.clone());
                        } else {
                            observed.inputs.remove(&key);
                        }
                    }
                }
            }
        }
        let observed = observed.to_store()?;
        if let Some(checkpoint) = before_publication()? {
            // This is a publication receipt, not a change to the reviewed
            // inputs: it describes the ledger after the durable append.
            entry.generation.changeset.apply_plan[crate::policy_root::ledger::CHECKPOINT_FIELD] =
                serde_json::to_value(checkpoint).map_err(GateError::invalid)?;
        }
        let result = if plan.recovery {
            store.recovery_exit(entry, snapshot, now_ms)
        } else {
            store.commit_generation_with_project_files(
                entry,
                snapshot,
                observed,
                plan.project_files.clone(),
                now_ms,
            )
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
