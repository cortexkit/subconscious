//! Upgrading a plane that ck-bus 0.1.29 built with five streams to the current six
//! streams and module durables, in place. This is the upgrade arm of the "Install
//! bootstrap and own users" acceptance row in `docs/ck-bus.md`. The vault
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
//!   boot leaves all six streams and all module durables unchanged. Further cases cover
//!   an existing event stream's cap and readers whose acknowledgement floors must survive.

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
    issue::{sign_user_jwt, UserJwtRequest},
    vault::{VaultError, VaultSigning},
    wire::{self, VaultPublicKey, VaultSignature},
    Credentials,
};
use futures_util::StreamExt;
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    issuance::VerdictClient,
    report::{Row, RowReport, ServedBy},
    signer::{nats::nats_server_bin, HarnessSigner, SIGNER_OPERATIONS},
};
use serde_json::{json, Value};
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

const EVENT_PER_SUBJECT_CAP: i64 = 100_000;

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
    cortexkit_bus_testkit::new_machine_id()
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
            streams.event.clone(),
            "m_prefrontal-core",
            format!("ck.{}.event.broca.>", names.account()),
        ),
        (
            streams.event.clone(),
            "m_astrocyte",
            format!("ck.{}.event.insula.>", names.account()),
        ),
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

    // Booting again changes nothing: every stream and durable stays as it is.
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

/// An actual private NATS account bootstrapped as the five-stream version left it.
/// Each test chooses the event stream and consumer state the upgraded bootstrap sees.
struct UpgradePlane {
    server: BusServer,
    _dir: tempfile::TempDir,
    deps: BootDeps,
    config: Result<BrokerConfig, String>,
    machine_id: MachineId,
    names: AccountNames,
    account_public: String,
    broker: NatsBroker,
    js: jetstream::Context,
}

impl UpgradePlane {
    async fn start() -> Self {
        let (bin, version) = nats_server_bin().expect("real-bus upgrade tests require nats-server");
        eprintln!("nats-server {version}");
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
            incarnation: "event-upgrade".to_string(),
            own_spawn: Arc::new(FixedOwnSpawn),
        };
        let config = Ok(BrokerConfig {
            url: server.url.clone(),
            operator_jwt_path: server.operator_jwt.clone(),
            system_account: server.system_account.clone(),
        });
        let machine_id = MachineId::parse(&fresh_machine_id()).unwrap();
        let names = AccountNames::derive(&format!("box_{}", machine_id.as_str())).unwrap();
        let ready = boot_with(
            &machine_id,
            &config,
            &FiveStreamBroker(NatsBroker::new(server.url.clone(), credentials.clone())),
            &deps,
        )
        .await;
        let account_public = ready.account.account_public.clone();
        let js = jetstream::new(bus::box_client(&trust, &server, &account_public).await);
        let broker = NatsBroker::new(server.url.clone(), credentials);
        Self {
            server,
            _dir: dir,
            deps,
            config,
            machine_id,
            names,
            account_public,
            broker,
            js,
        }
    }

    async fn bootstrap(&self) -> Ready {
        boot_with(&self.machine_id, &self.config, &self.broker, &self.deps).await
    }

    async fn store_event(&self, module: &str, event: &str) -> u64 {
        self.js
            .publish(
                self.names.event_subject(module, event, 1).unwrap(),
                "kept event".into(),
            )
            .await
            .unwrap()
            .await
            .unwrap()
            .sequence
    }

    async fn consumer(&self, durable: &str) -> jetstream::consumer::Consumer<pull::Config> {
        self.js
            .get_consumer_from_stream(durable, &self.names.streams().event)
            .await
            .unwrap_or_else(|error| panic!("{durable} must exist: {error}"))
    }

    async fn ack_one(&self, durable: &str) -> u64 {
        let consumer = self.consumer(durable).await;
        let message = consumer
            .fetch()
            .max_messages(1)
            .expires(Duration::from_secs(3))
            .messages()
            .await
            .unwrap()
            .next()
            .await
            .expect("an event arrives")
            .unwrap();
        let sequence = message.info().unwrap().stream_sequence;
        message.double_ack().await.unwrap();
        assert_eq!(
            consumer.get_info().await.unwrap().ack_floor.stream_sequence,
            sequence
        );
        sequence
    }

    async fn cursor(&self, durable: &str) -> Value {
        let info = self.consumer(durable).await.get_info().await.unwrap();
        json!({
            "created": info.created, "config": info.config,
            "ack_floor": [info.ack_floor.consumer_sequence, info.ack_floor.stream_sequence],
            "delivered": [info.delivered.consumer_sequence, info.delivered.stream_sequence],
            "num_ack_pending": info.num_ack_pending,
            "num_pending": info.num_pending,
        })
    }
}

#[tokio::test]
async fn bootstrap_raises_existing_event_cap_to_100000_without_losing_messages() {
    let _gate = harness::acceptance_gate().await;
    let plane = UpgradePlane::start().await;
    let spec = shipped_streams(&plane.names)
        .into_iter()
        .find(|spec| spec.kind == StreamKind::Event)
        .unwrap();
    let mut config = bootstrap::plane::stream_config(&spec);
    config.max_messages_per_subject = 10_000;
    plane.js.create_stream(config).await.unwrap();
    let sequence = plane.store_event("broca", "run.started").await;
    let before = stream_state(&plane.js, &spec.name).await;
    assert_eq!(before.0["max_msgs_per_subject"], 10_000);
    drop(plane.bootstrap().await);
    let after = stream_state(&plane.js, &spec.name).await;
    assert_eq!(after.0["max_msgs_per_subject"], 100_000);
    assert_eq!(after.1, before.1, "the stream was not recreated");
    assert_eq!(after.2, before.2, "messages survived the cap update");
    let message = plane
        .js
        .get_stream(&spec.name)
        .await
        .unwrap()
        .get_raw_message(sequence)
        .await
        .unwrap();
    assert_eq!(message.payload.as_ref(), b"kept event");
    plane.server.stop().await;
}

#[tokio::test]
async fn bootstrap_creates_every_table_event_durable_with_its_filter_and_settings() {
    let _gate = harness::acceptance_gate().await;
    let plane = UpgradePlane::start().await;
    drop(plane.bootstrap().await);
    for (durable, module) in [("m_prefrontal-core", "broca"), ("m_astrocyte", "insula")] {
        let info = plane.consumer(durable).await.get_info().await.unwrap();
        let config = info.config;
        let filters = if config.filter_subjects.is_empty() {
            vec![config.filter_subject]
        } else {
            config.filter_subjects
        };
        assert_eq!(
            filters,
            vec![format!("ck.{}.event.{module}.>", plane.names.account())]
        );
        assert!(config.deliver_subject.is_none(), "{durable} must be pull");
        assert_eq!(config.ack_policy, jetstream::consumer::AckPolicy::Explicit);
        assert_eq!(
            config.deliver_policy,
            jetstream::consumer::DeliverPolicy::All
        );
        assert_eq!(config.ack_wait, Duration::from_secs(30));
        assert_eq!(config.max_ack_pending, 1_000);
        assert_eq!(config.max_deliver, -1);
    }
    plane.server.stop().await;
}

#[tokio::test]
async fn second_bootstrap_keeps_event_durable_creation_times_and_ack_floors() {
    let _gate = harness::acceptance_gate().await;
    let plane = UpgradePlane::start().await;
    drop(plane.bootstrap().await);
    plane.store_event("broca", "run.started").await;
    plane.store_event("insula", "signal").await;
    let durables = ["m_basal", "m_prefrontal-core", "m_astrocyte"];
    let mut before = Vec::new();
    for durable in durables {
        assert!(plane.ack_one(durable).await > 0);
        before.push(plane.cursor(durable).await);
    }
    drop(plane.bootstrap().await);
    for (durable, before) in durables.into_iter().zip(before) {
        assert_eq!(
            plane.cursor(durable).await,
            before,
            "{durable} did not churn"
        );
    }
    plane.server.stop().await;
}

#[tokio::test]
async fn bootstrap_updates_event_filters_and_limits_in_place_preserving_ack_floors() {
    let _gate = harness::acceptance_gate().await;
    let plane = UpgradePlane::start().await;
    drop(plane.bootstrap().await);
    let sequence = plane.store_event("broca", "run.started").await;
    assert_eq!(plane.ack_one("m_prefrontal-core").await, sequence);
    let before = plane.cursor("m_prefrontal-core").await;
    let mut old = plane
        .consumer("m_prefrontal-core")
        .await
        .cached_info()
        .config
        .clone();
    old.filter_subject.clear();
    old.filter_subjects = vec![format!("ck.{}.event.>", plane.names.account())];
    old.ack_wait = Duration::from_secs(60);
    old.max_ack_pending = 5;
    plane
        .js
        .update_consumer_on_stream(old, &plane.names.streams().event)
        .await
        .unwrap();
    drop(plane.bootstrap().await);
    assert_eq!(plane.cursor("m_prefrontal-core").await, before);
    plane.server.stop().await;
}

#[tokio::test]
async fn bootstrap_refuses_immutable_event_config_without_deleting_an_acked_durable() {
    let _gate = harness::acceptance_gate().await;
    let plane = UpgradePlane::start().await;
    drop(plane.bootstrap().await);
    plane
        .js
        .delete_consumer_from_stream("m_prefrontal-core", &plane.names.streams().event)
        .await
        .unwrap();
    plane
        .js
        .create_consumer_on_stream(
            pull::Config {
                durable_name: Some("m_prefrontal-core".to_string()),
                filter_subject: format!("ck.{}.event.broca.>", plane.names.account()),
                ack_policy: jetstream::consumer::AckPolicy::All,
                ..Default::default()
            },
            &plane.names.streams().event,
        )
        .await
        .unwrap();
    plane.store_event("broca", "run.started").await;
    assert!(plane.ack_one("m_prefrontal-core").await > 0);
    let before = plane.cursor("m_prefrontal-core").await;
    let failure = match boot(
        Some(&plane.machine_id),
        BrokerInputs {
            config: &plane.config,
            broker: &plane.broker,
        },
        &plane.deps,
    )
    .await
    {
        Ok(_) => panic!("an immutable ack-policy mismatch must refuse bootstrap"),
        Err(failure) => failure,
    };
    assert_eq!(failure.cause, bootstrap::cause::MODULE_DURABLES_UNAVAILABLE);
    assert!(
        failure.message.contains("m_prefrontal-core"),
        "{}",
        failure.message
    );
    assert_eq!(plane.cursor("m_prefrontal-core").await, before);
    plane.server.stop().await;
}

#[tokio::test]
async fn prefrontal_event_grant_pulls_and_acks_seven_token_events_but_refuses_other_durables_and_create(
) {
    let _gate = harness::acceptance_gate().await;
    let plane = UpgradePlane::start().await;
    drop(plane.bootstrap().await);
    let credentials = plane.deps.credentials.clone();
    let user = credentials.custody.generate_user();
    let grant = grants::issued_grant(&plane.names, "prefrontal-core", &user, &[]).unwrap();
    let now = harness::signer::nats::unix_now();
    let jwt = sign_user_jwt(
        credentials.vault.as_ref(),
        &credentials.key_ids,
        &UserJwtRequest {
            root_credential_id: &bus::box_root_id(),
            user_public: &user,
            issuer_account: Some(&plane.account_public),
            name: "event-reader-conformance",
            issued_at: now,
            expires_at: now + 900,
            grant: &grant,
        },
    )
    .await
    .unwrap();
    let signer_user = user.clone();
    let client = VerdictClient::connect(
        &plane.server.url,
        &jwt.jwt,
        Arc::new(move |nonce| {
            let signed = credentials
                .custody
                .sign_nonce(&signer_user, &nonce)
                .map_err(|error| error.to_string());
            Box::pin(async move { signed })
        }),
        Some(format!("_INBOX.{user}")),
    )
    .await
    .unwrap();
    let subject = plane
        .names
        .event_subject("broca", "run.started", 1)
        .unwrap();
    assert_eq!(subject.split('.').count(), 7);
    let sequence = plane.store_event("broca", "run.started").await;
    let js = jetstream::new(client.client.clone());
    let consumer: jetstream::consumer::Consumer<pull::Config> = js
        .get_consumer_from_stream("m_prefrontal-core", &plane.names.streams().event)
        .await
        .expect("prefrontal-core may inspect its event durable");
    let message = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(3))
        .messages()
        .await
        .unwrap()
        .next()
        .await
        .expect("seven-token broca event arrives")
        .unwrap();
    assert_eq!(message.subject.as_str(), subject);
    let ack = message.reply.clone().unwrap().to_string();
    message.ack().await.unwrap();
    client.expect_allowed(&ack).await;
    for verb in ["INFO", "MSG.NEXT"] {
        client
            .expect_allowed(&format!(
                "$JS.API.CONSUMER.{verb}.{}.m_prefrontal-core",
                plane.names.streams().event
            ))
            .await;
    }
    assert_eq!(
        consumer.get_info().await.unwrap().ack_floor.stream_sequence,
        sequence
    );
    // Check each denied subject through the server's permission-violation callback,
    // not merely a timed-out request that could also mean an unavailable broker.
    for durable in ["m_basal", "m_astrocyte"] {
        for verb in ["INFO", "MSG.NEXT"] {
            let subject = format!(
                "$JS.API.CONSUMER.{verb}.{}.{durable}",
                plane.names.streams().event
            );
            client.publish(&subject, b"{}").await;
            client.expect_denied(&subject).await;
        }
        let subject = ack.replace(".m_prefrontal-core.", &format!(".{durable}."));
        assert_ne!(subject, ack);
        client.publish(&subject, b"").await;
        client.expect_denied(&subject).await;
    }
    for subject in [
        format!("$JS.API.CONSUMER.CREATE.{}", plane.names.streams().event),
        format!(
            "$JS.API.CONSUMER.CREATE.{}.m_prefrontal-core",
            plane.names.streams().event
        ),
        format!(
            "$JS.API.CONSUMER.DURABLE.CREATE.{}.m_prefrontal-core",
            plane.names.streams().event
        ),
    ] {
        client.publish(&subject, b"{}").await;
        client.expect_denied(&subject).await;
    }
    assert_eq!(
        plane
            .js
            .get_stream(&plane.names.streams().event)
            .await
            .unwrap()
            .consumer_names()
            .collect::<Vec<_>>()
            .await
            .len(),
        3,
        "a denied create added no consumer"
    );
    plane.server.stop().await;
}
