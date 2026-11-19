use std::collections::{BTreeMap, BTreeSet};

use subc_config::{Changeset, Digest, InputVector, Plan};
use subc_protocol::config::{ConfigDeclareRequest, ConfigView};

use crate::config_store::{
    DeliverySnapshot, ProjectCutoffs, ProjectFileApproval, StoreBody, Tuple,
};

use super::{GateError, Result};

const TARGETS: &str = "tuple_targets";
const USER_INPUT: &str = "module:";
const PROJECT_INPUT: &str = "project_tuple:";

/// Supply the daemon-computed view checked by the module's review, the user and
/// workspace cutoffs (the views below the project layer, each computed
/// independently), its normalized declaration and the digest of the admitted
/// build that answered the review. Do not use field deltas to construct this
/// payload: a fully overridden layer can still change approval.
pub(crate) struct ProjectReview {
    pub view: ConfigView,
    pub cutoffs: ProjectCutoffs,
    pub declaration: ConfigDeclareRequest,
    pub reviewed_build: Digest,
}

impl ProjectReview {
    fn approval_digest(&self) -> Result<Digest> {
        fn content(view: &ConfigView) -> impl serde::Serialize + '_ {
            (
                &view.project_id,
                &view.effective,
                &view.sources,
                &view.layers,
            )
        }
        subc_config::digest(
            subc_config::PURPOSE_SNAPSHOT,
            &serde_json::json!([
                "project_tuple_v1",
                content(&self.view),
                content(&self.cutoffs.user),
                content(&self.cutoffs.workspace),
                self.declaration,
                self.reviewed_build,
            ]),
        )
        .map_err(GateError::invalid)
    }
}

/// Give the gate one payload per successfully reviewed project (including
/// unchanged reviews), plus any approved physical-file updates. The captured
/// snapshots must carry a `ProjectTuple` input witness for each reviewed tuple,
/// including an absent location when planning a first file. Do not include
/// skipped projects or their held file changes in the reviewed payloads.
#[derive(Default)]
pub(crate) struct ProjectReviews {
    pub tuples: BTreeMap<Tuple, ProjectReview>,
    pub files: Vec<ProjectFileApproval>,
}

impl ProjectReviews {
    pub(super) fn bind(
        self,
        store: Option<&StoreBody>,
        inputs: &InputVector,
        changeset: &mut Changeset,
        user_modules: &BTreeSet<String>,
    ) -> Result<(DeliverySnapshot, Vec<ProjectFileApproval>)> {
        let mut delivery = DeliverySnapshot::default();
        let mut targets: BTreeSet<_> = read_targets(inputs)?
            .into_iter()
            .filter(|tuple| tuple.project.is_none() && user_modules.contains(&tuple.module))
            .collect();
        for (tuple, review) in self.tuples {
            let project = tuple
                .project
                .as_ref()
                .filter(|id| review.view.project_id.as_ref() == Some(id))
                .ok_or_else(|| invalid("project review view names a different tuple"))?;
            if !inputs
                .inputs
                .contains_key(&project_input_key(&tuple.module, project))
            {
                return Err(invalid("reviewed project is outside the plan read set"));
            }
            let approval_digest = review.approval_digest()?;
            let previous = store
                .and_then(|body| {
                    body.generations
                        .values()
                        .rev()
                        .find(|entry| entry.tuples.contains(&tuple))
                })
                .and_then(|entry| entry.delivery.as_ref())
                .and_then(|delivery| delivery.project_approval_digests.get(&tuple.module))
                .and_then(|projects| projects.get(project));
            if previous == Some(&approval_digest) {
                continue;
            }
            if delivery
                .declarations
                .get(&tuple.module)
                .is_some_and(|prior| *prior != review.declaration)
            {
                return Err(invalid(
                    "one generation cannot approve different declarations of a module",
                ));
            }
            delivery
                .declarations
                .insert(tuple.module.clone(), review.declaration);
            delivery
                .project_views
                .entry(tuple.module.clone())
                .or_default()
                .insert(project.clone(), review.view);
            delivery
                .project_cutoffs
                .entry(tuple.module.clone())
                .or_default()
                .insert(project.clone(), review.cutoffs);
            delivery
                .project_approval_digests
                .entry(tuple.module.clone())
                .or_default()
                .insert(project.clone(), approval_digest);
            targets.insert(tuple);
        }

        changeset.affected_projects = targets
            .iter()
            .filter_map(|tuple| tuple.project.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        changeset.apply_plan[TARGETS] = serde_json::json!(targets);
        changeset.apply_plan["project_approval_digests"] =
            serde_json::json!(delivery.project_approval_digests);
        if !self.files.is_empty() {
            changeset.apply_plan["project_file_approvals"] = serde_json::Value::Array(
                self.files
                    .iter()
                    .map(ProjectFileApproval::bound_record)
                    .collect(),
            );
        }
        Ok((delivery, self.files))
    }
}

pub(super) fn project_input_key(module: &str, project: &str) -> String {
    format!("{PROJECT_INPUT}{}", serde_json::json!([module, project]))
}

fn invalid(detail: &str) -> GateError {
    GateError::new("apply_plan_invalid", detail)
}

fn read_targets(inputs: &InputVector) -> Result<BTreeSet<Tuple>> {
    inputs
        .inputs
        .keys()
        .filter_map(|key| {
            if let Some(module) = key.strip_prefix(USER_INPUT) {
                Some(serde_json::from_str(module).map(|module| Tuple {
                    module,
                    project: None,
                }))
            } else {
                key.strip_prefix(PROJECT_INPUT).map(|project| {
                    serde_json::from_str::<(String, String)>(project).map(|(module, project)| {
                        Tuple {
                            module,
                            project: Some(project),
                        }
                    })
                })
            }
        })
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| invalid("plan read set contains a malformed tuple identity"))
}

pub(super) fn checked(plan: &Plan) -> Result<BTreeSet<Tuple>> {
    let allowed = read_targets(&plan.input_vector)?;
    let has_project_inputs = allowed.iter().any(|tuple| tuple.project.is_some());
    let Some(explicit) = plan.changeset.apply_plan.get(TARGETS) else {
        if has_project_inputs {
            return Err(invalid("project plan is missing its bound tuple targets"));
        }
        // For byte compatibility, an absent list means user-only. Its targets
        // are the user tuples in the already digest-bound module read set.
        return Ok(allowed);
    };
    let sorted: Vec<Tuple> = serde_json::from_value(explicit.clone())
        .map_err(|_| invalid("invalid bound tuple targets"))?;
    if sorted.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid("bound tuple targets must be sorted and unique"));
    }
    let targets: BTreeSet<_> = sorted.into_iter().collect();
    if !targets.is_subset(&allowed) {
        return Err(invalid("bound tuple target is outside the plan read set"));
    }
    if !has_project_inputs && targets != allowed {
        return Err(invalid(
            "bound tuple targets disagree with the user-only read set",
        ));
    }
    Ok(targets)
}
