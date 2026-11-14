use std::{fs, sync::Arc, time::Duration};

use cortexkit_test_support::ScratchDir;
use serde_json::{json, Value};
use subc_client_rs::{
    async_trait, BindDecision, CallOptions, ConsumerOptions, HandlerOutcome, ModuleHandler,
    RequestCtx, RouteBindRequest, SubcConsumer,
};
use subc_protocol::{
    manifest::{
        Concurrency, ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
    },
    BindIdentity, Principal, RouteTarget,
};
use tokio::{sync::Mutex, time::timeout};

const ADAPTER: &str = "mcp-stdio-adapter";
const BUDGET: Duration = Duration::from_secs(15);

struct Vault(Arc<Mutex<Vec<Principal>>>);

#[async_trait]
impl ModuleHandler for Vault {
    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        let expected = Principal::Reserved {
            module_id: ADAPTER.into(),
        };
        if request.principal.as_ref() != Some(&expected) {
            return BindDecision::reject(
                "test_vault_requires_adapter",
                "expected attested adapter",
            );
        }
        self.0.lock().await.push(expected);
        BindDecision::accept()
    }

    async fn handle(&self, _: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let request: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            request,
            json!({"method":"credential.get","params":{"handle":"vault:test-token"}})
        );
        HandlerOutcome::Response(serde_json::to_vec(&json!({"payload":"resolved-token"})).unwrap())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vault_consumer_succeeds_after_registration_channel_adoption() {
    let root = ScratchDir::new("adapter-credential-channel");
    for directory in ["config", "data", "run/logs"] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    let registry = root.join("config/mcp-servers.jsonc");
    fs::write(
        &registry,
        serde_json::to_vec(&json!({"fixture":{
            "command":env!("CARGO_BIN_EXE_fake-mcp-child"),
            "env":{"TOKEN":{"handle":"vault:test-token"}}
        }}))
        .unwrap(),
    )
    .unwrap();
    let config = root.join("config/subc.jsonc");
    fs::write(&config, serde_json::to_vec(&json!({"version":1,"modules":{ADAPTER:{
        "program":cortexkit_test_support::ckdev_binary(env!("CARGO_BIN_EXE_ck-mcp-stdio-adapter")),
        "args":["--config",registry], "reserved":true,"registration_channel":true,
        "env":{"XDG_DATA_HOME":root.join("data"),"XDG_CONFIG_HOME":root.join("config"),"XDG_RUNTIME_DIR":root.join("run")}
    }}})).unwrap()).unwrap();
    let connection = root.join("run/subc-connection.json");
    let bootstrap = subc_daemon::bootstrap::BootstrapConfig::new(&connection, 0)
        .with_machine_id_path(root.join("data/machine-id"))
        .with_live_children_record(root.join("run/live-children.json"))
        .with_capture_logs_dir(root.join("run/logs"))
        .with_terminal_journal_path(root.join("run/terminals.jsonl"))
        .with_daemon_config_path(&config)
        .unwrap();
    #[cfg(target_os = "macos")]
    let bootstrap = bootstrap.with_privacy_trampoline(cortexkit_test_support::ckdev_binary(
        std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("ck-subc"),
    ));
    let daemon = tokio::spawn(subc_daemon::bootstrap::run_with_config(bootstrap));
    let client = timeout(BUDGET, async {
        loop {
            if let Ok(client) = SubcConsumer::connect(&connection, ConsumerOptions::default()).await
            {
                break client;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let principals = Arc::new(Mutex::new(Vec::new()));
    let manifest = ModuleManifest::builder("claustrum", "1.0.0")
        .provides(vec![ProviderRole::ManagementSurface {
            operations: vec![ManagementOperation {
                name: "credential.get".into(),
                kind: ManagementOperationKind::Query,
                description: None,
            }],
            config_schema: json!({}),
            observability: vec![],
            identity_scope: vec![],
            concurrency: Concurrency::ModuleManaged,
        }])
        .build();
    let (_, serving) =
        subc_client_rs::serve_with_handle(&connection, manifest, Vault(principals.clone()))
            .await
            .unwrap();
    let vault = tokio::spawn(serving);
    let outcome = timeout(
        BUDGET,
        client.call(
            RouteTarget::ManagementSurface {
                module_id: ADAPTER.into(),
            },
            BindIdentity::new("adapter-test", "credential-channel"),
            serde_json::to_vec(&json!({
                "server":"fixture", "op":"tools/call",
                "payload":{"method":"tools/call","params":{"check":"credential"}}
            }))
            .unwrap(),
            CallOptions {
                timeout: BUDGET,
                ..CallOptions::default()
            },
        ),
    )
    .await;
    let stopped = client.supervisor_set_enabled(ADAPTER, false).await;
    client.close().await;
    vault.abort();
    daemon.abort();
    let _ = vault.await;
    let _ = daemon.await;
    stopped.unwrap();
    let reply: Value = serde_json::from_slice(
        &outcome
            .expect("adapter call deadline")
            .expect("adapter call succeeds"),
    )
    .unwrap();
    assert_eq!(
        reply["payload"]["environment"]["TOKEN"], "resolved-token",
        "{reply}"
    );
    assert_eq!(
        *principals.lock().await,
        vec![Principal::Reserved {
            module_id: ADAPTER.into()
        }]
    );
}
