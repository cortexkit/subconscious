//! Client-facing subc channel-0 control wire shapes.
//!
//! This crate is the client ↔ subc control-plane boundary. It depends only on
//! [`subc-protocol`] for shared primitives such as `RouteTarget` and
//! `BindIdentity`; clients can use it without depending on the
//! daemon implementation.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use subc_protocol::{manifest::ProviderRole, session::HealthStatus, BindIdentity, RouteTarget};

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
    pub const SUPERVISOR_LIST: &str = "supervisor.list";
    pub const SUPERVISOR_RESTART: &str = "supervisor.restart";
    pub const SUPERVISOR_RELOAD: &str = "supervisor.reload";
    pub const SUPERVISOR_RESCAN: &str = "supervisor.rescan";
    pub const SUPERVISOR_SET_ENABLED: &str = "supervisor.set_enabled";
    pub const SUPERVISOR_HEALTH_PROBE: &str = "supervisor.health_probe";
    pub const SUPERVISOR_HEALTH: &str = "supervisor.health";
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
    SupervisorRestart { module_id: String },
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
    #[serde(rename = "supervisor.set_enabled")]
    SupervisorSetEnabled { module_id: String, enabled: bool },
    #[serde(rename = "supervisor.health_probe")]
    SupervisorHealthProbe { module_id: String },
    #[serde(rename = "supervisor.health")]
    SupervisorHealth {},
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
}
