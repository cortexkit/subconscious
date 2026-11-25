use std::sync::Arc;

use subc_control::config::ConfigPlanView;
use subc_protocol::config::ConfigReviewReply;

use crate::config_gate::{
    capture_plan::CaptureReview, project_registry::RegistryScope, review::checked_fields,
};
use crate::config_store::Tuple;

use super::*;

type WorkspaceReads = Vec<(
    capture_plan::CaptureAuthority,
    crate::config_store::ProjectFileTarget,
)>;

pub(super) fn reviewed_workspace_reads(
    context: Option<&ProjectCapture>,
    files: &[crate::config_store::ProjectFileApproval],
) -> Result<WorkspaceReads> {
    files
        .iter()
        .filter_map(|file| {
            let target = file.target();
            (matches!(file, crate::config_store::ProjectFileApproval::Adopt { .. })
                && matches!(
                    target.layer,
                    crate::config_store::ProjectFileLayer::Workspace { .. }
                ))
            .then(|| {
                let authority = context
                    .and_then(|context| context.authority.get(&target.module))
                    .ok_or_else(capture_plan::changed)?;
                Ok((authority.clone(), target.clone()))
            })
        })
        .collect()
}

/// Checked registry evidence for exactly one pending capture plan.
#[derive(Clone)]
pub(crate) struct CaptureApply {
    pub(super) digest: Digest,
    pub(super) enumeration: Option<cortexkit_project_identity::EnumerateReply>,
    pub(super) revisions: BTreeMap<String, super::super::project_registry::RegistryRevision>,
}

impl RuntimeOwner {
    pub(super) fn read_settings_file_witness(
        &self,
        users: &BTreeSet<String>,
        workspaces: WorkspaceReads,
    ) -> Result<Digest> {
        let mut observed = BTreeMap::new();
        for (authority, target) in workspaces {
            let path = target.path().ok_or_else(capture_plan::changed)?;
            let pool = self
                .capturer
                .projects
                .root_io()
                .ok_or_else(capture_plan::changed)?;
            // Root workers may outlive a timed-out observer. They only read,
            // so waiting for this bounded result never holds the owner mutex.
            let current = pool.read_blocking(target.root.clone(), move || {
                authority.read_reviewed_workspace(&target)
            })??;
            observed.insert(path, current);
        }
        serde_json::to_vec(&(users, observed))
            .map(|bytes| subc_config::raw_digest(&bytes))
            .map_err(GateError::invalid)
    }

    pub(super) fn recheck_settings_file_witness(&self, digest: &Digest) -> Result<Option<Digest>> {
        let (users, workspaces, expected) = {
            let state = self.lock()?;
            let Some(plan) = state.gate.plans.get(digest) else {
                return Ok(None);
            };
            (
                plan.settings_users.clone(),
                reviewed_workspace_reads(plan.project_capture.as_ref(), &plan.project_files)?,
                plan.settings_file_witness.clone(),
            )
        };
        if users.is_none() && workspaces.is_empty() {
            return Ok(None);
        }
        let current =
            self.read_settings_file_witness(&users.clone().unwrap_or_default(), workspaces)?;
        if users.is_none() {
            return Ok(None);
        }
        if expected.as_ref() != Some(&current) {
            return Err(capture_plan::changed());
        }
        Ok(Some(current))
    }

    pub(crate) async fn prepare_capture(self: &Arc<Self>, kind: PlanKind) -> Result<PreparedPlan> {
        if kind != PlanKind::Change || !self.project_capture_enabled {
            return self.on_blocking(move |owner| owner.prepare(kind)).await;
        }
        let registry = self
            .on_blocking(|owner| {
                let mut state = owner.lock()?;
                Self::check_authority(&mut state, false)?;
                if state.recovery_exit_hold || state.store.status().state != StoreState::Stamped {
                    return Ok(None);
                }
                let scopes = Self::declarations(&mut state)?
                    .into_keys()
                    .map(|module| (module, RegistryScope::all()))
                    .collect::<BTreeMap<_, _>>();
                Ok(Some((state.store.body()?.base_revision, scopes)))
            })
            .await?;
        let Some((base_revision, scopes)) = registry else {
            return self.on_blocking(move |owner| owner.prepare(kind)).await;
        };
        let batch = self.capturer.projects.capture_batch(&scopes).await;
        self.on_blocking(move |owner| owner.prepare_inner(kind, Some((base_revision, batch))))
            .await
    }

    pub(crate) async fn recheck_capture_apply(
        self: &Arc<Self>,
        attempt: u64,
    ) -> Result<Option<CaptureApply>> {
        let capture = self
            .on_blocking(move |owner| {
                let state = owner.lock()?;
                let current_base = state.store.loaded_admission_state().1.base_revision;
                let digest = state.gate.approval_digest(attempt)?;
                let plan = &state.gate.plans[&digest];
                let Some(capture) = &plan.project_capture else {
                    return Ok(None);
                };
                if current_base != plan.view.plan.base_revision {
                    return Err(capture_plan::changed());
                }
                Ok((!capture.authority.is_empty()).then(|| (digest, capture.authority.clone())))
            })
            .await?;
        let Some((digest, authority)) = capture else {
            return Ok(None);
        };
        let scopes = authority
            .iter()
            .map(|(module, witness)| (module.clone(), witness.scope.clone()))
            .collect();
        let (enumeration, current) = self
            .capturer
            .projects
            .capture_batch(&scopes)
            .await
            .map_err(capture_plan::registry_retry)?;
        for (module, witness) in &authority {
            witness.recheck(current.get(module).ok_or_else(capture_plan::changed)?)?;
        }
        Ok(Some(CaptureApply {
            digest,
            enumeration,
            revisions: current,
        }))
    }

    pub(super) fn capture_with_project_context(
        &self,
        state: &mut OwnerState,
        modules: &BTreeSet<String>,
        projects: &ProjectCapture,
    ) -> Result<CapturedConfiguration> {
        let declarations = Self::declarations(state)?;
        let body = state.store.body()?;
        let approved = super::super::GateSnapshot::from_store(&body.approved)?;
        let held =
            capture_plan::retained_user_files(body, &approved, &declarations, &projects.held_user);
        self.capturer
            .capture_with_retained_user(&declarations, Some(modules), held)
    }

    pub(crate) fn capture_refusal(
        &self,
        prepared: &PreparedPlan,
    ) -> Result<Option<ConfigPlanView>> {
        let Some(projects) = prepared
            .project_capture
            .as_ref()
            .filter(|plan| !plan.refusals.is_empty())
        else {
            return Ok(None);
        };
        let state = self.lock()?;
        let (store_state, body) = state.store.loaded_admission_state();
        let mut view = projects.refused_view(
            &state.gate.incarnation,
            prepared.base_revision,
            body.last_generation,
            prepared.read_set.clone(),
        )?;
        view.unstamped = store_state == StoreState::Unstamped;
        if let Some(edit) = &prepared.edits {
            view.refusals.extend(edit.refusals.clone());
        }
        Ok(Some(view))
    }
}

impl PreparedPlan {
    pub(crate) fn refuse_project_review(&mut self, tuple: &Tuple, reply: &ConfigReviewReply) {
        if let Some(plan) = &mut self.project_capture {
            let paths: BTreeSet<_> = reply
                .refusals
                .iter()
                .flat_map(|refusal| &refusal.paths)
                .filter(|path| path.starts_with('/') && path.len() <= 1024)
                .cloned()
                .collect();
            let paths = if paths.is_empty() {
                BTreeSet::from([String::new()])
            } else {
                paths
            };
            for path in paths {
                plan.refusals.push(subc_control::config::ConfigPlanRefusal {
                    module_id: tuple.module.clone(),
                    project_id: tuple.project.clone(),
                    workspace_id: None,
                    path,
                    code: "module_review_refused".into(),
                    message: "the module refused the project edits".into(),
                });
            }
        }
    }

    pub(crate) fn capture_refused(&self) -> bool {
        self.project_capture
            .as_ref()
            .is_some_and(|capture| !capture.refusals.is_empty())
    }

    pub(crate) fn project_reviews(&self) -> impl Iterator<Item = (&Tuple, &CaptureReview)> {
        self.project_capture.iter().flat_map(|plan| &plan.reviews)
    }

    pub(crate) fn accept_project_review(
        &mut self,
        tuple: &Tuple,
        reply: ConfigReviewReply,
    ) -> Result<Vec<subc_config::FieldChange>> {
        let review = self
            .project_capture
            .as_ref()
            .and_then(|plan| plan.reviews.get(tuple))
            .ok_or_else(|| {
                GateError::new("module_review_invalid", "project did not request a review")
            })?;
        let fields = checked_fields(
            &tuple.module,
            &review.schema,
            review.base.as_ref(),
            review.base_schema.as_ref(),
            &review.approval.view,
            reply,
        )?;
        let review = self
            .project_capture
            .as_mut()
            .and_then(|plan| plan.reviews.remove(tuple))
            .expect("the checked review is still present");
        self.projects
            .get_or_insert_with(Default::default)
            .tuples
            .insert(tuple.clone(), review.approval);
        Ok(fields)
    }
}
