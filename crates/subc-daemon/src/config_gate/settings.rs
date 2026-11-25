//! Settings reads separate admitted schema metadata from approved delivery values.

use super::settings_observation::{SettingsFiles, SettingsObservation};
use super::*;
use crate::config_gate::{reviewed_build, GateSnapshot, InputKey};
use crate::config_store::{HistoryEntry, StoreBody};
use serde_json::Value;
use subc_config::{Layer, Schema};
use subc_control::config::{
    ConfigControlResponse, ConfigModule, ConfigSettingsLayer, ConfigThrough,
};

#[path = "settings_projects.rs"]
pub(super) mod projects;

/// Owner inputs that can change a settings row without a file or catalog edit.
/// The publication separately compares the ungated module IDs from `config.status`.
#[derive(PartialEq, Eq)]
pub(super) struct SettingsVersion {
    store: (StoreState, u64),
    active: Vec<(ConnectionId, Option<u64>)>,
    plans: (u64, usize),
    live_projects: u64,
}

impl SettingsVersion {
    pub(super) fn from_state(state: &OwnerState) -> Self {
        Self {
            store: (
                state.store.loaded_admission_state().0,
                state.store.publication_sequence(),
            ),
            // Declarations are immutable per connection. Track their identity
            // and fallback acknowledgement without comparing the schemas.
            active: state
                .active_declarations
                .values()
                .map(|connection| {
                    (
                        *connection,
                        state.delivery.fallback_applied.get(connection).copied(),
                    )
                })
                .collect(),
            // Insertion (including replacement) advances the sequence; removal
            // alone changes the count. Neither requires inspecting plan bodies.
            plans: (state.gate.next_plan_sequence, state.gate.plans.len()),
            live_projects: state.delivery.live_projects.settings_revision,
        }
    }
}

struct SettingsSnapshot {
    declaration_pending: BTreeMap<String, subc_protocol::config::ConfigDeclarationPending>,
    fallback_applied: BTreeSet<String>,
    stamped: bool,
    declarations: BTreeMap<String, ConfigDeclareRequest>,
    pending: BTreeMap<String, Digest>,
    known: BTreeSet<String>,
}

#[derive(Default)]
struct ModuleApproval {
    approved: Option<u64>,
    active: Option<u64>,
    pending: bool,
    failed: Option<u64>,
}

impl RuntimeOwner {
    pub(crate) fn subscribe_settings(&self) -> tokio::sync::watch::Receiver<u64> {
        self.settings_changed.subscribe()
    }

    pub(crate) fn settings_projection_stale(&self) -> bool {
        // A disconnect can remove a declaration before a new version is published.
        self.projection_stale
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn settings_snapshot(
        &self,
        state: &OwnerState,
        approved: &GateSnapshot,
        mut known: BTreeSet<String>,
    ) -> Result<SettingsSnapshot> {
        let (_, body) = state.store.loaded_admission_state();
        let declarations = Self::loaded_declarations(state);
        known.extend(approved.inputs.keys().filter_map(|key| match key {
            InputKey::Entry(module) | InputKey::Declaration(module) => Some(module.clone()),
            _ => None,
        }));
        known.extend(declarations.keys().cloned());
        // The polling projection also knows configured modules without schemas.
        if !body.stamped {
            known.extend(
                self.polled_snapshot()?
                    .projection
                    .status
                    .ungated_modules
                    .iter()
                    .cloned(),
            );
        }
        let pending = known
            .iter()
            .filter_map(|module| {
                state
                    .gate
                    .pending_file_plan(module)
                    .map(|digest| (module.clone(), digest))
            })
            .collect();
        let declaration_pending = declarations
            .iter()
            .filter_map(|(module, declaration)| {
                Self::declaration_pending(state, module, declaration)
                    .map(|marker| (module.clone(), marker))
            })
            .collect();
        let fallback_applied = state
            .active_declarations
            .iter()
            .filter_map(|(module, connection)| {
                let generation = state.delivery.fallback_applied.get(connection)?;
                body.versions
                    .iter()
                    .any(|(tuple, versions)| {
                        tuple.module == *module
                            && tuple.project.is_none()
                            && versions.active == Some(*generation)
                    })
                    .then(|| module.clone())
            })
            .collect();
        Ok(SettingsSnapshot {
            declaration_pending,
            fallback_applied,
            stamped: body.stamped,
            declarations,
            pending,
            known,
        })
    }

    pub(crate) fn settings_modules(
        &self,
        known: BTreeSet<String>,
    ) -> Result<ConfigControlResponse> {
        self.observe_settings_modules(known)?
            .ok_or_else(GateError::capture_race)
    }

    fn observe_settings_modules(
        &self,
        known: BTreeSet<String>,
    ) -> Result<Option<ConfigControlResponse>> {
        Ok(self
            .observe_settings(known, false)?
            .map(|observation| observation.response))
    }

    // None means that concurrent edits prevented a consistent observation, not
    // that a settings file is invalid. Watches retain their previous rows.
    pub(crate) fn observe_settings(
        &self,
        known: BTreeSet<String>,
        watch_files: bool,
    ) -> Result<Option<SettingsObservation>> {
        self.observe_settings_rows(known, watch_files, false)
    }

    pub(crate) fn observe_settings_rows(
        &self,
        known: BTreeSet<String>,
        watch_files: bool,
        projects: bool,
    ) -> Result<Option<SettingsObservation>> {
        #[cfg(test)]
        if let Some(hook) = self.settings_projection_hook.lock().unwrap().as_mut() {
            hook()?;
        }
        let mut state = self.lock()?;
        state.check_authority_for_read()?;
        let (_, body) = state.store.loaded_admission_state();
        let approved = GateSnapshot::from_store(&body.approved)?;
        let snapshot = self.settings_snapshot(&state, &approved, known)?;
        let mut observation = None;
        for _ in 0..3 {
            let write_back = edits::loaded_write_back_status(&state)?;
            #[cfg(test)]
            if let Some(hook) = self.settings_status_hook.lock().unwrap().as_mut() {
                hook()?;
            }
            let mut files = SettingsFiles::default();
            let captured = if watch_files {
                self.capturer
                    .capture_settings_with(&snapshot.declarations, |path| files.capture(path))?
            } else {
                self.capturer.capture_settings(&snapshot.declarations)?
            };
            // External editors can still write while the daemon holds its lock.
            // Compare status on either side of capture using the same approval.
            if write_back == edits::loaded_write_back_status(&state)? {
                files.write_back_pending = !write_back.is_empty();
                observation = Some((write_back, captured, files));
                break;
            }
        }
        let Some((write_back, captured, files)) = observation else {
            return Ok(None);
        };
        if projects && snapshot.stamped {
            self.log_user_file_observations(&captured);
        }
        let rereview = if snapshot.stamped {
            reviewed_build::pending(body, snapshot.declarations.keys())?
        } else {
            BTreeMap::new()
        };
        let mut approvals = BTreeMap::new();
        for module in &snapshot.known {
            let entry = approved_entry(body, module);
            let versions = body.versions.iter().find_map(|(tuple, versions)| {
                (tuple.module == *module && tuple.project.is_none()).then_some(versions)
            });
            approvals.insert(
                module.as_str(),
                ModuleApproval {
                    approved: entry.map(|entry| entry.generation.id),
                    active: versions.and_then(|versions| versions.active),
                    pending: versions.is_some_and(|versions| versions.pending.is_some()),
                    failed: entry
                        .filter(|entry| entry.generation.never_redeliver.contains(module))
                        .map(|entry| entry.generation.id),
                },
            );
        }
        let project_state = projects.then_some(state);
        let observed = snapshot
            .stamped
            .then(|| captured.proposal_snapshot(&approved));
        let mut modules = Vec::new();
        for module in &snapshot.known {
            let mut row = ConfigModule::default();
            row.module_id = module.clone();
            row.gated = snapshot.declarations.contains_key(module);
            let mut module_error = captured.module_errors.get(module).cloned();
            if module_error.is_none() {
                if let Some(declaration) = snapshot.declarations.get(module) {
                    match schema_with_digest(declaration) {
                        Ok((schema, digest)) => {
                            row.display_name = schema
                                .normalized()
                                .get("title")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                            row.schema_digest = Some(digest);
                        }
                        Err(error) => module_error = Some(error),
                    }
                }
            }
            let approval = &approvals[module.as_str()];
            row.approved_generation = approval.approved;
            row.active_generation = approval.active;
            if !snapshot.stamped && row.gated && module_error.is_none() {
                row.approved_generation = Some(0);
                row.active_generation = Some(0);
            }
            row.pending_plan = snapshot.pending.get(module).cloned();
            row.declaration_pending = snapshot.declaration_pending.get(module).cloned();
            row.file_error = captured.file_errors.get(module).cloned();
            let mut proposal = false;
            let mut formatting = false;
            if let Some(observed) = observed.as_ref().filter(|_| row.file_error.is_none()) {
                for key in approved.changed_keys(observed).into_iter().filter(|key| {
                    key.module() == Some(module.as_str()) && !key.is_program() && !key.is_project()
                }) {
                    if approved.inputs.get(key).map(|input| &input.value)
                        == observed.inputs.get(key).map(|input| &input.value)
                    {
                        formatting = true;
                    } else {
                        proposal = true;
                    }
                }
            }
            let behind = write_back.get(module).cloned();
            row.status = settings_status(
                row.gated,
                approval.failed.is_some(),
                rereview.contains_key(module),
                approval.pending,
                behind.is_some(),
                proposal,
                formatting,
            )
            .into();
            row.status_reason = match row.status.as_str() {
                "failed_delivery" => approval
                    .failed
                    .map(|generation| format!("Generation {generation} failed delivery.")),
                "needs_rereview" => {
                    Some("The admitted program awaits configuration review.".into())
                }
                "write_back_behind" => behind,
                _ => None,
            };
            if row.declaration_pending.is_some() && snapshot.fallback_applied.contains(module) {
                row.status = "active".into();
                row.status_reason =
                    Some("Running last approved settings; declaration awaits approval.".into());
            }
            if let Some(error) = module_error {
                row.status = error.code;
                row.status_reason = Some(error.detail);
            }
            modules.push(row);
        }
        if let Some(state) = project_state {
            self.append_project_settings_rows(
                &state,
                &snapshot,
                &rereview,
                &write_back,
                &mut modules,
            )?;
        }
        Ok(Some(SettingsObservation {
            response: ConfigControlResponse::Modules {
                modules,
                stamped: snapshot.stamped,
            },
            files,
        }))
    }

    pub(crate) fn settings_schema(
        &self,
        known: BTreeSet<String>,
        module: String,
    ) -> Result<ConfigControlResponse> {
        let mut state = self.lock()?;
        state.check_authority_for_read()?;
        let (_, body) = state.store.loaded_admission_state();
        let approved = GateSnapshot::from_store(&body.approved)?;
        let snapshot = self.settings_snapshot(&state, &approved, known)?;
        drop(state);
        let declaration = snapshot.declaration(&module)?;
        crate::config_gate::strict_json::check_canonical(&declaration.schema, "/schema")?;
        let (schema, schema_digest) = schema_with_digest(declaration)?;
        Ok(ConfigControlResponse::Schema {
            module_id: module,
            schema_digest,
            schema: schema.normalized().clone(),
        })
    }

    pub(crate) fn settings_show(
        &self,
        known: BTreeSet<String>,
        module: String,
        through: Option<ConfigThrough>,
    ) -> Result<ConfigControlResponse> {
        self.user_show_view(known, &module)?
            .response(module, None, through)
    }

    fn user_show_view(&self, known: BTreeSet<String>, module: &str) -> Result<ShowView> {
        let mut state = self.lock()?;
        state.check_authority_for_read()?;
        let (_, body) = state.store.loaded_admission_state();
        let approved = GateSnapshot::from_store(&body.approved)?;
        let snapshot = self.settings_snapshot(&state, &approved, known)?;
        let declaration = snapshot.declaration(module)?;
        let (generation, view, approved_declaration) = if snapshot.stamped {
            let entry = approved_entry(body, module).ok_or_else(|| {
                GateError::new("config_gated", "module has no approved configuration")
            })?;
            let delivery = entry.delivery.as_ref().ok_or_else(|| {
                GateError::new("config_gated", "approved delivery is unavailable")
            })?;
            let view = delivery.modules.get(module).cloned().ok_or_else(|| {
                GateError::new("config_gated", "approved module view is unavailable")
            })?;
            let declaration =
                delivery.declarations.get(module).cloned().ok_or_else(|| {
                    GateError::new("config_gated", "approved schema is unavailable")
                })?;
            let generation = crate::config_gate::module_base_generation(body, module);
            drop(state);
            (generation, view, declaration)
        } else {
            drop(state);
            let captured = self.capturer.capture_module_settings(
                &BTreeMap::from([(module.into(), declaration.clone())]),
                &BTreeSet::from([module.into()]),
            )?;
            let captured = captured
                .modules
                .get(module)
                .ok_or_else(|| GateError::new("config_gated", "module view is unavailable"))?;
            (0, captured.view.clone(), declaration.clone())
        };
        let file_error = if snapshot.stamped {
            self.capturer.settings_file_error(module, declaration)?
        } else {
            None
        };
        Ok(ShowView {
            declaration: approved_declaration,
            view,
            stamped: snapshot.stamped,
            base_generation: generation,
            file_error,
            pending_plan: snapshot.pending.get(module).cloned(),
            declaration_pending: snapshot.declaration_pending.get(module).cloned(),
        })
    }
}

struct ShowView {
    declaration: ConfigDeclareRequest,
    view: ConfigView,
    stamped: bool,
    base_generation: u64,
    file_error: Option<subc_control::config::ConfigFileError>,
    pending_plan: Option<Digest>,
    declaration_pending: Option<subc_protocol::config::ConfigDeclarationPending>,
}

impl ShowView {
    fn response(
        self,
        module: String,
        project_id: Option<String>,
        through: Option<ConfigThrough>,
    ) -> Result<ConfigControlResponse> {
        let schema = schema(&self.declaration)?;
        let defaults =
            (through == Some(ConfigThrough::Default)).then(|| schema.settings_defaults());
        let mut layers = Vec::new();
        if project_id.is_none() {
            let document = defaults
                .as_ref()
                .map_or_else(|| schema.defaults(), |view| view.effective.clone());
            layers.push(ConfigSettingsLayer::new(Layer::Default, document));
        }
        for layer in &self.view.layers {
            let layer_name = match layer.layer {
                subc_protocol::config::ConfigLayer::User => Layer::User,
                subc_protocol::config::ConfigLayer::Workspace => Layer::Workspace,
                subc_protocol::config::ConfigLayer::Project => Layer::Project,
                _ => {
                    return Err(GateError::new(
                        "config_gated",
                        "approved layer is unsupported",
                    ))
                }
            };
            if !included(layer_name, through) {
                continue;
            }
            if project_id.is_some() && layer.bytes.is_none() {
                continue;
            }
            let document = match &layer.bytes {
                Some(bytes) => {
                    let text = std::str::from_utf8(bytes).map_err(GateError::invalid)?;
                    let text = subc_jsonc::jsonc_to_json(text).map_err(GateError::invalid)?;
                    crate::config_gate::strict_json::parse(&text)?
                }
                None => serde_json::json!({}),
            };
            let mut document = ConfigSettingsLayer::new(layer_name, document);
            if project_id.is_none() || layer_name == Layer::Workspace {
                document.workspace_id.clone_from(&layer.workspace_id);
            }
            if project_id.is_none() || layer_name == Layer::Project {
                document.project_id.clone_from(&layer.project_id);
            }
            layers.push(document);
        }
        layers.sort_by_key(|layer| match layer.layer {
            Layer::Default => 0,
            Layer::User => 1,
            Layer::Workspace => 2,
            Layer::Project => 3,
            _ => 4,
        });
        let (effective, sources) = if let Some(defaults) = defaults {
            (defaults.effective, defaults.sources)
        } else {
            let documents: Vec<_> = layers
                .iter()
                .map(|layer| (layer.layer, &layer.document))
                .collect();
            let sources =
                schema.settings_sources(&self.view.effective, &documents, self.view.sources);
            (self.view.effective, sources)
        };
        Ok(ConfigControlResponse::Show {
            module_id: module,
            project_id,
            through,
            stamped: self.stamped,
            base_generation: self.base_generation,
            effective,
            sources,
            layers,
            file_error: self.file_error,
            pending_plan: self.pending_plan,
            declaration_pending: self.declaration_pending,
        })
    }
}

impl SettingsSnapshot {
    fn declaration(&self, module: &str) -> Result<&ConfigDeclareRequest> {
        if !self.known.contains(module) {
            return Err(GateError::new("unknown_module", "module is unknown"));
        }
        self.declarations.get(module).ok_or_else(|| {
            GateError::new("module_ungated", "module declares no configuration schema")
        })
    }
}

pub(super) fn schema(declaration: &ConfigDeclareRequest) -> Result<Schema> {
    Schema::new(declaration.schema.clone())
        .map_err(|_| GateError::new("schema_invalid", "admitted schema is invalid"))
}

fn schema_with_digest(declaration: &ConfigDeclareRequest) -> Result<(Schema, Digest)> {
    let schema = schema(declaration)?;
    let digest = schema.digest().map_err(|error| {
        crate::config_gate::strict_json::canonical_error(error, schema.normalized(), "/schema")
    })?;
    Ok((schema, digest))
}

pub(super) fn approved_entry<'a>(body: &'a StoreBody, module: &str) -> Option<&'a HistoryEntry> {
    body.generations.values().rev().find(|entry| {
        !matches!(
            entry.kind,
            crate::config_store::CommitKind::Development
                | crate::config_store::CommitKind::DeclarationApproval
        ) && entry
            .tuples
            .iter()
            .any(|tuple| tuple.module == module && tuple.project.is_none())
    })
}

fn included(layer: Layer, through: Option<ConfigThrough>) -> bool {
    match through {
        Some(ConfigThrough::Default) => layer == Layer::Default,
        Some(ConfigThrough::User) => matches!(layer, Layer::Default | Layer::User),
        Some(ConfigThrough::Workspace) => layer != Layer::Project,
        None => true,
    }
}

/// Inability to run approved settings outranks file drift; a real proposal
/// outranks a formatting-only change. Ungated modules never claim adoption.
#[allow(clippy::too_many_arguments)]
fn settings_status(
    gated: bool,
    failed: bool,
    rereview: bool,
    activation: bool,
    write_back: bool,
    proposal: bool,
    formatting: bool,
) -> &'static str {
    if !gated {
        "ungated"
    } else if failed {
        "failed_delivery"
    } else if rereview {
        "needs_rereview"
    } else if activation {
        "pending_activation"
    } else if write_back {
        "write_back_behind"
    } else if proposal {
        "pending_proposal"
    } else if formatting {
        "formatting_only"
    } else {
        "adopted"
    }
}

#[cfg(test)]
#[path = "settings_tests.rs"]
pub(crate) mod tests;
