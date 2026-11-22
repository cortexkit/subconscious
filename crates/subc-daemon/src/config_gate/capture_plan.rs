//! A file scan records pending changes separately from the inputs a plan can approve.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use subc_config::{Digest, InputState, Layer, Plan, RecordedValue, Schema};
use subc_control::config::{ConfigPlanRefusal, ConfigPlanView};
use subc_protocol::config::{ConfigLayer, ConfigLayerSnapshot, ConfigStatus, ConfigView};

use crate::config_store::{
    ProjectCutoffs, ProjectFileApproval, ProjectFileLayer, ProjectFileTarget, StoreBody, Tuple,
};

use super::{
    capture::{CapturedConfiguration, CapturedFile, CapturedModule},
    project_layers::ProjectLayerSchema,
    project_registry::{layer_path, RegistryProject, RegistryRevision, RegistryScope},
    targets::ProjectReview,
    CapturedInput, GateError, GateSnapshot, InputKey, Result,
};

pub(super) struct ModuleRoots {
    revision: RegistryRevision,
    skipped: BTreeSet<String>,
    held: BTreeSet<PathBuf>,
    physical: BTreeMap<PathBuf, Option<Digest>>,
}

#[derive(Clone)]
pub(super) struct CaptureAuthority {
    pub scope: RegistryScope,
    saved: RegistryRevision,
    held: BTreeSet<PathBuf>,
    readers: BTreeSet<String>,
}

impl CaptureAuthority {
    pub fn for_edit(scope: RegistryScope, saved: RegistryRevision) -> Self {
        Self {
            scope: scope.targets_only(),
            saved,
            held: BTreeSet::new(),
            readers: BTreeSet::new(),
        }
    }

    pub fn revision_digest(&self) -> Result<Digest> {
        self.saved.digest()
    }

    pub fn check_busy(&self, pool: &super::root_io::RootIoPool) -> Result<()> {
        if self
            .saved
            .projects
            .iter()
            .flat_map(RegistryProject::roots)
            .any(|root| pool.is_busy(&root.path))
        {
            return Err(GateError::from(super::root_io::RootIoError::RootBusy));
        }
        Ok(())
    }

    pub fn recheck(&self, current: &RegistryRevision) -> Result<()> {
        self.saved.recheck(current, &self.scope)?;
        for project in &current.projects {
            if !self.readers.contains(&project.project_id) {
                continue;
            }
            for root in project.roots() {
                if self.held.contains(&layer_path(&root.path, &current.module)) {
                    continue;
                }
                let (_, then) = self.saved.layer_location(&project.project_id, &root.path)?;
                let (_, now) = current
                    .layer_location(&project.project_id, &root.path)
                    .map_err(|error| {
                        if error.code.starts_with("layer_") {
                            changed()
                        } else {
                            error
                        }
                    })?;
                if !then.same_identity(now)
                    || then.file.as_ref().map(|file| &file.digest)
                        != now.file.as_ref().map(|file| &file.digest)
                {
                    return Err(changed());
                }
            }
        }
        Ok(())
    }
}

pub(super) fn changed() -> GateError {
    GateError::new(
        "config_changed_since_review",
        "reviewed configuration changed; plan again",
    )
}

pub(super) fn registry_retry(error: GateError) -> GateError {
    if error.code == "project_registry_unavailable" {
        GateError::new(
            &error.code,
            format!(
                "the project list is unavailable, try again: {}",
                error.detail
            ),
        )
    } else {
        error
    }
}

pub(super) fn known_projects(body: &StoreBody) -> bool {
    body.registry_membership
        .as_ref()
        .is_some_and(|membership| membership.ever_had_projects || !membership.projects.is_empty())
        || body
            .versions
            .iter()
            .any(|(tuple, _)| tuple.project.is_some())
}

pub(super) fn notice(
    module: &str,
    project: Option<&RegistryProject>,
    path: String,
    error: GateError,
) -> ConfigPlanRefusal {
    ConfigPlanRefusal {
        module_id: module.into(),
        project_id: project.map(|project| project.project_id.clone()),
        workspace_id: project.and_then(|project| project.workspace_id.clone()),
        path,
        code: error.code,
        message: error.detail,
    }
}

pub(super) fn file_key(module: &str, path: &Path) -> InputKey {
    InputKey::ProjectFile {
        module: module.into(),
        path: path.to_string_lossy().into_owned(),
    }
}

pub(super) fn file_input(digest: Option<Digest>) -> CapturedInput {
    CapturedInput {
        state: digest.map_or(InputState::Absent, |digest| InputState::Present { digest }),
        value: RecordedValue::new(serde_json::Value::Null),
    }
}

impl ModuleRoots {
    pub fn new(
        revision: RegistryRevision,
        approved: &GateSnapshot,
        notices: &mut Vec<ConfigPlanRefusal>,
    ) -> Result<Self> {
        let mut skipped = BTreeSet::new();
        let mut held = BTreeSet::new();
        for project in &revision.projects {
            if let Err(error) = revision.project_layers(&project.project_id) {
                if error.code != "config_unavailable" && !error.code.starts_with("layer_") {
                    return Err(error);
                }
                notices.push(notice(
                    &revision.module,
                    Some(project),
                    String::new(),
                    error,
                ));
                skipped.insert(project.project_id.clone());
                held.extend(
                    project
                        .roots()
                        .map(|root| layer_path(&root.path, &revision.module)),
                );
            }
        }
        // Substituting an approved held file can restore a second project
        // layer that was absent on disk. Defer that reader and propagate its
        // hold before deriving the publishable physical-file changes.
        loop {
            let mut newly_skipped = Vec::new();
            for project in &revision.projects {
                if skipped.contains(&project.project_id) {
                    continue;
                }
                let present = project
                    .project_roots
                    .iter()
                    .filter(|root| {
                        let path = layer_path(&root.path, &revision.module);
                        if held.contains(&path) {
                            return approved
                                .inputs
                                .get(&file_key(&revision.module, &path))
                                .is_some_and(|input| {
                                    matches!(input.state, InputState::Present { .. })
                                });
                        }
                        revision
                            .layer_location(&project.project_id, &root.path)
                            .is_ok_and(|(_, location)| location.file.is_some())
                    })
                    .take(2)
                    .count();
                if present > 1 {
                    newly_skipped.push(project);
                }
            }
            if newly_skipped.is_empty() {
                break;
            }
            for project in newly_skipped {
                notices.push(notice(&revision.module, Some(project), String::new(), GateError::new(
                    "layer_root_ambiguous", "more than one project root supplies a layer when held files use approved bytes",
                )));
                skipped.insert(project.project_id.clone());
                held.extend(
                    project
                        .roots()
                        .map(|root| layer_path(&root.path, &revision.module)),
                );
            }
        }
        let mut physical = BTreeMap::new();
        for project in &revision.projects {
            for root in project.roots() {
                if let Ok((_, location)) = revision.layer_location(&project.project_id, &root.path)
                {
                    physical.insert(
                        layer_path(&root.path, &revision.module),
                        location.file.as_ref().map(|file| file.digest.clone()),
                    );
                }
            }
        }
        Ok(Self {
            revision,
            skipped,
            held,
            physical,
        })
    }

    pub fn holds_user(&self) -> bool {
        !self.skipped.is_empty()
    }

    pub fn observe(
        &self,
        approved: &GateSnapshot,
        observed: &mut GateSnapshot,
        notices: &mut Vec<ConfigPlanRefusal>,
    ) {
        let module = &self.revision.module;
        for (path, digest) in &self.physical {
            let key = file_key(module, path);
            if digest.is_none()
                && !approved.inputs.contains_key(&key)
                && !observed.inputs.contains_key(&key)
            {
                continue;
            }
            let input = file_input(digest.clone());
            if self.held.contains(path)
                && input.state
                    != approved
                        .inputs
                        .get(&key)
                        .map_or(InputState::Absent, |input| input.state.clone())
            {
                notices.push(held_notice(module, path));
            }
            observed.inputs.insert(key, input);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn review(
        &self,
        body: &StoreBody,
        approved: &GateSnapshot,
        previous_user: Option<&ConfigView>,
        captured: &CapturedModule,
        candidate: &mut GateSnapshot,
        reviews: &mut BTreeMap<Tuple, CaptureReview>,
        files: &mut Vec<ProjectFileApproval>,
        refusals: &mut Vec<ConfigPlanRefusal>,
    ) -> Result<Option<CaptureAuthority>> {
        let module = &self.revision.module;
        let schema = ProjectLayerSchema::new(captured.schema.clone());
        let approved_files = approved_files(body, approved, module);
        let user_snapshot = &captured.view.layers[0];
        let user_changed =
            previous_user.and_then(|view| view.layers.first()) != Some(user_snapshot);
        let build = super::reviewed_build::active(body, module)?
            .map(Digest::try_from)
            .transpose()
            .map_err(GateError::invalid)?
            .unwrap_or_else(|| super::program::approved_base(approved, module));
        let mut scope = RegistryScope::default();
        let mut readers = BTreeSet::new();
        if user_changed && !self.holds_user() {
            scope.include_all();
            readers.extend(
                self.revision
                    .projects
                    .iter()
                    .map(|project| project.project_id.clone()),
            );
        }
        for (path, digest) in &self.physical {
            let key = file_key(module, path);
            let input = file_input(digest.clone());
            if self.held.contains(path)
                || (!body.project_write_back.contains_key(path)
                    && input.state
                        == approved
                            .inputs
                            .get(&key)
                            .map_or(InputState::Absent, |input| input.state.clone()))
            {
                continue;
            }
            let Some((project, root)) = self.revision.projects.iter().find_map(|project| {
                project
                    .roots()
                    .find(|root| layer_path(&root.path, module) == *path)
                    .map(|root| (project, root))
            }) else {
                continue;
            };
            scope.paths.insert(path.clone());
            for project in &self.revision.projects {
                if project
                    .workspace_root
                    .as_ref()
                    .is_some_and(|root| layer_path(&root.path, module) == *path)
                {
                    scope.workspaces.extend(project.workspace_id.clone());
                }
            }
            let layer = if project
                .project_roots
                .iter()
                .any(|project_root| project_root.path == root.path)
            {
                ProjectFileLayer::Project {
                    project_id: project.project_id.clone(),
                }
            } else {
                ProjectFileLayer::Workspace {
                    workspace_id: project
                        .workspace_id
                        .clone()
                        .expect("workspace root has an id"),
                }
            };
            files.push(ProjectFileApproval::Adopt {
                target: ProjectFileTarget {
                    module: module.clone(),
                    layer,
                    registry_revision: self.revision.digest_for_project(&project.project_id)?,
                    root: root.path.clone(),
                    root_identity: root.file_id.clone().ok_or_else(changed)?,
                    relative_path: Path::new(".cortexkit").join(format!("{module}.jsonc")),
                },
                observed_digest: digest.clone(),
            });
            candidate.inputs.insert(key, input);
        }
        for project in &self.revision.projects {
            let id = &project.project_id;
            if self.skipped.contains(id) {
                continue;
            }
            let tuple = Tuple {
                module: module.clone(),
                project: Some(id.clone()),
            };
            let previous = body
                .generations
                .values()
                .rev()
                .find(|entry| entry.tuples.contains(&tuple));
            let base_delivery = previous.and_then(|entry| entry.delivery.as_ref());
            let mut base = base_delivery
                .and_then(|d| d.project_views.get(module)?.get(id))
                .cloned();
            if let (Some(base), Some(previous)) = (&mut base, previous) {
                base.generation = previous.generation.id;
                base.status = if body
                    .versions
                    .iter()
                    .any(|(key, v)| *key == tuple && v.active == Some(base.generation))
                {
                    ConfigStatus::Active
                } else {
                    ConfigStatus::Pending
                };
            }
            let base_declaration = base_delivery.and_then(|d| d.declarations.get(module));
            let layers = self.layers(project, &approved_files)?;
            if base.is_none() && layers.is_empty() {
                continue;
            }
            let mut snapshots = vec![user_snapshot.clone()];
            snapshots[0].project_id = Some(id.clone());
            snapshots.extend(layers);
            let candidate_review = build_review(
                module,
                id,
                captured,
                &schema,
                build.clone(),
                base,
                base_declaration,
                snapshots,
            );
            match candidate_review {
                Ok(mut review) => {
                    review.approval.registry_identity = Some(self.revision.digest_for_project(id)?);
                    let digest = review.approval.approval_digest()?;
                    let prior =
                        base_delivery.and_then(|d| d.project_approval_digests.get(module)?.get(id));
                    if prior == Some(&digest) {
                        continue;
                    }
                    let key = InputKey::ProjectTuple {
                        module: module.clone(),
                        project: id.clone(),
                    };
                    candidate.inputs.insert(key, file_input(Some(digest)));
                    scope.projects.insert(id.clone());
                    readers.insert(id.clone());
                    reviews.insert(tuple, review);
                }
                Err(error) => refusals.push(notice(module, Some(project), String::new(), error)),
            }
        }
        readers.extend(
            scope
                .paths
                .iter()
                .flat_map(|path| self.revision.readers(path)),
        );
        if scope.paths.is_empty() && scope.projects.is_empty() && !user_changed {
            return Ok(None);
        }
        Ok(Some(CaptureAuthority {
            scope,
            saved: self.revision.clone(),
            held: self.held.clone(),
            readers,
        }))
    }

    fn layers(
        &self,
        project: &RegistryProject,
        approved: &BTreeMap<PathBuf, &ConfigLayerSnapshot>,
    ) -> Result<Vec<ConfigLayerSnapshot>> {
        tuple_layers(&self.revision.module, project, |root, path, _layer| {
            if self.held.contains(path) {
                Ok(approved.get(path).and_then(|layer| layer.bytes.clone()))
            } else {
                let (_, location) = self.revision.layer_location(&project.project_id, root)?;
                Ok(location.file.as_ref().map(|file| file.bytes.clone()))
            }
        })
    }
}

pub(super) fn tuple_layers(
    module: &str,
    project: &RegistryProject,
    mut bytes: impl FnMut(&Path, &Path, &ConfigLayer) -> Result<Option<Vec<u8>>>,
) -> Result<Vec<ConfigLayerSnapshot>> {
    let mut input = |root: &Path, layer| -> Result<Option<ConfigLayerSnapshot>> {
        let path = layer_path(root, module);
        let bytes = bytes(root, &path, &layer)?;
        Ok(bytes.map(|bytes| {
            let mut snapshot =
                ConfigLayerSnapshot::file(module, layer, path.to_string_lossy(), Some(bytes));
            snapshot.project_id = Some(project.project_id.clone());
            if snapshot.layer == ConfigLayer::Workspace {
                snapshot.workspace_id.clone_from(&project.workspace_id);
            }
            snapshot
        }))
    };
    let mut higher = None;
    for root in &project.project_roots {
        if let Some(layer) = input(&root.path, ConfigLayer::Project)? {
            if higher.is_some() {
                return Err(GateError::new(
                    "layer_root_ambiguous",
                    "more than one approved project root supplies a layer",
                ));
            }
            higher = Some(layer);
        }
    }
    let lower = project
        .workspace_root
        .as_ref()
        .map(|root| input(&root.path, ConfigLayer::Workspace))
        .transpose()?
        .flatten();
    Ok(lower
        .filter(|lower| {
            higher
                .as_ref()
                .is_none_or(|higher| higher.path != lower.path)
        })
        .into_iter()
        .chain(higher)
        .collect())
}

pub(super) fn approved_files<'a>(
    body: &'a StoreBody,
    approved: &GateSnapshot,
    module: &str,
) -> BTreeMap<PathBuf, &'a ConfigLayerSnapshot> {
    body.generations.values().filter_map(|entry| {
        entry.delivery.as_ref()?.project_views.get(module)
    }).flat_map(|projects| projects.values()).flat_map(|view| &view.layers)
        .filter(|layer| layer.layer != ConfigLayer::User)
        .filter_map(|layer| {
            let path = PathBuf::from(layer.path.as_ref()?);
            let input = approved.inputs.get(&file_key(module, &path))?;
            matches!(&input.state, InputState::Present { digest } if Some(digest) == layer.digest.as_ref())
                .then_some((path, layer))
        }).collect()
}

fn held_notice(module: &str, path: &Path) -> ConfigPlanRefusal {
    notice(
        module,
        None,
        path.to_string_lossy().into_owned(),
        GateError::new(
            "config_unavailable",
            "file change held because a project that reads it was skipped",
        ),
    )
}

pub(crate) struct CaptureReview {
    pub approval: ProjectReview,
    pub schema: Schema,
    pub base: Option<ConfigView>,
    pub base_schema: Option<Schema>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_review(
    module: &str,
    project: &str,
    captured: &CapturedModule,
    schema: &ProjectLayerSchema,
    reviewed_build: Digest,
    base: Option<ConfigView>,
    base_declaration: Option<&subc_protocol::config::ConfigDeclareRequest>,
    layers: Vec<ConfigLayerSnapshot>,
) -> Result<CaptureReview> {
    let base_schema = base_declaration
        .map(|declaration| {
            Schema::new(declaration.schema.clone()).map_err(|_| {
                GateError::new(
                    "approved_schema_unavailable",
                    "approved project schema is damaged",
                )
            })
        })
        .transpose()?;
    let user = captured
        .file
        .bytes
        .as_deref()
        .map(super::capture::parse_layer)
        .transpose()?;
    let recorded = layers[1..]
        .iter()
        .map(|layer| {
            let kind = if layer.layer == ConfigLayer::Workspace {
                Layer::Workspace
            } else {
                Layer::Project
            };
            schema.record_layer(
                kind,
                layer.bytes.as_deref().expect("only present project layers"),
                module,
                project,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let reviewed = schema.review_tuple(
        module,
        project,
        user.clone(),
        &recorded,
        base_schema.as_ref().zip(base.as_ref()),
    )?;
    let workspace_end = if layers
        .get(1)
        .is_some_and(|layer| layer.layer == ConfigLayer::Workspace)
    {
        2
    } else {
        1
    };
    let prefix =
        schema.review_tuple(module, project, user, &recorded[..workspace_end - 1], None)?;
    let view = |reviewed: subc_config::ReviewedTuple, layers| {
        let mut view = ConfigView::new(
            0,
            reviewed.effective,
            reviewed.sources,
            layers,
            ConfigStatus::Pending,
        );
        view.project_id = Some(project.into());
        view
    };
    let mut user = captured.view.clone();
    user.project_id = Some(project.into());
    user.layers.truncate(1);
    user.layers[0].project_id = Some(project.into());
    let workspace = view(prefix, layers[..workspace_end].to_vec());
    Ok(CaptureReview {
        approval: ProjectReview {
            view: view(reviewed, layers),
            cutoffs: ProjectCutoffs { user, workspace },
            declaration: captured.declaration.clone(),
            reviewed_build,
            registry_identity: None,
        },
        schema: captured.schema.clone(),
        base,
        base_schema,
    })
}

#[derive(Clone, Default)]
pub(super) struct ProjectCapture {
    pub authority: BTreeMap<String, CaptureAuthority>,
    pub held_user: BTreeSet<String>,
    pub witnesses: GateSnapshot,
    pub edit_observations: GateSnapshot,
}

#[derive(Default)]
pub(super) struct CapturePlan {
    pub witnesses: GateSnapshot,
    pub context: ProjectCapture,
    pub reviews: BTreeMap<Tuple, CaptureReview>,
    pub files: Vec<ProjectFileApproval>,
    pub skipped: Vec<ConfigPlanRefusal>,
    pub refusals: Vec<ConfigPlanRefusal>,
}

impl CapturePlan {
    pub fn refused_view(
        &self,
        incarnation: &str,
        revision: u64,
        generation: u64,
        read_set: subc_config::InputVector,
    ) -> Result<ConfigPlanView> {
        let mut changeset = subc_config::Changeset::new();
        changeset.apply_plan = serde_json::json!({"project_refusals": self.refusals});
        let mut view = ConfigPlanView::new(Plan::new(
            revision,
            generation,
            incarnation,
            read_set,
            changeset,
        ))
        .map_err(GateError::invalid)?;
        view.refusals.clone_from(&self.refusals);
        view.skipped.clone_from(&self.skipped);
        Ok(view)
    }
}

pub(super) fn retained_user_files(
    body: &StoreBody,
    approved: &GateSnapshot,
    declarations: &BTreeMap<String, subc_protocol::config::ConfigDeclareRequest>,
    held_user: &BTreeSet<String>,
) -> BTreeMap<PathBuf, CapturedFile> {
    let mut files = BTreeMap::new();
    for module in held_user {
        let Some(declaration) = declarations.get(module) else {
            continue;
        };
        let path = &declaration.user_file;
        let key = InputKey::Layer {
            module: module.clone(),
            path: path.clone(),
        };
        let state = approved
            .inputs
            .get(&key)
            .map_or(InputState::Absent, |input| input.state.clone());
        let bytes = approved_user(body, module).and_then(|view| view.layers.first()?.bytes.clone());
        files.insert(
            path.into(),
            CapturedFile::retained(path.into(), bytes, state),
        );
    }
    files
}

pub(super) fn approved_user<'a>(body: &'a StoreBody, module: &str) -> Option<&'a ConfigView> {
    body.generations
        .values()
        .rev()
        .find_map(|entry| entry.delivery.as_ref()?.modules.get(module))
}

pub(super) fn capture_projects(
    capturer: &super::capture::ConfigCapturer,
    body: &StoreBody,
    approved: &GateSnapshot,
    captured: &CapturedConfiguration,
    roots: &BTreeMap<String, ModuleRoots>,
    mut plan: CapturePlan,
) -> Result<(CapturePlan, GateSnapshot)> {
    let mut observed = captured.proposal_snapshot(approved);
    let previous = body
        .observed
        .as_ref()
        .map(GateSnapshot::from_store)
        .transpose()?
        .unwrap_or_else(|| approved.clone());
    observed.carry_project_inputs(&previous);
    for (module, roots) in roots {
        let Some(module_capture) = captured.modules.get(module) else {
            continue;
        };
        if roots.holds_user() {
            let path = &module_capture.file.path;
            let key = InputKey::Layer {
                module: module.clone(),
                path: path.to_string_lossy().into_owned(),
            };
            if let Ok((live, input)) =
                capturer.capture_module(module, &module_capture.declaration, CapturedFile::open)
            {
                if live.file.bytes != module_capture.file.bytes {
                    plan.skipped.push(held_notice(module, path));
                    observed.inputs.insert(key, input);
                } else if let Some(input) = approved.inputs.get(&key) {
                    observed.inputs.insert(key, input.clone());
                } else {
                    observed.inputs.remove(&key);
                }
            } else if let Some(input) = previous.inputs.get(&key) {
                observed.inputs.insert(key, input.clone());
                plan.skipped.push(held_notice(module, path));
            }
        }
        roots.observe(approved, &mut observed, &mut plan.skipped);
        if let Some(authority) = roots.review(
            body,
            approved,
            approved_user(body, module),
            module_capture,
            &mut plan.witnesses,
            &mut plan.reviews,
            &mut plan.files,
            &mut plan.refusals,
        )? {
            plan.context.authority.insert(module.clone(), authority);
        }
    }
    Ok((plan, observed))
}
