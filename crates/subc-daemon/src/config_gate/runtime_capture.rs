use std::sync::Arc;

use subc_control::config::ConfigPlanView;
use subc_protocol::config::ConfigReviewReply;

use crate::config_gate::{
    capture_plan::CaptureReview, project_registry::RegistryScope, review::checked_fields,
};
use crate::config_store::Tuple;

use super::*;

/// Checked registry evidence for exactly one pending capture plan.
pub(crate) struct CaptureApply {
    pub(super) digest: Digest,
    pub(super) enumeration: Option<cortexkit_project_identity::EnumerateReply>,
}

impl RuntimeOwner {
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
        Ok(Some(projects.refused_view(
            &state.gate.incarnation,
            prepared.base_revision,
            state.store.loaded_admission_state().1.last_generation,
            prepared.read_set.clone(),
        )?))
    }
}

impl PreparedPlan {
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
