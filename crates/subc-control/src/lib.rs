//! Client-facing subc channel-0 control wire shapes.
//!
//! This crate is the client ↔ subc control-plane boundary. It depends only on
//! [`subc-protocol`] for shared primitives such as `RouteTarget` and
//! `BindIdentity`; clients can use it without depending on the
//! daemon implementation.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use subc_protocol::{
    manifest::{CapabilityDeclarations, ManifestProvenance, ProviderRole},
    session::HealthStatus,
    BindIdentity, RouteTarget,
};

/// Daemon-spawned consumer identity presented on route.open.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ConsumerIdentity {
    pub module_id: String,
    pub launch_nonce: String,
}

/// Reserved dotted operation prefixes for the v0.4 control vocabulary.
///
/// `scheduler.` and `watch.` were reserved here from v0.4 until 2026-08-10 and
/// were removed deliberately rather than left as placeholders: neither was ever
/// implemented, and both capabilities are now owned elsewhere by ruling --
/// scheduled tasks belong to the session runtime (prefrontal) because the
/// daemon is state-free routing, and external-event watching belongs to the
/// connectors module (plexus). A reserved name for something that will never be
/// built here reads as a roadmap commitment to anyone surveying the protocol,
/// and it recruited exactly that misunderstanding from an outside contributor.
pub mod ops {
    pub const SERVER: &str = "server.";
    pub const CATALOG: &str = "catalog.";
    pub const ROUTE: &str = "route.";
    pub const SUPERVISOR: &str = "supervisor.";
    pub const CONFIG: &str = "config.";

    pub const SERVER_DESCRIBE: &str = "server.describe";
    pub const CATALOG_LIST: &str = "catalog.list";
    pub const ROUTE_OPEN: &str = "route.open";
    pub const ROUTE_POLL: &str = "route.poll";
    pub const ROUTE_CLOSING: &str = "route.closing";
    pub const ROUTE_CLOSED: &str = "route.closed";
    pub const SUPERVISOR_LIST: &str = "supervisor.list";
    pub const SUPERVISOR_RESTART: &str = "supervisor.restart";
    pub const SUPERVISOR_RELOAD: &str = "supervisor.reload";
    pub const SUPERVISOR_RESCAN: &str = "supervisor.rescan";
    pub const SUPERVISOR_RELEASE_RESERVED: &str = "supervisor.release_reserved";
    pub const SUPERVISOR_SET_ENABLED: &str = "supervisor.set_enabled";
    pub const SUPERVISOR_HEALTH_PROBE: &str = "supervisor.health_probe";
    pub const SUPERVISOR_HEALTH: &str = "supervisor.health";
    pub const SUPERVISOR_STDERR_TAIL: &str = "supervisor.stderr_tail";
    pub const SUPERVISOR_TERMINALS: &str = "supervisor.terminals";
    pub const SUPERVISOR_ROUTES: &str = "supervisor.routes";
    pub const SUPERVISOR_PROVENANCE: &str = "supervisor.provenance";
}

/// Client-originated channel-0 control RPC body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
// RouteOpen carries the complete route metadata, while several control operations
// are markers; retain the direct public wire shape instead of boxing its fields.
#[allow(clippy::large_enum_variant)]
pub enum ClientControlRequest {
    #[serde(rename = "server.describe")]
    ServerDescribe {},
    #[serde(rename = "catalog.list")]
    CatalogList {
        /// Absent lists every registered module; present narrows to one. A
        /// narrowed list for an unregistered id is an empty list rather than an
        /// error, so absent and unregistered are distinguishable only by which
        /// question you asked.
        #[serde(default)]
        module_id: Option<String>,
    },
    #[serde(rename = "route.open")]
    RouteOpen {
        target: RouteTarget,
        identity: BindIdentity,
        /// The consumer's claim to a supervised launch, which the daemon verifies
        /// against its live spawn nonces before stamping a principal.
        ///
        /// Absent is a legitimate shape, not an omission: a direct key-holder has
        /// no launch nonce to present, and the daemon stamps `Direct`. So absence
        /// means NO CLAIM WAS MADE, never that a claim was refused — a refused
        /// claim is an error frame and the route never opens. A provider deciding
        /// what to trust reads the stamped principal on the bind, not this.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consumer_identity: Option<ConsumerIdentity>,
        /// Consumer-declared reverse-request capabilities for the route. This is
        /// an unverified declaration, not a privilege grant; if a consumer
        /// over-declares, providers may still send reverse requests that later
        /// time out or deny. Providers must treat an absent field as no
        /// reverse-request capability. The vocabulary is open strings; known MCP
        /// method-family values today are "elicitation", "sampling", and
        /// "roots".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consumer_capabilities: Option<Vec<String>>,
        /// Opaque admission facts supplied by the configured carrier module.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        admission_facts: Option<serde_json::Value>,
    },
    #[serde(rename = "route.poll")]
    RoutePoll {
        route_channel: u16,
        route_epoch: u32,
        kind: PollKind,
    },
    #[serde(rename = "supervisor.list")]
    SupervisorList {},
    #[serde(rename = "supervisor.restart")]
    SupervisorRestart {
        module_id: String,
        /// Optional per-restart override of the module's drain budget, in ms.
        /// Absent: the module's configured `drain_timeout_ms` (or the daemon
        /// default) applies. `0` tears down without waiting — the wedge-bounce
        /// escape, where a stuck in-flight request would never settle anyway.
        /// Additive; older daemons that predate this field reject unknown
        /// fields on channel-0 requests, so senders must omit it unless asked
        /// for (the CLI only sends it when a flag is passed).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        drain_timeout_ms: Option<u64>,
    },
    #[serde(rename = "supervisor.reload")]
    SupervisorReload { module_id: String },
    #[serde(rename = "supervisor.rescan")]
    SupervisorRescan {
        /// Compute the reconciliation and return it WITHOUT applying it.
        ///
        /// Rescan retires any supervised module absent from the config, which
        /// stops live processes. Both halves of that decision are inspectable in
        /// advance -- the config is a file, the running set is `supervisor.list`
        /// -- but nothing reconstructs the diff for the operator, so it is read
        /// from the result table AFTER the retires have happened.
        ///
        /// A preview must be computed daemon-side rather than by a client, because
        /// a client would have to locate the daemon's config itself: two rules
        /// selecting one subject, agreeing until someone runs a daemon with a
        /// non-default config. A preview that can describe a different file than
        /// the operation reads is worse than none, because it is believed.
        ///
        /// Defaults to false so an existing client sending `{}` still executes,
        /// and is OMITTED when false so the bytes an existing client sends are
        /// unchanged. Serialising `preview:false` would have altered the request's
        /// wire form for every caller that never asked for a preview -- caught by
        /// the golden fixture, which is the whole reason that pin exists.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        preview: bool,
    },
    /// Retire the retained exact-id reservation after its configuration entry has
    /// been removed. This is intentionally separate from rescan so deleting
    /// configuration never silently opens a protected module id to registration.
    #[serde(rename = "supervisor.release_reserved")]
    SupervisorReleaseReserved { module_id: String },
    #[serde(rename = "supervisor.set_enabled")]
    SupervisorSetEnabled { module_id: String, enabled: bool },
    #[serde(rename = "supervisor.health_probe")]
    SupervisorHealthProbe { module_id: String },
    #[serde(rename = "supervisor.health")]
    SupervisorHealth {},
    /// Enumerate the routes currently served by one supervised module, or every
    /// module when omitted.
    ///
    /// This privileged census is control-plane-only. It is deliberately not an
    /// MCP facade or agent-tool operation: callers holding the daemon control
    /// connection may inspect live route ownership, while agent-facing modules
    /// must not be able to address that surface at all.
    ///
    /// The daemon answers from its forwarding table under a read lock and never
    /// consults a module. That makes the read safe during a drain, when a module
    /// cannot be queried without recreating the hang/restart hazard that route
    /// status reads avoid.
    #[serde(rename = "supervisor.routes")]
    SupervisorRoutes {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        module_id: Option<String>,
    },
    /// Report source-tagged provenance for supervised modules, optionally narrowed
    /// to one module.
    #[serde(rename = "supervisor.provenance")]
    SupervisorProvenance {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        module_id: Option<String>,
    },
    /// Retained stderr for one module.
    ///
    /// A separate op rather than a field on `supervisor.list`: the tail is
    /// kilobytes per module and `list` renders every module, so carrying it in
    /// the snapshot would charge every status read for a payload almost no
    /// caller wants. Caps ride on the REQUEST so a caller wanting twenty lines
    /// and one wanting the whole ring need no separate fields anywhere.
    #[serde(rename = "supervisor.stderr_tail")]
    SupervisorStderrTail {
        module_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_lines: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<u32>,
    },
    /// Retained terminal exits for one module.
    ///
    /// This stays separate from `supervisor.list`: a history grows with every
    /// incident, while the list is a current-state read most callers issue often.
    ///
    /// The read MUST stay off the supervisor command channel — it reads the
    /// module's shared ring directly. This is a requirement, not an
    /// optimisation: when the supervision task itself dies, every
    /// command-channel op returns `CommandClosed`, and that is precisely the
    /// moment an operator needs the exit history most. A history reachable only
    /// through the machinery whose death you are diagnosing is unreachable when
    /// it matters. Proven failure mode, not a hypothetical.
    #[serde(rename = "supervisor.terminals")]
    SupervisorTerminals { module_id: String },
}

/// subc's channel-0 response body for client control RPCs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
pub enum ClientControlResponse {
    #[serde(rename = "server.describe")]
    ServerDescribe {
        protocol_ver: u8,
        subc_ops: Vec<String>,
        capabilities: Vec<String>,
        connected_clients: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        counters: Option<serde_json::Value>,
        /// Git commit the daemon was built from, or "unavailable" when the
        /// build could not read it. The crate version cannot discriminate a
        /// skewed daemon/CLI pair (it moves per release, not per commit), so
        /// this is the identity a consumer compares against its own embedded
        /// commit to detect that it is talking to an older build than it was
        /// compiled with. Absent from daemons predating the field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build_git_sha: Option<String>,
        /// sha256 of the workspace Cargo.lock at build time, or "unavailable".
        /// Answers "which dependency set" where the commit answers "which
        /// source"; a commit match with a digest mismatch means a rebuild
        /// against edited dependencies. Absent from daemons predating the
        /// field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build_lock_digest: Option<String>,
        /// Daemon-evaluated capability requirements. Present when the configured
        /// fleet has declarations to evaluate, so operators can inspect an absent
        /// required capability without parsing daemon logs.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capability_requirements: Vec<CapabilityRequirementStatus>,
    },
    #[serde(rename = "catalog.list")]
    CatalogList {
        generation: u64,
        modules: Vec<CatalogEntry>,
        subc_ops: Vec<String>,
    },
    #[serde(rename = "route.open")]
    RouteOpen {
        route_channel: u16,
        route_epoch: u32,
    },
    #[serde(rename = "route.poll")]
    RoutePoll {
        route_channel: u16,
        route_epoch: u32,
        status: Option<String>,
        live: Option<bool>,
    },
    #[serde(rename = "supervisor.list")]
    SupervisorList {
        generation: u64,
        modules: Vec<SupervisorEntry>,
    },
    #[serde(rename = "supervisor.ack")]
    SupervisorAck { module_id: String, applied: bool },
    #[serde(rename = "supervisor.rescan")]
    SupervisorRescan {
        #[serde(flatten)]
        result: SupervisorRescanResult,
    },
    #[serde(rename = "supervisor.health_probe")]
    SupervisorHealthProbe {
        module_id: String,
        status: HealthStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metrics: Option<serde_json::Value>,
    },
    #[serde(rename = "supervisor.health")]
    SupervisorHealth {
        generation: u64,
        modules: Vec<SupervisorHealthEntry>,
    },
    #[serde(rename = "supervisor.routes")]
    SupervisorRoutes { modules: Vec<SupervisorRouteModule> },
    #[serde(rename = "supervisor.provenance")]
    SupervisorProvenance {
        daemon: SupervisorDaemonProvenance,
        modules: Vec<SupervisorModuleProvenance>,
    },
    #[serde(rename = "supervisor.stderr_tail")]
    SupervisorStderrTail {
        module_id: String,
        #[serde(flatten)]
        tail: StderrTail,
    },
    #[serde(rename = "supervisor.terminals")]
    SupervisorTerminals {
        module_id: String,
        #[serde(flatten)]
        terminals: TerminalHistory,
    },
}

/// Daemon-originated channel-0 control push body.
///
/// A module cannot originate these pushes: subc creates them from its own
/// forwarding state and enqueues them directly to client connection sinks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
pub enum ClientControlPush {
    #[serde(rename = "route.closing")]
    RouteClosing {
        module_id: String,
        reason: RouteCloseReason,
    },
    #[serde(rename = "route.closed")]
    RouteClosed {
        module_id: String,
        reason: RouteCloseReason,
        /// The exact result of the forwarding-quiescence wait for live routes.
        drained: bool,
        /// Pending route.bind relays forced down before that wait. They are not
        /// covered by `drained`, even when live routes quiesced.
        abandoned: u32,
        /// Whether subc will leave this module down until operator action.
        ///
        /// The claim covers daemon-owned recovery only. `None` is accepted only
        /// from daemons that predate this field; every current daemon emission is
        /// `Some`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal: Option<bool>,
    },
}

/// Why subc is closing a module's client routes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteCloseReason {
    Reload,
    Restart,
    Disable,
    Crash,
    /// A live route became forbidden because newly attested capability metadata
    /// matched its supervised opening module's deny edge.
    CapabilityDenied,
}

/// A module's retained stderr, oldest entry first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StderrTail {
    pub capture: StderrCaptureState,
    pub entries: Vec<StderrTailEntry>,
    /// Lines not present above: evicted by the ring, or held back by this
    /// request's own caps.
    ///
    /// Non-zero means the first entry is not the first line the module wrote. A
    /// reader hunting a cause needs that, or an absent explanation reads as a
    /// module that never gave one.
    ///
    /// Zero is skipped so the common complete-tail case stays compact.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub dropped_lines: u64,
}

/// Live routes served by one module.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorRouteModule {
    pub module_id: String,
    pub routes: Vec<SupervisorRoute>,
}

/// One live consumer route in a [`SupervisorRouteModule`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorRoute {
    pub consumer: SupervisorRouteConsumer,
    /// Milliseconds since the daemon bound this route.
    pub age_ms: u64,
    /// True once the endpoint began draining. Draining routes remain visible so
    /// a census does not misreport an already-closing route as live.
    pub draining: bool,
    /// WHY the endpoint is draining — the same reason vocabulary the
    /// route.closing push carries — present exactly when `draining` is true.
    /// Additive: older daemons omit it, and a census consumer must treat a
    /// draining route without a reason as draining-for-an-unstated-reason,
    /// never as not-draining.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_reason: Option<RouteCloseReason>,
}

/// Source-tagged provenance for one supervised module.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorModuleProvenance {
    pub module_id: String,
    pub module_declared: ModuleDeclaredProvenance,
    pub daemon_observed: SupervisorObservedProcess,
}

/// A module's declared build metadata, if its HELLO manifest carried it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ModuleDeclaredProvenance {
    Reported { build: ManifestProvenance },
    Unverifiable,
}

/// Process facts observed by the daemon for a supervised module.
///
/// Build claims remain under `module_declared`; mixing them here would imply the
/// daemon independently observed module-provided metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorObservedProcess {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_from: Option<PathBuf>,
    pub running_image: RunningImageAgreement,
}

/// Daemon provenance paired with its runtime process observation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorDaemonProvenance {
    pub daemon_build: DaemonBuildProvenance,
    pub daemon_observed: DaemonObservedProcess,
}

/// Build metadata embedded in the daemon binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonBuildProvenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_git_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_lock_digest: Option<String>,
}

/// Runtime process facts observed for the daemon itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonObservedProcess {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    pub running_image: RunningImageAgreement,
}

/// Whether the executable currently running agrees with the spawned image.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunningImageAgreement {
    Match {
        evidence: RunningImageEvidence,
    },
    Mismatch {
        running: RunningImageEvidence,
        disk: RunningImageEvidence,
    },
    Unavailable {
        reason: RunningImageUnavailableReason,
    },
}

/// Platform-specific evidence used to compare a running image with its spawn path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RunningImageEvidence {
    LinuxProcSha256 { digest: String },
    MacosSpawnInode { device: u64, inode: u64 },
}

/// Closed reasons why an executable identity could not be observed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunningImageUnavailableReason {
    NotRunning,
    UnsupportedPlatform,
    RunningExecutableUnreadable,
    SpawnedPathUnreadable,
    HashFailed,
    ProcessIdentityUnconfirmed,
}

/// The identity tier the daemon can honestly report for a route consumer.
///
/// A caller that proved a live daemon-issued launch nonce is named `reserved`.
/// A direct key-holder has no such attestation, so it is reported as `direct`
/// with its connection counter instead of an invented module name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SupervisorRouteConsumer {
    Reserved { module_id: String },
    Direct { connection_id: u64 },
}

/// Whether stderr is being captured for a module, and if not, why not.
///
/// A typed state rather than an empty-tail convention. "The module printed
/// nothing before dying" and "nobody was capturing" send an operator in opposite
/// directions, and rendering them alike is the defect this op exists to fix --
/// the same shape as a `detail -` that means both no-detail and never-probed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StderrCaptureState {
    /// A reader is attached, or was attached and saw clean EOF. An empty
    /// `entries` under this state means the module genuinely wrote nothing.
    Captured,
    /// Retained entries are valid, but the stderr reader ended before clean EOF.
    Incomplete { reason: String },
    /// No reader was attached. `entries` says nothing about what the module wrote.
    NotCaptured { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StderrTailEntry {
    Line {
        text: String,
        /// The line was cut at the per-line cap and `text` is a prefix.
        ///
        /// Carried as a field rather than left to a marker in `text` so a
        /// consumer can branch on it without string matching.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    },
    /// The supervisor spawned a new process. Entries after this came from it.
    ///
    /// In-band because position is the information: which side of the restart a
    /// line falls on is unanswerable from a count.
    ProcessStart,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// Bounded terminal history for one module, oldest retained record first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalHistory {
    /// Unix milliseconds at daemon start. The history is in-memory and resets with
    /// the daemon, so an empty list only means "no exits during this incarnation".
    pub daemon_started_at_ms: u64,
    pub entries: Vec<TerminalEntry>,
    /// Earlier terminal exits evicted by the bounded ring.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub dropped: u64,
}

/// One terminal child exit and the supervisor action it selected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<i32>,
    pub at_ms: u64,
    pub disposition: TerminalDisposition,
}

/// The supervisor disposition selected after observing a terminal exit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminalDisposition {
    Stopped,
    Disabled,
    Failed,
    Restarting,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PollKind {
    Status,
    Liveness,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogEntry {
    pub module_id: String,
    /// The registered module's self-declared build version, projected from its
    /// manifest so a consumer can tell WHICH BUILD of a module it is talking
    /// to at connect time.
    ///
    /// Without this, a client compiled against a module's current source reads
    /// a contract that is true of the repository and false of the running
    /// process -- the types match, the JSON decodes, and the meaning has
    /// changed. That failure carries no error to notice; the version in the
    /// catalog turns a semantic skew into a log line at connect instead of a
    /// wrong sentence on a user's screen.
    ///
    /// Optional on the wire only because entries serialized by older daemons
    /// lack it: absent means "daemon predates the field", never "module has
    /// no version" (the manifest field is required at registration).
    ///
    /// The reading is ARMED BY OBSERVATION, not by this documentation: until
    /// a consumer has seen at least one populated entry from the daemon it is
    /// connected to, an all-None catalog is indistinguishable from an old
    /// daemon, and a client shipping the documented reading against it would
    /// hold a guarantee it does not have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_version: Option<String>,
    pub roles: Vec<ProviderRole>,
    pub control_ops: Vec<String>,
    /// Static capability declarations from the registering module's manifest.
    ///
    /// Optional on the wire so consumers connected to a daemon that predates the
    /// capability grammar retain their existing catalog decoding behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<CapabilityDeclarations>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityRequirementStatus {
    pub consumer: String,
    pub capability: String,
    pub need: String,
    pub verdict: String,
    pub episode_seq: u64,
    pub config_satisfiable: bool,
    pub runtime_available: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorRescanResult {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed_pending_reload: Vec<String>,
    /// Modules whose enabled flag differs between config and running state.
    ///
    /// Rescan calls `set_enabled` for these, so omitting them made the preview
    /// describe two of the three mutation classes it performs. A module changing
    /// only its enabled flag landed in no bucket at all -- not added, removed or
    /// changed, and deliberately not counted as unchanged either -- so the sole
    /// evidence was that the buckets no longer summed to the configured module
    /// count. A preview is consulted precisely when someone is being careful,
    /// which is the worst place to under-report.
    ///
    /// Empty is skipped so consumers written against the older shape keep
    /// parsing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_changes: Vec<String>,
    pub unchanged: u32,
    /// True when this reconciliation was computed but NOT applied.
    ///
    /// Carried on the result rather than left to the caller's memory of what it
    /// asked for. A preview and an execution are otherwise byte-identical, so a
    /// reader who meets this output later -- in a log, a transcript, a pasted
    /// snippet -- cannot tell which one happened. Absent when false, so existing
    /// consumers see the shape they already parse.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preview: bool,
    /// Config sections that changed but which rescan CANNOT apply, so the
    /// operator learns a daemon restart is required from the command they just
    /// ran rather than from the journal.
    ///
    /// The daemon has always detected this and logged a warning. A warning in a
    /// log is addressed to whoever is reading the log, and the person who just
    /// edited the config is by construction looking at the CLI instead: reported
    /// by an outside contributor after a module crash-looped through four
    /// respawns because a new top-level `storage` section was silently not
    /// applied, diagnosable only by journal archaeology.
    ///
    /// Names the SECTIONS rather than a boolean, because "something else
    /// changed" sends the operator back to diffing their own file -- which is
    /// the work the message exists to save.
    ///
    /// Empty is skipped, so consumers written against the older shape keep
    /// parsing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart_required: Vec<String>,
    /// Required capabilities that a dry-run's resulting module set would leave
    /// unprovided. Rows are human-readable because the preview is an operator
    /// explanation, not a second manifest schema.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capability_warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorEntry {
    pub module_id: String,
    pub state: String,
    pub enabled: bool,
    pub live: bool,
    pub health: SupervisorHealthStatus,
    /// When the daemon last collected this module's health, as unix
    /// milliseconds. Absent means NEVER PROBED (a module inside its first probe
    /// window, whose `health` is therefore `Unknown` rather than good), not
    /// probed-long-ago. An old value and an absent one call for opposite
    /// readings, so do not render them alike.
    #[serde(default)]
    pub last_probe_ms: Option<u64>,
    /// Exit code of the module's most recent process exit, if the process has
    /// exited at least once. Survives respawn so a now-`running` module still
    /// reports what killed its previous incarnation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
    /// Terminating signal of the module's most recent process exit (Unix), if
    /// any. `Some(9)` = SIGKILL (OOM/jetsam/kill-on-drop), `Some(6)` = SIGABRT
    /// (often a panic-abort). Survives respawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_signal: Option<i32>,
    /// Unix milliseconds when the most recent child exit was observed. Present
    /// even when the terminal ring is not queried, so existing list readers can
    /// order their latest observed exit against events they already received.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_ms: Option<u64>,
    /// Replacement processes spawned for this module so far, against the budget
    /// that disables it.
    ///
    /// THIS IS THE COUNTER THAT ENDS A MODULE, and it is not the one beside it.
    /// `SupervisorHealthEntry::consecutive_failures` returns to zero on any
    /// successful probe, so a module can miss probes all day and read zero; this
    /// one only decreases when an operator restarts, reloads, or re-enables the
    /// module. Reaching the budget moves it to `Failed` and it stays there until
    /// somebody intervenes.
    ///
    /// So a module one restart from being disabled is indistinguishable from a
    /// freshly booted one unless this pair is read. Both are reported together
    /// because the count alone does not say how close it is.
    ///
    /// Absent from daemons predating the field, which is why it is optional
    /// rather than defaulted to zero: zero would assert a full budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_count: Option<u32>,
    /// Replacement processes this module is allowed before it is disabled. See
    /// `restart_count`; absent on daemons predating the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_restarts: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorHealthStatus {
    Ok,
    Degraded,
    Failing,
    Unresponsive,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SupervisorHealthEntry {
    pub module_id: String,
    pub status: SupervisorHealthStatus,
    /// The module's own human-readable note on its state. Absent means the
    /// module said nothing, which is the ordinary shape for a healthy module and
    /// is NOT a claim that nothing is wrong. Never parse it: it is prose the
    /// module may reword freely, and `status` plus `metrics` are the machine
    /// surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The module's own metrics object, relayed opaquely. Absent means the module
    /// published none on this probe — either it reports no metrics at all, or the
    /// probe did not reach it — so absence cannot distinguish "nothing to report"
    /// from "nobody asked". Read `last_probe_ms` to tell those apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<serde_json::Value>,
    pub consecutive_failures: u32,
    /// Number of recurring health replies received after their daemon deadline.
    /// Each increment is evidence that the module remained alive despite a miss.
    #[serde(default)]
    pub late_answer_count: u64,
    /// End-to-end latency of the newest late reply, measured from probe start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_late_answer_latency_ms: Option<u64>,
    /// The escalation the supervisor last took for this module (report, restart,
    /// alert). Absent means NO ACTION HAS EVER BEEN TAKEN, not that the last one
    /// succeeded — a module that has never misbehaved and one whose action record
    /// predates a daemon restart both present as absent.
    #[serde(default)]
    pub last_action: Option<String>,
    /// When `last_action` was taken, as unix milliseconds. Absent exactly when
    /// `last_action` is absent; the pair moves together.
    #[serde(default)]
    pub last_action_ms: Option<u64>,
    /// When the daemon last collected this entry, as unix milliseconds.
    ///
    /// `supervisor.health` answers from the supervisor's STORED record rather
    /// than probing, so every field above describes some moment in the past and
    /// nothing here said which. That matters most right after a restart, where
    /// the surface is used to confirm a deploy: a record collected before the
    /// restart reports the OLD process, reads as a failed deploy, and invites a
    /// redeploy of something that was already correct.
    ///
    /// `None` means never probed — distinct from probed-long-ago, and the reader
    /// must not collapse them. Absent on modules that advertise no health
    /// capability, which is why it is optional rather than defaulted to zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use subc_protocol::{BindIdentity, RouteTarget};

    #[test]
    fn route_poll_uses_kind_field() {
        let body = serde_json::to_value(ClientControlRequest::RoutePoll {
            route_channel: 7,
            route_epoch: 11,
            kind: PollKind::Status,
        })
        .unwrap();

        assert_eq!(body["op"], "route.poll");
        assert_eq!(body["route_epoch"], 11);
        assert_eq!(body["kind"], "status");
        assert!(body.get("op").is_some());
    }

    #[test]
    fn route_open_is_internally_tagged() {
        let request = ClientControlRequest::RouteOpen {
            target: RouteTarget::ToolProvider {
                module_id: "aft".to_string(),
            },
            identity: BindIdentity {
                project_root: "/tmp/project".into(),
                harness: "opencode".to_string(),
                session: "session-1".to_string(),
            },
            consumer_identity: None,
            consumer_capabilities: None,
            admission_facts: None,
        };

        let body = serde_json::to_value(request).unwrap();
        assert_eq!(body["op"], "route.open");
        assert_eq!(body["target"]["kind"], "tool_provider");
        assert!(body.get("consumer_identity").is_none());
        assert!(body.get("consumer_capabilities").is_none());
    }

    #[test]
    fn route_open_without_optional_fields_still_decodes() {
        let body = serde_json::json!({
            "op": "route.open",
            "target": { "kind": "tool_provider", "module_id": "aft" },
            "identity": {
                "project_root": "/tmp/project",
                "harness": "opencode",
                "session": "session-1"
            }
        });

        let decoded: ClientControlRequest = serde_json::from_value(body).unwrap();
        let ClientControlRequest::RouteOpen {
            consumer_identity,
            consumer_capabilities,
            admission_facts,
            ..
        } = decoded
        else {
            panic!("decoded wrong request variant");
        };
        assert_eq!(consumer_identity, None);
        assert_eq!(consumer_capabilities, None);
        assert_eq!(admission_facts, None);
    }

    #[test]
    fn new_route_closed_decoder_accepts_old_daemon_without_terminal() {
        let old_wire = r#"{"op":"route.closed","module_id":"aft-tools","reason":"crash","drained":false,"abandoned":0}"#;
        let decoded: ClientControlPush = serde_json::from_str(old_wire).unwrap();
        match decoded {
            ClientControlPush::RouteClosed { terminal, .. } => assert_eq!(terminal, None),
            other => panic!("unexpected push: {other:?}"),
        }
        assert!(!serde_json::to_string(&decoded)
            .unwrap()
            .contains("terminal"));
    }

    #[test]
    fn old_route_closed_decoder_ignores_new_terminal_field() {
        #[derive(serde::Deserialize)]
        #[serde(tag = "op")]
        enum LegacyClientControlPush {
            #[serde(rename = "route.closed")]
            RouteClosed {
                module_id: String,
                reason: RouteCloseReason,
                drained: bool,
                abandoned: u32,
            },
        }

        let wire = r#"{"op":"route.closed","module_id":"aft-tools","reason":"crash","drained":false,"abandoned":0,"terminal":true}"#;
        let decoded: LegacyClientControlPush = serde_json::from_str(wire).unwrap();
        match decoded {
            LegacyClientControlPush::RouteClosed {
                module_id,
                reason,
                drained,
                abandoned,
            } => {
                assert_eq!(module_id, "aft-tools");
                assert_eq!(reason, RouteCloseReason::Crash);
                assert!(!drained);
                assert_eq!(abandoned, 0);
            }
        }
    }

    #[test]
    fn supervisor_routes_is_a_control_plane_request() {
        let body = serde_json::json!({
            "op": "supervisor.routes",
            "module_id": "aft"
        });

        let request: ClientControlRequest = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), body);
    }
}
