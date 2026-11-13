use std::{fs, io::Write, path::PathBuf, time::Duration};

use serde_json::json;
use subc_client_rs::{
    async_trait,
    config::{ConfigDeclareRequest, ConfigReviewReply, ConfigReviewRequest, ConfigState},
    serve_from_env_with_handle, BindDecision, CallOptions, HandlerOutcome, ModuleHandler,
    RequestCtx, RouteBindRequest, SubcConsumer,
};
use subc_protocol::{
    manifest::{Concurrency, ModuleManifest, ProviderRole},
    BindIdentity, RouteTarget,
};

struct Module {
    root: PathBuf,
    id: String,
}

impl Module {
    fn event(&self, value: serde_json::Value) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join(format!("{}.events", self.id)))
            .unwrap();
        writeln!(file, "{value}").unwrap();
    }
}

#[async_trait]
impl ModuleHandler for Module {
    async fn handle(&self, _: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }

    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        self.event(
            json!({"event":"bind", "principal":request.principal, "identity":request.identity}),
        );
        BindDecision::accept()
    }

    fn config_declaration(&self) -> Option<ConfigDeclareRequest> {
        Some(ConfigDeclareRequest::new(
            json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "properties":{}, "unevaluatedProperties":false}),
            self.root
                .join(format!("config/cortexkit/{}.jsonc", self.id))
                .to_str()
                .unwrap(),
            vec![],
        ))
    }

    async fn config_review(&self, _: ConfigReviewRequest) -> ConfigReviewReply {
        self.event(json!({"event":"review"}));
        ConfigReviewReply::new(vec![], vec![])
    }

    async fn config_apply(&self, _: subc_client_rs::config::ConfigView) -> Result<(), Vec<String>> {
        Ok(())
    }

    async fn on_config_state(&self, state: ConfigState) {
        self.event(json!({"event":"config", "state":state.as_str()}));
    }
}

fn main() {
    // The control deliberately models an older SDK that registers with its
    // nonce even though the daemon also supplied a registration channel.
    if std::env::var_os("CHANNEL_MODULE_LEGACY").is_some() {
        std::env::remove_var("SUBC_REGISTRATION_FD");
        std::env::remove_var("SUBC_REGISTRATION_PIPE");
    }
    tokio::runtime::Runtime::new().unwrap().block_on(run());
}

async fn run() {
    let root = PathBuf::from(std::env::var_os("CHANNEL_MODULE_ROOT").unwrap());
    let id = std::env::var("SUBC_MODULE_ID").unwrap();
    if id == "caller" {
        if let Some(nonce) = subc_client_rs::launch_nonce().unwrap() {
            fs::write(root.join("caller.nonce"), nonce.value()).unwrap();
        }
    }
    let manifest = ModuleManifest::builder(&id, "1.0.0")
        .provides(vec![ProviderRole::ToolProvider {
            tools: vec![],
            identity_scope: vec![],
            concurrency: Concurrency::ModuleManaged,
            emits_push: false,
            sub_supervises: false,
        }])
        .build();
    let (handle, serving) = serve_from_env_with_handle(
        manifest,
        Module {
            root: root.clone(),
            id: id.clone(),
        },
    )
    .await
    .unwrap();
    let task = tokio::spawn(serving);
    if id == "caller" {
        while !root.join("go").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let sync = handle.scope_sync(1, vec![]).await;
        let apply = handle.scope_apply(2, vec![], vec![]).await;
        let agents = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let result = handle.agent_commands(vec![], vec![], Some(vec![])).await;
                match result {
                    Err(error) if error.is_retryable() => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    result => break result,
                }
            }
        })
        .await
        .expect("agent command accounting settles");
        let consumer = SubcConsumer::connect(
            &root.join("runtime/subc-connection.json"),
            Default::default(),
        )
        .await
        .unwrap();
        // A supervised SDK consumer mints and presents a self claim before opening routes.
        let result = consumer
            .call(
                RouteTarget::ToolProvider {
                    module_id: "target".into(),
                },
                BindIdentity::new("channel-fixture", "session"),
                b"child-bound roundtrip".to_vec(),
                CallOptions {
                    timeout: Duration::from_secs(5),
                    ..Default::default()
                },
            )
            .await;
        fs::write(
            root.join("result.staging"),
            json!({
                "sync":sync.map(|r| r.generation).map_err(|e| e.to_string()),
                "apply":apply.map(|r| r.generation).map_err(|e| e.to_string()),
                "agents":agents.map_err(|e| e.to_string()),
                "reply":result.map(|b| String::from_utf8(b).unwrap()).map_err(|e| e.to_string()),
            })
            .to_string(),
        )
        .unwrap();
        fs::rename(root.join("result.staging"), root.join("result.json")).unwrap();
    }
    task.await.unwrap().unwrap();
}
