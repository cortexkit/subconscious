//! Tiny supervised publisher used by the private-rig integration tests.
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use subc_client_rs::{
    CallOptions, ConsumerIdentity, ConsumerOptions, HandlerOutcome, ModuleHandler, RequestCtx,
    SubcConsumer,
};
use subc_protocol::{manifest::ModuleManifest, BindIdentity, RouteTarget};

const LIMIT: Duration = Duration::from_secs(30);
async fn wait_file(path: &Path) {
    let deadline = Instant::now() + LIMIT;
    while !path.is_file() {
        assert!(Instant::now() < deadline, "publish signal never appeared");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Idle;
#[async_trait]
impl ModuleHandler for Idle {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(b"{}".to_vec())
    }

    /// Record the storage descriptor the daemon allocated for this module, so a
    /// test can check the private daemon serves one under its own data home.
    async fn on_hello_ack(&self, ack: &subc_protocol::ModuleHelloAckBody) {
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
        std::fs::write(
            runtime.join("module-storage.json"),
            serde_json::to_vec(&ack.storage).unwrap(),
        )
        .unwrap();
    }
}

async fn ckbus_call(
    consumer: &SubcConsumer,
    identity: &ConsumerIdentity,
    _connection: &Path,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let body = consumer
        .call(
            RouteTarget::ManagementSurface {
                module_id: "ckbus".into(),
            },
            BindIdentity::new("testkit", "publisher"),
            serde_json::to_vec(&json!({"method": method, "params": params})).unwrap(),
            CallOptions {
                timeout: Duration::from_secs(5),
                consumer_identity: Some(identity.clone()),
                ..Default::default()
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    let value: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
    Ok(value["result"].clone())
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let connection = loop {
        match args.next() {
            Some(arg) if arg == "--subc" => break PathBuf::from(args.next().expect("--subc path")),
            Some(_) => continue,
            None => panic!("the daemon passes --subc to its modules"),
        }
    };
    let identity = ConsumerIdentity {
        module_id: std::env::var("SUBC_MODULE_ID").unwrap(),
        launch_nonce: subc_client_rs::launch_nonce()
            .unwrap()
            .unwrap()
            .value()
            .to_string(),
    };
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async move {
        let consumer = Arc::new(SubcConsumer::connect(&connection, ConsumerOptions::default()).await.unwrap());
        let (_handle, serving) = subc_client_rs::serve_with_handle(&connection,
            ModuleManifest::builder(identity.module_id.clone(), "0.0.0-test")
                    .provides(vec![subc_protocol::manifest::ProviderRole::ManagementSurface {
                        operations: vec![subc_protocol::manifest::ManagementOperation {
                            name: "probe.ping".into(),
                            kind: subc_protocol::manifest::ManagementOperationKind::Query,
                            description: None,
                        }],
                        config_schema: json!({}), observability: vec![], identity_scope: vec![],
                        concurrency: subc_protocol::manifest::Concurrency::ModuleManaged,
                    }]).build(), Idle).await.unwrap();
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
        std::fs::write(runtime.join("module-homes.json"), serde_json::to_vec(&json!({
                "data": std::env::var("XDG_DATA_HOME").unwrap(),
                "pid": std::process::id(),
            "runtime": runtime, "config": std::env::var("XDG_CONFIG_HOME").unwrap(),
        })).unwrap()).unwrap();
        if let Some(result) = std::env::var_os("TESTKIT_RESULT").map(PathBuf::from) {
            let deadline = Instant::now() + LIMIT;
            let credential = loop {
                if let Ok(answer) = ckbus_call(&consumer, &identity, &connection, "ckbus.credential", json!({})).await {
                    if answer["jwt"].is_string() { break answer; }
                }
                assert!(Instant::now() < deadline, "ckbus never issued a credential");
                tokio::time::sleep(Duration::from_millis(100)).await;
            };
            std::fs::write(result.join("credential.json"), serde_json::to_vec(&credential).unwrap()).unwrap();
            wait_file(&result.join("publish-now")).await;
            let public = credential["credential_public"].as_str().unwrap().to_string();
            let client = async_nats::ConnectOptions::with_jwt(credential["jwt"].as_str().unwrap().to_string(), move |nonce| {
                let consumer = consumer.clone(); let identity = identity.clone(); let connection = connection.clone(); let public = public.clone();
                let task = tokio::spawn(async move {
                    let answer = ckbus_call(&consumer, &identity, &connection, "ckbus.nonce_sign",
                        json!({"nonce_b64": STANDARD.encode(nonce), "credential_public": public})).await?;
                    STANDARD.decode(answer["signature_b64"].as_str().unwrap()).map_err(|error| error.to_string())
                });
                async move { task.await.unwrap().map_err(async_nats::AuthError::new) }
            }).connection_timeout(Duration::from_secs(5))
                .connect(credential["server_url"].as_str().unwrap()).await.unwrap();
            client.publish(format!("ck.{}.event.testkit_probe.smoke.v1", credential["acct"].as_str().unwrap()), "private smoke".into()).await.unwrap();
            client.flush().await.unwrap();
        }
        serving.await.unwrap();
    });
}
