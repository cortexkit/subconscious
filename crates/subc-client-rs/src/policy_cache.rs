//! Revision-aware policy resolution for modules that enforce fleet gates.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use subc_protocol::{BindIdentity, RouteTarget};
use tokio::time::timeout;

use crate::{CallOptions, RouteHandle, SubcConsumer, SubscribeOptions};

/// The resolver module used when a deployment does not override the target.
pub const DEFAULT_POLICY_RESOLVER_MODULE_ID: &str = "prefrontal-core";

const POLICY_RESOLVER_HARNESS: &str = "subc-client-rs-policy-resolver";
const POLICY_RESOLVE_OP: &str = "policy.resolve";
const POLICY_REVISION_BUMP_OP: &str = "policy.revision_bump";
const POLICY_SUBSCRIBE_OP: &str = "policy.subscribe";

/// The principal whose policy is being resolved. Wire encoding is the
/// resolver's UNTAGGED object-key form — `{"agent_id": ...}` or
/// `{"session_id": ...}` — pinned by the producer-real contract vectors
/// (tests/fixtures/policy_resolve_contract_vectors.json, vendored from
/// prefrontal where each committed request is executed against live dispatch).
/// The first cut serialized a tagged {kind, value} form and every live call
/// failed shape-parse: prose pinned the field names but only bytes pin an
/// encoding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum Subject {
    /// A stable registry agent identifier.
    #[serde(rename = "agent_id")]
    AgentId(String),
    /// A session identifier that the resolver maps to an agent.
    #[serde(rename = "session_id")]
    SessionToResolve(String),
}

impl Subject {
    fn route_session(&self) -> String {
        match self {
            Self::AgentId(agent_id) => format!("agent:{agent_id}"),
            Self::SessionToResolve(session_id) => format!("session:{session_id}"),
        }
    }
}

/// A resolver decision. The vocabulary is closed AT AUTHORING on the resolver
/// side (policy.set refuses anything but allow | deny | ask, mutation-proved
/// there), with two reply-only values: `deny` doubles as the policy-less
/// closed default, and `deny_unknown_domain` marks a domain no producer
/// declared — a consumer bug surfaced as its own variant rather than folded
/// into an ordinary deny. `ask` is the ask-first/park spelling: the caller-arm
/// split (attended parks, unattended treats as transient) happens ABOVE this
/// type; the helper only carries the verdict. Unknown wire values are retained
/// so an older consumer does not discard a valid reply from a newer resolver.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PolicyVerdict {
    Allow,
    Deny,
    Ask,
    DenyUnknownDomain,
    Unknown(String),
}

impl PolicyVerdict {
    /// Return the wire spelling of this decision.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::DenyUnknownDomain => "deny_unknown_domain",
            Self::Unknown(value) => value,
        }
    }
}

impl Serialize for PolicyVerdict {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PolicyVerdict {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            "ask" => Self::Ask,
            "deny_unknown_domain" => Self::DenyUnknownDomain,
            _ => Self::Unknown(value),
        })
    }
}

/// Bounds owned by the shared resolver helper rather than individual consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyResolverConfig {
    /// Maximum duration for opening the resolver route and receiving its reply.
    pub hard_timeout: Duration,
    /// Minimum cache lifetime applied to a resolver-provided TTL.
    pub ttl_floor_ms: u64,
}

/// A resolver failure rather than a policy decision. The `cause` is
/// diagnostic prose for logs and operators, never for matching: callers
/// branch on the VARIANT (per the decision-vs-fault split), and the cause
/// exists because a unit Fault swallowed three different integration defects
/// (subject shape, request envelope, wrong route plane) in its first hour,
/// each costing a raw-probe cycle to see through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyResolveError {
    /// The resolver could not supply a usable decision within the hard timeout.
    Fault { cause: String },
}

impl PolicyResolveError {
    fn fault(cause: impl Into<String>) -> Self {
        Self::Fault {
            cause: cause.into(),
        }
    }
}

impl fmt::Display for PolicyResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fault { cause } => write!(f, "policy resolver fault: {cause}"),
        }
    }
}

impl Error for PolicyResolveError {}

/// Shared resolve-with-cache helper for fleet policy gates.
pub struct PolicyResolver {
    // Arc so the bump-subscription task can hold the consumer without
    // demanding Clone on SubcConsumer's public surface.
    consumer: std::sync::Arc<SubcConsumer>,
    resolver_module_id: String,
    config: PolicyResolverConfig,
    cache: Arc<Mutex<CacheState>>,
}

struct CacheState {
    last_known_revision: u64,
    entries: HashMap<CacheKey, CacheEntry>,
    push_routes: HashSet<RouteHandle>,
}

impl CacheState {
    fn new() -> Self {
        Self {
            last_known_revision: 0,
            entries: HashMap::new(),
            push_routes: HashSet::new(),
        }
    }

    fn observe_revision(&mut self, revision: u64) {
        if revision > self.last_known_revision {
            self.last_known_revision = revision;
            self.entries.clear();
        }
    }
}

/// The project scope being resolved. Untagged params-level form pinned by the
/// producer vectors: exactly one of `project_root` (filesystem root) or
/// `project_id` (registry id, resolved through entorhinal) appears in params.
/// An unknown id REFUSES (`project_unresolved`) rather than draping the
/// closed default over a typo -- the vector's own name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum ProjectRef {
    #[serde(rename = "project_root")]
    Root(String),
    #[serde(rename = "project_id")]
    Id(String),
}

impl ProjectRef {
    /// The bind-identity project root: a real root binds as itself; an id has
    /// no filesystem meaning, so its route identity uses a stable synthetic
    /// root (the daemon canonicalizes vanished roots, and the resolver never
    /// reads this field for id-form scope resolution).
    fn bind_root(&self) -> PathBuf {
        match self {
            Self::Root(root) => PathBuf::from(root),
            Self::Id(_) => PathBuf::from("/"),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    domain: String,
    gate_id: String,
    subject: Subject,
    project: ProjectRef,
}

struct CacheEntry {
    verdict: PolicyVerdict,
    revision: u64,
    expires_at: Instant,
}

#[derive(Serialize)]
/// The managed-call envelope: `{method, params}` out, `{result}` back — the
/// module-op convention every consumer speaks (proven against the live
/// resolver; the first cut sent a flat body its own fake accepted, which is
/// the build-local trap: the fake must mirror the CONVENTION, not the draft).
struct PolicyResolveRequest<'a> {
    method: &'static str,
    params: PolicyResolveParams<'a>,
}

#[derive(Serialize)]
struct PolicyResolveParams<'a> {
    domain: &'a str,
    gate_id: &'a str,
    subject: &'a Subject,
    #[serde(flatten)]
    project: &'a ProjectRef,
}

#[derive(Deserialize)]
struct PolicyResolveEnvelope {
    result: PolicyResolveReply,
}

#[derive(Deserialize)]
struct PolicyResolveReply {
    verdict: PolicyVerdict,
    /// The CURRENT global policy generation at resolve time — the cache
    /// watermark — never the matched rule's write stamp (producer-pinned
    /// semantic: any policy write in any scope bumps it).
    revision: u64,
    ttl_ms: u64,
}

#[derive(Deserialize)]
struct PolicyRevisionBumpBody {
    revision: u64,
}

/// The held-stream event, NESTED framing pinned by the producer's push_event
/// fixture entry ({op, body: {revision}} -- the rooms.hint_wait emit shape).
/// A flat {revision} was the eighth encoding drift; the fixture's byte-pin
/// against the producer's own encoder is what keeps this parser honest.
#[derive(Deserialize)]
struct PolicyRevisionBump {
    op: String,
    body: PolicyRevisionBumpBody,
}

impl PolicyResolver {
    /// Create a resolver that targets [`DEFAULT_POLICY_RESOLVER_MODULE_ID`].
    pub fn new(consumer: SubcConsumer, config: PolicyResolverConfig) -> Self {
        Self::with_resolver_target(consumer, DEFAULT_POLICY_RESOLVER_MODULE_ID, config)
    }

    /// Create a resolver that targets a deployment-specific policy module.
    pub fn with_resolver_target(
        consumer: SubcConsumer,
        resolver_module_id: impl Into<String>,
        config: PolicyResolverConfig,
    ) -> Self {
        Self {
            consumer: std::sync::Arc::new(consumer),
            resolver_module_id: resolver_module_id.into(),
            config,
            cache: Arc::new(Mutex::new(CacheState::new())),
        }
    }

    /// Resolve one gate, using a revision-validated cache entry only while its TTL remains live.
    pub async fn resolve(
        &self,
        domain: &str,
        gate_id: &str,
        subject: Subject,
        project: ProjectRef,
    ) -> Result<PolicyVerdict, PolicyResolveError> {
        let key = CacheKey {
            domain: domain.to_string(),
            gate_id: gate_id.to_string(),
            subject: subject.clone(),
            project: project.clone(),
        };

        if let Some(verdict) = self.cached_verdict(&key) {
            return Ok(verdict);
        }

        let request = PolicyResolveRequest {
            method: POLICY_RESOLVE_OP,
            params: PolicyResolveParams {
                domain,
                gate_id,
                subject: &subject,
                project: &project,
            },
        };
        let body = serde_json::to_vec(&request)
            .map_err(|e| PolicyResolveError::fault(format!("request encode: {e}")))?;
        let route_identity = BindIdentity {
            project_root: project.bind_root(),
            harness: POLICY_RESOLVER_HARNESS.to_string(),
            session: subject.route_session(),
        };
        let call_options = CallOptions {
            timeout: self.config.hard_timeout,
            route_retry_deadline: self.config.hard_timeout,
            ..CallOptions::default()
        };

        let wire_call = async {
            let route = self
                .consumer
                .open_route(
                    // policy.resolve serves on the resolver's MANAGEMENT
                    // SURFACE (live-proven; a ToolProvider bind reaches the
                    // wrong plane and faults every call).
                    RouteTarget::ManagementSurface {
                        module_id: self.resolver_module_id.clone(),
                    },
                    route_identity,
                    call_options.clone(),
                )
                .await
                .map_err(|e| PolicyResolveError::fault(format!("route open: {e}")))?;
            self.install_push_receiver(route)?;
            self.consumer
                .request(&route, body, call_options)
                .await
                .map_err(|e| PolicyResolveError::fault(format!("request: {e}")))
        };
        let response = timeout(self.config.hard_timeout, wire_call)
            .await
            .map_err(|_| PolicyResolveError::fault("hard timeout elapsed"))??;
        // Module replies wrap in the `{result}` envelope; a missing wrapper is
        // a contract violation, not a tolerable variant — the raw shape never
        // reaches consumers un-enveloped on this convention.
        let envelope: PolicyResolveEnvelope = serde_json::from_slice(&response)
            .map_err(|e| PolicyResolveError::fault(format!("reply envelope: {e}")))?;
        let reply = envelope.result;
        let ttl = Duration::from_millis(reply.ttl_ms.max(self.config.ttl_floor_ms));
        let expires_at = Instant::now()
            .checked_add(ttl)
            .ok_or_else(|| PolicyResolveError::fault("ttl overflow"))?;

        let mut cache = lock(&self.cache);
        // A regression cannot be a fresh answer after this process has observed a
        // newer monotonic generation, so never return or cache it as a decision.
        if reply.revision < cache.last_known_revision {
            return Err(PolicyResolveError::fault(
                "reply revision regressed below an observed generation",
            ));
        }
        cache.observe_revision(reply.revision);
        cache.entries.insert(
            key,
            CacheEntry {
                verdict: reply.verdict.clone(),
                revision: reply.revision,
                expires_at,
            },
        );
        Ok(reply.verdict)
    }

    fn cached_verdict(&self, key: &CacheKey) -> Option<PolicyVerdict> {
        let cache = lock(&self.cache);
        let entry = cache.entries.get(key)?;
        if entry.expires_at > Instant::now() && entry.revision == cache.last_known_revision {
            Some(entry.verdict.clone())
        } else {
            None
        }
    }

    /// Hold the resolver's `policy.subscribe` stream and fold revision bumps
    /// into the cache watermark. The bump lane is the HOUSE SUBSCRIPTION
    /// pattern -- a held-open request answered with StreamData events -- not
    /// spontaneous Push frames; the first cut installed a push receiver its
    /// own fake satisfied while the live module streamed to nobody (the
    /// convention-vs-draft class, sixth member). Best-effort by constraint 4:
    /// the stream dying only means staleness reverts to the TTL bound, so the
    /// holder re-subscribes on the next resolve rather than retrying in a
    /// loop.
    fn install_push_receiver(&self, route: RouteHandle) -> Result<(), PolicyResolveError> {
        {
            let mut cache = lock(&self.cache);
            if !cache.push_routes.insert(route) {
                return Ok(());
            }
        }
        let consumer = std::sync::Arc::clone(&self.consumer);
        let cache = Arc::clone(&self.cache);
        let body = serde_json::to_vec(&serde_json::json!({
            "method": POLICY_SUBSCRIBE_OP,
            "params": {},
        }))
        .expect("static subscribe body serializes");
        tokio::spawn(async move {
            match consumer
                .subscribe_route(&route, body, SubscribeOptions::default())
                .await
            {
                Ok(mut subscription) => {
                    drain_revision_bumps(subscription.events(), Arc::clone(&cache)).await;
                }
                Err(_) => {
                    // No bump lane: TTL alone bounds staleness (constraint 4
                    // makes that correct, merely slower). Clearing the marker
                    // lets the next resolve retry the subscription.
                }
            }
            lock(&cache).push_routes.remove(&route);
        });
        Ok(())
    }
}

async fn drain_revision_bumps(
    events: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    cache: Arc<Mutex<CacheState>>,
) {
    while let Some(body) = events.recv().await {
        let Ok(bump) = serde_json::from_slice::<PolicyRevisionBump>(&body) else {
            continue;
        };
        if bump.op == POLICY_REVISION_BUMP_OP {
            // A push only makes cached values stale sooner. It never completes a
            // resolve, creates a verdict, or extends a cached entry's lifetime.
            lock(&cache).observe_revision(bump.body.revision);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::PolicyVerdict;

    #[test]
    fn unknown_verdict_strings_are_retained_for_forward_compatibility() {
        let verdict: PolicyVerdict = serde_json::from_str("\"future_verdict\"").unwrap();
        assert_eq!(
            verdict,
            PolicyVerdict::Unknown("future_verdict".to_string())
        );
    }
}
