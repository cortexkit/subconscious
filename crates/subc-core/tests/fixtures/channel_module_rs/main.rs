#![forbid(unsafe_code)]

use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use serde_json::json;
use sha2::{Digest, Sha256};
use subc_client_rs::{
    async_trait,
    config::{ConfigDeclareRequest, ConfigReviewReply, ConfigReviewRequest, ConfigState},
    BindDecision, CallOptions, ConnectionEnd, HandlerOutcome, ModuleHandle, ModuleHandler,
    RequestCtx, RouteBindRequest, ScopeCallError, ScopeSyncReply, ServeOptions, SubcConsumer,
};
use subc_protocol::{
    manifest::{Concurrency, ModuleManifest, ProviderRole},
    BindIdentity, ModuleHelloAckBody, RouteTarget,
};

struct Module {
    root: PathBuf,
    id: String,
    upgrade_fixture: Option<String>,
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

    fn e2e_carried(&self) -> bool {
        self.upgrade_fixture.as_deref() == Some("e2e-carry")
    }

    async fn attest_principal(&self, handle: &ModuleHandle, phase: &str) {
        let consumer = handle
            .consumer_local(
                &self.root.join("runtime/subc-connection.json"),
                Default::default(),
            )
            .await
            .unwrap();
        let payload = phase.as_bytes();
        let reply = consumer
            .call(
                RouteTarget::ToolProvider {
                    module_id: self.id.clone(),
                },
                BindIdentity::new(phase, "live-upgrade"),
                payload.to_vec(),
                CallOptions {
                    timeout: Duration::from_secs(5),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(reply, payload);
        self.event(json!({"event":phase}));
    }
}

#[async_trait]
impl ModuleHandler for Module {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        if self.e2e_carried() && body == b"original-generation payload" {
            print!("upgrade output crosses ");
            io::stdout().flush().unwrap();
            eprintln!("upgrade stderr before exec");
            self.event(json!({"event":"entered_barrier"}));
            while !self.root.join("release-handler").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(!ctx.cancellation_token().is_cancelled());
            println!("the exec");
            println!("upgrade original-generation handler completed");
            self.event(json!({"event":"finished_barrier"}));
        }
        HandlerOutcome::Response(body)
    }

    async fn on_hello_ack(&self, ack: &ModuleHelloAckBody) {
        if self.e2e_carried() {
            let hash = ack
                .resume_token
                .as_ref()
                .map(|token| format!("{:x}", Sha256::digest(token)));
            self.event(json!({"event":"hello_ack", "pid":std::process::id(), "token_sha256":hash}));
        }
    }

    async fn on_reconnected(
        &self,
        handle: &ModuleHandle,
        _: Option<Result<ScopeSyncReply, ScopeCallError>>,
    ) {
        if self.e2e_carried() {
            self.attest_principal(handle, "reconnected").await;
        }
    }

    async fn on_connection_end(&self, end: ConnectionEnd) {
        if self.e2e_carried() || self.upgrade_fixture.as_deref() == Some("slow-stop") {
            self.event(
                json!({"event":"connection_end", "reason":end.as_str(), "pid":std::process::id()}),
            );
        }
        if self.upgrade_fixture.as_deref() == Some("slow-stop") {
            std::future::pending::<()>().await;
        }
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
    let upgrade_fixture = std::env::var("CHANNEL_MODULE_UPGRADE_FIXTURE").ok();
    let live_upgrade = matches!(upgrade_fixture.as_deref(), Some("carry" | "e2e-carry"));
    let (handle, serving) = ServeOptions::default()
        .with_live_upgrade(live_upgrade)
        .serve_from_env_with_handle(
            manifest,
            Module {
                root: root.clone(),
                id: id.clone(),
                upgrade_fixture: upgrade_fixture.clone(),
            },
        )
        .await
        .unwrap()
        .unwrap();
    if upgrade_fixture.is_some() {
        fs::write(
            root.join(format!("{id}.registered")),
            std::process::id().to_string(),
        )
        .unwrap();
    }
    let task = tokio::spawn(serving);
    if upgrade_fixture.as_deref() == Some("e2e-carry") {
        Module {
            root: root.clone(),
            id: id.clone(),
            upgrade_fixture,
        }
        .attest_principal(&handle, "connected")
        .await;
    }
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
