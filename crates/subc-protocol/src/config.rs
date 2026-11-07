//! Optional module config-gate control bodies.
//!
//! A module opts in by listing [`CONFIG_REVIEW_OP`] in HELLO `control_ops`, not
//! by changing its manifest. Send these operations only after HELLO_ACK advertises
//! [`CONFIG_GATE_CAPABILITY`]. Without it a module keeps its existing file reader;
//! with it, a refusal must never cause a fallback to unapproved files.
//!
//! These standalone bodies leave the existing exhaustive session enums alone.
//! Extension fields are ignored on read. Unknown policy strings preserve their
//! spelling but grant no authority. All calls use channel 0; failures use
//! [`crate::ErrorBody`], not a successful reply with an empty configuration.

use std::collections::BTreeMap;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use subc_config::{Digest, FieldChange, Layer};

pub const CONFIG_GATE_CAPABILITY: &str = "config.gate";
pub const CONFIG_REVIEW_OP: &str = "config.review";
pub const CONFIG_DECLARE_OP: &str = "config.declare";
pub const CONFIG_GET_OP: &str = "config.get";
pub const CONFIG_APPLIED_OP: &str = "config.applied";
pub const CONFIG_CHANGED_OP: &str = "config.changed";

macro_rules! op_tag {
    ($name:ident, $wire:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        enum $name {
            #[serde(rename = $wire)]
            Op,
        }
    };
}
op_tag!(ReviewOp, "config.review");
op_tag!(DeclareOp, "config.declare");
op_tag!(GetOp, "config.get");
op_tag!(AppliedOp, "config.applied");
op_tag!(ChangedOp, "config.changed");

macro_rules! open_string_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[non_exhaustive]
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(from = "String", into = "String")]
        pub enum $name {
            $($variant,)+
            /// A future spelling, retained for diagnostics but never authority.
            Unknown(String),
        }
        impl $name {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)+
                    Self::Unknown(value) => value,
                }
            }
        }
        impl From<String> for $name {
            fn from(value: String) -> Self {
                match value.as_str() {
                    $($wire => Self::$variant,)+
                    _ => Self::Unknown(value),
                }
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self { value.as_str().to_owned() }
        }
    };
}
pub(crate) use open_string_enum;

open_string_enum! {
    /// Snapshot layer identity. An unknown layer must not be merged or delivered.
    ConfigLayer {
        Default => "default", User => "user", Workspace => "workspace", Project => "project"
    }
}
impl ConfigLayer {
    pub fn known_layer(&self) -> Option<Layer> {
        match self {
            Self::Default => Some(Layer::Default),
            Self::User => Some(Layer::User),
            Self::Workspace => Some(Layer::Workspace),
            Self::Project => Some(Layer::Project),
            Self::Unknown(_) => None,
        }
    }
}

open_string_enum! {
    /// Delivery status. An unknown status must not be treated as an active view.
    ConfigStatus {
        Unstamped => "unstamped", Pending => "pending", Active => "active",
        Failed => "failed", Gated => "gated"
    }
}
impl ConfigStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }
}

open_string_enum! {
    /// Only an explicit ACK acknowledges a complete generation, including readiness.
    ConfigApplyResult { Ack => "ack", Nack => "nack" }
}
impl ConfigApplyResult {
    /// Unknown results have the same fail-closed disposition as a NACK.
    pub fn is_ack(&self) -> bool {
        matches!(self, Self::Ack)
    }
}

/// Immutable raw input, never an instruction to open a mutable path.
///
/// Exactly one of `path` and `proposal_id` identifies the input; the daemon
/// validates that location against the approved declaration. `bytes` and `digest`
/// are either both present or both absent (a missing file, not an empty file).
/// Bytes serialize as canonical RFC 4648 padded base64. Decoding checks both the
/// encoding and the SHA-256 of the exact bytes, including BOMs and line endings.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SnapshotWire")]
pub struct ConfigLayerSnapshot {
    pub module_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub layer: ConfigLayer,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    #[serde(
        serialize_with = "serialize_bytes",
        skip_serializing_if = "Option::is_none"
    )]
    pub bytes: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<Digest>,
}

impl ConfigLayerSnapshot {
    /// Snapshot a file or its absence. An empty byte slice is a present file.
    pub fn file(
        module_id: impl Into<String>,
        layer: ConfigLayer,
        path: impl Into<String>,
        bytes: Option<Vec<u8>>,
    ) -> Self {
        let digest = bytes.as_deref().map(subc_config::raw_digest);
        Self {
            module_id: module_id.into(),
            project_id: None,
            workspace_id: None,
            layer,
            path: Some(path.into()),
            proposal_id: None,
            bytes,
            digest,
        }
    }
}

fn serialize_bytes<S: serde::Serializer>(
    bytes: &Option<Vec<u8>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    bytes
        .as_ref()
        .map(|bytes| STANDARD.encode(bytes))
        .serialize(serializer)
}

#[derive(Deserialize)]
struct SnapshotWire {
    module_id: String,
    project_id: Option<String>,
    workspace_id: Option<String>,
    layer: ConfigLayer,
    path: Option<String>,
    proposal_id: Option<String>,
    bytes: Option<String>,
    digest: Option<Digest>,
}

impl TryFrom<SnapshotWire> for ConfigLayerSnapshot {
    type Error = String;
    fn try_from(wire: SnapshotWire) -> Result<Self, Self::Error> {
        let bytes = wire
            .bytes
            .map(|encoded| {
                let bytes = STANDARD
                    .decode(&encoded)
                    .map_err(|_| "bytes: invalid canonical base64")?;
                if STANDARD.encode(&bytes) != encoded {
                    return Err("bytes: non-canonical base64");
                }
                Ok(bytes)
            })
            .transpose()
            .map_err(str::to_owned)?;
        match (&bytes, &wire.digest) {
            (Some(bytes), Some(digest)) if subc_config::raw_digest(bytes) != *digest => {
                return Err("digest: does not match bytes".into())
            }
            (Some(_), None) => return Err("digest: required with bytes".into()),
            (None, Some(_)) => return Err("bytes: required with digest".into()),
            _ => {}
        }
        Ok(Self {
            module_id: wire.module_id,
            project_id: wire.project_id,
            workspace_id: wire.workspace_id,
            layer: wire.layer,
            path: wire.path,
            proposal_id: wire.proposal_id,
            bytes,
            digest: wire.digest,
        })
    }
}

/// One immutable module/project view. Generation zero with `unstamped` status
/// represents current files before stamping, not an approved generation.
/// Unknown statuses or snapshot layers must not be activated.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigView {
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub effective: Value,
    /// JSON pointer to the source layer of each effective field.
    pub sources: BTreeMap<String, Layer>,
    pub layers: Vec<ConfigLayerSnapshot>,
    pub status: ConfigStatus,
}
impl ConfigView {
    pub fn new(
        generation: u64,
        effective: Value,
        sources: BTreeMap<String, Layer>,
        layers: Vec<ConfigLayerSnapshot>,
        status: ConfigStatus,
    ) -> Self {
        Self {
            generation,
            attempt: None,
            project_id: None,
            effective,
            sources,
            layers,
            status,
        }
    }
}

/// Pure daemon-to-module review of immutable candidate layers against a supplied
/// active base. The module must not read files, environment, network or secrets,
/// or recompute old values under a new build. `base: null` means no active base.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigReviewRequest {
    op: ReviewOp,
    pub base_generation: u64,
    pub layers: Vec<ConfigLayerSnapshot>,
    pub base: Option<ConfigView>,
}
impl ConfigReviewRequest {
    pub fn new(
        base_generation: u64,
        layers: Vec<ConfigLayerSnapshot>,
        base: Option<ConfigView>,
    ) -> Self {
        Self {
            op: ReviewOp::Op,
            base_generation,
            layers,
            base,
        }
    }
}

/// A field or project refusal. Cross-field reasons name every implicated JSON
/// pointer in `paths`. Reasons must not contain secret values.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigReviewRefusal {
    pub code: String,
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub reasons: Vec<String>,
}
impl ConfigReviewRefusal {
    pub fn new(code: impl Into<String>, paths: Vec<String>, reasons: Vec<String>) -> Self {
        Self {
            code: code.into(),
            paths,
            project_id: None,
            reasons,
        }
    }
}

/// Normalized old/new values, sources and apply kinds for every field, including
/// unchanged values whose source changed. Any refusal prevents approval; the
/// daemon validates this reply against the schema and supplied immutable base.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigReviewReply {
    op: ReviewOp,
    pub fields: Vec<FieldChange>,
    pub refusals: Vec<ConfigReviewRefusal>,
}
impl ConfigReviewReply {
    pub fn new(fields: Vec<FieldChange>, refusals: Vec<ConfigReviewRefusal>) -> Self {
        Self {
            op: ReviewOp::Op,
            fields,
            refusals,
        }
    }
}

/// Declare once after HELLO_ACK and before the first get or applied on this
/// connection. The schema and layer locations are proposals, not authority:
/// only the approved declaration is enforced after stamping. Workspace mappings
/// remain daemon-owned. Module identity comes from the connection, never a body.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigDeclareRequest {
    op: DeclareOp,
    pub schema: Value,
    pub user_file: String,
    pub project_dirs: Vec<String>,
}
impl ConfigDeclareRequest {
    pub fn new(schema: Value, user_file: impl Into<String>, project_dirs: Vec<String>) -> Self {
        Self {
            op: DeclareOp::Op,
            schema,
            user_file: user_file.into(),
            project_dirs,
        }
    }
}

/// Pull the caller's view. Exact-generation selectors are only for the caller's
/// daemon-assigned delivery attempt, not arbitrary modules or project tuples.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigGetRequest {
    op: GetOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
}
impl ConfigGetRequest {
    pub fn new() -> Self {
        Self {
            op: GetOp::Op,
            project_id: None,
            generation: None,
            attempt: None,
        }
    }
}
impl Default for ConfigGetRequest {
    fn default() -> Self {
        Self::new()
    }
}

/// Active and optional pending views. Apply `pending` before ACKing it; retain
/// `active` for recovery. A module with no approved configuration is refused
/// with [`crate::error_codes::CONFIG_GATED`], never given empty file bytes.
/// `active: null` is permitted when a first delivery has no prior active view.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigGetReply {
    op: GetOp,
    pub active: Option<ConfigView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<ConfigView>,
}
impl ConfigGetReply {
    pub fn new(active: Option<ConfigView>, pending: Option<ConfigView>) -> Self {
        Self {
            op: GetOp::Op,
            active,
            pending,
        }
    }
}

/// A complete-generation outcome bound to the caller's child, project and
/// delivery attempt. An ACK includes readiness, never a partial field apply.
/// Duplicate, stale or wrong-generation outcomes cannot move active pointers.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigAppliedRequest {
    op: AppliedOp,
    pub generation: u64,
    pub attempt: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub result: ConfigApplyResult,
    pub reasons: Vec<String>,
}
impl ConfigAppliedRequest {
    pub fn new(
        generation: u64,
        attempt: u64,
        result: ConfigApplyResult,
        reasons: Vec<String>,
    ) -> Self {
        Self {
            op: AppliedOp::Op,
            generation,
            attempt,
            project_id: None,
            result,
            reasons,
        }
    }
}

/// Daemon hint to the active delivery target only. Resynchronize on reconnect
/// and ignore stale hints; this is not an activation record.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChanged {
    op: ChangedOp,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// JSON pointers of fields whose view changed.
    pub fields: Vec<String>,
}
impl ConfigChanged {
    pub fn new(generation: u64, fields: Vec<String>) -> Self {
        Self {
            op: ChangedOp::Op,
            generation,
            project_id: None,
            fields,
        }
    }
}

macro_rules! empty_reply {
    ($(#[$meta:meta])* $name:ident, $tag:ident) => {
        $(#[$meta])*
        #[non_exhaustive]
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct $name { op: $tag }
        impl $name { pub fn new() -> Self { Self { op: $tag::Op } } }
        impl Default for $name { fn default() -> Self { Self::new() } }
    };
}
empty_reply! {
    /// Receipt of a declaration does not approve its schema or locations.
    ConfigDeclareReply, DeclareOp
}
empty_reply! {
    /// Receipt of an outcome; refusals travel in an ERROR frame instead.
    ConfigAppliedReply, AppliedOp
}
