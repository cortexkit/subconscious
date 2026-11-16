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

use super::{control_error_frame, launch_spec_agreement, ControlHandler, SupervisorRescanContext};
use crate::{
    daemon_config::{ActivityConfig, ConfiguredModule, DaemonConfig},
    router::{RouteCtx, RouterError},
    supervise::{ModuleSpec, SupervisedModule},
    Frame, HealthConfig,
};

#[cfg(test)]
mod tests;

/// Where a rescan's configuration comes from, and whether it is applied.
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
        Ok(entry_pending_fields(&current, &next, applies_configuration))
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
        if super::config_gate::CONFIG_SETTINGS_SURFACE_ENABLED && self.config_runtime.is_some() {
            capabilities.push(subc_control::config::CONFIG_SETTINGS_CAPABILITY.to_owned());
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

    /// The configuration a rescan in `mode` works from. Called with the
    /// supervisor operation lock held. Errors are a refusal code and message.
    pub(super) fn load_rescan_config(
        &self,
        mode: &RescanMode,
        context: &SupervisorRescanContext,
    ) -> Result<Option<(DaemonConfig, ActivityConfig)>, (&'static str, String)> {
        let rejected = |err: crate::daemon_config::DaemonConfigError| {
            (
                "invalid_daemon_config",
                format!("supervisor rescan rejected daemon config: {err}"),
            )
        };
        match mode {
            RescanMode::Execute | RescanMode::Preview => {
                crate::daemon_config::load_with_activity(&context.config_path).map_err(rejected)
            }
            RescanMode::Candidate(candidate) => {
                crate::daemon_config::parse_with_activity(candidate, &context.config_path)
                    .map(Some)
                    .map_err(rejected)
            }
            RescanMode::Checked(expect) => {
                let doc = self.checked_config_doc(context, expect)?;
                crate::daemon_config::parse_with_activity(&doc, &context.config_path)
                    .map(Some)
                    .map_err(rejected)
            }
        }
    }

    /// The config file's text, provided it is the text the preview hashed and
    /// the staged configuration is the one the preview compared it with. The
    /// file is read once and those same bytes are parsed, so a write landing
    /// after the hash check cannot be what gets applied.
    fn checked_config_doc(
        &self,
        context: &SupervisorRescanContext,
        expect: &RescanExpect,
    ) -> Result<String, (&'static str, String)> {
        let path = context.config_path.display();
        let bytes = match std::fs::read(&context.config_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err((
                    CONFIG_CHANGED_SINCE_PREVIEW,
                    format!("daemon config not found at {path}; nothing was applied"),
                ));
            }
            Err(err) => {
                return Err((
                    "invalid_daemon_config",
                    format!("supervisor rescan could not read daemon config at {path}: {err}"),
                ));
            }
        };
        if !sha256_hex(&bytes).eq_ignore_ascii_case(&expect.config_sha256) {
            return Err((
                CONFIG_CHANGED_SINCE_PREVIEW,
                format!(
                    "daemon config at {path} is not the text that was previewed; nothing was applied"
                ),
            ));
        }
        let current = self.staged_generation(context);
        if current != expect.staged_generation {
            return Err((
                CONFIG_CHANGED_SINCE_PREVIEW,
                format!(
                    "the daemon's staged configuration changed after the preview \
                     (previewed {}:{}, now {}:{}); nothing was applied",
                    expect.staged_generation.incarnation,
                    expect.staged_generation.counter,
                    current.incarnation,
                    current.counter
                ),
            ));
        }
        // The hash matched text that arrived as a JSON string, so this only
        // fails on a hash collision; refuse rather than assume.
        String::from_utf8(bytes).map_err(|err| {
            (
                "invalid_daemon_config",
                format!("daemon config at {path} is not UTF-8: {err}"),
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
