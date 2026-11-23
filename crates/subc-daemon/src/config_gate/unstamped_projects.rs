//! Live project reads and per-connection polling. Observations are notification
//! baselines, never a source of configuration bytes for `config.get`.

use super::*;
use crate::config_gate::{
    capture::{parse_layer, CapturedModule},
    capture_plan,
    project_layers::ProjectLayerSchema,
    project_registry::{live::LiveProjectCapture, RegistryRevision, RegistryScope},
};
use std::sync::Arc;
use subc_config::{raw_digest, Layer};
use subc_control::config::ConfigFileError;
use subc_protocol::config::{ConfigChanged, ConfigGetReply, ConfigGetRequest, ConfigLayer};

/// The last confirmed observation of a requested project on an unstamped
/// install. A row can disappear when the project drops or its files go away;
/// a read or validation error is reported separately from file presence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnstampedProjectObservation {
    pub(crate) workspace_id: Option<String>,
    pub(crate) present: bool,
    pub(crate) file_error: Option<ConfigFileError>,
}

#[derive(Default)]
pub(super) struct LiveProjects {
    slots: HashMap<ConnectionId, BTreeMap<String, Slot>>,
    sequence: u64,
    pub(super) settings_revision: u64,
}

#[derive(Default)]
struct Slot {
    sequence: u64,
    observation: Option<UnstampedProjectObservation>,
    fingerprint: Option<Digest>,
}

impl LiveProjects {
    pub(super) fn disconnected(&mut self, connection: ConnectionId) -> bool {
        let Some(slots) = self.slots.remove(&connection) else {
            return false;
        };
        if slots.values().any(|slot| slot.observation.is_some()) {
            self.settings_revision = self.settings_revision.wrapping_add(1);
        }
        true
    }

    fn next_sequence(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }
}

struct Declaration {
    request: ConfigDeclareRequest,
    schema: Arc<ProjectLayerSchema>,
}

pub(crate) struct UnstampedProjectRead {
    connection: ConnectionId,
    module: String,
    project: String,
    declaration: Declaration,
    incarnation: Arc<()>,
    sequence: u64,
}

pub(crate) struct UnstampedProjectHint {
    pub(crate) connection: ConnectionId,
    pub(crate) module: String,
    pub(crate) hint: ConfigChanged,
    incarnation: Arc<()>,
    fingerprint: Digest,
}

struct Observation {
    workspace_id: Option<String>,
    present: Option<bool>,
    result: Result<(ConfigView, Digest)>,
}

impl RuntimeOwner {
    /// Snapshot the watch inputs for requested tuples on active declarations.
    /// Keys are `(module_id, project_id)`; absent layers have `present: false`.
    /// A confirmed registry drop removes the entry. `subscribe_settings` wakes
    /// on changes to these observations. This never reads a project root.
    #[allow(dead_code, reason = "input to the project settings projection")]
    pub(crate) fn unstamped_project_observations(
        &self,
    ) -> Result<BTreeMap<(String, String), UnstampedProjectObservation>> {
        let state = self.lock()?;
        if !live(&state) {
            return Ok(BTreeMap::new());
        }
        Ok(state
            .active_declarations
            .iter()
            .flat_map(|(module, connection)| {
                state
                    .delivery
                    .live_projects
                    .slots
                    .get(connection)
                    .into_iter()
                    .flatten()
                    .filter_map(|(project, slot)| {
                        Some(((module.clone(), project.clone()), slot.observation.clone()?))
                    })
            })
            .collect())
    }

    pub(crate) fn begin_unstamped_project_get(
        &self,
        connection: ConnectionId,
        module: String,
        request: ConfigGetRequest,
    ) -> Result<Option<UnstampedProjectRead>> {
        let Some(project) = request
            .project_id
            .filter(|_| self.project_settings_enabled())
        else {
            return Ok(None);
        };
        let mut state = self.lock()?;
        if state.store.loaded_admission_state().0 != StoreState::Unstamped {
            return Ok(None);
        }
        state.store_read(ConfigStore::check)?;
        if !live(&state) {
            return Err(superseded());
        }
        let declaration = declaration(&state, connection, &module).ok_or_else(|| {
            GateError::new(
                "declaration_order",
                "config.declare must precede get or applied on this connection",
            )
        })?;
        if request.generation.is_some() || request.attempt.is_some() {
            return Err(GateError::new(
                "config_gated",
                "unstamped reads have no assigned generation or delivery attempt",
            ));
        }
        let incarnation = state.authority_incarnation.clone();
        let projects = &mut state.state.delivery.live_projects;
        let sequence = projects.next_sequence();
        projects
            .slots
            .entry(connection)
            .or_default()
            .entry(project.clone())
            .or_default();
        Ok(Some(UnstampedProjectRead {
            connection,
            module,
            project,
            declaration,
            incarnation,
            sequence,
        }))
    }

    pub(crate) async fn read_unstamped_project(
        self: &Arc<Self>,
        read: UnstampedProjectRead,
    ) -> Result<ConfigGetReply> {
        let scope = RegistryScope::new(
            BTreeSet::from([read.project.clone()]),
            BTreeSet::new(),
            BTreeSet::new(),
        )
        .waiting_for_root();
        let revision = self
            .capturer
            .projects
            .snapshot_scoped(&read.module, &scope)
            .await;
        self.on_blocking(move |owner| {
            let observation = owner.observe_live_project(&read, revision);
            owner.record_live_project(&read, &observation, false)?;
            let (view, _) = observation.result?;
            Ok(ConfigGetReply::new(Some(view), None))
        })
        .await
    }

    pub(crate) async fn poll_unstamped_projects(
        self: &Arc<Self>,
        hints: tokio::sync::mpsc::UnboundedSender<UnstampedProjectHint>,
    ) -> Result<()> {
        let mut requests = self
            .on_blocking(|owner| owner.live_project_requests())
            .await?;
        if requests.is_empty() {
            return Ok(());
        }
        let registry = &self.capturer.projects;
        let reply = registry.enumerate_live().await?;
        let pool = registry.root_io().ok_or_else(|| {
            GateError::new("config_unavailable", "registry capture worker stopped")
        })?;
        let mut capture = LiveProjectCapture::new(pool, &reply, requests.keys().cloned());
        while let Some((key, revision)) = capture.next().await {
            let Some(reads) = requests.remove(&key) else {
                continue;
            };
            let hints = hints.clone();
            self.on_blocking(move |owner| {
                for read in reads {
                    let observation = owner.observe_live_project(&read, revision.clone());
                    if let Ok(Some(hint)) = owner.record_live_project(&read, &observation, true) {
                        let _ = hints.send(hint);
                    }
                }
                Ok(())
            })
            .await?;
        }
        Ok(())
    }

    fn live_project_requests(
        &self,
    ) -> Result<BTreeMap<(String, String), Vec<UnstampedProjectRead>>> {
        let mut state = self.lock()?;
        if !live(&state) || state.delivery.live_projects.slots.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut requests = BTreeMap::<_, Vec<_>>::new();
        let sequence = state.state.delivery.live_projects.next_sequence();
        for (&connection, projects) in &state.delivery.live_projects.slots {
            let Some((module, _)) = state.declarations.get(&connection) else {
                continue;
            };
            for project in projects.keys() {
                let Some(declaration) = declaration(&state, connection, module) else {
                    continue;
                };
                requests
                    .entry((module.clone(), project.clone()))
                    .or_default()
                    .push(UnstampedProjectRead {
                        connection,
                        module: module.clone(),
                        project: project.clone(),
                        declaration,
                        incarnation: state.authority_incarnation.clone(),
                        sequence,
                    });
            }
        }
        Ok(requests)
    }
}

impl RuntimeOwner {
    fn observe_live_project(
        &self,
        read: &UnstampedProjectRead,
        revision: Result<RegistryRevision>,
    ) -> Observation {
        let mut observation = Observation {
            workspace_id: None,
            present: None,
            result: Err(superseded()),
        };
        observation.result = (|| {
            let revision = revision?;
            let project = revision.project(&read.project)?;
            observation.workspace_id.clone_from(&project.workspace_id);
            let layers = capture_plan::tuple_layers(&read.module, project, |root, path, layer| {
                let file = revision.observed_layer_file(&read.project, root, path, layer)?;
                if file.is_some() {
                    observation.present = Some(true);
                }
                Ok(file)
            })?;
            observation.present = Some(!layers.is_empty());
            let captured = self.capturer.capture_module_settings(
                &BTreeMap::from([(read.module.clone(), read.declaration.request.clone())]),
                &BTreeSet::from([read.module.clone()]),
            )?;
            let user = captured.modules.get(&read.module).ok_or_else(|| {
                GateError::new("config_gated", "module declaration is unavailable")
            })?;
            let fingerprint = subc_config::digest(
                subc_config::PURPOSE_SNAPSHOT,
                &serde_json::json!([
                    revision.digest_for_project(&read.project)?,
                    user.file.bytes.as_deref().map(raw_digest),
                    layers
                        .iter()
                        .map(|layer| layer.bytes.as_deref().map(raw_digest))
                        .collect::<Vec<_>>()
                ]),
            )
            .map_err(GateError::invalid)?;
            Ok((
                live_view(
                    &read.module,
                    &read.project,
                    &read.declaration.schema,
                    user,
                    layers,
                    None,
                )?,
                fingerprint,
            ))
        })();
        observation
    }

    fn record_live_project(
        &self,
        read: &UnstampedProjectRead,
        observed: &Observation,
        poll: bool,
    ) -> Result<Option<UnstampedProjectHint>> {
        let mut state = self.lock()?;
        if !live(&state)
            || !Arc::ptr_eq(&state.authority_incarnation, &read.incarnation)
            || !state
                .declarations
                .get(&read.connection)
                .is_some_and(|(module, declaration)| {
                    module == &read.module && declaration == &read.declaration.request
                })
        {
            return Err(superseded());
        }
        let projects = &mut state.state.delivery.live_projects;
        let Some(slot) = projects
            .slots
            .get_mut(&read.connection)
            .and_then(|slots| slots.get_mut(&read.project))
        else {
            return Err(superseded());
        };
        if read.sequence < slot.sequence {
            return Ok(None);
        }
        let error = observed.result.as_ref().err();
        // An outage is not a membership observation. Don't let an older
        // in-flight poll overwrite a newer successful observation, either.
        if error.is_some_and(|error| error.code == "project_registry_unavailable") {
            return Ok(None);
        }
        slot.sequence = read.sequence;
        let observation =
            (!error.is_some_and(|error| error.code == "unknown_project")).then(|| {
                UnstampedProjectObservation {
                    workspace_id: observed.workspace_id.clone(),
                    present: observed.present.unwrap_or_else(|| {
                        slot.observation.as_ref().is_some_and(|row| row.present)
                    }),
                    file_error: error.cloned().map(GateError::into_settings_file_error),
                }
            });
        let new_fingerprint = observed.result.as_ref().ok().map(|(_, digest)| digest);
        let hint = new_fingerprint
            .filter(|fingerprint| poll && Some(*fingerprint) != slot.fingerprint.as_ref())
            .map(|fingerprint| {
                let mut hint = ConfigChanged::new(0, vec![]);
                hint.project_id = Some(read.project.clone());
                UnstampedProjectHint {
                    connection: read.connection,
                    module: read.module.clone(),
                    hint,
                    incarnation: read.incarnation.clone(),
                    fingerprint: fingerprint.clone(),
                }
            });
        slot.fingerprint = new_fingerprint.cloned();
        if slot.observation != observation {
            slot.observation = observation;
            projects.settings_revision = projects.settings_revision.wrapping_add(1);
            state.changed = true;
        }
        Ok(hint)
    }

    pub(crate) fn unstamped_project_hint_current(
        &self,
        hint: &UnstampedProjectHint,
    ) -> Result<bool> {
        let state = self.lock()?;
        Ok(live(&state)
            && Arc::ptr_eq(&state.authority_incarnation, &hint.incarnation)
            && state
                .delivery
                .live_projects
                .slots
                .get(&hint.connection)
                .and_then(|slots| slots.get(hint.hint.project_id.as_deref()?))
                .and_then(|slot| slot.fingerprint.as_ref())
                == Some(&hint.fingerprint))
    }
}

fn declaration(state: &OwnerState, connection: ConnectionId, module: &str) -> Option<Declaration> {
    let (_, request) = state
        .declarations
        .get(&connection)
        .filter(|(id, _)| id == module)?;
    let schema = state.live_project_schemas.get(&connection)?;
    Some(Declaration {
        request: request.clone(),
        schema: schema.clone(),
    })
}

fn live(state: &OwnerState) -> bool {
    state.store.loaded_admission_state().0 == StoreState::Unstamped
        && state.authority_exit.is_none()
}

fn superseded() -> GateError {
    GateError::new(
        "delivery_superseded",
        "configuration authority changed during the project read",
    )
}

pub(super) fn live_view(
    module: &str,
    project: &str,
    schema: &ProjectLayerSchema,
    user: &CapturedModule,
    layers: Vec<subc_protocol::config::ConfigLayerSnapshot>,
    through: Option<subc_control::config::ConfigThrough>,
) -> Result<ConfigView> {
    use subc_control::config::ConfigThrough;
    let mut view = user.view.clone();
    view.project_id = Some(project.into());
    // The bare user result is the fallback only after both locations have
    // been read successfully and no workspace or project file is present.
    if layers.is_empty() {
        return Ok(view);
    }
    let recorded = layers
        .iter()
        .map(|layer| {
            let kind = if layer.layer == ConfigLayer::Workspace {
                Layer::Workspace
            } else {
                Layer::Project
            };
            schema
                .record_layer(
                    kind,
                    layer.bytes.as_deref().expect("only present project layers"),
                    module,
                    project,
                )
                .map_err(|error| {
                    error
                        .at_settings_layer(kind)
                        .with_settings_path(|_| layer.path.clone())
                })
        })
        .collect::<Result<Vec<_>>>()?;
    let user = user.file.bytes.as_deref().map(parse_layer).transpose()?;
    let locate = |error: GateError| {
        error.with_settings_path(|source| {
            let layer = if source == Layer::Workspace {
                ConfigLayer::Workspace
            } else {
                ConfigLayer::Project
            };
            layers.iter().find(|item| item.layer == layer)?.path.clone()
        })
    };
    let mut reviewed = schema
        .review_tuple(module, project, user.clone(), &recorded, None)
        .map_err(locate)?;
    let mut keep = layers.len();
    match through {
        Some(ConfigThrough::Default | ConfigThrough::User) => return Ok(view),
        Some(ConfigThrough::Workspace) => {
            keep = usize::from(layers[0].layer == ConfigLayer::Workspace);
            if keep == 0 {
                return Ok(view);
            }
            if keep < layers.len() {
                reviewed = schema
                    .review_tuple(module, project, user, &recorded[..keep], None)
                    .map_err(locate)?;
            }
        }
        None => {}
    }
    view.effective = reviewed.effective;
    view.sources = reviewed.sources;
    view.layers[0].project_id = view.project_id.clone();
    view.layers.extend(layers.into_iter().take(keep));
    Ok(view)
}
