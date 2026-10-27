//! Daemon-observed facts about the process on the other end of a route's
//! opening connection, relayed to the target module on `route.bind`.
//!
//! These keys sit outside the exhaustive [`ModuleControlRequest::RouteBind`]
//! variant so that adding them changes no Rust construction site. A daemon
//! encodes a bind with [`encode_route_bind`], which appends them after the
//! variant's own keys; a module decodes the same body twice, once as
//! [`ModuleControlRequest`] and once as [`RouteBindPeer`].
//!
//! Each key is sent only to a target declaring its capability in
//! `capabilities.provides`, and a target declaring neither receives exactly
//! the bytes it received before these keys existed.

use serde::{ser::Error as _, Deserialize, Deserializer, Serialize, Serializer};

use crate::session::ModuleControlRequest;

/// Declaring this in `capabilities.provides` asks the daemon for
/// [`RouteBindPeer::observed_peer`] on every `route.bind`.
pub const OBSERVED_PEER_CAPABILITY: &str = "observed-peer/v1";

/// Declaring this in `capabilities.provides` asks the daemon for
/// [`RouteBindPeer::operator_app`] on a `route.bind` whose route was opened by
/// a recognised operator app.
pub const OPERATOR_APP_CAPABILITY: &str = "operator-app/v1";

/// Whether the daemon could read the peer's identity from the kernel when it
/// accepted the connection.
///
/// A value from a newer daemon decodes as [`ObservedPeerStatus::Unknown`],
/// keeping its spelling. Readers must treat anything other than
/// [`ObservedPeerStatus::Observed`] as carrying no peer facts.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObservedPeerStatus {
    /// The daemon read the peer's user (and possibly more) at accept time.
    Observed,
    /// The connection offers no kernel peer identity (TCP, or a platform
    /// without the primitive), or reading it failed.
    Unobserved,
    /// A status this crate does not know, kept with its spelling so it
    /// re-encodes unchanged.
    Unknown(String),
}

impl Serialize for ObservedPeerStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Observed => "observed",
            Self::Unobserved => "unobserved",
            Self::Unknown(value) => value,
        })
    }
}

impl<'de> Deserialize<'de> for ObservedPeerStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "observed" => Self::Observed,
            "unobserved" => Self::Unobserved,
            value => Self::Unknown(value.to_owned()),
        })
    }
}

/// What the daemon learned from the kernel about the process that opened the
/// route, read once when its connection was accepted.
///
/// This is an observation, not an authorization: the route's principal is
/// unchanged by it. `exe` is informational. `signing_id` is present only when
/// the platform validated the process's code signature.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservedPeer {
    pub status: ObservedPeerStatus,
    /// The peer's effective user id (macOS and Linux).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// The peer's user SID (Windows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    /// The peer's executable path, omitted when it could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    /// The peer's code-signing identifier, present only for validly signed
    /// code on a platform that checks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_id: Option<String>,
}

impl ObservedPeer {
    /// A connection with no kernel peer identity: `{"status":"unobserved"}`.
    pub fn unobserved() -> Self {
        Self {
            status: ObservedPeerStatus::Unobserved,
            uid: None,
            sid: None,
            exe: None,
            signing_id: None,
        }
    }

    /// An observed peer identified by its Unix user id.
    pub fn observed_uid(uid: u32) -> Self {
        Self {
            status: ObservedPeerStatus::Observed,
            uid: Some(uid),
            ..Self::unobserved()
        }
    }

    /// An observed peer identified by its Windows user SID.
    pub fn observed_sid(sid: impl Into<String>) -> Self {
        Self {
            status: ObservedPeerStatus::Observed,
            sid: Some(sid.into()),
            ..Self::unobserved()
        }
    }

    /// Set or clear the executable path.
    #[must_use]
    pub fn with_exe(mut self, exe: Option<String>) -> Self {
        self.exe = exe;
        self
    }

    /// Set or clear the code-signing identifier.
    #[must_use]
    pub fn with_signing_id(mut self, signing_id: Option<String>) -> Self {
        self.signing_id = signing_id;
        self
    }
}

/// The daemon's statement that the route was opened over a connection it
/// recognised, when the connection was made, as a verified operator app.
///
/// It reports a fact fixed at bind time and is never retracted on the route.
/// It is not authority: a module still asks the daemon with `operator.confirm`
/// before acting on the person's behalf, and the daemon decides.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorApp {
    /// The app's identifier in the daemon's compiled app table.
    pub app: String,
    /// The app's process id when its connection was made.
    pub pid: u32,
}

impl OperatorApp {
    pub fn new(app: impl Into<String>, pid: u32) -> Self {
        Self {
            app: app.into(),
            pid,
        }
    }
}

/// The `route.bind` keys carried beside [`ModuleControlRequest::RouteBind`].
///
/// Both are absent unless the target declared the matching capability, and an
/// absent key is omitted, never `null`. Decoding ignores every other key, so a
/// module reads this from the same body it decodes as the bind.
#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteBindPeer {
    /// Sent only to a target declaring [`OBSERVED_PEER_CAPABILITY`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_peer: Option<ObservedPeer>,
    /// Sent only to a target declaring [`OPERATOR_APP_CAPABILITY`], and only
    /// when the daemon recognised the opening connection as an operator app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_app: Option<OperatorApp>,
}

impl RouteBindPeer {
    /// No peer keys: the bind keeps its legacy bytes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set or clear `observed_peer`.
    #[must_use]
    pub fn with_observed_peer(mut self, observed_peer: Option<ObservedPeer>) -> Self {
        self.observed_peer = observed_peer;
        self
    }

    /// Set or clear `operator_app`.
    #[must_use]
    pub fn with_operator_app(mut self, operator_app: Option<OperatorApp>) -> Self {
        self.operator_app = operator_app;
        self
    }

    /// Whether neither key is present.
    pub fn is_empty(&self) -> bool {
        self.observed_peer.is_none() && self.operator_app.is_none()
    }
}

/// Encodes a `route.bind` body with `peer`'s keys after the bind's own keys.
///
/// With an empty `peer` the bytes equal `serde_json::to_vec(bind)`. Fails for
/// any request other than [`ModuleControlRequest::RouteBind`], because only a
/// bind carries these keys.
pub fn encode_route_bind(
    bind: &ModuleControlRequest,
    peer: &RouteBindPeer,
) -> serde_json::Result<Vec<u8>> {
    if !matches!(bind, ModuleControlRequest::RouteBind { .. }) {
        return Err(serde_json::Error::custom(
            "peer keys can only be added to a route.bind request",
        ));
    }
    if peer.is_empty() {
        return serde_json::to_vec(bind);
    }

    #[derive(Serialize)]
    struct WithPeer<'a> {
        #[serde(flatten)]
        bind: &'a ModuleControlRequest,
        #[serde(flatten)]
        peer: &'a RouteBindPeer,
    }
    serde_json::to_vec(&WithPeer { bind, peer })
}
