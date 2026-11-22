mod migration;
mod schema;

use super::*;
use async_trait::async_trait;
use settings::{compose_gateway_config, parse_managed_config, RawGatewayConfig};
use std::{collections::BTreeSet, sync::Mutex as StdMutex};
use subc_client_rs::{
    config::{
        ConfigDeclareRequest, ConfigError, ConfigLayerSnapshot, ConfigReviewReply,
        ConfigReviewRequest, ConfigState, ConfigView, Layer, CONFIG_GATE_CAPABILITY,
    },
    HandlerOutcome, ModuleHandler, RequestCtx,
};
use subc_control::config::CONFIG_SETTINGS_PROJECTS_CAPABILITY;
use tokio::sync::Notify;

const SETTINGS_UNSUPPORTED: &str =
    "MCP gateway settings need a daemon with project settings (config.settings.projects/v1)";
const LEGACY_FILE: &str = "mcp.jsonc";
const USER_FILE: &str = "subc-mcp.jsonc";

#[derive(Default)]
pub(super) struct GatewaySettings {
    state: StdMutex<SettingsState>,
    changed: Notify,
}

#[derive(Default)]
struct SettingsState {
    startup_error: Option<String>,
    supports_projects: bool,
    delivery_status: Option<String>,
    delivery_error: Option<String>,
    views: BTreeMap<Option<String>, SettingsView>,
}

#[derive(Default)]
struct SettingsView {
    user: Option<RawGatewayConfig>,
    project: Option<RawGatewayConfig>,
    notes: Vec<String>,
}

impl SettingsView {
    fn from_layers(layers: &[ConfigLayerSnapshot]) -> Result<Self> {
        let mut view = Self::default();
        for layer in layers {
            let Some(bytes) = &layer.bytes else { continue };
            let path = Path::new(layer.path.as_deref().unwrap_or(USER_FILE));
            let raw = parse_managed_config(std::str::from_utf8(bytes)?, path)?;
            let slot = match layer.layer.known_layer() {
                Some(Layer::User) => &mut view.user,
                Some(Layer::Project) => &mut view.project,
                _ => {
                    return Err(other_error(
                        "MCP settings accept only user and project layers",
                    ))
                }
            };
            if slot.replace(raw).is_some() {
                return Err(other_error("duplicate MCP settings layer"));
            }
        }
        Ok(view)
    }

    fn config(&self, harness: &str) -> (GatewayConfig, Vec<settings::ProjectNote>) {
        compose_gateway_config(self.user.clone(), self.project.clone(), harness)
    }

    fn collect_notes(&mut self) {
        let names = self
            .user
            .iter()
            .chain(&self.project)
            .flat_map(RawGatewayConfig::harness_names)
            .map(str::to_ascii_lowercase)
            .collect::<BTreeSet<_>>();
        // The empty harness isolates the top-level settings (map keys cannot be empty).
        for name in std::iter::once("").chain(names.iter().map(String::as_str)) {
            for note in self.config(name).1 {
                if self.notes.len() == 32 {
                    return;
                }
                let prefix = if name.is_empty() {
                    String::new()
                } else {
                    format!("harness.{name}: ")
                };
                tracing::warn!(target: "catalog", "dropping project MCP config field {prefix}{}: {}", note.field, note.reason);
                self.notes
                    .push(format!("{prefix}{}: {}", note.field, note.reason));
            }
        }
    }
}

impl GatewaySettings {
    fn lock(&self) -> std::sync::MutexGuard<'_, SettingsState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn startup_check(&self) -> Result<()> {
        let state = self.lock();
        if let Some(error) = &state.startup_error {
            return Err(other_error(error));
        }
        if !state.supports_projects {
            return Err(other_error(SETTINGS_UNSUPPORTED));
        }
        Ok(())
    }

    pub(super) fn snapshot(
        &self,
        project_id: Option<&str>,
        harness: &str,
    ) -> Option<ConfigSnapshot> {
        let project_id = project_id.map(str::to_owned);
        let (effective, _) = self.lock().views.get(&project_id)?.config(harness);
        Some(ConfigSnapshot {
            project_id,
            effective,
        })
    }

    async fn wait_for_view(&self, project_id: Option<&str>, harness: &str) -> ConfigSnapshot {
        loop {
            let mut changed = std::pin::pin!(self.changed.notified());
            changed.as_mut().enable();
            if let Some(snapshot) = self.snapshot(project_id, harness) {
                return snapshot;
            }
            changed.await;
        }
    }

    fn health(&self) -> serde_json::Value {
        let state = self.lock();
        let notes = state
            .views
            .iter()
            .filter(|(_, view)| !view.notes.is_empty())
            .map(|(id, view)| serde_json::json!({"project_id": id, "ignored": view.notes}))
            .take(32)
            .collect::<Vec<_>>();
        serde_json::json!({
            "status": state.delivery_status,
            "startup_error": state.startup_error.as_deref().or_else(||
                (!state.supports_projects).then_some(SETTINGS_UNSUPPORTED)),
            "delivery_error": state.delivery_error,
            "project_notes": notes,
        })
    }
}

pub(super) struct GatewayModule {
    subc: SubcClient,
    module_id: String,
    user_file: PathBuf,
}

impl GatewayModule {
    pub(super) fn new(subc: SubcClient, module_id: String) -> Result<Self> {
        let home = PathBuf::from(cortexkit_store_types::resolve_config_home());
        if !home.is_absolute() {
            return Err(other_error("MCP settings require an absolute config home"));
        }
        let config_dir = home.join("cortexkit");
        let data_dir = PathBuf::from(cortexkit_store_types::resolve_data_home())
            .join("cortexkit")
            .join("subc-mcp");
        if let Err(error) = migration::convert(&config_dir, &data_dir) {
            let message = format!("MCP settings conversion failed: {error}");
            tracing::error!("{message}");
            subc.settings.lock().startup_error = Some(message);
        }
        Ok(Self {
            subc,
            module_id,
            user_file: config_dir.join(USER_FILE),
        })
    }
}

#[async_trait]
impl ModuleHandler for GatewayModule {
    async fn handle(&self, _: RequestCtx, _: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Error {
            code: "unsupported_request".into(),
            message: "MCP gateway does not serve daemon routes".into(),
        }
    }

    async fn on_hello_ack(&self, ack: &subc_protocol::ModuleHelloAckBody) {
        let supports = |capability| {
            ack.subc_capabilities
                .iter()
                .any(|value| value == capability)
        };
        let supported = supports(CONFIG_GATE_CAPABILITY)
            && (supports(CONFIG_SETTINGS_PROJECTS_CAPABILITY)
                || daemon_advertises_projects(&self.subc)
                    .await
                    .unwrap_or_else(|error| {
                        tracing::warn!("could not check daemon project settings support: {error}");
                        false
                    }));
        self.subc.settings.lock().supports_projects = supported;
    }

    fn config_declaration(&self) -> Option<ConfigDeclareRequest> {
        Some(ConfigDeclareRequest::new(
            schema::document(),
            self.user_file.to_string_lossy(),
            vec![],
        ))
    }

    async fn config_review(&self, request: ConfigReviewRequest) -> ConfigReviewReply {
        schema::review(&self.module_id, &request)
    }

    async fn config_apply(&self, view: ConfigView) -> std::result::Result<(), Vec<String>> {
        self.subc
            .settings
            .startup_check()
            .map_err(|error| vec![error.to_string()])?;
        let mut settings =
            SettingsView::from_layers(&view.layers).map_err(|error| vec![error.to_string()])?;
        settings.collect_notes();
        self.subc
            .settings
            .lock()
            .views
            .insert(view.project_id, settings);
        self.subc.settings.changed.notify_waiters();
        Ok(())
    }

    async fn on_config_state(&self, state: ConfigState) {
        let mut status = self.subc.settings.lock();
        status.delivery_status = Some(state.as_str().into());
        status.delivery_error = match state {
            ConfigState::Failed { reasons, .. } => Some(reasons.join("; ")),
            ConfigState::ConfigGated { refusal, .. }
            | ConfigState::NotDeliveryTarget { refusal, .. }
            | ConfigState::AnchorPending { refusal, .. } => Some(refusal.message),
            _ => None,
        };
    }

    async fn on_config_error(&self, error: ConfigError) {
        tracing::warn!("MCP settings delivery failed: {error}");
        self.subc.settings.lock().delivery_error = Some(error.to_string());
    }

    async fn health(&self) -> HealthReport {
        let mut report = self.subc.health_report().await;
        if let Some(metrics) = &mut report.metrics {
            metrics["settings"] = self.subc.settings.health();
        }
        if let Err(error) = self.subc.settings.startup_check() {
            report.status = HealthStatus::Degraded;
            if report.detail.is_none() {
                report.detail = Some(error.to_string());
            }
        }
        report
    }
}

async fn daemon_advertises_projects(subc: &SubcClient) -> Result<bool> {
    let frame = build_frame(
        FrameType::Request,
        control_flags(),
        0,
        0,
        subc.next_corr()?,
        serde_json::to_vec(&ClientControlRequest::ServerDescribe {})?,
    )?;
    let response = subc.request(frame, SUBC_RESPONSE_TIMEOUT).await?;
    if response.header.ty == FrameType::Error {
        return Err(error_response("server.describe failed", &response.body));
    }
    match serde_json::from_slice::<ClientControlResponse>(&response.body)? {
        ClientControlResponse::ServerDescribe { capabilities, .. } => Ok(capabilities
            .iter()
            .any(|capability| capability == CONFIG_SETTINGS_PROJECTS_CAPABILITY)),
        _ => Err(other_error(
            "unexpected server.describe response for project settings",
        )),
    }
}

pub(super) async fn session_config(subc: &SubcClient, hello: &ShimHello) -> Result<ConfigSnapshot> {
    subc.settings.startup_check()?;
    let root = session_scope::canonical_dir(&hello.project_root)?;
    check_project_files(&root)?;
    let workdir = root
        .to_str()
        .ok_or_else(|| other_error("project root must be valid UTF-8"))?;
    let identity = BindIdentity::new(hello.harness.clone(), bind_session_from_hello(hello)?);
    let relay = Arc::new(RelaySession::new(identity.session.clone()));
    let project_id = session_scope::resolve_project(subc, &identity, &relay, workdir).await?;
    if let Some(id) = &project_id {
        if let Some(handle) = subc.settings_handle.get() {
            handle.refresh_project_config(id.clone());
        }
    }
    time::timeout(
        SUBC_RESPONSE_TIMEOUT,
        subc.settings
            .wait_for_view(project_id.as_deref(), &hello.harness),
    )
    .await
    .map_err(|_| {
        other_error(
            "MCP settings unavailable: the daemon has not delivered an approved settings view",
        )
    })
}

fn check_project_files(root: &Path) -> Result<()> {
    let settings = root.join(".cortexkit");
    if migration::exists(&settings.join(LEGACY_FILE))?
        && !migration::exists(&settings.join(USER_FILE))?
    {
        return Err(other_error(format!(
            "Legacy project MCP settings at {} are unavailable: rename the file to {USER_FILE} and use the new snake_case key names",
            settings.join(LEGACY_FILE).display(),
        )));
    }
    Ok(())
}
