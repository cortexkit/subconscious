use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use serde_json::{json, Map, Value};
use subc_client_rs::{async_trait, HandlerOutcome, ModuleHandler, RequestCtx};
use subc_protocol::session::{HealthReport, HealthStatus};

use crate::{constants::DEFAULT_MAX_CHILDREN, registry::ServerRegistry};

const BAD_REQUEST: &str = "bad_request";
const SPAWN_SHAPED_FIELDS: &[&str] = &[
    "command",
    "argv",
    "args",
    "cwd",
    "env",
    "spawn",
    "spawn_spec",
];

/// Atomics let health checks report lifecycle state without blocking on child state.
#[derive(Debug)]
pub struct HealthMetrics {
    children_live: AtomicU64,
    children_max: AtomicU64,
    spawns_total: AtomicU64,
    spawn_failures_total: AtomicU64,
    idle_evictions_total: AtomicU64,
    calls_in_flight: AtomicU64,
    oldest_in_flight_ms: AtomicU64,
    cache_served_total: AtomicU64,
}

impl Default for HealthMetrics {
    fn default() -> Self {
        Self {
            children_live: AtomicU64::new(0),
            children_max: AtomicU64::new(DEFAULT_MAX_CHILDREN),
            spawns_total: AtomicU64::new(0),
            spawn_failures_total: AtomicU64::new(0),
            idle_evictions_total: AtomicU64::new(0),
            calls_in_flight: AtomicU64::new(0),
            oldest_in_flight_ms: AtomicU64::new(0),
            cache_served_total: AtomicU64::new(0),
        }
    }
}

impl HealthMetrics {
    pub fn snapshot(&self) -> Value {
        json!({
            "children_live": self.children_live.load(Ordering::Relaxed),
            "children_max": self.children_max.load(Ordering::Relaxed),
            "spawns_total": self.spawns_total.load(Ordering::Relaxed),
            "spawn_failures_total": self.spawn_failures_total.load(Ordering::Relaxed),
            "idle_evictions_total": self.idle_evictions_total.load(Ordering::Relaxed),
            "calls_in_flight": self.calls_in_flight.load(Ordering::Relaxed),
            "oldest_in_flight_ms": self.oldest_in_flight_ms.load(Ordering::Relaxed),
            "cache_served_total": self.cache_served_total.load(Ordering::Relaxed),
        })
    }
}

pub struct AdapterHandler {
    metrics: Arc<HealthMetrics>,
    // Child lifecycle code consumes this immutable startup registry to select a child.
    _registry: ServerRegistry,
}

impl AdapterHandler {
    pub fn new(registry: ServerRegistry) -> Self {
        Self {
            metrics: Arc::new(HealthMetrics::default()),
            _registry: registry,
        }
    }

    pub fn metrics(&self) -> &Arc<HealthMetrics> {
        &self.metrics
    }

    fn route_outcome(&self, body: &[u8]) -> HandlerOutcome {
        match parse_envelope(body) {
            Ok(_) => refusal("not_implemented", "MCP child forwarding is not implemented")
                .into_handler_outcome(),
            Err(error) => error.into_handler_outcome(),
        }
    }
}

#[async_trait]
impl ModuleHandler for AdapterHandler {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        self.route_outcome(&body)
    }

    async fn health(&self) -> HealthReport {
        HealthReport {
            status: HealthStatus::Ok,
            detail: Some("stdio MCP child lifecycle has no active children".to_string()),
            metrics: Some(self.metrics.snapshot()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EnvelopeError {
    InvalidJson,
    EnvelopeMustBeObject,
    UnsupportedOperation,
    MissingOrNonStringServer,
    NonObjectPayload,
    SpawnShapedField { field: String },
}

impl EnvelopeError {
    fn into_handler_outcome(self) -> HandlerOutcome {
        match self {
            Self::InvalidJson | Self::EnvelopeMustBeObject => {
                refusal("invalid_envelope", "route envelope must be a JSON object")
                    .into_handler_outcome()
            }
            Self::UnsupportedOperation => refusal(
                "unsupported_op",
                "route envelope op must be tools/list or tools/call",
            )
            .into_handler_outcome(),
            Self::MissingOrNonStringServer => refusal(
                "missing_server",
                "route envelope must include a string server",
            )
            .into_handler_outcome(),
            Self::NonObjectPayload => refusal(
                "non_object_payload",
                "route envelope payload must be an object",
            )
            .into_handler_outcome(),
            Self::SpawnShapedField { field } => refusal(
                "spawn_shaped_field",
                "route envelopes may not contain child spawn fields",
            )
            .with_field(field)
            .into_handler_outcome(),
        }
    }
}

fn refusal(reason: &str, message: &'static str) -> AdapterRefusal {
    AdapterRefusal {
        code: BAD_REQUEST,
        message,
        detail: json!({ "reason": reason }),
    }
}

struct AdapterRefusal {
    code: &'static str,
    message: &'static str,
    detail: Value,
}

impl AdapterRefusal {
    fn with_field(mut self, field: String) -> Self {
        if let Value::Object(detail) = &mut self.detail {
            detail.insert("field".to_string(), Value::String(field));
        }
        self
    }

    fn into_handler_outcome(self) -> HandlerOutcome {
        HandlerOutcome::ErrorWithDetail {
            code: self.code.to_string(),
            message: self.message.to_string(),
            detail: self.detail,
        }
    }
}

fn parse_envelope(body: &[u8]) -> Result<(), EnvelopeError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| EnvelopeError::InvalidJson)?;
    let object = value
        .as_object()
        .ok_or(EnvelopeError::EnvelopeMustBeObject)?;
    validate_operation(object)?;
    validate_server(object)?;
    validate_payload(object)?;
    if let Some(field) = find_spawn_shaped_field(&value) {
        return Err(EnvelopeError::SpawnShapedField { field });
    }
    Ok(())
}

fn validate_operation(object: &Map<String, Value>) -> Result<(), EnvelopeError> {
    match object.get("op").and_then(Value::as_str) {
        Some("tools/list" | "tools/call") => Ok(()),
        _ => Err(EnvelopeError::UnsupportedOperation),
    }
}

fn validate_server(object: &Map<String, Value>) -> Result<(), EnvelopeError> {
    object
        .get("server")
        .and_then(Value::as_str)
        .map(|_| ())
        .ok_or(EnvelopeError::MissingOrNonStringServer)
}

fn validate_payload(object: &Map<String, Value>) -> Result<(), EnvelopeError> {
    object
        .get("payload")
        .filter(|payload| payload.is_object())
        .map(|_| ())
        .ok_or(EnvelopeError::NonObjectPayload)
}

fn find_spawn_shaped_field(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => object.iter().find_map(|(field, value)| {
            if SPAWN_SHAPED_FIELDS.contains(&field.as_str()) {
                Some(field.clone())
            } else {
                find_spawn_shaped_field(value)
            }
        }),
        Value::Array(items) => items.iter().find_map(find_spawn_shaped_field),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::{json, Value};
    use subc_client_rs::{HandlerOutcome, ModuleHandler};
    use subc_protocol::session::HealthStatus;

    use super::{parse_envelope, AdapterHandler, EnvelopeError};
    use crate::registry::parse_document;

    fn handler() -> AdapterHandler {
        let (registry, warnings) = parse_document(
            Path::new("registry.jsonc"),
            r#"{ "github": { "command": "mcp" } }"#,
        )
        .unwrap();
        assert!(warnings.is_empty());
        AdapterHandler::new(registry)
    }

    fn refusal_for(body: Value) -> (String, String, Value, Value, Value) {
        let handler = handler();
        let before = handler.metrics().snapshot();
        let outcome =
            handler.route_outcome(&serde_json::to_vec(&body).expect("test request serializes"));
        let after = handler.metrics().snapshot();
        let HandlerOutcome::ErrorWithDetail {
            code,
            message,
            detail,
        } = outcome
        else {
            panic!("invalid envelope must produce a detailed ERROR outcome");
        };
        (code, message, detail, before, after)
    }

    #[test]
    fn unknown_op_is_a_typed_bad_request_without_child_side_effect() {
        let (code, _message, detail, before, after) = refusal_for(json!({
            "server": "github",
            "op": "resources/list",
            "payload": {},
        }));

        assert_eq!(code, "bad_request");
        assert_eq!(detail["reason"], "unsupported_op");
        assert_eq!(before, after);
    }

    #[test]
    fn missing_server_is_a_typed_bad_request_without_child_side_effect() {
        let (code, _message, detail, before, after) =
            refusal_for(json!({ "op": "tools/list", "payload": {} }));

        assert_eq!(code, "bad_request");
        assert_eq!(detail["reason"], "missing_server");
        assert_eq!(before, after);
    }

    #[test]
    fn non_object_payload_is_a_typed_bad_request_without_child_side_effect() {
        let (code, _message, detail, before, after) = refusal_for(json!({
            "server": "github",
            "op": "tools/list",
            "payload": [],
        }));

        assert_eq!(code, "bad_request");
        assert_eq!(detail["reason"], "non_object_payload");
        assert_eq!(before, after);
    }

    #[test]
    fn nested_spawn_shaped_field_is_a_typed_bad_request_without_child_side_effect() {
        let (code, _message, detail, before, after) = refusal_for(json!({
            "server": "github",
            "op": "tools/call",
            "payload": { "params": { "command": "/bin/sh" } },
        }));

        assert_eq!(code, "bad_request");
        assert_eq!(detail["reason"], "spawn_shaped_field");
        assert_eq!(detail["field"], "command");
        assert_eq!(before, after);
    }

    #[test]
    fn valid_envelope_is_accepted_before_child_forwarding_is_available() {
        assert_eq!(
            parse_envelope(
                br#"{"server":"github","op":"tools/list","payload":{"method":"tools/list"}}"#
            ),
            Ok(())
        );
    }

    #[test]
    fn parser_has_specific_errors_for_invalid_json_and_non_object_envelope() {
        assert_eq!(parse_envelope(b"{"), Err(EnvelopeError::InvalidJson));
        assert_eq!(
            parse_envelope(b"[]"),
            Err(EnvelopeError::EnvelopeMustBeObject)
        );
    }

    #[test]
    fn health_reports_every_stable_lifecycle_metric() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let report = runtime.block_on(handler().health());
        let metrics = report.metrics.expect("health must carry lifecycle metrics");

        assert_eq!(report.status, HealthStatus::Ok);
        for key in [
            "children_live",
            "children_max",
            "spawns_total",
            "spawn_failures_total",
            "idle_evictions_total",
            "calls_in_flight",
            "oldest_in_flight_ms",
            "cache_served_total",
        ] {
            assert!(metrics.get(key).is_some(), "missing metric {key}");
        }
        assert_eq!(metrics["children_live"], 0);
        assert_eq!(metrics["children_max"], 8);
        assert_eq!(metrics["spawns_total"], 0);
    }
}
