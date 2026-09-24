//! Ladder row "Signer outage and restart" (slice 11 of `docs/specs/ck-bus-module.md`),
//! against the acceptance daemon supervising BOTH the `ck-bus` binary and a real
//! `nats-server` as siblings, as production declares them.
//!
//! Serving side: harness-signer. Every JWT, account JWT and revocation list ck-bus signs
//! in these arms is signed by the harness signer's fixture keys. The vault recovery arm
//! is served by harness-stub (the refusing phase) and harness-signer (once it answers).
//!
//! "Signer" in the row's name is ck-bus itself: it is the only process that can sign a
//! participant's connect nonce (`ckbus.nonce_sign`), so while it is down nobody can
//! connect or reconnect, and a restarted ck-bus holds none of the seeds its previous
//! process issued.
//!
//! Arms (the row's text):
//! - ck-bus killed (SIGKILL) under the supervisor: nats-server's pid from provenance is
//!   unchanged, a connected participant keeps publishing and receiving, and a new
//!   participant's connect fails until ck-bus is back, then succeeds.
//! - Twin: nats-server restarted with ck-bus up: every participant re-signs its nonce
//!   through `ckbus.nonce_sign` and reconnects, with no refetch.
//! - ck-bus killed and restarted: established connections survive. A participant forced
//!   to reconnect gets `ckbus_credential_superseded`, refetches, reconnects at the next
//!   epoch, and its superseded user is revoked. A participant never forced to reconnect
//!   stays connected and unrevoked.
//! - Control: with ck-bus down and nats-server restarted, every participant is
//!   disconnected and stays so until ck-bus returns (and then reconnects only after a
//!   refetch). nats-server's parent is the daemon's process, never ck-bus.
//! - The expiry arm records `user-jwt-ttl-unpinned`, naming the observed JWT without
//!   `exp`.
//!
//! One arm beyond the row's text: the VAULT (Claustrum, the signer ck-bus itself signs
//! through) refusing and then answering again. The sentinel, health and bootstrap rows
//! show ck-bus staying up and reporting a refusing vault, but none shows it recovering;
//! this row is the one about signer outages, so the recovery is proven here: ck-bus
//! bootstraps on its next per-period retry, with no restart.
//!
//! The kill arms need a window in which ck-bus is dead and not yet replaced. The
//! supervisor replaces a crashed module after its restart backoff, so this row declares
//! ck-bus's backoff as `CKBUS_BACKOFF_MS`, and every "during the outage" observation is
//! checked against the daemon's record of when it spawned the replacement.

#[allow(dead_code)]
#[path = "../src/bootstrap/mod.rs"]
mod bootstrap;
#[allow(dead_code)]
#[path = "../src/credentials/mod.rs"]
mod credentials;
#[allow(dead_code)]
#[path = "../src/grants/mod.rs"]
mod grants;
#[allow(dead_code)]
mod harness;
#[allow(dead_code)]
#[path = "../src/issuance/mod.rs"]
mod issuance;
#[allow(dead_code)]
#[path = "../src/revocation/mod.rs"]
mod revocation;
#[allow(dead_code)]
#[path = "../src/runtime/seams.rs"]
mod runtime;
#[allow(dead_code)]
#[path = "../src/spawn_consumer/mod.rs"]
mod spawn_consumer;

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use cortexkit_bus_naming::AccountNames;
use futures_util::StreamExt;
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    config::{self, SentinelTiming},
    control, data_home,
    issuance::{self as rows, VerdictClient, PARTICIPANT},
    report::{Row, RowReport, ServedBy},
    sentinel,
    signer::{
        nats::{nats_server_bin, unix_now},
        run::SignerRun,
        HarnessSigner, SIGNER_OPERATIONS,
    },
    stubs::{StubRecorder, CALLOSUM_OPERATIONS, CLAUSTRUM_OPERATIONS},
};
use nkeys::KeyPair;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_client_rs::{
    consumer::{CallOptions, ConsumerOptions, SubcConsumer},
    BindDecision, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest,
};
use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_daemon::{
    bootstrap::{run_with_config, BootstrapConfig},
    test_support::TestTempDir,
};
use subc_protocol::{BindIdentity, RouteTarget};
use tokio::task::JoinHandle;

const BOOT_LIMIT: Duration = Duration::from_secs(60);
/// The supervisor's delay before it replaces a crashed ck-bus, declared in ck-bus's
/// `restart` block for the kill arm. A participant's call to an absent ck-bus is not
/// refused at once: the client library retries the route open until the call's deadline,
/// which is 15 s in the harness participant's relay. The outage must outlast that, or a
/// connect begun during it would simply wait for the replacement and succeed, and the
/// arm could not show a refused connect. The arm checks the outage it got against the
/// daemon's record of when it spawned the replacement rather than trusting this number.
const CKBUS_BACKOFF_MS: u64 = 20_000;
/// The second participant module: the "new" participant of the kill arm, and the one
/// never forced to reconnect in the restart arm.
const BYSTANDER: &str = "bystander";
/// The environment variable `harness::issuance` hands the participant child its argv
/// through. The harness keeps its constant private, so the value is repeated here; the
/// child entry reads it, and a mismatch would leave the bystander never registering.
const PARTICIPANT_ARGV_ENV: &str = "CKBUS_PARTICIPANT_ARGV";
/// The subject a participant receives on: under its own inbox prefix, which its grant
/// lets it subscribe to.
const INBOX_LEAF: &str = "outage";
/// How long one publish/receive exchange may take on a healthy connection.
const EXCHANGE_LIMIT: Duration = Duration::from_secs(5);

/// Runs only when the daemon starts this executable as a participant.
#[test]
fn participant_child() {
    rows::participant_child_entry();
}

fn vocabulary() -> BTreeSet<String> {
    SIGNER_OPERATIONS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

fn passed() {
    RowReport::passed(Row::SignerOutage)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&vocabulary());
}

fn fresh_machine_id() -> String {
    let seed = format!("{:?}{}", SystemTime::now(), std::process::id());
    Sha256::digest(seed.as_bytes())[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn at_ms(line: &Value) -> u64 {
    line["at_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("a ck-bus log line carries at_ms: {line}"))
}

/// One nonce signature a participant's connection asked ck-bus for: when, and the
/// refusal code when it was refused.
#[derive(Debug, Clone)]
struct Signed {
    at_ms: u64,
    outcome: Result<(), String>,
}

type SignLog = Arc<Mutex<Vec<Signed>>>;

/// Has participant `module_id` call ck-bus's `method` as itself (attested). The outer
/// error is the relay itself failing, which is a harness fault; the inner one is ck-bus's
/// answer, or the daemon's refusal to route to ck-bus, as `(code, message)`.
async fn relay_to(
    connection_file: &Path,
    module_id: &str,
    method: &str,
    params: Value,
) -> Result<rows::CkbusReply, String> {
    let consumer = SubcConsumer::connect(connection_file, ConsumerOptions::default())
        .await
        .map_err(|error| error.to_string())?;
    let body = json!({
        "method": "participant.relay",
        "params": {"attest": true, "body": {"method": method, "params": params}},
    });
    let reply = consumer
        .call(
            RouteTarget::ManagementSurface {
                module_id: module_id.to_string(),
            },
            BindIdentity::new(
                connection_file
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_default(),
                "ck-bus-outage-row",
                "relay",
            ),
            serde_json::to_vec(&body).unwrap(),
            CallOptions {
                timeout: Duration::from_secs(20),
                ..CallOptions::default()
            },
        )
        .await
        .map_err(|error| error.to_string());
    consumer.close().await;
    let reply: Value = serde_json::from_slice(&reply?).map_err(|error| error.to_string())?;
    if let Some(ok) = reply.get("ok") {
        return Ok(Ok(ok["result"].clone()));
    }
    let error = &reply["error"];
    Ok(Err((
        error["code"].as_str().unwrap_or_default().to_string(),
        error["message"].as_str().unwrap_or_default().to_string(),
    )))
}

/// A nonce signer that relays every connect nonce to `ckbus.nonce_sign` as `module_id`
/// and records each outcome, so an arm can see what a reconnect was answered.
fn recording_signer(
    connection_file: PathBuf,
    module_id: &'static str,
    credential_public: String,
    log: SignLog,
) -> rows::NonceSigner {
    Arc::new(move |nonce: Vec<u8>| {
        let connection_file = connection_file.clone();
        let credential_public = credential_public.clone();
        let log = log.clone();
        Box::pin(async move {
            let reply = relay_to(
                &connection_file,
                module_id,
                issuance::NONCE_SIGN_OP,
                json!({"nonce_b64": STANDARD.encode(&nonce), "credential_public": credential_public}),
            )
            .await;
            let signed = match reply {
                Ok(Ok(result)) => STANDARD
                    .decode(result["signature_b64"].as_str().unwrap_or_default())
                    .map_err(|error| format!("undecodable signature: {error}")),
                Ok(Err((code, message))) => Err(format!("{code}: {message}")),
                Err(relay) => Err(format!("relay_failed: {relay}")),
            };
            log.lock().unwrap().push(Signed {
                at_ms: now_ms(),
                outcome: signed.as_ref().map(|_| ()).map_err(Clone::clone),
            });
            signed
        })
    })
}

/// A participant connected with a credential ck-bus issued it.
struct Participant {
    module_id: &'static str,
    answer: Value,
    client: VerdictClient,
    inbox: async_nats::Subscriber,
    signs: SignLog,
}

impl Participant {
    fn public(&self) -> String {
        self.answer["credential_public"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn epoch(&self) -> u64 {
        self.answer["credential_epoch"].as_u64().unwrap()
    }

    fn connected(&self) -> bool {
        self.client.client.connection_state() == async_nats::connection::State::Connected
    }

    fn disconnects(&self) -> usize {
        self.client
            .events()
            .iter()
            .filter(|event| *event == "disconnected")
            .count()
    }

    /// Nonce signatures refused at or after `since_ms`.
    fn refusals_since(&self, since_ms: u64) -> Vec<String> {
        self.signs
            .lock()
            .unwrap()
            .iter()
            .filter(|signed| signed.at_ms >= since_ms)
            .filter_map(|signed| signed.outcome.clone().err())
            .collect()
    }

    /// Nonce signatures answered at or after `since_ms`.
    fn signatures_since(&self, since_ms: u64) -> usize {
        self.signs
            .lock()
            .unwrap()
            .iter()
            .filter(|signed| signed.at_ms >= since_ms && signed.outcome.is_ok())
            .count()
    }
}

/// Removes the supervised nats-server if the arm fails before its own teardown: the
/// daemon runs in this test process, and aborting it does not stop a `protocol: "none"`
/// child.
struct ServerGuard {
    conf: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(self.conf.display().to_string())
            .status();
    }
}

/// The acceptance daemon as `SignerRun` starts it (in this process, capture logs and
/// terminal journal inside the run's tree), except that this row renders the whole
/// config before the daemon reads it and registers its own vault. The daemon applies a
/// module's `restart` policy only when it first supervises the module, so ck-bus's
/// backoff and the supervised nats-server must be in the config at start; `SignerRun`
/// renders its config itself and offers no hook for either.
struct Daemon {
    root: TestTempDir,
    connection_file: PathBuf,
    operator_dir: Option<PathBuf>,
    operator_before: data_home::TreeFingerprint,
    daemon: JoinHandle<Result<(), subc_daemon::bootstrap::BootstrapError>>,
    modules: Vec<JoinHandle<Result<(), subc_client_rs::SubcModuleError>>>,
}

impl Daemon {
    async fn start<V>(root: TestTempDir, config_file: &Path, vault: V) -> Self
    where
        V: ModuleHandler + 'static,
    {
        let operator_dir = data_home::operator_module_dir();
        if let Some(dir) = &operator_dir {
            assert!(
                !dir.starts_with(root.path()),
                "operator data home {} must lie outside the fixture tree",
                dir.display()
            );
        }
        let operator_before = data_home::fingerprint(operator_dir.as_deref());
        let connection_file = root.join("run/subc-connection.json");
        let machine_id_path = root.join("run/machine-id");
        fs::write(&machine_id_path, format!("{}\n", fresh_machine_id())).unwrap();
        let bootstrap = BootstrapConfig::new(&connection_file, 0)
            .with_terminal_journal_path(root.join("run/terminals.jsonl"))
            .with_capture_logs_dir(root.join("run/logs"))
            .with_machine_id_path(machine_id_path)
            .with_daemon_config_path(config_file)
            .expect("fixture daemon config must load");
        let daemon = tokio::spawn(run_with_config(bootstrap));
        control::wait_for_connection(&connection_file, Instant::now() + Duration::from_secs(10))
            .await;
        let callosum = StubRecorder::refusing("callosum", CALLOSUM_OPERATIONS);
        let modules = vec![
            {
                let connection_file = connection_file.clone();
                // Registered as claustrum with the harness signer's manifest: the same
                // management surface and vocabulary ck-bus reaches in production.
                let manifest = HarnessSigner::generated(&[]).manifest();
                tokio::spawn(async move {
                    subc_client_rs::serve_with(&connection_file, manifest, vault).await
                })
            },
            {
                let connection_file = connection_file.clone();
                tokio::spawn(async move {
                    subc_client_rs::serve_with(&connection_file, callosum.manifest(), callosum)
                        .await
                })
            },
        ];
        let daemon = Self {
            root,
            connection_file,
            operator_dir,
            operator_before,
            daemon,
            modules,
        };
        for module_id in ["claustrum", "callosum", "ckbus"] {
            daemon.wait_for_catalog_id(module_id).await;
        }
        daemon
    }

    async fn wait_for_catalog_id(&self, module_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let response = control::response(
                &self.connection_file,
                ClientControlRequest::CatalogList {
                    module_id: Some(module_id.to_string()),
                },
            )
            .await;
            let ClientControlResponse::CatalogList { modules, .. } = response else {
                panic!("catalog.list must return its matching response variant");
            };
            if modules.iter().any(|module| module.module_id == module_id) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "catalog registration {module_id} did not appear"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// `module_id`'s pid from `supervisor.provenance` (`daemon_observed.pid`), re-read
    /// every 200 ms for up to 2 s while absent.
    async fn supervised_pid(&self, module_id: &str) -> u32 {
        self.observed(module_id).await.0
    }

    /// `module_id`'s pid and the daemon's record of when it spawned that process.
    async fn observed(&self, module_id: &str) -> (u32, Option<u64>) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let response = control::response(
                &self.connection_file,
                ClientControlRequest::SupervisorProvenance {
                    module_id: Some(module_id.to_string()),
                },
            )
            .await;
            let ClientControlResponse::SupervisorProvenance { modules, .. } = response else {
                panic!("supervisor.provenance must return its matching response variant");
            };
            let matching: Vec<_> = modules
                .into_iter()
                .filter(|entry| entry.module_id == module_id)
                .collect();
            assert_eq!(matching.len(), 1, "one {module_id} provenance entry");
            if let Some(pid) = matching[0].daemon_observed.pid {
                return (pid, matching[0].daemon_observed.spawned_at_ms);
            }
            assert!(
                Instant::now() < deadline,
                "supervisor.provenance {module_id} pid stayed absent for 2 s: {matching:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn shutdown(self) {
        for module in self.modules {
            module.abort();
            let _ = module.await;
        }
        self.daemon.abort();
        let _ = self.daemon.await;
        assert_eq!(
            data_home::fingerprint(self.operator_dir.as_deref()),
            self.operator_before,
            "the operator's real ckbus data home must be unchanged by the run"
        );
    }
}

/// A participant child declared under `module_id`, as `harness::issuance` declares
/// `participant`.
fn participant_block() -> Value {
    let exe = std::env::current_exe().expect("the row's own executable");
    json!({
        "program": "/bin/sh",
        "args": [
            "-c",
            format!(
                "{PARTICIPANT_ARGV_ENV}=\"$*\" exec \"$0\" --exact {} --nocapture --test-threads=1",
                rows::CHILD_TEST
            ),
            exe.display().to_string(),
        ],
        "enabled": true,
        "protocol": "subc",
        "drain_timeout_ms": 2000,
    })
}

/// Renders the fixture config with the harness sentinel values and ck-bus's broker
/// inputs, then applies `edit` to it.
fn render_config(
    root: &TestTempDir,
    ckbus_env: Vec<(String, String)>,
    edit: impl FnOnce(&mut Value),
) -> PathBuf {
    let config_file = config::render(
        root,
        Path::new(env!("CARGO_BIN_EXE_ck-bus")),
        SentinelTiming {
            period_ms: Some(sentinel::PERIOD_MS),
            timeout_ms: Some(sentinel::TIMEOUT_MS),
        },
    );
    let mut value: Value = serde_json::from_slice(&fs::read(&config_file).unwrap()).unwrap();
    for (key, entry) in ckbus_env {
        value["modules"]["ckbus"]["env"][key] = json!(entry);
    }
    edit(&mut value);
    fs::write(&config_file, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    config_file
}

struct Plane {
    trust: TrustChain,
    run: Daemon,
    url: String,
    server_log: PathBuf,
    ready: Value,
    observer: async_nats::Client,
    _guard: ServerGuard,
}

impl Plane {
    fn root(&self) -> PathBuf {
        self.run.root.path().to_path_buf()
    }

    fn account_public(&self) -> String {
        self.ready["account_public"].as_str().unwrap().to_string()
    }

    fn names(&self) -> AccountNames {
        grants::derive_account(self.ready["acct"].as_str().unwrap()).unwrap()
    }

    fn server_log(&self) -> String {
        fs::read_to_string(&self.server_log).unwrap_or_default()
    }

    /// The `n`th `event` line in ck-bus's capture logs, waiting for it.
    async fn nth(&self, event: &str, n: usize) -> Value {
        bus::wait_event(&self.root(), event, n, BOOT_LIMIT).await;
        bus::events(&self.root(), event)[n - 1].clone()
    }

    async fn pid(&self, module_id: &str) -> u32 {
        self.run.supervised_pid(module_id).await
    }

    async fn relay(&self, module_id: &str, method: &str, params: Value) -> rows::CkbusReply {
        relay_to(&self.run.connection_file, module_id, method, params)
            .await
            .unwrap_or_else(|error| panic!("the {module_id} relay failed: {error}"))
    }

    /// Fetches `module_id`'s credential and connects with it, the nonce signed by ck-bus.
    /// Fails, with what refused it, when either step does.
    async fn connect(&self, module_id: &'static str) -> Result<Participant, String> {
        let answer = self
            .relay(module_id, issuance::CREDENTIAL_OP, json!({}))
            .await
            .map_err(|(code, message)| format!("ckbus.credential: {code}: {message}"))?;
        let public = answer["credential_public"].as_str().unwrap().to_string();
        let signs = SignLog::default();
        let client = VerdictClient::connect(
            &self.url,
            answer["jwt"].as_str().unwrap(),
            recording_signer(
                self.run.connection_file.clone(),
                module_id,
                public.clone(),
                signs.clone(),
            ),
            Some(format!("_INBOX.{public}")),
        )
        .await
        .map_err(|error| format!("connect: {error}"))?;
        let inbox = client
            .client
            .subscribe(format!("_INBOX.{public}.{INBOX_LEAF}"))
            .await
            .map_err(|error| format!("subscribe: {error}"))?;
        client
            .client
            .flush()
            .await
            .map_err(|error| error.to_string())?;
        Ok(Participant {
            module_id,
            answer,
            client,
            inbox,
            signs,
        })
    }

    /// The participant publishes, and a harness observer receives it; the observer
    /// publishes to the participant's inbox, and the participant receives it.
    async fn exchange(&self, participant: &mut Participant, tag: &str) {
        let dead = self.names().effect_dead();
        let mut watch = self.observer.subscribe(dead.clone()).await.unwrap();
        self.observer_round_trip().await;
        let outbound = format!("{} {tag} outbound", participant.module_id);
        participant
            .client
            .client
            .publish(dead.clone(), outbound.clone().into())
            .await
            .unwrap();
        participant.client.client.flush().await.unwrap();
        let received = tokio::time::timeout(EXCHANGE_LIMIT, async {
            while let Some(message) = watch.next().await {
                if message.payload.as_ref() == outbound.as_bytes() {
                    return;
                }
            }
        })
        .await;
        assert!(
            received.is_ok(),
            "{outbound:?} never reached the observer on {dead}; events {:?}",
            participant.client.events()
        );
        participant.client.expect_allowed(&dead).await;

        // The server handles each connection's operations in order, so having received
        // the participant's publish it has also handled the inbox subscription the
        // participant sent before it (including one re-sent on a reconnect).
        let inbound = format!("{} {tag} inbound", participant.module_id);
        self.observer
            .publish(
                format!("_INBOX.{}.{INBOX_LEAF}", participant.public()),
                inbound.clone().into(),
            )
            .await
            .unwrap();
        self.observer.flush().await.unwrap();
        let message = tokio::time::timeout(EXCHANGE_LIMIT, participant.inbox.next())
            .await
            .unwrap_or_else(|_| panic!("{inbound:?} never reached the participant"))
            .expect("the participant's inbox subscription is open");
        assert_eq!(message.payload.as_ref(), inbound.as_bytes());
    }

    /// Returns once the server has handled every operation the observer sent before.
    /// `flush` only empties the client's write buffer; a message the observer publishes
    /// to itself comes back only after the server handled everything queued ahead of it,
    /// so a subscription made just before is live when this returns.
    async fn observer_round_trip(&self) {
        let subject = format!("outage.sync.{}", KeyPair::new_user().public_key());
        let mut echo = self.observer.subscribe(subject.clone()).await.unwrap();
        self.observer.publish(subject, "sync".into()).await.unwrap();
        self.observer.flush().await.unwrap();
        tokio::time::timeout(EXCHANGE_LIMIT, echo.next())
            .await
            .expect("the observer's own message came back")
            .expect("the echo subscription is open");
    }

    async fn system(&self) -> async_nats::Client {
        let user = KeyPair::new_user();
        let jwt = self.trust.user_jwt(
            &bus::system_root_id(),
            &self.trust.system_account,
            &user,
            unix_now() - 60,
        );
        bus::connect(&self.url, jwt, user)
            .await
            .expect("a harness system client connects")
    }

    async fn revocations(&self) -> serde_json::Map<String, Value> {
        let system = self.system().await;
        let jwt = bus::lookup(&system, &self.account_public())
            .await
            .expect("the box account JWT is stored");
        bus::claims(&jwt)["nats"]["revocations"]
            .as_object()
            .cloned()
            .unwrap_or_default()
    }

    async fn wait_revoked(&self, user: &str) {
        let deadline = Instant::now() + BOOT_LIMIT;
        while !self.revocations().await.contains_key(user) {
            assert!(
                Instant::now() < deadline,
                "{user} was never revoked; revocation lines: {:?}",
                bus::events(&self.root(), "ckbus.revocation.step")
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn list_entry(&self, module_id: &str) -> Value {
        let response = control::response(
            &self.run.connection_file,
            ClientControlRequest::SupervisorList {},
        )
        .await;
        let ClientControlResponse::SupervisorList { modules, .. } = response else {
            panic!("supervisor.list must return its matching response variant");
        };
        let entry = modules
            .iter()
            .find(|module| module.module_id == module_id)
            .unwrap_or_else(|| panic!("supervisor.list lists {module_id}"));
        serde_json::to_value(entry).unwrap()
    }

    async fn set_enabled(&self, module_id: &str, enabled: bool) {
        let reply = control::rpc(
            &self.run.connection_file,
            ClientControlRequest::SupervisorSetEnabled {
                module_id: module_id.to_string(),
                enabled,
            },
        )
        .await;
        if let control::ControlReply::Error(error) = reply {
            panic!(
                "supervisor.set_enabled {module_id} {enabled} refused: {} {}",
                error.code, error.message
            );
        }
    }

    /// Disables `module_id` and waits until the supervisor reports it stopped.
    async fn stop(&self, module_id: &str) {
        self.set_enabled(module_id, false).await;
        let deadline = Instant::now() + BOOT_LIMIT;
        loop {
            let entry = self.list_entry(module_id).await;
            // Only a finished stop: `draining` is also not running, with the process alive.
            let stopped = entry["state"] == "disabled" || entry["state"] == "stopped";
            if entry["enabled"] == false && entry["live"] == false && stopped {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{module_id} did not stop: {entry}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Restarts nats-server through the supervisor and returns its new pid.
    async fn restart_server(&self) -> u32 {
        let before = self.pid("nats-server").await;
        let reply = control::rpc(
            &self.run.connection_file,
            ClientControlRequest::SupervisorRestart {
                module_id: "nats-server".to_string(),
                drain_timeout_ms: None,
            },
        )
        .await;
        if let control::ControlReply::Error(error) = reply {
            panic!(
                "supervisor.restart nats-server refused: {} {}",
                error.code, error.message
            );
        }
        let deadline = Instant::now() + BOOT_LIMIT;
        loop {
            let entry = self.list_entry("nats-server").await;
            if entry["state"] == "running" {
                let pid = self.pid("nats-server").await;
                if pid != before {
                    return pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "nats-server was never replaced: {entry}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_observer_connected(&self) {
        let deadline = Instant::now() + BOOT_LIMIT;
        while self.observer.connection_state() != async_nats::connection::State::Connected {
            assert!(
                Instant::now() < deadline,
                "the harness observer never reconnected"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn finish(self, participants: Vec<Participant>) {
        drop(participants);
        self.stop("nats-server").await;
        self.run.shutdown().await;
    }
}

/// The parent pid of `pid`, from `ps`.
fn parent_pid(pid: u32) -> u32 {
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("ps names {pid}'s parent: {output:?}"))
}

/// A plane in which the daemon supervises nats-server and ck-bus as siblings, with both
/// participants registered and neither holding a credential. `ckbus_backoff_ms` is
/// ck-bus's crash-restart backoff; `None` keeps the supervisor's default.
async fn start(ckbus_backoff_ms: Option<u64>) -> Option<Plane> {
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err((gate, observation)) => {
            RowReport::skipped(Row::SignerOutage, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&vocabulary());
            return None;
        }
    };
    let trust = TrustChain::generate();
    let root = SignerRun::tree();
    // The harness server writes setup's outputs (operator JWT, resolver, server.conf);
    // it is stopped at once, and the supervisor runs the same configuration instead.
    let server = BusServer::start(&bin, &root.join("nats"), &trust, LOOPBACK).await;
    let url = server.url.clone();
    let server_log = server.log.clone();
    let conf = server.dir.join("server.conf");
    let env = server.ckbus_env();
    server.stop().await;
    let guard = ServerGuard { conf: conf.clone() };

    let config_file = render_config(&root, env, |value| {
        let modules = &mut value["modules"];
        modules["nats-server"]["program"] = json!(bin.display().to_string());
        modules["nats-server"]["args"] = json!(["-c", conf.display().to_string()]);
        modules["nats-server"]["enabled"] = json!(true);
        if let Some(backoff) = ckbus_backoff_ms {
            modules["ckbus"]["restart"] = json!({"backoff_ms": backoff, "max_backoff_ms": backoff});
        }
        modules[PARTICIPANT] = participant_block();
        modules[BYSTANDER] = participant_block();
    });
    let run = Daemon::start(root, &config_file, trust.signer.clone()).await;
    // ck-bus retries an unreachable broker once per sentinel period, so it boots once
    // the supervised server listens, whichever starts first.
    let ready = bus::wait_event(run.root.path(), "ckbus.bootstrap.ready", 1, BOOT_LIMIT).await;
    run.wait_for_catalog_id(PARTICIPANT).await;
    run.wait_for_catalog_id(BYSTANDER).await;

    let user = KeyPair::new_user();
    let account_public = ready["account_public"].as_str().unwrap().to_string();
    let jwt = trust.user_jwt(&bus::box_root_id(), &account_public, &user, unix_now() - 60);
    let observer = bus::connect(&url, jwt, user)
        .await
        .expect("the harness observer connects");
    Some(Plane {
        trust,
        run,
        url,
        server_log,
        ready,
        observer,
        _guard: guard,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killing_ckbus_leaves_the_server_and_open_connections_up_and_new_connects_wait_for_it() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(Some(CKBUS_BACKOFF_MS)).await else {
        return;
    };
    let mut participant = plane
        .connect(PARTICIPANT)
        .await
        .expect("participant connects");
    plane.exchange(&mut participant, "before the kill").await;
    let server_pid = plane.pid("nats-server").await;
    let ckbus_pid = plane.pid("ckbus").await;
    let started_before = sentinel::started_count(&plane.root());

    let killed_ms = now_ms();
    sentinel::signal(ckbus_pid, "KILL");
    // While ck-bus is dead: the server is the same process and the connected participant
    // keeps publishing and receiving.
    assert_eq!(plane.pid("nats-server").await, server_pid);
    plane.exchange(&mut participant, "during the outage").await;
    let outage_checked_ms = now_ms();

    // A new participant tries to connect until it can. Each attempt is (started,
    // finished, what refused it).
    let mut refused: Vec<(u64, u64, String)> = Vec::new();
    let (mut newcomer, connected_ms) = loop {
        let attempt_ms = now_ms();
        match plane.connect(BYSTANDER).await {
            Ok(newcomer) => break (newcomer, now_ms()),
            Err(refusal) => refused.push((attempt_ms, now_ms(), refusal)),
        }
        assert!(
            now_ms() - killed_ms < BOOT_LIMIT.as_millis() as u64,
            "the new participant never connected; refusals {refused:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    // The outage's bounds, from the daemon's own records: when it saw the killed process
    // exit, and when it spawned the replacement.
    plane.nth("ckbus.runtime.started", started_before + 1).await;
    let back = plane.nth("ckbus.bootstrap.ready", 2).await;
    let exited = plane.list_entry("ckbus").await;
    let (replacement_pid, spawned) = plane.run.observed("ckbus").await;
    assert_ne!(replacement_pid, ckbus_pid, "ck-bus was replaced");
    let spawned_ms = spawned.expect("the daemon records when it spawned the replacement");
    let exited_ms = exited["last_exit_ms"]
        .as_u64()
        .expect("the exit was observed");
    eprintln!(
        "kill at {killed_ms}; exit observed at {exited_ms}; outage checks done at \
         {outage_checked_ms}; replacement spawned at {spawned_ms}; ready at {}; new \
         participant connected at {connected_ms}; refusals {refused:?}",
        at_ms(&back)
    );
    assert_eq!(exited["last_exit_signal"], 9, "the kill ended it: {exited}");
    assert!(exited_ms >= killed_ms);
    assert!(
        outage_checked_ms < spawned_ms,
        "the outage observations finished before the replacement was spawned"
    );
    let (first_start, first_end, _) = refused
        .first()
        .unwrap_or_else(|| panic!("a connect during the outage was refused"));
    assert!(
        *first_start < spawned_ms && *first_end < spawned_ms,
        "the first connect was tried and refused while no ck-bus process existed"
    );
    assert!(
        connected_ms >= at_ms(&back),
        "the new participant connected only once ck-bus was back"
    );
    assert_eq!(
        plane.pid("nats-server").await,
        server_pid,
        "nats-server's pid from provenance is unchanged"
    );
    assert!(participant.connected());
    assert_eq!(
        participant.disconnects(),
        0,
        "the established connection never dropped: {:?}",
        participant.client.events()
    );
    plane.exchange(&mut participant, "after the return").await;
    plane.exchange(&mut newcomer, "after the return").await;

    passed();
    plane.finish(vec![participant, newcomer]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarting_the_server_with_ckbus_up_has_every_participant_re_sign_and_reconnect() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(None).await else {
        return;
    };
    let mut first = plane
        .connect(PARTICIPANT)
        .await
        .expect("participant connects");
    let mut second = plane.connect(BYSTANDER).await.expect("bystander connects");
    plane
        .exchange(&mut first, "before the server restart")
        .await;
    plane
        .exchange(&mut second, "before the server restart")
        .await;
    let ckbus_pid = plane.pid("ckbus").await;
    let started = sentinel::started_count(&plane.root());

    let restarted_ms = now_ms();
    let server_pid = plane.pid("nats-server").await;
    let new_pid = plane.restart_server().await;
    assert_ne!(new_pid, server_pid);
    let deadline = Instant::now() + BOOT_LIMIT;
    for participant in [&first, &second] {
        // Reconnected, through a nonce ck-bus signed after the restart.
        while !(participant.connected()
            && participant.disconnects() >= 1
            && participant.signatures_since(restarted_ms) >= 1)
        {
            assert!(
                Instant::now() < deadline,
                "{} never re-signed and reconnected: events {:?}, refusals {:?}\n{}",
                participant.module_id,
                participant.client.events(),
                participant.refusals_since(restarted_ms),
                plane.server_log()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            participant.refusals_since(restarted_ms),
            Vec::<String>::new(),
            "{}'s re-sign was answered first time",
            participant.module_id
        );
    }
    plane.wait_observer_connected().await;
    plane.exchange(&mut first, "after the server restart").await;
    plane
        .exchange(&mut second, "after the server restart")
        .await;
    assert_eq!(plane.pid("ckbus").await, ckbus_pid, "ck-bus stayed up");
    assert_eq!(sentinel::started_count(&plane.root()), started);
    let revocations = plane.revocations().await;
    for participant in [&first, &second] {
        assert!(
            !revocations.contains_key(&participant.public()),
            "a reconnect needs no refetch and revokes nothing"
        );
    }

    passed();
    plane.finish(vec![first, second]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_ckbus_keeps_established_connections_and_supersedes_only_a_forced_reconnect() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(None).await else {
        return;
    };
    let mut forced = plane
        .connect(PARTICIPANT)
        .await
        .expect("participant connects");
    let mut untouched = plane.connect(BYSTANDER).await.expect("bystander connects");
    plane.exchange(&mut forced, "before the kill").await;
    plane.exchange(&mut untouched, "before the kill").await;
    let server_pid = plane.pid("nats-server").await;

    sentinel::signal(plane.pid("ckbus").await, "KILL");
    plane.nth("ckbus.bootstrap.ready", 2).await;
    for participant in [&mut forced, &mut untouched] {
        assert!(participant.connected());
        assert_eq!(
            participant.disconnects(),
            0,
            "{}'s connection survived the restart: {:?}",
            participant.module_id,
            participant.client.events()
        );
        plane.exchange(participant, "after the restart").await;
    }
    assert_eq!(plane.pid("nats-server").await, server_pid);

    // Forced to reconnect: the restarted ck-bus holds no seed for its key.
    let forced_ms = now_ms();
    forced
        .client
        .client
        .force_reconnect()
        .await
        .expect("a reconnect can be forced");
    let deadline = Instant::now() + BOOT_LIMIT;
    while !forced
        .refusals_since(forced_ms)
        .iter()
        .any(|refusal| refusal.starts_with(issuance::code::CREDENTIAL_SUPERSEDED))
    {
        assert!(
            Instant::now() < deadline,
            "the forced reconnect was never refused as superseded: {:?}",
            forced.refusals_since(forced_ms)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!forced.connected(), "a superseded key cannot reconnect");
    assert_eq!(forced.signatures_since(forced_ms), 0);

    // The refetch: the next epoch of the same generation, then a reconnect.
    let mut refetched = plane
        .connect(PARTICIPANT)
        .await
        .expect("the refetch connects");
    assert_eq!(refetched.epoch(), forced.epoch() + 1);
    assert_eq!(
        refetched.answer["spawn_generation"],
        forced.answer["spawn_generation"]
    );
    plane.exchange(&mut refetched, "after the refetch").await;
    plane.wait_revoked(&forced.public()).await;
    let revocations = plane.revocations().await;
    assert!(!revocations.contains_key(&refetched.public()));
    assert!(
        !revocations.contains_key(&untouched.public()),
        "a participant never forced to reconnect is not revoked"
    );
    assert!(untouched.connected());
    assert_eq!(untouched.disconnects(), 0);
    plane.exchange(&mut untouched, "after the refetch").await;

    // The expiry arm: no JWT lifetime is pinned, and ck-bus issues none.
    let claims = bus::claims(refetched.answer["jwt"].as_str().unwrap());
    assert!(claims.get("exp").is_none(), "{claims}");
    RowReport::skipped(
        Row::SignerOutage,
        "user-jwt-ttl-unpinned",
        format!(
            "expiry arm: ck-bus issued the epoch-{} user JWT {} with no exp claim; the \
             foundation names no per-process user JWT lifetime",
            refetched.epoch(),
            claims["jti"].as_str().unwrap_or_default()
        ),
    )
    .served_by(ServedBy::HarnessSigner)
    .emit(&vocabulary());

    passed();
    plane.finish(vec![forced, refetched, untouched]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_ckbus_down_a_restarted_server_leaves_every_participant_disconnected_until_it_returns()
{
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(None).await else {
        return;
    };
    let mut first = plane
        .connect(PARTICIPANT)
        .await
        .expect("participant connects");
    let mut second = plane.connect(BYSTANDER).await.expect("bystander connects");
    plane.exchange(&mut first, "before the outage").await;
    plane.exchange(&mut second, "before the outage").await;

    // ck-bus is never nats-server's parent: both are the supervisor's children.
    let ckbus_pid = plane.pid("ckbus").await;
    let server_pid = plane.pid("nats-server").await;
    assert_ne!(parent_pid(server_pid), ckbus_pid);
    assert_eq!(
        parent_pid(server_pid),
        std::process::id(),
        "nats-server is a child of the daemon, which runs in this test process"
    );

    plane.stop("ckbus").await;
    let restarted_ms = now_ms();
    let new_pid = plane.restart_server().await;
    assert_eq!(parent_pid(new_pid), std::process::id());
    let deadline = Instant::now() + BOOT_LIMIT;
    for participant in [&first, &second] {
        // Disconnected, and at least one reconnect attempt found no signer.
        while participant.connected() || participant.refusals_since(restarted_ms).is_empty() {
            assert!(
                Instant::now() < deadline,
                "{} was not held off the server: events {:?}",
                participant.module_id,
                participant.client.events()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    // It stays so: three sentinel periods with ck-bus down, and no signature answered.
    let hold_until = Instant::now() + Duration::from_millis(sentinel::PERIOD_MS * 3);
    while Instant::now() < hold_until {
        for participant in [&first, &second] {
            assert!(!participant.connected(), "{}", participant.module_id);
            assert_eq!(participant.signatures_since(restarted_ms), 0);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eprintln!(
        "refusals while ck-bus was down: {:?} / {:?}",
        first.refusals_since(restarted_ms),
        second.refusals_since(restarted_ms)
    );

    // ck-bus returns. Its new process holds no seed for either key, so the old
    // credentials stay refused and each participant reconnects after one refetch.
    plane.set_enabled("ckbus", true).await;
    plane.nth("ckbus.bootstrap.ready", 2).await;
    for participant in [&first, &second] {
        let refusal = plane
            .relay(
                participant.module_id,
                issuance::NONCE_SIGN_OP,
                json!({
                    "nonce_b64": STANDARD.encode(b"outage-control"),
                    "credential_public": participant.public(),
                }),
            )
            .await
            .expect_err("a key from before the outage is not signed for");
        assert_eq!(
            refusal.0,
            issuance::code::CREDENTIAL_SUPERSEDED,
            "{refusal:?}"
        );
        assert!(!participant.connected());
    }
    plane.wait_observer_connected().await;
    let mut first_back = plane
        .connect(PARTICIPANT)
        .await
        .expect("participant refetches");
    let mut second_back = plane.connect(BYSTANDER).await.expect("bystander refetches");
    assert_eq!(first_back.epoch(), first.epoch() + 1);
    assert_eq!(second_back.epoch(), second.epoch() + 1);
    plane.exchange(&mut first_back, "after the return").await;
    plane.exchange(&mut second_back, "after the return").await;
    plane.wait_revoked(&first.public()).await;
    plane.wait_revoked(&second.public()).await;

    passed();
    plane
        .finish(vec![first, second, first_back, second_back])
        .await;
}

/// The vault as the recovery arm drives it: the harness stub's refusal until
/// `answering` is set, the harness signer's answers after.
#[derive(Clone)]
struct SwitchableVault {
    signer: HarnessSigner,
    refusing: StubRecorder,
    answering: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl ModuleHandler for SwitchableVault {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        if self.answering.load(Ordering::SeqCst) {
            self.signer.answer(&body)
        } else {
            self.refusing.handle(ctx, body).await
        }
    }

    async fn on_bind(&self, _request: &RouteBindRequest) -> BindDecision {
        BindDecision::accept()
    }
}

/// How long a successful bootstrap attempt may take after its period's sleep: it signs
/// three JWTs through the vault route, pushes and reads back the account JWT and
/// creates the bucket and streams on a fresh server. Measured at 70 to 120 ms on a
/// developer machine; the slack absorbs a loaded CI machine while staying under a
/// second period, so a recovery that took one more retry still fails.
const BOOT_ATTEMPT_SLACK: Duration = Duration::from_millis(900);

/// Beyond the row's text (see the module comment): the vault refuses, then answers,
/// and ck-bus recovers on its next retry without a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusing_vault_that_answers_again_is_recovered_from_without_a_restart() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err((gate, observation)) => {
            RowReport::skipped(Row::SignerOutage, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&vocabulary());
            return;
        }
    };
    let trust = TrustChain::generate();
    let root: TestTempDir = SignerRun::tree();
    let server = BusServer::start(&bin, &root.join("nats"), &trust, LOOPBACK).await;
    let config_file = render_config(&root, server.ckbus_env(), |_| {});
    let answering = Arc::new(AtomicBool::new(false));
    let vault = SwitchableVault {
        signer: trust.signer.clone(),
        refusing: StubRecorder::refusing("claustrum", CLAUSTRUM_OPERATIONS),
        answering: answering.clone(),
    };
    let run_root = root.path().to_path_buf();
    let run = Daemon::start(root, &config_file, vault).await;
    let connection_file = run.connection_file.clone();

    // The outage: ck-bus is running, reports the vault as the cause, and has built
    // nothing. The predicate requires `bootstrap: down` with the vault's cause, which
    // `bootstrap: starting` (also Failing/Unavailable) never carries.
    let started = sentinel::started(&run_root, 1).await;
    assert_eq!(started.period, Duration::from_millis(sentinel::PERIOD_MS));
    let (_, _, metrics, _) = sentinel::wait_health(
        &connection_file,
        BOOT_LIMIT,
        "down/Unavailable naming the unreachable vault",
        |status, _, metrics| {
            status == "Failing"
                && metrics["class"] == "Unavailable"
                && metrics["bootstrap"] == "down"
                && metrics["cause"] == bootstrap::cause::ROOT_KEY_UNREACHABLE
        },
    )
    .await;
    eprintln!("outage health: {metrics}");
    bus::wait_event(&run_root, "ckbus.bootstrap.down", 2, BOOT_LIMIT).await;
    let before = sentinel::ckbus_list_entry(&connection_file).await;
    assert_eq!(before["state"], "running", "{before}");
    assert!(
        !bus::account_json(&run_root).exists(),
        "no account recorded"
    );
    assert!(!bus::own_users_json(&run_root).exists(), "no user recorded");
    assert_eq!(
        server.stored_accounts(),
        BTreeSet::from([trust.system_account.clone()]),
        "no account created while the vault refuses"
    );

    // The vault answers again.
    let answering_ms = now_ms();
    answering.store(true, Ordering::SeqCst);
    let ready = bus::wait_event(
        &run_root,
        "ckbus.bootstrap.ready",
        1,
        started.period * 2 + Duration::from_secs(10),
    )
    .await;
    let downs = bus::events(&run_root, "ckbus.bootstrap.down");
    let last_down = downs.last().expect("the outage was logged");
    let gap = Duration::from_millis(at_ms(&ready) - at_ms(last_down));
    eprintln!(
        "vault answering at {answering_ms}; last refused attempt {} at {}; ready attempt {} at \
         {}; gap {gap:?} (period {:?}, slack {BOOT_ATTEMPT_SLACK:?})",
        last_down["attempt"],
        at_ms(last_down),
        ready["attempt"],
        at_ms(&ready),
        started.period
    );
    assert!(
        downs
            .iter()
            .filter(|down| at_ms(down) > answering_ms)
            .count()
            <= 1,
        "at most the attempt in flight when the vault returned was refused: {downs:?}"
    );
    assert_eq!(
        ready["attempt"].as_u64(),
        last_down["attempt"].as_u64().map(|attempt| attempt + 1),
        "the first retry after the last refusal boots"
    );
    assert!(
        gap >= started.period * 9 / 10 && gap <= started.period + BOOT_ATTEMPT_SLACK,
        "the recovery came on the next sentinel period: {gap:?}"
    );
    rows_wait_up(&connection_file, &run_root, &ready, started.period).await;

    // The same process throughout: no restart.
    let after = sentinel::ckbus_list_entry(&connection_file).await;
    assert_eq!(after["state"], "running", "{after}");
    assert_eq!(after["restart_count"], before["restart_count"], "{after}");
    assert_eq!(
        after["lifetime_restarts"], before["lifetime_restarts"],
        "{after}"
    );
    assert_eq!(sentinel::started_count(&run_root), 1, "one ck-bus process");
    assert_eq!(ready["incarnation"], started.incarnation.as_str());
    assert!(bus::account_json(&run_root).exists());

    server.stop().await;
    run.shutdown().await;
    let mut advertised = vocabulary();
    advertised.extend(CLAUSTRUM_OPERATIONS.iter().map(|op| (*op).to_string()));
    RowReport::passed(Row::SignerOutage)
        .served_by(ServedBy::HarnessStub)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.public_key")
        .reached("credential.sign")
        .emit(&advertised);
}

/// Health reads up within two sentinel periods of the recovered bootstrap, timed by
/// ck-bus's own lines (the sentinel row's bound for a fresh bootstrap).
async fn rows_wait_up(connection_file: &Path, run_root: &Path, ready: &Value, period: Duration) {
    sentinel::wait_health(
        connection_file,
        period * 2 + Duration::from_secs(5),
        "bus.health.up",
        sentinel::is_up,
    )
    .await;
    let up = bus::events(run_root, "ckbus.sentinel.verdict")
        .into_iter()
        .find(|verdict| verdict["verdict"] == "up")
        .expect("the up verdict is logged");
    let took = at_ms(&up) - at_ms(ready);
    assert!(
        Duration::from_millis(took) <= period * 2,
        "bus.health.up within two periods of the recovered bootstrap: {took} ms"
    );
}
