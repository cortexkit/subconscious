//! Upgrading a plane that ck-bus 0.1.29 built with five streams to the current six
//! streams and two module durables, in place. This is the upgrade arm of the "Install
//! bootstrap and own users" acceptance row in `docs/specs/ck-bus-module.md`. The vault
//! is the harness signer answered in process, the broker a real `nats-server`; no subc
//! daemon is started.
//!
//! - The emitted configuration of the five older streams is byte-for-byte the one the
//!   five-stream ck-bus sent (`EARLIER_STREAM_CONFIGS`), and the event stream carries its
//!   per-subject cap.
//! - Live: boot once as the five-stream ck-bus did (the event stream and the module
//!   durables withheld), store one message on each stream, then boot the current ck-bus.
//!   The five streams are untouched (same configuration as the server reports it, same
//!   creation time, same message count), the event stream exists with its limits, and
//!   `m_basal` and `m_prefrontal-core` exist with the shipped durable configuration. The
//!   room post published before `m_prefrontal-core` existed is pending on it. A further
//!   boot leaves all six streams and both module durables unchanged.

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
#[path = "../src/runtime/seams.rs"]
mod runtime;

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_nats::jetstream::{self, consumer::pull};
use async_trait::async_trait;
use bootstrap::{
    boot,
    config::BrokerConfig,
    plane::{
        BoxPlane, Broker, CensusRecord, ConsumerState, DurableConsumer, NatsBroker, PlaneError,
        SentinelLink, SystemPlane,
    },
    store::Store,
    BootDeps, BrokerInputs, OwnProcess, OwnSpawn, Ready,
};
use cortexkit_bus_naming::{shipped_streams, AccountNames, StreamKind, StreamSpec, GIB};
use credentials::{
    vault::{VaultError, VaultSigning},
    wire::{self, VaultPublicKey, VaultSignature},
    Credentials,
};
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    report::{Row, RowReport, ServedBy},
    signer::{nats::nats_server_bin, HarnessSigner, SIGNER_OPERATIONS},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subc_client_rs::HandlerOutcome;
use subc_protocol::MachineId;

/// The account the emitted-configuration golden below was recorded for.
const GOLDEN_ACCOUNT: &str = "box_upgradegolden";

/// What the five-stream ck-bus (0.1.29) sent to create each of its streams, serialized
/// exactly as `create_stream` puts it on the wire, recorded from that build for
/// `GOLDEN_ACCOUNT`. nats-server refuses a create whose configuration differs from the
/// stored stream, so any change here would stop an upgraded ck-bus from booting.
const EARLIER_STREAM_CONFIGS: [&str; 5] = [
    r#"{"name":"CK_BOX_UPGRADEGOLDEN_ROOM","max_bytes":1073741824,"max_msgs":0,"max_msgs_per_subject":0,"discard":"old","subjects":["ck.box_upgradegolden.room.*.post"],"retention":"limits","max_consumers":0,"max_age":86400000000000,"storage":"file","num_replicas":1,"duplicate_window":120000000000,"consumer_limits":null}"#,
    r#"{"name":"CK_BOX_UPGRADEGOLDEN_WAKE","max_bytes":1073741824,"max_msgs":0,"max_msgs_per_subject":0,"discard":"old","subjects":["ck.box_upgradegolden.wake.*.fire"],"retention":"limits","max_consumers":0,"max_age":86400000000000,"storage":"file","num_replicas":1,"duplicate_window":120000000000,"consumer_limits":null}"#,
    r#"{"name":"CK_BOX_UPGRADEGOLDEN_PEER","max_bytes":1073741824,"max_msgs":0,"max_msgs_per_subject":0,"discard":"old","subjects":["ck.box_upgradegolden.peer.*.*.deliver"],"retention":"limits","max_consumers":0,"max_age":86400000000000,"storage":"file","num_replicas":1,"duplicate_window":120000000000,"consumer_limits":null}"#,
    r#"{"name":"CK_BOX_UPGRADEGOLDEN_EFFECT","max_bytes":268435456,"max_msgs":0,"max_msgs_per_subject":0,"discard":"new","subjects":["ck.box_upgradegolden.effect.*.*.intent"],"retention":"workqueue","max_consumers":0,"max_age":86400000000000,"storage":"file","num_replicas":1,"duplicate_window":120000000000,"consumer_limits":null}"#,
    r#"{"name":"CK_BOX_UPGRADEGOLDEN_EFFECT_DEAD","max_bytes":67108864,"max_msgs":0,"max_msgs_per_subject":0,"discard":"old","subjects":["ck.box_upgradegolden.effect.dead"],"retention":"limits","max_consumers":0,"max_age":604800000000000,"storage":"file","num_replicas":1,"duplicate_window":120000000000,"consumer_limits":null}"#,
];

const EVENT_PER_SUBJECT_CAP: i64 = 10_000;

fn signer_vocabulary() -> BTreeSet<String> {
    SIGNER_OPERATIONS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

fn passed() {
    RowReport::passed(Row::InstallBootstrap)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&signer_vocabulary());
}

#[test]
fn the_five_older_streams_are_emitted_exactly_as_before() {
    let names = AccountNames::derive(GOLDEN_ACCOUNT).unwrap();
    let specs = shipped_streams(&names);
    let older: Vec<&StreamSpec> = specs
        .iter()
        .filter(|spec| spec.kind != StreamKind::Event)
        .collect();
    assert_eq!(older.len(), EARLIER_STREAM_CONFIGS.len());
    for (spec, earlier) in older.into_iter().zip(EARLIER_STREAM_CONFIGS) {
        let emitted = serde_json::to_string(&bootstrap::plane::stream_config(spec)).unwrap();
        assert_eq!(
            emitted, earlier,
            "{} must be sent exactly as the five-stream ck-bus sent it",
            spec.name
        );
    }
}

#[test]
fn the_event_stream_is_emitted_with_its_per_subject_cap() {
    let names = AccountNames::derive(GOLDEN_ACCOUNT).unwrap();
    let spec = shipped_streams(&names)
        .into_iter()
        .find(|spec| spec.kind == StreamKind::Event)
        .expect("the naming crate ships the event stream");
    assert_eq!(spec.max_msgs_per_subject, Some(EVENT_PER_SUBJECT_CAP));
    let emitted = serde_json::to_value(bootstrap::plane::stream_config(&spec)).unwrap();
    assert_eq!(emitted["name"], "CK_BOX_UPGRADEGOLDEN_EVENT");
    assert_eq!(
        emitted["subjects"],
        serde_json::json!(["ck.box_upgradegolden.event.>"])
    );
    assert_eq!(emitted["max_msgs_per_subject"], EVENT_PER_SUBJECT_CAP);
    assert_eq!(emitted["max_age"], 7 * 24 * 3600 * 1_000_000_000_u64);
    assert_eq!(emitted["max_bytes"], GIB);
    assert_eq!(emitted["discard"], "old");
    assert_eq!(emitted["retention"], "limits");
    assert_eq!(emitted["duplicate_window"], 120_000_000_000_u64);
}

/// The harness signer answering in process, through claustrum's exact wire.
struct InProcessVault {
    signer: HarnessSigner,
}

impl InProcessVault {
    fn body(&self, request: Vec<u8>) -> Vec<u8> {
        match self.signer.answer(&request) {
            HandlerOutcome::Response(body) => body,
            _ => panic!("the harness signer answers every well-formed request"),
        }
    }
}

#[async_trait]
impl VaultSigning for InProcessVault {
    async fn sign(
        &self,
        credential_id: &str,
        payload: &[u8],
    ) -> Result<VaultSignature, VaultError> {
        let body = self.body(wire::sign_request(credential_id, payload).unwrap());
        wire::parse_sign_reply(&body).map_err(|error| VaultError::Malformed(error.to_string()))
    }

    async fn public_key(&self, credential_id: &str) -> Result<VaultPublicKey, VaultError> {
        let body = self.body(wire::public_key_request(credential_id));
        wire::parse_public_key_reply(&body)
            .map_err(|error| VaultError::Malformed(error.to_string()))
    }
}

/// ck-bus's own process as a supervisor would report it; no daemon runs here.
struct FixedOwnSpawn;

#[async_trait]
impl OwnSpawn for FixedOwnSpawn {
    async fn own_process(&self) -> Result<OwnProcess, String> {
        Ok(OwnProcess {
            module_id: "ckbus".to_string(),
            spawn_generation: 1,
        })
    }
}

/// The real broker as the five-stream ck-bus used it: its box plane creates every stream
/// but the event stream and creates no module durable, which is what that build's
/// bootstrap did. Every other call goes to the real plane.
struct FiveStreamBroker(NatsBroker);

#[async_trait]
impl Broker for FiveStreamBroker {
    async fn connect_system(
        &self,
        jwt: &str,
        user_public: &str,
    ) -> Result<Arc<dyn SystemPlane>, PlaneError> {
        self.0.connect_system(jwt, user_public).await
    }

    async fn connect_box(
        &self,
        jwt: &str,
        user_public: &str,
    ) -> Result<Arc<dyn BoxPlane>, PlaneError> {
        Ok(Arc::new(FiveStreamBox(
            self.0.connect_box(jwt, user_public).await?,
        )))
    }
}

struct FiveStreamBox(Arc<dyn BoxPlane>);

#[async_trait]
impl BoxPlane for FiveStreamBox {
    async fn ensure_census(&self, account: &AccountNames) -> Result<(), PlaneError> {
        self.0.ensure_census(account).await
    }
    async fn ensure_stream(&self, spec: &StreamSpec) -> Result<(), PlaneError> {
        if spec.kind == StreamKind::Event {
            return Ok(());
        }
        self.0.ensure_stream(spec).await
    }
    async fn publish(&self, subject: &str, payload: Vec<u8>) -> Result<(), PlaneError> {
        self.0.publish(subject, payload).await
    }
    async fn census_put(&self, subject: &str, value: Vec<u8>) -> Result<(), PlaneError> {
        self.0.census_put(subject, value).await
    }
    async fn census_get(
        &self,
        account: &AccountNames,
        key: &str,
    ) -> Result<Option<CensusRecord>, PlaneError> {
        self.0.census_get(account, key).await
    }
    async fn census_keys(&self, account: &AccountNames) -> Result<Vec<String>, PlaneError> {
        self.0.census_keys(account).await
    }
    async fn census_delete(
        &self,
        account: &AccountNames,
        key: &str,
        revision: u64,
    ) -> Result<(), PlaneError> {
        self.0.census_delete(account, key, revision).await
    }
    async fn create_durable(&self, _durable: &DurableConsumer) -> Result<(), PlaneError> {
        Ok(())
    }
    async fn consumer_state(
        &self,
        stream: &str,
        durable: &str,
    ) -> Result<Option<ConsumerState>, PlaneError> {
        self.0.consumer_state(stream, durable).await
    }
    async fn delete_durable(&self, stream: &str, durable: &str) -> Result<bool, PlaneError> {
        self.0.delete_durable(stream, durable).await
    }
    async fn purge_subject(&self, stream: &str, filter_subject: &str) -> Result<u64, PlaneError> {
        self.0.purge_subject(stream, filter_subject).await
    }
    async fn consumer_names(&self, stream: &str) -> Result<Vec<String>, PlaneError> {
        self.0.consumer_names(stream).await
    }
    fn sentinel_link(&self) -> Option<SentinelLink> {
        self.0.sentinel_link()
    }
}

fn fresh_machine_id() -> String {
    let seed = format!("{:?}{}", std::time::SystemTime::now(), std::process::id());
    Sha256::digest(seed.as_bytes())[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn boot_with(
    machine_id: &MachineId,
    config: &Result<BrokerConfig, String>,
    broker: &dyn Broker,
    deps: &BootDeps,
) -> Ready {
    match boot(Some(machine_id), BrokerInputs { config, broker }, deps).await {
        Ok(ready) => ready,
        Err(failure) => panic!(
            "boot failed: {} ({}): {}",
            failure.cause,
            if failure.retry { "retried" } else { "stopped" },
            failure.message
        ),
    }
}

/// One stream as the server reports it: its configuration, its creation time and how
/// many messages it holds. A stream deleted and created again has a new creation time,
/// and one whose configuration was updated reports the new configuration.
async fn stream_state(js: &jetstream::Context, name: &str) -> (Value, String, u64) {
    let mut stream = js
        .get_stream(name)
        .await
        .unwrap_or_else(|error| panic!("{name} must exist: {error}"));
    let info = stream.info().await.expect("stream info").clone();
    (
        serde_json::to_value(&info.config).unwrap(),
        format!("{:?}", info.created),
        info.state.messages,
    )
}

/// One consumer's configuration and creation time, and how many messages it has not
/// delivered yet.
async fn consumer_state(
    js: &jetstream::Context,
    stream: &str,
    durable: &str,
) -> (Value, String, u64) {
    let mut consumer = js
        .get_consumer_from_stream::<pull::Config, _, _>(durable, stream)
        .await
        .unwrap_or_else(|error| panic!("{durable} on {stream} must exist: {error}"));
    let info = consumer.info().await.expect("consumer info").clone();
    (
        serde_json::to_value(&info.config).unwrap(),
        format!("{:?}", info.created),
        info.num_pending,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_five_stream_plane_upgrades_in_place_to_six_streams_and_the_module_durables() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let bin = match nats_server_bin() {
        Ok((bin, version)) => {
            eprintln!("nats-server {version}");
            bin
        }
        Err((gate, observation)) => {
            RowReport::skipped(Row::InstallBootstrap, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&signer_vocabulary());
            return;
        }
    };
    let trust = TrustChain::generate();
    let dir = tempfile::tempdir().unwrap();
    let server = BusServer::start(&bin, &dir.path().join("nats"), &trust, LOOPBACK).await;
    let credentials = Arc::new(Credentials::new(Arc::new(InProcessVault {
        signer: trust.signer.clone(),
    })));
    let deps = BootDeps {
        credentials: credentials.clone(),
        grants: Arc::new(grants::GrantSeam),
        store: Store::new(dir.path().join("store")),
        incarnation: "upgrade".to_string(),
        own_spawn: Arc::new(FixedOwnSpawn),
    };
    let config = Ok(BrokerConfig {
        url: server.url.clone(),
        operator_jwt_path: server.operator_jwt.clone(),
        system_account: server.system_account.clone(),
    });
    let machine_id = MachineId::parse(&fresh_machine_id()).unwrap();
    let names = AccountNames::derive(&format!("box_{}", machine_id.as_str())).unwrap();
    let streams = names.streams().clone();

    // The plane as the five-stream ck-bus left it.
    let earlier = boot_with(
        &machine_id,
        &config,
        &FiveStreamBroker(NatsBroker::new(server.url.clone(), credentials.clone())),
        &deps,
    )
    .await;
    let account_public = earlier.account.account_public.clone();
    drop(earlier);
    let client = bus::box_client(&trust, &server, &account_public).await;
    let js = jetstream::new(client);
    assert!(
        js.get_stream(&streams.event).await.is_err(),
        "the five-stream plane has no event stream"
    );
    let older: Vec<(String, String)> = vec![
        (
            streams.room.clone(),
            names.room_post("room_upgrade").unwrap(),
        ),
        (
            streams.wake.clone(),
            names.wake_fire("agent_upgrade").unwrap(),
        ),
        (
            streams.peer.clone(),
            names
                .peer_delivery("agent_upgrade", "sess_upgrade")
                .unwrap(),
        ),
        (
            streams.effect.clone(),
            names
                .effect_intent("agent_upgrade", "sess_upgrade")
                .unwrap(),
        ),
        (streams.effect_dead.clone(), names.effect_dead()),
    ];
    for (_, subject) in &older {
        js.publish(subject.clone(), "before the upgrade".into())
            .await
            .expect("publish")
            .await
            .unwrap_or_else(|error| panic!("{subject} is stored: {error}"));
    }
    let mut before = Vec::new();
    for (stream, _) in &older {
        let state = stream_state(&js, stream).await;
        assert_eq!(state.2, 1, "{stream} holds the message published to it");
        before.push(state);
    }

    // The current ck-bus boots on that plane.
    let broker = NatsBroker::new(server.url.clone(), credentials.clone());
    drop(boot_with(&machine_id, &config, &broker, &deps).await);
    for ((stream, _), earlier) in older.iter().zip(&before) {
        assert_eq!(
            &stream_state(&js, stream).await,
            earlier,
            "{stream} is left exactly as the five-stream ck-bus created it: same \
             configuration, same creation time, same message count"
        );
    }
    let (event_config, _, _) = stream_state(&js, &streams.event).await;
    assert_eq!(
        event_config["subjects"],
        serde_json::json!([names.event_binding()])
    );
    assert_eq!(event_config["max_msgs_per_subject"], EVENT_PER_SUBJECT_CAP);
    assert_eq!(event_config["max_bytes"], GIB);
    assert_eq!(event_config["discard"], "old");
    assert_eq!(event_config["retention"], "limits");
    assert_eq!(
        event_config["max_age"],
        Duration::from_secs(7 * 24 * 3600).as_nanos() as u64
    );

    let module_durables = [
        (streams.event.clone(), "m_basal", names.event_binding()),
        (
            streams.room.clone(),
            "m_prefrontal-core",
            names.room_binding(),
        ),
    ];
    for (stream, durable, filter) in &module_durables {
        let (config, _, _) = consumer_state(&js, stream, durable).await;
        let filters = match config.get("filter_subjects") {
            Some(Value::Array(filters)) if !filters.is_empty() => Value::Array(filters.clone()),
            _ => serde_json::json!([config["filter_subject"]]),
        };
        assert_eq!(filters, serde_json::json!([filter]), "{durable}'s filter");
        assert_eq!(config["durable_name"], *durable);
        assert_eq!(config["ack_policy"], "explicit", "{durable}");
        assert_eq!(config["deliver_policy"], "all", "{durable}");
        assert_eq!(config["ack_wait"], 30_000_000_000_u64, "{durable}");
        assert_eq!(config["max_deliver"], -1, "{durable}");
        assert_eq!(config["max_ack_pending"], 1000, "{durable}");
    }
    // The room post stored before `m_prefrontal-core` existed is waiting for it.
    let (_, _, room_pending) = consumer_state(&js, &streams.room, "m_prefrontal-core").await;
    assert_eq!(
        room_pending, 1,
        "a post made before the durable existed is kept"
    );
    js.publish(
        names.event_subject("someone", "thing_done", 1).unwrap(),
        "an event before basal first connects".into(),
    )
    .await
    .expect("publish")
    .await
    .expect("the event is stored");

    // Booting again changes nothing: every stream and both durables stay as they are.
    let mut six = older
        .iter()
        .map(|(stream, _)| stream.clone())
        .collect::<Vec<_>>();
    six.push(streams.event.clone());
    let mut streams_before = Vec::new();
    for stream in &six {
        streams_before.push(stream_state(&js, stream).await);
    }
    let mut durables_before = Vec::new();
    for (stream, durable, _) in &module_durables {
        durables_before.push(consumer_state(&js, stream, durable).await);
    }
    assert_eq!(durables_before[0].2, 1, "m_basal holds the event for basal");
    drop(boot_with(&machine_id, &config, &broker, &deps).await);
    for (stream, earlier) in six.iter().zip(&streams_before) {
        assert_eq!(
            &stream_state(&js, stream).await,
            earlier,
            "{stream} is unchanged by a further boot"
        );
    }
    for ((stream, durable, _), earlier) in module_durables.iter().zip(&durables_before) {
        assert_eq!(
            &consumer_state(&js, stream, durable).await,
            earlier,
            "{durable} is unchanged by a further boot"
        );
    }

    server.stop().await;
    passed();
}
