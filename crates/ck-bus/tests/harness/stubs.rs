use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::{json, Value};
use subc_client_rs::{BindDecision, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest};
use subc_protocol::{
    manifest::{
        Concurrency, ExecutionMode, ManagementOperation, ManagementOperationKind, ModuleManifest,
        ProviderRole, Tool,
    },
    Principal,
};

pub const CLAUSTRUM_OPERATIONS: &[&str] = &[
    "credential.get",
    "credential.get_scoped",
    "credential.sign",
    "credential.public_key",
    "credential.list_scoped",
    "credential.status",
    "credential.report_auth_failure",
];

pub const CALLOSUM_OPERATIONS: &[&str] = &["callosum.hub_read"];

#[derive(Clone, Default)]
pub struct StubReplyTable {
    replies: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
}

impl StubReplyTable {
    pub fn record_json(&self, operation: &str, body: Value) {
        self.replies
            .lock()
            .expect("stub reply table lock must remain usable")
            .insert(
                operation.to_string(),
                serde_json::to_vec(&body).expect("recorded stub reply must encode"),
            );
    }

    fn reply(&self, operation: &str) -> Option<Vec<u8>> {
        self.replies
            .lock()
            .expect("stub reply table lock must remain usable")
            .get(operation)
            .cloned()
    }
}

#[derive(Clone)]
pub struct StubRecorder {
    module_id: &'static str,
    operations: Arc<BTreeSet<String>>,
    replies: StubReplyTable,
    principals: Arc<Mutex<Vec<Option<Principal>>>>,
}

impl StubRecorder {
    pub fn refusing(module_id: &'static str, operations: &[&str]) -> Self {
        Self {
            module_id,
            operations: Arc::new(operations.iter().map(|op| (*op).to_string()).collect()),
            replies: StubReplyTable::default(),
            principals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn manifest(&self) -> ModuleManifest {
        if self.module_id == "claustrum" {
            // Claustrum registers its read surface as a management surface
            // (`credentials-module/src/main.rs::manifest` at 57a501b), not a tool
            // provider, and the daemon routes by role, so the stub standing in for it
            // registers the same way.
            let operations = self
                .operations
                .iter()
                .map(|name| ManagementOperation {
                    name: name.clone(),
                    kind: ManagementOperationKind::Query,
                    description: Some("acceptance harness shape stub".to_string()),
                })
                .collect();
            return ModuleManifest::builder(self.module_id, "0.0.0-harness-stub")
                .provides(vec![ProviderRole::ManagementSurface {
                    operations,
                    config_schema: json!({}),
                    observability: vec![],
                    identity_scope: vec![],
                    concurrency: Concurrency::ModuleManaged,
                }])
                .build();
        }
        let tools = self
            .operations
            .iter()
            .map(|name| Tool {
                name: name.clone(),
                description: Some("acceptance harness shape stub".to_string()),
                execution_mode: ExecutionMode::Pure,
                schema: json!({"type": "object"}),
            })
            .collect();
        ModuleManifest::builder(self.module_id, "0.0.0-harness-stub")
            .provides(vec![ProviderRole::ToolProvider {
                tools,
                identity_scope: vec![],
                concurrency: Concurrency::ModuleManaged,
                emits_push: false,
                sub_supervises: false,
            }])
            .build()
    }

    pub fn operations(&self) -> BTreeSet<String> {
        self.operations.as_ref().clone()
    }

    pub fn replies(&self) -> StubReplyTable {
        self.replies.clone()
    }

    pub fn observed_principals(&self) -> Vec<Option<Principal>> {
        self.principals
            .lock()
            .expect("stub principal recorder lock must remain usable")
            .clone()
    }
}

#[async_trait]
impl ModuleHandler for StubRecorder {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        // Claustrum's read surface names the operation in `method`; the older stub
        // requests name it in `op`.
        let operation = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("method")
                    .or_else(|| value.get("op"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let Some(operation) = operation else {
            return HandlerOutcome::Error {
                code: "stub_request_missing_op".to_string(),
                message: format!("{} harness stub requires a string op", self.module_id),
            };
        };
        if !self.operations.contains(&operation) {
            return HandlerOutcome::Error {
                code: "unknown_stub_operation".to_string(),
                message: format!(
                    "{} harness stub does not advertise {operation}",
                    self.module_id
                ),
            };
        }
        match self.replies.reply(&operation) {
            Some(body) => HandlerOutcome::Response(body),
            None => HandlerOutcome::ErrorWithDetail {
                code: "stub_reply_shape_unrecorded".to_string(),
                message: format!(
                    "{} harness stub has no recorded reply shape for {operation}",
                    self.module_id
                ),
                detail: json!({"module_id": self.module_id, "op": operation}),
            },
        }
    }

    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        self.principals
            .lock()
            .expect("stub principal recorder lock must remain usable")
            .push(request.principal.clone());
        match request.principal.as_ref() {
            Some(Principal::Reserved { module_id }) if module_id == "ckbus" => {
                BindDecision::accept()
            }
            _ => BindDecision::reject(
                "harness_stub_principal_refused",
                format!(
                    "{} harness stub requires the daemon-stamped reserved ckbus principal",
                    self.module_id
                ),
            ),
        }
    }
}

pub fn operation_names(entry: &subc_control::CatalogEntry) -> BTreeSet<String> {
    entry
        .roles
        .iter()
        .flat_map(|role| match role {
            ProviderRole::ToolProvider { tools, .. } => tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>(),
            ProviderRole::ManagementSurface { operations, .. } => operations
                .iter()
                .map(|operation| operation.name.clone())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::StubReplyTable;
    use serde_json::json;

    #[test]
    fn reply_shape_registration_seam_round_trips_recorded_bytes() {
        let table = StubReplyTable::default();
        table.record_json("credential.get", json!({"fixture": "shape"}));
        let reply = table
            .reply("credential.get")
            .expect("observable recorded stub reply must be retrievable");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reply).unwrap(),
            json!({"fixture": "shape"}),
            "observable stub reply table must preserve the recorded JSON shape"
        );
    }
}
