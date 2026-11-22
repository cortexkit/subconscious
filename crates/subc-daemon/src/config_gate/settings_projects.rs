//! Owner settings projection for project tuples. Repository reads run on the
//! dedicated pool for project-directory I/O, outside the configuration owner lock.

use super::*;
use crate::config_gate::{
    capture::{parse_layer, CapturedModule, SettingsCapture},
    capture_plan::tuple_layers,
    project_layers::ProjectLayerSchema,
    project_registry::{layer_path, live::LiveProjectCapture, RegistryRevision, RegistryScope},
};
use crate::config_store::{ProjectDeliveryState, Tuple};
use std::{path::PathBuf, sync::Arc};
use subc_control::config::{
    ConfigFileError, ConfigFileErrorsIncomplete, ConfigStatus, ConfigStatusFileError,
};
use subc_protocol::config::ConfigDeclarationPending;

// Leave room within ck's 10-second wait to report a stalled project folder.
const STATUS_PROJECT_SCAN_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

fn key(module: &str, project: &str) -> Tuple {
    Tuple {
        module: module.into(),
        project: Some(project.into()),
    }
}

fn approved_project<'a>(body: &'a StoreBody, tuple: &Tuple) -> Option<&'a HistoryEntry> {
    body.generations
        .values()
        .rev()
        .find(|entry| entry.tuples.contains(tuple))
}

fn project_view<'a>(entry: &'a HistoryEntry, tuple: &Tuple) -> Option<&'a ConfigView> {
    entry
        .delivery
        .as_ref()?
        .project_views
        .get(&tuple.module)?
        .get(tuple.project.as_deref()?)
}

fn pending_plan(state: &OwnerState, tuple: &Tuple) -> Option<Digest> {
    state
        .gate
        .plans
        .values()
        .filter(|plan| {
            plan.edits.is_none() && plan.targets().is_ok_and(|targets| targets.contains(tuple))
        })
        .max_by_key(|plan| plan.creation_sequence)
        .map(|plan| plan.view.digest.clone())
}

fn declaration_pending(
    state: &OwnerState,
    tuple: &Tuple,
    approved: Option<&ConfigDeclareRequest>,
    current: Option<&ConfigDeclareRequest>,
) -> Option<ConfigDeclarationPending> {
    let (approved, current) = (approved?, current?);
    if approved == current {
        return None;
    }
    let mut marker = ConfigDeclarationPending::default();
    marker.plan_digest = state
        .gate
        .plans
        .values()
        .filter(|plan| {
            plan.delivery
                .as_ref()
                .and_then(|delivery| delivery.declarations.get(&tuple.module))
                == Some(current)
                && plan.targets().is_ok_and(|targets| targets.contains(tuple))
        })
        .max_by_key(|plan| plan.creation_sequence)
        .map(|plan| plan.view.digest.clone());
    Some(marker)
}

enum ShowBasis {
    Approved(Box<ShowView>),
    StampedFallback {
        pending_plan: Option<Digest>,
        file_error: Option<ConfigFileError>,
    },
    Unstamped(ConfigDeclareRequest),
}

impl RuntimeOwner {
    pub(crate) async fn settings_show_project(
        self: &Arc<Self>,
        known: BTreeSet<String>,
        module: String,
        project: String,
        through: Option<ConfigThrough>,
    ) -> Result<ConfigControlResponse> {
        if !self.project_settings_enabled() {
            return Err(GateError::new(
                "invalid_params",
                "project settings are not served",
            ));
        }
        let basis = self
            .on_blocking({
                let (known, module, project) = (known.clone(), module.clone(), project.clone());
                move |owner| owner.project_show_basis(known, &module, &project, through)
            })
            .await?;
        let show = match basis {
            ShowBasis::Approved(show) => *show,
            ShowBasis::StampedFallback { .. } => {
                let reply = self.capturer.projects.enumerate_live().await?;
                if !reply
                    .projects
                    .iter()
                    .any(|entry| entry.project_id == project)
                {
                    return Err(GateError::new(
                        "unknown_project",
                        "project is not registered",
                    ));
                }
                self.on_blocking({
                    let (module, project) = (module.clone(), project.clone());
                    move |owner| {
                        // Approval can race the enumeration. Prefer it to the fallback.
                        match owner.project_show_basis(known.clone(), &module, &project, through)? {
                            ShowBasis::Approved(show) => Ok(*show),
                            ShowBasis::StampedFallback {
                                pending_plan,
                                file_error,
                            } => {
                                let mut show = owner.user_show_view(known, &module)?;
                                show.pending_plan = pending_plan;
                                show.file_error = file_error.or(show.file_error);
                                Ok(show)
                            }
                            ShowBasis::Unstamped(_) => Err(GateError::capture_race()),
                        }
                    }
                })
                .await?
            }
            ShowBasis::Unstamped(declaration) => {
                let scope = RegistryScope::new(
                    BTreeSet::from([project.clone()]),
                    BTreeSet::new(),
                    BTreeSet::new(),
                );
                let revision = self
                    .capturer
                    .projects
                    .snapshot_scoped(&module, &scope)
                    .await?;
                self.on_blocking({
                    let (module, project) = (module.clone(), project.clone());
                    move |owner| {
                        let layers = read_layers(&module, &project, &revision)?;
                        let captured = owner.capturer.capture_modules(
                            &BTreeMap::from([(module.clone(), declaration.clone())]),
                            &BTreeSet::from([module.clone()]),
                        )?;
                        let user = captured.modules.get(&module).ok_or_else(|| {
                            GateError::new("config_gated", "module view is unavailable")
                        })?;
                        let schema = ProjectLayerSchema::new(user.schema.clone());
                        let view = unstamped_projects::live_view(
                            &module, &project, &schema, user, layers, through,
                        )?;
                        Ok(ShowView {
                            declaration,
                            view,
                            stamped: false,
                            base_generation: 0,
                            file_error: None,
                            pending_plan: None,
                            declaration_pending: None,
                        })
                    }
                })
                .await?
            }
        };
        show.response(module, Some(project), through)
    }

    fn project_show_basis(
        &self,
        known: BTreeSet<String>,
        module: &str,
        project: &str,
        through: Option<ConfigThrough>,
    ) -> Result<ShowBasis> {
        let mut state = self.lock()?;
        state.check_authority_for_read()?;
        let (_, body) = state.store.loaded_admission_state();
        let tuple = key(module, project);
        if body.stamped {
            let file_error = self.project_settings_file_error(&state, &tuple)?;
            let pending_plan = pending_plan(&state, &tuple);
            if let Some(entry) = approved_project(body, &tuple) {
                let unavailable =
                    || GateError::new("config_gated", "approved project settings are unavailable");
                let delivery = entry.delivery.as_ref().ok_or_else(unavailable)?;
                let view = match through {
                    Some(ConfigThrough::User | ConfigThrough::Workspace) => {
                        let cutoffs = delivery
                            .project_cutoffs
                            .get(module)
                            .and_then(|cutoffs| cutoffs.get(project))
                            .ok_or_else(unavailable)?;
                        if through == Some(ConfigThrough::User) {
                            &cutoffs.user
                        } else {
                            &cutoffs.workspace
                        }
                    }
                    _ => project_view(entry, &tuple).ok_or_else(unavailable)?,
                };
                let declaration = delivery.declarations.get(module).ok_or_else(unavailable)?;
                let current = Self::loaded_declarations(&state);
                let mut show = ShowView {
                    declaration: declaration.clone(),
                    view: view.clone(),
                    stamped: true,
                    base_generation: crate::config_gate::module_base_generation(body, module),
                    file_error,
                    pending_plan,
                    declaration_pending: declaration_pending(
                        &state,
                        &tuple,
                        Some(declaration),
                        current.get(module),
                    ),
                };
                let current = current.get(module).unwrap_or(declaration).clone();
                drop(state);
                if show.file_error.is_none() {
                    show.file_error = self.capturer.settings_file_error(module, &current)?;
                }
                return Ok(ShowBasis::Approved(Box::new(show)));
            }
            return Ok(ShowBasis::StampedFallback {
                pending_plan,
                file_error,
            });
        }
        let approved = GateSnapshot::from_store(&body.approved)?;
        let snapshot = self.settings_snapshot(&state, &approved, known)?;
        Ok(ShowBasis::Unstamped(snapshot.declaration(module)?.clone()))
    }
}

type Layers = Vec<ConfigLayerSnapshot>;

fn read_layers(module: &str, id: &str, revision: &RegistryRevision) -> Result<Layers> {
    let project = revision.project(id)?;
    tuple_layers(module, project, |root, _| {
        let (_, location) = revision.layer_location(id, root)?;
        Ok(location.file.as_ref().map(|file| file.bytes.clone()))
    })
}

#[derive(Default)]
pub(in crate::config_gate::runtime) struct ProjectObservations {
    refresh: tokio::sync::Mutex<()>,
    cached: Mutex<Option<ProjectObservationsSnapshot>>,
}

#[cfg(test)]
impl ProjectObservations {
    pub(in crate::config_gate::runtime) async fn pause_refresh(
        &self,
    ) -> tokio::sync::MutexGuard<'_, ()> {
        self.refresh.lock().await
    }
}

struct ProjectObservationsSnapshot {
    incarnation: Arc<()>,
    rows: Arc<BTreeMap<Tuple, ProjectFiles>>,
}

#[derive(Default)]
struct ProjectFileScan {
    registered: BTreeSet<String>,
    files: BTreeMap<Tuple, ProjectFiles>,
    incomplete: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ProjectFiles {
    workspace: Option<String>,
    paths: BTreeSet<PathBuf>,
    present: bool,
    read_failed: bool,
    // An invalid read is not evidence of a deletion. Keep the last complete
    // set of file digests until the paths can be read again.
    layers: Option<BTreeMap<String, LayerDigest>>,
    error: Option<ConfigFileError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LayerDigest {
    raw: Digest,
    parsed: Option<Digest>,
}

impl LayerDigest {
    fn new(bytes: &[u8]) -> Self {
        Self {
            raw: subc_config::raw_digest(bytes),
            parsed: parse_layer(bytes)
                .ok()
                .and_then(|value| subc_config::digest(subc_config::PURPOSE_SNAPSHOT, &value).ok()),
        }
    }
}

impl RuntimeOwner {
    pub(crate) async fn refresh_settings_projects(self: &Arc<Self>, all: bool) -> Result<()> {
        let Ok(_refresh) = self.settings_projects.refresh.try_lock() else {
            return Ok(());
        };
        let (incarnation, declarations, own) = self
            .on_blocking(move |owner| {
                let state = owner.lock()?;
                let (_, body) = state.store.loaded_admission_state();
                let own: BTreeSet<_> = body
                    .versions
                    .iter()
                    .filter_map(|(tuple, _)| tuple.project.is_some().then_some(tuple.clone()))
                    .collect();
                let declarations = if all || (body.stamped && !own.is_empty()) {
                    Self::loaded_declarations(&state)
                } else {
                    BTreeMap::new()
                };
                Ok((state.authority_incarnation.clone(), declarations, own))
            })
            .await?;
        if declarations.is_empty() && !all {
            return Ok(());
        }
        let declarations = Arc::new(declarations);
        let settings = self
            .on_blocking({
                let declarations = Arc::clone(&declarations);
                move |owner| owner.capturer.capture_settings(&declarations)
            })
            .await?;
        let mut scan = ProjectFileScan::default();
        self.scan_project_settings(&declarations, settings, (!all).then_some(&own), &mut scan)
            .await?;
        self.publish_project_settings(incarnation, declarations, scan.registered, scan.files)
            .await
    }

    async fn scan_project_settings(
        self: &Arc<Self>,
        declarations: &BTreeMap<String, ConfigDeclareRequest>,
        settings: SettingsCapture,
        only: Option<&BTreeSet<Tuple>>,
        scan: &mut ProjectFileScan,
    ) -> Result<()> {
        let registry = &self.capturer.projects;
        let reply = registry.enumerate_live().await?;
        let pool = registry.root_io().ok_or_else(|| {
            GateError::new("config_unavailable", "registry capture worker stopped")
        })?;
        scan.registered = reply
            .projects
            .iter()
            .map(|project| project.project_id.clone())
            .collect();
        let requests = declarations
            .keys()
            .flat_map(|module| {
                scan.registered
                    .iter()
                    .map(move |project| (module.clone(), project.clone()))
            })
            .filter(|(module, project)| {
                only.is_none_or(|tuples| tuples.contains(&key(module, project)))
            })
            .collect::<Vec<_>>();
        let expected = requests.len();
        let mut capture = LiveProjectCapture::new(pool, &reply, requests);
        let users = Arc::new((
            settings
                .configuration
                .modules
                .into_iter()
                .map(|(module, user)| {
                    let schema = ProjectLayerSchema::new(user.schema.clone());
                    (module, (user, schema))
                })
                .collect::<BTreeMap<_, _>>(),
            settings.file_errors,
        ));
        while let Some(((module, project), revision)) = capture.next().await {
            let users = Arc::clone(&users);
            let tuple = key(&module, &project);
            let observed = self
                .on_blocking(move |_| {
                    Ok(observe_project_files(
                        &module,
                        &project,
                        revision,
                        users.0.get(&module),
                        users.1.get(&module),
                    ))
                })
                .await?;
            scan.incomplete |= observed.read_unavailable();
            scan.files.insert(tuple, observed);
        }
        // A busy root may be skipped rather than replacing a watch's last row.
        scan.incomplete |= scan.files.len() != expected;
        Ok(())
    }

    async fn publish_project_settings(
        self: &Arc<Self>,
        incarnation: Arc<()>,
        declarations: Arc<BTreeMap<String, ConfigDeclareRequest>>,
        registered: BTreeSet<String>,
        files: BTreeMap<Tuple, ProjectFiles>,
    ) -> Result<()> {
        self.on_blocking(move |owner| {
            let state = owner.lock()?;
            if !Arc::ptr_eq(&state.authority_incarnation, &incarnation) {
                return Ok(());
            }
            // If a declaration changed during the capture, retry with its new
            // schema on the next tick rather than publishing obsolete errors.
            let current = Self::loaded_declarations(&state);
            let mut cached = owner.settings_projects.cached.lock().map_err(|_| {
                GateError::new(
                    "config_unavailable",
                    "project settings observations are unavailable",
                )
            })?;
            let mut changed = cached
                .as_ref()
                .is_some_and(|snapshot| !Arc::ptr_eq(&snapshot.incarnation, &incarnation));
            if changed {
                *cached = None;
            }
            let snapshot = cached.get_or_insert_with(|| ProjectObservationsSnapshot {
                incarnation,
                rows: Arc::default(),
            });
            let rows = Arc::make_mut(&mut snapshot.rows);
            rows.retain(|tuple, _| {
                let keep = tuple
                    .project
                    .as_ref()
                    .is_some_and(|project| registered.contains(project))
                    && current.contains_key(&tuple.module);
                changed |= !keep;
                keep
            });
            for (tuple, mut files) in files {
                if current.get(&tuple.module) != declarations.get(&tuple.module) {
                    continue;
                }
                let old = rows.entry(tuple).or_default();
                if files.layers.is_none() {
                    files.present |= old.present;
                }
                if files.error.is_some() && old.layers.is_some() {
                    files.layers.clone_from(&old.layers);
                }
                if *old != files {
                    *old = files;
                    changed = true;
                }
            }
            if changed {
                owner
                    .settings_changed
                    .send_modify(|version| *version = version.wrapping_add(1));
            }
            Ok(())
        })
        .await
    }

    pub(in crate::config_gate) async fn report_project_settings_file_errors(
        self: &Arc<Self>,
        declarations: &BTreeMap<String, ConfigDeclareRequest>,
        captured: SettingsCapture,
        status: &mut ConfigStatus,
    ) {
        if !self.project_settings_enabled() || declarations.is_empty() {
            return;
        }
        let mut scan = ProjectFileScan::default();
        let result = tokio::time::timeout(
            STATUS_PROJECT_SCAN_BUDGET,
            self.scan_project_settings(declarations, captured, None, &mut scan),
        )
        .await;
        status
            .file_errors
            .extend(scan.files.into_iter().filter_map(|(tuple, files)| {
                if !(files.present || files.read_failed) || files.read_unavailable() {
                    return None;
                }
                let mut row = ConfigStatusFileError::new(tuple.module, files.error?);
                row.project_id = tuple.project;
                row.workspace_id = files.workspace;
                Some(row)
            }));
        let reason = match result {
            Ok(Ok(())) if !scan.incomplete => return,
            Ok(Err(error)) => error.code,
            _ => "config_unavailable".into(),
        };
        status.file_errors_incomplete = Some(ConfigFileErrorsIncomplete::new(reason));
    }

    fn project_settings_observations(
        &self,
        state: &OwnerState,
    ) -> Result<Arc<BTreeMap<Tuple, ProjectFiles>>> {
        let cached = self.settings_projects.cached.lock().map_err(|_| {
            GateError::new(
                "config_unavailable",
                "project settings observations are unavailable",
            )
        })?;
        Ok(cached
            .as_ref()
            .filter(|snapshot| Arc::ptr_eq(&snapshot.incarnation, &state.authority_incarnation))
            .map(|snapshot| snapshot.rows.clone())
            .unwrap_or_default())
    }

    fn project_settings_file_error(
        &self,
        state: &OwnerState,
        tuple: &Tuple,
    ) -> Result<Option<ConfigFileError>> {
        Ok(self
            .project_settings_observations(state)?
            .get(tuple)
            .and_then(|files| files.error.clone()))
    }
}

impl ProjectFiles {
    fn read_unavailable(&self) -> bool {
        self.read_failed
            && self
                .error
                .as_ref()
                .is_some_and(|error| error.code == "config_unavailable")
    }
}

fn observe_project_files(
    module: &str,
    project: &str,
    revision: Result<RegistryRevision>,
    user: Option<&(CapturedModule, ProjectLayerSchema)>,
    user_error: Option<&ConfigFileError>,
) -> ProjectFiles {
    let mut files = ProjectFiles::default();
    let result: Result<()> = (|| {
        let revision = revision?;
        let entry = revision.project(project)?;
        files.workspace.clone_from(&entry.workspace_id);
        files.paths = entry
            .roots()
            .map(|root| layer_path(&root.path, module))
            .collect();
        let layers = read_layers(module, project, &revision)?;
        files.present = !layers.is_empty();
        let user_layer = user.and_then(|(user, _)| user.view.layers.first());
        files.layers = Some(
            user_layer
                .into_iter()
                .chain(&layers)
                .filter_map(|layer| {
                    Some((
                        layer.path.clone()?,
                        LayerDigest::new(layer.bytes.as_deref()?),
                    ))
                })
                .collect(),
        );
        if let Some((user, schema)) = user {
            unstamped_projects::live_view(module, project, schema, user, layers, None)?;
        }
        Ok(())
    })();
    files.error = match result {
        Err(error) => Some(ConfigFileError::new(error.code, error.detail)),
        Ok(()) => user_error.cloned(),
    };
    files.read_failed = files.layers.is_none();
    files
}

impl RuntimeOwner {
    pub(super) fn append_project_settings_rows(
        &self,
        state: &OwnerState,
        snapshot: &SettingsSnapshot,
        rereview: &BTreeMap<String, reviewed_build::Rereview>,
        user_write_back: &BTreeMap<String, String>,
        rows: &mut Vec<ConfigModule>,
    ) -> Result<()> {
        let (_, body) = state.store.loaded_admission_state();
        let observations = self.project_settings_observations(state)?;
        let mut tuples: BTreeSet<_> = observations.keys().cloned().collect();
        if snapshot.stamped {
            tuples.extend(
                body.versions
                    .iter()
                    .filter_map(|(tuple, _)| tuple.project.is_some().then_some(tuple.clone())),
            );
            for plan in state
                .gate
                .plans
                .values()
                .filter(|plan| plan.edits.is_none())
            {
                tuples.extend(
                    plan.targets()?
                        .into_iter()
                        .filter(|tuple| tuple.project.is_some()),
                );
            }
        }
        let users: BTreeMap<_, _> = rows
            .iter()
            .map(|row| (row.module_id.as_str(), row))
            .collect();
        let mut projects = Vec::new();
        for tuple in tuples {
            let Some(user) = users.get(tuple.module.as_str()) else {
                continue;
            };
            let Some(project) = tuple.project.as_ref() else {
                continue;
            };
            let files = observations.get(&tuple);
            let workspace_id = if snapshot.stamped {
                let Some(workspace) = body
                    .registry_membership
                    .as_ref()
                    .and_then(|list| list.projects.get(project))
                else {
                    continue;
                };
                workspace.clone()
            } else {
                files.and_then(|files| files.workspace.clone())
            };
            let entry = snapshot
                .stamped
                .then(|| approved_project(body, &tuple))
                .flatten();
            let plan = pending_plan(state, &tuple);
            if entry.is_none()
                && plan.is_none()
                && !files.is_some_and(|files| files.present || files.read_failed)
            {
                continue;
            }
            let versions = body.project_versions(&tuple);
            let failed = entry.is_some_and(|entry| {
                entry
                    .project_outcomes
                    .get(&tuple.module)
                    .and_then(|outcomes| outcomes.get(project))
                    .is_some_and(|outcome| outcome.state == ProjectDeliveryState::Failed)
            });
            let user_behind = entry.and_then(|entry| entry.project_basis(&tuple))
                == user.approved_generation
                && user_write_back.contains_key(&tuple.module);
            let behind = snapshot.stamped
                && (user_behind
                    || body.project_write_back.iter().any(|(path, intent)| {
                        intent.target.module == tuple.module
                            && (files.is_some_and(|files| files.paths.contains(path))
                                || entry
                                    .and_then(|entry| project_view(entry, &tuple))
                                    .is_some_and(|view| {
                                        view.layers.iter().any(|layer| {
                                            layer.path.as_deref().is_some_and(|name| {
                                                std::path::Path::new(name) == path
                                            })
                                        })
                                    }))
                    }));
            let (proposal, formatting) = if snapshot.stamped {
                file_drift(files, entry.and_then(|entry| project_view(entry, &tuple)))
            } else {
                (false, false)
            };
            let declaration_pending = declaration_pending(
                state,
                &tuple,
                entry.and_then(|entry| entry.delivery.as_ref()?.declarations.get(&tuple.module)),
                snapshot.declarations.get(&tuple.module),
            );
            let mut row = (*user).clone();
            row.project_id = tuple.project.clone();
            row.workspace_id = workspace_id;
            row.approved_generation = if snapshot.stamped {
                entry.map(|entry| entry.generation.id)
            } else {
                Some(0)
            };
            row.active_generation = if snapshot.stamped {
                versions.and_then(|versions| versions.active)
            } else {
                Some(0)
            };
            row.file_error = files
                .and_then(|files| files.error.clone())
                .or_else(|| user.file_error.clone());
            row.pending_plan = plan;
            row.declaration_pending = declaration_pending;
            row.status = settings_status(
                row.gated,
                failed,
                rereview.contains_key(&tuple.module),
                versions.is_some_and(|versions| versions.pending.is_some()),
                behind,
                proposal || row.pending_plan.is_some(),
                formatting,
            )
            .into();
            row.status_reason = match row.status.as_str() {
                "failed_delivery" => entry
                    .map(|entry| format!("Generation {} failed delivery.", entry.generation.id)),
                "needs_rereview" => {
                    Some("The admitted program awaits configuration review.".into())
                }
                "write_back_behind" => {
                    Some("At least one settings file has not been written back.".into())
                }
                _ => None,
            };
            projects.push(row);
        }
        rows.extend(projects);
        rows.sort_by(|a, b| (&a.module_id, &a.project_id).cmp(&(&b.module_id, &b.project_id)));
        Ok(())
    }
}

fn file_drift(files: Option<&ProjectFiles>, approved: Option<&ConfigView>) -> (bool, bool) {
    let Some(observed) = files.and_then(|files| files.layers.as_ref()) else {
        return (false, false);
    };
    let approved: BTreeMap<_, _> = approved
        .into_iter()
        .flat_map(|view| &view.layers)
        .filter_map(|layer| {
            Some((
                layer.path.as_ref()?,
                (layer.digest.as_ref()?, layer.bytes.as_deref()?),
            ))
        })
        .collect();
    let mut formatting = false;
    if approved.len() != observed.len() {
        return (true, false);
    }
    for (path, digest) in observed {
        let Some((raw, bytes)) = approved.get(path) else {
            return (true, false);
        };
        if *raw != &digest.raw {
            if digest.parsed.is_none() || digest.parsed != LayerDigest::new(bytes).parsed {
                return (true, false);
            }
            formatting = true;
        }
    }
    (false, formatting)
}
