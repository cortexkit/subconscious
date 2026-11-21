//! `supervisor.rescan_checked`: previewing caller-supplied configuration text
//! without writing or applying it, and applying the config file only while the
//! file and the staged configuration still match that preview.
//!
//! Both halves run through [`ControlHandler::handle_supervisor_rescan`], the
//! code `supervisor.rescan` uses, so a preview and an apply cannot drift from
//! what a plain rescan does. This file holds only what differs: where the
//! configuration text comes from, the apply's precondition, and the extra
//! detail a candidate preview reports.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};

use sha2::{Digest, Sha256};
use subc_control::{
    ops, BusyGaugeReading, LaunchSpecAgreement, RescanExpect, RescanModuleAction,
    RescanModuleActionKind, StagedGeneration, SupervisorRescanResult, CONFIG_CHANGED_SINCE_PREVIEW,
    RESCAN_CANDIDATE_MAX_BYTES, RESCAN_CANDIDATE_PREVIEW_CAPABILITY, RESCAN_CANDIDATE_TOO_LARGE,
};

use super::*;
use crate::{
    daemon_config::{ActivityConfig, ConfiguredModule, DaemonConfig},
    router::{RouteCtx, RouterError},
    supervise::{ModuleSpec, SupervisedModule},
    Frame, HealthConfig,
};

#[cfg(test)]
mod tests;

/// Where a rescan's configuration comes from, and whether it is applied.
#[derive(Clone)]
pub(super) enum RescanMode {
    /// `supervisor.rescan`: read the config file and apply it.
    Execute,
    /// `supervisor.rescan { preview: true }`: read the config file and apply
    /// nothing.
    Preview,
    /// A `supervisor.rescan_checked` preview: parse this text and apply
    /// nothing.
    Candidate(String),
    /// A `supervisor.rescan_checked` apply: read the config file and apply it,
    /// but only if the file and the staged configuration still match.
    Checked(RescanExpect),
}

const CONFIG_READ_DEADLINE: Duration = Duration::from_secs(5);
// A timed-out filesystem read cannot be cancelled. Its slot stays charged
// until it finishes, so repeated rescans cannot create unbounded threads.
static CONFIG_READ_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(super) struct RescanInput {
    pub(super) config: Option<(DaemonConfig, ActivityConfig)>,
    pub(super) approved_preview: Option<DaemonConfig>,
    pub(super) guard: tokio::sync::OwnedMutexGuard<()>,
}

pub(super) enum RescanInputError {
    Config(&'static str, String),
    Gate(crate::config_gate::GateError),
}

impl RescanInputError {
    pub(super) fn into_frame(self, request: &Frame) -> Result<Frame, RouterError> {
        match self {
            Self::Config(code, message) => control_error_frame(request, code, message),
            Self::Gate(error) => config_gate::config_error_frame(request, error),
        }
    }
}

impl RescanMode {
    pub(super) fn previews(&self) -> bool {
        matches!(self, Self::Preview | Self::Candidate(_))
    }
}

/// One supervised module as a rescan finds it: the module, its staged launch
/// spec and health policy, and whether it is enabled.
pub(super) type CurrentModule = (SupervisedModule, ModuleSpec, HealthConfig, bool);

#[derive(Debug, Default)]
pub(super) struct ModuleEntries {
    registration_channels: BTreeMap<String, bool>,
    configured: BTreeMap<String, ConfiguredModule>,
}

impl ModuleEntries {
    pub(super) fn reserved(&self, module_id: &str) -> bool {
        self.configured
            .get(module_id)
            .is_some_and(|entry| entry.reserved)
    }
    pub(super) fn seed(&mut self, modules: &[ConfiguredModule]) {
        self.configured = modules
            .iter()
            .cloned()
            .map(|m| (m.module_id.clone(), m))
            .collect();
        self.registration_channels = modules
            .iter()
            .map(|m| (m.module_id.clone(), m.registration_channel))
            .collect();
    }

    pub(super) fn record(
        &mut self,
        configured: &BTreeMap<String, ConfiguredModule>,
        added: &[String],
        removed: &[String],
    ) {
        for id in removed {
            self.registration_channels.remove(id);
        }
        for id in added {
            self.registration_channels
                .insert(id.clone(), configured[id].registration_channel);
        }
        self.configured = configured.clone();
    }
}

pub(super) fn entry_pending_fields(
    current: &ConfiguredModule,
    next: &ConfiguredModule,
    applies_configuration: bool,
) -> Vec<subc_control::SupervisorPendingField> {
    use subc_control::{PendingModuleApply as Apply, SupervisorPendingField};
    let mut fields = Vec::new();
    let mut add = |field: &str, applies, value| {
        fields.push(SupervisorPendingField {
            field: field.to_owned(),
            applies,
            value: Some(value),
        })
    };
    if current.register_timeout != next.register_timeout {
        add(
            "register_timeout_s",
            Apply::NextSupervision,
            serde_json::json!(next.register_timeout.as_secs()),
        );
    }
    if current.restart != next.restart {
        add(
            "restart",
            Apply::NextSupervision,
            serde_json::json!({
                "max_restarts": next.restart.max_restarts,
                "window_secs": next.restart.window.as_secs(),
                "backoff_ms": next.restart.backoff.as_millis() as u64,
                "max_backoff_ms": next.restart.max_backoff.as_millis() as u64,
            }),
        );
    }
    let drain = |module: &ConfiguredModule| {
        module
            .drain_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(crate::supervise::DEFAULT_DRAIN_TIMEOUT)
    };
    if !applies_configuration && drain(current) != drain(next) {
        add(
            "drain_timeout_ms",
            Apply::WithNextLaunchOrHealthChange,
            serde_json::json!(drain(next).as_millis() as u64),
        );
    }
    let relay = |module: &ConfiguredModule| {
        module
            .route_bind_relay_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(crate::DEFAULT_ROUTE_BIND_RELAY_TIMEOUT)
    };
    if relay(current) != relay(next) {
        add(
            "route_bind_relay_timeout_ms",
            Apply::DaemonRestart,
            serde_json::json!(relay(next).as_millis() as u64),
        );
    }
    if current.registration_channel != next.registration_channel {
        add(
            "registration_channel",
            Apply::ConfigGate,
            serde_json::json!(next.registration_channel),
        );
    }
    fields
}

impl ControlHandler {
    pub(super) fn registration_channel_in_force(
        declaration: Option<&str>,
        configured: Option<bool>,
    ) -> Option<bool> {
        match declaration {
            Some("declared") => Some(true),
            Some("undeclared") => Some(false),
            // Entries with `protocol: none` speak no control protocol and have
            // no registration channel; use their approved metadata instead.
            _ => configured,
        }
    }

    pub(super) fn pending_module_fields(
        &self,
        module: &SupervisedModule,
        proposed: Option<&ConfiguredModule>,
        applies_configuration: bool,
        approved_registration_channel: Option<bool>,
        publication: Option<&crate::config_gate::runtime::Published>,
    ) -> Result<Vec<subc_control::SupervisorPendingField>, String> {
        let id = module.module_id();
        let (recorded, registration_channel) = self
            .rescan
            .as_ref()
            .map(|context| {
                let entries = context
                    .module_entries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                (
                    entries.configured.get(id).cloned(),
                    entries
                        .registration_channels
                        .get(id)
                        .copied()
                        .unwrap_or(false),
                )
            })
            .unwrap_or_default();
        let Some(next) = proposed.cloned().or(recorded) else {
            return Ok(Vec::new());
        };
        let status = module.status().map_err(|e| e.to_string())?;
        let mut current = next.clone();
        current.register_timeout = module
            .configured_register_timeout()
            .map_err(|e| e.to_string())?;
        current.restart = crate::supervise::RestartPolicy {
            max_restarts: status.max_restarts,
            window: status.restart_window,
            backoff: status.restart_backoff,
            max_backoff: status.restart_max_backoff,
        };
        current.drain_timeout_ms = Some(status.drain_timeout.as_millis() as u64);
        current.route_bind_relay_timeout_ms =
            Some(self.route_bind_relay_timeout_for(id).as_millis() as u64);
        current.registration_channel =
            approved_registration_channel.unwrap_or(registration_channel);
        let mut fields = entry_pending_fields(&current, &next, applies_configuration);
        // The caller passes the publication it already read for the whole
        // list, so a status of many modules reads it once. With no settings
        // owner, or one reporting an unstamped install, nothing is pending.
        // An unreadable publication keeps the field, as the safer report.
        let unstamped = match &self.config_runtime {
            None => true,
            Some(_) => publication.is_some_and(|view| {
                matches!(
                    view.approved_daemon,
                    crate::config_gate::runtime::ApprovedDaemonView::NotStamped
                )
            }),
        };
        if unstamped {
            fields.retain(|field| field.field != "registration_channel");
        }
        Ok(fields)
    }
}

/// The comparison a rescan makes between the configured modules and the
/// supervised ones, before anything is applied.
pub(super) struct RescanPlan {
    pub(super) current: BTreeMap<String, CurrentModule>,
    pub(super) configured: BTreeMap<String, ConfiguredModule>,
    pub(super) added: Vec<String>,
    pub(super) removed: Vec<String>,
    /// Modules whose launch spec changes. Their running process keeps the old
    /// one until it restarts.
    pub(super) changed_pending_reload: Vec<String>,
    pub(super) changed_pending: Vec<subc_control::SupervisorPendingModule>,
    /// Modules whose launch spec or health policy changes.
    pub(super) configuration_changes: BTreeSet<String>,
    pub(super) enabled_changes: BTreeSet<String>,
    pub(super) unchanged: u32,
}

impl RescanPlan {
    pub(super) fn preview_result(&self) -> SupervisorRescanResult {
        SupervisorRescanResult {
            added: self.added.clone(),
            removed: self.removed.clone(),
            changed_pending_reload: self.changed_pending_reload.clone(),
            changed_pending: self.changed_pending.clone(),
            enabled_changes: self.enabled_changes.iter().cloned().collect(),
            unchanged: self.unchanged,
            preview: true,
            // Filled by the caller on both paths, so the preview reports
            // restart-required sections identically to an executed rescan --
            // the preview is where an operator is most likely to be looking.
            restart_required: Vec::new(),
            capability_warnings: Vec::new(),
            module_actions: Vec::new(),
            earlier_pending: Vec::new(),
            expect: None,
        }
    }
}

/// The configuration path `server.describe` reports: the path the daemon
/// reads, unchanged, or nothing when that path is relative. A relative path
/// resolves against the daemon's working directory, so a client reading it
/// would find a different file. A path that is not valid UTF-8 is left out
/// too, because the reply carries it as a JSON string.
pub(super) fn reported_config_path(path: &Path) -> Option<PathBuf> {
    (path.is_absolute() && path.to_str().is_some()).then(|| path.to_path_buf())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The parts of a module's entry that differ between its staged state and the
/// configured entry, named as `RescanModuleAction::changed` documents.
fn changed_parts(
    current_spec: &ModuleSpec,
    current_health: &HealthConfig,
    configured: &ConfiguredModule,
) -> Vec<String> {
    let next = configured.module_spec();
    [
        ("program", current_spec.program != next.program),
        ("arguments", current_spec.args != next.args),
        ("environment", current_spec.env != next.env),
        ("protocol", current_spec.protocol != next.protocol),
        ("reserved", current_spec.reserved != next.reserved),
        (
            "reserved_prefixes",
            current_spec.reserved_prefixes != next.reserved_prefixes,
        ),
        ("overlap", current_spec.overlap != next.overlap),
        (
            "carry_containment",
            current_spec.carry_containment != next.carry_containment,
        ),
        ("health", *current_health != configured.health),
    ]
    .into_iter()
    .filter(|(_, changed)| *changed)
    .map(|(part, _)| part.to_string())
    .collect()
}

impl ControlHandler {
    /// The capabilities `server.describe` lists. `supervisor.rescan_checked`
    /// is announced only by a daemon that has a config file to rescan; any
    /// other daemon refuses the op as `rescan_unavailable`.
    pub(super) fn describe_capabilities(&self) -> Vec<String> {
        let mut capabilities = self.subc_capabilities.as_ref().to_vec();
        if super::config_gate::CONFIG_SETTINGS_SURFACE_ENABLED {
            if let Some(owner) = &self.config_runtime {
                capabilities.push(subc_control::config::CONFIG_SETTINGS_CAPABILITY.to_owned());
                if owner.project_settings_enabled() {
                    capabilities
                        .push(subc_control::config::CONFIG_SETTINGS_PROJECTS_CAPABILITY.to_owned());
                }
            }
        }
        if self.rescan.is_some() {
            capabilities.push(RESCAN_CANDIDATE_PREVIEW_CAPABILITY.to_string());
        }
        capabilities
    }

    fn staged_generation(&self, context: &SupervisorRescanContext) -> StagedGeneration {
        StagedGeneration {
            incarnation: self.supervisor.spawn_snapshot().cursor.daemon_incarnation,
            counter: context.generation.load(Ordering::SeqCst),
        }
    }

    pub(super) async fn handle_supervisor_rescan_checked(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        preview: bool,
        candidate: Option<String>,
        expect: Option<RescanExpect>,
    ) -> Result<Vec<Frame>, RouterError> {
        let corr = frame.header.corr;
        let mode = match (preview, candidate, expect) {
            (true, Some(candidate), None) => {
                if candidate.len() > RESCAN_CANDIDATE_MAX_BYTES {
                    return Ok(vec![control_error_frame(
                        &frame,
                        RESCAN_CANDIDATE_TOO_LARGE,
                        format!(
                            "the candidate config is {} bytes; a preview accepts at most {RESCAN_CANDIDATE_MAX_BYTES}",
                            candidate.len()
                        ),
                    )?]);
                }
                RescanMode::Candidate(candidate)
            }
            (false, None, Some(expect)) => {
                self.log_supervisor_request_received(
                    ctx,
                    corr,
                    ops::SUPERVISOR_RESCAN_CHECKED,
                    None,
                    None,
                )?;
                RescanMode::Checked(expect)
            }
            _ => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_control_body",
                    "supervisor.rescan_checked takes either preview: true with a candidate and \
                     no expect, or preview: false with an expect and no candidate",
                )?]);
            }
        };
        let requester = self.supervisor_requester(ctx, corr)?;
        self.handle_supervisor_rescan(frame, mode, requester).await
    }

    // The caller holds the rescan lock through both read and apply. Take the
    // supervisor operation lock only after the read so other operations can
    // proceed during file I/O. Check the staged generation for changes made
    // outside the rescan lock, then build the plan from current module state.
    pub(super) async fn rescan_input(
        &self,
        mode: &RescanMode,
        context: &SupervisorRescanContext,
    ) -> Result<RescanInput, RescanInputError> {
        use tokio::time::{timeout_at, Instant};
        use RescanInputError::Config;

        let staged = self.staged_generation(context);
        let (config, approved_preview) = if matches!(mode, RescanMode::Candidate(_)) {
            (Self::read_rescan_config(mode, &context.config_path)?, None)
        } else {
            let deadline = Instant::now() + CONFIG_READ_DEADLINE;
            let timed_out = || {
                Config(
                    "rescan_config_read_timeout",
                    format!(
                        "supervisor rescan timed out reading daemon config at {}; nothing was applied",
                        context.config_path.display()
                    ),
                )
            };
            let permit = timeout_at(deadline, CONFIG_READ_SLOTS.acquire())
                .await
                .map_err(|_| timed_out())?
                .expect("rescan read slots never close");
            if Instant::now() >= deadline {
                return Err(timed_out());
            }
            let path = context.config_path.clone();
            let owner = if matches!(mode, RescanMode::Preview) {
                self.config_runtime.clone()
            } else {
                None
            };
            let mode = mode.clone();
            let (send, recv) = tokio::sync::oneshot::channel();
            std::thread::Builder::new()
                .name("rescan-config-read".into())
                .spawn(move || {
                    let _permit = permit;
                    let result = (|| {
                        let config = Self::read_rescan_config(&mode, &path)?;
                        // Preview can record a file proposal, but only the caller
                        // (after the deadline and generation checks) can apply a
                        // rescan. The worker has no supervisor handle.
                        let approved = if let Some(owner) = owner.filter(|_| !send.is_closed()) {
                            owner
                                .observe_file_proposal()
                                .map_err(RescanInputError::Gate)?;
                            owner
                                .approved_daemon_configuration()
                                .map_err(RescanInputError::Gate)?
                        } else {
                            None
                        };
                        Ok((config, approved))
                    })();
                    let _ = send.send(result);
                })
                .map_err(|error| {
                    Config(
                        "rescan_failed",
                        format!("could not start supervisor rescan config read: {error}"),
                    )
                })?;
            let result = timeout_at(deadline, recv).await.map_err(|_| timed_out())?;
            // Tokio polls the result before the timer when both are ready.
            // Never apply a read after its deadline, even in that race.
            if Instant::now() >= deadline {
                return Err(timed_out());
            }
            result.map_err(|error| {
                Config(
                    "rescan_failed",
                    format!("supervisor rescan config reader stopped: {error}"),
                )
            })??
        };

        let guard = self.supervisor.operation_lock().lock_owned().await;
        let current = self.staged_generation(context);
        let changed = match mode {
            RescanMode::Candidate(_) => None,
            RescanMode::Checked(expect) if current != expect.staged_generation => Some((
                CONFIG_CHANGED_SINCE_PREVIEW,
                &expect.staged_generation,
                "after the preview",
            )),
            _ if current != staged => {
                Some(("rescan_failed", &staged, "while reading daemon config"))
            }
            _ => None,
        };
        if let Some((code, before, reason)) = changed {
            return Err(Config(code, format!(
                "the daemon's staged configuration changed {reason} (was {}:{}, now {}:{}); nothing was applied",
                before.incarnation, before.counter, current.incarnation, current.counter,
            )));
        }
        Ok(RescanInput {
            config,
            approved_preview,
            guard,
        })
    }

    fn read_rescan_config(
        mode: &RescanMode,
        path: &Path,
    ) -> Result<Option<(DaemonConfig, ActivityConfig)>, RescanInputError> {
        let rejected = |err: crate::daemon_config::DaemonConfigError| {
            RescanInputError::Config(
                "invalid_daemon_config",
                format!("supervisor rescan rejected daemon config: {err}"),
            )
        };
        match mode {
            RescanMode::Execute | RescanMode::Preview => {
                crate::daemon_config::load_with_activity(path).map_err(rejected)
            }
            RescanMode::Candidate(candidate) => {
                crate::daemon_config::parse_with_activity(candidate, path)
                    .map(Some)
                    .map_err(rejected)
            }
            RescanMode::Checked(expect) => {
                let doc = Self::checked_config_doc(path, expect)?;
                crate::daemon_config::parse_with_activity(&doc, path)
                    .map(Some)
                    .map_err(rejected)
            }
        }
    }

    /// Read and hash the bytes that will be parsed, so a write landing after
    /// the hash check cannot be what gets applied.
    fn checked_config_doc(path: &Path, expect: &RescanExpect) -> Result<String, RescanInputError> {
        use RescanInputError::Config;
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(Config(
                    CONFIG_CHANGED_SINCE_PREVIEW,
                    format!(
                        "daemon config not found at {}; nothing was applied",
                        path.display()
                    ),
                ));
            }
            Err(err) => {
                return Err(Config(
                    "invalid_daemon_config",
                    format!(
                        "supervisor rescan could not read daemon config at {}: {err}",
                        path.display()
                    ),
                ));
            }
        };
        if !sha256_hex(&bytes).eq_ignore_ascii_case(&expect.config_sha256) {
            return Err(Config(
                CONFIG_CHANGED_SINCE_PREVIEW,
                format!(
                    "daemon config at {} is not the text that was previewed; nothing was applied",
                    path.display(),
                ),
            ));
        }
        // The hash matched text that arrived as a JSON string, so this only
        // fails on a hash collision; refuse rather than assume.
        String::from_utf8(bytes).map_err(|err| {
            Config(
                "invalid_daemon_config",
                format!("daemon config at {} is not UTF-8: {err}", path.display()),
            )
        })
    }

    /// The preview of `candidate` (already parsed into `modules`): the plain
    /// rescan preview plus what applying it would do to each module, the
    /// modules still waiting for a restart from an earlier rescan, and the
    /// `expect` an apply must send. Applies nothing.
    pub(super) fn preview_candidate(
        &self,
        context: &SupervisorRescanContext,
        modules: Vec<ConfiguredModule>,
        candidate: &str,
    ) -> Result<SupervisorRescanResult, String> {
        let plan = self.plan_rescan(modules)?;
        let mut result = plan.preview_result();
        let default_drain_ms = duration_ms(context.supervisor.default_drain_timeout());
        let module_ids = plan
            .current
            .keys()
            .chain(plan.configured.keys())
            .collect::<BTreeSet<_>>();
        let mut drained = BTreeSet::new();
        for module_id in module_ids {
            let current = plan.current.get(module_id);
            let configured = plan.configured.get(module_id);
            let (action, changed) = match (current, configured) {
                (None, Some(configured)) if configured.enabled => {
                    (RescanModuleActionKind::Start, Vec::new())
                }
                (Some((_, _, _, true)), None) => (RescanModuleActionKind::Stop, Vec::new()),
                (Some((_, _, _, enabled)), Some(configured)) if *enabled != configured.enabled => {
                    if configured.enabled {
                        (RescanModuleActionKind::Start, Vec::new())
                    } else {
                        (RescanModuleActionKind::Stop, Vec::new())
                    }
                }
                (Some((_, spec, health, enabled)), Some(configured))
                    if plan.configuration_changes.contains(module_id) =>
                {
                    let changed = changed_parts(spec, health, configured);
                    // A rescan stores new launch inputs without touching the
                    // running process, so only a module that is enabled needs
                    // a restart to use them. A disabled one starts with them
                    // whenever it is enabled.
                    if *enabled && plan.changed_pending_reload.contains(module_id) {
                        (RescanModuleActionKind::Restart, changed)
                    } else {
                        (RescanModuleActionKind::Reload, changed)
                    }
                }
                _ => continue,
            };
            let mut entry = RescanModuleAction {
                module_id: module_id.clone(),
                action,
                changed,
                drain_timeout_ms: None,
                busy_gauges: Vec::new(),
            };
            if matches!(
                entry.action,
                RescanModuleActionKind::Restart | RescanModuleActionKind::Stop
            ) {
                if let Some((module, ..)) = current {
                    let status = module.status().map_err(|err| {
                        format!("failed to read status for module_id '{module_id}': {err}")
                    })?;
                    // A rescan that changes the entry also installs its drain
                    // budget before anything drains; otherwise the module
                    // keeps the budget it runs with now.
                    entry.drain_timeout_ms = Some(match configured {
                        Some(configured) if plan.configuration_changes.contains(module_id) => {
                            configured.drain_timeout_ms.unwrap_or(default_drain_ms)
                        }
                        _ => duration_ms(status.drain_timeout),
                    });
                    let metrics = status
                        .health
                        .metrics
                        .as_ref()
                        .and_then(serde_json::Value::as_object);
                    entry.busy_gauges =
                        crate::supervise::declared_busy_gauges(&self.registry, module_id)
                            .map_err(|err| {
                                format!(
                                    "failed to read busy gauges for module_id '{module_id}': {err}"
                                )
                            })?
                            .into_iter()
                            .map(|gauge| BusyGaugeReading {
                                value: metrics.and_then(|metrics| metrics.get(&gauge)).cloned(),
                                gauge,
                            })
                            .collect();
                }
                drained.insert(module_id.clone());
            }
            result.module_actions.push(entry);
        }
        for (module_id, (module, spec, _, _)) in &plan.current {
            if drained.contains(module_id) {
                continue;
            }
            let status = module.status().map_err(|err| {
                format!("failed to read status for module_id '{module_id}': {err}")
            })?;
            if launch_spec_agreement(
                spec,
                status.process_alive,
                status.launch_spec_digest.as_ref(),
            ) == LaunchSpecAgreement::Mismatch
            {
                result.earlier_pending.push(module_id.clone());
            }
        }
        result.expect = Some(RescanExpect {
            config_sha256: sha256_hex(candidate.as_bytes()),
            staged_generation: self.staged_generation(context),
        });
        Ok(result)
    }
}

#[cfg(all(test, unix))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_config_read_times_out_without_holding_operation_lock() {
    use crate::{Registry, RestartPolicy, Supervisor, SupervisorHandle};
    use cortexkit_test_support::ScratchDir;
    use std::io::Write;
    use std::sync::{mpsc, Arc};
    use subc_protocol::{FrameType, Principal};

    let dir = ScratchDir::new("rescan-stalled-file");
    let path = dir.join("subc.jsonc");
    assert!(std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap()
        .success());
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor =
        Supervisor::new(registry.clone(), RestartPolicy::default()).with_handle(handle.clone());
    let handler = Arc::new(
        ControlHandler::new(registry)
            .with_supervisor(handle.clone())
            .with_supervisor_rescan(supervisor, &path, None),
    );
    let (opened, is_open) = tokio::sync::oneshot::channel();
    let (release, released) = mpsc::channel();
    let feeder = std::thread::spawn(move || {
        let mut fifo = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        opened.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
        let _ = fifo.write_all(br#"{"version":1,"modules":{"never-applied":{"program":"/bin/true","protocol":"none","enabled":false}}}"#);
    });
    let request = tokio::spawn({
        let handler = Arc::clone(&handler);
        async move {
            let frame = Frame::build(
                FrameType::Request,
                super::control_flags(),
                0,
                0,
                1,
                Vec::new(),
            )
            .unwrap();
            handler
                .handle_supervisor_rescan(frame, RescanMode::Execute, Principal::Direct)
                .await
                .unwrap()
                .remove(0)
        }
    });
    tokio::time::timeout(Duration::from_secs(2), is_open)
        .await
        .unwrap()
        .unwrap();

    // Restart takes the operation lock even for an unknown module. Receive
    // that ordinary refusal while the reader is still waiting for file data.
    let frame = Frame::build(
        FrameType::Request,
        super::control_flags(),
        0,
        0,
        2,
        Vec::new(),
    )
    .unwrap();
    let restart = tokio::time::timeout(
        Duration::from_secs(1),
        handler.handle_supervisor_restart(frame, "missing".into(), None, Principal::Direct),
    )
    .await;
    let mut request = request;
    let reply = tokio::time::timeout(Duration::from_secs(6), &mut request).await;
    release.send(()).unwrap();
    feeder.join().unwrap();
    let restart = restart
        .expect("the rescan held the supervisor operation lock during its config read")
        .unwrap()
        .remove(0);
    let body: serde_json::Value = serde_json::from_slice(&restart.body).unwrap();
    assert_eq!(body["code"], "unknown_module");
    let reply = reply
        .expect("rescan did not answer before the config reader was released")
        .unwrap();
    assert_eq!(reply.header.ty, FrameType::Error);
    let body: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["code"], "rescan_config_read_timeout");
    assert!(body["message"].as_str().unwrap().contains("subc.jsonc"));
    assert!(handle.list().is_empty());
    assert_eq!(
        handler
            .rescan
            .as_ref()
            .unwrap()
            .generation
            .load(Ordering::SeqCst),
        0
    );
}

#[derive(Debug, Clone)]
pub(super) struct SupervisorRescanContext {
    pub(super) supervisor: Supervisor,
    pub(super) config_path: PathBuf,
    pub(super) configured_port: Option<u16>,
    pub(super) storage_config: Option<crate::daemon_config::StorageConfig>,
    pub(super) admission_facts_carrier_module_id: Option<String>,
    pub(super) admission_facts_targets: Option<Vec<String>>,
    pub(super) scope_authority_owners: Vec<String>,
    federated_scope_owner: String,
    pub(super) privileged_operator_prompt: PrivilegedOperatorPrompt,
    /// Counts executed rescans that set out to change the supervised modules'
    /// staged or applied configuration, counted before the first change so a
    /// rescan that fails partway still counts. With the daemon incarnation it
    /// names the state a `supervisor.rescan_checked` preview was computed
    /// against.
    generation: Arc<std::sync::atomic::AtomicU64>,
    rescan_lock: Arc<tokio::sync::Mutex<()>>,
    pub(super) module_entries: Arc<Mutex<rescan_checked::ModuleEntries>>,
}

impl ControlHandler {
    pub fn with_supervisor_rescan(
        mut self,
        supervisor: Supervisor,
        config_path: impl Into<PathBuf>,
        configured_port: Option<u16>,
    ) -> Self {
        self.rescan = Some(SupervisorRescanContext {
            supervisor,
            config_path: config_path.into(),
            configured_port,
            storage_config: self.storage_config.clone(),
            admission_facts_carrier_module_id: self.admission_facts_carrier_module_id.clone(),
            admission_facts_targets: self.admission_facts_targets.clone(),
            scope_authority_owners: self.scope_authority_owners.clone(),
            federated_scope_owner: self.federated_scope_owner.clone(),
            privileged_operator_prompt: self.privileged_operator_prompt,
            generation: Arc::default(),
            rescan_lock: Arc::default(),
            module_entries: Arc::default(),
        });
        self
    }

    pub(crate) fn with_supervisor_rescan_entries(
        self,
        modules: &[crate::daemon_config::ConfiguredModule],
    ) -> Self {
        if let Some(context) = &self.rescan {
            let mut entries = context
                .module_entries
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            entries.seed(modules);
        }
        self
    }
}

impl ControlHandler {
    /// `supervisor.rescan` and `supervisor.rescan_checked`. `mode` says where
    /// the configuration comes from and whether to apply it; every mode then
    /// runs the same comparison, so a preview describes the apply it previews.
    pub(super) async fn handle_supervisor_rescan(
        &self,
        frame: Frame,
        mode: RescanMode,
        requester: Principal,
    ) -> Result<Vec<Frame>, RouterError> {
        let preview = mode.previews();
        let Some(context) = self.rescan.clone() else {
            return Ok(vec![control_error_frame(
                &frame,
                "rescan_unavailable",
                "the daemon was not started with a reloadable config path".to_string(),
            )?]);
        };

        let _rescan_guard = context.rescan_lock.lock().await;
        let rescan_checked::RescanInput {
            config: loaded,
            approved_preview,
            guard: _operation_guard,
        } = match self.rescan_input(&mode, &context).await {
            Ok(input) => input,
            Err(error) => return Ok(vec![error.into_frame(&frame)?]),
        };
        // `load` reports a missing file as Ok(None), which is correct at boot
        // (no config, nothing to supervise) and catastrophic here: rescan treats
        // "not in the config" as "remove it", so an absent file would read as an
        // empty module list and retire the entire running fleet. An editor
        // writing via write-new-then-rename, or a half-finished edit, is enough
        // to open that window. Refuse instead: a config that cannot be read
        // carries no instruction to remove anything.
        let Some((config, activity_config)) = loaded else {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_daemon_config",
                format!(
                    "daemon config not found at {}; refusing to rescan (an absent config would \
                     retire every supervised module)",
                    context.config_path.display()
                ),
            )?]);
        };
        let (
            configured_port,
            storage_config,
            admission_facts_carrier_module_id,
            admission_facts_targets,
            scope_authority_owners,
            federated_scope_owner,
            privileged_operator_prompt,
            modules,
            reserved_capabilities,
        ) = (
            config.port,
            config.storage,
            config.admission_facts_carrier_module_id,
            config.admission_facts_targets,
            config.scope_authority_owners,
            config.federated_scope_owner,
            config.privileged_operator_prompt,
            config.modules,
            config.reserved_capabilities,
        );

        // Collect the sections rescan cannot apply, so the REPLY carries them.
        //
        // The warning below has always been correct and has always gone only to
        // the journal -- addressed to whoever reads logs, while the person who
        // just edited the config is looking at the CLI. Naming each section
        // individually rather than setting a flag: "something outside modules
        // changed" sends the operator back to diffing their own file, which is
        // the work this is meant to save.
        let mut restart_required = Vec::new();
        for section in RestartRequiredSection::ALL {
            let changed = match section {
                RestartRequiredSection::Port => configured_port != context.configured_port,
                RestartRequiredSection::Storage => storage_config != context.storage_config,
                RestartRequiredSection::AdmissionFactsCarrierModuleId => {
                    admission_facts_carrier_module_id != context.admission_facts_carrier_module_id
                }
                RestartRequiredSection::AdmissionFactsTargets => {
                    admission_facts_targets != context.admission_facts_targets
                }
                RestartRequiredSection::ScopeAuthorityOwners => {
                    scope_authority_owners != context.scope_authority_owners
                }
                RestartRequiredSection::FederatedScopeOwner => {
                    federated_scope_owner != context.federated_scope_owner
                }
                RestartRequiredSection::PrivilegedOperatorPrompt => {
                    privileged_operator_prompt != context.privileged_operator_prompt
                }
            };
            if changed {
                restart_required.push(section.label().to_string());
            }
        }
        if !restart_required.is_empty() {
            warn!(
                config_path = %context.config_path.display(),
                sections = %restart_required.join(", "),
                "daemon config changed outside the modules section; restart the daemon to apply those changes"
            );
        }

        for configured in &modules {
            if let Err(err) = validate_spec(&configured.module_spec()) {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_daemon_config",
                    format!("supervisor rescan rejected daemon config: {err}"),
                )?]);
            }
        }

        let configured_capabilities = modules
            .iter()
            .map(|module| (module.module_id.clone(), module.enabled))
            .collect::<Vec<_>>();
        let preview_capability_warnings = if preview {
            let (_, registrations) = self.runtime_capability_snapshot()?;
            let current_modules = self
                .supervisor
                .list()
                .into_iter()
                .map(|module| module.module_id().to_string())
                .collect::<BTreeSet<_>>();
            let resulting_modules = configured_capabilities.clone();
            let removed = current_modules
                .into_iter()
                .filter(|module_id| {
                    !resulting_modules
                        .iter()
                        .any(|(configured_id, _)| configured_id == module_id)
                })
                .collect::<Vec<_>>();
            // A candidate preview also names providers the candidate disables.
            // `supervisor.rescan`'s own preview keeps its removal-only warnings.
            let disabled = match &mode {
                RescanMode::Candidate(_) => resulting_modules
                    .iter()
                    .filter(|(_, enabled)| !enabled)
                    .map(|(module_id, _)| module_id.clone())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            self.capability_evaluator.preview_warnings(
                resulting_modules,
                &removed,
                &disabled,
                &registrations,
            )
        } else {
            Vec::new()
        };
        let reconciled = match (&mode, approved_preview.as_ref()) {
            (RescanMode::Preview, Some(approved)) => Ok((
                config_gate::preview_approved_daemon(approved, &modules),
                Vec::new(),
            )),
            (RescanMode::Candidate(candidate), _) => self
                .preview_candidate(&context, modules, candidate)
                .map(|result| (result, Vec::new())),
            _ => {
                self.reconcile_supervised_modules(
                    &context.supervisor,
                    modules,
                    preview,
                    requester.clone(),
                )
                .await
            }
        };
        let (result, changed) = match reconciled {
            Ok(result) => result,
            Err(message) => {
                return Ok(vec![control_error_frame(&frame, "rescan_failed", message)?])
            }
        };
        if !preview {
            self.rescan_policy_root(config.agent_policy_root).await;
            if let Some(ledger) = &self.activity {
                ledger.module_rescanned(&result.added, &result.removed, requester.clone());
                ledger.config_changed(&result.added, &result.removed, &changed, requester);
            }
            if let Err(code) = self.configure_activity(activity_config).await {
                warn!(
                    code,
                    "activity retention could not be applied during rescan"
                );
            }
            self.capability_evaluator
                .configure(configured_capabilities, reserved_capabilities);
            self.capability_evaluator.wake_deadline_loop();
            self.refresh_capability_requirements();
        }
        let mut result = result;
        result.restart_required = restart_required;
        result.capability_warnings = preview_capability_warnings;
        let response = ClientControlResponse::SupervisorRescan { result };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorRescan",
        )?])
    }
}

impl ControlHandler {
    /// Compare the configured module set with the supervised one, changing
    /// nothing. The staged spec each supervised module holds is what the
    /// configured entry is compared with.
    fn plan_rescan(
        &self,
        configured_modules: Vec<crate::daemon_config::ConfiguredModule>,
    ) -> Result<RescanPlan, String> {
        let mut current = BTreeMap::new();
        for module in self.supervisor.list() {
            let (spec, health) = module.configuration().map_err(|err| {
                format!(
                    "failed to read configuration for module_id '{}': {err}",
                    module.module_id()
                )
            })?;
            let enabled = module
                .status()
                .map_err(|err| {
                    format!(
                        "failed to read status for module_id '{}': {err}",
                        module.module_id()
                    )
                })?
                .enabled;
            current.insert(
                module.module_id().to_string(),
                (module, spec, health, enabled),
            );
        }
        let configured = configured_modules
            .into_iter()
            .map(|module| (module.module_id.clone(), module))
            .collect::<BTreeMap<_, _>>();

        let added = configured
            .keys()
            .filter(|module_id| !current.contains_key(*module_id))
            .cloned()
            .collect::<Vec<_>>();
        let removed = current
            .keys()
            .filter(|module_id| !configured.contains_key(*module_id))
            .cloned()
            .collect::<Vec<_>>();
        let mut changed_pending_reload = Vec::new();
        let mut changed_pending = Vec::new();
        let mut configuration_changes = BTreeSet::new();
        let mut enabled_changes = BTreeSet::new();
        let mut unchanged = 0_u32;
        let publication = self
            .config_runtime
            .as_ref()
            .and_then(|owner| owner.polled_snapshot().ok());

        for (module_id, configured_module) in &configured {
            let Some((module, current_spec, current_health, current_enabled)) =
                current.get(module_id)
            else {
                continue;
            };
            // Compare the whole launch spec so a future launch field cannot
            // accidentally become a live-only policy change. Health is stored
            // separately and applies live without replacing the process.
            let launch_changed = *current_spec != configured_module.module_spec();
            let configuration_changed =
                launch_changed || *current_health != configured_module.health;
            let pending = self.pending_module_fields(
                module,
                Some(configured_module),
                configuration_changed,
                Self::registration_channel_in_force(
                    publication
                        .as_ref()
                        .and_then(|view| view.registration_declaration(module_id))
                        .as_deref(),
                    publication
                        .as_ref()
                        .and_then(|view| match &view.approved_daemon {
                            crate::config_gate::runtime::ApprovedDaemonView::Approved {
                                config,
                                ..
                            } => config
                                .modules
                                .iter()
                                .find(|entry| entry.module_id == *module_id)
                                .map(|entry| entry.registration_channel),
                            _ => None,
                        }),
                ),
                publication.as_deref(),
            )?;
            let has_pending = !pending.is_empty();
            if has_pending {
                changed_pending.push(subc_control::SupervisorPendingModule {
                    module_id: module_id.clone(),
                    fields: pending
                        .into_iter()
                        .map(|mut field| {
                            field.value = None;
                            field
                        })
                        .collect(),
                });
            }
            // Applying a rescan also replaces the recorded config value, even
            // when an operator already put the runtime in the requested state.
            let enabled_changed = *current_enabled != configured_module.enabled
                || self
                    .capability_evaluator
                    .configured_enabled(module_id)
                    .is_some_and(|enabled| enabled != configured_module.enabled);
            if configuration_changed {
                configuration_changes.insert(module_id.clone());
            }
            if launch_changed {
                changed_pending_reload.push(module_id.clone());
            }
            if enabled_changed {
                enabled_changes.insert(module_id.clone());
            }
            if !configuration_changed && !enabled_changed && !has_pending {
                unchanged = unchanged.saturating_add(1);
            }
        }
        Ok(RescanPlan {
            current,
            configured,
            added,
            removed,
            changed_pending_reload,
            changed_pending,
            configuration_changes,
            enabled_changes,
            unchanged,
        })
    }

    /// Reconcile the running module set against the configured one.
    ///
    /// With `preview` set, the diff is computed and returned WITHOUT applying any
    /// of it: nothing is retired, reconfigured, enabled or spawned. The preview
    /// deliberately shares this function with the executing path rather than
    /// computing the same diff somewhere else -- two implementations of one
    /// decision agree until they do not, and the whole value of a preview is that
    /// it describes the operation that will actually run.
    pub(super) async fn reconcile_supervised_modules(
        &self,
        supervisor: &Supervisor,
        configured_modules: Vec<crate::daemon_config::ConfiguredModule>,
        preview: bool,
        requester: Principal,
    ) -> Result<(SupervisorRescanResult, Vec<String>), String> {
        let plan = self.plan_rescan(configured_modules)?;

        // Everything above this point is pure computation over two snapshots.
        // Everything below MUTATES. The preview returns here so the boundary is a
        // single early return rather than a condition repeated at each mutation
        // site, where one missed guard would apply part of a change the caller was
        // told would not happen.
        if preview {
            return Ok((plan.preview_result(), Vec::new()));
        }
        let changed = plan
            .configuration_changes
            .union(&plan.enabled_changes)
            .cloned()
            .collect::<Vec<_>>();
        let RescanPlan {
            current,
            configured,
            added,
            removed,
            changed_pending_reload,
            changed_pending,
            configuration_changes,
            enabled_changes,
            unchanged,
        } = plan;
        // The staged generation moves before the first change rather than
        // after the last, so a rescan that fails partway still invalidates
        // every earlier `supervisor.rescan_checked` preview: part of its diff
        // may have run.
        if !(added.is_empty()
            && removed.is_empty()
            && configuration_changes.is_empty()
            && changed_pending.is_empty()
            && enabled_changes.is_empty())
        {
            if let Some(context) = &self.rescan {
                context
                    .generation
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        for module_id in &removed {
            let module = &current
                .get(module_id)
                .expect("removed module came from current supervisor state")
                .0;
            module
                .retire_requested(Some(requester.clone()))
                .await
                .map_err(|err| {
                    format!("failed to retire module_id '{module_id}' during rescan: {err}")
                })?;
            // TOMBSTONE BEFORE RETIRE, and the order is the whole fix.
            //
            // `handle_route_open` resolves an absent module in three steps:
            // registry, then supervisor status, then tombstone. Retiring first
            // opens a window where ALL THREE ARE ABSENT -- the registry entry
            // went with the teardown above, the supervisor entry went with
            // `retire`, and the tombstone does not exist yet -- so a route.open
            // landing in it gets `unknown_module` (RETRYABLE, "never heard of
            // it") for a module that was deliberately removed and whose caller
            // should get `module_removed` (TERMINAL, carrying a removal age).
            //
            // Writing the tombstone first closes it: during the window the
            // supervisor entry still answers, so the caller gets
            // `target_unavailable` -- retryable, and TRUE, because the module
            // is mid-teardown. After both statements it is `module_removed`.
            // No instant remains where a removed module reads as one that
            // never existed.
            //
            // NOT DETERMINISTICALLY TESTABLE FROM HERE, said plainly because
            // the absence of a test beside a fix invites deletion: these are
            // two sync statements with no await between them, so reaching the
            // window needs a second worker thread to land exactly between them
            // and there is no hook to force it. MEASURED: the 25 daemon_config
            // tests pass identically with the old order and the new one, so
            // the existing suite cannot see this and a green run is not
            // evidence either way. What the suite does hold is the
            // post-condition -- a removed module answers `module_removed` --
            // which this preserves.
            //
            // Found by an Athena panel reading the shipped tree against a
            // design note (2026-09-19), as the one concrete instance of that
            // note's class that survived contact with source. Direction is
            // benign: retryable where terminal was intended, never the reverse.
            self.supervisor.record_rescan_removal(module_id);
            self.supervisor.retire(module_id);
            self.route_outages.forget(module_id);
        }

        for module_id in configured.keys() {
            let Some((module, _, _, current_enabled)) = current.get(module_id) else {
                continue;
            };
            let configured_module = configured
                .get(module_id)
                .expect("configured module id came from configured map");
            if configuration_changes.contains(module_id) {
                module
                    .update_configuration(
                        configured_module.module_spec(),
                        configured_module.health.clone(),
                        configured_module.drain_timeout_ms,
                    )
                    .await
                    .map_err(|err| {
                        format!(
                            "failed to update module_id '{module_id}' configuration during rescan: {err}"
                        )
                    })?;
            }
            // Config-only drift is applied by the evaluator's config update;
            // do not turn it into a runtime start or stop.
            if *current_enabled != configured_module.enabled {
                // A rescan that starts or stops a module applies an operator's
                // edit to the config, so the resulting outage was asked for.
                self.route_outages.mark_operator_action(module_id);
                module
                    .set_enabled_requested(configured_module.enabled, Some(requester.clone()))
                    .await
                    .map_err(|err| {
                        self.route_outages.operator_action_ended_unrefused(module_id);
                        format!(
                            "failed to apply module_id '{module_id}' enabled={} during rescan: {err}",
                            configured_module.enabled
                        )
                    })?;
            }
        }

        for module_id in &added {
            let configured_module = configured
                .get(module_id)
                .expect("added module id came from configured map");
            supervisor
                .supervise_configured_requested_async(
                    configured_module.module_spec(),
                    configured_module.enabled,
                    configured_module.health.clone(),
                    configured_module.drain_timeout_ms,
                    configured_module.restart,
                    configured_module.register_timeout,
                    Some(requester.clone()),
                )
                .await
                .map_err(|err| {
                    format!("failed to add module_id '{module_id}' during rescan: {err}")
                })?;
        }

        if let Some(context) = &self.rescan {
            let mut entries = context
                .module_entries
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            entries.record(&configured, &added, &removed);
        }

        Ok((
            SupervisorRescanResult {
                added,
                removed,
                changed_pending_reload,
                changed_pending,
                enabled_changes: enabled_changes.iter().cloned().collect(),
                unchanged,
                preview: false,
                // Filled by the caller, which is the only layer that can see the
                // previous config to diff against.
                restart_required: Vec::new(),
                capability_warnings: Vec::new(),
                module_actions: Vec::new(),
                earlier_pending: Vec::new(),
                expect: None,
            },
            changed,
        ))
    }
}
