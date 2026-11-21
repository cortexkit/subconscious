#[path = "control/list_diagnostics.rs"]
pub(crate) mod list_diagnostics;
mod route_open_refusal_log;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant as StdInstant},
};

use crate::scopes::admission::denied_capability;
use serde::{Deserialize, Serialize};
use subc_control::{
    ops, CapabilityRequirementStatus, CatalogEntry, ClientControlPush, ClientControlRequest,
    ClientControlResponse, ConsumerIdentity, DaemonBuildProvenance, DaemonObservedProcess,
    ModuleDeclaredProvenance, ModuleProtocol, NotReadyReason, PendingReloadVerdict, PollKind,
    ReloadPathAgreement, ReloadPathUnavailableReason, RouteCloseReason, SpawnCursor,
    StderrCaptureState, StderrTail, StderrTailEntry, SupervisorDaemonProvenance, SupervisorEntry,
    SupervisorHealthEntry, SupervisorModuleProvenance, SupervisorObservedProcess,
    SupervisorRescanResult, SupervisorRoute, SupervisorRouteConsumer, SupervisorRouteModule,
};
use subc_protocol::{
    call_mode::{derive_call_mode, CALL_MODES_CAPABILITY},
    error_codes,
    manifest::{
        validate_hello_capability_grammar, validate_hello_confirm_operations,
        validate_hello_display_name, validate_hello_event_declarations,
        validate_hello_flow_operations, validate_hello_operation_deferral,
        validate_hello_rich_ui_descriptor, validate_hello_role_declarations,
        validate_hello_self_signal_declarations, validate_role_declarations_value,
        CapabilityDeclarations, CapabilityNeed, Concurrency, ManifestProvenance, ModuleManifest,
        ProviderRole,
    },
    peer::{
        ObservedPeer, RouteBindPeer, OBSERVED_PEER_CAPABILITY, OPERATOR_APP_CAPABILITY,
        PEER_BUILD_CAPABILITY,
    },
    provider_declarations::ProviderDeclarations,
    scope::{
        ScopeEnd, ScopeRecord, ScopeRecordOutcome, ScopeRecordResult, ScopeSelector,
        CAP_ROUTE_ROLE_VERSIONS_V1, CAP_SCOPES_V1, SCOPE_APPLY_OP, SCOPE_DESCRIBE_OP,
        SCOPE_SYNC_OP,
    },
    session::{
        validate_role_versions, HealthReport, ModuleControlPush, ModuleControlRequest,
        ModuleControlRequestFromModule, ModuleControlResponse, ModuleControlResponseToModule,
        OperatorConfirmRequest, MODULE_CONTROL_OP_HEALTH_CHECK, MODULE_TO_SUBC_OP_CATALOG_UPDATE,
        MODULE_TO_SUBC_OP_CATALOG_UPDATE_ROLE_DECLARATIONS, ROLE_VERSIONS_FIELD,
    },
    BindIdentity, ErrorBody, Flags, FrameType, ModuleHelloAckBody, ModuleHelloBody, Principal,
    Priority, RouteTarget, PROTOCOL_VERSION,
};
use tokio::time::{timeout_at, Instant};
use tracing::{debug, info, instrument::WithSubscriber, warn};

use crate::{
    caller_identity::CallerIdentity,
    capability_requirements::{
        log_duplicate_claim_events, log_manifest_change, log_requirement_events,
        CapabilityRequirementEvaluator, CapabilityVerdict, DuplicateClaimSource, RegisteredModule,
        RequirementStatus, RuntimeModule,
    },
    child_roster::DaemonShutdownFlag,
    daemon_config::{PrivilegedOperatorPrompt, RestartRequiredSection},
    forwarding::{
        CloseReason, EndpointRoute, ForwardingError, ForwardingTable, GoodbyeTarget,
        ModuleControlRpcCompletion, ModuleControlRpcOutcome, ModuleEndpointId,
        PendingModuleControlRpc, RouteBindRelayOutcome, RoutePollSnapshot, RouteRelease,
    },
    observability::{
        ROUTE_OPEN_REFUSED_DECLARED_NOT_READY, ROUTE_OPEN_REFUSED_REQUIRED_CAPABILITY_UNPROVIDED,
    },
    privileged_prompt::{prompt_text, PrivilegedOperation},
    provenance::{
        process_start_time, spawned_file_identity, ExecutableIdentityProbe, SpawnedFileIdentity,
    },
    registry::{ChannelState, ConnectionId, RegistrationEndReason, Registry, RegistryError},
    router::{daemon_error_flags, RouteCtx, RouterError},
    scopes::{HelloLaunchNonces, ScopeRecords, ScopeTable},
    server::MAX_PENDING_ROUTE_BINDS_PER_TARGET,
    stderr_tail::{CaptureState, TailEntry},
    supervise::{
        validate_spec, ModuleProcessLiveness, ReservedHelloRejection, ReservedHelloVerification,
        SpawnSubscribeRefusal, SupervisorHandle, SwapHelloAdmission,
    },
    ConnectedClients, DaemonCounters, Frame, ProjectRootId, Supervisor,
};

/// Lowest envelope version this subc build will negotiate.
///
/// Module HELLO negotiation is exact: peers must use the daemon's locked
/// protocol version. Older and newer peers receive `version_unsupported` and
/// are not registered.
pub const MIN_SUPPORTED_VERSION: u8 = PROTOCOL_VERSION;

mod route_open;
use route_open::is_routable_role;
use route_open::principal_label;
#[cfg(test)]
use route_open::RouteBindAdmission;
pub(crate) use route_open::RouteBindBreakers;
pub(crate) use route_open::RouteBindConcurrency;
use route_open::RouteOpenCaller;
use route_open::RouteOpenRequest;
use route_open::RouteOpenTimings;
pub use route_open::DEFAULT_ROUTE_BIND_BREAKER_COOLDOWN;
pub use route_open::DEFAULT_ROUTE_BIND_BREAKER_THRESHOLD;
pub use route_open::DEFAULT_ROUTE_BIND_RELAY_TIMEOUT;

mod server_describe;
use server_describe::subc_ops;
use server_describe::CAP_ADMISSION_FACTS_RELAY;
use server_describe::CAP_CHANNEL_LIFECYCLE;
use server_describe::CAP_MANIFEST_REGISTRATION;
use server_describe::CAP_PING_PONG;
use server_describe::CAP_SESSION_ATTACH;

mod supervisor_operations;
use supervisor_operations::launch_spec_agreement;
#[cfg(test)]
use supervisor_operations::reload_verdict;
use supervisor_operations::DaemonProvenanceFacts;
use supervisor_operations::ModuleControlRpcGuard;
use supervisor_operations::DEFAULT_HEALTH_PROBE_TIMEOUT;

mod activity;
#[cfg(test)]
use activity::ActivityRequestPause;

mod scopes;
#[cfg(test)]
use scopes::ScopeOutcomeCounts;

mod module_operations;
use module_operations::is_known_module_request_op;
use module_operations::manifest_concurrency;
#[cfg(test)]
use module_operations::manifest_concurrency_was_defaulted;
#[cfg(test)]
use module_operations::module_baseline_control_ops;
use module_operations::module_control_request_op;
use module_operations::module_registration_grants_op;
use module_operations::parse_module_control_request_from_module;
#[cfg(all(test, unix, feature = "test-support"))]
use module_operations::MODULE_TO_SUBC_UNADVERTISED_OPS;
mod advertised_ops;
use advertised_ops::*;
mod caller_attestation;
mod channel_operations;
#[cfg(all(test, unix, feature = "test-support"))]
#[path = "control/channel_operations_tests.rs"]
mod channel_operations_tests;
pub(crate) mod claims;
#[cfg(test)]
#[path = "control/config_advertisement_tests.rs"]
mod config_advertisement_tests;
mod config_gate;
#[path = "operator_apps/connection.rs"]
mod peer_identity;
use caller_attestation::{caller_for_target, scope_acts_for_another};
mod activity_reservations;
#[cfg(all(test, unix, feature = "test-support"))]
#[path = "control/caller_attestation_tests.rs"]
mod caller_attestation_tests;
mod capability_census;
use capability_census::append_capability_problem_detail;
use capability_census::capability_census_trigger;
mod route_bind_settlement;
mod scope_expiry;
mod scope_watch;
mod usage;

mod agent_commands;
pub(crate) mod catalog;
mod policy_root;
mod rescan_checked;
use rescan_checked::RescanMode;
use rescan_checked::SupervisorRescanContext;

const SLOW_CONTROL_DISPATCH_THRESHOLD: Duration = Duration::from_secs(1);

/// A test hold: signal arrival, then wait for release on the holding thread.
#[cfg(test)]
type TestPause = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

/// A test hook run once at a fixed point in a handler.
#[cfg(test)]
type TestHook = Box<dyn FnOnce() + Send>;

/// A test hold for async code: signal arrival, then await release.
#[cfg(test)]
type AsyncTestPause = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

/// Real channel-0 control handler for subc itself.
#[derive(Clone)]
pub struct ControlHandler {
    #[cfg(test)]
    activity_request_pause: Arc<Mutex<Option<Arc<ActivityRequestPause>>>>,
    /// Holds connection cleanup at a numbered point: 0 is just after the agent
    /// declaration queue closes, 1 just after the registration channel is
    /// forgotten.
    #[cfg(test)]
    cleanup_pause: Arc<Mutex<Option<(u8, TestPause)>>>,
    /// Runs once in `route.open`'s launch-nonce attestation, right after the
    /// nonce is resolved, to interleave a nonce withdrawal there.
    #[cfg(test)]
    after_nonce_attestation: Arc<Mutex<Option<TestHook>>>,
    /// Holds a registration channel's HELLO after its reader resolved the
    /// channel and before the blocking HELLO handler starts.
    #[cfg(test)]
    hello_ingress_pause: Arc<Mutex<Option<AsyncTestPause>>>,
    /// Holds an agent command declaration at a numbered point in its handler.
    #[cfg(test)]
    agent_commands_pause: Arc<Mutex<Option<(u8, TestPause)>>>,
    claims: Arc<claims::Claims>,
    registry: Arc<Registry>,
    control_usage: Arc<Mutex<usage::ControlUsage>>,
    catalog_cache: Arc<Mutex<catalog::CatalogCache>>,
    catalog_readiness_revision: Arc<std::sync::atomic::AtomicU64>,
    config_runtime: Option<Arc<crate::config_gate::runtime::RuntimeOwner>>,
    config_review_admission: Arc<dyn crate::config_gate::ReviewAdmission>,
    config_review_admission_is_default: bool,
    activity: Option<crate::activity::ActivityLedger>,
    activity_streams: Arc<Mutex<HashMap<(ConnectionId, u64), tokio::task::AbortHandle>>>,
    activity_reservations: activity_reservations::ActivityReservations,
    catalog_streams: Arc<Mutex<HashMap<(ConnectionId, u64), tokio::task::AbortHandle>>>,
    config_settings_streams: Arc<Mutex<HashMap<(ConnectionId, u64), tokio::task::AbortHandle>>>,
    scope_streams: Arc<Mutex<HashMap<(ConnectionId, u64), tokio::task::AbortHandle>>>,
    forwarding: Arc<ForwardingTable>,
    process_liveness: Option<Arc<dyn ModuleProcessLiveness>>,
    supervisor: SupervisorHandle,
    pub(crate) resource_tracker: Arc<crate::resource_totals::Tracker>,
    agent_declarations: agent_commands::Queues,
    list_admission: Arc<tokio::sync::Semaphore>,
    subc_capabilities: Arc<[String]>,
    /// Daemon-wide route.bind relay budget. Used as the fallback when the
    /// target module has no per-module override in
    /// `route_bind_relay_timeouts`.
    route_bind_relay_timeout: Duration,
    /// Per-module route.bind relay budget overrides, keyed by module id. When
    /// `handle_route_open` resolves the deadline for a target module, a
    /// per-module entry wins over the daemon-wide value above.
    route_bind_relay_timeouts: BTreeMap<String, Duration>,
    /// Per-target-module bind-relay breaker state. Shared with the forwarding
    /// table, which is where a new module connection resets it.
    route_bind_breakers: RouteBindBreakers,
    /// Live relay admissions keyed by target module. Shared through the
    /// forwarding table so cloned or separately built handlers enforce one cap.
    route_bind_concurrency: RouteBindConcurrency,
    /// Start and end of each module's not-serving period as seen by
    /// `route.open`, so an outage gets one line at each edge instead of only
    /// the per-refusal INFO lines. Taken from the forwarding table, so every
    /// handler built over one table shares it.
    route_outages: Arc<crate::route_outage::RouteOutageTracker>,
    route_refusal_log: route_open_refusal_log::RefusalLog,
    /// Consecutive relay timeouts that open a module's breaker.
    route_bind_breaker_threshold: u32,
    /// How long a breaker stays open before one probe is admitted.
    route_bind_breaker_cooldown: Duration,
    health_probe_timeout: Duration,
    /// Central storage policy. When set, each registering module receives its
    /// resolved storage descriptor in HELLO_ACK; `None` leaves the field absent.
    storage_config: Option<crate::daemon_config::StorageConfig>,
    /// The machine id established at boot, served on every HELLO_ACK and on
    /// `server.describe`. Fixed for the daemon's lifetime: `ck machine adopt`
    /// changes the file, never this value. `None` serves no id.
    machine_id: Option<crate::machine_id::MachineId>,
    admission_facts_carrier_module_id: Option<String>,
    admission_facts_targets: Option<Vec<String>>,
    /// Scope records with their sync authorities and tombstones; see
    /// `crate::scopes`. Shared by clones of this handler, so every connection
    /// reads and writes one table.
    scopes: Arc<RwLock<ScopeTable>>,
    /// Wakes the expiry loop when an accepted scope change may have added a
    /// deadline, so the loop can wait without ticking while no live scope has
    /// one. A permit is stored when nothing waits, so no wake is lost.
    scope_deadline_added: Arc<tokio::sync::Notify>,
    /// The configured `scope_authority_owners`, kept so a rescan can report a
    /// changed value as needing a daemon restart; rescan never applies it.
    scope_authority_owners: Vec<String>,
    federated_scope_owner: String,
    /// Fixed at startup so rescan cannot change the policy that guards rescan itself.
    privileged_operator_prompt: PrivilegedOperatorPrompt,
    /// The local and peer address of each accepted TCP client connection, read
    /// from the daemon's own socket, so a privileged request can name its caller.
    caller_endpoints: Arc<Mutex<HashMap<ConnectionId, (SocketAddr, SocketAddr)>>>,
    build_class_warned: Arc<Mutex<HashSet<ConnectionId>>>,
    observed_peers: Arc<Mutex<HashMap<ConnectionId, ObservedPeer>>>,
    captured_peers: Arc<Mutex<HashMap<ConnectionId, Arc<peer_identity::PeerCapture>>>>,
    config_desktop_stamps:
        Arc<Mutex<HashMap<ConnectionId, crate::operator_apps::OperatorAppStamp>>>,
    local_connections: Arc<Mutex<HashSet<ConnectionId>>>,
    pub(crate) operator_app_policy: crate::operator_apps::OperatorAppPolicy,
    pub(crate) operator_apps:
        Arc<Mutex<HashMap<ConnectionId, crate::operator_apps::OperatorAppStamp>>>,
    /// One launch entry per app, behind a leaf lock: no other lock is taken
    /// while it is held. Entries are memory-only, so a daemon or
    /// installed-service restart drops them and a relaunched daemon never
    /// mistakes an older process for its own launch.
    pub(crate) launch_table: std::sync::Arc<crate::app_open::LaunchTable>,
    /// Serializes `app.open` launches: the liveness check in the decision and
    /// the later publish must not interleave across concurrent calls, or two
    /// calls could both launch and the second publish would orphan the first
    /// process. Refusals and `already_running` answers share the same lock;
    /// `app.open` is rare, so the cost is negligible.
    pub(crate) app_open_launch_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    // Portable wire samples exercise dispatch without starting an OS process.
    #[cfg(test)]
    pub(crate) app_open_test_platform: Option<crate::operator_apps::AppOpenPlatform>,
    #[cfg(test)]
    pub(crate) app_open_test_launch: Option<fn() -> Result<u32, crate::app_open::AppOpenRefusal>>,
    #[cfg(test)]
    pub(crate) config_review_test_reply: Option<subc_protocol::config::ConfigReviewReply>,
    #[cfg(test)]
    recovery_exit_test_after_step: Option<fn(usize) -> Result<(), crate::config_gate::GateError>>,
    #[cfg(test)]
    recovery_exit_test_completed: Option<fn(u64)>,
    #[cfg(test)]
    apply_reconcile_pause: Arc<Mutex<Option<AsyncTestPause>>>,
    #[cfg(test)]
    recovery_exit_test_quiet_retry: bool,
    #[cfg(test)]
    recovery_exit_test_retry_pause: Option<fn(u64) -> Option<PathBuf>>,
    #[cfg(test)]
    config_delivery_test_wakes:
        Option<tokio::sync::mpsc::UnboundedSender<(&'static str, tokio::time::Instant)>>,
    #[cfg(all(test, unix))]
    config_delivery_test_idle:
        Option<tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>>,
    /// Test-only Windows signature and flash hooks. Production builds carry
    /// neither: with the publisher unset, the production table never reaches
    /// verification, and the default attention hook only logs.
    #[allow(dead_code)]
    #[cfg(windows)]
    pub(crate) windows_verify: Option<crate::app_open::WindowsVerifyFn>,
    #[allow(dead_code)]
    #[cfg(windows)]
    pub(crate) windows_flash: Option<crate::app_open::WindowsFlashFn>,
    /// Test-only tracking fault hook: makes opening the tracking handle on
    /// the launched pid fail, so a test can prove an untrackable launch is
    /// stopped rather than left running.
    #[allow(dead_code)]
    #[cfg(windows)]
    pub(crate) windows_track: Option<crate::app_open::WindowsTrackFn>,
    /// Test-only count of Windows spawn attempts, so a test can prove a
    /// refused launch started no process. Unit tests only.
    #[cfg(all(test, windows))]
    pub(crate) windows_spawn_attempts: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(test)]
    caller_lookup: Option<TestCallerLookup>,
    /// The launch nonce each module connection presented at HELLO, which is how
    /// a `scope.sync` is matched to the owner's current launch.
    hello_launch_nonces: Arc<Mutex<HelloLaunchNonces>>,
    rescan: Option<SupervisorRescanContext>,
    connected_clients: ConnectedClients,
    counters: DaemonCounters,
    /// Why this daemon serves no local endpoint, when binding one failed. `None`
    /// when the endpoint is served, and also on a handler that was never told.
    local_endpoint_unavailable: Option<String>,
    capability_evaluator: Arc<CapabilityRequirementEvaluator>,
    /// The supervisor's daemon-shutdown flag. Once it is set, requirement
    /// changes are logged at INFO with `cause=daemon_shutdown` instead of at
    /// their usual severity (see [`log_requirement_events`]). A handler built
    /// without one has a flag that is never set.
    daemon_shutdown: DaemonShutdownFlag,
    daemon_provenance: DaemonProvenanceFacts,
    #[cfg(test)]
    control_dispatch_delay: Option<Duration>,
    #[cfg(test)]
    provenance_probe_override: Option<subc_control::RunningImageAgreement>,
    /// Holds `route.open` after the route.bind is queued to the module and
    /// before it is marked enqueued, so a test can make the module's reply (or
    /// a client teardown) land exactly inside that window.
    #[cfg(test)]
    route_bind_relay_sent_hold: Option<Arc<tokio::sync::Barrier>>,
    /// Holds a HELLO after its registry entry is reserved and before its
    /// forwarding endpoint is installed, so a test can read the catalog and
    /// open routes while the module is registered but not yet routable. A
    /// blocking barrier because the HELLO handler is synchronous; the test
    /// runs the HELLO on its own thread.
    #[cfg(test)]
    hello_registry_to_forwarding_hold: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    hello_ack_to_publication_hold: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    hello_before_publication_hold: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    hello_after_launch_check_hold: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    hello_inside_publication_hold: Option<Arc<std::sync::Barrier>>,
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

impl ControlHandler {
    pub(crate) fn install_registration_service(
        &self,
        router: std::sync::Weak<crate::Router>,
        auth: crate::server::ServerAuth,
    ) {
        self.supervisor.install_registration_service(router, auth);
    }
    pub fn new(registry: Arc<Registry>) -> Self {
        Self::with_forwarding(registry, Arc::new(ForwardingTable::default()))
    }

    pub fn with_forwarding(registry: Arc<Registry>, forwarding: Arc<ForwardingTable>) -> Self {
        let counters = forwarding.counters();
        // Taken from the forwarding table rather than created here, so that the
        // breaker a `route.open` consults is the same one a module's
        // registration resets, however many handlers are built over one table.
        let route_bind_breakers = forwarding.route_bind_breakers();
        let route_bind_concurrency = forwarding.route_bind_concurrency();
        let route_outages = forwarding.route_outages();
        Self {
            registry,
            control_usage: Arc::new(Mutex::new(usage::ControlUsage::default())),
            catalog_cache: Arc::new(Mutex::new(catalog::CatalogCache::default())),
            catalog_readiness_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(test)]
            activity_request_pause: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            cleanup_pause: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            after_nonce_attestation: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            hello_ingress_pause: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            agent_commands_pause: Arc::new(Mutex::new(None)),
            claims: Arc::new(claims::Claims::default()),
            config_runtime: None,
            config_review_admission: Arc::new(crate::config_gate::MissingReviewAdmission),
            config_review_admission_is_default: true,
            activity: None,
            activity_streams: Arc::new(Mutex::new(HashMap::new())),
            activity_reservations: Default::default(),
            catalog_streams: Arc::new(Mutex::new(HashMap::new())),
            config_settings_streams: Arc::new(Mutex::new(HashMap::new())),
            scope_streams: Arc::new(Mutex::new(HashMap::new())),
            forwarding,
            process_liveness: None,
            supervisor: SupervisorHandle::new(),
            resource_tracker: Arc::new(crate::resource_totals::Tracker::new(
                env!("CARGO_PKG_VERSION").to_owned(),
            )),
            agent_declarations: agent_commands::Queues::default(),
            list_admission: Arc::new(tokio::sync::Semaphore::new(4)),
            subc_capabilities: Arc::from([
                CAP_MANIFEST_REGISTRATION.to_string(),
                CAP_CHANNEL_LIFECYCLE.to_string(),
                CAP_PING_PONG.to_string(),
                CAP_SESSION_ATTACH.to_string(),
                CAP_ADMISSION_FACTS_RELAY.to_string(),
                CAP_SCOPES_V1.to_string(),
                subc_protocol::scope::SCOPE_FEDERATED_CAPABILITY.to_string(),
                subc_protocol::scope::SCOPE_FEDERATED_V2_CAPABILITY.to_string(),
                subc_protocol::peer::CALLER_ATTESTATION_CAPABILITY.to_string(),
                PEER_BUILD_CAPABILITY.to_string(),
                CAP_ROUTE_ROLE_VERSIONS_V1.to_string(),
                subc_protocol::manifest::CAP_CATALOG_EVENTS_V1.to_string(),
                subc_protocol::manifest::CAP_SCOPE_BIND_PROOF_V1.to_string(),
                subc_protocol::manifest::CAP_SCOPE_BIND_PROOF_V2.to_string(),
                subc_protocol::manifest::CAP_SCOPE_REPLACE_IF_IDLE_V1.to_string(),
                subc_protocol::scope::SCOPE_ON_BEHALF_OF_CAPABILITY.to_string(),
                subc_protocol::scope::WORKER_RUNS_CAPABILITY.to_string(),
                subc_protocol::scope::SCOPE_WORKDIR_CAPABILITY.to_string(),
                subc_protocol::scope::FLOW_IDENTITY_SCOPES_CAPABILITY.to_string(),
                subc_protocol::scope::SCOPE_SERVED_BY_CAPABILITY.to_string(),
                subc_control::ACTIVITY_LEDGER_CAPABILITY.to_string(),
                subc_protocol::ROUTE_CLOSE_REASONS_CAPABILITY.to_string(),
            ]),
            route_bind_relay_timeout: DEFAULT_ROUTE_BIND_RELAY_TIMEOUT,
            route_bind_relay_timeouts: BTreeMap::new(),
            route_bind_breakers,
            route_bind_concurrency,
            route_outages,
            route_refusal_log: Default::default(),
            route_bind_breaker_threshold: DEFAULT_ROUTE_BIND_BREAKER_THRESHOLD,
            route_bind_breaker_cooldown: DEFAULT_ROUTE_BIND_BREAKER_COOLDOWN,
            health_probe_timeout: DEFAULT_HEALTH_PROBE_TIMEOUT,
            storage_config: None,
            machine_id: None,
            admission_facts_carrier_module_id: None,
            admission_facts_targets: None,
            scopes: Arc::new(RwLock::new(ScopeTable::new(
                crate::daemon_config::default_scope_authority_owners(),
            ))),
            scope_deadline_added: Arc::new(tokio::sync::Notify::new()),
            scope_authority_owners: crate::daemon_config::default_scope_authority_owners(),
            federated_scope_owner: crate::daemon_config::default_federated_scope_owner(),
            privileged_operator_prompt: PrivilegedOperatorPrompt::default(),
            caller_endpoints: Arc::new(Mutex::new(HashMap::new())),
            build_class_warned: Arc::new(Mutex::new(HashSet::new())),
            observed_peers: Arc::new(Mutex::new(HashMap::new())),
            captured_peers: Arc::new(Mutex::new(HashMap::new())),
            config_desktop_stamps: Arc::new(Mutex::new(HashMap::new())),
            local_connections: Arc::new(Mutex::new(HashSet::new())),
            operator_app_policy: Default::default(),
            operator_apps: Arc::new(Mutex::new(HashMap::new())),
            launch_table: std::sync::Arc::new(crate::app_open::LaunchTable::new()),
            app_open_launch_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(test)]
            app_open_test_platform: None,
            #[cfg(test)]
            app_open_test_launch: None,
            #[cfg(test)]
            config_review_test_reply: None,
            #[cfg(test)]
            recovery_exit_test_after_step: None,
            #[cfg(test)]
            recovery_exit_test_completed: None,
            #[cfg(test)]
            apply_reconcile_pause: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            recovery_exit_test_quiet_retry: false,
            #[cfg(test)]
            recovery_exit_test_retry_pause: None,
            #[cfg(test)]
            config_delivery_test_wakes: None,
            #[cfg(all(test, unix))]
            config_delivery_test_idle: None,
            #[cfg(windows)]
            windows_verify: None,
            #[cfg(windows)]
            windows_flash: None,
            #[cfg(windows)]
            windows_track: None,
            #[cfg(all(test, windows))]
            windows_spawn_attempts: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(test)]
            caller_lookup: None,
            hello_launch_nonces: Arc::new(Mutex::new(HelloLaunchNonces::default())),
            rescan: None,
            connected_clients: ConnectedClients::new(),
            local_endpoint_unavailable: None,
            counters,
            capability_evaluator: Arc::new(CapabilityRequirementEvaluator::new()),
            daemon_shutdown: DaemonShutdownFlag::default(),
            daemon_provenance: DaemonProvenanceFacts::default(),
            #[cfg(test)]
            control_dispatch_delay: None,
            #[cfg(test)]
            provenance_probe_override: None,
            #[cfg(test)]
            route_bind_relay_sent_hold: None,
            #[cfg(test)]
            hello_registry_to_forwarding_hold: None,
            #[cfg(test)]
            hello_ack_to_publication_hold: None,
            #[cfg(test)]
            hello_before_publication_hold: None,
            #[cfg(test)]
            hello_after_launch_check_hold: None,
            #[cfg(test)]
            hello_inside_publication_hold: None,
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

    /// Set the machine id served to every registering module (HELLO_ACK) and on
    /// `server.describe`.
    pub fn with_machine_id(mut self, machine_id: Option<crate::machine_id::MachineId>) -> Self {
        self.machine_id = machine_id;
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

    /// Set the module ids whose scopes may carry `agent_id` and `delegates`.
    /// Replaces the scope table with an empty one under the new list, so call it
    /// while building the handler, before any module can sync.
    pub fn with_scope_authority_owners(mut self, owners: Vec<String>) -> Self {
        self.scopes = Arc::new(RwLock::new(
            ScopeTable::new(owners.iter().cloned())
                .with_federated_owner(self.federated_scope_owner.clone()),
        ));
        self.scope_authority_owners = owners;
        self
    }

    /// Set the exclusive federated owner while building the handler, before syncs.
    pub fn with_federated_scope_owner(mut self, owner: String) -> Self {
        self.scopes = Arc::new(RwLock::new(
            ScopeTable::new(self.scope_authority_owners.iter().cloned())
                .with_federated_owner(owner.clone()),
        ));
        self.federated_scope_owner = owner;
        self
    }

    /// Set the startup policy before installing the rescan context or serving requests.
    pub fn with_privileged_operator_prompt(mut self, mode: PrivilegedOperatorPrompt) -> Self {
        self.privileged_operator_prompt = mode;
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
        if let Some(owner) = &self.config_runtime {
            self.supervisor.install_binding_retirement(owner);
        }
        self.supervisor.set_activity(self.activity.clone());
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

    /// Share the supervisor's daemon-shutdown flag with this handler, so
    /// requirement changes after the daemon has begun its announced shutdown
    /// are not reported as faults.
    pub(crate) fn with_daemon_shutdown_flag(mut self, flag: DaemonShutdownFlag) -> Self {
        self.daemon_shutdown = flag;
        self
    }

    pub fn with_connected_clients(mut self, connected_clients: ConnectedClients) -> Self {
        self.connected_clients = connected_clients;
        self
    }

    /// Report why this daemon serves no local endpoint, in `server.describe`, so
    /// an operator can see a bind failure without reading the daemon log.
    pub fn with_local_endpoint_unavailable(mut self, reason: Option<String>) -> Self {
        self.local_endpoint_unavailable = reason;
        self
    }

    pub fn forwarding(&self) -> Arc<ForwardingTable> {
        Arc::clone(&self.forwarding)
    }

    pub(crate) fn counters(&self) -> DaemonCounters {
        self.counters.clone()
    }

    /// Remove a connection's registry entries WITHOUT signalling the supervisor's
    /// registration-release watch. The signal is what the supervisor waits on
    /// before spawning a replacement, so it must only fire once forwarding
    /// teardown is also done (see [`Self::cleanup_connection`] /
    /// [`Self::handle_goodbye`]). Unpublished HELLO rollback uses the separate
    /// registry-only path below because publication already owns the spawn lock.
    fn deregister_connection(
        &self,
        connection_id: ConnectionId,
        reason: RegistrationEndReason,
    ) -> Result<Vec<crate::registry::ModuleRegistration>, RegistryError> {
        if let Some(spawn) = self.supervisor.registration_spawn(connection_id) {
            spawn.end_primary_registration();
        }
        self.registry
            .deregister_connection_with_reason(connection_id, reason)
    }

    fn request_refusal_close(
        &self,
        connection: ConnectionId,
        sink: Option<&crate::FrameSink>,
        reason: CloseReason,
    ) {
        if let Some(sink) = sink {
            sink.terminate_ingress();
        }
        if self.claims.close_notifications_enabled() {
            self.forwarding.request_connection_close(connection, reason);
        }
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

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_test_operator_app_policy(
        mut self,
        policy: crate::operator_apps::TestOperatorAppPolicy,
    ) -> Self {
        self.operator_app_policy = policy.into();
        self
    }

    /// Inject the Windows signature check for tests. The injected check
    /// answers the verified version string or the rejection reason; the
    /// daemon enforces the floor itself on the returned version.
    #[cfg(all(windows, any(test, feature = "test-support")))]
    pub fn with_test_windows_verifier(mut self, verify: crate::app_open::WindowsVerifyFn) -> Self {
        self.windows_verify = Some(verify);
        self
    }

    /// Record which pids the daemon would flash for `already_running`.
    #[cfg(all(windows, any(test, feature = "test-support")))]
    pub fn with_test_windows_flash(mut self, flash: crate::app_open::WindowsFlashFn) -> Self {
        self.windows_flash = Some(flash);
        self
    }

    /// Inject a tracking failure for tests: makes `track_launched_process`
    /// fail so a test can prove a launch that cannot be tracked is stopped,
    /// never left running.
    #[cfg(all(windows, any(test, feature = "test-support")))]
    pub fn with_test_windows_track(mut self, track: crate::app_open::WindowsTrackFn) -> Self {
        self.windows_track = Some(track);
        self
    }

    pub(crate) fn record_local_connection(&self, connection: ConnectionId) {
        self.local_connections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(connection);
    }

    // Keep the connection-level delivery decision shared with route.bind. The
    // target's capability opt-in is separate from recognition of the caller.
    fn connection_operator_app(
        &self,
        stamp: Option<&crate::operator_apps::OperatorAppStamp>,
    ) -> Option<subc_protocol::peer::OperatorApp> {
        stamp
            .filter(|stamp| stamp.delivery_enabled(&self.operator_app_policy))
            .map(|stamp| stamp.app.clone())
    }

    pub(crate) fn record_observed_peer(
        &self,
        connection: ConnectionId,
        peer: subc_os::local_peer::ObservedPeer,
    ) {
        use subc_os::local_peer::{ObservedPeer as OsPeer, PeerUser};
        let peer = match peer {
            OsPeer::Unobserved => ObservedPeer::unobserved(),
            OsPeer::Observed(peer) => {
                let observed = match peer.user {
                    PeerUser::Uid(uid) => ObservedPeer::observed_uid(uid),
                    PeerUser::Sid(sid) => ObservedPeer::observed_sid(sid),
                };
                observed
                    .with_exe(
                        peer.exe
                            .and_then(|path| path.into_os_string().into_string().ok()),
                    )
                    .with_signing_id(peer.signing_id)
            }
        };
        self.observed_peers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(connection, peer);
    }

    pub(crate) fn record_caller_endpoints(
        &self,
        connection: ConnectionId,
        endpoints: Option<(SocketAddr, SocketAddr)>,
    ) {
        let mut callers = self
            .caller_endpoints
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        callers.remove(&connection);
        if let Some(endpoints) = endpoints {
            callers.insert(connection, endpoints);
        }
    }

    #[cfg(test)]
    pub(crate) fn with_caller_lookup_for_test(
        mut self,
        lookup: impl Fn(Option<(SocketAddr, SocketAddr)>) -> CallerIdentity + Send + Sync + 'static,
    ) -> Self {
        self.caller_lookup = Some(Arc::new(lookup));
        self
    }

    async fn resolve_caller(&self, connection: ConnectionId) -> CallerIdentity {
        let endpoints = self
            .caller_endpoints
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&connection)
            .copied();
        #[cfg(test)]
        let lookup = self.caller_lookup.clone();
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(lookup) = lookup {
                return lookup(endpoints);
            }
            endpoints
                .map(|(local, peer)| CallerIdentity::lookup_endpoints(local, peer))
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    }

    pub(crate) fn defers_privileged_control(&self, ctx: &RouteCtx, frame: &Frame) -> bool {
        frame.header.channel == 0
            && frame.header.ty == FrameType::Request
            && self
                .forwarding
                .module_endpoint_for_connection(ctx.connection_id)
                .is_ok_and(|endpoint| endpoint.is_none())
            && parse_client_control_request(&frame.body)
                .ok()
                .is_some_and(|request| privileged_operation(&request).is_some())
    }

    pub(crate) async fn handle_control_frame_timed(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        dispatch_started_at: Option<StdInstant>,
    ) -> Result<Vec<Frame>, RouterError> {
        let first = ctx.egress.first_ingress_frame();
        self.handle_control_frame_ingress(ctx, frame, dispatch_started_at, first)
            .await
    }

    pub(crate) async fn handle_control_frame_ingress(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        dispatch_started_at: Option<StdInstant>,
        first: bool,
    ) -> Result<Vec<Frame>, RouterError> {
        if ctx.egress.ingress_terminal() {
            return Ok(Vec::new());
        }
        #[cfg(test)]
        self.claims.record_dispatch(frame.header.corr);
        let claim_binding = self.claims.binding(ctx.connection_id);
        let mut prefix_attempt = (frame.header.ty == FrameType::Hello
            && claim_binding
                .as_ref()
                .is_some_and(|binding| binding.prefix_module_id().is_some()))
        .then(|| self.prefix_hello_attempt(ctx));
        if claim_binding
            .as_ref()
            .is_some_and(|binding| binding.prefix_module_id().is_some())
        {
            let registered = self
                .forwarding
                .module_endpoint_for_connection(ctx.connection_id)
                .map_err(RouterError::Forwarding)?
                .is_some();
            if !registered && frame.header.ty != FrameType::Hello {
                return self
                    .refuse_claim_connection(ctx, &frame, "claim_hello_required")
                    .await;
            }
            if registered && frame.header.ty == FrameType::Goodbye {
                ctx.egress.terminate_ingress();
                let result = self.handle_goodbye(ctx.connection_id);
                self.claims.connection_closed(ctx.connection_id);
                self.request_refusal_close(
                    ctx.connection_id,
                    Some(&ctx.egress),
                    CloseReason::new("claim_registration_ended", "prefix registration ended"),
                );
                return result;
            }
            if registered && frame.header.ty == FrameType::Request {
                let allowed =
                    serde_json::from_slice::<ControlOpProbe>(&frame.body).is_ok_and(|probe| {
                        matches!(
                            probe.op.as_str(),
                            "catalog.update"
                                | "catalog.update_role_declarations"
                                | "scope.describe"
                                | "scope.watch"
                                | "supervisor.live_roots"
                                | subc_protocol::policy_root::GET_OP
                                | "activity.record"
                        )
                    });
                if !allowed {
                    return Ok(vec![control_error_frame(
                        &frame,
                        channel_operations::CLAIM_OPERATION_NOT_ALLOWED,
                        "prefix registrations have only brokered module operations",
                    )?]);
                }
            }
            if registered
                && frame.header.ty == FrameType::Push
                && !serde_json::from_slice::<ControlOpProbe>(&frame.body)
                    .is_ok_and(|probe| probe.op == "route.status")
            {
                return Ok(vec![control_error_frame(
                    &frame,
                    channel_operations::CLAIM_OPERATION_NOT_ALLOWED,
                    "prefix registrations may push only route.status for routes they serve",
                )?]);
            }
        }
        match frame.header.ty {
            FrameType::Ping => Ok(vec![pong(&frame)?]),
            FrameType::Hello => {
                if claim_binding
                    .as_ref()
                    .is_some_and(|binding| binding.prefix_module_id().is_none())
                {
                    return self
                        .refuse_claim_connection(ctx, &frame, "claim_hello_not_allowed")
                        .await;
                }
                // Resolved once, here: the blocking HELLO below may outlive this
                // reader and run after cleanup has forgotten the channel, and it
                // must still be judged as this channel's HELLO.
                let channel_spawn = self.supervisor.registration_spawn(ctx.connection_id);
                if let Some(spawn) = &channel_spawn {
                    spawn.wait_launch().await;
                    // Refused launches close the channel. Leave their pending
                    // HELLO unanswered instead of racing a reply against shutdown.
                    if !spawn.launch_ready() {
                        ctx.egress.terminate_ingress();
                        return Ok(Vec::new());
                    }
                    #[cfg(test)]
                    self.pause_hello_ingress_for_test().await;
                } else if claim_binding.is_none() {
                    if let Ok(hello) = serde_json::from_slice::<serde_json::Value>(&frame.body) {
                        if let (Some(module), Some(nonce)) = (
                            hello
                                .get("manifest")
                                .and_then(|value| value.get("module_id"))
                                .and_then(|value| value.as_str()),
                            hello.get("launch_nonce").and_then(|value| value.as_str()),
                        ) {
                            if !self
                                .config_runtime
                                .as_ref()
                                .is_some_and(|owner| owner.module_is_gated(module))
                                && !self.supervisor.wait_program_binding(module, nonce).await
                            {
                                self.request_refusal_close(
                                    ctx.connection_id,
                                    Some(&ctx.egress),
                                    CloseReason::new(
                                        "program_refused",
                                        "the spawned program binding failed",
                                    ),
                                );
                                return Ok(vec![control_error_frame(
                                    &frame,
                                    if self
                                        .config_runtime
                                        .as_ref()
                                        .is_some_and(|owner| owner.is_recovering())
                                    {
                                        "config_gated"
                                    } else {
                                        "program_refused"
                                    },
                                    "the spawned program binding failed",
                                )?]);
                            }
                        }
                    }
                }
                let handler = self.clone();
                let connection = ctx.connection_id;
                let egress = ctx.egress.clone();
                let correlation = frame.header.corr;
                let dispatcher = tracing::dispatcher::get_default(|dispatcher| dispatcher.clone());
                let span = tracing::Span::current();
                let hello_binding = claim_binding.clone();
                let replies = tokio::task::spawn_blocking(move || {
                    tracing::dispatcher::with_default(&dispatcher, || {
                        span.in_scope(|| {
                            handler.handle_hello_checked(
                                connection,
                                Some(egress),
                                frame,
                                true,
                                hello_binding,
                                channel_spawn,
                            )
                        })
                    })
                })
                .await
                .map_err(|error| RouterError::backend(0, correlation, error.to_string()))??;
                #[cfg(test)]
                if prefix_attempt.is_some() {
                    self.claims.pause_prefix_commit_for_test(false).await;
                }
                if let Some(attempt) = &mut prefix_attempt {
                    let accepted = !replies
                        .iter()
                        .any(|reply| reply.header.ty == FrameType::Error)
                        && self
                            .registry
                            .get_module_by_connection(ctx.connection_id)
                            .map_err(|err| RouterError::backend(0, correlation, err.to_string()))?
                            .is_some();
                    if accepted {
                        attempt.accepted = true;
                    } else {
                        ctx.egress.terminate_ingress();
                        for reply in replies {
                            ctx.egress.send_flushed(reply).await?;
                        }
                        return Ok(Vec::new());
                    }
                }
                Ok(replies)
            }
            FrameType::Goodbye => self.handle_goodbye(ctx.connection_id),
            FrameType::Cancel => {
                // Channel-0 cancellation withdraws approvals and subscriptions
                // and fences pending agent declarations;
                // successful cancellation has no ordinary reply.
                // A config Apply may also own an OS prompt. Cancel that prompt
                // before config cancellation can report the request handled.
                if self
                    .agent_declarations
                    .cancel(ctx.connection_id, frame.header.corr)
                    || self
                        .forwarding
                        .operator_confirms()
                        .cancel(ctx.connection_id, frame.header.corr)
                    || self.cancel_config_approval(ctx.connection_id, frame.header.corr)
                    || self
                        .supervisor
                        .cancel_spawn_subscription(ctx.connection_id, frame.header.corr)
                    || self.cancel_activity_subscription(ctx, &frame)?
                    || self.cancel_catalog_subscription(ctx.connection_id, frame.header.corr)
                    || self
                        .cancel_config_settings_subscription(ctx.connection_id, frame.header.corr)
                    || self.cancel_scope_subscription(ctx.connection_id, frame.header.corr)
                {
                    Ok(Vec::new())
                } else {
                    Ok(vec![control_error_frame(
                        &frame,
                        "unknown_subscription",
                        "no subscription has this correlation id",
                    )?])
                }
            }
            FrameType::Request => {
                // This additive operation is not part of the existing exhaustive
                // module-control enum. Probe the op before decoding that enum.
                let op = serde_json::from_slice::<ControlOpProbe>(&frame.body).ok();
                if let Some(probe) = &op {
                    self.record_control_usage(ctx.connection_id, &probe.op);
                }
                let claim_present = op.as_ref().is_some_and(|probe| probe.op == "claim.present");
                if let Some(refusal) = self
                    .recovery_request_refusal(
                        ctx,
                        &frame,
                        op.as_ref().map(|probe| probe.op.as_str()),
                    )
                    .await?
                {
                    if claim_present {
                        self.reclaim_presented_expiry(&frame);
                        return self
                            .refuse_claim_connection(ctx, &frame, "config_gated")
                            .await;
                    }
                    return Ok(vec![refusal]);
                }
                if claim_present {
                    return self.handle_claim_present(ctx, &frame, first).await;
                }
                if op.as_ref().is_some_and(|probe| probe.op == "claim.mint") {
                    return self.handle_claim_mint(ctx, &frame).await;
                }
                if let Some(refusal) = self.registration_operation_refusal(
                    ctx,
                    &frame,
                    op.as_ref().map(|probe| probe.op.as_str()),
                )? {
                    return Ok(vec![refusal]);
                }
                if let Some(probe) = op.as_ref().filter(|probe| policy_root::is_op(&probe.op)) {
                    return self.handle_policy_root(ctx, frame, &probe.op).await;
                }
                if op
                    .as_ref()
                    .is_some_and(|probe| probe.op == ops::CATALOG_WATCH)
                {
                    return self.handle_catalog_watch(ctx, frame);
                }
                if op
                    .as_ref()
                    .is_some_and(|probe| probe.op == ops::SCOPE_WATCH)
                {
                    return self.handle_scope_watch(ctx, frame);
                }
                if op
                    .as_ref()
                    .is_some_and(|probe| probe.op == MODULE_TO_SUBC_OP_CATALOG_UPDATE)
                    && subc_protocol::provider_declarations::catalog_update_has_provider_declarations(&frame.body)
                        .unwrap_or(false)
                {
                    return Ok(vec![control_error_frame(
                        &frame,
                        error_codes::CATALOG_UPDATE_FROZEN_DECLARATIONS,
                        "provider_declarations are fixed at HELLO; restart the module to change them",
                    )?]);
                }
                if let Some(probe) = op
                    .as_ref()
                    .filter(|probe| config_gate::is_module_config_op(&probe.op))
                {
                    return self
                        .handle_module_config_request(ctx, frame, &probe.op)
                        .await;
                }
                if op
                    .as_ref()
                    .is_some_and(|probe| config_gate::is_config_op(&probe.op))
                {
                    return self.handle_config_gate_request(ctx, frame).await;
                }
                if op
                    .as_ref()
                    .is_some_and(|probe| probe.op == ops::ACTIVITY_RECORD)
                {
                    return self.handle_activity_record(ctx, frame).await;
                }
                if op.as_ref().is_some_and(|probe| {
                    probe.op == ops::ACTIVITY_QUERY || probe.op == ops::ACTIVITY_SUBSCRIBE
                }) {
                    // A refusal and a CANCEL that won must not both end the stream.
                    let key = (ctx.connection_id, frame.header.corr);
                    let replies = self.handle_activity_read(ctx, frame).await?;
                    return Ok(self.activity_reservations.settle(key, replies));
                }
                if op
                    .as_ref()
                    .is_some_and(|probe| probe.op == "operator.confirm")
                {
                    return self.handle_operator_confirm(ctx, frame).await;
                }
                if op.as_ref().is_some_and(|probe| probe.op == ops::APP_OPEN) {
                    return self
                        .observe_control_wait(
                            ctx,
                            None,
                            ops::APP_OPEN,
                            self.handle_app_open(ctx, frame),
                        )
                        .await;
                }
                if op
                    .as_ref()
                    .is_some_and(|probe| probe.op == ops::CONNECTION_DESCRIBE)
                {
                    return self.handle_connection_describe(ctx, frame).await;
                }
                if op
                    .as_ref()
                    .is_some_and(|probe| probe.op == subc_control::SCOPE_SERVED_BY_OP)
                {
                    return self.handle_scope_served_by(ctx.connection_id, frame);
                }
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
                    if let ModuleControlRequestFromModule::AgentCommands {
                        started,
                        finished,
                        known,
                    } = request
                    {
                        return self
                            .agent_declarations
                            .enqueue(
                                self,
                                ctx,
                                frame,
                                crate::resource_totals::Declaration {
                                    started,
                                    finished,
                                    known,
                                },
                            )
                            .await;
                    }
                    let result =
                        self.handle_module_control_request(ctx.connection_id, frame, request);
                    log_slow_control_dispatch(
                        dispatch_started_at,
                        op,
                        ctx.connection_id,
                        corr,
                        None,
                    );
                    return result;
                }

                if is_known_module_request_op(&frame.body) {
                    let is_agents = serde_json::from_slice::<ControlOpProbe>(&frame.body)
                        .is_ok_and(|probe| probe.op == "supervisor.agent_commands");
                    let code = if is_agents {
                        "not_supervised"
                    } else if op
                        .as_ref()
                        .is_some_and(|probe| probe.op == subc_protocol::scope::ROUTE_AUTHORIZE_OP)
                    {
                        error_codes::ROUTE_AUTHORIZE_NOT_PERMITTED
                    } else {
                        "not_registered"
                    };
                    return Ok(vec![control_error_frame(
                        &frame,
                        code,
                        "module-originated control requests require the module's own registered connection",
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
                let mut route_open_timings = (op == ops::ROUTE_OPEN).then(RouteOpenTimings::new);
                #[cfg(test)]
                if let Some(delay) = self.control_dispatch_delay {
                    tokio::time::sleep(delay).await;
                }
                let result = self
                    .handle_client_control_request(ctx, frame, request, &mut route_open_timings)
                    .await;
                if let Some(timings) = route_open_timings.as_mut() {
                    timings.finish();
                }
                log_slow_control_dispatch(
                    dispatch_started_at,
                    op,
                    ctx.connection_id,
                    corr,
                    route_open_timings.as_ref(),
                );
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

    #[cfg(test)]
    fn pause_cleanup_for_test(&self, stage: u8) {
        let mut slot = self.cleanup_pause.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_some_and(|(at, _)| *at == stage) {
            let (_, (reached, release)) = slot.take().unwrap();
            drop(slot);
            let _ = reached.send(());
            let _ = release.recv_timeout(Duration::from_secs(5));
        }
    }

    /// Run `hook` once, in the next `route.open` that attests a launch nonce,
    /// right after the nonce is resolved.
    #[cfg(all(test, unix, feature = "test-support"))]
    pub(crate) fn after_nonce_attestation_for_test(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .after_nonce_attestation
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Box::new(hook));
    }

    #[cfg(test)]
    pub(super) fn pause_after_nonce_attestation_for_test(&self) {
        let hook = self
            .after_nonce_attestation
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub fn cleanup_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Vec<crate::registry::ModuleRegistration>, RegistryError> {
        // The server loop joins its offloaded request tasks (route opens,
        // privileged requests and activity reads) before this cleanup runs, and
        // inline dispatch has ended. Two kinds of work are not joined. Agent
        // command declarations run on their queue's detached worker, so the
        // queue is closed first: every declaration still queued or validating
        // has its commit fence cancelled before anything below can change how
        // the connection resolves. A HELLO whose reader was dropped keeps
        // running on its blocking thread. It carries the channel spawn its
        // reader resolved at ingress instead of re-reading the index forgotten
        // below, so it is still judged as that channel's HELLO: the retired
        // spawn refuses it before it records anything, and a channel HELLO
        // never records a launch nonce, so it cannot leave this connection any
        // nonce authority.
        self.agent_declarations.close(connection_id);
        self.close_catalog_subscriptions(connection_id);
        self.close_config_settings_subscriptions(connection_id);
        self.close_scope_subscriptions(connection_id);
        #[cfg(test)]
        self.pause_cleanup_for_test(0);
        if let Some(spawn) = self.supervisor.registration_spawn(connection_id) {
            spawn.retire_registration_channel();
        }
        self.supervisor
            .forget_registration_connection(connection_id);
        #[cfg(test)]
        self.pause_cleanup_for_test(1);
        self.local_connections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&connection_id);
        self.caller_endpoints
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&connection_id);
        self.build_class_warned
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&connection_id);
        // Retire the capture before its published facts so an in-flight
        // verification cannot recreate them after cleanup.
        self.captured_peers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&connection_id);
        self.observed_peers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&connection_id);
        self.config_desktop_stamps
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&connection_id);
        self.operator_apps
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&connection_id);
        self.forwarding
            .operator_confirms()
            .connection_closed(connection_id);
        let cleanup = self.cleanup_connection_with_end_reason(
            connection_id,
            RegistrationEndReason::ConnectionClosed,
        );
        // A draining writer can still be reached by a route until forwarding
        // cleanup finishes. Keep its claim sink visible to launch retirement.
        self.claims.connection_closed(connection_id);
        cleanup
    }

    fn cleanup_connection_with_end_reason(
        &self,
        connection_id: ConnectionId,
        requested_reason: RegistrationEndReason,
    ) -> Result<Vec<crate::registry::ModuleRegistration>, RegistryError> {
        #[cfg(test)]
        if let Some(pause) = self
            .activity_request_pause
            .lock()
            .unwrap()
            .as_ref()
            .filter(|pause| pause.connection == connection_id)
        {
            pause
                .cleanup_started
                .store(true, std::sync::atomic::Ordering::Release);
        }
        if let Some(runtime) = &self.config_runtime {
            runtime.disconnected(connection_id);
        }
        self.agent_declarations.close(connection_id);
        let end_reason = if requested_reason == RegistrationEndReason::ConnectionClosed {
            self.registry
                .get_module_by_connection(connection_id)?
                .and_then(|registration| self.supervisor.get(&registration.manifest.module_id))
                .and_then(|module| module.registration_end_reason().ok().flatten())
                .unwrap_or(requested_reason)
        } else {
            requested_reason
        };
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
        let crash_closed = crash_closed.map(|(module_id, routes)| {
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
            // The forwarding table gates all providers at the start of daemon
            // shutdown, before their connections are closed. An ordinary
            // module disconnect still reports crash if that gate is not set.
            let (reason, drained) = match self
                .forwarding
                .daemon_drain_result_for_connection(connection_id)
            {
                Ok(Some(drained)) => (RouteCloseReason::Restart, drained),
                Ok(None) => (RouteCloseReason::Crash, false),
                Err(err) => {
                    warn!(error = %err, "failed to read daemon drain state; reporting crash conservatively");
                    (RouteCloseReason::Crash, false)
                }
            };
            (module_id, routes, reason, drained, terminal)
        });
        let registrations = self.deregister_connection(connection_id, end_reason);
        // Read before teardown: the spawn whose registration this connection
        // was. Its registration ending is an authority change for it.
        let ended_authority = self
            .forwarding
            .target_authority_for_connection(connection_id)
            .ok()
            .flatten();
        let cleanup = if crash_closed.is_some() {
            self.forwarding.cleanup_connection_counted(connection_id)
        } else {
            // A module connection's teardown needs the count of abandoned
            // route.bind relays for its route.closed notice below. Any other
            // connection, such as a client's, sends no such notice and needs
            // only its routes released, so it uses the route-only wrapper and
            // reports zero.
            self.forwarding
                .cleanup_connection(connection_id)
                .map(|released| crate::forwarding::ConnectionCleanup {
                    released,
                    abandoned_relays: 0,
                })
        };
        // The route.closed push waits for forwarding teardown because only
        // teardown knows how many pending route.bind relays it aborted. It still
        // goes out before the GOODBYEs for the released routes, and its targets
        // were captured above, before teardown removed those routes.
        if let Some((module_id, routes, reason, drained, terminal)) = crash_closed {
            let abandoned = cleanup
                .as_ref()
                .map_or(0, |cleanup| cleanup.abandoned_relays);
            send_route_control_pushes(
                &self.forwarding,
                routes,
                ClientControlPush::RouteClosed {
                    module_id,
                    channels: Vec::new(),
                    reason,
                    drained,
                    abandoned,
                    excluded_subscriptions: 0,
                    terminal: Some(terminal),
                },
            );
        }
        if let Ok(cleanup) = cleanup {
            self.emit_route_goodbyes(cleanup.released);
        }
        if let Some(record) = ended_authority {
            self.change_spawn_authority(&record);
        }
        // Signal the registration-release watch only now that BOTH registry and
        // forwarding teardown are done, so a supervisor waiting to spawn a
        // replacement never observes release while old routes still exist.
        if matches!(&registrations, Ok(r) if !r.is_empty()) {
            crate::supervise::notify_registration_release();
            self.capability_evaluator.wake_deadline_loop();
            self.refresh_capability_requirements();
        }
        self.supervisor.remove_spawn_subscribers(connection_id);
        self.activity_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(id, _), task| {
                if *id == connection_id {
                    task.abort();
                    false
                } else {
                    true
                }
            });
        // Sync authority dies with its connection, so the owner's next
        // connection can take it; the owner's scopes stay as they are.
        self.hello_launch_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .forget(connection_id);
        self.scopes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .release_connection(connection_id);
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
        scope_expiry::ScopeExpiryContext::new(self).emit_route_goodbyes(released_routes);
    }

    async fn handle_client_control_request(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        request: ClientControlRequest,
        route_open_timings: &mut Option<RouteOpenTimings>,
    ) -> Result<Vec<Frame>, RouterError> {
        if let Some(owner) = &self.config_runtime {
            let module = match &request {
                ClientControlRequest::SupervisorRestart { module_id, .. }
                | ClientControlRequest::SupervisorReload { module_id }
                | ClientControlRequest::SupervisorSwap { module_id, .. }
                | ClientControlRequest::SupervisorSetEnabled {
                    module_id,
                    enabled: true,
                } => Some(module_id.as_str()),
                _ => None,
            };
            let rescan = matches!(
                &request,
                ClientControlRequest::SupervisorRescan { preview: false }
                    | ClientControlRequest::SupervisorRescanChecked { preview: false, .. }
            );
            let held = if module.is_some() || rescan {
                let module = module.map(str::to_owned);
                owner
                    .on_blocking(move |owner| owner.checked_admission_hold(module.as_deref()))
                    .await
            } else {
                Ok(())
            };
            if let Err(error) = held {
                if error.code != "config_delivery_deferred" {
                    let code = if error.code == "store_tampered" {
                        "store_tampered"
                    } else {
                        "config_gated"
                    };
                    return Ok(vec![control_error_frame(&frame, code, error.detail)?]);
                }
            }
        }
        if let Some(operation) = privileged_operation(&request) {
            let caller = self.resolve_caller(ctx.connection_id).await;
            let op = client_control_request_op(&request);
            let text = prompt_text(operation, &caller);
            match self.privileged_operator_prompt {
                PrivilegedOperatorPrompt::Log => {
                    info!(target: "subc_daemon::privileged_prompt", operation = op,
                        target = ?operation, caller = %caller.description(), summary = %text,
                        "privileged request would prompt the operator");
                }
                PrivilegedOperatorPrompt::Enforce => {
                    let key =
                        crate::operator_confirm::DeclineKey::for_caller(&caller, ctx.connection_id);
                    let shortcut =
                        self.operator_shortcut_stamp(ctx.connection_id)
                            .await
                            .map(|stamp| {
                                self.shortcut_context(Some(stamp), None, None, ctx.connection_id)
                            });
                    let outcome = self
                        .forwarding
                        .operator_confirms()
                        .approve_privileged_with_shortcut(
                            ctx,
                            &frame,
                            op,
                            text,
                            caller.description(),
                            key,
                            shortcut,
                        )
                        .await;
                    if outcome.code != "confirmed" {
                        return Ok(vec![outcome.frame(&frame)]);
                    }
                }
            }
        }
        match request {
            ClientControlRequest::DaemonShutdownIntent { intent } => {
                if intent.as_str() != Some("restart") {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "invalid_params",
                        "intent must be 'restart'",
                    )?]);
                }
                // A stamped daemon boots a newer approved generation only from a
                // durable restart intent, written here before the daemon exits.
                // Failing to write it never blocks the restart: the next start
                // then boots the last active generation and the newer one waits.
                if let Some(owner) = &self.config_runtime {
                    match owner
                        .on_blocking(|owner| owner.assign_daemon_restart())
                        .await
                    {
                        Ok(Some(generation)) => {
                            info!(
                                generation,
                                "daemon restart will warm up approved generation"
                            )
                        }
                        Ok(None) => {}
                        Err(error) => warn!(
                            %error,
                            "daemon restart intent not recorded; the next start boots the last active generation"
                        ),
                    }
                }
                let response = ClientControlResponse::DaemonShutdownIntent {
                    intent: "restart".to_owned(),
                    expires_at_ms: self.supervisor.announce_daemon_restart(),
                };
                Ok(vec![control_response_body_frame(
                    &frame,
                    &response,
                    "ClientControlResponse::DaemonShutdownIntent",
                )?])
            }
            ClientControlRequest::ServerDescribe {} => self.handle_server_describe(frame),
            ClientControlRequest::PolicyRoot(_)
            | ClientControlRequest::PolicyRootSet(_)
            | ClientControlRequest::PolicyRootRecover(_) => {
                unreachable!("policy root ops dispatch before the client-only path")
            }
            ClientControlRequest::CatalogList { module_id } => {
                self.handle_catalog_list(frame, module_id)
            }
            ClientControlRequest::RouteOpen {
                target,
                identity,
                consumer_identity,
                consumer_capabilities,
                role_versions,
                busy_refusals,
                admission_facts,
                scope,
                ..
            } => {
                let extensions = match serde_json::from_slice::<subc_control::RouteOpenRequestExt>(
                    &frame.body,
                ) {
                    Ok(extensions) => extensions,
                    Err(err) => {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "invalid_control_body",
                            format!("malformed route.open extensions: {err}"),
                        )?]);
                    }
                };
                self.handle_route_open(
                    ctx,
                    frame,
                    RouteOpenRequest {
                        target,
                        identity,
                        consumer_identity,
                        consumer_capabilities,
                        role_versions,
                        busy_refusals,
                        admission_facts,
                        scope,
                        accepts_close_reasons: extensions.accepts_close_reasons,
                    },
                    route_open_timings.get_or_insert_with(RouteOpenTimings::new),
                )
                .await
            }
            ClientControlRequest::RoutePoll {
                route_channel,
                route_epoch,
                kind,
            } => self.handle_route_poll(ctx, frame, route_channel, route_epoch, kind),
            ClientControlRequest::SupervisorList {} => {
                let Ok(permit) = Arc::clone(&self.list_admission).try_acquire_owned() else {
                    self.agent_declarations.refuse(
                        self,
                        ctx,
                        control_error_frame(
                            &frame,
                            "busy",
                            "supervisor list work is at capacity; retry",
                        )?,
                    );
                    return Ok(Vec::new());
                };
                // Filesystem-backed diagnostics must not hold up this connection's reader.
                let handler = self.clone();
                let sink = ctx.egress.clone();
                tokio::spawn(
                    async move {
                        let _permit = permit;
                        let result = handler.handle_supervisor_list(frame.clone()).await;
                        let replies = match result {
                            Ok(replies) => replies,
                            Err(error) => {
                                control_error_frame(&frame, "backend_error", error.to_string())
                                    .into_iter()
                                    .collect()
                            }
                        };
                        for reply in replies {
                            if !matches!(
                                tokio::time::timeout(Duration::from_millis(200), sink.send(reply))
                                    .await,
                                Ok(Ok(()))
                            ) {
                                break;
                            }
                        }
                    }
                    .with_current_subscriber(),
                );
                Ok(Vec::new())
            }
            ClientControlRequest::SupervisorSpawnSnapshot {} => {
                self.handle_supervisor_spawn_snapshot(frame)
            }
            ClientControlRequest::SupervisorSpawnSubscribe { since } => {
                self.handle_supervisor_spawn_subscribe(ctx, frame, since)
            }
            ClientControlRequest::SupervisorRestart {
                module_id,
                drain_timeout_ms,
            } => {
                self.log_supervisor_request_received(
                    ctx,
                    frame.header.corr,
                    ops::SUPERVISOR_RESTART,
                    Some(&module_id),
                    None,
                )?;
                let requester = self.supervisor_requester(ctx, frame.header.corr)?;
                self.handle_supervisor_restart(frame, module_id, drain_timeout_ms, requester)
                    .await
            }
            ClientControlRequest::SupervisorSwap {
                module_id,
                ready_timeout_ms,
            } => {
                self.log_supervisor_request_received(
                    ctx,
                    frame.header.corr,
                    ops::SUPERVISOR_SWAP,
                    Some(&module_id),
                    None,
                )?;
                let requester = self.supervisor_requester(ctx, frame.header.corr)?;
                self.handle_supervisor_swap(frame, module_id, ready_timeout_ms, requester)
                    .await
            }
            ClientControlRequest::SupervisorReload { module_id } => {
                self.log_supervisor_request_received(
                    ctx,
                    frame.header.corr,
                    ops::SUPERVISOR_RELOAD,
                    Some(&module_id),
                    None,
                )?;
                let requester = self.supervisor_requester(ctx, frame.header.corr)?;
                self.handle_supervisor_reload(frame, module_id, requester)
                    .await
            }
            ClientControlRequest::SupervisorRescan { preview } => {
                if !preview {
                    self.log_supervisor_request_received(
                        ctx,
                        frame.header.corr,
                        ops::SUPERVISOR_RESCAN,
                        None,
                        None,
                    )?;
                }
                let requester = self.supervisor_requester(ctx, frame.header.corr)?;
                let operation = self.handle_supervisor_rescan(
                    frame,
                    if preview {
                        RescanMode::Preview
                    } else {
                        RescanMode::Execute
                    },
                    requester,
                );
                self.observe_control_wait(ctx, None, ops::SUPERVISOR_RESCAN, operation)
                    .await
            }
            ClientControlRequest::SupervisorRescanChecked {
                preview,
                candidate,
                expect,
            } => {
                self.observe_control_wait(
                    ctx,
                    None,
                    ops::SUPERVISOR_RESCAN,
                    self.handle_supervisor_rescan_checked(ctx, frame, preview, candidate, expect),
                )
                .await
            }
            ClientControlRequest::SupervisorReleaseReserved { module_id } => {
                self.log_supervisor_request_received(
                    ctx,
                    frame.header.corr,
                    ops::SUPERVISOR_RELEASE_RESERVED,
                    Some(&module_id),
                    None,
                )?;
                self.handle_supervisor_release_reserved(frame, module_id)
                    .await
            }
            ClientControlRequest::SupervisorSetEnabled { module_id, enabled } => {
                self.log_supervisor_request_received(
                    ctx,
                    frame.header.corr,
                    ops::SUPERVISOR_SET_ENABLED,
                    Some(&module_id),
                    Some(enabled),
                )?;
                let requester = self.supervisor_requester(ctx, frame.header.corr)?;
                self.handle_supervisor_set_enabled(frame, module_id, enabled, requester)
                    .await
            }
            ClientControlRequest::SupervisorHealthProbe { module_id } => {
                self.handle_supervisor_health_probe(ctx, frame, module_id)
                    .await
            }
            ClientControlRequest::SupervisorHealth {} => self.handle_supervisor_health(frame),
            ClientControlRequest::SupervisorRoutes { module_id } => {
                self.handle_supervisor_routes(frame, module_id)
            }
            ClientControlRequest::SupervisorProvenance { module_id } => {
                self.handle_supervisor_provenance(ctx, frame, module_id)
                    .await
            }
            ClientControlRequest::SupervisorStderrTail {
                module_id,
                max_lines,
                max_bytes,
            } => self.handle_supervisor_stderr_tail(frame, module_id, max_lines, max_bytes),
            ClientControlRequest::SupervisorTerminals { module_id } => {
                self.handle_supervisor_terminals(ctx, frame, module_id)
                    .await
            }
        }
    }

    /// Explicit test-only phone policy; daemon configuration cannot select it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_test_phone_shortcut(mut self, enabled: bool) -> Self {
        self.operator_app_policy.phone_enabled = enabled;
        self
    }

    fn handle_goodbye(&self, connection_id: ConnectionId) -> Result<Vec<Frame>, RouterError> {
        debug!(connection_id = connection_id.get(), "handling GOODBYE");
        // GOODBYE ends the connection's logical session even when its socket
        // stays open. Use disconnect teardown so verdicts, client notices and
        // scope authority are released at the same lifecycle boundary.
        self.cleanup_connection_with_end_reason(
            connection_id,
            RegistrationEndReason::ExplicitGoodbye,
        )
        .map_err(|err| RouterError::backend(0, 0, err.to_string()))?;
        Ok(Vec::new())
    }
}

impl Default for ControlHandler {
    fn default() -> Self {
        Self::new(Arc::new(Registry::default()))
    }
}

impl crate::supervise::SwapPromotionObserver for ControlHandler {
    fn swap_promoted(&self, registration: &crate::registry::ModuleRegistration) {
        self.apply_registration_capabilities(registration);
    }
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
    route_open_timings: Option<&RouteOpenTimings>,
) {
    let Some(dispatch_started_at) = dispatch_started_at else {
        return;
    };
    // Use the same monotonic clock as the phase timers and relay deadline.
    let elapsed = Instant::from_std(dispatch_started_at).elapsed();
    if elapsed >= SLOW_CONTROL_DISPATCH_THRESHOLD {
        if let Some(timings) = route_open_timings {
            warn!(
                op = %op,
                connection_id = connection_id.get(),
                corr,
                elapsed_ms = elapsed.as_millis() as u64,
                module_id = ?timings.module_id,
                principal = %timings.principal,
                admission_ms = timings.admission.as_millis() as u64,
                project_root_ms = timings.project_root.as_millis() as u64,
                egress_reservation_ms = timings.egress_reservation.as_millis() as u64,
                bind_send_ms = timings.bind_send.as_millis() as u64,
                bind_ack_wait_ms = timings.bind_ack_wait.as_millis() as u64,
                "slow control dispatch"
            );
            return;
        }
        warn!(
            op = %op,
            connection_id = connection_id.get(),
            corr,
            elapsed_ms = elapsed.as_millis() as u64,
            "slow control dispatch"
        );
    }
}

/// A test stand-in for the OS caller lookup, given the recorded endpoints.
#[cfg(test)]
type TestCallerLookup =
    Arc<dyn Fn(Option<(SocketAddr, SocketAddr)>) -> CallerIdentity + Send + Sync>;

/// Every new client operation needs an explicit privilege decision here.
fn privileged_operation(request: &impl PrivilegedRequest) -> Option<PrivilegedOperation<'_>> {
    request.privileged_operation()
}

trait PrivilegedRequest {
    fn privileged_operation(&self) -> Option<PrivilegedOperation<'_>>;
}

impl PrivilegedRequest for ClientControlRequest {
    fn privileged_operation(&self) -> Option<PrivilegedOperation<'_>> {
        let request = self;
        match request {
        ClientControlRequest::SupervisorRestart { module_id, .. } => {
            Some(PrivilegedOperation::Restart { module_id })
        }
        ClientControlRequest::SupervisorSwap { module_id, .. } => {
            Some(PrivilegedOperation::Swap { module_id })
        }
        ClientControlRequest::SupervisorReload { module_id } => {
            Some(PrivilegedOperation::Reload { module_id })
        }
        ClientControlRequest::SupervisorRescan { preview } => {
            if *preview {
                None
            } else {
                Some(PrivilegedOperation::Rescan)
            }
        }
        // Only the well-formed apply asks for approval. A preview changes
        // nothing, and any other combination is refused before it touches
        // anything, so prompting the operator for it would ask about a request
        // that cannot run.
        ClientControlRequest::SupervisorRescanChecked {
            preview,
            candidate,
            expect,
        } => (!*preview && candidate.is_none() && expect.is_some())
            .then_some(PrivilegedOperation::Rescan),
        ClientControlRequest::SupervisorSetEnabled { module_id, enabled } => {
            Some(PrivilegedOperation::SetEnabled {
                module_id,
                enabled: *enabled,
            })
        }
        ClientControlRequest::SupervisorReleaseReserved { module_id } => {
            Some(PrivilegedOperation::ReleaseReserved { module_id })
        }
        ClientControlRequest::ServerDescribe {}
        | ClientControlRequest::PolicyRoot(_)
        | ClientControlRequest::PolicyRootSet(_)
        | ClientControlRequest::PolicyRootRecover(_)
        // The announcement uses the same client admission as supervisor.restart,
        // but cannot stop anything itself and needs no operator confirmation.
        | ClientControlRequest::DaemonShutdownIntent { .. }
        | ClientControlRequest::CatalogList { .. }
        | ClientControlRequest::RouteOpen { .. }
        | ClientControlRequest::RoutePoll { .. }
        | ClientControlRequest::SupervisorList {}
        | ClientControlRequest::SupervisorHealth {}
        | ClientControlRequest::SupervisorHealthProbe { .. }
        | ClientControlRequest::SupervisorRoutes { .. }
        | ClientControlRequest::SupervisorProvenance { .. }
        | ClientControlRequest::SupervisorStderrTail { .. }
        | ClientControlRequest::SupervisorTerminals { .. }
        | ClientControlRequest::SupervisorSpawnSnapshot {}
        | ClientControlRequest::SupervisorSpawnSubscribe { .. } => None,
    }
    }
}

fn client_control_request_op(request: &ClientControlRequest) -> &'static str {
    match request {
        ClientControlRequest::PolicyRoot(_) => subc_protocol::policy_root::GET_OP,
        ClientControlRequest::PolicyRootSet(_) => subc_control::policy_root::SET_OP,
        ClientControlRequest::PolicyRootRecover(_) => subc_control::policy_root::RECOVER_OP,
        ClientControlRequest::DaemonShutdownIntent { .. } => ops::DAEMON_SHUTDOWN_INTENT,
        ClientControlRequest::ServerDescribe {} => ops::SERVER_DESCRIBE,
        ClientControlRequest::SupervisorProvenance { .. } => ops::SUPERVISOR_PROVENANCE,
        ClientControlRequest::CatalogList { .. } => ops::CATALOG_LIST,
        ClientControlRequest::RouteOpen { .. } => ops::ROUTE_OPEN,
        ClientControlRequest::RoutePoll { .. } => ops::ROUTE_POLL,
        ClientControlRequest::SupervisorList {} => ops::SUPERVISOR_LIST,
        ClientControlRequest::SupervisorSpawnSnapshot {} => ops::SUPERVISOR_SPAWN_SNAPSHOT,
        ClientControlRequest::SupervisorSpawnSubscribe { .. } => ops::SUPERVISOR_SPAWN_SUBSCRIBE,
        ClientControlRequest::SupervisorRestart { .. } => ops::SUPERVISOR_RESTART,
        ClientControlRequest::SupervisorSwap { .. } => ops::SUPERVISOR_SWAP,
        ClientControlRequest::SupervisorReload { .. } => ops::SUPERVISOR_RELOAD,
        ClientControlRequest::SupervisorRescan { .. } => ops::SUPERVISOR_RESCAN,
        ClientControlRequest::SupervisorRescanChecked { .. } => ops::SUPERVISOR_RESCAN_CHECKED,
        ClientControlRequest::SupervisorReleaseReserved { .. } => ops::SUPERVISOR_RELEASE_RESERVED,
        ClientControlRequest::SupervisorSetEnabled { .. } => ops::SUPERVISOR_SET_ENABLED,
        ClientControlRequest::SupervisorHealthProbe { .. } => ops::SUPERVISOR_HEALTH_PROBE,
        ClientControlRequest::SupervisorHealth {} => ops::SUPERVISOR_HEALTH,
        ClientControlRequest::SupervisorRoutes { .. } => ops::SUPERVISOR_ROUTES,
        ClientControlRequest::SupervisorStderrTail { .. } => ops::SUPERVISOR_STDERR_TAIL,
        ClientControlRequest::SupervisorTerminals { .. } => ops::SUPERVISOR_TERMINALS,
    }
}

fn parse_client_control_request(
    body: &[u8],
) -> Result<ClientControlRequest, (serde_json::Error, ControlRequestBodyError)> {
    serde_json::from_slice::<ClientControlRequest>(body).map_err(|err| {
        let classification = match serde_json::from_slice::<ControlOpProbe>(body) {
            Ok(probe)
                if SUBC_CONTROL_OPS.contains(&probe.op.as_str())
                    || SUBC_UNADVERTISED_CONTROL_OPS.contains(&probe.op.as_str()) =>
            {
                ControlRequestBodyError::InvalidBody
            }
            Ok(_) => ControlRequestBodyError::UnknownOp,
            Err(_) => ControlRequestBodyError::InvalidBody,
        };
        (err, classification)
    })
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

pub(crate) fn control_error_frame(
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

/// A channel-0 ERROR for a refusal the daemon decided itself: every
/// route.open refusal, the drain `module_reloading` and scope-change/end
/// bodies, spawn-subscription and supervisor refusals. Carries the
/// daemon-origin bit, which is what lets a client treat `module_reloading`
/// as proof that its request never reached a module. Response, push,
/// goodbye and relay-request frames keep the plain control flags in their
/// own builders.
pub(crate) fn control_error_body_frame(
    frame: &Frame,
    error: ErrorBody,
) -> Result<Frame, RouterError> {
    control_error_frame_with_flags(frame, error, daemon_error_flags())
}

/// A channel-0 ERROR relaying a module's own rejection body unchanged: a
/// refused route.bind answered as a route.open refusal, or a refused
/// module-control RPC. Plain control flags — the code and message are
/// module-chosen, so the frame must not claim daemon authorship, even when
/// the module picks a daemon code like `module_reloading`.
fn control_relayed_error_body_frame(frame: &Frame, error: ErrorBody) -> Result<Frame, RouterError> {
    control_error_frame_with_flags(frame, error, control_flags())
}

fn control_error_frame_with_flags(
    frame: &Frame,
    error: ErrorBody,
    flags: Flags,
) -> Result<Frame, RouterError> {
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
        flags,
        0,
        0,
        frame.header.corr,
        body,
    )
    .map_err(RouterError::FrameBuild)
}

pub(crate) fn control_response_body_frame<T: Serialize>(
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

fn forwarding_error_code(err: &ForwardingError) -> &'static str {
    match err {
        ForwardingError::ConnectionRoleConflict { .. } => "invalid_request",
        ForwardingError::NoModuleConnection => "target_unavailable",
        ForwardingError::ModuleReloading { .. } => "module_reloading",
        ForwardingError::ClientRouteChannelExhausted { .. }
        | ForwardingError::ModuleRouteChannelExhausted { .. } => "route_limit",
        ForwardingError::StaleModuleEndpoint
        | ForwardingError::UnknownReservation { .. }
        | ForwardingError::ConnectionClosing { .. }
        | ForwardingError::ClientEgressClosed { .. }
        | ForwardingError::ModuleEgressUnavailable { .. } => "target_unavailable",
        // Only a swap candidate's registration can produce this, and it means
        // exactly what a second active HELLO for a live id means.
        ForwardingError::CandidateSlotOccupied { .. } => "duplicate_module_id",
        ForwardingError::TargetRegistrationChanged(_) => "capability_forbidden",
        ForwardingError::RelayDeadlineElapsed => "module_timeout",
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

pub(crate) fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

/// GOODBYE for a route.bind the daemon gave up on after reserving the module's
/// channel. The target is the module (a client never saw the route), so this
/// takes the module path: delivered late rather than dropped when the module's
/// queue is momentarily full, and never closing its connection.
fn send_goodbye_target_best_effort(
    counters: &DaemonCounters,
    target: &GoodbyeTarget,
    context: &'static str,
) {
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
    crate::forwarding::send_module_route_goodbye(
        counters,
        &target.sink,
        frame,
        target.module_id.as_deref(),
        context,
    );
}

fn route_close_reason_for_opener(
    reason: RouteCloseReason,
    accepts_close_reasons: Option<&[String]>,
) -> RouteCloseReason {
    if reason == RouteCloseReason::AuthorityChanged
        && !accepts_close_reasons
            .is_some_and(|reasons| reasons.iter().any(|reason| reason == "authority_changed"))
    {
        // Older readers forbid reopening for unknown reasons. Reload already
        // permits reopening and, like an authority change, requires a new bind.
        RouteCloseReason::Reload
    } else {
        reason
    }
}

pub(crate) fn send_route_control_pushes(
    forwarding: &ForwardingTable,
    routes: Vec<EndpointRoute>,
    push: ClientControlPush,
) {
    let reason = match &push {
        ClientControlPush::RouteClosing { reason, .. }
        | ClientControlPush::RouteClosed { reason, .. } => *reason,
    };
    let mut targets: Vec<(GoodbyeTarget, RouteCloseReason, Vec<u16>)> = Vec::new();
    for route in routes {
        let recipient_reason =
            route_close_reason_for_opener(reason, route.accepts_close_reasons.as_deref());
        let target = route.goodbye_target;
        if let Some((existing, _, channels)) = targets.iter_mut().find(|(existing, reason, _)| {
            existing.connection_id == target.connection_id && *reason == recipient_reason
        }) {
            debug_assert_eq!(
                existing.negotiated_ver, target.negotiated_ver,
                "one connection cannot negotiate multiple frame versions"
            );
            if !channels.contains(&target.channel) {
                channels.push(target.channel);
            }
            continue;
        }
        let channel = target.channel;
        targets.push((target, recipient_reason, vec![channel]));
    }
    for (target, recipient_reason, mut channels) in targets {
        channels.sort_unstable();
        let mut push = push.clone();
        match &mut push {
            ClientControlPush::RouteClosing {
                channels: covered,
                reason,
                ..
            }
            | ClientControlPush::RouteClosed {
                channels: covered,
                reason,
                ..
            } => {
                *covered = channels;
                *reason = recipient_reason;
            }
        }
        let body = match serde_json::to_vec(&push) {
            Ok(body) => body,
            Err(err) => {
                warn!(error = %err, "failed to serialize route lifecycle control PUSH");
                continue;
            }
        };
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
                    crate::forwarding::UndeliveredFrame {
                        module_id: target.module_id.as_deref(),
                        sink: &target.sink,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "control/scope_outcome_count_tests.rs"]
mod scope_outcome_count_tests;

#[cfg(test)]
#[path = "control/control_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "control/concurrency_default_exposure_tests.rs"]
mod concurrency_default_exposure_tests;
