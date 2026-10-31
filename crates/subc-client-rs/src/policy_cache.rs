//! Revision-aware policy resolution for modules that enforce fleet gates.

use std::{
    collections::HashMap,
    error::Error,
    fmt,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use subc_protocol::{BindIdentity, RouteTarget};
use tokio::time::timeout;

use crate::{CallOptions, CloseRouteOptions, RouteHandle, SubcConsumer, SubscribeOptions};

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
///
/// Each subject (per project root) resolves over its own route, which also
/// carries a `policy.subscribe` task. State for a subject is released once its
/// verdicts expire: expired entries are removed, and a route whose last verdict
/// has expired, with no resolve in flight on it, is closed and its subscription
/// task aborted. That happens in a sweep at the start of `resolve`, which runs
/// only once the earliest recorded expiry has passed, so a resolver holds state
/// only for subjects with a verdict still live, plus at most whatever expired
/// since its last call. Expiry was chosen over an LRU cap because TTL is what
/// already bounds a verdict's usefulness, and a cap would either close routes
/// with live verdicts or need a size nobody can pick for every deployment.
pub struct PolicyResolver {
    // Arc so the bump-subscription task can hold the consumer without
    // demanding Clone on SubcConsumer's public surface.
    consumer: std::sync::Arc<SubcConsumer>,
    resolver_module_id: String,
    config: PolicyResolverConfig,
    cache: Arc<Mutex<CacheState>>,
    /// Held by a sweep while it chooses and closes idle routes, and by a
    /// resolve while it opens its route. Without it a sweep could close a
    /// route that a resolve had just reopened from the consumer's cache.
    route_gate: tokio::sync::Mutex<()>,
    /// Running subscription tasks, counted by the tasks themselves.
    live_subscriptions: Arc<AtomicUsize>,
}

/// A resolver route's identity: bind root and route session. Two subjects or
/// projects that bind the same identity share the route.
type RouteIdentity = (PathBuf, String);

struct CacheState {
    last_known_revision: u64,
    entries: HashMap<CacheKey, CacheEntry>,
    routes: HashMap<RouteIdentity, RouteState>,
    /// Earliest instant at which some entry or route may have become
    /// releasable; `None` when nothing is waiting to expire.
    next_sweep_at: Option<Instant>,
    next_subscription_id: u64,
}

struct RouteState {
    handle: Option<RouteHandle>,
    /// Expiry of the latest verdict resolved over this route. A revision bump
    /// invalidates verdicts but not the route; it stays until this passes.
    live_until: Instant,
    in_flight: usize,
    subscription: Option<SubscriptionTask>,
}

struct SubscriptionTask {
    id: u64,
    handle: RouteHandle,
    abort: tokio::task::AbortHandle,
}

impl CacheState {
    fn new() -> Self {
        Self {
            last_known_revision: 0,
            entries: HashMap::new(),
            routes: HashMap::new(),
            next_sweep_at: None,
            next_subscription_id: 0,
        }
    }

    fn observe_revision(&mut self, revision: u64) {
        if revision > self.last_known_revision {
            self.last_known_revision = revision;
            self.entries.clear();
        }
    }

    fn schedule_sweep(&mut self, at: Instant) {
        self.next_sweep_at = Some(self.next_sweep_at.map_or(at, |next| next.min(at)));
    }

    fn sweep_due(&self, now: Instant) -> bool {
        self.next_sweep_at.is_some_and(|at| at <= now)
    }

    /// Remove expired entries and idle routes; return the route handles the
    /// caller must close. Aborting a subscription task drops its
    /// `Subscription`, which sends the provider a Cancel.
    fn evict_expired(&mut self, now: Instant) -> Vec<RouteHandle> {
        self.entries.retain(|_, entry| entry.expires_at > now);
        let mut closing = Vec::new();
        self.routes.retain(|_, route| {
            if route.in_flight > 0 || route.live_until > now {
                return true;
            }
            if let Some(subscription) = route.subscription.take() {
                subscription.abort.abort();
            }
            closing.extend(route.handle);
            false
        });
        self.next_sweep_at = None;
        let pending = self
            .entries
            .values()
            .map(|entry| entry.expires_at)
            .chain(self.routes.values().map(|route| route.live_until))
            .collect::<Vec<_>>();
        for at in pending {
            self.schedule_sweep(at);
        }
        closing
    }
}

/// Marks a resolve in flight on a route so a sweep leaves the route open.
struct InFlight {
    cache: Arc<Mutex<CacheState>>,
    route: RouteIdentity,
}

impl InFlight {
    fn enter(cache: &Arc<Mutex<CacheState>>, route: RouteIdentity) -> Self {
        let now = Instant::now();
        let mut state = lock(cache);
        state
            .routes
            .entry(route.clone())
            .or_insert_with(|| RouteState {
                handle: None,
                live_until: now,
                in_flight: 0,
                subscription: None,
            })
            .in_flight += 1;
        Self {
            cache: Arc::clone(cache),
            route,
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut state = lock(&self.cache);
        let Some(route) = state.routes.get_mut(&self.route) else {
            return;
        };
        route.in_flight -= 1;
        if route.in_flight == 0 {
            // A failed or cancelled resolve leaves no verdict to extend the
            // route's life; let the next sweep consider it.
            let live_until = route.live_until;
            state.schedule_sweep(live_until);
        }
    }
}

/// Decrements the live subscription count when its task ends, including by
/// abort (which drops the task's future, and this with it).
struct LiveSubscription(Arc<AtomicUsize>);

impl Drop for LiveSubscription {
    fn drop(&mut self) {
        self.0.fetch_sub(1, AtomicOrdering::SeqCst);
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

/// Per-subject state a [`PolicyResolver`] holds, from [`PolicyResolver::footprint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyResolverFootprint {
    /// Cached verdicts.
    pub entries: usize,
    /// Resolver routes held open, one per subject and project root.
    pub routes: usize,
    /// Running `policy.subscribe` tasks, at most one per held route.
    pub subscriptions: usize,
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
            route_gate: tokio::sync::Mutex::new(()),
            live_subscriptions: Arc::new(AtomicUsize::new(0)),
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

        self.sweep_expired().await;
        if let Some(verdict) = self.cached_verdict(&key) {
            return Ok(verdict);
        }
        let route_id: RouteIdentity = (project.bind_root(), subject.route_session());
        let _in_flight = InFlight::enter(&self.cache, route_id.clone());

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
        let route_identity =
            BindIdentity::new(POLICY_RESOLVER_HARNESS.to_string(), subject.route_session());
        let call_options = CallOptions {
            timeout: self.config.hard_timeout,
            route_retry_deadline: self.config.hard_timeout,
            ..CallOptions::default()
        };

        let wire_call = async {
            let gate = self.route_gate.lock().await;
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
            drop(gate);
            self.install_push_receiver(&route_id, route);
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
        if let Some(route) = cache.routes.get_mut(&route_id) {
            route.live_until = route.live_until.max(expires_at);
        }
        cache.schedule_sweep(expires_at);
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

    /// How much per-subject state this resolver holds right now.
    pub fn footprint(&self) -> PolicyResolverFootprint {
        let cache = lock(&self.cache);
        PolicyResolverFootprint {
            entries: cache.entries.len(),
            routes: cache
                .routes
                .values()
                .filter(|route| route.handle.is_some())
                .count(),
            subscriptions: self.live_subscriptions.load(AtomicOrdering::SeqCst),
        }
    }

    /// Release expired entries and the routes and subscription tasks of
    /// subjects with no live verdict. A no-op until the earliest recorded
    /// expiry has passed.
    async fn sweep_expired(&self) {
        if !lock(&self.cache).sweep_due(Instant::now()) {
            return;
        }
        let _gate = self.route_gate.lock().await;
        let closing = lock(&self.cache).evict_expired(Instant::now());
        for handle in closing {
            // A handle from an earlier connection generation fails locally;
            // that route is already gone with its connection.
            let _ = self
                .consumer
                .close_handle(&handle, CloseRouteOptions::default())
                .await;
        }
    }

    fn cached_verdict(&self, key: &CacheKey) -> Option<PolicyVerdict> {
        let mut cache = lock(&self.cache);
        let entry = cache.entries.get(key)?;
        if entry.expires_at > Instant::now() && entry.revision == cache.last_known_revision {
            Some(entry.verdict.clone())
        } else {
            // Never usable again: expired, or from an older revision.
            cache.entries.remove(key);
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
    ///
    /// One task per held route: the task is recorded on the route's state, is
    /// replaced when the route's handle changes (a reconnect), and is aborted
    /// when a sweep closes the route.
    fn install_push_receiver(&self, route_id: &RouteIdentity, route: RouteHandle) {
        let mut cache = lock(&self.cache);
        let id = cache.next_subscription_id;
        let Some(state) = cache.routes.get_mut(route_id) else {
            return;
        };
        state.handle = Some(route);
        if let Some(existing) = &state.subscription {
            if existing.handle == route {
                return;
            }
            existing.abort.abort();
        }
        let consumer = std::sync::Arc::clone(&self.consumer);
        let task_cache = Arc::clone(&self.cache);
        let task_route_id = route_id.clone();
        self.live_subscriptions.fetch_add(1, AtomicOrdering::SeqCst);
        let live = LiveSubscription(Arc::clone(&self.live_subscriptions));
        let body = serde_json::to_vec(&serde_json::json!({
            "method": POLICY_SUBSCRIBE_OP,
            "params": {},
        }))
        .expect("static subscribe body serializes");
        let task = tokio::spawn(async move {
            let _live = live;
            match consumer
                .subscribe_route(&route, body, SubscribeOptions::default())
                .await
            {
                Ok(mut subscription) => {
                    drain_revision_bumps(subscription.events(), Arc::clone(&task_cache)).await;
                }
                Err(_) => {
                    // No bump lane: TTL alone bounds staleness (constraint 4
                    // makes that correct, merely slower). Clearing the marker
                    // lets the next resolve retry the subscription.
                }
            }
            let mut cache = lock(&task_cache);
            if let Some(state) = cache.routes.get_mut(&task_route_id) {
                if state
                    .subscription
                    .as_ref()
                    .is_some_and(|task| task.id == id)
                {
                    state.subscription = None;
                }
            }
        });
        state.subscription = Some(SubscriptionTask {
            id,
            handle: route,
            abort: task.abort_handle(),
        });
        cache.next_subscription_id += 1;
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
