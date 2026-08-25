use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant as StdInstant},
};

use serde::{Deserialize, Serialize};
use subc_control::{
    ops, CapabilityRequirementStatus, CatalogEntry, ClientControlPush, ClientControlRequest,
    ClientControlResponse, ConsumerIdentity, DaemonBuildProvenance, DaemonObservedProcess,
    ModuleDeclaredProvenance, PollKind, RouteCloseReason, StderrCaptureState, StderrTail,
    StderrTailEntry, SupervisorDaemonProvenance, SupervisorEntry, SupervisorHealthEntry,
    SupervisorModuleProvenance, SupervisorObservedProcess, SupervisorRescanResult, SupervisorRoute,
    SupervisorRouteConsumer, SupervisorRouteModule, TerminalEntry, TerminalHistory,
};
use subc_protocol::{
    error_codes,
    manifest::{
        validate_hello_capability_grammar, CapabilityDeclarations, Concurrency, ManifestProvenance,
        ModuleManifest, ProviderRole,
    },
    session::{
        HealthReport, ModuleControlPush, ModuleControlRequest, ModuleControlRequestFromModule,
        ModuleControlResponse, ModuleControlResponseToModule, MODULE_CONTROL_OP_HEALTH_CHECK,
        MODULE_TO_SUBC_OP_CATALOG_UPDATE,
    },
    BindIdentity, ErrorBody, Flags, FrameType, ModuleHelloAckBody, ModuleHelloBody, Principal,
    Priority, RouteTarget, PROTOCOL_VERSION,
};
use tokio::time::{timeout_at, Instant};
use tracing::{debug, info, warn};

use crate::{
    capability_requirements::{
        log_duplicate_claim_events, log_requirement_events, CapabilityRequirementEvaluator,
        DuplicateClaimSource, RegisteredModule, RequirementStatus, RuntimeModule,
    },
    forwarding::{
        CloseReason, EndpointRoute, ForwardingError, ForwardingTable, GoodbyeTarget,
        ModuleControlRpcCompletion, ModuleControlRpcOutcome, ModuleEndpointId,
        PendingModuleControlRpc, RouteBindRelayOutcome, RoutePollSnapshot, RouteRelease,
    },
    provenance::{spawned_file_identity, ExecutableIdentityProbe, SpawnedFileIdentity},
    registry::{ChannelState, ConnectionId, Registry, RegistryError},
    router::{RouteCtx, RouterError},
    stderr_tail::{CaptureState, TailEntry},
    supervise::{validate_spec, ModuleProcessLiveness, ReservedHelloRejection, SupervisorHandle},
    ConnectedClients, DaemonCounters, Frame, ProjectRootId, Supervisor,
};

/// Lowest envelope version this subc build will negotiate.
///
/// Module HELLO negotiation is exact: peers must use the daemon's locked
/// protocol version. Older and newer peers receive `version_unsupported` and
/// are not registered.
pub const MIN_SUPPORTED_VERSION: u8 = PROTOCOL_VERSION;

const CAP_MANIFEST_REGISTRATION: &str = "manifest_registration_v1";
const CAP_CHANNEL_LIFECYCLE: &str = "channel_lifecycle_v1";
const CAP_PING_PONG: &str = "ping_pong_v1";
const CAP_SESSION_ATTACH: &str = "session_attach_v1";
const CAP_ADMISSION_FACTS_RELAY: &str = "admission_facts_relay_v1";

const SUBC_CONTROL_OPS: &[&str] = &[
    ops::SERVER_DESCRIBE,
    ops::CATALOG_LIST,
    ops::ROUTE_OPEN,
    ops::ROUTE_POLL,
    ops::ROUTE_CLOSING,
    ops::ROUTE_CLOSED,
    ops::SUPERVISOR_LIST,
    ops::SUPERVISOR_RESTART,
    ops::SUPERVISOR_RELOAD,
    ops::SUPERVISOR_RESCAN,
    ops::SUPERVISOR_RELEASE_RESERVED,
    ops::SUPERVISOR_SET_ENABLED,
    ops::SUPERVISOR_HEALTH_PROBE,
    ops::SUPERVISOR_HEALTH,
    ops::SUPERVISOR_STDERR_TAIL,
    ops::SUPERVISOR_TERMINALS,
    ops::SUPERVISOR_ROUTES,
    ops::SUPERVISOR_PROVENANCE,
];

const MODULE_TO_SUBC_CONTROL_OPS: &[&str] = &[MODULE_TO_SUBC_OP_CATALOG_UPDATE];

const MODULE_BASELINE_CONTROL_OPS: &[&str] = &["route.bind", "route.status"];

/// How long subc waits for a module to ack a relayed route.bind before returning
/// `module_timeout`. The ack waits on the module's own configure, which for AFT
/// includes a synchronous bounded project walk (up to ~20k files) plus gitignore
/// and DB-open work — on a cold page cache or a large repo that legitimately
/// exceeds a couple of seconds. The default is generous because rejecting a VALID
/// bind is far worse than waiting on a slow one; a consumer that wants a tighter
/// bound retries the bind itself (the sanctioned warm-bind-retry pattern).
const DEFAULT_ROUTE_BIND_RELAY_TIMEOUT: Duration = Duration::from_secs(12);
const DEFAULT_HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const SLOW_CONTROL_DISPATCH_THRESHOLD: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct DaemonProvenanceFacts {
    build: DaemonBuildProvenance,
    pid: Option<u32>,
    started_at_ms: Option<u64>,
    executable_path: Option<PathBuf>,
    executable_identity: Option<SpawnedFileIdentity>,
    probe: ExecutableIdentityProbe,
}

impl Default for DaemonProvenanceFacts {
    fn default() -> Self {
        Self {
            build: DaemonBuildProvenance {
                build_git_sha: None,
                build_lock_digest: None,
            },
            pid: None,
            started_at_ms: None,
            executable_path: None,
            executable_identity: None,
            probe: ExecutableIdentityProbe::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct SupervisorRescanContext {
    supervisor: Supervisor,
    config_path: PathBuf,
    configured_port: Option<u16>,
    storage_config: Option<crate::daemon_config::StorageConfig>,
    admission_facts_carrier_module_id: Option<String>,
    admission_facts_targets: Option<Vec<String>>,
}

/// Real channel-0 control handler for subc itself.
#[derive(Clone)]
pub struct ControlHandler {
    registry: Arc<Registry>,
    forwarding: Arc<ForwardingTable>,
    process_liveness: Option<Arc<dyn ModuleProcessLiveness>>,
    supervisor: SupervisorHandle,
    subc_capabilities: Arc<[String]>,
    /// Daemon-wide route.bind relay budget. Used as the fallback when the
    /// target module has no per-module override in
    /// `route_bind_relay_timeouts`.
    route_bind_relay_timeout: Duration,
    /// Per-module route.bind relay budget overrides, keyed by module id. When
    /// `handle_route_open` resolves the deadline for a target module, a
    /// per-module entry wins over the daemon-wide value above.
    route_bind_relay_timeouts: BTreeMap<String, Duration>,
    health_probe_timeout: Duration,
    /// Central storage policy. When set, each registering module receives its
    /// resolved storage descriptor in HELLO_ACK; `None` leaves the field absent.
    storage_config: Option<crate::daemon_config::StorageConfig>,
    admission_facts_carrier_module_id: Option<String>,
    admission_facts_targets: Option<Vec<String>>,
    rescan: Option<SupervisorRescanContext>,
    connected_clients: ConnectedClients,
    counters: DaemonCounters,
    capability_evaluator: Arc<CapabilityRequirementEvaluator>,
    daemon_provenance: DaemonProvenanceFacts,
    #[cfg(test)]
    control_dispatch_delay: Option<Duration>,
    #[cfg(test)]
    provenance_probe_override: Option<subc_control::RunningImageAgreement>,
}

impl fmt::Debug for ControlHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlHandler")
            .field("registry", &self.registry)
            .field("forwarding", &self.forwarding)
            .field("process_liveness", &self.process_liveness.is_some())
            .field("supervisor", &self.supervisor)
            .field("subc_capabilities", &self.subc_capabilities)
            .finish()
    }
}

struct RouteOpenRequest {
    target: RouteTarget,
    identity: BindIdentity,
    consumer_identity: Option<ConsumerIdentity>,
    consumer_capabilities: Option<Vec<String>>,
    admission_facts: Option<serde_json::Value>,
}

struct RouteBindReservationGuard {
    forwarding: Arc<ForwardingTable>,
    endpoint: ModuleEndpointId,
    relay_corr: u64,
    armed: bool,
}

struct ModuleControlRpcGuard {
    forwarding: Arc<ForwardingTable>,
    endpoint: ModuleEndpointId,
    corr: u64,
    armed: bool,
}

impl ModuleControlRpcGuard {
    fn new(forwarding: Arc<ForwardingTable>, endpoint: ModuleEndpointId, corr: u64) -> Self {
        Self {
            forwarding,
            endpoint,
            corr,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ModuleControlRpcGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .forwarding
                .cancel_module_control_rpc(self.endpoint, self.corr);
        }
    }
}

impl RouteBindReservationGuard {
    fn new(forwarding: Arc<ForwardingTable>, endpoint: ModuleEndpointId, relay_corr: u64) -> Self {
        Self {
            forwarding,
            endpoint,
            relay_corr,
            armed: true,
        }
    }

    fn release_and_disarm(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(Some(target)) = self.forwarding.abort_pending_relay(
            self.endpoint,
            self.relay_corr,
            RouteBindRelayOutcome::ModuleGone("route.open handler canceled".to_string()),
        ) {
            send_goodbye_target_best_effort(&target, "canceled route.bind");
        }
        self.armed = false;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RouteBindReservationGuard {
    fn drop(&mut self) {
        self.release_and_disarm();
    }
}

impl ControlHandler {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self::with_forwarding(registry, Arc::new(ForwardingTable::default()))
    }

    pub fn with_forwarding(registry: Arc<Registry>, forwarding: Arc<ForwardingTable>) -> Self {
        let counters = forwarding.counters();
        Self {
            registry,
            forwarding,
            process_liveness: None,
            supervisor: SupervisorHandle::new(),
            subc_capabilities: Arc::from([
                CAP_MANIFEST_REGISTRATION.to_string(),
                CAP_CHANNEL_LIFECYCLE.to_string(),
                CAP_PING_PONG.to_string(),
                CAP_SESSION_ATTACH.to_string(),
                CAP_ADMISSION_FACTS_RELAY.to_string(),
            ]),
            route_bind_relay_timeout: DEFAULT_ROUTE_BIND_RELAY_TIMEOUT,
            route_bind_relay_timeouts: BTreeMap::new(),
            health_probe_timeout: DEFAULT_HEALTH_PROBE_TIMEOUT,
            storage_config: None,
            admission_facts_carrier_module_id: None,
            admission_facts_targets: None,
            rescan: None,
            connected_clients: ConnectedClients::new(),
            counters,
            capability_evaluator: Arc::new(CapabilityRequirementEvaluator::new()),
            daemon_provenance: DaemonProvenanceFacts::default(),
            #[cfg(test)]
            control_dispatch_delay: None,
            #[cfg(test)]
            provenance_probe_override: None,
        }
    }

    /// Set the central storage policy: registering modules then receive their
    /// resolved storage descriptor in HELLO_ACK.
    pub fn with_storage_config(
        mut self,
        storage_config: Option<crate::daemon_config::StorageConfig>,
    ) -> Self {
        self.storage_config = storage_config;
        self
    }

    /// Configure the exact reserved module and target ids permitted to relay
    /// opaque admission facts. Config-file loading validates this authority;
    /// this builder keeps the same policy available to embedded test daemons.
    pub fn with_admission_facts_config(
        mut self,
        carrier_module_id: Option<String>,
        targets: Option<Vec<String>>,
    ) -> Self {
        self.admission_facts_carrier_module_id = carrier_module_id;
        self.admission_facts_targets = targets;
        self
    }

    /// Override the route.bind relay timeout. Used by tests that assert the
    /// timeout path so they don't block on the production-safe default.
    pub fn with_route_bind_relay_timeout(mut self, timeout: Duration) -> Self {
        self.route_bind_relay_timeout = timeout;
        self
    }

    /// Install per-module route.bind relay budget overrides. A module id
    /// listed here wins over the daemon-wide default set via
    /// `with_route_bind_relay_timeout`. Values are pre-resolved at parse time
    /// from `subc.jsonc` (per-module > daemon-wide > absent), so callers pass
    /// the same `Duration` the bind path will use.
    pub fn with_route_bind_relay_timeouts(
        mut self,
        timeouts: impl IntoIterator<Item = (String, Duration)>,
    ) -> Self {
        self.route_bind_relay_timeouts = timeouts.into_iter().collect();
        self
    }

    /// Resolve the route.bind relay budget for a specific target module id.
    /// Per-module overrides win; the daemon-wide value (set via
    /// `with_route_bind_relay_timeout` or the built-in default) is the
    /// fallback. Exposed so config-aware callers (bootstrap, tests) can audit
    /// the same resolution `handle_route_open` will use.
    pub fn route_bind_relay_timeout_for(&self, module_id: &str) -> Duration {
        self.route_bind_relay_timeouts
            .get(module_id)
            .copied()
            .unwrap_or(self.route_bind_relay_timeout)
    }

    #[cfg(test)]
    pub(crate) fn with_health_probe_timeout(mut self, timeout: Duration) -> Self {
        self.health_probe_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_control_dispatch_delay(mut self, delay: Duration) -> Self {
        self.control_dispatch_delay = Some(delay);
        self
    }

    pub fn with_process_liveness(
        mut self,
        process_liveness: Arc<dyn ModuleProcessLiveness>,
    ) -> Self {
        self.process_liveness = Some(process_liveness);
        self
    }

    pub fn with_supervisor(mut self, supervisor: SupervisorHandle) -> Self {
        self.supervisor = supervisor;
        self
    }

    pub fn with_daemon_provenance(
        mut self,
        pid: u32,
        started_at_ms: u64,
        executable_path: Option<PathBuf>,
        build_git_sha: Option<String>,
        build_lock_digest: Option<String>,
    ) -> Self {
        let executable_identity = executable_path.as_deref().and_then(spawned_file_identity);
        self.daemon_provenance = DaemonProvenanceFacts {
            build: DaemonBuildProvenance {
                build_git_sha,
                build_lock_digest,
            },
            pid: Some(pid),
            started_at_ms: Some(started_at_ms),
            executable_path,
            executable_identity,
            probe: ExecutableIdentityProbe::default(),
        };
        self
    }

    #[cfg(test)]
    fn with_provenance_probe_result(mut self, result: subc_control::RunningImageAgreement) -> Self {
        self.provenance_probe_override = Some(result);
        self
    }

    /// Install the configured module set and its reserved capability bindings.
    /// Bindings are configuration-scoped and may point at a provider that has not
    /// been installed yet, so this does not require the bound module to exist.
    pub fn with_capability_config(
        self,
        modules: impl IntoIterator<Item = (String, bool)>,
        reserved_capabilities: BTreeMap<String, String>,
    ) -> Self {
        self.capability_evaluator
            .configure(modules, reserved_capabilities);
        self
    }

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
        });
        self
    }

    pub fn with_connected_clients(mut self, connected_clients: ConnectedClients) -> Self {
        self.connected_clients = connected_clients;
        self
    }

    pub fn forwarding(&self) -> Arc<ForwardingTable> {
        Arc::clone(&self.forwarding)
    }

    pub(crate) fn counters(&self) -> DaemonCounters {
        self.counters.clone()
    }

    /// Wake at each candidate's own deadline so a stalled fresh exec emits its
    /// requirement event without depending on an operator polling a status command.
    pub fn spawn_capability_deadline_loop(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                self.capability_evaluator
                    .wait_for_change_or_deadline()
                    .await;
                self.refresh_capability_requirements();
            }
        });
    }

    fn runtime_capability_snapshot(
        &self,
    ) -> Result<(Vec<RuntimeModule>, Vec<RegisteredModule>), RouterError> {
        let runtime = self
            .supervisor
            .list()
            .into_iter()
            .map(|module| {
                let status = module.status().map_err(|err| {
                    RouterError::backend(0, 0, format!("failed to read capability status: {err}"))
                })?;
                Ok(RuntimeModule {
                    module_id: status.module_id,
                    state: status.state,
                    enabled: status.enabled,
                })
            })
            .collect::<Result<Vec<_>, RouterError>>()?;
        let (_, registrations) = self.registry.list_modules().map_err(|err| {
            RouterError::backend(
                0,
                0,
                format!("failed to list capability registrations: {err}"),
            )
        })?;
        let registrations = registrations
            .into_iter()
            .map(|registration| RegisteredModule {
                module_id: registration.manifest.module_id,
                module_version: registration.manifest.module_version,
                capabilities: registration.manifest.capabilities,
            })
            .collect();
        Ok((runtime, registrations))
    }

    pub fn refresh_capability_requirements(&self) {
        match self.runtime_capability_snapshot() {
            Ok((runtime, registrations)) => {
                log_requirement_events(
                    self.capability_evaluator
                        .evaluate_now(&runtime, &registrations),
                );
            }
            Err(err) => warn!(error = %err, "failed to recompute capability requirements"),
        }
    }

    /// Reconcile only live, attested route bindings after a capability deny edge
    /// or target claim was added. This is deliberately a control-plane census:
    /// the opaque forwarding hot path must not grow a per-frame capability check.
    fn enforce_capability_denies(&self) {
        let (_, registrations) = match self.registry.list_modules() {
            Ok(snapshot) => snapshot,
            Err(err) => {
                warn!(error = %err, "failed to read registrations for capability deny census");
                return;
            }
        };
        let manifests = registrations
            .into_iter()
            .map(|registration| {
                (
                    registration.manifest.module_id.clone(),
                    registration.manifest,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let census = match self.forwarding.route_census(None) {
            Ok(census) => census,
            Err(err) => {
                warn!(error = %err, "failed to read route census for capability deny enforcement");
                return;
            }
        };

        for (target_module_id, routes) in census {
            let Some(target_manifest) = manifests.get(&target_module_id) else {
                continue;
            };
            let mut closed_routes = Vec::new();
            let mut module_goodbyes = Vec::new();
            for route in routes {
                let Principal::Reserved {
                    module_id: opening_module_id,
                } = &route.principal
                else {
                    continue;
                };
                let Some(opening_manifest) = manifests.get(opening_module_id) else {
                    continue;
                };
                let Some(capability) = denied_capability(opening_manifest, target_manifest) else {
                    continue;
                };

                match self.forwarding.release_client_route(
                    route.goodbye_target.connection_id,
                    route.goodbye_target.channel,
                    route.goodbye_target.epoch,
                ) {
                    Ok(RouteRelease::Removed(module_goodbye)) => {
                        warn!(
                            opening_module_id,
                            target_module_id,
                            capability,
                            "force-closing route because an attested capability deny edge now matches"
                        );
                        closed_routes.push(route);
                        module_goodbyes.push(module_goodbye);
                    }
                    Ok(RouteRelease::Stale | RouteRelease::Absent) => {}
                    Err(err) => warn!(
                        opening_module_id,
                        target_module_id,
                        capability,
                        error = %err,
                        "failed to force-close capability-denied route"
                    ),
                }
            }

            if closed_routes.is_empty() {
                continue;
            }
            send_route_control_pushes(
                &self.forwarding,
                closed_routes,
                ClientControlPush::RouteClosed {
                    module_id: target_module_id,
                    reason: RouteCloseReason::CapabilityDenied,
                    drained: false,
                    abandoned: 0,
                    terminal: Some(false),
                },
            );
            self.emit_route_goodbyes(module_goodbyes);
        }
    }

    fn capability_requirement_statuses(&self) -> Vec<CapabilityRequirementStatus> {
        self.capability_evaluator
            .statuses()
            .into_iter()
            .map(capability_requirement_status)
            .collect()
    }

    /// Remove a connection's registry entries WITHOUT signalling the supervisor's
    /// registration-release watch. The signal is what the supervisor waits on
    /// before spawning a replacement, so it must only fire once forwarding
    /// teardown is also done (see [`Self::cleanup_connection`] /
    /// [`Self::handle_goodbye`]). Used directly only where there is no forwarding
    /// state to tear down (a HELLO that failed before module registration).
    fn deregister_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Vec<crate::registry::ModuleRegistration>, RegistryError> {
        self.registry.deregister_connection(connection_id)
    }

    /// Test-only compatibility entry point for unit control handling that does not have a socket sink.
    ///
    /// The real server path uses [`Self::handle_control_frame`] so module HELLO registration can
    /// record the module connection's [`crate::FrameSink`] and session attach can await the module
    /// relay response. This seam stays cfg(test) so production has only one channel-0 path.
    #[cfg(test)]
    pub fn handle_control(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        match frame.header.ty {
            FrameType::Ping => Ok(vec![pong(&frame)?]),
            FrameType::Hello => self.handle_hello(connection_id, None, frame),
            FrameType::Goodbye => self.handle_goodbye(connection_id),
            ty => Ok(vec![control_error_frame(
                &frame,
                "unsupported_control_frame",
                format!("unsupported channel-0 frame {ty:?}"),
            )?]),
        }
    }

    pub async fn handle_control_frame(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        self.handle_control_frame_timed(ctx, frame, None).await
    }

    pub(crate) async fn handle_control_frame_timed(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        dispatch_started_at: Option<StdInstant>,
    ) -> Result<Vec<Frame>, RouterError> {
        match frame.header.ty {
            FrameType::Ping => Ok(vec![pong(&frame)?]),
            FrameType::Hello => {
                self.handle_hello(ctx.connection_id, Some(ctx.egress.clone()), frame)
            }
            FrameType::Goodbye => self.handle_goodbye(ctx.connection_id),
            FrameType::Request => {
                if self
                    .forwarding
                    .module_endpoint_for_connection(ctx.connection_id)
                    .map_err(RouterError::Forwarding)?
                    .is_some()
                {
                    if !is_known_module_request_op(&frame.body) {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "unsupported_control_frame",
                            "module-originated channel-0 REQUEST is not supported",
                        )?]);
                    }
                    let request = match parse_module_control_request_from_module(&frame.body) {
                        Ok(request) => request,
                        Err((err, ControlRequestBodyError::UnknownOp)) => {
                            return Ok(vec![control_error_frame(
                                &frame,
                                "unsupported_control_frame",
                                format!("unsupported module-originated channel-0 REQUEST: {err}"),
                            )?])
                        }
                        Err((err, ControlRequestBodyError::InvalidBody)) => {
                            return Ok(vec![control_error_frame(
                                &frame,
                                "invalid_control_body",
                                format!("malformed module control body: {err}"),
                            )?])
                        }
                    };
                    let op = module_control_request_op(&request);
                    let corr = frame.header.corr;
                    log_control_dispatch_arrival(op, ctx.connection_id, corr);
                    let result =
                        self.handle_module_control_request(ctx.connection_id, frame, request);
                    log_slow_control_dispatch(dispatch_started_at, op, ctx.connection_id, corr);
                    return result;
                }

                if is_known_module_request_op(&frame.body) {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "not_registered",
                        "catalog.update requires an active module registration owned by this connection",
                    )?]);
                }

                let request = match parse_client_control_request(&frame.body) {
                    Ok(request) => request,
                    Err((err, ControlRequestBodyError::UnknownOp)) => {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "unknown_control_op",
                            format!("unknown client control op: {err}"),
                        )?])
                    }
                    Err((err, ControlRequestBodyError::InvalidBody)) => {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "invalid_control_body",
                            format!("malformed client control body: {err}"),
                        )?])
                    }
                };
                let op = client_control_request_op(&request);
                let corr = frame.header.corr;
                log_control_dispatch_arrival(op, ctx.connection_id, corr);
                #[cfg(test)]
                if let Some(delay) = self.control_dispatch_delay {
                    tokio::time::sleep(delay).await;
                }
                let result = self
                    .handle_client_control_request(ctx, frame, request)
                    .await;
                log_slow_control_dispatch(dispatch_started_at, op, ctx.connection_id, corr);
                result
            }
            FrameType::Push => {
                let Some(endpoint) = self
                    .forwarding
                    .module_endpoint_for_connection(ctx.connection_id)
                    .map_err(RouterError::Forwarding)?
                else {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "unsupported_control_frame",
                        "client-originated channel-0 PUSH is not supported",
                    )?]);
                };
                self.handle_status_update(endpoint, frame)
            }
            FrameType::Response | FrameType::Error
                if self
                    .forwarding
                    .module_endpoint_for_connection(ctx.connection_id)
                    .map_err(RouterError::Forwarding)?
                    .is_some() =>
            {
                self.handle_module_relay_response(ctx.connection_id, frame)
            }
            ty => Ok(vec![control_error_frame(
                &frame,
                "unsupported_control_frame",
                format!("unsupported channel-0 frame {ty:?}"),
            )?]),
        }
    }

    pub fn cleanup_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Vec<crate::registry::ModuleRegistration>, RegistryError> {
        let crash_closed = self
            .registry
            .get_module_by_connection(connection_id)?
            .and_then(|registration| {
                self.forwarding
                    .module_endpoint_for_connection(connection_id)
                    .ok()
                    .flatten()
                    .and_then(|endpoint| self.forwarding.endpoint_routes(endpoint).ok())
                    .map(|routes| (registration.manifest.module_id, routes))
            });
        if let Some((module_id, routes)) = crash_closed {
            let terminal = match self.supervisor.get(&module_id) {
                None => false,
                Some(module) => match module.will_recover_after_connection_loss() {
                    Ok(will_recover) => !will_recover,
                    Err(err) => {
                        warn!(
                            %module_id,
                            error = %err,
                            "failed to read crash recovery verdict; reporting non-terminal conservatively"
                        );
                        false
                    }
                },
            };
            send_route_control_pushes(
                &self.forwarding,
                routes,
                ClientControlPush::RouteClosed {
                    module_id,
                    reason: RouteCloseReason::Crash,
                    drained: false,
                    abandoned: 0,
                    terminal: Some(terminal),
                },
            );
        }
        let registrations = self.deregister_connection(connection_id);
        if let Ok(released_routes) = self.forwarding.cleanup_connection(connection_id) {
            self.emit_route_goodbyes(released_routes);
        }
        // Signal the registration-release watch only now that BOTH registry and
        // forwarding teardown are done, so a supervisor waiting to spawn a
        // replacement never observes release while old routes still exist.
        if matches!(&registrations, Ok(r) if !r.is_empty()) {
            crate::supervise::notify_registration_release();
            self.capability_evaluator.wake_deadline_loop();
            self.refresh_capability_requirements();
        }
        registrations
    }

    pub(crate) fn handle_route_goodbye(
        &self,
        connection_id: ConnectionId,
        route_channel: u16,
        route_epoch: u32,
    ) -> Result<bool, RouterError> {
        debug!(
            connection_id = connection_id.get(),
            route_channel, route_epoch, "handling route GOODBYE"
        );
        let RouteRelease::Removed(released_route) = self
            .forwarding
            .release_client_route(connection_id, route_channel, route_epoch)
            .map_err(RouterError::Forwarding)?
        else {
            return Ok(false);
        };
        self.emit_route_goodbyes(vec![released_route]);
        Ok(true)
    }

    fn emit_route_goodbyes(&self, released_routes: Vec<GoodbyeTarget>) {
        for released in released_routes {
            let frame = match Frame::build_with_version(
                released.negotiated_ver,
                FrameType::Goodbye,
                control_flags(),
                released.channel,
                released.epoch,
                0,
                Vec::new(),
            ) {
                Ok(frame) => frame,
                Err(err) => {
                    warn!(
                        route_channel = released.channel,
                        error = %err,
                        "failed to build route GOODBYE frame"
                    );
                    continue;
                }
            };
            if let Err(err) = released.sink.try_send(frame) {
                if released.close_on_delivery_failure() {
                    warn!(
                        target_connection_id = released.connection_id.get(),
                        route_channel = released.channel,
                        error = %err,
                        "route GOODBYE was not delivered to client; closing target connection"
                    );
                    if self
                        .forwarding
                        .escalate_client_delivery_failure(
                            released.connection_id,
                            released.channel,
                            released.epoch,
                            CloseReason::new(
                                "route_goodbye_delivery_failed",
                                format!(
                                    "failed to enqueue route GOODBYE for channel {}: {err}",
                                    released.channel
                                ),
                            ),
                        )
                        .unwrap_or(false)
                    {
                        self.counters.increment_goodbye_relay_client_failed();
                    }
                } else {
                    self.counters
                        .increment_goodbye_relay_module_dropped(released.module_id.as_deref());
                    warn!(
                        target_connection_id = released.connection_id.get(),
                        route_channel = released.channel,
                        error = %err,
                        "route GOODBYE to module dropped under backpressure; not closing shared module connection"
                    );
                }
            }
        }
    }

    /// Best-effort GOODBYE to a module for a route channel subc reserved but then
    /// abandoned (route.bind relay timed out, its waiter was cancelled, or subc's
    /// own commit failed after the module had already accepted). Without this, a
    /// module that accepts late keeps a binding subc has torn down, so a later
    /// frame on that module channel could misdeliver if the channel is reused.
    ///
    /// Never closes the shared module connection on failure: a dropped notification
    /// only wastes a bounded amount of warm module-side state, which the module's
    /// own idle reaper reclaims. Only call this once the route.bind relay was
    /// actually enqueued to the module — if the relay send itself failed, the
    /// module never created a binding and there is nothing to tear down.
    fn send_abandoned_route_bind_goodbye(
        &self,
        module_sink: &crate::FrameSink,
        negotiated_ver: u8,
        module_channel: u16,
        module_epoch: u32,
    ) {
        let frame = match Frame::build_with_version(
            negotiated_ver,
            FrameType::Goodbye,
            control_flags(),
            module_channel,
            module_epoch,
            0,
            Vec::new(),
        ) {
            Ok(frame) => frame,
            Err(err) => {
                warn!(
                    route_channel = module_channel,
                    error = %err,
                    "failed to build GOODBYE for abandoned route.bind"
                );
                return;
            }
        };
        if let Err(err) = module_sink.try_send(frame) {
            warn!(
                route_channel = module_channel,
                error = %err,
                "GOODBYE for abandoned route.bind dropped; module idle reaper will reclaim the binding"
            );
        }
    }

    fn handle_hello(
        &self,
        connection_id: ConnectionId,
        sink: Option<crate::FrameSink>,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        debug!(
            connection_id = connection_id.get(),
            corr = frame.header.corr,
            "handling HELLO"
        );
        let hello_value = match serde_json::from_slice::<serde_json::Value>(&frame.body) {
            Ok(value) => value,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_hello",
                    format!("malformed HELLO body: {err}"),
                )?])
            }
        };
        if let Err(err) = validate_hello_capability_grammar(&hello_value) {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_capability_grammar",
                err.to_string(),
            )?]);
        }
        if let Some(provenance) = hello_value
            .get("manifest")
            .and_then(|manifest| manifest.get("provenance"))
        {
            if let Err(err) = serde_json::from_value::<ManifestProvenance>(provenance.clone()) {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_manifest",
                    format!("malformed manifest provenance: {err}"),
                )?]);
            }
        }
        let hello = match serde_json::from_value::<ModuleHelloBody>(hello_value) {
            Ok(hello) => hello,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_hello",
                    format!("malformed HELLO body: {err}"),
                )?])
            }
        };

        if hello.protocol_ver != hello.manifest.protocol_ver {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_manifest",
                format!(
                    "HELLO protocol_ver {} does not match manifest protocol_ver {}",
                    hello.protocol_ver, hello.manifest.protocol_ver
                ),
            )?]);
        }

        if hello.manifest.module_id.trim().is_empty() {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_manifest",
                "manifest module_id must not be empty",
            )?]);
        }

        let negotiated_ver = match negotiate_version(hello.protocol_ver) {
            Ok(negotiated_ver) => negotiated_ver,
            Err(message) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "version_unsupported",
                    message,
                )?])
            }
        };

        // Reserved-module identity gate: a module_id configured `reserved` may be
        // registered ONLY by the process subc spawned for it, proven by echoing the
        // one-time launch nonce subc injected. A non-reserved id has no recorded
        // nonce and always passes. This blocks a key-holder from impersonating a
        // security-boundary module (e.g. the credential vault) while the real one is
        // down/restarting and its registration slot is momentarily free.
        if let Some(rejection) = self
            .supervisor
            .reserved_hello_rejection(&hello.manifest.module_id, hello.launch_nonce.as_deref())
        {
            let message = match rejection {
                ReservedHelloRejection::Exact { module_id } => format!(
                    "module_id '{module_id}' is reserved; HELLO without a valid launch nonce is rejected"
                ),
                ReservedHelloRejection::Prefix {
                    prefix,
                    owner_module_id,
                } => format!(
                    "module_id '{}' matches reserved prefix '{prefix}' owned by '{owner_module_id}'; HELLO without the owner launch nonce is rejected",
                    hello.manifest.module_id
                ),
            };
            return Ok(vec![control_error_frame(
                &frame,
                "reserved_module",
                message,
            )?]);
        }

        let reserved_capability_refusals = self.capability_evaluator.reserved_hello_refusals(
            &hello.manifest.module_id,
            hello.manifest.capabilities.as_ref(),
        );
        if let Some(refusal) = reserved_capability_refusals.first() {
            let capability = refusal.capability.clone();
            let bound_module = refusal.claimants[0].clone();
            log_duplicate_claim_events(reserved_capability_refusals);
            return Ok(vec![control_error_frame(
                &frame,
                "reserved_capability",
                format!(
                    "capability '{}' is reserved for module_id '{}'; claimant '{}' was refused",
                    capability, bound_module, hello.manifest.module_id
                ),
            )?]);
        }

        // A connection that already opened client routes must not also register as
        // a module: cleanup would then release only one side and leak the other.
        if self
            .forwarding
            .connection_has_client_routes(connection_id)
            .map_err(RouterError::Forwarding)?
        {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_hello",
                "connection has open client routes and cannot also register as a module",
            )?]);
        }

        let control_ops = effective_module_control_ops(hello.control_ops);
        let registration = match self.registry.register_with_control_ops(
            hello.manifest,
            negotiated_ver,
            connection_id,
            control_ops,
        ) {
            Ok(registration) => registration,
            Err(RegistryError::DuplicateModuleId { module_id }) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "duplicate_module_id",
                    format!(
                        "module_id '{module_id}' is already registered; duplicate HELLO rejected"
                    ),
                )?])
            }
            Err(err @ RegistryError::PathHazardModuleId { .. }) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_module_id",
                    err.to_string(),
                )?])
            }
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "registry_error",
                    err.to_string(),
                )?])
            }
        };

        if let Some(sink) = sink {
            // The forwarding table's module store is also the daemon-to-module
            // control-RPC lane, so every HELLO gets a live endpoint even when the
            // manifest has no routable provider role. Non-routable modules still
            // cannot receive route.bind in production: `handle_route_open` checks
            // the registry manifest with `target_has_required_role` before the
            // only production call to `begin_route_bind_relay_for` below that
            // route.open path. The remaining direct relay callers are unit tests
            // and benchmark harnesses that construct forwarding state explicitly.
            let concurrency = manifest_concurrency(&registration.manifest);
            if let Err(err) = self.forwarding.register_module_connection(
                connection_id,
                registration.manifest.module_id.clone(),
                negotiated_ver,
                concurrency,
                sink,
            ) {
                // Forwarding registration failed, so there is no forwarding
                // state to tear down. Remove the registry entry and signal the
                // release watch directly.
                if matches!(self.deregister_connection(connection_id), Ok(r) if !r.is_empty()) {
                    crate::supervise::notify_registration_release();
                }
                return Ok(vec![control_error_frame(
                    &frame,
                    forwarding_error_code(&err),
                    err.to_string(),
                )?]);
            }
        }

        // Exposure over assumption: Concurrency's serde default is pinned to the
        // pre-field behavior (ModuleManaged), so a management surface that is
        // genuinely Serial and just never declared it inherits concurrent
        // delivery silently. Logging which registrations RESOLVED BY DEFAULT
        // turns "no module has been bitten yet" into the checkable claim "no
        // module is exposed" -- one read of the boot log instead of a fleet
        // audit. Detected from the raw HELLO bytes because the serde default
        // deliberately erases the absent/declared distinction from the type.
        if manifest_concurrency_was_defaulted(&frame.body, &registration.manifest) {
            info!(
                module_id = %registration.manifest.module_id,
                "management surface registered with DEFAULTED concurrency=module_managed (manifest predates the field; declare the real lane)"
            );
        }

        let cached_registration = RegisteredModule {
            module_id: registration.manifest.module_id.clone(),
            module_version: registration.manifest.module_version.clone(),
            capabilities: registration.manifest.capabilities.clone(),
        };
        if self.capability_evaluator.record_hello(&cached_registration) {
            warn!(
                module_id = %cached_registration.module_id,
                "capability claims drifted from the cached manifest"
            );
        }
        if capability_census_trigger(None, registration.manifest.capabilities.as_ref()) {
            self.enforce_capability_denies();
        }
        self.refresh_capability_requirements();

        info!(
            module_id = %registration.manifest.module_id,
            module_version = %registration.manifest.module_version,
            negotiated_ver,
            routable_provider = manifest_provides_routable_role(&registration.manifest),
            connection_id = connection_id.get(),
            "module registered"
        );

        let ack = ModuleHelloAckBody {
            negotiated_ver,
            subc_ops: module_subc_ops(),
            subc_capabilities: self.subc_capabilities.as_ref().to_vec(),
            storage: self
                .storage_config
                .as_ref()
                .map(|cfg| cfg.descriptor_for(&registration.manifest.module_id)),
        };
        let body = serde_json::to_vec(&ack).map_err(|err| {
            RouterError::backend(
                0,
                frame.header.corr,
                format!("failed to encode HELLO_ACK: {err}"),
            )
        })?;

        Ok(vec![Frame::build_with_version(
            negotiated_ver,
            FrameType::HelloAck,
            control_flags(),
            0,
            0,
            frame.header.corr,
            body,
        )
        .map_err(RouterError::FrameBuild)?])
    }

    async fn handle_client_control_request(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        request: ClientControlRequest,
    ) -> Result<Vec<Frame>, RouterError> {
        match request {
            ClientControlRequest::ServerDescribe {} => self.handle_server_describe(frame),
            ClientControlRequest::CatalogList { module_id } => {
                self.handle_catalog_list(frame, module_id)
            }
            ClientControlRequest::RouteOpen {
                target,
                identity,
                consumer_identity,
                consumer_capabilities,
                admission_facts,
            } => {
                self.handle_route_open(
                    ctx,
                    frame,
                    RouteOpenRequest {
                        target,
                        identity,
                        consumer_identity,
                        consumer_capabilities,
                        admission_facts,
                    },
                )
                .await
            }
            ClientControlRequest::RoutePoll {
                route_channel,
                route_epoch,
                kind,
            } => self.handle_route_poll(ctx, frame, route_channel, route_epoch, kind),
            ClientControlRequest::SupervisorList {} => self.handle_supervisor_list(frame),
            ClientControlRequest::SupervisorRestart {
                module_id,
                drain_timeout_ms,
            } => {
                self.handle_supervisor_restart(frame, module_id, drain_timeout_ms)
                    .await
            }
            ClientControlRequest::SupervisorReload { module_id } => {
                self.handle_supervisor_reload(frame, module_id).await
            }
            ClientControlRequest::SupervisorRescan { preview } => {
                self.handle_supervisor_rescan(frame, preview).await
            }
            ClientControlRequest::SupervisorReleaseReserved { module_id } => {
                self.handle_supervisor_release_reserved(frame, module_id)
                    .await
            }
            ClientControlRequest::SupervisorSetEnabled { module_id, enabled } => {
                self.handle_supervisor_set_enabled(frame, module_id, enabled)
                    .await
            }
            ClientControlRequest::SupervisorHealthProbe { module_id } => {
                self.handle_supervisor_health_probe(frame, module_id).await
            }
            ClientControlRequest::SupervisorHealth {} => self.handle_supervisor_health(frame),
            ClientControlRequest::SupervisorRoutes { module_id } => {
                self.handle_supervisor_routes(frame, module_id)
            }
            ClientControlRequest::SupervisorProvenance { module_id } => {
                self.handle_supervisor_provenance(frame, module_id).await
            }
            ClientControlRequest::SupervisorStderrTail {
                module_id,
                max_lines,
                max_bytes,
            } => self.handle_supervisor_stderr_tail(frame, module_id, max_lines, max_bytes),
            ClientControlRequest::SupervisorTerminals { module_id } => {
                self.handle_supervisor_terminals(frame, module_id)
            }
        }
    }

    fn handle_module_control_request(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        request: ModuleControlRequestFromModule,
    ) -> Result<Vec<Frame>, RouterError> {
        match request {
            ModuleControlRequestFromModule::CatalogUpdate {
                provides,
                capabilities,
            } => self.handle_catalog_update(connection_id, frame, provides, capabilities),
        }
    }

    fn handle_catalog_update(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        provides: Vec<ProviderRole>,
        capabilities: Option<CapabilityDeclarations>,
    ) -> Result<Vec<Frame>, RouterError> {
        self.refresh_capability_requirements();
        let Some(registration) = self
            .registry
            .get_module_by_connection(connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
        else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "catalog.update requires an active module registration owned by this connection",
            )?]);
        };

        if let Some(message) =
            catalog_update_frozen_field_message(&registration.manifest, &provides)
        {
            return Ok(vec![control_error_frame(
                &frame,
                "catalog_update_frozen_field",
                message,
            )?]);
        }

        let candidate = ModuleManifest {
            provides: provides.clone(),
            capabilities: capabilities
                .clone()
                .or_else(|| registration.manifest.capabilities.clone()),
            ..registration.manifest.clone()
        };
        if let Err(err) = candidate.validate_capability_grammar() {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_capability_grammar",
                err.to_string(),
            )?]);
        }

        let updated = self
            .registry
            .replace_catalog_for_connection(connection_id, provides, capabilities)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        if updated.is_none() {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "catalog.update requires an active module registration owned by this connection",
            )?]);
        }
        if let Ok((_, registrations)) = self.runtime_capability_snapshot() {
            log_duplicate_claim_events(
                self.capability_evaluator
                    .duplicate_claims(DuplicateClaimSource::CatalogUpdate, &registrations),
            );
        }
        if capability_census_trigger(
            registration.manifest.capabilities.as_ref(),
            updated
                .as_ref()
                .and_then(|entry| entry.manifest.capabilities.as_ref()),
        ) {
            self.enforce_capability_denies();
        }
        self.refresh_capability_requirements();

        let response = ModuleControlResponseToModule::CatalogUpdate {};
        control_response_body_frame(
            &frame,
            &response,
            "ModuleControlResponseToModule::CatalogUpdate",
        )
        .map(|frame| vec![frame])
    }

    fn handle_server_describe(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        self.refresh_capability_requirements();
        // A bare connection count is ambiguous between many clients holding a
        // route each and one client accumulating hundreds, so publish the
        // concentration alongside it. Route state is best-effort here: a
        // diagnostic endpoint must still answer if the forwarding lock is
        // contended.
        let mut counters = self.counters.snapshot();
        if let (Ok((connections_with_routes, max)), Some(obj)) = (
            self.forwarding.client_route_concentration(),
            counters.as_object_mut(),
        ) {
            obj.insert(
                "client_connections_with_routes".into(),
                connections_with_routes.into(),
            );
            obj.insert("max_routes_on_one_connection".into(), max.into());
        }
        let response = ClientControlResponse::ServerDescribe {
            protocol_ver: PROTOCOL_VERSION,
            subc_ops: subc_ops(),
            capabilities: self.subc_capabilities.as_ref().to_vec(),
            connected_clients: self.connected_clients.count(),
            counters: Some(counters),
            build_git_sha: Some(env!("SUBC_BUILD_GIT_SHA").to_string()),
            build_lock_digest: Some(env!("SUBC_BUILD_LOCK_DIGEST").to_string()),
            capability_requirements: self.capability_requirement_statuses(),
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::ServerDescribe",
        )?])
    }

    fn handle_catalog_list(
        &self,
        frame: Frame,
        module_id: Option<String>,
    ) -> Result<Vec<Frame>, RouterError> {
        let (generation, modules) = self.registry.list_modules().map_err(|err| {
            RouterError::backend(0, frame.header.corr, format!("registry error: {err}"))
        })?;
        let entries = modules
            .into_iter()
            .filter(|registration| {
                module_id
                    .as_deref()
                    .map(|wanted| registration.manifest.module_id == wanted)
                    .unwrap_or(true)
            })
            .map(|registration| {
                let roles = registration.manifest.provides;
                CatalogEntry {
                    module_id: registration.manifest.module_id,
                    module_version: Some(registration.manifest.module_version),
                    roles,
                    control_ops: registration.control_ops,
                    capabilities: registration.manifest.capabilities,
                }
            })
            .collect();
        let response = ClientControlResponse::CatalogList {
            generation,
            modules: entries,
            subc_ops: subc_ops(),
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::CatalogList",
        )?])
    }

    fn route_open_principal(
        &self,
        frame: &Frame,
        consumer_identity: Option<ConsumerIdentity>,
    ) -> Result<Result<Principal, Frame>, RouterError> {
        let Some(consumer_identity) = consumer_identity else {
            return Ok(Ok(Principal::Direct));
        };

        if self.supervisor.spawned_consumer_authorized(
            &consumer_identity.module_id,
            &consumer_identity.launch_nonce,
        ) {
            return Ok(Ok(Principal::Reserved {
                module_id: consumer_identity.module_id,
            }));
        }

        Ok(Err(control_error_frame(
            frame,
            "bad_consumer_identity",
            format!(
                "consumer_identity for module_id '{}' did not match a supervised launch nonce",
                consumer_identity.module_id
            ),
        )?))
    }

    async fn handle_route_open(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        request: RouteOpenRequest,
    ) -> Result<Vec<Frame>, RouterError> {
        let RouteOpenRequest {
            target,
            mut identity,
            consumer_identity,
            consumer_capabilities,
            admission_facts,
        } = request;
        let target_module_id = target_module_id(&target).to_string();
        debug!(
            connection_id = ctx.connection_id.get(),
            corr = frame.header.corr,
            module_id = %target_module_id,
            "handling route.open"
        );

        // WHY THESE REPLIES DISCRIMINATE FREELY, since the usual rule is the
        // opposite. Below, a caller learns whether a module is unregistered,
        // supervised-but-down (with state/enabled/live), or registered without the
        // requested role. Elsewhere that is an enumeration leak: a probe learning
        // the shape of a fleet it cannot otherwise see.
        //
        // It is not one here, and the reason is the ACCESS MODEL rather than
        // anything about these errors. Reaching route.open requires the
        // pre-envelope HMAC handshake, whose key lives in a 0600 user-owned
        // connection file, so any caller who completes it already runs as this
        // user -- and can read subc.jsonc for the module list and `ck module
        // status` for live state. The reply discloses nothing the caller cannot
        // read more easily from disk, while the precision is load-bearing:
        // `unknown_module` is retryable and a missing role is not.
        //
        // IF THE HANDSHAKE EVER ADMITS A PRINCIPAL THAT IS NOT THIS USER -- a
        // remote transport, a sandboxed caller, a shared-host mode -- THAT
        // PREMISE DIES AND THESE THREE REPLIES MUST COLLAPSE INTO ONE.
        let Some(registration) = self
            .registry
            .get_module(&target_module_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
        else {
            if let Some((status, warming)) =
                self.supervisor_status(&target_module_id, frame.header.corr)?
            {
                return Ok(vec![control_error_frame(
                    &frame,
                    if warming {
                        "module_warming"
                    } else {
                        "target_unavailable"
                    },
                    format!(
                        "module_id '{target_module_id}' is supervised but not available (state={}, enabled={}, live={})",
                        status.state, status.enabled, status.live
                    ),
                )?]);
            }
            if let Some(removed_ago_ms) =
                self.supervisor.removal_tombstone_age_ms(&target_module_id)
            {
                return Ok(vec![control_error_frame(
                    &frame,
                    error_codes::MODULE_REMOVED,
                    format!("module_id '{target_module_id}' was removed {removed_ago_ms} ms ago"),
                )?]);
            }
            return Ok(vec![control_error_frame(
                &frame,
                error_codes::UNKNOWN_MODULE,
                format!("module_id '{target_module_id}' is not registered"),
            )?]);
        };

        if !target_has_required_role(&target, &registration.manifest.provides) {
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                format!("module_id '{target_module_id}' does not provide the requested target"),
            )?]);
        }

        if registration.state != ChannelState::Active {
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                format!("module_id '{target_module_id}' is not active"),
            )?]);
        }

        if self
            .forwarding
            .module_is_draining(&target_module_id)
            .map_err(RouterError::Forwarding)?
        {
            return Ok(vec![control_error_frame(
                &frame,
                "module_reloading",
                format!("module_id '{target_module_id}' is reloading"),
            )?]);
        }

        if self
            .process_liveness
            .as_ref()
            .and_then(|process_liveness| process_liveness.process_live(&target_module_id))
            == Some(false)
        {
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                format!("module_id '{target_module_id}' is not live"),
            )?]);
        }

        if !self
            .forwarding
            .has_live_module_connection(&target_module_id)
            .map_err(RouterError::Forwarding)?
        {
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                format!("module_id '{target_module_id}' has no live forwarding connection"),
            )?]);
        }

        if let Some(error) =
            self.guard_module_control_op(&frame, &target_module_id, "route.bind")?
        {
            return Ok(vec![error]);
        }

        let principal = match self.route_open_principal(&frame, consumer_identity)? {
            Ok(principal) => principal,
            Err(error) => return Ok(vec![error]),
        };

        // This is attested, control-plane policy for supervised module origins.
        // Keep it before route reservation and out of the opaque forwarding hot
        // path: data frames must never acquire a per-frame capability check.
        if let Principal::Reserved {
            module_id: opening_module_id,
        } = &principal
        {
            if let Some(opening_registration) = self
                .registry
                .get_module(opening_module_id)
                .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
            {
                if let Some(capability) =
                    denied_capability(&opening_registration.manifest, &registration.manifest)
                {
                    warn!(
                        opening_module_id,
                        target_module_id,
                        capability,
                        "refusing route.open because an attested capability deny edge matches"
                    );
                    return Ok(vec![control_error_frame(
                        &frame,
                        "capability_forbidden",
                        format!(
                            "module_id '{opening_module_id}' must never reach capability '{capability}' provided by '{target_module_id}'"
                        ),
                    )?]);
                }
            }
        }

        if admission_facts.is_some() {
            let carrier_matches = matches!(
                &principal,
                Principal::Reserved { module_id }
                    if self.admission_facts_carrier_module_id.as_deref() == Some(module_id)
            );
            if !carrier_matches {
                return Ok(vec![control_error_frame(
                    &frame,
                    "admission_facts_not_permitted",
                    "admission facts may only be carried by the configured reserved module",
                )?]);
            }

            let target_allowed = self
                .admission_facts_targets
                .as_ref()
                .is_some_and(|targets| targets.iter().any(|id| id == &target_module_id));
            if !target_allowed {
                return Ok(vec![control_error_frame(
                    &frame,
                    "admission_facts_target_not_allowed",
                    format!(
                        "admission facts are not permitted for target module_id '{target_module_id}'"
                    ),
                )?]);
            }

            // Keep the value opaque to subc. The downstream admission validator owns
            // schema and semantic checks; this daemon only enforces carrier authority
            // and the configured destination allowlist.
        }

        // Bind admits a root that no longer exists on disk, because refusing here
        // closes the only exit from a paused run: cancel needs a bound route, and a
        // renamed or reclaimed directory makes that route unopenable forever. The
        // run itself is intact and still addressable by its recorded identity.
        //
        // This does NOT relax the rule the strict constructor protects. That rule is
        // that no root is ever aliased into NEW durable state -- a missing component
        // can reappear as a symlink elsewhere, which would move the identity and
        // split a session's history across two of them. The engine now refuses the
        // two operations that create such state (send and import) at admission,
        // which is a narrower way to hold the same invariant: reads and terminations
        // are admitted, writes are not. That refusal had to ship before this line
        // changed, or there is an interval where a send commits under a provisional
        // identity -- the exact failure the original policy existed to prevent.
        //
        // Resolution follows realpath rather than lexical cleanup: the longest
        // existing ancestor is canonicalized and the missing tail re-appended, so a
        // live root is unchanged and a vanished leaf keeps the identity it was
        // admitted under. Lexical cleanup would mint a DIFFERENT identity for the
        // same caller the moment the directory vanished, which strands the run more
        // quietly than refusing it.
        let project_root = match ProjectRootId::from_path_allowing_missing(&identity.project_root) {
            Ok(project_root) => project_root,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_project_root",
                    err.to_string(),
                )?])
            }
        };
        identity.project_root = project_root.as_path().to_path_buf();

        // Resolve the per-module budget here so the wait matches the operator's
        // intent for this specific target. A per-module override in
        // `subc.jsonc` (or `with_route_bind_relay_timeouts` for embedded
        // daemons) wins over the daemon-wide default.
        let route_bind_relay_timeout = self.route_bind_relay_timeout_for(&target_module_id);
        let relay_deadline = Instant::now() + route_bind_relay_timeout;
        let pending = match self
            .forwarding
            .begin_route_bind_relay_for(
                ctx.connection_id,
                ctx.egress.clone(),
                response_version(&frame),
                frame.header.corr,
                &target_module_id,
                principal.clone(),
                relay_deadline,
            )
            .await
        {
            Ok(pending) => pending,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    forwarding_error_code(&err),
                    err.to_string(),
                )?])
            }
        };
        let crate::forwarding::PendingRouteBindRelay {
            endpoint,
            module_sink,
            negotiated_ver,
            client_channel,
            client_epoch,
            module_channel,
            module_epoch,
            corr: relay_corr,
            receiver,
        } = pending;
        let mut reservation =
            RouteBindReservationGuard::new(Arc::clone(&self.forwarding), endpoint, relay_corr);

        debug!(
            connection_id = ctx.connection_id.get(),
            client_channel,
            client_epoch,
            module_channel,
            module_epoch,
            "reserved route handle pair"
        );
        let relay = ModuleControlRequest::RouteBind {
            route_channel: module_channel,
            epoch: module_epoch,
            target,
            identity,
            principal: Some(principal),
            consumer_capabilities,
            admission_facts,
        };
        let relay_body = serde_json::to_vec(&relay).map_err(|err| {
            RouterError::backend(
                0,
                frame.header.corr,
                format!("failed to encode route.bind request: {err}"),
            )
        })?;
        let relay_frame = Frame::build_with_version(
            negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            relay_corr,
            relay_body,
        )
        .map_err(RouterError::FrameBuild)?;

        if let Err(err) = module_sink.send(relay_frame).await {
            reservation.release_and_disarm();
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                err.to_string(),
            )?]);
        }

        if !self
            .forwarding
            .mark_route_bind_relay_enqueued(endpoint, relay_corr)
            .map_err(RouterError::Forwarding)?
        {
            self.send_abandoned_route_bind_goodbye(
                &module_sink,
                negotiated_ver,
                module_channel,
                module_epoch,
            );
        }

        match timeout_at(relay_deadline, receiver).await {
            Ok(Ok(RouteBindRelayOutcome::Accepted)) => {
                reservation.disarm();
                Ok(Vec::new())
            }
            Ok(Ok(RouteBindRelayOutcome::Rejected(body))) => {
                reservation.release_and_disarm();
                Ok(vec![control_error_body_frame(&frame, body)?])
            }
            Ok(Ok(RouteBindRelayOutcome::ModuleGone(message))) => {
                reservation.release_and_disarm();
                // Fires when the module's connection closes while a relayed
                // bind is pending -- typically a caller racing a module restart
                // whose bind was relayed BEFORE the drain mark went up. Logged
                // because the caller sees only its own error and the fleet has
                // already spent one diagnosis round unable to tell this arm
                // from a relay timeout without daemon-side evidence.
                tracing::warn!(
                    module_id = %target_module_id,
                    "route.bind relay abandoned: {message}"
                );
                Ok(vec![control_error_frame(
                    &frame,
                    "target_unavailable",
                    message,
                )?])
            }
            Ok(Err(_)) => {
                reservation.release_and_disarm();
                Ok(vec![control_error_frame(
                    &frame,
                    "target_unavailable",
                    "route.bind relay waiter was canceled before the module responded",
                )?])
            }
            Err(_) => {
                reservation.release_and_disarm();
                // The generous budget just burned to no answer: the module is
                // registered and its connection is up, but its bind handler sat
                // on the ack for the full budget (warm-on-bind, cold configure,
                // or a wedged handler). Every earlier unavailability shape
                // fast-refuses BEFORE the relay, so this arm firing means the
                // slowness is module-side -- log it so the per-module timeline
                // is reconstructable without client audit rows.
                tracing::warn!(
                    module_id = %target_module_id,
                    timeout_ms = route_bind_relay_timeout.as_millis() as u64,
                    "route.bind relay timed out: module did not ack within budget"
                );
                Ok(vec![control_error_frame(
                    &frame,
                    "module_timeout",
                    format!(
                        "module_id '{target_module_id}' did not answer route.bind within {:?}",
                        route_bind_relay_timeout
                    ),
                )?])
            }
        }
    }

    fn handle_supervisor_list(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        let generation = self
            .registry
            .generation()
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let modules = self
            .supervisor
            .list()
            .into_iter()
            .map(|module| {
                let status = module.status_for_control("list").map_err(|err| {
                    RouterError::backend(
                        0,
                        frame.header.corr,
                        format!("failed to read supervisor status: {err}"),
                    )
                })?;
                Ok(SupervisorEntry {
                    module_id: status.module_id,
                    state: status.state.to_string(),
                    enabled: status.enabled,
                    live: status.live,
                    health: status.health.status,
                    last_probe_ms: status.health.last_probe_ms,
                    last_exit_code: status.last_exit.as_ref().and_then(|e| e.code),
                    last_exit_signal: status.last_exit.as_ref().and_then(|e| e.signal),
                    last_exit_ms: status.last_exit.as_ref().map(|e| e.at_ms),
                    restart_count: Some(status.restart_count),
                    max_restarts: Some(status.max_restarts),
                })
            })
            .collect::<Result<Vec<_>, RouterError>>()?;
        let response = ClientControlResponse::SupervisorList {
            generation,
            modules,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorList",
        )?])
    }

    fn handle_supervisor_stderr_tail(
        &self,
        frame: Frame,
        module_id: String,
        max_lines: Option<u32>,
        max_bytes: Option<u32>,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        let snapshot = module.stderr_tail(
            max_lines.map(|value| value as usize),
            max_bytes.map(|value| value as usize),
        );

        let response = ClientControlResponse::SupervisorStderrTail {
            module_id,
            tail: StderrTail {
                capture: match snapshot.capture {
                    CaptureState::Captured => StderrCaptureState::Captured,
                    CaptureState::Incomplete { reason } => {
                        StderrCaptureState::Incomplete { reason }
                    }
                    CaptureState::NotCaptured { reason } => {
                        StderrCaptureState::NotCaptured { reason }
                    }
                },
                entries: snapshot
                    .entries
                    .into_iter()
                    .map(|entry| match entry {
                        TailEntry::Line { text, truncated } => {
                            StderrTailEntry::Line { text, truncated }
                        }
                        TailEntry::ProcessStart => StderrTailEntry::ProcessStart,
                    })
                    .collect(),
                dropped_lines: snapshot.dropped_lines,
            },
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorStderrTail",
        )?])
    }

    fn handle_supervisor_terminals(
        &self,
        frame: Frame,
        module_id: String,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        let snapshot = module.terminal_history();
        let response = ClientControlResponse::SupervisorTerminals {
            module_id,
            terminals: TerminalHistory {
                daemon_started_at_ms: snapshot.daemon_started_at_ms,
                entries: snapshot
                    .entries
                    .into_iter()
                    .map(|entry| TerminalEntry {
                        exit_code: entry.exit_code,
                        exit_signal: entry.exit_signal,
                        at_ms: entry.at_ms,
                        disposition: entry.disposition,
                    })
                    .collect(),
                dropped: snapshot.dropped,
            },
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorTerminals",
        )?])
    }

    fn handle_supervisor_routes(
        &self,
        frame: Frame,
        module_id: Option<String>,
    ) -> Result<Vec<Frame>, RouterError> {
        let modules = self
            .forwarding
            .route_census(module_id.as_deref())
            .map_err(RouterError::Forwarding)?
            .into_iter()
            .map(|(module_id, routes)| SupervisorRouteModule {
                module_id,
                routes: routes
                    .into_iter()
                    .map(|route| SupervisorRoute {
                        consumer: match route.principal {
                            Principal::Reserved { module_id } => {
                                SupervisorRouteConsumer::Reserved { module_id }
                            }
                            Principal::Direct | Principal::Unverified => {
                                SupervisorRouteConsumer::Direct {
                                    connection_id: route.goodbye_target.connection_id.get(),
                                }
                            }
                        },
                        age_ms: Instant::now()
                            .saturating_duration_since(route.bound_at)
                            .as_millis()
                            .try_into()
                            .unwrap_or(u64::MAX),
                        draining: route.draining,
                        drain_reason: route.drain_reason,
                    })
                    .collect(),
            })
            .collect();
        let response = ClientControlResponse::SupervisorRoutes { modules };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorRoutes",
        )?])
    }

    async fn handle_supervisor_provenance(
        &self,
        frame: Frame,
        module_id: Option<String>,
    ) -> Result<Vec<Frame>, RouterError> {
        let mut selected = if let Some(module_id) = module_id {
            let Some(module) = self.supervisor.get(&module_id) else {
                return Ok(vec![control_error_frame(
                    &frame,
                    "unknown_module",
                    format!("module_id '{module_id}' is not supervised"),
                )?]);
            };
            vec![module]
        } else {
            self.supervisor.list()
        };

        let mut modules = Vec::with_capacity(selected.len());
        for module in selected.drain(..) {
            let status = module.status().map_err(|err| {
                RouterError::backend(
                    0,
                    frame.header.corr,
                    format!("failed to read supervisor status: {err}"),
                )
            })?;
            let module_declared = self
                .registry
                .get_module(&status.module_id)
                .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
                .and_then(|registration| registration.manifest.provenance)
                .map(|build| ModuleDeclaredProvenance::Reported { build })
                .unwrap_or(ModuleDeclaredProvenance::Unverifiable);
            #[cfg(test)]
            let running_image = match &self.provenance_probe_override {
                Some(result) => result.clone(),
                None => module.running_image_agreement().await,
            };
            #[cfg(not(test))]
            let running_image = module.running_image_agreement().await;
            modules.push(SupervisorModuleProvenance {
                module_id: status.module_id,
                module_declared,
                daemon_observed: SupervisorObservedProcess {
                    pid: status.pid,
                    spawned_at_ms: status.spawned_at_ms,
                    spawned_from: status.spawned_from,
                    running_image,
                },
            });
        }
        let daemon = SupervisorDaemonProvenance {
            daemon_build: self.daemon_provenance.build.clone(),
            daemon_observed: DaemonObservedProcess {
                pid: self.daemon_provenance.pid,
                started_at_ms: self.daemon_provenance.started_at_ms,
                running_image: self
                    .daemon_provenance
                    .probe
                    .observe(
                        self.daemon_provenance.pid,
                        self.daemon_provenance.executable_path.as_deref(),
                        self.daemon_provenance.executable_identity,
                    )
                    .await,
            },
        };
        let response = ClientControlResponse::SupervisorProvenance { daemon, modules };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorProvenance",
        )?])
    }

    fn handle_supervisor_health(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        self.refresh_capability_requirements();
        let generation = self
            .registry
            .generation()
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let modules = self
            .supervisor
            .list()
            .into_iter()
            .map(|module| {
                let status = module.status_for_control("health").map_err(|err| {
                    RouterError::backend(
                        0,
                        frame.header.corr,
                        format!("failed to read supervisor health: {err}"),
                    )
                })?;
                let module_id = status.module_id;
                let capability_detail = self
                    .capability_evaluator
                    .required_problem_detail(&module_id);
                Ok(SupervisorHealthEntry {
                    module_id,
                    status: status.health.status,
                    detail: append_capability_problem_detail(
                        status.health.detail,
                        capability_detail,
                    ),
                    metrics: status.health.metrics,
                    consecutive_failures: status.health.consecutive_failures,
                    late_answer_count: status.health.late_answer_count,
                    last_late_answer_latency_ms: status.health.last_late_answer_latency_ms,
                    last_action: status.health.last_action,
                    last_action_ms: status.health.last_action_ms,
                    last_probe_ms: status.health.last_probe_ms,
                })
            })
            .collect::<Result<Vec<_>, RouterError>>()?;
        let response = ClientControlResponse::SupervisorHealth {
            generation,
            modules,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorHealth",
        )?])
    }

    async fn handle_supervisor_restart(
        &self,
        frame: Frame,
        module_id: String,
        drain_timeout_ms: Option<u64>,
    ) -> Result<Vec<Frame>, RouterError> {
        let operation_lock = self.supervisor.operation_lock();
        let _operation_guard = operation_lock.lock().await;
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        if let Err(err) = module.restart(drain_timeout_ms).await {
            let (code, message) = match err {
                crate::supervise::SuperviseError::Disabled { .. } => {
                    ("module_disabled", err.to_string())
                }
                _ => (
                    "target_unavailable",
                    format!("failed to restart module_id '{module_id}': {err}"),
                ),
            };
            return Ok(vec![control_error_frame(&frame, code, message)?]);
        }

        let response = ClientControlResponse::SupervisorAck {
            module_id,
            applied: true,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }

    async fn handle_supervisor_reload(
        &self,
        frame: Frame,
        module_id: String,
    ) -> Result<Vec<Frame>, RouterError> {
        let operation_lock = self.supervisor.operation_lock();
        let _operation_guard = operation_lock.lock().await;
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        if let Err(err) = module.reload().await {
            let (code, message) = match err {
                crate::supervise::SuperviseError::Disabled { .. } => {
                    ("module_disabled", err.to_string())
                }
                _ => (
                    "reload_failed",
                    format!("failed to reload module_id '{module_id}': {err}"),
                ),
            };
            return Ok(vec![control_error_frame(&frame, code, message)?]);
        }

        let response = ClientControlResponse::SupervisorAck {
            module_id,
            applied: true,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }

    async fn handle_supervisor_rescan(
        &self,
        frame: Frame,
        preview: bool,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(context) = self.rescan.clone() else {
            return Ok(vec![control_error_frame(
                &frame,
                "rescan_unavailable",
                "the daemon was not started with a reloadable config path".to_string(),
            )?]);
        };

        let operation_lock = self.supervisor.operation_lock();
        let _operation_guard = operation_lock.lock().await;
        let loaded = match crate::daemon_config::load(&context.config_path) {
            Ok(config) => config,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_daemon_config",
                    format!("supervisor rescan rejected daemon config: {err}"),
                )?])
            }
        };
        // `load` reports a missing file as Ok(None), which is correct at boot
        // (no config, nothing to supervise) and catastrophic here: rescan treats
        // "not in the config" as "remove it", so an absent file would read as an
        // empty module list and retire the entire running fleet. An editor
        // writing via write-new-then-rename, or a half-finished edit, is enough
        // to open that window. Refuse instead: a config that cannot be read
        // carries no instruction to remove anything.
        let Some(config) = loaded else {
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
            modules,
            reserved_capabilities,
        ) = (
            config.port,
            config.storage,
            config.admission_facts_carrier_module_id,
            config.admission_facts_targets,
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
        if configured_port != context.configured_port {
            restart_required.push("port".to_string());
        }
        if storage_config != context.storage_config {
            restart_required.push("storage".to_string());
        }
        if admission_facts_carrier_module_id != context.admission_facts_carrier_module_id {
            restart_required.push("admission_facts_carrier_module_id".to_string());
        }
        if admission_facts_targets != context.admission_facts_targets {
            restart_required.push("admission_facts_targets".to_string());
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
            self.capability_evaluator.preview_removal_warnings(
                resulting_modules,
                &removed,
                &registrations,
            )
        } else {
            Vec::new()
        };
        let result = match self
            .reconcile_supervised_modules(&context.supervisor, modules, preview)
            .await
        {
            Ok(result) => result,
            Err(message) => {
                return Ok(vec![control_error_frame(&frame, "rescan_failed", message)?])
            }
        };
        if !preview {
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

    async fn handle_supervisor_release_reserved(
        &self,
        frame: Frame,
        module_id: String,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(context) = self.rescan.clone() else {
            return Ok(vec![control_error_frame(
                &frame,
                "release_unavailable",
                "reserved-id release requires a daemon started with a reloadable config path",
            )?]);
        };
        let operation_lock = self.supervisor.operation_lock();
        let _operation_guard = operation_lock.lock().await;
        let loaded = match crate::daemon_config::load(&context.config_path) {
            Ok(Some(config)) => config,
            Ok(None) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_daemon_config",
                    format!(
                        "daemon config not found at {}; refusing to release reserved module_id '{module_id}'",
                        context.config_path.display()
                    ),
                )?])
            }
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_daemon_config",
                    format!("unable to verify reserved-id release against daemon config: {err}"),
                )?])
            }
        };
        if loaded
            .modules
            .iter()
            .any(|configured| configured.module_id == module_id)
        {
            return Ok(vec![control_error_frame(
                &frame,
                "reserved_module_configured",
                format!(
                    "module_id '{module_id}' remains configured; remove its config entry and rescan before releasing its reserved id"
                ),
            )?]);
        }
        if !self.supervisor.release_retained_reserved_gate(&module_id) {
            return Ok(vec![control_error_frame(
                &frame,
                "reserved_gate_not_retained",
                format!(
                    "module_id '{module_id}' has no retired reserved-id gate to release; rescan its removed reserved configuration first"
                ),
            )?]);
        }

        let response = ClientControlResponse::SupervisorAck {
            module_id,
            applied: true,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }

    /// Reconcile the running module set against the configured one.
    ///
    /// With `preview` set, the diff is computed and returned WITHOUT applying any
    /// of it: nothing is retired, reconfigured, enabled or spawned. The preview
    /// deliberately shares this function with the executing path rather than
    /// computing the same diff somewhere else -- two implementations of one
    /// decision agree until they do not, and the whole value of a preview is that
    /// it describes the operation that will actually run.
    async fn reconcile_supervised_modules(
        &self,
        supervisor: &Supervisor,
        configured_modules: Vec<crate::daemon_config::ConfiguredModule>,
        preview: bool,
    ) -> Result<SupervisorRescanResult, String> {
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
        let mut configuration_changes = BTreeSet::new();
        let mut enabled_changes = BTreeSet::new();
        let mut unchanged = 0_u32;

        for (module_id, configured_module) in &configured {
            let Some((_, current_spec, current_health, current_enabled)) = current.get(module_id)
            else {
                continue;
            };
            let configuration_changed = *current_spec != configured_module.module_spec()
                || *current_health != configured_module.health;
            let enabled_changed = *current_enabled != configured_module.enabled;
            if configuration_changed {
                configuration_changes.insert(module_id.clone());
                changed_pending_reload.push(module_id.clone());
            }
            if enabled_changed {
                enabled_changes.insert(module_id.clone());
            }
            if !configuration_changed && !enabled_changed {
                unchanged = unchanged.saturating_add(1);
            }
        }

        // Everything above this point is pure computation over two snapshots.
        // Everything below MUTATES. The preview returns here so the boundary is a
        // single early return rather than a condition repeated at each mutation
        // site, where one missed guard would apply part of a change the caller was
        // told would not happen.
        if preview {
            return Ok(SupervisorRescanResult {
                added,
                removed,
                changed_pending_reload,
                enabled_changes: enabled_changes.iter().cloned().collect(),
                unchanged,
                preview: true,
                // Filled by the caller on both paths, so the preview reports
                // restart-required sections identically to an executed rescan --
                // the preview is where an operator is most likely to be looking.
                restart_required: Vec::new(),
                capability_warnings: Vec::new(),
            });
        }

        for module_id in &removed {
            let module = &current
                .get(module_id)
                .expect("removed module came from current supervisor state")
                .0;
            module.retire().await.map_err(|err| {
                format!("failed to retire module_id '{module_id}' during rescan: {err}")
            })?;
            self.supervisor.retire(module_id);
            self.supervisor.record_rescan_removal(module_id);
        }

        for module_id in configured.keys() {
            let Some((module, _, _, _)) = current.get(module_id) else {
                continue;
            };
            let configured_module = configured
                .get(module_id)
                .expect("configured module id came from configured map");
            if configuration_changes.contains(module_id) {
                module
                    .update_configuration(
                        configured_module.module_spec(),
                        configured_module.health,
                        configured_module.drain_timeout_ms,
                    )
                    .await
                    .map_err(|err| {
                        format!(
                            "failed to update module_id '{module_id}' configuration during rescan: {err}"
                        )
                    })?;
            }
            if enabled_changes.contains(module_id) {
                module
                    .set_enabled(configured_module.enabled)
                    .await
                    .map_err(|err| {
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
                .supervise_configured_with_health(
                    configured_module.module_spec(),
                    configured_module.enabled,
                    configured_module.health,
                    configured_module.drain_timeout_ms,
                )
                .map_err(|err| {
                    format!("failed to add module_id '{module_id}' during rescan: {err}")
                })?;
        }

        Ok(SupervisorRescanResult {
            added,
            removed,
            changed_pending_reload,
            enabled_changes: enabled_changes.iter().cloned().collect(),
            unchanged,
            preview: false,
            // Filled by the caller, which is the only layer that can see the
            // previous config to diff against.
            restart_required: Vec::new(),
            capability_warnings: Vec::new(),
        })
    }

    async fn handle_supervisor_set_enabled(
        &self,
        frame: Frame,
        module_id: String,
        enabled: bool,
    ) -> Result<Vec<Frame>, RouterError> {
        let operation_lock = self.supervisor.operation_lock();
        let _operation_guard = operation_lock.lock().await;
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        let applied = match module.set_enabled(enabled).await {
            Ok(applied) => applied,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "target_unavailable",
                    format!("failed to set module_id '{module_id}' enabled={enabled}: {err}"),
                )?])
            }
        };

        self.capability_evaluator.wake_deadline_loop();
        self.refresh_capability_requirements();
        let response = ClientControlResponse::SupervisorAck { module_id, applied };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }

    async fn handle_supervisor_health_probe(
        &self,
        frame: Frame,
        module_id: String,
    ) -> Result<Vec<Frame>, RouterError> {
        self.refresh_capability_requirements();
        let Some(registration) = self
            .registry
            .get_module(&module_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
        else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not registered"),
            )?]);
        };

        // This guard's ACCEPT direction is fenced, but only INCIDENTALLY: no test is
        // named for it. Making `module_registration_grants_op` return false
        // unconditionally reddens five tests, and every one is named for something
        // else -- capability relay, probe/bind demultiplexing, supervision-only
        // probing. They exercise a successful advertisement check on the way to their
        // own subject.
        //
        // Real protection, fragile in a specific way: narrowing any of those tests to
        // focus on its stated subject would silently remove coverage nobody knows
        // they are carrying. Recorded here rather than as a sixth test, because the
        // useful fact is WHICH tests hold the guard up -- a new test would add
        // coverage without telling the next person what the existing ones quietly do.
        if !module_registration_grants_op(&registration.control_ops, MODULE_CONTROL_OP_HEALTH_CHECK)
        {
            return Ok(vec![control_error_frame(
                &frame,
                "health_not_advertised",
                format!("module_id '{module_id}' did not advertise health.check"),
            )?]);
        }

        let deadline = Instant::now() + self.health_probe_timeout;
        let pending = match self.forwarding.begin_module_control_rpc_for(
            &module_id,
            MODULE_CONTROL_OP_HEALTH_CHECK,
            deadline,
        ) {
            Ok(pending) => pending,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    forwarding_error_code(&err),
                    err.to_string(),
                )?])
            }
        };

        let PendingModuleControlRpc {
            endpoint,
            module_sink,
            negotiated_ver,
            corr: probe_corr,
            receiver,
        } = pending;
        let mut guard =
            ModuleControlRpcGuard::new(Arc::clone(&self.forwarding), endpoint, probe_corr);
        let probe_body =
            serde_json::to_vec(&ModuleControlRequest::HealthCheck {}).map_err(|err| {
                RouterError::backend(
                    0,
                    frame.header.corr,
                    format!("failed to encode health.check request: {err}"),
                )
            })?;
        let probe_frame = Frame::build_with_version(
            negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            probe_corr,
            probe_body,
        )
        .map_err(RouterError::FrameBuild)?;

        if let Err(err) = module_sink.send(probe_frame).await {
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                err.to_string(),
            )?]);
        }

        match timeout_at(deadline, receiver).await {
            Ok(Ok(ModuleControlRpcOutcome::Response(response))) => {
                guard.disarm();
                let Some(report) = response.health_report() else {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "invalid_control_body",
                        "health.check RPC returned a non-health response",
                    )?]);
                };
                // Metrics go out whole here. The supervisor's cached snapshot
                // caps this blob (see truncate_health_metrics), and this path
                // exists precisely to answer without that cap -- so applying it
                // here would leave no way to see what the cached view drops.
                let HealthReport {
                    status,
                    detail,
                    metrics,
                } = report;
                let capability_detail = self
                    .capability_evaluator
                    .required_problem_detail(&module_id);
                let response = ClientControlResponse::SupervisorHealthProbe {
                    module_id,
                    status,
                    detail: append_capability_problem_detail(detail, capability_detail),
                    metrics,
                };
                Ok(vec![control_response_body_frame(
                    &frame,
                    &response,
                    "ClientControlResponse::SupervisorHealthProbe",
                )?])
            }
            Ok(Ok(ModuleControlRpcOutcome::Rejected(body))) => {
                guard.disarm();
                Ok(vec![control_error_body_frame(&frame, body)?])
            }
            Ok(Ok(ModuleControlRpcOutcome::ModuleGone(message))) => {
                guard.disarm();
                Ok(vec![control_error_frame(
                    &frame,
                    "target_unavailable",
                    message,
                )?])
            }
            Ok(Ok(ModuleControlRpcOutcome::MalformedResponse(message))) => {
                guard.disarm();
                Ok(vec![control_error_frame(
                    &frame,
                    "invalid_control_body",
                    message,
                )?])
            }
            Ok(Ok(ModuleControlRpcOutcome::UnexpectedOp { expected, actual })) => {
                guard.disarm();
                Ok(vec![control_error_frame(
                    &frame,
                    "invalid_control_body",
                    format!("expected module-control op '{expected}', got '{actual}'"),
                )?])
            }
            Ok(Ok(ModuleControlRpcOutcome::DeadlineElapsed)) => {
                guard.disarm();
                Ok(vec![control_error_frame(
                    &frame,
                    "module_timeout",
                    format!(
                        "module_id '{module_id}' answered health.check after {:?}",
                        self.health_probe_timeout
                    ),
                )?])
            }
            Ok(Err(_)) => Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                "health.check waiter was canceled before the module responded",
            )?]),
            Err(_) => Ok(vec![control_error_frame(
                &frame,
                "module_timeout",
                format!(
                    "module_id '{module_id}' did not answer health.check within {:?}",
                    self.health_probe_timeout
                ),
            )?]),
        }
    }

    fn supervisor_status(
        &self,
        module_id: &str,
        corr: u64,
    ) -> Result<Option<(crate::supervise::ModuleStatus, bool)>, RouterError> {
        self.supervisor
            .get(module_id)
            .map(|module| {
                let warming = module.is_warming_for_control("status").map_err(|err| {
                    RouterError::backend(
                        0,
                        corr,
                        format!(
                            "failed to read supervisor warming state for module_id '{module_id}': {err}"
                        ),
                    )
                })?;
                module.status_for_control("status").map_err(|err| {
                    RouterError::backend(
                        0,
                        corr,
                        format!(
                            "failed to read supervisor status for module_id '{module_id}': {err}"
                        ),
                    )
                }).map(|status| (status, warming))
            })
            .transpose()
    }

    fn guard_module_control_op(
        &self,
        frame: &Frame,
        module_id: &str,
        op: &str,
    ) -> Result<Option<Frame>, RouterError> {
        if self.module_grants_op(module_id, op, frame.header.corr)? {
            return Ok(None);
        }

        Ok(Some(control_error_frame(
            frame,
            "op_not_allowed",
            format!("module_id '{module_id}' did not grant control op '{op}'"),
        )?))
    }

    fn module_grants_op(&self, module_id: &str, op: &str, corr: u64) -> Result<bool, RouterError> {
        let Some(registration) = self
            .registry
            .get_module(module_id)
            .map_err(|err| RouterError::backend(0, corr, err.to_string()))?
        else {
            return Ok(false);
        };
        Ok(module_registration_grants_op(&registration.control_ops, op))
    }

    fn handle_status_update(
        &self,
        endpoint: ModuleEndpointId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let update = match serde_json::from_slice::<ModuleControlPush>(&frame.body) {
            Ok(update) => update,
            Err(err) => {
                // Forward-compat: a newer module may push a channel-0 op this subc
                // version doesn't know. The control contract says unknown push ops
                // are IGNORED, never answered with an error. Only a malformed body
                // for an op we DO know is a real error worth surfacing.
                if is_known_module_push_op(&frame.body) {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "invalid_control_body",
                        format!("malformed module control push body: {err}"),
                    )?]);
                }
                return Ok(Vec::new());
            }
        };

        match update {
            ModuleControlPush::RouteStatus {
                route_channel,
                route_epoch,
                status,
            } => {
                self.forwarding
                    .cache_status(endpoint, route_channel, route_epoch, status)
                    .map_err(RouterError::Forwarding)?;
            }
        }
        Ok(Vec::new())
    }

    fn handle_route_poll(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        route_channel: u16,
        route_epoch: u32,
        kind: PollKind,
    ) -> Result<Vec<Frame>, RouterError> {
        let snapshot = self
            .forwarding
            .route_poll_snapshot(ctx.connection_id, route_channel, route_epoch)
            .map_err(RouterError::Forwarding)?;
        let response = match (kind, snapshot) {
            (PollKind::Status, RoutePollSnapshot::Bound { status, .. }) => {
                ClientControlResponse::RoutePoll {
                    route_channel,
                    route_epoch,
                    status,
                    live: None,
                }
            }
            (PollKind::Status, RoutePollSnapshot::Absent) => ClientControlResponse::RoutePoll {
                route_channel,
                route_epoch,
                status: None,
                live: None,
            },
            (PollKind::Liveness, RoutePollSnapshot::Bound { module_id, .. }) => {
                // ABSENCE HERE MEANS "NOT SUPERVISED", NOT "UNKNOWN", and that
                // is what makes reporting `true` correct rather than a
                // confident guess. `process_live` returns None only when the
                // module id has no supervisor snapshot at all -- an
                // externally-started module the daemon did not spawn -- and
                // for those the supervisor has no opinion to offer, ever. It
                // is never None for a supervised module in an unknown state:
                // a supervised module always has a snapshot, and the answer
                // comes from `state == Running && process_alive`.
                //
                // The route is Bound, so the module completed a HELLO on a
                // live connection; "the process this route points at is
                // running" is therefore attested by the binding rather than
                // assumed. Reporting `false` for an unsupervised module would
                // be the actual lie -- it would tell a client its healthy
                // route is dead because the daemon does not manage the
                // process.
                //
                // IF `process_live` EVER GAINS A THIRD CASE -- a supervised
                // module whose liveness is genuinely unknown, e.g. a snapshot
                // that has not been populated yet -- THIS DEFAULT BECOMES
                // WRONG and must split: unsupervised stays true, unknown
                // becomes null so the client can tell the two apart. The
                // response field is already `Option<bool>`, so the wire can
                // carry that distinction today.
                let live = self
                    .process_liveness
                    .as_ref()
                    .and_then(|source| source.process_live(&module_id))
                    .unwrap_or(true);
                ClientControlResponse::RoutePoll {
                    route_channel,
                    route_epoch,
                    status: None,
                    live: Some(live),
                }
            }
            (PollKind::Liveness, RoutePollSnapshot::Absent) => ClientControlResponse::RoutePoll {
                route_channel,
                route_epoch,
                status: None,
                live: Some(false),
            },
        };

        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::RoutePoll",
        )?])
    }

    pub(crate) fn observe_module_control_completion(
        &self,
        completion: ModuleControlRpcCompletion,
    ) -> bool {
        match completion {
            ModuleControlRpcCompletion::Unknown => false,
            ModuleControlRpcCompletion::Settled => true,
            ModuleControlRpcCompletion::LateHealthAnswer { module_id, latency } => {
                let latency_ms = latency.as_millis().min(u128::from(u64::MAX)) as u64;
                info!(
                    module_id = %module_id,
                    latency_ms,
                    "late health.check answer proves the module is alive"
                );
                match self
                    .supervisor
                    .record_late_health_answer(&module_id, latency_ms)
                {
                    Ok(true) => {}
                    Ok(false) => debug!(
                        module_id = %module_id,
                        latency_ms,
                        "late health.check answer has no active supervisor snapshot"
                    ),
                    Err(err) => warn!(
                        module_id = %module_id,
                        latency_ms,
                        error = %err,
                        "failed to record late health.check answer"
                    ),
                }
                true
            }
        }
    }

    fn handle_module_relay_response(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let mut secondary_error = None;
        let outcome = match frame.header.ty {
            FrameType::Response => match serde_json::from_slice::<ControlOpProbe>(&frame.body) {
                Ok(probe) if probe.op == "route.bind" => {
                    match serde_json::from_slice::<ModuleControlResponse>(&frame.body) {
                        Ok(ModuleControlResponse::RouteBindAck {}) => {
                            RouteBindRelayOutcome::Accepted
                        }
                        Ok(other) => {
                            let message =
                                format!("route.bind response carried unexpected body: {other:?}");
                            secondary_error = Some(control_error_frame(
                                &frame,
                                "invalid_control_body",
                                message.clone(),
                            )?);
                            RouteBindRelayOutcome::ModuleGone(message)
                        }
                        Err(err) => {
                            let message = format!("malformed route.bind response body: {err}");
                            secondary_error = Some(control_error_frame(
                                &frame,
                                "invalid_control_body",
                                message.clone(),
                            )?);
                            RouteBindRelayOutcome::ModuleGone(message)
                        }
                    }
                }
                Ok(probe) => {
                    let outcome = match serde_json::from_slice::<ModuleControlResponse>(&frame.body)
                    {
                        Ok(response) => ModuleControlRpcOutcome::Response(response),
                        Err(err) => ModuleControlRpcOutcome::MalformedResponse(format!(
                            "malformed {} response body: {err}",
                            probe.op
                        )),
                    };
                    let completion = self
                        .forwarding
                        .complete_module_control_rpc(
                            connection_id,
                            frame.header.corr,
                            Some(&probe.op),
                            outcome,
                        )
                        .map_err(RouterError::Forwarding)?;
                    if !self.observe_module_control_completion(completion) {
                        debug!(
                            connection_id = connection_id.get(),
                            corr = frame.header.corr,
                            op = %probe.op,
                            "dropping late or unknown module-control RPC response"
                        );
                    }
                    return Ok(Vec::new());
                }
                Err(err) => {
                    if let Some(expected_op) = self
                        .forwarding
                        .pending_module_control_op(connection_id, frame.header.corr)
                        .map_err(RouterError::Forwarding)?
                    {
                        let completion = self
                            .forwarding
                            .complete_module_control_rpc(
                                connection_id,
                                frame.header.corr,
                                None,
                                ModuleControlRpcOutcome::MalformedResponse(format!(
                                    "malformed {expected_op} response body: {err}"
                                )),
                            )
                            .map_err(RouterError::Forwarding)?;
                        if !self.observe_module_control_completion(completion) {
                            debug!(
                                connection_id = connection_id.get(),
                                corr = frame.header.corr,
                                "dropping late malformed module-control RPC response"
                            );
                        }
                        return Ok(Vec::new());
                    }
                    let message = format!("malformed route.bind response body: {err}");
                    secondary_error = Some(control_error_frame(
                        &frame,
                        "invalid_control_body",
                        message.clone(),
                    )?);
                    RouteBindRelayOutcome::ModuleGone(message)
                }
            },
            FrameType::Error => {
                if self
                    .forwarding
                    .pending_module_control_op(connection_id, frame.header.corr)
                    .map_err(RouterError::Forwarding)?
                    .is_some()
                {
                    let outcome = match serde_json::from_slice::<ErrorBody>(&frame.body) {
                        Ok(body) => ModuleControlRpcOutcome::Rejected(body),
                        Err(err) => ModuleControlRpcOutcome::MalformedResponse(format!(
                            "malformed module-control ERROR body: {err}"
                        )),
                    };
                    let completion = self
                        .forwarding
                        .complete_module_control_rpc(
                            connection_id,
                            frame.header.corr,
                            None,
                            outcome,
                        )
                        .map_err(RouterError::Forwarding)?;
                    if !self.observe_module_control_completion(completion) {
                        debug!(
                            connection_id = connection_id.get(),
                            corr = frame.header.corr,
                            "dropping late or unknown module-control RPC error"
                        );
                    }
                    return Ok(Vec::new());
                }
                match serde_json::from_slice::<ErrorBody>(&frame.body) {
                    Ok(body) => RouteBindRelayOutcome::Rejected(body),
                    Err(err) => {
                        let message = format!("malformed route.bind ERROR body: {err}");
                        secondary_error = Some(control_error_frame(
                            &frame,
                            "invalid_control_body",
                            message.clone(),
                        )?);
                        RouteBindRelayOutcome::ModuleGone(message)
                    }
                }
            }
            ty => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "unsupported_control_frame",
                    format!("unsupported module channel-0 frame {ty:?}"),
                )?])
            }
        };

        let completion = self
            .forwarding
            .complete_pending_relay(connection_id, frame.header.corr, outcome)
            .map_err(RouterError::Forwarding)?;
        if let Some(target) = completion.abandoned.as_ref() {
            send_goodbye_target_best_effort(target, "late accepted route.bind");
        }
        if !completion.settled {
            debug!(
                connection_id = connection_id.get(),
                corr = frame.header.corr,
                frame_type = ?frame.header.ty,
                "dropping late or unknown route.bind relay response"
            );
        }
        Ok(secondary_error.into_iter().collect())
    }

    fn handle_goodbye(&self, connection_id: ConnectionId) -> Result<Vec<Frame>, RouterError> {
        debug!(connection_id = connection_id.get(), "handling GOODBYE");
        let registrations = self
            .deregister_connection(connection_id)
            .map_err(|err| RouterError::backend(0, 0, err.to_string()))?;
        let released_routes = self
            .forwarding
            .cleanup_connection(connection_id)
            .map_err(RouterError::Forwarding)?;
        self.emit_route_goodbyes(released_routes);
        // Notify only after forwarding teardown completes (see cleanup_connection).
        if !registrations.is_empty() {
            crate::supervise::notify_registration_release();
        }
        Ok(Vec::new())
    }
}

impl Default for ControlHandler {
    fn default() -> Self {
        Self::new(Arc::new(Registry::default()))
    }
}

fn capability_requirement_status(status: RequirementStatus) -> CapabilityRequirementStatus {
    CapabilityRequirementStatus {
        consumer: status.consumer,
        capability: status.capability,
        need: match status.need {
            subc_protocol::manifest::CapabilityNeed::Required => "required".to_string(),
            subc_protocol::manifest::CapabilityNeed::Optional => "optional".to_string(),
        },
        verdict: status.verdict.as_str().to_string(),
        episode_seq: status.episode_seq,
        config_satisfiable: status.config_satisfiable,
        runtime_available: status.runtime_available,
        detail: status.detail,
    }
}

fn append_capability_problem_detail(
    detail: Option<String>,
    capability_detail: Option<String>,
) -> Option<String> {
    match (detail, capability_detail) {
        (Some(detail), Some(capability_detail)) => Some(format!("{detail}; {capability_detail}")),
        (Some(detail), None) => Some(detail),
        (None, Some(capability_detail)) => Some(capability_detail),
        (None, None) => None,
    }
}

fn subc_ops() -> Vec<String> {
    SUBC_CONTROL_OPS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

fn module_subc_ops() -> Vec<String> {
    SUBC_CONTROL_OPS
        .iter()
        .chain(MODULE_TO_SUBC_CONTROL_OPS.iter())
        .map(|op| (*op).to_string())
        .collect()
}

#[cfg(test)]
fn module_baseline_control_ops() -> Vec<String> {
    MODULE_BASELINE_CONTROL_OPS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

fn effective_module_control_ops(declared: Option<Vec<String>>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut effective = Vec::new();
    for op in MODULE_BASELINE_CONTROL_OPS {
        if seen.insert((*op).to_string()) {
            effective.push((*op).to_string());
        }
    }
    for op in declared.unwrap_or_default() {
        if seen.insert(op.clone()) {
            effective.push(op);
        }
    }
    effective
}

fn module_registration_grants_op(control_ops: &[String], op: &str) -> bool {
    MODULE_BASELINE_CONTROL_OPS.contains(&op) || control_ops.iter().any(|granted| granted == op)
}

fn target_module_id(target: &RouteTarget) -> &str {
    match target {
        RouteTarget::ToolProvider { module_id }
        | RouteTarget::ManagementSurface { module_id }
        | RouteTarget::InternalService { module_id, .. } => module_id,
    }
}

fn target_has_required_role(target: &RouteTarget, roles: &[ProviderRole]) -> bool {
    roles.iter().any(|role| match (target, role) {
        (RouteTarget::ToolProvider { .. }, ProviderRole::ToolProvider { .. }) => true,
        (RouteTarget::ManagementSurface { .. }, ProviderRole::ManagementSurface { .. }) => true,
        (
            RouteTarget::InternalService { service_id, .. },
            ProviderRole::InternalService {
                service_id: provided,
                ..
            },
        ) => service_id == provided,
        _ => false,
    })
}

fn is_routable_role(role: &ProviderRole) -> bool {
    matches!(
        role,
        ProviderRole::ToolProvider { .. }
            | ProviderRole::ManagementSurface { .. }
            | ProviderRole::InternalService { .. }
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlRequestBodyError {
    UnknownOp,
    InvalidBody,
}

#[derive(Debug, Deserialize)]
struct ControlOpProbe {
    op: String,
}

/// Channel-0 push ops this subc version understands. A push whose `op` is not in
/// this set is treated as a forward-compat unknown and ignored rather than errored.
const MODULE_PUSH_OPS: &[&str] = &["route.status"];

fn is_known_module_push_op(body: &[u8]) -> bool {
    serde_json::from_slice::<ControlOpProbe>(body)
        .map(|probe| MODULE_PUSH_OPS.contains(&probe.op.as_str()))
        .unwrap_or(false)
}

fn is_known_module_request_op(body: &[u8]) -> bool {
    serde_json::from_slice::<ControlOpProbe>(body)
        .map(|probe| MODULE_TO_SUBC_CONTROL_OPS.contains(&probe.op.as_str()))
        .unwrap_or(false)
}

fn log_control_dispatch_arrival(op: &'static str, connection_id: ConnectionId, corr: u64) {
    debug!(
        op = %op,
        connection_id = connection_id.get(),
        corr,
        "control dispatch"
    );
}

fn log_slow_control_dispatch(
    dispatch_started_at: Option<StdInstant>,
    op: &'static str,
    connection_id: ConnectionId,
    corr: u64,
) {
    let Some(dispatch_started_at) = dispatch_started_at else {
        return;
    };
    let elapsed = dispatch_started_at.elapsed();
    if elapsed >= SLOW_CONTROL_DISPATCH_THRESHOLD {
        warn!(
            op = %op,
            connection_id = connection_id.get(),
            corr,
            elapsed_ms = elapsed.as_millis() as u64,
            "slow control dispatch"
        );
    }
}

fn client_control_request_op(request: &ClientControlRequest) -> &'static str {
    match request {
        ClientControlRequest::ServerDescribe {} => ops::SERVER_DESCRIBE,
        ClientControlRequest::CatalogList { .. } => ops::CATALOG_LIST,
        ClientControlRequest::RouteOpen { .. } => ops::ROUTE_OPEN,
        ClientControlRequest::RoutePoll { .. } => ops::ROUTE_POLL,
        ClientControlRequest::SupervisorList {} => ops::SUPERVISOR_LIST,
        ClientControlRequest::SupervisorRestart { .. } => ops::SUPERVISOR_RESTART,
        ClientControlRequest::SupervisorReload { .. } => ops::SUPERVISOR_RELOAD,
        ClientControlRequest::SupervisorRescan { .. } => ops::SUPERVISOR_RESCAN,
        ClientControlRequest::SupervisorReleaseReserved { .. } => ops::SUPERVISOR_RELEASE_RESERVED,
        ClientControlRequest::SupervisorSetEnabled { .. } => ops::SUPERVISOR_SET_ENABLED,
        ClientControlRequest::SupervisorHealthProbe { .. } => ops::SUPERVISOR_HEALTH_PROBE,
        ClientControlRequest::SupervisorHealth {} => ops::SUPERVISOR_HEALTH,
        ClientControlRequest::SupervisorRoutes { .. } => ops::SUPERVISOR_ROUTES,
        ClientControlRequest::SupervisorStderrTail { .. } => ops::SUPERVISOR_STDERR_TAIL,
        ClientControlRequest::SupervisorTerminals { .. } => ops::SUPERVISOR_TERMINALS,
    }
}

fn module_control_request_op(request: &ModuleControlRequestFromModule) -> &'static str {
    match request {
        ModuleControlRequestFromModule::CatalogUpdate { .. } => MODULE_TO_SUBC_OP_CATALOG_UPDATE,
    }
}

fn parse_client_control_request(
    body: &[u8],
) -> Result<ClientControlRequest, (serde_json::Error, ControlRequestBodyError)> {
    serde_json::from_slice::<ClientControlRequest>(body).map_err(|err| {
        let classification = match serde_json::from_slice::<ControlOpProbe>(body) {
            Ok(probe) if SUBC_CONTROL_OPS.contains(&probe.op.as_str()) => {
                ControlRequestBodyError::InvalidBody
            }
            Ok(_) => ControlRequestBodyError::UnknownOp,
            Err(_) => ControlRequestBodyError::InvalidBody,
        };
        (err, classification)
    })
}

fn parse_module_control_request_from_module(
    body: &[u8],
) -> Result<ModuleControlRequestFromModule, (serde_json::Error, ControlRequestBodyError)> {
    serde_json::from_slice::<ModuleControlRequestFromModule>(body).map_err(|err| {
        let classification = match serde_json::from_slice::<ControlOpProbe>(body) {
            Ok(probe) if MODULE_TO_SUBC_CONTROL_OPS.contains(&probe.op.as_str()) => {
                ControlRequestBodyError::InvalidBody
            }
            Ok(_) => ControlRequestBodyError::UnknownOp,
            Err(_) => ControlRequestBodyError::InvalidBody,
        };
        (err, classification)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ProviderRoleKind {
    ToolProvider,
    PipelineStage,
    ManagementSurface,
    InternalService,
}

fn provider_role_kind(role: &ProviderRole) -> ProviderRoleKind {
    match role {
        ProviderRole::ToolProvider { .. } => ProviderRoleKind::ToolProvider,
        ProviderRole::PipelineStage { .. } => ProviderRoleKind::PipelineStage,
        ProviderRole::ManagementSurface { .. } => ProviderRoleKind::ManagementSurface,
        ProviderRole::InternalService { .. } => ProviderRoleKind::InternalService,
    }
}

fn provider_role_kind_set(roles: &[ProviderRole]) -> BTreeSet<ProviderRoleKind> {
    roles.iter().map(provider_role_kind).collect()
}

/// Return whether a catalog change can create a newly violating live route.
/// Removing an attested claim is intentionally excluded: it makes fewer routes
/// forbidden and therefore must leave the existing route census untouched.
fn capability_census_trigger(
    old: Option<&CapabilityDeclarations>,
    new: Option<&CapabilityDeclarations>,
) -> bool {
    let old_provides = old
        .map(|capabilities| capabilities.provides.iter().collect::<HashSet<_>>())
        .unwrap_or_default();
    let old_denies = old
        .map(|capabilities| capabilities.must_never_reach.iter().collect::<HashSet<_>>())
        .unwrap_or_default();
    let new = new.cloned().unwrap_or(CapabilityDeclarations {
        provides: Vec::new(),
        requires: Vec::new(),
        must_never_reach: Vec::new(),
    });

    new.provides
        .iter()
        .any(|capability| !old_provides.contains(capability))
        || new
            .must_never_reach
            .iter()
            .any(|capability| !old_denies.contains(capability))
}

/// Find the first capability an attested opener denies that an attested target
/// claims. Both manifests are live registry records, never cached or client data.
fn denied_capability<'a>(
    opening_manifest: &'a ModuleManifest,
    target_manifest: &ModuleManifest,
) -> Option<&'a str> {
    let opening_capabilities = opening_manifest.capabilities.as_ref()?;
    let target_capabilities = target_manifest.capabilities.as_ref()?;
    opening_capabilities
        .must_never_reach
        .iter()
        .find(|denied| {
            target_capabilities
                .provides
                .iter()
                .any(|provided| provided == *denied)
        })
        .map(String::as_str)
}

fn catalog_update_frozen_field_message(
    registered: &ModuleManifest,
    provides: &[ProviderRole],
) -> Option<String> {
    let old_has_provides = !registered.provides.is_empty();
    let new_has_provides = !provides.is_empty();
    if old_has_provides != new_has_provides {
        return Some(format!(
            "catalog.update cannot change module '{}' between supervision-only and routable; routability is fixed at HELLO",
            registered.module_id
        ));
    }

    if provider_role_kind_set(&registered.provides) != provider_role_kind_set(provides) {
        return Some(format!(
            "catalog.update cannot change provider role kinds for module '{}'; role kinds are fixed at HELLO",
            registered.module_id
        ));
    }

    let registered_concurrency = manifest_concurrency(registered);
    let candidate = ModuleManifest {
        provides: provides.to_vec(),
        ..registered.clone()
    };
    let candidate_concurrency = manifest_concurrency(&candidate);
    if candidate_concurrency != registered_concurrency {
        return Some(format!(
            "catalog.update cannot change module '{}' concurrency from {:?} to {:?}; concurrency is fixed at HELLO",
            registered.module_id, registered_concurrency, candidate_concurrency
        ));
    }

    // control_ops live beside the manifest in the HELLO body, not inside
    // ModuleManifest, so a provides-only catalog.update cannot change them.
    None
}

fn manifest_provides_routable_role(manifest: &ModuleManifest) -> bool {
    manifest.provides.iter().any(is_routable_role)
}

/// Returns the routable-provider concurrency subc should enforce for this manifest.
///
/// ToolProvider and ManagementSurface store their delivery concurrency directly.
/// InternalService has no role-specific concurrency field, so it retains the
/// existing ModuleManaged default for backward compatibility.
fn manifest_concurrency(manifest: &ModuleManifest) -> Concurrency {
    manifest
        .provides
        .iter()
        .find_map(|provider| match provider {
            ProviderRole::ToolProvider { concurrency, .. }
            | ProviderRole::ManagementSurface { concurrency, .. } => Some(concurrency.clone()),
            ProviderRole::PipelineStage { .. } | ProviderRole::InternalService { .. } => None,
        })
        .unwrap_or(Concurrency::ModuleManaged)
}

/// True when the manifest carries a ManagementSurface role whose concurrency
/// was RESOLVED BY SERDE DEFAULT rather than declared. Reads the raw HELLO
/// bytes because the typed manifest deliberately erases that distinction: the
/// default exists for wire compatibility, and this probe exists so the default
/// stays observable. Any parse irregularity returns false -- the caller only
/// logs, and a malformed body already failed registration upstream.
fn manifest_concurrency_was_defaulted(raw_hello: &[u8], manifest: &ModuleManifest) -> bool {
    let has_management_surface = manifest
        .provides
        .iter()
        .any(|provider| matches!(provider, ProviderRole::ManagementSurface { .. }));
    if !has_management_surface {
        return false;
    }
    let Ok(raw) = serde_json::from_slice::<serde_json::Value>(raw_hello) else {
        return false;
    };
    let Some(provides) = raw
        .get("manifest")
        .and_then(|manifest| manifest.get("provides"))
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    // ProviderRole is internally tagged (`tag = "role"`), so the wire shape is
    // flat: {"role": "management_surface", ..., "concurrency": ...} -- verified
    // against the management_surface_manifest_without_concurrency golden, not
    // recalled (the externally-tagged guess was this function's first bug).
    provides.iter().any(|role| {
        role.get("role").and_then(serde_json::Value::as_str) == Some("management_surface")
            && role.get("concurrency").is_none()
    })
}

fn negotiate_version(peer_version: u8) -> Result<u8, String> {
    if peer_version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol_ver {peer_version} is unsupported; this daemon requires exactly {PROTOCOL_VERSION}"
        ));
    }
    Ok(PROTOCOL_VERSION)
}

fn pong(frame: &Frame) -> Result<Frame, RouterError> {
    Frame::build_with_version(
        response_version(frame),
        FrameType::Pong,
        frame.header.flags,
        0,
        0,
        frame.header.corr,
        Vec::new(),
    )
    .map_err(RouterError::FrameBuild)
}

fn control_error_frame(
    frame: &Frame,
    code: &'static str,
    message: impl Into<String>,
) -> Result<Frame, RouterError> {
    control_error_body_frame(
        frame,
        ErrorBody {
            code: code.to_string(),
            message: message.into(),
            detail: None,
        },
    )
}

fn control_error_body_frame(frame: &Frame, error: ErrorBody) -> Result<Frame, RouterError> {
    let body = serde_json::to_vec(&error).map_err(|err| {
        RouterError::backend(
            0,
            frame.header.corr,
            format!("failed to encode control ERROR: {err}"),
        )
    })?;

    Frame::build_with_version(
        response_version(frame),
        FrameType::Error,
        control_flags(),
        0,
        0,
        frame.header.corr,
        body,
    )
    .map_err(RouterError::FrameBuild)
}

fn control_response_body_frame<T: Serialize>(
    frame: &Frame,
    reply: &T,
    label: &'static str,
) -> Result<Frame, RouterError> {
    let body = serde_json::to_vec(reply).map_err(|err| {
        RouterError::backend(
            0,
            frame.header.corr,
            format!("failed to encode {label}: {err}"),
        )
    })?;

    Frame::build_with_version(
        response_version(frame),
        FrameType::Response,
        control_flags(),
        0,
        0,
        frame.header.corr,
        body,
    )
    .map_err(RouterError::FrameBuild)
}

/// Map a forwarding failure to the wire code a client sees.
///
/// The code is not a label: clients BRANCH on it. `is_retryable_route_open_code`
/// in both SDKs treats `target_unavailable`, `module_reloading`, `unknown_module`
/// and `module_timeout` as "retry in place", so a code chosen here decides
/// whether a caller retries or gives up.
///
/// That makes attribution the load-bearing property, not merely having a code. A
/// permanent fault published as a retryable one produces a fleet-wide retry storm
/// against something that can never recover; a transient fault published as
/// permanent gives up on work that would have succeeded. Both look correct in a
/// log, which is why `retryability_of_forwarding_codes_matches_the_failure` pins
/// the mapping per variant rather than merely asserting that some code exists.
///
/// That fence partitions by RETRYABILITY, which is coarser than identity: swapping
/// two codes on the same side of the boundary passes it. Measured rather than
/// assumed — `NoModuleConnection` re-pointed at `module_reloading` is caught only
/// by `supervision_only_module_health_probe_does_not_enable_route_open_and_cleans_up`,
/// a test named for something else that happens to assert the string.
///
/// That accidental coverage is deliberately left alone rather than promoted to a
/// named test, because it guards a property this function does not promise.
/// Checked at source: every consumer branches on the RETRYABLE SET and none on a
/// specific code within a class, so identity is free to change and only the
/// partition is a contract. Splitting it out would assert a guarantee nothing
/// depends on — and a suite that promises more than the code does is the harder
/// thing to correct later, because the next reader cannot tell which assertions
/// are load-bearing.
///
/// Pin identity here the moment a consumer branches on a specific code.
fn forwarding_error_code(err: &ForwardingError) -> &'static str {
    match err {
        ForwardingError::NoModuleConnection => "target_unavailable",
        ForwardingError::ModuleReloading { .. } => "module_reloading",
        ForwardingError::ClientRouteChannelExhausted { .. }
        | ForwardingError::ModuleRouteChannelExhausted { .. } => "route_limit",
        ForwardingError::StaleModuleEndpoint
        | ForwardingError::UnknownReservation { .. }
        | ForwardingError::ConnectionClosing { .. }
        | ForwardingError::ClientEgressClosed { .. } => "target_unavailable",
        ForwardingError::RelayCorrelationExhausted
        | ForwardingError::RouteOpenBuild(_)
        | ForwardingError::Poisoned => "forwarding_error",
    }
}

fn response_version(frame: &Frame) -> u8 {
    if (MIN_SUPPORTED_VERSION..=PROTOCOL_VERSION).contains(&frame.header.ver) {
        frame.header.ver
    } else {
        PROTOCOL_VERSION
    }
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

fn send_goodbye_target_best_effort(target: &GoodbyeTarget, context: &str) {
    let Ok(frame) = Frame::build_with_version(
        target.negotiated_ver,
        FrameType::Goodbye,
        control_flags(),
        target.channel,
        target.epoch,
        0,
        Vec::new(),
    ) else {
        return;
    };
    if let Err(err) = target.sink.try_send(frame) {
        warn!(
            route_channel = target.channel,
            route_epoch = target.epoch,
            error = %err,
            %context,
            "route GOODBYE dropped under backpressure"
        );
    }
}

pub(crate) fn send_route_control_pushes(
    forwarding: &ForwardingTable,
    routes: Vec<EndpointRoute>,
    push: ClientControlPush,
) {
    let body = match serde_json::to_vec(&push) {
        Ok(body) => body,
        Err(err) => {
            warn!(error = %err, "failed to serialize route lifecycle control PUSH");
            return;
        }
    };
    let mut targets = Vec::new();
    for route in routes {
        let target = route.goodbye_target;
        if let Some(existing) = targets
            .iter()
            .find(|existing: &&GoodbyeTarget| existing.connection_id == target.connection_id)
        {
            debug_assert_eq!(
                existing.negotiated_ver, target.negotiated_ver,
                "one connection cannot negotiate multiple frame versions"
            );
            continue;
        }
        targets.push(target);
    }
    for target in targets {
        let frame = match Frame::build_with_version(
            target.negotiated_ver,
            FrameType::Push,
            control_flags(),
            0,
            0,
            0,
            body.clone(),
        ) {
            Ok(frame) => frame,
            Err(err) => {
                warn!(
                    route_channel = target.channel,
                    error = %err,
                    "failed to build route lifecycle control PUSH frame"
                );
                continue;
            }
        };
        if let Err(err) = target.sink.try_send(frame) {
            if target.close_on_delivery_failure() {
                warn!(
                    target_connection_id = target.connection_id.get(),
                    route_channel = target.channel,
                    error = %err,
                    "route lifecycle control PUSH was not delivered to client; closing target connection"
                );
                let _ = forwarding.escalate_client_delivery_failure(
                    target.connection_id,
                    target.channel,
                    target.epoch,
                    CloseReason::new(
                        "route_lifecycle_push_delivery_failed",
                        format!(
                            "failed to enqueue route lifecycle control PUSH for channel {}: {err}",
                            target.channel
                        ),
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use serde_json::{json, Value};
    use subc_protocol::{
        manifest::{
            Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope,
            ManagementOperation, ManagementOperationKind, ObservabilityKind, ObservabilitySurface,
            ProviderRole, StorageBinding, StorageKind, StorageScope, Tool,
        },
        session::HealthStatus,
        FrameType,
    };

    use super::*;
    use crate::{
        registry::ChannelState,
        router::FrameSink,
        stderr_tail::DEFAULT_MAX_LINE_BYTES,
        supervise::{ModuleSpec, ModuleState, RestartPolicy, Supervisor, SupervisorHandle},
        RouteCtx, Router,
    };
    use tokio::{
        sync::mpsc,
        time::{sleep, Instant},
    };

    /// Locates the `fake-aft-stub` binary from a `src/lib.rs` unit test.
    ///
    /// `CARGO_BIN_EXE_*` (compile-time `env!` and runtime `std::env::var` alike)
    /// is only populated for `tests/*.rs` integration test binaries -- this file
    /// compiles as part of the library target, which gets neither. This test's
    /// own executable path is `<target-dir>/<profile>/deps/subc_core-<hash>`,
    /// and the sibling binary lives two directories up at
    /// `<target-dir>/<profile>/fake-aft-stub`.
    ///
    /// THE BINARY IS NOT ALWAYS THERE, and the existence check below is why.
    /// `cargo test -p subc-core` builds every target including `[[bin]]`, so the
    /// stub is on disk; `cargo test -p subc-core --lib` builds ONLY the library
    /// test and leaves the stub unbuilt. A bare spawn then fails with a raw
    /// `NotFound`, which reads as a broken test rather than an unbuilt
    /// dependency -- so state the cause and the remedy instead. Deliberately a
    /// panic and not a silent skip: a test that quietly passes when it could not
    /// run is worse than one that fails, because it reports health it never
    /// verified.
    fn fake_aft_stub_path() -> PathBuf {
        let mut path = std::env::current_exe().expect("current_exe available in tests");
        path.pop(); // .../deps/
        path.pop(); // .../<profile>/
        path.push(if cfg!(windows) {
            "fake-aft-stub.exe"
        } else {
            "fake-aft-stub"
        });
        assert!(
            path.exists(),
            "fake-aft-stub not built at {}: run `cargo test -p subc-core` (which builds \
             [[bin]] targets) rather than `cargo test -p subc-core --lib` (which does not)",
            path.display()
        );
        path
    }

    /// The retryable set both SDKs branch on, kept byte-identical to
    /// `is_retryable_route_open_code` in subc-client-rs and subc-client.
    const CLIENT_RETRYABLE: &[&str] = &[
        "unknown_module",
        "module_reloading",
        "target_unavailable",
        "module_timeout",
    ];

    /// A code is not a label — clients branch on it, so publishing the wrong KIND
    /// of failure is worse than publishing none. A permanent fault dressed as
    /// retryable makes every client in the fleet retry forever against something
    /// that cannot recover; a transient fault dressed as permanent abandons work
    /// that would have succeeded.
    ///
    /// Asserting "a code exists" cannot catch either, because the string is free
    /// to say anything. This enumerates every variant and pins which side of the
    /// retry boundary it lands on, so a new variant must be classified here
    /// deliberately rather than inheriting whichever arm it was appended to.
    #[test]
    fn retryability_of_forwarding_codes_matches_the_failure() {
        // Transient by nature: the target is booting, reloading, or its endpoint
        // was swapped mid-flight. Retrying is how these resolve.
        let transient = [
            ForwardingError::NoModuleConnection,
            ForwardingError::ModuleReloading {
                module_id: "m".into(),
            },
            ForwardingError::StaleModuleEndpoint,
            ForwardingError::UnknownReservation {
                client_channel: 1,
                module_channel: 1,
            },
            ForwardingError::ConnectionClosing {
                connection_id: ConnectionId::new(1),
            },
            ForwardingError::ClientEgressClosed {
                connection_id: ConnectionId::new(1),
            },
        ];
        for err in transient {
            let code = forwarding_error_code(&err);
            assert!(
                CLIENT_RETRYABLE.contains(&code),
                "{err:?} is transient but publishes {code:?}, which clients treat as permanent"
            );
        }

        // Not fixed by retrying. Channel and correlation exhaustion need the
        // caller to close routes, and a poisoned lock is a daemon that cannot
        // recover at all — the worst thing to advertise as retryable, since every
        // client would storm a daemon that will never answer.
        let permanent = [
            ForwardingError::ClientRouteChannelExhausted {
                connection_id: ConnectionId::new(1),
            },
            ForwardingError::ModuleRouteChannelExhausted {
                endpoint: ModuleEndpointId {
                    connection_id: ConnectionId::new(1),
                    generation: 1,
                },
            },
            ForwardingError::RelayCorrelationExhausted,
            ForwardingError::RouteOpenBuild("x".into()),
            ForwardingError::Poisoned,
        ];
        for err in permanent {
            let code = forwarding_error_code(&err);
            assert!(
                !CLIENT_RETRYABLE.contains(&code),
                "{err:?} cannot be fixed by retrying but publishes {code:?}, which clients retry"
            );
        }
    }

    /// The principal is the daemon's answer to "who is calling", and modules
    /// branch on it: aft gates bash on it, cerebellum gates browser control,
    /// plexus gates connector invocation. So a stamp is an authorization input in
    /// another process, not a label — and both possible answers SUCCEED, which is
    /// what makes a wrong one quiet. An unattested caller stamped `Reserved` hands
    /// first-party capability to something that never proved it; a supervised one
    /// stamped `Direct` silently strips a module of capability it is entitled to.
    ///
    /// Neither shows up in a test that only checks the bind succeeded. Before this
    /// test the only coverage was accidental —
    /// `route_open_round_trip_via_tagged_shape_forwards_through_stub` asserts the
    /// stamped principal on its way past, so narrowing that wire-shape test to its
    /// stated subject would have deleted the last assertion on this value. It
    /// still asserts the stamp, which is now redundancy rather than the only
    /// guard: both fail under the same mutation, and this one names the reason.
    /// SCOPE: this handler's supervisor has spawned nothing, so
    /// `spawned_consumer_authorized` can only ever return false and the GRANT arm
    /// is unreachable here. Both assertions below are refusals, and a mutant that
    /// refuses everything would satisfy them.
    ///
    /// The grant side is covered where a real nonce exists: `tests/forwarding.rs`
    /// spawns a supervised consumer, reads its live nonce, and asserts the module
    /// observed `principal.kind == "reserved"` carrying that module_id — verified
    /// at source rather than assumed, since a citation is a claim about another
    /// file and ages like one. Recorded because a harness that structurally
    /// cannot reach an arm reports "none" for that arm identically to one that
    /// covers it and found nothing.
    #[tokio::test]
    async fn an_unattested_caller_is_never_stamped_as_a_supervised_module() {
        let handler = ControlHandler::default();
        let frame =
            Frame::build(FrameType::Request, control_flags(), 0, 0, 900, Vec::new()).unwrap();

        // Absent consumer_identity is the ordinary case: a human at a terminal, or
        // any process holding the connection file. Nothing was proved, so nothing
        // may be granted beyond the unattested floor.
        let stamped = handler.route_open_principal(&frame, None).unwrap().unwrap();
        assert_eq!(
            stamped,
            Principal::Direct,
            "a caller that proved nothing must not be stamped as a supervised module"
        );

        // A claimed module_id with a nonce no supervised child was given is a
        // forgery attempt, not a weaker caller: it must be REFUSED rather than
        // quietly demoted to Direct, or an impersonation attempt looks identical
        // to an ordinary unattested connection.
        let forged = handler
            .route_open_principal(
                &frame,
                Some(ConsumerIdentity {
                    module_id: "aft".to_string(),
                    launch_nonce: "not-a-real-nonce".to_string(),
                }),
            )
            .unwrap();
        let refusal = forged.expect_err("an unmatched launch nonce must not yield a principal");
        assert_eq!(parse_error(&refusal)["code"], "bad_consumer_identity");
    }

    /// The test above hands `route_open_principal` an identity it built itself,
    /// which proves the stamping rule and nothing about where the identity comes
    /// from. The real producer is a wire body, and the two are joined by a serde
    /// field name that nothing else asserts.
    ///
    /// That join fails quietly in one specific way: an unrecognised key is simply
    /// absent after parsing, so a renamed or misspelled `consumer_identity`
    /// yields `None` and every supervised module silently drops to `Direct`.
    /// Capability-wise that is the safe direction, but it surfaces far from its
    /// cause — as a module mysteriously refused bash — and it would pass every
    /// test that builds its own input.
    ///
    /// Deliberately NOT closed with `deny_unknown_fields`: refusing unknown keys
    /// would break every client the moment the daemon gains a field, trading a
    /// quiet demotion for a hard refusal on additive change. Asserting the join
    /// instead means a rename breaks a test here rather than the fleet.
    #[test]
    fn a_wire_body_actually_yields_the_consumer_identity_the_daemon_stamps_from() {
        let body = br#"{"op":"route.open","target":{"kind":"tool_provider","module_id":"m"},"identity":{"session":"s","project_root":"/p","harness":"h"},"consumer_identity":{"module_id":"aft","launch_nonce":"n"}}"#;
        let parsed: ClientControlRequest = serde_json::from_slice(body).unwrap();
        let ClientControlRequest::RouteOpen {
            consumer_identity, ..
        } = parsed
        else {
            panic!("route.open body must parse as RouteOpen");
        };
        assert_eq!(
            consumer_identity,
            Some(ConsumerIdentity {
                module_id: "aft".to_string(),
                launch_nonce: "n".to_string(),
            }),
            "the wire field name must reach the value route_open_principal reads"
        );
    }

    fn manifest(module_id: &str, protocol_ver: u8) -> ModuleManifest {
        ModuleManifest {
            module_id: module_id.to_string(),
            module_version: "0.1.0".to_string(),
            protocol_ver,
            trust_tier: subc_protocol::manifest::TrustTier::FirstParty,
            provides: vec![ProviderRole::ToolProvider {
                tools: vec![Tool {
                    name: "read".to_string(),
                    description: None,
                    execution_mode: ExecutionMode::Pure,
                    schema: json!({"type": "object"}),
                }],
                identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
                concurrency: Concurrency::ModuleManaged,
                emits_push: true,
                sub_supervises: true,
            }],
            consumes: Vec::new(),
            bindings: Bindings {
                storage: StorageBinding {
                    kind: StorageKind::Sqlite,
                    scope: StorageScope::Project,
                    owns_schema: true,
                },
                vault_grants: Vec::new(),
                identity: IdentityBinding {
                    requires: vec![IdentityScope::Project],
                    optional: vec![IdentityScope::Session],
                },
            },
            capabilities: None,
            provenance: None,
        }
    }

    fn hello_frame(module_id: &str, protocol_ver: u8, corr: u64) -> Frame {
        hello_frame_with_control_ops(module_id, protocol_ver, corr, None)
    }

    fn hello_frame_with_control_ops(
        module_id: &str,
        protocol_ver: u8,
        corr: u64,
        control_ops: Option<Vec<String>>,
    ) -> Frame {
        hello_frame_full(module_id, protocol_ver, corr, control_ops, None)
    }

    fn hello_frame_with_nonce(
        module_id: &str,
        protocol_ver: u8,
        corr: u64,
        launch_nonce: Option<&str>,
    ) -> Frame {
        hello_frame_full(
            module_id,
            protocol_ver,
            corr,
            None,
            launch_nonce.map(ToOwned::to_owned),
        )
    }

    fn hello_frame_full(
        module_id: &str,
        protocol_ver: u8,
        corr: u64,
        control_ops: Option<Vec<String>>,
        launch_nonce: Option<String>,
    ) -> Frame {
        let body = serde_json::to_vec(&ModuleHelloBody {
            manifest: manifest(module_id, protocol_ver),
            protocol_ver,
            control_ops,
            launch_nonce,
        })
        .unwrap();
        Frame::build(FrameType::Hello, control_flags(), 0, 0, corr, body).unwrap()
    }

    fn non_routable_hello_frame_with_control_ops(
        module_id: &str,
        corr: u64,
        control_ops: Option<Vec<String>>,
    ) -> Frame {
        let mut manifest = manifest(module_id, PROTOCOL_VERSION);
        manifest.provides.clear();
        let body = serde_json::to_vec(&ModuleHelloBody {
            manifest,
            protocol_ver: PROTOCOL_VERSION,
            control_ops,
            launch_nonce: None,
        })
        .unwrap();
        Frame::build(FrameType::Hello, control_flags(), 0, 0, corr, body).unwrap()
    }

    fn capability_grammar_hello_frame(
        capabilities: Value,
        runtime_computed: Option<Value>,
        corr: u64,
    ) -> Frame {
        let mut body = serde_json::to_value(ModuleHelloBody {
            manifest: manifest("capability-grammar-test", PROTOCOL_VERSION),
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        })
        .expect("HELLO body serializes");
        body["manifest"]["capabilities"] = capabilities;
        if let Some(runtime_computed) = runtime_computed {
            body["runtime_computed"] = runtime_computed;
        }
        Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&body).expect("HELLO body reserializes"),
        )
        .expect("HELLO frame builds")
    }

    fn channel_request(channel: u16, corr: u64) -> Frame {
        Frame::build(
            FrameType::Request,
            Flags::new(true, Priority::Interactive, false),
            channel,
            0,
            corr,
            b"opaque".to_vec(),
        )
        .unwrap()
    }

    fn route_ctx(
        connection_id: ConnectionId,
    ) -> (RouteCtx, mpsc::Receiver<crate::router::OutboundFrame>) {
        let (tx, rx) = mpsc::channel(8);
        (
            RouteCtx {
                connection_id,
                egress: FrameSink::new(tx),
            },
            rx,
        )
    }

    fn parse_ack(frame: &Frame) -> ModuleHelloAckBody {
        serde_json::from_slice(&frame.body).unwrap()
    }

    fn parse_error(frame: &Frame) -> Value {
        serde_json::from_slice(&frame.body).unwrap()
    }

    fn parse_route_poll(frame: &Frame) -> ClientControlResponse {
        serde_json::from_slice(&frame.body).unwrap()
    }

    fn route_poll_frame(corr: u64, kind: PollKind, route_channel: u16) -> Frame {
        let body = serde_json::to_vec(&ClientControlRequest::RoutePoll {
            route_channel,
            route_epoch: 0,
            kind,
        })
        .unwrap();
        Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
    }

    fn supervisor_health_probe_frame(corr: u64, module_id: &str) -> Frame {
        let body = serde_json::to_vec(&ClientControlRequest::SupervisorHealthProbe {
            module_id: module_id.to_string(),
        })
        .unwrap();
        Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
    }

    fn route_open_frame(corr: u64, module_id: &str, project_root: std::path::PathBuf) -> Frame {
        route_open_frame_with_consumer_capabilities(corr, module_id, project_root, None)
    }

    fn route_open_frame_with_consumer_capabilities(
        corr: u64,
        module_id: &str,
        project_root: std::path::PathBuf,
        consumer_capabilities: Option<Vec<String>>,
    ) -> Frame {
        let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
            target: RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            identity: BindIdentity {
                project_root,
                harness: "unit".to_string(),
                session: "session".to_string(),
            },
            consumer_identity: None,
            consumer_capabilities,
            admission_facts: None,
        })
        .unwrap();
        Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
    }

    fn route_open_frame_with_admission_facts(
        corr: u64,
        module_id: &str,
        consumer_identity: Option<subc_control::ConsumerIdentity>,
        facts: Option<Value>,
    ) -> Frame {
        let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
            target: RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            identity: BindIdentity {
                project_root: unique_project_root("admission-facts"),
                harness: "unit".to_string(),
                session: format!("session-{corr}"),
            },
            consumer_identity,
            consumer_capabilities: None,
            admission_facts: facts,
        })
        .unwrap();
        Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
    }

    fn health_response(corr: u64, status: HealthStatus) -> Frame {
        let body = serde_json::to_vec(&ModuleControlResponse::HealthCheck {
            status,
            detail: Some("warming".to_string()),
            metrics: Some(json!({"queue_depth": 3})),
        })
        .unwrap();
        Frame::build(FrameType::Response, control_flags(), 0, 0, corr, body).unwrap()
    }

    fn route_bind_ack(corr: u64) -> Frame {
        let body = serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).unwrap();
        Frame::build(FrameType::Response, control_flags(), 0, 0, corr, body).unwrap()
    }

    fn unique_project_root(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "subc-control-{label}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn assert_route_poll_liveness(frame: &Frame, expected_live: bool) {
        match parse_route_poll(frame) {
            ClientControlResponse::RoutePoll {
                status: None,
                live: Some(live),
                ..
            } => assert_eq!(live, expected_live),
            other => panic!("unexpected route.poll response: {other:?}"),
        }
    }

    fn bind_liveness_route(
        registry: &Registry,
        forwarding: &ForwardingTable,
        module_id: &str,
    ) -> (RouteCtx, u16, u32) {
        let module_connection = ConnectionId::new(101);
        let client_connection = ConnectionId::new(202);
        let registration = registry
            .register_with_control_ops(
                manifest(module_id, PROTOCOL_VERSION),
                PROTOCOL_VERSION,
                module_connection,
                module_baseline_control_ops(),
            )
            .unwrap();
        let (module_tx, _module_rx) = mpsc::channel(8);
        let endpoint = forwarding
            .register_module_connection(
                module_connection,
                module_id.to_string(),
                PROTOCOL_VERSION,
                manifest_concurrency(&registration.manifest),
                FrameSink::new(module_tx),
            )
            .unwrap();
        let (client_ctx, _client_rx) = route_ctx(client_connection);
        let pending = forwarding
            .begin_route_bind_relay_for_test(
                client_connection,
                client_ctx.egress.clone(),
                1,
                module_id,
            )
            .unwrap();
        assert_eq!(pending.endpoint, endpoint);
        let route_channel = pending.client_channel;
        let route_epoch = pending.client_epoch;
        forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        (client_ctx, route_channel, route_epoch)
    }

    struct FakeProcessLiveness {
        live: Option<bool>,
    }

    impl ModuleProcessLiveness for FakeProcessLiveness {
        fn process_live(&self, _module_id: &str) -> Option<bool> {
            self.live
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervisor_stderr_tail_converts_a_real_truncated_ring_entry_to_prefix_only_wire_data()
    {
        let registry = Arc::new(Registry::default());
        let supervisor_handle = SupervisorHandle::new();
        let supervisor = Supervisor::new(
            Arc::clone(&registry),
            RestartPolicy::new(1, Duration::from_millis(10)),
        )
        .with_handle(supervisor_handle.clone());
        let source_line = format!("config error: {}", "x".repeat(DEFAULT_MAX_LINE_BYTES));
        let module = supervisor
            .spawn(ModuleSpec {
                module_id: "stderr-tail-wire".to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: vec![
                    ("FAKE_AFT_STDERR_LINE".to_string(), source_line.clone()),
                    ("FAKE_AFT_EXIT_CODE".to_string(), "1".to_string()),
                ],
                reserved: false,
                reserved_prefixes: Vec::new(),
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let tail = module.stderr_tail(None, None);
            if tail
                .entries
                .iter()
                .any(|entry| matches!(entry, TailEntry::ProcessStart))
                && tail.entries.iter().any(|entry| {
                    matches!(
                        entry,
                        TailEntry::Line {
                            truncated: true,
                            ..
                        }
                    )
                })
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "module did not produce a truncated line and restart boundary: {tail:?}"
            );
            sleep(Duration::from_millis(10)).await;
        }

        let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
        let request = ClientControlRequest::SupervisorStderrTail {
            module_id: "stderr-tail-wire".to_string(),
            max_lines: None,
            max_bytes: None,
        };
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            1,
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        let (ctx, _egress) = route_ctx(ConnectionId::new(1));
        let responses = handler.handle_control_frame(&ctx, frame).await.unwrap();
        let ClientControlResponse::SupervisorStderrTail { tail, .. } =
            serde_json::from_slice(&responses[0].body).unwrap()
        else {
            panic!("expected supervisor.stderr_tail response");
        };

        assert!(
            tail.entries
                .iter()
                .any(|entry| matches!(entry, StderrTailEntry::ProcessStart)),
            "the control response lost the restart boundary"
        );
        let Some(StderrTailEntry::Line { text, truncated }) = tail.entries.iter().find(|entry| {
            matches!(
                entry,
                StderrTailEntry::Line {
                    truncated: true,
                    ..
                }
            )
        }) else {
            panic!("the control response lost the truncated line");
        };
        assert_eq!(text, &source_line[..DEFAULT_MAX_LINE_BYTES]);
        assert!(*truncated);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervisor_terminals_golden_is_generated_through_the_real_handler() {
        let registry = Arc::new(Registry::default());
        let supervisor_handle = SupervisorHandle::new();
        let supervisor =
            Supervisor::new(Arc::clone(&registry), RestartPolicy::new(1, Duration::ZERO))
                .with_handle(supervisor_handle.clone());
        let module = supervisor
            .spawn(ModuleSpec {
                module_id: "terminal-golden".to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
                reserved: false,
                reserved_prefixes: Vec::new(),
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while module.terminal_history().entries.len() != 2 {
            assert!(
                Instant::now() < deadline,
                "module did not retain two terminal exits: {:?}",
                module.terminal_history()
            );
            sleep(Duration::from_millis(10)).await;
        }

        let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
        let request = ClientControlRequest::SupervisorTerminals {
            module_id: "terminal-golden".to_string(),
        };
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            1,
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        let (ctx, _egress) = route_ctx(ConnectionId::new(1));
        let responses = handler.handle_control_frame(&ctx, frame).await.unwrap();
        let response: ClientControlResponse = serde_json::from_slice(&responses[0].body).unwrap();
        let ClientControlResponse::SupervisorTerminals { terminals, .. } = &response else {
            panic!("expected supervisor.terminals response");
        };
        assert_eq!(terminals.entries.len(), 2);
        assert_eq!(terminals.dropped, 0);

        let mut rendered = serde_json::to_value(response).unwrap();
        // Wall-clock fields are the observation contract, but not stable fixture
        // bytes; normalize only them after the real handler has shaped the response.
        rendered["daemon_started_at_ms"] = json!(1_700_000_000_000u64);
        for (index, entry) in rendered["entries"]
            .as_array_mut()
            .expect("terminal response entries array")
            .iter_mut()
            .enumerate()
        {
            entry["at_ms"] = json!(1_700_000_000_001u64 + index as u64);
        }

        let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../subc-control/tests/golden/client_control_response_supervisor_terminals.json");
        let serialized = serde_json::to_string_pretty(&rendered).unwrap() + "\n";
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(&golden_path, &serialized).unwrap();
        }
        let expected: Value =
            serde_json::from_str(&std::fs::read_to_string(&golden_path).unwrap()).unwrap();
        assert_eq!(rendered, expected);
    }

    #[test]
    fn hello_registers_manifest_and_returns_ack() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let conn = ConnectionId::new(1);

        let responses = handler
            .handle_control(conn, hello_frame("aft", PROTOCOL_VERSION, 7))
            .unwrap();

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].header.ty, FrameType::HelloAck);
        assert_eq!(responses[0].header.channel, 0);
        assert_eq!(responses[0].header.corr, 7);
        let ack = parse_ack(&responses[0]);
        assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
        assert!(ack
            .subc_capabilities
            .contains(&CAP_MANIFEST_REGISTRATION.to_string()));
        assert!(ack.subc_ops.contains(&ops::SUPERVISOR_LIST.to_string()));
        assert!(ack.subc_ops.contains(&ops::SUPERVISOR_RESTART.to_string()));
        assert!(ack
            .subc_ops
            .contains(&ops::SUPERVISOR_SET_ENABLED.to_string()));
        assert!(ack
            .subc_ops
            .contains(&MODULE_TO_SUBC_OP_CATALOG_UPDATE.to_string()));

        let registration = registry.get_module("aft").unwrap().unwrap();
        assert_eq!(registration.negotiated_ver, PROTOCOL_VERSION);
        assert_eq!(registration.state, ChannelState::Active);
        assert_eq!(registration.connection_id, conn);
        assert_eq!(registration.control_ops, module_baseline_control_ops());
    }

    #[test]
    fn capability_grammar_refusals_name_the_field_and_leave_no_catalog_entry() {
        let invalid_identifiers = [
            ("case_change", "credentials-Provider/v1"),
            ("leading_zero", "credentials-provider/v01"),
            ("trailing_hyphen", "credentials-provider-/v1"),
            ("consecutive_hyphens", "credentials--provider/v1"),
            ("uppercase", "Credentials-provider/v1"),
            ("missing_v", "credentials-provider/1"),
            ("whitespace", "credentials provider/v1"),
            ("zero_version", "credentials-provider/v0"),
            ("out_of_range_version", "credentials-provider/v4294967296"),
            (
                "overlength_name",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/v1",
            ),
        ];
        let mut cases = invalid_identifiers
            .into_iter()
            .map(|(name, identifier)| {
                (
                    format!("identifier_{name}"),
                    "capabilities.provides[0]".to_string(),
                    identifier.to_string(),
                    json!({ "provides": [identifier] }),
                    None,
                )
            })
            .collect::<Vec<_>>();
        cases.extend([
            (
                "unknown_need".to_string(),
                "capabilities.requires[0].need".to_string(),
                "deferred".to_string(),
                json!({ "requires": [{ "capability": "credentials-provider/v1", "need": "deferred" }] }),
                None,
            ),
            (
                "duplicate_provides".to_string(),
                "capabilities.provides[1]".to_string(),
                "credentials-provider/v1".to_string(),
                json!({ "provides": ["credentials-provider/v1", "credentials-provider/v1"] }),
                None,
            ),
            (
                "duplicate_must_never_reach".to_string(),
                "capabilities.must_never_reach[1]".to_string(),
                "credentials-provider/v1".to_string(),
                json!({ "must_never_reach": ["credentials-provider/v1", "credentials-provider/v1"] }),
                None,
            ),
            (
                "duplicate_requires_same_need".to_string(),
                "capabilities.requires[1]".to_string(),
                "credentials-provider/v1".to_string(),
                json!({ "requires": [
                    { "capability": "credentials-provider/v1", "need": "required" },
                    { "capability": "credentials-provider/v1", "need": "required" }
                ] }),
                None,
            ),
            (
                "duplicate_requires_conflicting_need".to_string(),
                "capabilities.requires[1]".to_string(),
                "credentials-provider/v1".to_string(),
                json!({ "requires": [
                    { "capability": "credentials-provider/v1", "need": "required" },
                    { "capability": "credentials-provider/v1", "need": "optional" }
                ] }),
                None,
            ),
            (
                "capabilities_root_pointer".to_string(),
                "runtime_computed[0]".to_string(),
                "/capabilities".to_string(),
                json!({}),
                Some(json!(["/capabilities"])),
            ),
            (
                "capabilities_descendant_pointer".to_string(),
                "runtime_computed[0]".to_string(),
                "/capabilities/provides".to_string(),
                json!({}),
                Some(json!(["/capabilities/provides"])),
            ),
            (
                "malformed_pointer_without_leading_slash".to_string(),
                "runtime_computed[0]".to_string(),
                "capabilities".to_string(),
                json!({}),
                Some(json!(["capabilities"])),
            ),
            (
                "malformed_pointer_escape".to_string(),
                "runtime_computed[0]".to_string(),
                "/roles/~2/tools".to_string(),
                json!({}),
                Some(json!(["/roles/~2/tools"])),
            ),
            (
                "unknown_capabilities_field".to_string(),
                "capabilities.future".to_string(),
                "<array>".to_string(),
                json!({ "future": [] }),
                None,
            ),
        ]);

        for (index, (name, field, value, capabilities, runtime_computed)) in
            cases.into_iter().enumerate()
        {
            let registry = Arc::new(Registry::default());
            let handler = ControlHandler::new(Arc::clone(&registry));
            let response = handler
                .handle_control(
                    ConnectionId::new((index + 1) as u64),
                    capability_grammar_hello_frame(
                        capabilities,
                        runtime_computed,
                        index as u64 + 1,
                    ),
                )
                .expect("invalid HELLO returns a refusal");

            assert_eq!(response.len(), 1, "{name} must emit one refusal");
            let error = parse_error(&response[0]);
            assert_eq!(error["code"], "invalid_capability_grammar", "{name}");
            let message = error["message"]
                .as_str()
                .expect("error message is a string");
            assert!(
                message.contains(&field),
                "{name}: field missing from {message}"
            );
            assert!(
                message.contains(&value),
                "{name}: value missing from {message}"
            );
            assert_eq!(
                registry
                    .active_registration_count()
                    .expect("registry reads"),
                0,
                "{name}: refused HELLO must not create a catalog entry"
            );
        }
    }

    #[test]
    fn legal_runtime_pointer_and_capabilities_are_mirrored_in_catalog_list() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let capabilities = json!({
            "provides": ["credentials-provider/v1"],
            "requires": [{ "capability": "context-transform/v1", "need": "optional" }],
            "must_never_reach": ["federation-transport/v1"]
        });
        let response = handler
            .handle_control(
                ConnectionId::new(99),
                capability_grammar_hello_frame(
                    capabilities.clone(),
                    Some(json!(["/roles/0/tools"])),
                    99,
                ),
            )
            .expect("valid HELLO registers");
        assert_eq!(response[0].header.ty, FrameType::HelloAck);

        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            100,
            serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None })
                .expect("catalog request serializes"),
        )
        .expect("catalog request frame builds");
        let response = handler
            .handle_catalog_list(request, None)
            .expect("catalog list succeeds");
        let ClientControlResponse::CatalogList { modules, .. } =
            serde_json::from_slice(&response[0].body).expect("catalog response decodes")
        else {
            panic!("catalog request must return catalog.list");
        };
        assert_eq!(modules.len(), 1);
        assert_eq!(
            serde_json::to_value(&modules[0].capabilities).expect("catalog capabilities serialize"),
            capabilities
        );
    }

    #[test]
    fn catalog_list_mirrors_management_operation_description() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let description = "List managed records and return their identifiers and metadata.";
        let mut manifest = manifest("described-management", PROTOCOL_VERSION);
        manifest.provides = vec![ProviderRole::ManagementSurface {
            operations: vec![ManagementOperation {
                name: "records.list".to_string(),
                kind: ManagementOperationKind::Query,
                description: Some(description.to_string()),
            }],
            config_schema: json!({"type": "object"}),
            observability: vec![ObservabilitySurface {
                name: "records.stats".to_string(),
                kind: ObservabilityKind::Snapshot,
            }],
            identity_scope: vec![IdentityScope::Project],
            concurrency: Concurrency::ModuleManaged,
        }];
        registry
            .register_with_control_ops(
                manifest,
                PROTOCOL_VERSION,
                ConnectionId::new(99),
                Vec::new(),
            )
            .expect("described management manifest registers");

        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            100,
            serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None })
                .expect("catalog request serializes"),
        )
        .expect("catalog request frame builds");
        let response = handler
            .handle_catalog_list(request, None)
            .expect("catalog list succeeds");
        let body: Value = serde_json::from_slice(&response[0].body).expect("catalog response JSON");
        assert_eq!(
            body["modules"][0]["roles"][0]["operations"][0]["description"], description,
            "catalog.list must preserve the declared operation description verbatim"
        );
    }

    #[test]
    fn reserved_capability_refusal_mutation_proof_leaves_no_catalog_entry() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry)).with_capability_config(
            [("vault".to_string(), true), ("squatter".to_string(), true)],
            BTreeMap::from([("credentials-provider/v1".to_string(), "vault".to_string())]),
        );
        let mut squatter = manifest("squatter", PROTOCOL_VERSION);
        squatter.capabilities = Some(subc_protocol::manifest::CapabilityDeclarations {
            provides: vec!["credentials-provider/v1".to_string()],
            requires: Vec::new(),
            must_never_reach: Vec::new(),
        });
        let frame = Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            77,
            serde_json::to_vec(&ModuleHelloBody {
                manifest: squatter,
                protocol_ver: PROTOCOL_VERSION,
                control_ops: None,
                launch_nonce: None,
            })
            .expect("HELLO serializes"),
        )
        .expect("HELLO frame builds");
        let response = handler
            .handle_control(ConnectionId::new(77), frame)
            .expect("reserved claim receives a typed refusal");
        assert_eq!(parse_error(&response[0])["code"], "reserved_capability");
        assert_eq!(
            registry
                .active_registration_count()
                .expect("registry reads"),
            0,
            "a reserved capability refusal must not leave a catalog entry"
        );
    }

    #[test]
    fn server_describe_surfaces_required_capability_verdict_fields() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry)).with_capability_config(
            [
                ("consumer".to_string(), true),
                ("provider".to_string(), false),
            ],
            BTreeMap::new(),
        );
        let mut consumer = manifest("consumer", PROTOCOL_VERSION);
        consumer.capabilities = Some(subc_protocol::manifest::CapabilityDeclarations {
            provides: Vec::new(),
            requires: vec![subc_protocol::manifest::CapabilityRequirement {
                capability: "credentials-provider/v1".to_string(),
                need: subc_protocol::manifest::CapabilityNeed::Required,
            }],
            must_never_reach: Vec::new(),
        });
        let hello = Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            78,
            serde_json::to_vec(&ModuleHelloBody {
                manifest: consumer,
                protocol_ver: PROTOCOL_VERSION,
                control_ops: None,
                launch_nonce: None,
            })
            .expect("HELLO serializes"),
        )
        .expect("HELLO frame builds");
        handler
            .handle_control(ConnectionId::new(78), hello)
            .expect("consumer registers");
        let describe = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            79,
            serde_json::to_vec(&ClientControlRequest::ServerDescribe {})
                .expect("request serializes"),
        )
        .expect("describe frame builds");
        let response = handler
            .handle_server_describe(describe)
            .expect("server.describe succeeds");
        let rendered: Value = serde_json::from_slice(&response[0].body).expect("response JSON");
        let requirement = &rendered["capability_requirements"][0];
        assert_eq!(requirement["consumer"], "consumer");
        assert_eq!(requirement["verdict"], "never_provided");
        assert_eq!(requirement["episode_seq"], 1);
        assert_eq!(requirement["config_satisfiable"], false);
        assert_eq!(requirement["runtime_available"], false);
        assert!(requirement["detail"]
            .as_str()
            .expect("detail string")
            .contains("credentials-provider/v1"));
    }

    #[test]
    fn catalog_list_omits_capabilities_for_legacy_manifest() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let hello = handler
            .handle_control(
                ConnectionId::new(101),
                hello_frame("legacy-capability-manifest", PROTOCOL_VERSION, 101),
            )
            .expect("legacy HELLO registers");
        assert_eq!(hello[0].header.ty, FrameType::HelloAck);

        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            102,
            serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None })
                .expect("catalog request serializes"),
        )
        .expect("catalog request frame builds");
        let response = handler
            .handle_catalog_list(request, None)
            .expect("catalog list succeeds");
        let body: Value = serde_json::from_slice(&response[0].body).expect("catalog response JSON");
        assert!(
            body["modules"][0].get("capabilities").is_none(),
            "legacy manifest must retain an absent capabilities field on catalog.list"
        );
    }

    #[test]
    fn hello_ack_omits_storage_when_no_storage_config() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let responses = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("aft", PROTOCOL_VERSION, 7),
            )
            .unwrap();
        let ack = parse_ack(&responses[0]);
        assert_eq!(ack.storage, None, "no storage config -> no descriptor");
    }

    #[test]
    fn hello_ack_delivers_resolved_storage_descriptor_per_module() {
        // With a central sqlite storage policy, each registering module gets its
        // own resolved descriptor in HELLO_ACK, keyed by its module id.
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry)).with_storage_config(Some(
            crate::daemon_config::StorageConfig::Sqlite {
                data_home: std::path::PathBuf::from("/data"),
            },
        ));

        let responses = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("alfonso-routing", PROTOCOL_VERSION, 7),
            )
            .unwrap();
        let ack = parse_ack(&responses[0]);
        assert_eq!(
            ack.storage,
            Some(serde_json::json!({
                "module_id": "alfonso-routing",
                "storage_namespace": "default",
                "isolation": { "kind": "module" },
                "backend": {
                    "backend": "sqlite",
                    "path": "/data/cortexkit/alfonso-routing/store.db"
                }
            })),
            "the delivered descriptor is the module's own sqlite store path"
        );
    }

    #[test]
    fn hello_control_ops_none_is_baseline_and_guard_rejects_synthetic_gated_op() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let conn = ConnectionId::new(1);
        let responses = handler
            .handle_control(
                conn,
                hello_frame_with_control_ops("aft", PROTOCOL_VERSION, 7, None),
            )
            .unwrap();
        assert_eq!(responses[0].header.ty, FrameType::HelloAck);
        let registration = registry.get_module("aft").unwrap().unwrap();
        assert_eq!(registration.control_ops, module_baseline_control_ops());

        let frame =
            Frame::build(FrameType::Request, control_flags(), 0, 0, 77, Vec::new()).unwrap();
        assert!(handler
            .guard_module_control_op(&frame, "aft", "route.bind")
            .unwrap()
            .is_none());
        let error = handler
            .guard_module_control_op(&frame, "aft", "test.synthetic")
            .unwrap()
            .expect("synthetic ungranted op should be rejected");
        assert_eq!(error.header.ty, FrameType::Error);
        assert_eq!(parse_error(&error)["code"], "op_not_allowed");
    }

    #[test]
    fn hello_control_ops_some_adds_optional_grants() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame_with_control_ops(
                    "aft",
                    PROTOCOL_VERSION,
                    7,
                    Some(vec![
                        "future.synthetic".to_string(),
                        "route.bind".to_string(),
                    ]),
                ),
            )
            .unwrap();
        let registration = registry.get_module("aft").unwrap().unwrap();
        assert_eq!(
            registration.control_ops,
            vec![
                "route.bind".to_string(),
                "route.status".to_string(),
                "future.synthetic".to_string(),
            ]
        );
        let frame =
            Frame::build(FrameType::Request, control_flags(), 0, 0, 78, Vec::new()).unwrap();
        assert!(handler
            .guard_module_control_op(&frame, "aft", "future.synthetic")
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn health_probe_refuses_unadvertised_module_without_sending_frame() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(10));
        let responses = handler
            .handle_control_frame(
                &module_ctx,
                hello_frame_with_control_ops("aft", PROTOCOL_VERSION, 7, None),
            )
            .await
            .unwrap();
        assert_eq!(responses[0].header.ty, FrameType::HelloAck);

        let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(20));
        let responses = handler
            .handle_control_frame(&client_ctx, supervisor_health_probe_frame(77, "aft"))
            .await
            .unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&responses[0])["code"], "health_not_advertised");
        assert!(module_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn health_probe_demuxes_while_route_bind_relay_is_in_flight() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(30));
        handler
            .handle_control_frame(
                &module_ctx,
                hello_frame_with_control_ops(
                    "aft",
                    PROTOCOL_VERSION,
                    7,
                    Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
                ),
            )
            .await
            .unwrap();

        let project_root = unique_project_root("demux");
        let (route_client_ctx, mut route_client_rx) = route_ctx(ConnectionId::new(31));
        let route_handler = handler.clone();
        let route_task = tokio::spawn(async move {
            route_handler
                .handle_control_frame(
                    &route_client_ctx,
                    route_open_frame(100, "aft", project_root),
                )
                .await
                .unwrap()
        });
        let bind_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            serde_json::from_slice::<ModuleControlRequest>(&bind_frame.body).unwrap(),
            ModuleControlRequest::RouteBind { .. }
        ));

        let (health_client_ctx, _health_client_rx) = route_ctx(ConnectionId::new(32));
        let health_handler = handler.clone();
        let health_task = tokio::spawn(async move {
            health_handler
                .handle_control_frame(
                    &health_client_ctx,
                    supervisor_health_probe_frame(101, "aft"),
                )
                .await
                .unwrap()
        });
        let health_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<ModuleControlRequest>(&health_frame.body).unwrap(),
            ModuleControlRequest::HealthCheck {}
        );

        handler
            .handle_control_frame(
                &module_ctx,
                health_response(health_frame.header.corr, HealthStatus::Degraded),
            )
            .await
            .unwrap();
        let health_response = health_task.await.unwrap();
        assert_eq!(health_response.len(), 1);
        match serde_json::from_slice::<ClientControlResponse>(&health_response[0].body).unwrap() {
            ClientControlResponse::SupervisorHealthProbe {
                module_id,
                status,
                detail,
                metrics,
            } => {
                assert_eq!(module_id, "aft");
                assert_eq!(status, HealthStatus::Degraded);
                assert_eq!(detail.as_deref(), Some("warming"));
                assert_eq!(metrics, Some(json!({"queue_depth": 3})));
            }
            other => panic!("unexpected health response: {other:?}"),
        }

        handler
            .handle_control_frame(&module_ctx, route_bind_ack(bind_frame.header.corr))
            .await
            .unwrap();
        let route_response = route_task.await.unwrap();
        assert!(route_response.is_empty());
        let published = route_client_rx.recv().await.unwrap();
        assert!(matches!(
            serde_json::from_slice::<ClientControlResponse>(&published.body).unwrap(),
            ClientControlResponse::RouteOpen { .. }
        ));
    }

    /// The spawn-attestation guard is what stops a connected module from claiming
    /// another module's identity and being stamped `Reserved` for it. Every other
    /// test that supplies a consumer_identity supplies a CORRECT one, because a
    /// correct one is what the rest of the flow needs -- so the guard's rejection
    /// branch was never the subject of an assertion, only its acceptance branch.
    ///
    /// Deleting the guard's EFFECT (granting Reserved unconditionally) leaves the
    /// whole subc-core library suite green; only the forwarding integration tests
    /// notice, and they notice for unrelated reasons. This test exists so the
    /// refusal itself is asserted where the guard lives: it fails if the identity
    /// check stops refusing, which is the direction that matters, since a guard
    /// that wrongly ACCEPTS is silent while one that wrongly REJECTS is loud.
    #[tokio::test]
    async fn route_open_refuses_consumer_identity_that_fails_spawn_attestation() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("fed", "fed-nonce".to_string());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor);

        let (target_ctx, _target_rx) = route_ctx(ConnectionId::new(90));
        handler
            .handle_control_frame(&target_ctx, hello_frame("target", PROTOCOL_VERSION, 1))
            .await
            .unwrap();

        // A real supervised module id presenting the wrong nonce. This is the
        // impersonation case: the attacker knows a privileged module_id, which is
        // public, and guesses at the nonce, which is not.
        let wrong_nonce = handler
            .handle_control_frame(
                &route_ctx(ConnectionId::new(91)).0,
                route_open_frame_with_admission_facts(
                    20,
                    "target",
                    Some(subc_control::ConsumerIdentity {
                        module_id: "fed".to_string(),
                        launch_nonce: "not-the-real-nonce".to_string(),
                    }),
                    None,
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            parse_error(&wrong_nonce[0])["code"],
            "bad_consumer_identity",
            "a mismatched launch nonce must be refused, not stamped Reserved"
        );

        // A module id the supervisor never spawned at all, so no nonce exists to
        // compare against. An implementation that treats "no record" as "nothing
        // to check" fails open here while passing the case above.
        let never_spawned = handler
            .handle_control_frame(
                &route_ctx(ConnectionId::new(92)).0,
                route_open_frame_with_admission_facts(
                    21,
                    "target",
                    Some(subc_control::ConsumerIdentity {
                        module_id: "never-spawned".to_string(),
                        launch_nonce: "any-nonce".to_string(),
                    }),
                    None,
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            parse_error(&never_spawned[0])["code"],
            "bad_consumer_identity",
            "an unspawned module_id must be refused rather than accepted for lack of a record"
        );
    }

    /// The refusal test above proves the guard says NO. Nothing proved it can say
    /// YES, and the difference is not academic: replacing the whole authorization
    /// with `false` -- admitting no consumer identity at all, revoking Reserved
    /// standing for every supervised module in the fleet -- leaves 110 of the 111
    /// library tests GREEN. The one that notices does so by HANGING, because it
    /// waits for a bind that can no longer happen.
    ///
    /// A hang is the weakest signal a suite can produce. In CI it reads as a slow
    /// or flaky test, invites a RETRY rather than an investigation, and the retry
    /// hangs too and gets blamed on the runner. So a total revocation of the
    /// daemon's trust grant would have shipped behind a symptom nobody attributes
    /// to code.
    ///
    /// The bias is structural rather than accidental. A REFUSAL looks like a
    /// failure someone writes a test for; a GRANT looks like the happy path. Every
    /// binary-outcome guard whose STRICTNESS is the point acquires a refusal-heavy
    /// suite for that reason, and this one is the purest case in the daemon.
    ///
    /// This test asserts the EFFECT rather than the absence of an error: the module
    /// receives a RouteBind and it carries `Reserved` naming the attested module.
    /// A guard that admitted nobody would produce no bind at all; one that admitted
    /// everybody would stamp the wrong principal, which the refusal test catches.
    #[tokio::test]
    async fn route_open_stamps_reserved_for_a_correctly_attested_consumer() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("fed", "fed-nonce".to_string());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor);

        let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(95));
        handler
            .handle_control_frame(&target_ctx, hello_frame("target", PROTOCOL_VERSION, 1))
            .await
            .unwrap();

        let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(96));
        let route_handler = handler.clone();
        let route_task = tokio::spawn(async move {
            route_handler
                .handle_control_frame(
                    &client_ctx,
                    route_open_frame_with_admission_facts(
                        30,
                        "target",
                        Some(subc_control::ConsumerIdentity {
                            module_id: "fed".to_string(),
                            launch_nonce: "fed-nonce".to_string(),
                        }),
                        None,
                    ),
                )
                .await
                .unwrap()
        });

        // BOUND THE WAIT. The first version of this test recv'd unbounded, and under
        // the very mutation it exists to catch -- a guard that admits nobody -- no
        // bind is ever sent, so it HUNG rather than failing. That reproduces the
        // exact defect being fixed: a total revocation detected only as a stalled
        // suite, which reads as flakiness and invites a retry. An acceptance test
        // that waits for an effect must bound the wait, or a red becomes a hang.
        let bind_frame = tokio::time::timeout(Duration::from_secs(5), target_rx.recv())
            .await
            .expect("no route.bind within 5s: the consumer-identity guard refused a correctly attested consumer")
            .expect("module control channel closed before route.bind");
        let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
        let ModuleControlRequest::RouteBind { principal, .. } = bind else {
            panic!("expected route.bind")
        };
        assert_eq!(
            principal,
            Some(Principal::Reserved {
                module_id: "fed".to_string()
            }),
            "a correctly attested consumer must be stamped Reserved for its own id"
        );

        handler
            .handle_control_frame(&target_ctx, route_bind_ack(bind_frame.header.corr))
            .await
            .unwrap();
        assert!(route_task.await.unwrap().is_empty());
        assert!(
            matches!(
                serde_json::from_slice::<ClientControlResponse>(
                    &client_rx.recv().await.unwrap().body
                )
                .unwrap(),
                ClientControlResponse::RouteOpen { .. }
            ),
            "the route must actually open, not merely avoid an error"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_routes_serializes_live_draining_bindings_from_the_real_handler() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("fed", "fed-nonce".to_string());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor);

        let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(101));
        handler
            .handle_control_frame(&target_ctx, hello_frame("target", PROTOCOL_VERSION, 1))
            .await
            .unwrap();

        let (direct_ctx, mut direct_rx) = route_ctx(ConnectionId::new(102));
        let direct_handler = handler.clone();
        let direct_open = tokio::spawn(async move {
            direct_handler
                .handle_control_frame(
                    &direct_ctx,
                    route_open_frame(2, "target", unique_project_root("route-census-direct")),
                )
                .await
                .unwrap()
        });
        let direct_bind = tokio::time::timeout(Duration::from_secs(5), target_rx.recv())
            .await
            .expect("no direct route.bind within 5s")
            .expect("target control channel closed before direct route.bind");
        handler
            .handle_control_frame(&target_ctx, route_bind_ack(direct_bind.header.corr))
            .await
            .unwrap();
        assert!(direct_open.await.unwrap().is_empty());
        let _ = direct_rx.recv().await.unwrap();

        let (reserved_ctx, mut reserved_rx) = route_ctx(ConnectionId::new(103));
        let reserved_handler = handler.clone();
        let reserved_open = tokio::spawn(async move {
            reserved_handler
                .handle_control_frame(
                    &reserved_ctx,
                    route_open_frame_with_admission_facts(
                        3,
                        "target",
                        Some(ConsumerIdentity {
                            module_id: "fed".to_string(),
                            launch_nonce: "fed-nonce".to_string(),
                        }),
                        None,
                    ),
                )
                .await
                .unwrap()
        });
        let reserved_bind = tokio::time::timeout(Duration::from_secs(5), target_rx.recv())
            .await
            .expect("no reserved route.bind within 5s")
            .expect("target control channel closed before reserved route.bind");
        handler
            .handle_control_frame(&target_ctx, route_bind_ack(reserved_bind.header.corr))
            .await
            .unwrap();
        assert!(reserved_open.await.unwrap().is_empty());
        let _ = reserved_rx.recv().await.unwrap();

        forwarding
            .begin_module_drain("target", subc_control::RouteCloseReason::Reload)
            .unwrap();
        let (census_ctx, _census_rx) = route_ctx(ConnectionId::new(104));
        let census_body = serde_json::to_vec(&ClientControlRequest::SupervisorRoutes {
            module_id: Some("target".to_string()),
        })
        .unwrap();
        let census_frame =
            Frame::build(FrameType::Request, control_flags(), 0, 0, 4, census_body).unwrap();
        let response = handler
            .handle_control_frame(&census_ctx, census_frame)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let actual: Value = serde_json::from_slice(&response.body).unwrap();
        let decoded: ClientControlResponse = serde_json::from_value(actual.clone()).unwrap();
        assert!(matches!(
            decoded,
            ClientControlResponse::SupervisorRoutes { .. }
        ));
        let routes = actual["modules"][0]["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 2);
        assert!(routes.iter().all(|route| route["draining"] == true));
        // The census carries WHY: the reason the drain was begun with, in the
        // route.closing vocabulary, on every draining route this drain marked.
        assert!(
            routes.iter().all(|route| route["drain_reason"] == "reload"),
            "draining routes must name the drain's reason: {routes:?}"
        );
        assert!(routes.iter().any(|route| {
            route["consumer"] == serde_json::json!({"kind": "direct", "connection_id": 102})
        }));
        assert!(routes.iter().any(|route| {
            route["consumer"] == serde_json::json!({"kind": "reserved", "module_id": "fed"})
        }));

        let golden_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../subc-control/tests/golden/client_control_response_supervisor_routes.json");
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(
                &golden_path,
                format!("{}\n", serde_json::to_string_pretty(&actual).unwrap()),
            )
            .unwrap();
        }
        let expected: Value =
            serde_json::from_str(&std::fs::read_to_string(golden_path).unwrap()).unwrap();
        assert_eq!(actual, expected);
    }

    /// Read the vendored fed corpus rather than hand-building a package.
    ///
    /// A hand-built object encodes what the test author believed the carrier
    /// emits. These vectors are what it actually emits, and one of them exists
    /// specifically to pin OUR side of the seam: its note reads "SUBC relay
    /// ignores additive unknown fields at the traversal emit terminus."
    fn fed_admission_facts_vectors() -> Vec<(String, Value)> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/team-mode/conformance/vectors/fed/admission-facts-emit.jsonl");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("vendored fed corpus unreadable at {path:?}: {err}"));
        let vectors: Vec<(String, Value)> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let entry: Value = serde_json::from_str(line).expect("corpus line must be JSON");
                let id = entry["corpus_id"]
                    .as_str()
                    .expect("every vector carries a corpus_id")
                    .to_string();
                (id, entry["package"].clone())
            })
            .collect();
        // Pin the count: a corpus that silently shrinks would take its coverage
        // with it, and a suite reading N-1 vectors reports the same clean pass
        // as one reading N.
        assert_eq!(
            vectors.len(),
            3,
            "vendored fed corpus changed size; re-sync from subc-federation"
        );

        // Pin what makes the corpus DISCRIMINATING, not just present.
        //
        // The relay test below takes its expected value from the corpus, so the
        // corpus supplies the test's power to detect a lossy relay rather than
        // its correctness. A relay that dropped unrecognised fields would still
        // be caught -- but only by a package carrying fields it does not know.
        // Shrink every package to the handful of keys any implementation would
        // recognise and the test keeps passing over an input that can no longer
        // fail, which is the same clean green as a corpus that shrank away.
        //
        // So assert the precondition rather than duplicating the packages here:
        // at least one vector must carry a field beyond the small common set.
        // That is one claim to maintain instead of nine, and it fails loudly if
        // a re-sync ever flattens the corpus.
        const COMMONLY_MODELLED: [&str; 3] = ["schema", "verified_class", "org"];
        let richest = vectors
            .iter()
            .filter_map(|(_, package)| package.as_object())
            .map(|object| {
                object
                    .keys()
                    .filter(|key| !COMMONLY_MODELLED.contains(&key.as_str()))
                    .count()
            })
            .max()
            .unwrap_or(0);
        assert!(
            richest >= 2,
            "vendored corpus no longer carries a package with unmodelled fields, \
             so the relay test can no longer distinguish a verbatim relay from a lossy one"
        );

        vectors
    }

    /// The relay must carry the carrier's package through BYTE-FOR-BYTE.
    ///
    /// The gate test below proves the ACCESS RULE (who may send facts, to whom).
    /// This proves the PAYLOAD RULE, which the gate cannot: it hand-builds a
    /// three-key object, so a relay that quietly dropped fields it did not
    /// recognise would satisfy it. These vectors carry nine keys including ones
    /// this crate has no type for, so a typed relay fails here and only here.
    #[tokio::test]
    async fn admission_facts_relay_carries_vendored_packages_verbatim() {
        for (corpus_id, package) in fed_admission_facts_vectors() {
            let registry = Arc::new(Registry::default());
            let forwarding = Arc::new(ForwardingTable::default());
            let supervisor = SupervisorHandle::new();
            supervisor.set_spawn_nonce("fed", "fed-nonce".to_string());
            let handler =
                ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                    .with_supervisor(supervisor)
                    .with_admission_facts_config(
                        Some("fed".to_string()),
                        Some(vec!["target".to_string()]),
                    );

            let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(90));
            handler
                .handle_control_frame(&target_ctx, hello_frame("target", PROTOCOL_VERSION, 1))
                .await
                .unwrap();

            let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(91));
            let route_handler = handler.clone();
            let expected = package.clone();
            let route_task = tokio::spawn(async move {
                route_handler
                    .handle_control_frame(
                        &client_ctx,
                        route_open_frame_with_admission_facts(
                            20,
                            "target",
                            Some(subc_control::ConsumerIdentity {
                                module_id: "fed".to_string(),
                                launch_nonce: "fed-nonce".to_string(),
                            }),
                            Some(package),
                        ),
                    )
                    .await
                    .unwrap()
            });

            let bind_frame = target_rx.recv().await.unwrap();
            let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
            let ModuleControlRequest::RouteBind {
                admission_facts, ..
            } = bind
            else {
                panic!("{corpus_id}: expected route.bind")
            };
            assert_eq!(
                admission_facts,
                Some(expected),
                "{corpus_id}: relay must not add, drop or reshape any field"
            );

            handler
                .handle_control_frame(&target_ctx, route_bind_ack(bind_frame.header.corr))
                .await
                .unwrap();
            route_task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn admission_facts_gate_checks_carrier_target_and_precedence() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("fed", "fed-nonce".to_string());
        supervisor.set_spawn_nonce("other", "other-nonce".to_string());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor)
                .with_admission_facts_config(
                    Some("fed".to_string()),
                    Some(vec!["target".to_string()]),
                );

        let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(70));
        handler
            .handle_control_frame(&target_ctx, hello_frame("target", PROTOCOL_VERSION, 1))
            .await
            .unwrap();
        let (other_ctx, _other_rx) = route_ctx(ConnectionId::new(71));
        handler
            .handle_control_frame(&other_ctx, hello_frame("other", PROTOCOL_VERSION, 2))
            .await
            .unwrap();

        let facts = json!({"schema": 1, "verified_class": "member", "org": "01H"});
        let expected_facts = facts.clone();
        let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(72));
        let route_handler = handler.clone();
        let route_task = tokio::spawn(async move {
            route_handler
                .handle_control_frame(
                    &client_ctx,
                    route_open_frame_with_admission_facts(
                        10,
                        "target",
                        Some(subc_control::ConsumerIdentity {
                            module_id: "fed".to_string(),
                            launch_nonce: "fed-nonce".to_string(),
                        }),
                        Some(facts.clone()),
                    ),
                )
                .await
                .unwrap()
        });
        let bind_frame = target_rx.recv().await.unwrap();
        let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
        let ModuleControlRequest::RouteBind {
            admission_facts, ..
        } = bind
        else {
            panic!("expected route.bind")
        };
        assert_eq!(admission_facts, Some(expected_facts));
        handler
            .handle_control_frame(&target_ctx, route_bind_ack(bind_frame.header.corr))
            .await
            .unwrap();
        assert!(route_task.await.unwrap().is_empty());
        assert!(matches!(
            serde_json::from_slice::<ClientControlResponse>(&client_rx.recv().await.unwrap().body)
                .unwrap(),
            ClientControlResponse::RouteOpen { .. }
        ));

        let direct = handler
            .handle_control_frame(
                &route_ctx(ConnectionId::new(73)).0,
                route_open_frame_with_admission_facts(11, "target", None, Some(json!({"x": 1}))),
            )
            .await
            .unwrap();
        assert_eq!(
            parse_error(&direct[0])["code"],
            "admission_facts_not_permitted"
        );

        let different_reserved = handler
            .handle_control_frame(
                &route_ctx(ConnectionId::new(77)).0,
                route_open_frame_with_admission_facts(
                    15,
                    "target",
                    Some(subc_control::ConsumerIdentity {
                        module_id: "other".to_string(),
                        launch_nonce: "other-nonce".to_string(),
                    }),
                    Some(json!({"x": 1})),
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            parse_error(&different_reserved[0])["code"],
            "admission_facts_not_permitted"
        );

        let other_target = handler
            .handle_control_frame(
                &route_ctx(ConnectionId::new(74)).0,
                route_open_frame_with_admission_facts(
                    12,
                    "other",
                    Some(subc_control::ConsumerIdentity {
                        module_id: "fed".to_string(),
                        launch_nonce: "fed-nonce".to_string(),
                    }),
                    Some(json!({"x": 1})),
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            parse_error(&other_target[0])["code"],
            "admission_facts_target_not_allowed"
        );

        let nonexistent = handler
            .handle_control_frame(
                &route_ctx(ConnectionId::new(75)).0,
                route_open_frame_with_admission_facts(13, "missing", None, Some(json!({"x": 1}))),
            )
            .await
            .unwrap();
        assert_eq!(parse_error(&nonexistent[0])["code"], "unknown_module");

        let described = handler
            .handle_control_frame(
                &route_ctx(ConnectionId::new(76)).0,
                Frame::build(
                    FrameType::Request,
                    control_flags(),
                    0,
                    0,
                    14,
                    serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let ClientControlResponse::ServerDescribe { capabilities, .. } =
            serde_json::from_slice(&described[0].body).unwrap()
        else {
            panic!("expected server.describe response")
        };
        assert!(capabilities
            .iter()
            .any(|cap| cap == "admission_facts_relay_v1"));
    }

    #[tokio::test]
    async fn admission_facts_without_configured_carrier_are_rejected() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler = ControlHandler::with_forwarding(registry, forwarding);
        let (target_ctx, _) = route_ctx(ConnectionId::new(78));
        handler
            .handle_control_frame(&target_ctx, hello_frame("target", PROTOCOL_VERSION, 1))
            .await
            .unwrap();

        let responses = handler
            .handle_control_frame(
                &route_ctx(ConnectionId::new(79)).0,
                route_open_frame_with_admission_facts(16, "target", None, Some(json!({"x": 1}))),
            )
            .await
            .unwrap();
        assert_eq!(
            parse_error(&responses[0])["code"],
            "admission_facts_not_permitted"
        );
    }

    #[tokio::test]
    async fn route_open_relays_consumer_capabilities_verbatim() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(37));
        handler
            .handle_control_frame(&module_ctx, hello_frame("aft", PROTOCOL_VERSION, 7))
            .await
            .unwrap();

        let expected = vec!["elicitation".to_string(), "roots".to_string()];
        let expected_for_request = expected.clone();
        let project_root = unique_project_root("consumer-capabilities-present");
        let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(38));
        let route_handler = handler.clone();
        let route_task = tokio::spawn(async move {
            route_handler
                .handle_control_frame(
                    &client_ctx,
                    route_open_frame_with_consumer_capabilities(
                        401,
                        "aft",
                        project_root,
                        Some(expected_for_request),
                    ),
                )
                .await
                .unwrap()
        });
        let bind_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
        let ModuleControlRequest::RouteBind {
            consumer_capabilities,
            ..
        } = bind
        else {
            panic!("expected route.bind request, got {bind:?}");
        };
        assert_eq!(consumer_capabilities, Some(expected.clone()));

        handler
            .handle_control_frame(&module_ctx, route_bind_ack(bind_frame.header.corr))
            .await
            .unwrap();
        let route_response = route_task.await.unwrap();
        assert!(route_response.is_empty());
        let published = client_rx.recv().await.unwrap();
        assert!(matches!(
            serde_json::from_slice::<ClientControlResponse>(&published.body).unwrap(),
            ClientControlResponse::RouteOpen { .. }
        ));
    }

    #[tokio::test]
    async fn route_open_without_consumer_capabilities_relays_none() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(39));
        handler
            .handle_control_frame(&module_ctx, hello_frame("aft", PROTOCOL_VERSION, 7))
            .await
            .unwrap();

        let project_root = unique_project_root("consumer-capabilities-absent");
        let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(40));
        let route_handler = handler.clone();
        let route_task = tokio::spawn(async move {
            route_handler
                .handle_control_frame(&client_ctx, route_open_frame(402, "aft", project_root))
                .await
                .unwrap()
        });
        let bind_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
        let ModuleControlRequest::RouteBind {
            consumer_capabilities,
            ..
        } = bind
        else {
            panic!("expected route.bind request, got {bind:?}");
        };
        assert_eq!(consumer_capabilities, None);

        handler
            .handle_control_frame(&module_ctx, route_bind_ack(bind_frame.header.corr))
            .await
            .unwrap();
        let route_response = route_task.await.unwrap();
        assert!(route_response.is_empty());
        let published = client_rx.recv().await.unwrap();
        assert!(matches!(
            serde_json::from_slice::<ClientControlResponse>(&published.body).unwrap(),
            ClientControlResponse::RouteOpen { .. }
        ));
    }

    #[tokio::test]
    async fn supervision_only_module_health_probe_does_not_enable_route_open_and_cleans_up() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_health_probe_timeout(Duration::from_secs(5));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(35));
        let responses = handler
            .handle_control_frame(
                &module_ctx,
                non_routable_hello_frame_with_control_ops(
                    "mcp",
                    300,
                    Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
                ),
            )
            .await
            .unwrap();
        assert_eq!(responses[0].header.ty, FrameType::HelloAck);
        assert!(registry
            .get_module("mcp")
            .unwrap()
            .unwrap()
            .manifest
            .provides
            .is_empty());

        let (route_client_ctx, _route_client_rx) = route_ctx(ConnectionId::new(36));
        let route_response = handler
            .handle_control_frame(
                &route_client_ctx,
                route_open_frame(301, "mcp", unique_project_root("non-routable-mcp")),
            )
            .await
            .unwrap();
        assert_eq!(route_response[0].header.ty, FrameType::Error);
        assert_eq!(
            parse_error(&route_response[0])["code"],
            "target_unavailable"
        );
        assert!(parse_error(&route_response[0])["message"]
            .as_str()
            .unwrap()
            .contains("does not provide the requested target"));
        assert!(module_rx.try_recv().is_err());

        let (health_client_ctx, _health_client_rx) = route_ctx(ConnectionId::new(37));
        let health_handler = handler.clone();
        let health_task = tokio::spawn(async move {
            health_handler
                .handle_control_frame(
                    &health_client_ctx,
                    supervisor_health_probe_frame(302, "mcp"),
                )
                .await
                .unwrap()
        });
        let health_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<ModuleControlRequest>(&health_frame.body).unwrap(),
            ModuleControlRequest::HealthCheck {}
        );
        handler
            .handle_control_frame(
                &module_ctx,
                health_response(health_frame.header.corr, HealthStatus::Ok),
            )
            .await
            .unwrap();
        let health_response = health_task.await.unwrap();
        assert_eq!(health_response[0].header.ty, FrameType::Response);
        match serde_json::from_slice::<ClientControlResponse>(&health_response[0].body).unwrap() {
            ClientControlResponse::SupervisorHealthProbe {
                module_id, status, ..
            } => {
                assert_eq!(module_id, "mcp");
                assert_eq!(status, HealthStatus::Ok);
            }
            other => panic!("unexpected health response: {other:?}"),
        }

        // Exercise the forwarding cleanup path directly while leaving the registry
        // advertisement in place. If cleanup leaves a stale control sink behind,
        // the next probe will enqueue onto it and wait for the long probe timeout
        // instead of returning an immediate no-connection error.
        forwarding
            .cleanup_connection(module_ctx.connection_id)
            .unwrap();
        let (cleanup_probe_ctx, _cleanup_probe_rx) = route_ctx(ConnectionId::new(38));
        let cleanup_response = tokio::time::timeout(
            Duration::from_millis(200),
            handler.handle_control_frame(
                &cleanup_probe_ctx,
                supervisor_health_probe_frame(303, "mcp"),
            ),
        )
        .await
        .expect("probe should fail immediately when the control lane is gone")
        .unwrap();
        assert_eq!(cleanup_response[0].header.ty, FrameType::Error);
        assert_eq!(
            parse_error(&cleanup_response[0])["code"],
            "target_unavailable"
        );
        assert!(parse_error(&cleanup_response[0])["message"]
            .as_str()
            .unwrap()
            .contains("no module connection"));

        handler
            .cleanup_connection(module_ctx.connection_id)
            .unwrap();
    }

    #[tokio::test]
    async fn route_open_classifies_unregistered_running_supervised_module_as_warming() {
        let registry = Arc::new(Registry::default());
        let supervisor_handle = SupervisorHandle::new();
        let supervisor =
            Supervisor::new(Arc::clone(&registry), RestartPolicy::new(0, Duration::ZERO))
                .with_handle(supervisor_handle.clone())
                .with_connection_file_path(
                    std::env::temp_dir()
                        .join(format!("subc-route-open-warming-{}", std::process::id())),
                );
        let module = supervisor
            .supervise_configured(
                ModuleSpec {
                    module_id: "warming".to_string(),
                    program: fake_aft_stub_path(),
                    args: Vec::new(),
                    env: Vec::new(),
                    reserved: false,
                    reserved_prefixes: Vec::new(),
                },
                true,
            )
            .unwrap();
        assert_eq!(module.state().unwrap(), ModuleState::Running);

        let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
        let (ctx, _rx) = route_ctx(ConnectionId::new(39));
        let response = handler
            .handle_control_frame(
                &ctx,
                route_open_frame(304, "warming", unique_project_root("warming")),
            )
            .await
            .unwrap();
        module.stop().await.unwrap();

        assert_eq!(response[0].header.ty, FrameType::Error);
        let error = parse_error(&response[0]);
        assert_eq!(error["code"], "module_warming");
        assert!(error["message"]
            .as_str()
            .unwrap()
            .contains("state=running, enabled=true, live=false"));
    }

    #[tokio::test]
    async fn route_open_keeps_failed_unregistered_supervised_module_unavailable() {
        let registry = Arc::new(Registry::default());
        let supervisor_handle = SupervisorHandle::new();
        let missing_program = std::env::temp_dir().join(format!(
            "subc-route-open-missing-program-{}",
            std::process::id()
        ));
        let supervisor =
            Supervisor::new(Arc::clone(&registry), RestartPolicy::new(0, Duration::ZERO))
                .with_handle(supervisor_handle.clone());
        let module = supervisor
            .supervise_configured(
                ModuleSpec {
                    module_id: "failed".to_string(),
                    program: missing_program,
                    args: Vec::new(),
                    env: Vec::new(),
                    reserved: false,
                    reserved_prefixes: Vec::new(),
                },
                true,
            )
            .unwrap();
        assert_eq!(module.state().unwrap(), ModuleState::Failed);

        let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
        let (ctx, _rx) = route_ctx(ConnectionId::new(40));
        let response = handler
            .handle_control_frame(
                &ctx,
                route_open_frame(305, "failed", unique_project_root("failed")),
            )
            .await
            .unwrap();

        assert_eq!(response[0].header.ty, FrameType::Error);
        let error = parse_error(&response[0]);
        assert_eq!(error["code"], "target_unavailable");
        assert!(error["message"]
            .as_str()
            .unwrap()
            .contains("state=failed, enabled=true, live=false"));
    }

    #[tokio::test]
    async fn route_open_role_mismatch_remains_target_unavailable() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        handler
            .handle_control(
                ConnectionId::new(41),
                non_routable_hello_frame_with_control_ops("health-only", 306, None),
            )
            .unwrap();

        let (ctx, _rx) = route_ctx(ConnectionId::new(42));
        let response = handler
            .handle_control_frame(
                &ctx,
                route_open_frame(307, "health-only", unique_project_root("role-mismatch")),
            )
            .await
            .unwrap();

        assert_eq!(parse_error(&response[0])["code"], "target_unavailable");
        assert!(parse_error(&response[0])["message"]
            .as_str()
            .unwrap()
            .contains("does not provide the requested target"));
    }

    #[tokio::test]
    async fn route_open_inactive_registration_remains_target_unavailable() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        handler
            .handle_control(
                ConnectionId::new(43),
                hello_frame("inactive", PROTOCOL_VERSION, 308),
            )
            .unwrap();
        assert!(registry
            .set_module_state_for_test("inactive", ChannelState::Closed)
            .unwrap());

        let (ctx, _rx) = route_ctx(ConnectionId::new(44));
        let response = handler
            .handle_control_frame(
                &ctx,
                route_open_frame(309, "inactive", unique_project_root("inactive")),
            )
            .await
            .unwrap();

        assert_eq!(parse_error(&response[0])["code"], "target_unavailable");
        assert!(parse_error(&response[0])["message"]
            .as_str()
            .unwrap()
            .contains("is not active"));
    }

    #[tokio::test]
    async fn late_health_reply_is_recorded_through_the_module_response_path() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor_handle = SupervisorHandle::new();
        let supervisor = Supervisor::new(Arc::clone(&registry), crate::RestartPolicy::default())
            .with_forwarding(Arc::clone(&forwarding))
            .with_handle(supervisor_handle.clone());
        let module = supervisor
            .supervise_configured(
                crate::ModuleSpec {
                    module_id: "late-health-response".to_string(),
                    program: PathBuf::from("disabled-module"),
                    args: Vec::new(),
                    env: Vec::new(),
                    reserved: false,
                    reserved_prefixes: Vec::new(),
                },
                false,
            )
            .unwrap();
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor_handle);
        let (module_ctx, _module_rx) = route_ctx(ConnectionId::new(39));
        handler
            .handle_control_frame(
                &module_ctx,
                hello_frame_with_control_ops(
                    "late-health-response",
                    PROTOCOL_VERSION,
                    7,
                    Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
                ),
            )
            .await
            .unwrap();
        let probe_started_at = Instant::now() - Duration::from_millis(80);
        let pending = forwarding
            .begin_health_probe_rpc_for(
                "late-health-response",
                MODULE_CONTROL_OP_HEALTH_CHECK,
                probe_started_at,
                Instant::now() - Duration::from_millis(1),
            )
            .unwrap();
        assert!(forwarding
            .tombstone_health_probe_rpc(pending.endpoint, pending.corr)
            .unwrap());

        let responses = handler
            .handle_control_frame(&module_ctx, health_response(pending.corr, HealthStatus::Ok))
            .await
            .unwrap();

        assert!(responses.is_empty());
        let health = module.status().unwrap().health;
        assert_eq!(health.late_answer_count, 1);
        assert!(health.last_late_answer_latency_ms.unwrap() >= 80);
    }

    #[tokio::test]
    async fn health_probe_timeout_and_module_death_are_typed() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_health_probe_timeout(Duration::from_millis(50));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(40));
        handler
            .handle_control_frame(
                &module_ctx,
                hello_frame_with_control_ops(
                    "aft",
                    PROTOCOL_VERSION,
                    7,
                    Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
                ),
            )
            .await
            .unwrap();

        let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(41));
        let responses = handler
            .handle_control_frame(&client_ctx, supervisor_health_probe_frame(201, "aft"))
            .await
            .unwrap();
        assert_eq!(responses[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&responses[0])["code"], "module_timeout");
        let _ = module_rx.try_recv();

        let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(42));
        let health_handler = handler.clone();
        let death_task = tokio::spawn(async move {
            health_handler
                .handle_control_frame(&client_ctx, supervisor_health_probe_frame(202, "aft"))
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        handler
            .cleanup_connection(module_ctx.connection_id)
            .unwrap();
        let responses = death_task.await.unwrap();
        assert_eq!(responses[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&responses[0])["code"], "target_unavailable");
    }

    #[test]
    fn hello_requires_exact_protocol_version() {
        for (connection, offered) in [(1, PROTOCOL_VERSION - 1), (2, PROTOCOL_VERSION + 1)] {
            let registry = Arc::new(Registry::default());
            let handler = ControlHandler::new(Arc::clone(&registry));
            let responses = handler
                .handle_control(
                    ConnectionId::new(connection),
                    hello_frame("aft", offered, 9),
                )
                .unwrap();

            assert_eq!(responses.len(), 1);
            assert_eq!(responses[0].header.ty, FrameType::Error);
            let error = parse_error(&responses[0]);
            assert_eq!(error["code"], "version_unsupported");
            assert!(registry.get_module("aft").unwrap().is_none());
            assert_eq!(registry.active_registration_count().unwrap(), 0);
        }
    }

    #[test]
    fn unknown_module_push_op_is_ignored_but_malformed_known_op_errors() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let module_connection = ConnectionId::new(301);
        let registration = registry
            .register_with_control_ops(
                manifest("aft-push", PROTOCOL_VERSION),
                PROTOCOL_VERSION,
                module_connection,
                module_baseline_control_ops(),
            )
            .unwrap();
        let (module_tx, _module_rx) = mpsc::channel(8);
        let endpoint = forwarding
            .register_module_connection(
                module_connection,
                "aft-push".to_string(),
                PROTOCOL_VERSION,
                manifest_concurrency(&registration.manifest),
                FrameSink::new(module_tx),
            )
            .unwrap();

        // A push op this version does not know is ignored (forward-compat), not errored.
        let unknown = Frame::build(
            FrameType::Push,
            control_flags(),
            0,
            0,
            5,
            serde_json::to_vec(&json!({"op": "route.future.v2", "extra": 1})).unwrap(),
        )
        .unwrap();
        let out = handler.handle_status_update(endpoint, unknown).unwrap();
        assert!(
            out.is_empty(),
            "unknown push op must be ignored, got {out:?}"
        );

        // A malformed body for a KNOWN op is a real error worth surfacing.
        let malformed = Frame::build(
            FrameType::Push,
            control_flags(),
            0,
            0,
            6,
            serde_json::to_vec(&json!({"op": "route.status"})).unwrap(),
        )
        .unwrap();
        let out = handler.handle_status_update(endpoint, malformed).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&out[0])["code"], "invalid_control_body");
    }

    #[test]
    fn hello_rejected_when_connection_already_owns_client_routes() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        // Commits a client route on connection 202 (bound to a module on conn 101).
        let _ = bind_liveness_route(&registry, &forwarding, "aft-module");
        let client_connection = ConnectionId::new(202);

        // That same connection now tries to register as a module: rejected, so one
        // connection never holds both client-route and module-endpoint state.
        let responses = handler
            .handle_control(
                client_connection,
                hello_frame("aft-second", PROTOCOL_VERSION, 9),
            )
            .unwrap();
        assert_eq!(responses[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&responses[0])["code"], "invalid_hello");
        assert!(registry.get_module("aft-second").unwrap().is_none());
    }

    #[test]
    fn reserved_module_hello_requires_matching_launch_nonce() {
        let registry = Arc::new(Registry::default());
        let supervisor = SupervisorHandle::new();
        // The supervisor recorded the nonce it injected when it spawned the reserved
        // module; the HELLO verifier checks against the same shared handle.
        supervisor.set_reserved_nonce("vault", "the-real-nonce".to_string());
        let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor);

        // A HELLO with NO nonce is rejected.
        let no_nonce = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("vault", PROTOCOL_VERSION, 1),
            )
            .unwrap();
        assert_eq!(no_nonce[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&no_nonce[0])["code"], "reserved_module");
        assert!(registry.get_module("vault").unwrap().is_none());

        // A HELLO with the WRONG nonce is rejected.
        let wrong = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame_with_nonce("vault", PROTOCOL_VERSION, 2, Some("forged")),
            )
            .unwrap();
        assert_eq!(wrong[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&wrong[0])["code"], "reserved_module");
        assert!(registry.get_module("vault").unwrap().is_none());

        // A HELLO with the CORRECT nonce registers.
        let ok = handler
            .handle_control(
                ConnectionId::new(3),
                hello_frame_with_nonce("vault", PROTOCOL_VERSION, 3, Some("the-real-nonce")),
            )
            .unwrap();
        assert_eq!(ok[0].header.ty, FrameType::HelloAck);
        assert!(registry.get_module("vault").unwrap().is_some());
    }

    #[test]
    fn reserved_prefix_hello_uses_delimiter_sensitive_owner_nonce() {
        let registry = Arc::new(Registry::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("federation", "owner-nonce".to_string());
        supervisor.set_reserved_prefixes("federation", &["fed:".to_string()]);
        let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor);

        let squat = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("fed:peerA:tool", PROTOCOL_VERSION, 1),
            )
            .unwrap();
        assert_eq!(squat[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&squat[0])["code"], "reserved_module");
        assert!(parse_error(&squat[0])["message"]
            .as_str()
            .unwrap()
            .contains("fed:"));

        let accepted_peer = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame_with_nonce("fed:peerA:tool", PROTOCOL_VERSION, 2, Some("owner-nonce")),
            )
            .unwrap();
        assert_eq!(accepted_peer[0].header.ty, FrameType::HelloAck);

        let accepted_short = handler
            .handle_control(
                ConnectionId::new(3),
                hello_frame_with_nonce("fed:x", PROTOCOL_VERSION, 3, Some("owner-nonce")),
            )
            .unwrap();
        assert_eq!(accepted_short[0].header.ty, FrameType::HelloAck);

        for (conn, module_id) in [(4, "fedx:tool"), (5, "fed"), (6, "FED:x")] {
            let response = handler
                .handle_control(
                    ConnectionId::new(conn),
                    hello_frame(module_id, PROTOCOL_VERSION, conn),
                )
                .unwrap();
            assert_eq!(response[0].header.ty, FrameType::HelloAck, "{module_id}");
        }
    }

    #[test]
    fn exact_reserved_module_takes_precedence_over_reserved_prefix() {
        let registry = Arc::new(Registry::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("federation", "owner-nonce".to_string());
        supervisor.set_reserved_prefixes("federation", &["fed:".to_string()]);
        supervisor.set_reserved_nonce("fed:special", "exact-nonce".to_string());
        let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor);

        let owner_nonce = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame_with_nonce("fed:special", PROTOCOL_VERSION, 1, Some("owner-nonce")),
            )
            .unwrap();
        assert_eq!(owner_nonce[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&owner_nonce[0])["code"], "reserved_module");
        assert!(registry.get_module("fed:special").unwrap().is_none());

        let exact_nonce = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame_with_nonce("fed:special", PROTOCOL_VERSION, 2, Some("exact-nonce")),
            )
            .unwrap();
        assert_eq!(exact_nonce[0].header.ty, FrameType::HelloAck);
        assert!(registry.get_module("fed:special").unwrap().is_some());
    }

    #[test]
    fn non_reserved_module_ignores_launch_nonce() {
        let registry = Arc::new(Registry::default());
        // No reserved nonce recorded for these ids: they are not reserved, so HELLO
        // registration succeeds whether a spawned process echoes a nonce or not.
        let handler = ControlHandler::new(Arc::clone(&registry));
        let no_nonce = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("aft-no-nonce", PROTOCOL_VERSION, 1),
            )
            .unwrap();
        assert_eq!(no_nonce[0].header.ty, FrameType::HelloAck);
        assert!(registry.get_module("aft-no-nonce").unwrap().is_some());

        let echoed_nonce = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame_with_nonce("aft-with-nonce", PROTOCOL_VERSION, 2, Some("spawn-nonce")),
            )
            .unwrap();
        assert_eq!(echoed_nonce[0].header.ty, FrameType::HelloAck);
        assert!(registry.get_module("aft-with-nonce").unwrap().is_some());
    }

    #[test]
    fn malformed_hello_returns_error_and_handler_still_answers_ping() {
        let handler = ControlHandler::default();
        let conn = ConnectionId::new(1);
        let malformed = Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            3,
            b"{not json".to_vec(),
        )
        .unwrap();

        let error = handler.handle_control(conn, malformed).unwrap();
        assert_eq!(error[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&error[0])["code"], "invalid_hello");

        let ping = Frame::build(FrameType::Ping, control_flags(), 0, 0, 4, Vec::new()).unwrap();
        let pong = handler.handle_control(conn, ping).unwrap();
        assert_eq!(pong[0].header.ty, FrameType::Pong);
        assert_eq!(pong[0].header.corr, 4);
    }

    #[test]
    fn duplicate_module_id_is_rejected_without_replacing_active_registration() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));

        handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("aft", PROTOCOL_VERSION, 1),
            )
            .unwrap();
        let duplicate = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame("aft", PROTOCOL_VERSION, 2),
            )
            .unwrap();

        assert_eq!(duplicate[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&duplicate[0])["code"], "duplicate_module_id");
        let registration = registry.get_module("aft").unwrap().unwrap();
        assert_eq!(registration.connection_id, ConnectionId::new(1));
    }

    #[test]
    fn liveness_poll_reports_false_when_process_liveness_reports_dead() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let process_liveness = Arc::new(FakeProcessLiveness { live: Some(false) });
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_process_liveness(process_liveness);
        let (ctx, route_channel, route_epoch) =
            bind_liveness_route(&registry, &forwarding, "aft-dead");
        let responses = handler
            .handle_route_poll(
                &ctx,
                route_poll_frame(41, PollKind::Liveness, route_channel),
                route_channel,
                route_epoch,
                PollKind::Liveness,
            )
            .unwrap();

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].header.ty, FrameType::Response);
        assert_route_poll_liveness(&responses[0], false);
    }

    #[test]
    fn liveness_poll_without_process_source_uses_bound_route() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (ctx, route_channel, route_epoch) =
            bind_liveness_route(&registry, &forwarding, "aft-bound-only");
        let responses = handler
            .handle_route_poll(
                &ctx,
                route_poll_frame(42, PollKind::Liveness, route_channel),
                route_channel,
                route_epoch,
                PollKind::Liveness,
            )
            .unwrap();

        assert_route_poll_liveness(&responses[0], true);
    }

    #[test]
    fn liveness_poll_untracked_process_source_uses_bound_route() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let process_liveness = Arc::new(FakeProcessLiveness { live: None });
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_process_liveness(process_liveness);
        let (ctx, route_channel, route_epoch) =
            bind_liveness_route(&registry, &forwarding, "aft-untracked");
        let responses = handler
            .handle_route_poll(
                &ctx,
                route_poll_frame(43, PollKind::Liveness, route_channel),
                route_channel,
                route_epoch,
                PollKind::Liveness,
            )
            .unwrap();

        assert_route_poll_liveness(&responses[0], true);
    }

    #[tokio::test]
    async fn unknown_op_returns_unknown_control_op() {
        let handler = ControlHandler::default();
        let (ctx, _rx) = route_ctx(ConnectionId::new(77));
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            55,
            br#"{"op":"route.nope","route_channel":1}"#.to_vec(),
        )
        .unwrap();

        let response = handler.handle_control_frame(&ctx, request).await.unwrap();

        assert_eq!(response.len(), 1);
        assert_eq!(response[0].header.ty, FrameType::Error);
        assert_eq!(response[0].header.corr, 55);
        assert_eq!(parse_error(&response[0])["code"], "unknown_control_op");
    }

    #[tokio::test]
    async fn supervisor_provenance_rejects_unknown_exact_module() {
        let handler = ControlHandler::default();
        let (ctx, _rx) = route_ctx(ConnectionId::new(79));
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            57,
            br#"{"op":"supervisor.provenance","module_id":"missing"}"#.to_vec(),
        )
        .unwrap();

        let response = handler.handle_control_frame(&ctx, request).await.unwrap();

        assert_eq!(response.len(), 1);
        assert_eq!(response[0].header.ty, FrameType::Error);
        assert_eq!(response[0].header.corr, 57);
        let error = parse_error(&response[0]);
        assert_eq!(error["code"], "unknown_module");
        assert_eq!(error["message"], "module_id 'missing' is not supervised");
    }

    #[test]
    fn provenance_probe_override_keeps_handler_tests_deterministic() {
        let expected = subc_control::RunningImageAgreement::Unavailable {
            reason: subc_control::RunningImageUnavailableReason::HashFailed,
        };
        let handler = ControlHandler::default().with_provenance_probe_result(expected.clone());
        assert_eq!(handler.provenance_probe_override, Some(expected));
    }

    #[tokio::test]
    async fn malformed_control_bodies_return_invalid_control_body() {
        let handler = ControlHandler::default();
        let (ctx, _rx) = route_ctx(ConnectionId::new(78));

        for (corr, body) in [
            (56, br#"{"route_channel":1}"#.as_slice()),
            (57, br#"{"op":17,"route_channel":1}"#.as_slice()),
            (
                58,
                br#"{"op":"route.poll","route_channel":"bad","kind":"status"}"#.as_slice(),
            ),
        ] {
            let request = Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                corr,
                body.to_vec(),
            )
            .unwrap();
            let response = handler.handle_control_frame(&ctx, request).await.unwrap();

            assert_eq!(response.len(), 1);
            assert_eq!(response[0].header.ty, FrameType::Error);
            assert_eq!(response[0].header.corr, corr);
            assert_eq!(parse_error(&response[0])["code"], "invalid_control_body");
        }
    }

    #[tokio::test]
    async fn goodbye_tears_down_registration_and_later_channel_is_unknown() {
        let registry = Arc::new(Registry::default());
        let control = Arc::new(ControlHandler::new(Arc::clone(&registry)));
        let router = Router::with_control_handler(Arc::clone(&control));
        let connection = router.begin_connection();
        let (ctx, mut rx) = route_ctx(connection.id());

        router
            .route_for_connection(&ctx, hello_frame("aft", PROTOCOL_VERSION, 11))
            .await
            .unwrap();
        let response = rx.recv().await.unwrap();
        let ack = parse_ack(&response);
        assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
        let channel = 1;

        let goodbye =
            Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 12, Vec::new()).unwrap();
        router.route_for_connection(&ctx, goodbye).await.unwrap();
        assert!(rx.try_recv().is_err());
        assert!(registry.get_module("aft").unwrap().is_none());

        router
            .route_for_connection(&ctx, channel_request(channel, 13))
            .await
            .unwrap();
        let error_frame = rx.recv().await.unwrap();
        assert_eq!(error_frame.header.ty, FrameType::Error);
        assert_eq!(error_frame.header.channel, channel);
    }

    #[tokio::test]
    async fn dropping_router_connection_releases_registration() {
        let registry = Arc::new(Registry::default());
        let control = Arc::new(ControlHandler::new(Arc::clone(&registry)));
        let router = Router::with_control_handler(control);
        let connection = router.begin_connection();
        let (ctx, mut rx) = route_ctx(connection.id());

        router
            .route_for_connection(&ctx, hello_frame("aft", PROTOCOL_VERSION, 31))
            .await
            .unwrap();
        let response = rx.recv().await.unwrap();
        let ack = parse_ack(&response);
        assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
        assert!(registry.get_module("aft").unwrap().is_some());

        drop(connection);

        assert!(registry.get_module("aft").unwrap().is_none());
        assert_eq!(registry.active_registration_count().unwrap(), 0);
    }

    fn capability_manifest(
        module_id: &str,
        provides: &[&str],
        must_never_reach: &[&str],
    ) -> ModuleManifest {
        let mut manifest = manifest(module_id, PROTOCOL_VERSION);
        manifest.capabilities = Some(CapabilityDeclarations {
            provides: provides
                .iter()
                .map(|capability| (*capability).to_string())
                .collect(),
            requires: Vec::new(),
            must_never_reach: must_never_reach
                .iter()
                .map(|capability| (*capability).to_string())
                .collect(),
        });
        manifest
    }

    fn hello_frame_with_manifest(manifest: ModuleManifest, corr: u64) -> Frame {
        Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&ModuleHelloBody {
                protocol_ver: manifest.protocol_ver,
                manifest,
                control_ops: None,
                launch_nonce: None,
            })
            .expect("capability test HELLO serializes"),
        )
        .expect("capability test HELLO frame builds")
    }

    fn catalog_update_with_capabilities_frame(
        corr: u64,
        capabilities: CapabilityDeclarations,
    ) -> Frame {
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&ModuleControlRequestFromModule::CatalogUpdate {
                provides: manifest("catalog-update-placeholder", PROTOCOL_VERSION).provides,
                capabilities: Some(capabilities),
            })
            .expect("capability catalog.update serializes"),
        )
        .expect("capability catalog.update frame builds")
    }

    async fn register_capability_manifest(
        handler: &ControlHandler,
        ctx: &RouteCtx,
        manifest: ModuleManifest,
        corr: u64,
    ) {
        let replies = handler
            .handle_control_frame(ctx, hello_frame_with_manifest(manifest, corr))
            .await
            .expect("capability test HELLO succeeds");
        assert!(
            matches!(replies.as_slice(), [Frame { header, .. }] if header.ty == FrameType::HelloAck),
            "capability test HELLO must register"
        );
    }

    async fn open_route_for_capability_test(
        handler: &ControlHandler,
        target_ctx: &RouteCtx,
        target_rx: &mut mpsc::Receiver<crate::router::OutboundFrame>,
        client_connection_id: u64,
        corr: u64,
        target_module_id: &str,
        consumer_identity: Option<ConsumerIdentity>,
    ) -> (
        mpsc::Receiver<crate::router::OutboundFrame>,
        ModuleControlRequest,
    ) {
        let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(client_connection_id));
        let route_handler = handler.clone();
        let target_module_id = target_module_id.to_string();
        let route_task = tokio::spawn(async move {
            route_handler
                .handle_control_frame(
                    &client_ctx,
                    route_open_frame_with_admission_facts(
                        corr,
                        &target_module_id,
                        consumer_identity,
                        None,
                    ),
                )
                .await
                .expect("capability test route.open succeeds")
        });
        let bind = tokio::time::timeout(Duration::from_secs(1), target_rx.recv())
            .await
            .expect("capability test route.open must reach route.bind")
            .expect("target control receiver stays open");
        let bind_request: ModuleControlRequest =
            serde_json::from_slice(&bind.body).expect("route.bind decodes");
        handler
            .handle_control_frame(target_ctx, route_bind_ack(bind.header.corr))
            .await
            .expect("capability test route.bind ACK succeeds");
        assert!(route_task.await.expect("route.open task joins").is_empty());
        let opened = client_rx
            .recv()
            .await
            .expect("successful route.open publishes a response");
        assert!(matches!(
            serde_json::from_slice::<ClientControlResponse>(&opened.body),
            Ok(ClientControlResponse::RouteOpen { .. })
        ));
        (client_rx, bind_request)
    }

    fn assert_capability_denied_push(frame: Frame, target_module_id: &str) {
        assert_eq!(frame.header.ty, FrameType::Push);
        assert_eq!(frame.header.channel, 0);
        assert_eq!(
            serde_json::from_slice::<ClientControlPush>(&frame.body)
                .expect("route.closed control push decodes"),
            ClientControlPush::RouteClosed {
                module_id: target_module_id.to_string(),
                reason: RouteCloseReason::CapabilityDenied,
                drained: false,
                abandoned: 0,
                terminal: Some(false),
            }
        );
    }

    #[tokio::test]
    async fn route_open_capability_forbidden_mutation_proof_creates_no_route() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("opener", "opener-nonce".to_string());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor);
        let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(700));
        let (opener_ctx, _opener_rx) = route_ctx(ConnectionId::new(701));
        register_capability_manifest(
            &handler,
            &target_ctx,
            capability_manifest("target", &["credentials-provider/v1"], &[]),
            1,
        )
        .await;
        register_capability_manifest(
            &handler,
            &opener_ctx,
            capability_manifest("opener", &[], &["credentials-provider/v1"]),
            2,
        )
        .await;

        let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(702));
        let replies = handler
            .handle_control_frame(
                &client_ctx,
                route_open_frame_with_admission_facts(
                    3,
                    "target",
                    Some(ConsumerIdentity {
                        module_id: "opener".to_string(),
                        launch_nonce: "opener-nonce".to_string(),
                    }),
                    None,
                ),
            )
            .await
            .expect("denied route.open returns a typed frame");
        assert_eq!(parse_error(&replies[0])["code"], "capability_forbidden");
        assert_eq!(forwarding.active_binding_count().unwrap(), 0);
        assert!(
            target_rx.try_recv().is_err(),
            "forbidden route.open must not relay route.bind"
        );
    }

    #[tokio::test]
    async fn capability_deny_edge_hello_mutation_proof_force_closes_existing_route() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("opener", "opener-nonce".to_string());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor);
        let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(710));
        let (old_opener_ctx, _old_opener_rx) = route_ctx(ConnectionId::new(711));
        register_capability_manifest(
            &handler,
            &target_ctx,
            capability_manifest("target", &["credentials-provider/v1"], &[]),
            1,
        )
        .await;
        register_capability_manifest(
            &handler,
            &old_opener_ctx,
            capability_manifest("opener", &[], &[]),
            2,
        )
        .await;
        let (mut client_rx, _) = open_route_for_capability_test(
            &handler,
            &target_ctx,
            &mut target_rx,
            712,
            3,
            "target",
            Some(ConsumerIdentity {
                module_id: "opener".to_string(),
                launch_nonce: "opener-nonce".to_string(),
            }),
        )
        .await;
        assert_eq!(forwarding.active_binding_count().unwrap(), 1);

        handler
            .cleanup_connection(old_opener_ctx.connection_id)
            .expect("old opener registration cleans up");
        let (new_opener_ctx, _new_opener_rx) = route_ctx(ConnectionId::new(713));
        register_capability_manifest(
            &handler,
            &new_opener_ctx,
            capability_manifest("opener", &[], &["credentials-provider/v1"]),
            4,
        )
        .await;

        assert_capability_denied_push(
            client_rx
                .try_recv()
                .expect("HELLO deny addition must emit route.closed")
                .frame,
            "target",
        );
        assert_eq!(forwarding.active_binding_count().unwrap(), 0);
        assert!(matches!(
            target_rx.try_recv(),
            Ok(outbound) if outbound.header.ty == FrameType::Goodbye
        ));
    }

    #[tokio::test]
    async fn capability_claim_catalog_update_mutation_proof_force_closes_existing_route() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("opener", "opener-nonce".to_string());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor);
        let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(720));
        let (opener_ctx, _opener_rx) = route_ctx(ConnectionId::new(721));
        register_capability_manifest(
            &handler,
            &target_ctx,
            capability_manifest("target", &[], &[]),
            1,
        )
        .await;
        register_capability_manifest(
            &handler,
            &opener_ctx,
            capability_manifest("opener", &[], &["credentials-provider/v1"]),
            2,
        )
        .await;
        let (mut client_rx, _) = open_route_for_capability_test(
            &handler,
            &target_ctx,
            &mut target_rx,
            722,
            3,
            "target",
            Some(ConsumerIdentity {
                module_id: "opener".to_string(),
                launch_nonce: "opener-nonce".to_string(),
            }),
        )
        .await;
        assert_eq!(forwarding.active_binding_count().unwrap(), 1);

        let replies = handler
            .handle_control_frame(
                &target_ctx,
                catalog_update_with_capabilities_frame(
                    4,
                    CapabilityDeclarations {
                        provides: vec!["credentials-provider/v1".to_string()],
                        requires: Vec::new(),
                        must_never_reach: Vec::new(),
                    },
                ),
            )
            .await
            .expect("claim catalog.update succeeds");
        assert!(matches!(
            serde_json::from_slice::<ModuleControlResponseToModule>(&replies[0].body),
            Ok(ModuleControlResponseToModule::CatalogUpdate {})
        ));
        assert_capability_denied_push(
            client_rx
                .try_recv()
                .expect("claim addition must emit route.closed")
                .frame,
            "target",
        );
        assert_eq!(forwarding.active_binding_count().unwrap(), 0);
        assert!(matches!(
            target_rx.try_recv(),
            Ok(outbound) if outbound.header.ty == FrameType::Goodbye
        ));
    }

    #[tokio::test]
    async fn capability_claim_removal_mutation_proof_keeps_route_open_without_close_frame() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("opener", "opener-nonce".to_string());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor);
        let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(730));
        let (opener_ctx, _opener_rx) = route_ctx(ConnectionId::new(731));
        register_capability_manifest(
            &handler,
            &target_ctx,
            capability_manifest("target", &["credentials-provider/v1"], &[]),
            1,
        )
        .await;
        register_capability_manifest(
            &handler,
            &opener_ctx,
            capability_manifest("opener", &[], &[]),
            2,
        )
        .await;
        let (mut client_rx, _) = open_route_for_capability_test(
            &handler,
            &target_ctx,
            &mut target_rx,
            732,
            3,
            "target",
            Some(ConsumerIdentity {
                module_id: "opener".to_string(),
                launch_nonce: "opener-nonce".to_string(),
            }),
        )
        .await;
        assert_eq!(forwarding.active_binding_count().unwrap(), 1);

        handler
            .handle_control_frame(
                &target_ctx,
                catalog_update_with_capabilities_frame(
                    4,
                    CapabilityDeclarations {
                        provides: Vec::new(),
                        requires: Vec::new(),
                        must_never_reach: Vec::new(),
                    },
                ),
            )
            .await
            .expect("claim removal catalog.update succeeds");
        assert_eq!(
            forwarding.active_binding_count().unwrap(),
            1,
            "removing an attested target claim must leave the route census unchanged"
        );
        assert!(
            client_rx.try_recv().is_err(),
            "claim removal must not emit route.closed capability_denied"
        );
        assert!(
            target_rx.try_recv().is_err(),
            "claim removal must not send the target a route GOODBYE"
        );
    }

    /// A direct client may open a route to a denied capability provider; this
    /// policy applies only to attested supervised module origins, not to direct clients.
    #[tokio::test]
    async fn direct_client_scope_honesty_mutation_proof_opens_denied_capability_provider() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("opener", "opener-nonce".to_string());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor);
        let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(740));
        let (opener_ctx, _opener_rx) = route_ctx(ConnectionId::new(741));
        register_capability_manifest(
            &handler,
            &target_ctx,
            capability_manifest("target", &["credentials-provider/v1"], &[]),
            1,
        )
        .await;
        register_capability_manifest(
            &handler,
            &opener_ctx,
            capability_manifest("opener", &[], &["credentials-provider/v1"]),
            2,
        )
        .await;

        let (_client_rx, bind) = open_route_for_capability_test(
            &handler,
            &target_ctx,
            &mut target_rx,
            742,
            3,
            "target",
            None,
        )
        .await;
        let ModuleControlRequest::RouteBind { principal, .. } = bind else {
            panic!("direct scope-honesty route must bind");
        };
        assert_eq!(principal, Some(Principal::Direct));
        assert_eq!(forwarding.active_binding_count().unwrap(), 1);
    }

    /// A module that denies a capability receives no self-route exemption when it
    /// also attestedly provides that capability.
    #[tokio::test]
    async fn must_never_reach_self_route_is_capability_forbidden() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("self-provider", "self-nonce".to_string());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor);
        let (self_ctx, mut self_rx) = route_ctx(ConnectionId::new(750));
        register_capability_manifest(
            &handler,
            &self_ctx,
            capability_manifest(
                "self-provider",
                &["credentials-provider/v1"],
                &["credentials-provider/v1"],
            ),
            1,
        )
        .await;

        let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(751));
        let replies = handler
            .handle_control_frame(
                &client_ctx,
                route_open_frame_with_admission_facts(
                    2,
                    "self-provider",
                    Some(ConsumerIdentity {
                        module_id: "self-provider".to_string(),
                        launch_nonce: "self-nonce".to_string(),
                    }),
                    None,
                ),
            )
            .await
            .expect("self-route refusal returns a typed frame");
        assert_eq!(parse_error(&replies[0])["code"], "capability_forbidden");
        assert_eq!(forwarding.active_binding_count().unwrap(), 0);
        assert!(
            self_rx.try_recv().is_err(),
            "self denial must not relay route.bind"
        );
    }

    #[test]
    fn unsupported_channel_zero_frame_returns_error() {
        let handler = ControlHandler::default();
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            21,
            b"opaque".to_vec(),
        )
        .unwrap();

        let response = handler
            .handle_control(ConnectionId::new(1), request)
            .unwrap();

        assert_eq!(response[0].header.ty, FrameType::Error);
        assert_eq!(
            parse_error(&response[0])["code"],
            "unsupported_control_frame"
        );
    }
}

#[cfg(test)]
mod concurrency_default_exposure_tests {
    use super::*;

    fn hello_body(role_json: &str) -> Vec<u8> {
        format!(
            r#"{{"protocol_ver":2,"module_id":"m","manifest":{{"module_id":"m","module_version":"1.0.0","protocol_ver":2,"trust_tier":"first_party","provides":[{role_json}],"consumes":[],"bindings":{{"storage":{{"kind":"sqlite","scope":"project","owns_schema":false}},"vault_grants":[],"identity":{{"requires":[],"optional":[]}}}}}}}}"#
        )
        .into_bytes()
    }

    fn manifest_from(body: &[u8]) -> ModuleManifest {
        let value: serde_json::Value = serde_json::from_slice(body).expect("hello parses");
        serde_json::from_value(value.get("manifest").expect("manifest key").clone())
            .expect("manifest parses")
    }

    const SURFACE_TAIL: &str = r#""operations":[],"config_schema":{"type":"object"},"observability":[],"identity_scope":[]"#;

    #[test]
    fn absent_concurrency_on_management_surface_is_reported_as_defaulted() {
        let body = hello_body(&format!(
            r#"{{"role":"management_surface",{SURFACE_TAIL}}}"#
        ));
        let manifest = manifest_from(&body);
        // Precondition: serde really resolved it to the default, so the typed
        // manifest alone cannot answer the question this probe exists for.
        assert_eq!(manifest_concurrency(&manifest), Concurrency::ModuleManaged);
        assert!(manifest_concurrency_was_defaulted(&body, &manifest));
    }

    #[test]
    fn declared_concurrency_is_not_reported_even_when_it_equals_the_default() {
        let body = hello_body(&format!(
            r#"{{"role":"management_surface",{SURFACE_TAIL},"concurrency":"module_managed"}}"#
        ));
        let manifest = manifest_from(&body);
        assert_eq!(manifest_concurrency(&manifest), Concurrency::ModuleManaged);
        assert!(!manifest_concurrency_was_defaulted(&body, &manifest));
    }

    #[test]
    fn non_management_roles_are_never_reported() {
        let body = hello_body(
            r#"{"role":"internal_service","service_id":"s","transport":"bulk","agent_facing":false,"operations":[]}"#,
        );
        let manifest = manifest_from(&body);
        assert!(!manifest_concurrency_was_defaulted(&body, &manifest));
    }
}
