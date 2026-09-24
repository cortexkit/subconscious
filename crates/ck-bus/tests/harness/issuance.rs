//! The issuance rows' harness: a supervised participant, a relay to reach ck-bus AS that
//! participant, and a broker client that records the server's permission verdicts.
//!
//! The participant is this row's own test executable, declared as a subc module named
//! `participant` through the per-run config and started by `supervisor.rescan`, so it
//! is spawned by the daemon with a launch nonce and a spawn generation like any
//! production participant. The fixture file is never edited. `/bin/sh` wraps it only to
//! move the daemon's appended `--subc <file>` out of the test harness's argv, which
//! would refuse an unknown flag.
//!
//! Inside the child, the participant registers (HELLO with its nonce), then drops the
//! nonce from its environment and keeps it in memory. A relayed call with
//! `attest: true` opens its route with `ConsumerIdentity`, and one with `attest: false`
//! opens it with none, so the same child reaches ck-bus once attested and once as
//! `Direct`.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::{json, Value};
use subc_client_rs::{
    consumer::{CallError, CallOptions, ConsumerOptions, SubcConsumer},
    ConsumerIdentity, HandlerOutcome, ModuleHandler, RequestCtx,
};
use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_protocol::{
    manifest::{
        Concurrency, ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
    },
    BindIdentity, RouteTarget, SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV,
};

use super::{control, signer::run::SignerRun};

pub const PARTICIPANT: &str = "participant";
/// The row test each row file declares to run as the participant child.
pub const CHILD_TEST: &str = "participant_child";
const CHILD_ARGV_ENV: &str = "CKBUS_PARTICIPANT_ARGV";
const RELAY_OP: &str = "participant.relay";

/// Declares the participant in the run's config and has the daemon start it.
pub async fn register_participant(run: &SignerRun) {
    let exe = std::env::current_exe().expect("the row's own executable");
    let mut value: Value =
        serde_json::from_slice(&fs::read(&run.config_file).expect("rendered config"))
            .expect("rendered config is JSON");
    value["modules"][PARTICIPANT] = json!({
        "program": "/bin/sh",
        "args": [
            "-c",
            format!("{CHILD_ARGV_ENV}=\"$*\" exec \"$0\" --exact {CHILD_TEST} --nocapture --test-threads=1"),
            exe.display().to_string(),
        ],
        "enabled": true,
        "protocol": "subc",
        "drain_timeout_ms": 2000,
    });
    fs::write(
        &run.config_file,
        serde_json::to_vec_pretty(&value).expect("config encodes"),
    )
    .expect("rendered config writable");
    let reply = control::rpc(
        &run.connection_file,
        ClientControlRequest::SupervisorRescan { preview: false },
    )
    .await;
    if let control::ControlReply::Error(error) = reply {
        panic!(
            "supervisor.rescan refused: {} {}",
            error.code, error.message
        );
    }
    run.wait_for_catalog_id(PARTICIPANT).await;
}

/// The live spawn generation of `module_id`, read from `supervisor.spawn_snapshot`.
pub async fn live_generation(connection_file: &Path, module_id: &str) -> Option<u64> {
    let response = control::response(
        connection_file,
        ClientControlRequest::SupervisorSpawnSnapshot {},
    )
    .await;
    let ClientControlResponse::SupervisorSpawnSnapshot { snapshot } = response else {
        panic!("supervisor.spawn_snapshot must return its matching response variant");
    };
    snapshot
        .live
        .iter()
        .filter(|spawn| spawn.module_id == module_id)
        .map(|spawn| spawn.spawn_generation)
        .max()
}

/// Restarts the participant and waits until it is registered under a higher generation.
pub async fn respawn_participant(run: &SignerRun) -> u64 {
    let before = live_generation(&run.connection_file, PARTICIPANT)
        .await
        .expect("the participant is live before its respawn");
    let reply = control::rpc(
        &run.connection_file,
        ClientControlRequest::SupervisorRestart {
            module_id: PARTICIPANT.to_string(),
            drain_timeout_ms: None,
        },
    )
    .await;
    if let control::ControlReply::Error(error) = reply {
        panic!(
            "supervisor.restart participant refused: {} {}",
            error.code, error.message
        );
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(generation) = live_generation(&run.connection_file, PARTICIPANT).await {
            if generation > before {
                run.wait_for_catalog_id(PARTICIPANT).await;
                // The catalog can still show the previous registration for a moment; a
                // relay that answers proves the new process serves.
                wait_relay_ready(&run.connection_file).await;
                return generation;
            }
        }
        assert!(Instant::now() < deadline, "the participant never respawned");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_relay_ready(connection_file: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let reply = relay_raw(connection_file, json!({"ping": true})).await;
        if reply.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "participant relay never answered: {reply:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// What ck-bus answered a relayed call: its `result`, or its Error frame's code and
/// message.
pub type CkbusReply = Result<Value, (String, String)>;

/// Has the participant call ck-bus's `method` with `params`, attested or `Direct`.
pub async fn relay(
    connection_file: &Path,
    attest: bool,
    method: &str,
    params: Value,
) -> CkbusReply {
    let reply = relay_raw(
        connection_file,
        json!({"attest": attest, "body": {"method": method, "params": params}}),
    )
    .await
    .unwrap_or_else(|error| panic!("the participant relay failed: {error}"));
    if let Some(result) = reply.get("ok") {
        return Ok(result["result"].clone());
    }
    let error = &reply["error"];
    Err((
        error["code"].as_str().unwrap_or_default().to_string(),
        error["message"].as_str().unwrap_or_default().to_string(),
    ))
}

async fn relay_raw(connection_file: &Path, params: Value) -> Result<Value, String> {
    let consumer = SubcConsumer::connect(connection_file, ConsumerOptions::default())
        .await
        .map_err(|error| error.to_string())?;
    let body = serde_json::to_vec(&json!({"method": RELAY_OP, "params": params})).unwrap();
    let reply = consumer
        .call(
            RouteTarget::ManagementSurface {
                module_id: PARTICIPANT.to_string(),
            },
            BindIdentity::new(
                connection_file
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_default(),
                "ck-bus-issuance-rows",
                "relay",
            ),
            body,
            CallOptions {
                timeout: Duration::from_secs(20),
                ..CallOptions::default()
            },
        )
        .await
        .map_err(|error| error.to_string());
    consumer.close().await;
    let reply = reply?;
    serde_json::from_slice(&reply).map_err(|error| error.to_string())
}

/// The participant's side. Returns at once unless this process was started by the
/// daemon as the participant.
pub fn participant_child_entry() {
    let Ok(argv) = std::env::var(CHILD_ARGV_ENV) else {
        return;
    };
    let mut words = argv.split_whitespace();
    let mut connection_file = None;
    while let Some(word) = words.next() {
        if word == "--subc" {
            connection_file = words.next().map(PathBuf::from);
        } else if let Some(path) = word.strip_prefix("--subc=") {
            connection_file = Some(PathBuf::from(path));
        }
    }
    let connection_file = connection_file.expect("the daemon passes --subc to the participant");
    let module_id = std::env::var(SUBC_MODULE_ID_ENV).expect("the daemon names the participant");
    let launch_nonce =
        std::env::var(SUBC_LAUNCH_NONCE_ENV).expect("the daemon gives the participant a nonce");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("participant runtime");
    runtime.block_on(async move {
        let consumer = SubcConsumer::connect(&connection_file, ConsumerOptions::default())
            .await
            .expect("participant reaches the daemon as a client");
        let handler = Relay {
            consumer,
            identity: ConsumerIdentity {
                module_id: module_id.clone(),
                launch_nonce,
            },
            bind_dir: connection_file
                .parent()
                .map(PathBuf::from)
                .unwrap_or_default(),
        };
        let (_handle, serving) = subc_client_rs::serve_with_handle(
            &connection_file,
            relay_manifest(&module_id),
            handler,
        )
        .await
        .expect("participant registers");
        // Registered: from here on the nonce lives only in memory, so a call without an
        // explicit identity carries none and arrives as Direct.
        std::env::remove_var(SUBC_LAUNCH_NONCE_ENV);
        std::env::remove_var(SUBC_MODULE_ID_ENV);
        let _ = serving.await;
    });
}

fn relay_manifest(module_id: &str) -> ModuleManifest {
    ModuleManifest::builder(module_id, "0.0.0-issuance-participant")
        .provides(vec![ProviderRole::ManagementSurface {
            operations: vec![ManagementOperation {
                name: RELAY_OP.to_string(),
                kind: ManagementOperationKind::Mutate,
                description: Some("relays one call to ck-bus as this participant".to_string()),
            }],
            config_schema: json!({}),
            observability: vec![],
            identity_scope: vec![],
            concurrency: Concurrency::ModuleManaged,
        }])
        .build()
}

struct Relay {
    consumer: SubcConsumer,
    identity: ConsumerIdentity,
    bind_dir: PathBuf,
}

#[async_trait]
impl ModuleHandler for Relay {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let params = &request["params"];
        if params.get("ping").is_some() {
            return HandlerOutcome::Response(b"{\"pong\":true}".to_vec());
        }
        let attest = params["attest"].as_bool().unwrap_or(false);
        let options = CallOptions {
            consumer_identity: attest.then(|| self.identity.clone()),
            timeout: Duration::from_secs(15),
            ..CallOptions::default()
        };
        let reply = self
            .consumer
            .call(
                RouteTarget::ManagementSurface {
                    module_id: "ckbus".to_string(),
                },
                BindIdentity::new(
                    self.bind_dir.clone(),
                    "participant",
                    if attest { "attested" } else { "direct" },
                ),
                serde_json::to_vec(&params["body"]).unwrap(),
                options,
            )
            .await;
        let answer = match reply {
            Ok(bytes) => {
                json!({"ok": serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null)})
            }
            Err(CallError::Module(body)) => {
                json!({"error": {"code": body.code, "message": body.message}})
            }
            Err(error) => json!({"error": {
                "code": error.code().unwrap_or("relay_call_failed"),
                "message": error.to_string(),
            }}),
        };
        HandlerOutcome::Response(serde_json::to_vec(&answer).unwrap())
    }
}

/// A broker client whose server-reported errors (a permissions violation above all) are
/// recorded, so an arm can tell a server-side `Denied` from a silent drop.
pub struct VerdictClient {
    pub client: async_nats::Client,
    events: Arc<Mutex<Vec<String>>>,
}

impl VerdictClient {
    /// Connects with `jwt`, answering the nonce through `sign`. `inbox_prefix` sets the
    /// client's reply inbox; `None` leaves the library's default `_INBOX`.
    pub async fn connect(
        url: &str,
        jwt: &str,
        sign: Arc<
            dyn Fn(Vec<u8>) -> futures_util::future::BoxFuture<'static, Result<Vec<u8>, String>>
                + Send
                + Sync,
        >,
        inbox_prefix: Option<String>,
    ) -> Result<Self, async_nats::ConnectError> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let mut options = async_nats::ConnectOptions::with_jwt(jwt.to_string(), move |nonce| {
            // The library needs a `Sync` future; the signing future runs as its own task
            // and only the task handle is awaited here.
            let signing = tokio::spawn(sign(nonce.to_vec()));
            async move {
                signing
                    .await
                    .map_err(|error| async_nats::AuthError::new(error.to_string()))?
                    .map_err(async_nats::AuthError::new)
            }
        })
        .event_callback(move |event| {
            let recorded = recorded.clone();
            async move {
                recorded.lock().unwrap().push(event.to_string());
            }
        })
        .connection_timeout(Duration::from_secs(5))
        .request_timeout(Some(Duration::from_secs(3)));
        if let Some(prefix) = inbox_prefix {
            options = options.custom_inbox_prefix(prefix);
        }
        let client = options.connect(url).await?;
        Ok(Self { client, events })
    }

    /// Every server-reported permissions violation naming `subject`.
    pub fn violations_for(&self, subject: &str) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| {
                event.to_ascii_lowercase().contains("permissions violation")
                    && event.contains(&format!("\"{subject}\""))
            })
            .cloned()
            .collect()
    }

    /// Waits for the server's permissions violation naming `subject`.
    pub async fn expect_denied(&self, subject: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.violations_for(subject).is_empty() {
            assert!(
                Instant::now() < deadline,
                "the server never denied {subject}; events: {:?}",
                self.events.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Asserts that no permissions violation named `subject` arrived. The server sends a
    /// violation right after it reads the offending message, so the arm flushes and then
    /// allows it a fixed 250 ms to arrive. This is the weaker half of an allowed check;
    /// every arm also observes the allowed act's effect.
    pub async fn expect_allowed(&self, subject: &str) {
        self.client.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            self.violations_for(subject).is_empty(),
            "the server denied {subject}: {:?}",
            self.violations_for(subject)
        );
    }

    /// Publishes to `subject` and flushes.
    pub async fn publish(&self, subject: &str, payload: &'static [u8]) {
        self.client
            .publish(subject.to_string(), payload.into())
            .await
            .expect("publish queued");
        self.client.flush().await.expect("flush");
    }
}
