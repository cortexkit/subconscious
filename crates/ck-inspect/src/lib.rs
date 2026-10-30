//! Read-only process observations for operators and owner-authorized flows.

#[cfg(any(target_os = "linux", test))]
mod proc_threads;
mod profile;
#[cfg(any(target_os = "macos", test))]
mod sample;
mod sampler;

pub use profile::{Profile, Stack, Thread};

use async_trait::async_trait;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use subc_client_rs::{
    BindDecision, ConsumerOptions, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest,
    SubcConsumer,
};
use subc_control::{LiveSpawn, SupervisorEntry};
use subc_protocol::{
    manifest::{
        CapabilityDeclarations, Concurrency, ManagementOperation, ManagementOperationKind,
        ModuleManifest, ProviderRole,
    },
    Principal,
};

#[derive(Debug, PartialEq)]
struct Refusal {
    code: &'static str,
    reason: &'static str,
    retry_after_ms: Option<u64>,
}

impl Refusal {
    fn new(code: &'static str, reason: &'static str) -> Self {
        Self {
            code,
            reason,
            retry_after_ms: None,
        }
    }
    fn outcome(self) -> HandlerOutcome {
        let mut detail = json!({"class": "terminal", "reason": self.reason});
        if let Some(ms) = self.retry_after_ms {
            detail["retry_after_ms"] = ms.into();
        }
        HandlerOutcome::ErrorWithDetail {
            code: self.code.into(),
            message: self.reason.into(),
            detail,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Request {
    Resources,
    Profile { module_id: String, seconds: u64 },
}

/// Parse a request body by hand, so each refusal names what was wrong without
/// echoing the caller's input. The body is the fleet's management envelope,
/// `{"method": "<operation>", "params": {...}}`, which every management caller
/// sends (flows through Basal, the phone, `ck`). `params` may be left out when
/// the operation takes none. `seconds` accepts any JSON number with a whole
/// value, because callers in JavaScript hold numbers as doubles and may send 8
/// as 8.0.
fn parse_request(body: &[u8]) -> Result<Request, Refusal> {
    let invalid = |reason| Refusal::new("invalid_request", reason);
    let value: Value =
        serde_json::from_slice(body).map_err(|_| invalid("request body is not JSON"))?;
    let envelope = value
        .as_object()
        .ok_or_else(|| invalid("request body must be a JSON object"))?;
    if !envelope
        .keys()
        .all(|key| key == "method" || key == "params")
    {
        return Err(invalid("request takes only method and params"));
    }
    let method = envelope
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("request needs a method string"))?;
    let empty = serde_json::Map::new();
    let object = match envelope.get("params") {
        None => &empty,
        Some(params) => params
            .as_object()
            .ok_or_else(|| invalid("params must be a JSON object"))?,
    };
    let only = |allowed: &[&str]| object.keys().all(|key| allowed.contains(&key.as_str()));
    match method {
        "module.resources" => {
            if !object.is_empty() {
                return Err(invalid("module.resources takes no params"));
            }
            Ok(Request::Resources)
        }
        "module.profile" => {
            if !only(&["module_id", "seconds"]) {
                return Err(invalid(
                    "module.profile params take only module_id and seconds",
                ));
            }
            let module_id = object
                .get("module_id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("module_id must be a string"))?;
            let seconds = object
                .get("seconds")
                .and_then(whole_number)
                .filter(|seconds| (1..=10).contains(seconds))
                .ok_or_else(|| invalid("seconds must be a whole number from 1 to 10"))?;
            Ok(Request::Profile {
                module_id: module_id.to_owned(),
                seconds,
            })
        }
        _ => Err(invalid(
            "unknown method; expected module.resources or module.profile",
        )),
    }
}

fn whole_number(value: &Value) -> Option<u64> {
    if let Some(n) = value.as_u64() {
        return Some(n);
    }
    let f = value.as_f64()?;
    (f.is_finite() && f >= 0.0 && f.fract() == 0.0 && f <= u64::MAX as f64).then_some(f as u64)
}

#[async_trait]
trait Roster: Send + Sync {
    async fn list(&self) -> Result<Vec<SupervisorEntry>, Refusal>;
    async fn snapshot(&self) -> Result<Vec<LiveSpawn>, Refusal>;
}

#[async_trait]
impl Roster for SubcConsumer {
    async fn list(&self) -> Result<Vec<SupervisorEntry>, Refusal> {
        self.supervisor_list()
            .await
            .map(|reply| reply.modules)
            .map_err(|_| {
                Refusal::new(
                    "roster_unavailable",
                    "daemon does not provide the supervised roster",
                )
            })
    }
    async fn snapshot(&self) -> Result<Vec<LiveSpawn>, Refusal> {
        self.spawn_snapshot()
            .await
            .map(|reply| reply.live)
            .map_err(|_| {
                Refusal::new(
                    "roster_unavailable",
                    "daemon does not provide the supervised spawn snapshot",
                )
            })
    }
}

#[async_trait]
trait Sampler: Send + Sync {
    fn supported(&self) -> bool;
    async fn capture(
        &self,
        profile: Profile,
        deadline: tokio::time::Instant,
    ) -> Result<Profile, &'static str>;
}
struct OsSampler;
#[async_trait]
impl Sampler for OsSampler {
    fn supported(&self) -> bool {
        cfg!(any(target_os = "macos", target_os = "linux"))
    }
    async fn capture(
        &self,
        profile: Profile,
        deadline: tokio::time::Instant,
    ) -> Result<Profile, &'static str> {
        sampler::capture(profile, deadline).await
    }
}

struct Bounds {
    last: Mutex<HashMap<String, tokio::time::Instant>>,
    active: Arc<tokio::sync::Semaphore>,
}

struct ProfilePermit<'a> {
    bounds: &'a Bounds,
    id: String,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for ProfilePermit<'_> {
    fn drop(&mut self) {
        self.bounds
            .last
            .lock()
            .unwrap()
            .insert(self.id.clone(), tokio::time::Instant::now());
    }
}

impl Bounds {
    fn new() -> Self {
        Self {
            last: Mutex::new(HashMap::new()),
            active: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
    fn enter(&self, id: &str) -> Result<ProfilePermit<'_>, Refusal> {
        let permit = self
            .active
            .clone()
            .try_acquire_owned()
            .map_err(|_| Refusal::new("busy", "another profile is running"))?;
        let mut last = self.last.lock().unwrap();
        let now = tokio::time::Instant::now();
        if let Some(at) = last.get(id) {
            let remaining =
                Duration::from_secs(60).saturating_sub(now.saturating_duration_since(*at));
            if !remaining.is_zero() {
                return Err(Refusal {
                    code: "rate_limited",
                    reason: "module was profiled recently",
                    retry_after_ms: Some(remaining.as_millis().try_into().unwrap_or(u64::MAX)),
                });
            }
        }
        last.retain(|_, at| now.saturating_duration_since(*at) < Duration::from_secs(60));
        Ok(ProfilePermit {
            bounds: self,
            id: id.into(),
            _permit: permit,
        })
    }
}

struct Inspect {
    roster: Arc<dyn Roster>,
    sampler: Arc<dyn Sampler>,
    bounds: Bounds,
}

fn admitted(req: &RouteBindRequest) -> bool {
    matches!(req.principal, Some(Principal::Direct))
        || req
            .scope
            .as_ref()
            .is_some_and(|stamp| stamp.owner_authorized && stamp.attributes.flow_id.is_some())
}

fn target(entries: &[LiveSpawn], id: &str) -> Result<(u32, u64), Refusal> {
    entries
        .iter()
        .find(|e| e.module_id == id && e.pid > 0)
        .map(|e| (e.pid, e.spawn_generation))
        .ok_or_else(|| Refusal::new("not_running", "module has no live supervised process"))
}

impl Inspect {
    async fn query(&self, body: &[u8]) -> Result<Value, Refusal> {
        match parse_request(body)? {
            Request::Resources => {
                let entries = self.roster.list().await?;
                let live = self.roster.snapshot().await?;
                let modules = entries
                    .iter()
                    .map(|entry| {
                        let value = serde_json::to_value(entry).expect("supervisor entry is JSON");
                        let fields = ["module_id", "state", "spawn_generation", "resources"];
                        let mut output: serde_json::Map<String, Value> = fields
                            .into_iter()
                            .filter_map(|key| value.get(key).cloned().map(|v| (key.into(), v)))
                            .collect();
                        if let Some(spawn) = live.iter().find(|s| {
                            s.module_id == entry.module_id
                                && Some(s.spawn_generation) == entry.spawn_generation
                        }) {
                            output.insert("pid".into(), spawn.pid.into());
                        }
                        Value::Object(output)
                    })
                    .collect::<Vec<_>>();
                Ok(json!({"modules": modules}))
            }
            Request::Profile { module_id, seconds } => {
                let entries = self.roster.list().await?;
                if !entries.iter().any(|e| e.module_id == module_id) {
                    return Err(Refusal::new("unknown_module", "module is not supervised"));
                }
                let before = self.roster.snapshot().await?;
                let (pid, generation) = target(&before, &module_id)?;
                if !self.sampler.supported() {
                    return Err(Refusal::new(
                        "unsupported_platform",
                        "profiling is supported only on macOS and Linux",
                    ));
                }
                let _permit = self.bounds.enter(&module_id)?;
                let method = if cfg!(target_os = "macos") {
                    "sample"
                } else {
                    "proc_threads"
                };
                let profile = Profile::new(module_id.clone(), pid, seconds, method.into());
                let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds + 5);
                let captured =
                    tokio::time::timeout_at(deadline, self.sampler.capture(profile, deadline))
                        .await;
                // Check identity even after a sampler failure: a process that exited
                // during capture must never produce a result for its successor.
                let after = self.roster.snapshot().await?;
                if target(&after, &module_id).ok() != Some((pid, generation)) {
                    return Err(Refusal::new(
                        "module_restarted",
                        "supervised process changed during profiling",
                    ));
                }
                let mut profile = captured
                    .map_err(|_| Refusal::new("sampler_failed", "sampler deadline exceeded"))?
                    .map_err(|reason| Refusal::new("sampler_failed", reason))?;
                profile
                    .cap()
                    .map_err(|reason| Refusal::new("sampler_failed", reason))?;
                Ok(serde_json::to_value(profile).expect("profile is JSON"))
            }
        }
    }
}

#[async_trait]
impl ModuleHandler for Inspect {
    async fn on_bind(&self, req: &RouteBindRequest) -> BindDecision {
        if admitted(req) {
            BindDecision::accept()
        } else {
            BindDecision::reject(
                "not_permitted",
                "inspection requires a direct operator or an owner-authorized flow",
            )
        }
    }
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        match self.query(&body).await {
            Ok(value) => HandlerOutcome::Response(result_envelope(&value)),
            Err(error) => error.outcome(),
        }
    }
}

/// A successful management reply is `{"result": <value>}`, the envelope every
/// management caller unwraps (Basal refuses a reply without it as unreadable).
/// Refusals go out as Error frames instead, carrying their code and reason.
fn result_envelope(value: &Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "result": value })).expect("query result is JSON")
}

pub fn manifest() -> ModuleManifest {
    let operations = ["module.resources", "module.profile"];
    ModuleManifest::builder("inspect", env!("CARGO_PKG_VERSION"))
        .capabilities(Some(CapabilityDeclarations {
            provides: vec!["flow-scopes/v1".into()],
            requires: vec![],
            must_never_reach: vec![],
        }))
        .flow_operations(Some(operations.iter().map(|s| (*s).into()).collect()))
        .provides(vec![ProviderRole::ManagementSurface {
            operations: operations
                .into_iter()
                .map(|name| ManagementOperation {
                    name: name.into(),
                    kind: ManagementOperationKind::Query,
                    description: None,
                })
                .collect(),
            config_schema: json!({}),
            observability: vec![],
            identity_scope: vec![],
            concurrency: Concurrency::StatelessParallel,
        }])
        .build()
}

/// Serve until the daemon closes its module connection. The consumer connection
/// is private to inspection; callers can select queries, never control RPCs.
pub async fn serve(connection_file: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let roster = SubcConsumer::connect(connection_file, ConsumerOptions::default()).await?;
    let handler = Inspect {
        roster: Arc::new(roster),
        sampler: Arc::new(OsSampler),
        bounds: Bounds::new(),
    };
    let (_, serving) =
        subc_client_rs::serve_with_handle(connection_file, manifest(), handler).await?;
    serving.await?;
    Ok(())
}

#[cfg(test)]
mod tests;
