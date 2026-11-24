//! Plan repository edits by physical file, then review every module and project that reads each file.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use subc_config::{raw_digest, Digest};
use subc_control::config::{ConfigEdit, ConfigPlanRefusal};
use subc_protocol::config::{ConfigLayer, ConfigStatus};

use crate::config_store::{
    ExpectedProjectFile, ProjectFileApproval, ProjectFileLayer, ProjectFileTarget, StoreBody, Tuple,
};

use super::{edits, PreparedPlan, RuntimeOwner};
use crate::config_gate::{
    capture::{parse_layer, CONFIG_FILE_LIMIT},
    capture_plan::{self, CaptureAuthority, CapturePlan},
    program,
    project_layers::ProjectLayerSchema,
    project_registry::{layer_path, RegistryRevision, RegistryRoot, RegistryScope},
    reviewed_build, GateError, GateSnapshot, InputKey, Result,
};

#[derive(Clone, PartialEq, Eq)]
struct Selection {
    layer: ProjectFileLayer,
    reader: String,
    root: RegistryRoot,
}

fn refused(edit: &ConfigEdit, code: &str, detail: &str) -> ConfigPlanRefusal {
    let mut refusal = edits::refused(edit, code, detail);
    refusal.project_id.clone_from(&edit.project_id);
    refusal.workspace_id.clone_from(&edit.workspace_id);
    refusal
}

fn unknown_workspace() -> GateError {
    GateError::new(
        "unknown_workspace",
        "a registered workspace with a root and members is required",
    )
}

fn select(edit: &ConfigEdit, revision: &RegistryRevision) -> Result<Selection> {
    let project = edit
        .project_id
        .as_deref()
        .map(|id| revision.project(id))
        .transpose()?;
    let mismatch = || {
        GateError::new(
            "workspace_mismatch",
            "the project does not belong to the selected workspace",
        )
    };
    if edit.layer == "workspace" {
        let from_project = project
            .map(|p| p.workspace_id.as_ref().ok_or_else(unknown_workspace))
            .transpose()?;
        if let (Some(expected), Some(actual)) = (&edit.workspace_id, from_project) {
            if expected != actual {
                return Err(mismatch());
            }
        }
        let workspace =
            edit.workspace_id.as_ref().or(from_project).ok_or_else(|| {
                edits::invalid("a workspace edit must select a workspace or project")
            })?;
        let reader = project
            .or_else(|| {
                revision
                    .projects
                    .iter()
                    .find(|p| p.workspace_id.as_ref() == Some(workspace))
            })
            .ok_or_else(unknown_workspace)?;
        let root = reader
            .workspace_root
            .clone()
            .ok_or_else(unknown_workspace)?;
        return Ok(Selection {
            layer: ProjectFileLayer::Workspace {
                workspace_id: workspace.clone(),
            },
            reader: reader.project_id.clone(),
            root,
        });
    }
    let project = project.ok_or_else(|| edits::invalid("a project edit must select a project"))?;
    if edit.workspace_id.is_some() && edit.workspace_id != project.workspace_id {
        return Err(mismatch());
    }
    let roots = &project.project_roots;
    let no_root = || GateError::new("layer_root_invalid", "the project has no eligible root");
    let ambiguous = || {
        GateError::new(
            "layer_root_ambiguous",
            "the project layer cannot be assigned to a single root",
        )
    };
    if roots.is_empty() {
        return Err(no_root());
    }
    let mut present = None;
    for root in roots {
        if revision
            .layer_location(&project.project_id, &root.path)?
            .1
            .file
            .is_some()
            && present.replace(root).is_some()
        {
            return Err(ambiguous());
        }
    }
    let root = present
        .or_else(|| (roots.len() == 1).then(|| &roots[0]))
        .ok_or_else(ambiguous)?;
    Ok(Selection {
        layer: ProjectFileLayer::Project {
            project_id: project.project_id.clone(),
        },
        reader: project.project_id.clone(),
        root: root.clone(),
    })
}

fn add_scope(scope: &mut RegistryScope, edit: &ConfigEdit) {
    scope.projects.extend(edit.project_id.clone());
    if edit.layer == "workspace" {
        scope.workspaces.extend(edit.workspace_id.clone());
    }
}

impl RuntimeOwner {
    pub(crate) async fn prepare_control_edits(
        self: &Arc<Self>,
        base: BTreeMap<String, u64>,
        edits: Vec<ConfigEdit>,
    ) -> Result<PreparedPlan> {
        if !self.project_settings_enabled() || edits.is_empty() {
            return self
                .on_blocking(move |owner| owner.prepare_edits(base, edits))
                .await;
        }
        let (layers, user): (Vec<_>, Vec<_>) = edits
            .into_iter()
            .partition(|edit| matches!(edit.layer.as_str(), "workspace" | "project"));
        let touched = layers
            .iter()
            .chain(&user)
            .map(|edit| edit.module_id.clone())
            .collect();
        let mut prepared = self
            .on_blocking(move |owner| owner.prepare_edit_base(base, user, touched))
            .await?;
        let edit_plan = prepared.edits.as_mut().expect("prepared an edit plan");
        let mut scopes = BTreeMap::<String, RegistryScope>::new();
        if !edit_plan.unstamped {
            for write in &edit_plan.files {
                scopes.insert(write.module.clone(), RegistryScope::all());
            }
        }
        for edit in &layers {
            if !prepared.candidate.modules.contains_key(&edit.module_id) {
                edit_plan.refusals.push(refused(
                    edit,
                    "unknown_module",
                    "the module has no admitted settings schema",
                ));
            } else {
                add_scope(scopes.entry(edit.module_id.clone()).or_default(), edit);
            }
        }
        if edit_plan.unstamped && edit_plan.files.len() > 1 {
            return Err(GateError::new(
                "edit_spans_files",
                "an unstamped edit plan can touch only one file",
            ));
        }
        if scopes.is_empty() {
            return Ok(prepared);
        }
        let (mut enumeration, mut revisions) =
            self.capturer.projects.capture_batch(&scopes).await?;
        let mut selected = BTreeMap::<PathBuf, (Selection, Vec<ConfigEdit>)>::new();
        for edit in layers {
            let Some(revision) = revisions.get(&edit.module_id) else {
                continue;
            };
            match select(&edit, revision) {
                Ok(selection) => {
                    let path = layer_path(&selection.root.path, &edit.module_id);
                    let scope = scopes
                        .get_mut(&edit.module_id)
                        .expect("selected module was scoped");
                    scope.paths.insert(path.clone());
                    scope.projects.insert(selection.reader.clone());
                    if let ProjectFileLayer::Workspace { workspace_id } = &selection.layer {
                        scope.workspaces.insert(workspace_id.clone());
                    }
                    selected
                        .entry(path)
                        .or_insert_with(|| (selection, Vec::new()))
                        .1
                        .push(edit);
                }
                Err(error) if error.code.starts_with("layer_") => {
                    edit_plan
                        .refusals
                        .push(refused(&edit, &error.code, &error.detail))
                }
                Err(error) => return Err(error),
            }
        }
        if edit_plan.unstamped && edit_plan.files.len() + selected.len() > 1 {
            return Err(GateError::new(
                "edit_spans_files",
                "an unstamped edit plan can touch only one file",
            ));
        }
        // Selection can use an absent eligible root. Expand by physical path to
        // find all its readers, including projects outside the named workspace.
        if !selected.is_empty() {
            let (reply, expanded) = self.capturer.projects.capture_batch(&scopes).await?;
            for (selection, edits) in selected.values() {
                for edit in edits {
                    let before = select(edit, &revisions[&edit.module_id])?;
                    let after = select(edit, &expanded[&edit.module_id])?;
                    if before != after || before.root.path != selection.root.path {
                        return Err(capture_plan::changed());
                    }
                }
            }
            enumeration = reply;
            revisions = expanded;
        }
        let unstamped = edit_plan.unstamped;
        self.on_blocking(move |owner| {
            let mut state = owner.lock()?;
            Self::check_authority(&mut state, false)?;
            if state.store.status().base_revision != prepared.base_revision {
                return Err(capture_plan::changed());
            }
            if let Some(reply) = enumeration.as_ref().filter(|_| !unstamped) {
                state.store.replace_registry_membership(Ok(reply))?;
            }
            let body = state.store.body()?;
            let approved = GateSnapshot::from_store(&body.approved)?;
            let mut project_plan = CapturePlan::default();
            for (module, revision) in revisions {
                let scope = scopes.remove(&module).expect("revision was requested");
                let writes = selected
                    .iter()
                    .filter(|(_, (_, edits))| edits[0].module_id == module);
                plan_module(
                    body,
                    &approved,
                    &mut prepared,
                    &mut project_plan,
                    revision,
                    scope,
                    writes,
                )?;
            }
            prepared.project_capture = Some(project_plan);
            Ok(prepared)
        })
        .await
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_module<'a>(
    body: &StoreBody,
    approved: &GateSnapshot,
    prepared: &mut PreparedPlan,
    plan: &mut CapturePlan,
    revision: RegistryRevision,
    scope: RegistryScope,
    writes: impl Iterator<Item = (&'a PathBuf, &'a (Selection, Vec<ConfigEdit>))>,
) -> Result<()> {
    let module = &revision.module;
    let edit_plan = prepared
        .edits
        .as_ref()
        .expect("project edits need a user baseline");
    let unstamped = edit_plan.unstamped;
    let user_changed = edit_plan.files.iter().any(|write| &write.module == module);
    let captured = &prepared.candidate.modules[module];
    let approved_files = capture_plan::approved_files(body, approved, module);
    let mut proposed = BTreeMap::new();
    for (path, (selection, edits_for_file)) in writes {
        let staged = (|| {
            let (_, location) = revision.layer_location(&selection.reader, &selection.root.path)?;
            let baseline = if unstamped {
                location.file.as_ref().map(|file| file.bytes.as_slice())
            } else {
                approved_files
                    .get(path)
                    .and_then(|layer| layer.bytes.as_deref())
            };
            let retained_view = body.generations.values().rev().find_map(|entry| {
                entry
                    .delivery
                    .as_ref()?
                    .project_views
                    .get(module)?
                    .get(&selection.reader)
            });
            let effective = retained_view.map_or(&captured.view.effective, |view| &view.effective);
            for edit in edits_for_file {
                edits::schema_path(
                    captured.schema.normalized(),
                    Some(effective),
                    &edits::pointer(&edit.path)?,
                    false,
                )?;
            }
            let bytes = edits::edit_text(baseline, edits_for_file)?;
            if bytes.len() as u64 > CONFIG_FILE_LIMIT {
                return Err(GateError::new(
                    "layer_too_large",
                    "configuration document exceeds the 1 MiB JSONC limit",
                ));
            }
            parse_layer(&bytes)?;
            let on_disk = location.expected_file();
            let root_identity = selection
                .root
                .file_id
                .clone()
                .ok_or_else(capture_plan::changed)?;
            let prior = body
                .project_write_back
                .get(path)
                .filter(|old| !old.missing_approved && old.target.root_identity == root_identity);
            let expected = if prior.is_some_and(|old| old.expected == on_disk) {
                on_disk.clone()
            } else {
                match (baseline, &on_disk) {
                    (None, _) => ExpectedProjectFile::Absent,
                    (Some(bytes), ExpectedProjectFile::Present { identity, .. }) => {
                        ExpectedProjectFile::Present {
                            identity: identity.clone(),
                            digest: raw_digest(bytes),
                        }
                    }
                    // No current leaf can match the approved present file. Keep
                    // the expected state from a prior attempt when available.
                    (Some(_), ExpectedProjectFile::Absent) => prior
                        .map(|old| old.expected.clone())
                        .unwrap_or(ExpectedProjectFile::Absent),
                }
            };
            let target = ProjectFileTarget {
                module: module.clone(),
                layer: selection.layer.clone(),
                registry_revision: revision.digest_for_project(&selection.reader)?,
                root: selection.root.path.clone(),
                root_identity,
                relative_path: PathBuf::from(".cortexkit").join(format!("{module}.jsonc")),
            };
            Ok((
                target,
                expected,
                bytes,
                baseline.is_some()
                    && matches!(on_disk, ExpectedProjectFile::Absent)
                    && prior.is_none(),
            ))
        })();
        match staged {
            Ok((target, expected, bytes, missing_approved)) => {
                proposed.insert(path.clone(), bytes.clone());
                plan.witnesses.inputs.insert(
                    capture_plan::file_key(module, path),
                    capture_plan::file_input(Some(raw_digest(&bytes))),
                );
                plan.files.push(ProjectFileApproval::Write {
                    target,
                    expected,
                    bytes,
                    missing_approved,
                });
            }
            Err(error) if error.code == "config_unavailable" => return Err(error),
            Err(error) => {
                for edit in edits_for_file {
                    plan.refusals
                        .push(refused(edit, &error.code, &error.detail));
                }
            }
        }
    }
    if proposed.is_empty() && !user_changed {
        return Ok(());
    }
    let schema = ProjectLayerSchema::new(captured.schema.clone());
    let build = reviewed_build::active(body, module)?
        .map(Digest::try_from)
        .transpose()
        .map_err(GateError::invalid)?
        .unwrap_or_else(|| program::approved_base(approved, module));
    for project in &revision.projects {
        let id = &project.project_id;
        if !user_changed
            && !project
                .roots()
                .any(|root| proposed.contains_key(&layer_path(&root.path, module)))
        {
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
        let delivery = previous.and_then(|entry| entry.delivery.as_ref());
        let mut base = delivery
            .and_then(|delivery| delivery.project_views.get(module)?.get(id))
            .cloned();
        if let (Some(base), Some(previous)) = (&mut base, previous) {
            base.generation = previous.generation.id;
            base.status =
                if body.versions.iter().any(|(key, versions)| {
                    *key == tuple && versions.active == Some(base.generation)
                }) {
                    ConfigStatus::Active
                } else {
                    ConfigStatus::Pending
                };
        }
        let layers = capture_plan::tuple_layers(module, project, |root, path, _layer| {
            let (_, location) = revision.layer_location(id, root)?;
            plan.context.edit_observations.inputs.insert(
                capture_plan::file_key(module, path),
                capture_plan::file_input(location.file.as_ref().map(|file| file.digest.clone())),
            );
            if let Some(bytes) = proposed.get(path) {
                return Ok(Some(bytes.clone()));
            }
            if unstamped {
                return Ok(location.file.as_ref().map(|file| file.bytes.clone()));
            }
            if let Some(base) = &base {
                return Ok(base
                    .layers
                    .iter()
                    .find(|layer| {
                        layer.layer != ConfigLayer::User && layer.path.as_deref() == path.to_str()
                    })
                    .and_then(|layer| layer.bytes.clone()));
            }
            Ok(approved_files
                .get(path)
                .and_then(|layer| layer.bytes.clone()))
        });
        let review = layers.and_then(|layers| {
            if layers.is_empty() && base.is_none() {
                return Ok(None);
            }
            let mut user = captured.view.layers[0].clone();
            user.project_id = Some(id.clone());
            capture_plan::build_review(
                module,
                id,
                captured,
                &schema,
                build.clone(),
                base,
                delivery.and_then(|delivery| delivery.declarations.get(module)),
                std::iter::once(user).chain(layers).collect(),
            )
            .map(Some)
        });
        match review {
            Ok(Some(mut review)) if !unstamped => {
                review.approval.registry_identity = Some(revision.digest_for_project(id)?);
                plan.witnesses.inputs.insert(
                    InputKey::ProjectTuple {
                        module: module.clone(),
                        project: id.clone(),
                    },
                    capture_plan::file_input(Some(review.approval.approval_digest()?)),
                );
                plan.reviews.insert(tuple, review);
            }
            Ok(Some(review)) => {
                // An unstamped plan writes without a module review, so the
                // daemon lists the tuple's changed fields itself, comparing
                // the view the files give now with the view after the edit.
                let before = capture_plan::tuple_layers(module, project, |root, _, _| {
                    let (_, location) = revision.layer_location(id, root)?;
                    Ok(location.file.as_ref().map(|file| file.bytes.clone()))
                })
                .and_then(|layers| {
                    let mut user = captured.view.layers[0].clone();
                    user.project_id = Some(id.clone());
                    capture_plan::build_review(
                        module,
                        id,
                        captured,
                        &schema,
                        build.clone(),
                        None,
                        None,
                        std::iter::once(user).chain(layers).collect(),
                    )
                });
                let before = match before {
                    Ok(before) => Some(before.approval.view),
                    Err(error) if error.code == "config_unavailable" => return Err(error),
                    // The current files are not a valid view, so there is no
                    // old value to show: every field is listed as new.
                    Err(_) => None,
                };
                plan.unreviewed_fields.extend(
                    crate::config_gate::review::expected_fields(
                        module,
                        &review.schema,
                        before.as_ref(),
                        before.as_ref().map(|_| &captured.schema),
                        &review.approval.view,
                    )?
                    .into_values(),
                );
            }
            Ok(_) => {}
            Err(error) if error.code == "config_unavailable" => return Err(error),
            Err(error) => plan.refusals.push(capture_plan::notice(
                module,
                Some(project),
                String::new(),
                error,
            )),
        }
    }
    plan.context
        .authority
        .insert(module.clone(), CaptureAuthority::for_edit(scope, revision));
    Ok(())
}

impl RuntimeOwner {
    pub(crate) async fn apply_control_edits(
        self: &Arc<Self>,
        digest: Digest,
    ) -> Result<Option<u64>> {
        let witness = self
            .on_blocking({
                let digest = digest.clone();
                move |owner| {
                    let state = owner.lock()?;
                    let Some(plan) = state.gate.plans.get(&digest) else {
                        return Ok(None);
                    };
                    if let Some(context) = &plan.project_capture {
                        if let Some(pool) = owner.capturer.projects.root_io() {
                            for authority in context.authority.values() {
                                authority.check_busy(pool)?;
                            }
                        }
                    }
                    let Some(edit) = plan
                        .edits
                        .as_ref()
                        .filter(|edit| edit.unstamped && !plan.project_files.is_empty())
                    else {
                        return Ok(None);
                    };
                    debug_assert!(edit.files.is_empty());
                    if !plan.view.refusals.is_empty() {
                        return Err(GateError::new(
                            "plan_refused",
                            "the plan contains refused edits",
                        ));
                    }
                    let context = plan
                        .project_capture
                        .clone()
                        .ok_or_else(capture_plan::changed)?;
                    Ok(Some(context.authority))
                }
            })
            .await?;
        let Some(witness) = witness else {
            return self
                .on_blocking(move |owner| owner.apply_unstamped_edits(&digest))
                .await;
        };
        let scopes = witness
            .iter()
            .map(|(module, authority)| (module.clone(), authority.scope.clone()))
            .collect();
        let (_, revisions) = self.capturer.projects.capture_batch(&scopes).await?;
        for (module, authority) in &witness {
            authority.recheck(revisions.get(module).ok_or_else(capture_plan::changed)?)?;
        }
        let job = self
            .on_blocking(move |owner| {
                let mut state = owner.lock()?;
                Self::check_authority(&mut state, false)?;
                let status = state.store.status();
                let plan = state
                    .gate
                    .plans
                    .get(&digest)
                    .ok_or_else(capture_plan::changed)?;
                if status.state != crate::config_store::StoreState::Unstamped
                    || status.base_revision != plan.view.base_revision
                {
                    return Err(capture_plan::changed());
                }
                let intent = plan.project_files[0]
                    .clone()
                    .into_intent(0)
                    .ok_or_else(capture_plan::changed)?;
                let job = owner.submit_project_write(&revisions, intent)?;
                state.gate.expire_plan(&digest);
                state.plan_deadlines.remove(&digest);
                state.retained_plan_files.remove(&digest);
                Ok(job)
            })
            .await?;
        let result = job
            .wait()
            .await
            .map_err(GateError::from)
            .and_then(|result| result);
        self.notify_delivery_changed();
        result.map(|_| Some(0))
    }

    fn submit_project_write(
        &self,
        revisions: &BTreeMap<String, RegistryRevision>,
        intent: crate::config_store::ProjectWriteBackIntent,
    ) -> Result<crate::config_gate::root_io::RootIoJob<Result<()>>> {
        let pool = self.capturer.projects.root_io().ok_or_else(|| {
            GateError::new("config_unavailable", "registry root I/O is unavailable")
        })?;
        let revision = revisions
            .get(&intent.target.module)
            .ok_or_else(capture_plan::changed)?;
        revision.submit_write(
            pool,
            intent,
            #[cfg(test)]
            self.project_write_hook.lock().unwrap().clone(),
        )
    }

    pub(crate) async fn write_approved_project_files(
        self: &Arc<Self>,
        generation: u64,
        authority: super::CaptureApply,
    ) {
        let Ok(paths) = self
            .on_blocking(move |owner| {
                let mut state = owner.lock()?;
                Ok(state
                    .store
                    .body()?
                    .project_write_back
                    .iter()
                    .filter(|(_, intent)| intent.generation == generation)
                    .map(|(path, _)| path.clone())
                    .collect::<Vec<_>>())
            })
            .await
        else {
            return;
        };
        let revisions = Arc::new(authority.revisions);
        for path in paths {
            let revisions = revisions.clone();
            let job = self
                .on_blocking({
                    let path = path.clone();
                    move |owner| {
                        let mut state = owner.lock()?;
                        let intent = state
                            .store
                            .body()?
                            .project_write_back
                            .get(&path)
                            .filter(|intent| intent.generation == generation);
                        intent
                            .cloned()
                            .map(|intent| owner.submit_project_write(&revisions, intent))
                            .transpose()
                    }
                })
                .await;
            if let Ok(Some(job)) = job {
                if matches!(job.wait().await, Ok(Ok(()))) {
                    let _ = self
                        .record_project_write_adoptions(BTreeMap::from([(path, generation)]))
                        .await;
                }
            }
        }
    }

    pub(in crate::config_gate) async fn record_project_write_adoptions(
        self: &Arc<Self>,
        matches: BTreeMap<PathBuf, u64>,
    ) -> Result<()> {
        self.on_blocking(move |owner| {
            if owner.lock()?.store.adopt_project_write_backs(&matches)? {
                owner.notify_delivery_changed();
            }
            Ok(())
        })
        .await
    }
}

pub(in crate::config_gate) fn reconciliation_scopes(
    intents: &BTreeMap<PathBuf, crate::config_store::ProjectWriteBackIntent>,
) -> BTreeMap<String, RegistryScope> {
    let mut scopes = BTreeMap::<String, RegistryScope>::new();
    for (path, intent) in intents {
        let scope = scopes.entry(intent.target.module.clone()).or_default();
        scope.paths.insert(path.clone());
        match &intent.target.layer {
            ProjectFileLayer::Workspace { workspace_id } => {
                scope.workspaces.insert(workspace_id.clone());
            }
            ProjectFileLayer::Project { project_id } => {
                scope.projects.insert(project_id.clone());
            }
        }
    }
    scopes
        .into_iter()
        .map(|(module, scope)| (module, scope.targets_only()))
        .collect()
}

pub(in crate::config_gate) fn matching_project_writes(
    intents: &BTreeMap<PathBuf, crate::config_store::ProjectWriteBackIntent>,
    revisions: &BTreeMap<String, RegistryRevision>,
) -> BTreeMap<PathBuf, u64> {
    intents
        .iter()
        .filter_map(|(path, intent)| {
            let revision = revisions.get(&intent.target.module)?;
            let reader = revision.write_authority(&intent.target).ok()?;
            let (_, location) = revision.layer_location(reader, &intent.target.root).ok()?;
            (location.file.as_ref()?.bytes == intent.bytes)
                .then(|| (path.clone(), intent.generation))
        })
        .collect()
}
