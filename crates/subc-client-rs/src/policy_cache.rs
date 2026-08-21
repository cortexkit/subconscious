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

use crate::{CallOptions, RouteHandle, SubcConsumer};

/// The resolver module used when a deployment does not override the target.
pub const DEFAULT_POLICY_RESOLVER_MODULE_ID: &str = "prefrontal-core";

const POLICY_RESOLVER_HARNESS: &str = "subc-client-rs-policy-resolver";
const POLICY_RESOLVE_OP: &str = "policy.resolve";
const POLICY_REVISION_BUMP_OP: &str = "policy.revision_bump";

/// The principal whose policy is being resolved.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Subject {
    /// A stable registry agent identifier.
    AgentId(String),
    /// A session identifier that the resolver maps to an agent.
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

/// A resolver decision. Unknown wire values are retained so an older consumer
/// does not discard a valid reply from a newer resolver.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PolicyVerdict {
    Allowed,
    Denied,
    Unknown(String),
}

impl PolicyVerdict {
    /// Return the wire spelling of this decision.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied => "denied",
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
            "allowed" => Self::Allowed,
            "denied" => Self::Denied,
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

/// A resolver failure rather than a policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyResolveError {
    /// The resolver could not supply a usable decision within the hard timeout.
    Fault,
}

impl fmt::Display for PolicyResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fault => f.write_str("policy resolver fault"),
        }
    }
}

impl Error for PolicyResolveError {}

/// Shared resolve-with-cache helper for fleet policy gates.
pub struct PolicyResolver {
    consumer: SubcConsumer,
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

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    domain: String,
    gate_id: String,
    subject: Subject,
    project_root: String,
}

struct CacheEntry {
    verdict: PolicyVerdict,
    revision: u64,
    expires_at: Instant,
}

#[derive(Serialize)]
struct PolicyResolveRequest<'a> {
    op: &'static str,
    domain: &'a str,
    gate_id: &'a str,
    subject: &'a Subject,
    project_root: &'a str,
}

#[derive(Deserialize)]
struct PolicyResolveReply {
    verdict: PolicyVerdict,
    revision: u64,
    ttl_ms: u64,
}

#[derive(Deserialize)]
struct PolicyRevisionBump {
    op: String,
    revision: u64,
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
            consumer,
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
        project_root: &str,
    ) -> Result<PolicyVerdict, PolicyResolveError> {
        let key = CacheKey {
            domain: domain.to_string(),
            gate_id: gate_id.to_string(),
            subject: subject.clone(),
            project_root: project_root.to_string(),
        };

        if let Some(verdict) = self.cached_verdict(&key) {
            return Ok(verdict);
        }

        let request = PolicyResolveRequest {
            op: POLICY_RESOLVE_OP,
            domain,
            gate_id,
            subject: &subject,
            project_root,
        };
        let body = serde_json::to_vec(&request).map_err(|_| PolicyResolveError::Fault)?;
        let route_identity = BindIdentity {
            project_root: PathBuf::from(project_root),
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
                    RouteTarget::ToolProvider {
                        module_id: self.resolver_module_id.clone(),
                    },
                    route_identity,
                    call_options.clone(),
                )
                .await
                .map_err(|_| PolicyResolveError::Fault)?;
            self.install_push_receiver(route)?;
            self.consumer
                .request(&route, body, call_options)
                .await
                .map_err(|_| PolicyResolveError::Fault)
        };
        let response = timeout(self.config.hard_timeout, wire_call)
            .await
            .map_err(|_| PolicyResolveError::Fault)??;
        let reply: PolicyResolveReply =
            serde_json::from_slice(&response).map_err(|_| PolicyResolveError::Fault)?;
        let ttl = Duration::from_millis(reply.ttl_ms.max(self.config.ttl_floor_ms));
        let expires_at = Instant::now()
            .checked_add(ttl)
            .ok_or(PolicyResolveError::Fault)?;

        let mut cache = lock(&self.cache);
        // A regression cannot be a fresh answer after this process has observed a
        // newer monotonic generation, so never return or cache it as a decision.
        if reply.revision < cache.last_known_revision {
            return Err(PolicyResolveError::Fault);
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

    fn install_push_receiver(&self, route: RouteHandle) -> Result<(), PolicyResolveError> {
        let events = {
            let mut cache = lock(&self.cache);
            if !cache.push_routes.insert(route) {
                return Ok(());
            }
            match self.consumer.push_events(&route) {
                Ok(events) => events,
                Err(_) => {
                    cache.push_routes.remove(&route);
                    return Err(PolicyResolveError::Fault);
                }
            }
        };
        let cache = Arc::clone(&self.cache);
        tokio::spawn(async move {
            drain_revision_bumps(events, Arc::clone(&cache)).await;
            lock(&cache).push_routes.remove(&route);
        });
        Ok(())
    }
}

async fn drain_revision_bumps(
    mut events: tokio::sync::mpsc::Receiver<crate::PushEvent>,
    cache: Arc<Mutex<CacheState>>,
) {
    while let Some(event) = events.recv().await {
        let Ok(bump) = serde_json::from_slice::<PolicyRevisionBump>(&event.body) else {
            continue;
        };
        if bump.op == POLICY_REVISION_BUMP_OP {
            // A push only makes cached values stale sooner. It never completes a
            // resolve, creates a verdict, or extends a cached entry's lifetime.
            lock(&cache).observe_revision(bump.revision);
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
