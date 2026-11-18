//! Upgrading a plane that ck-bus 0.1.29 built with five streams to the current eleven
//! streams and module durables, in place. This is the upgrade arm of the "Install
//! bootstrap and own users" acceptance row in `docs/ck-bus.md`. The vault
//! is the harness signer answered in process, the broker a real `nats-server`; no subc
//! daemon is started.
//!
//! - Older wire configurations remain pinned in `EARLIER_STREAM_CONFIGS`. EFFECT's age
//!   and message cap and the hand-off destination policies change.
//! - Live: boot once as the five-stream ck-bus did (newer streams and module durables
//!   withheld), store one message on each stream, then boot the current ck-bus.
//!   The five streams retain their creation times and messages, EFFECT updates in
//!   place, the new streams exist with their limits, and
//!   `m_basal` and `m_prefrontal` exist with the shipped durable configuration. The
//!   room post published before `m_prefrontal` existed is pending on it. A further
//!   boot leaves all eleven streams and all module durables unchanged. Further cases cover
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

#[path = "stream_upgrade/prefrontal_rename.rs"]
mod prefrontal_rename;

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
/// `GOLDEN_ACCOUNT`. Changed limits must use an in-place update, not another create.
const EARLIER_STREAM_CONFIGS: [&str; 5] = [
    r#"{"name":"CK_BOX_UPGRADEGOLDEN_ROOM","max_bytes":1073741824,"max_msgs":0,"max_msgs_per_subject":0,"discard":"old","subjects":["ck.box_upgradegolden.room.*.post"],"retention":"limits","max_consumers":0,"max_age":86400000000000,"storage":"file","num_replicas":1,"duplicate_window":120000000000,"consumer_limits":null}"#,
    r#"{"name":"CK_BOX_UPGRADEGOLDEN_WAKE","max_bytes":1073741824,"max_msgs":0,"max_msgs_per_subject":0,"discard":"old","subjects":["ck.box_upgradegolden.wake.*.fire"],"retention":"limits","max_consumers":0,"max_age":86400000000000,"storage":"file","num_replicas":1,"duplicate_window":120000000000,"consumer_limits":null}"#,
    r#"{"name":"CK_BOX_UPGRADEGOLDEN_PEER","max_bytes":1073741824,"max_msgs":0,"max_msgs_per_subject":0,"discard":"old","subjects":["ck.box_upgradegolden.peer.*.*.deliver"],"retention":"limits","max_consumers":0,"max_age":86400000000000,"storage":"file","num_replicas":1,"duplicate_window":120000000000,"consumer_limits":null}"#,
    r#"{"name":"CK_BOX_UPGRADEGOLDEN_EFFECT","max_bytes":268435456,"max_msgs":0,"max_msgs_per_subject":0,"discard":"new","subjects":["ck.box_upgradegolden.effect.*.*.intent"],"retention":"workqueue","max_consumers":0,"max_age":86400000000000,"storage":"file","num_replicas":1,"duplicate_window":120000000000,"consumer_limits":null}"#,
    r#"{"name":"CK_BOX_UPGRADEGOLDEN_EFFECT_DEAD","max_bytes":67108864,"max_msgs":0,"max_msgs_per_subject":0,"discard":"old","subjects":["ck.box_upgradegolden.effect.dead"],"retention":"limits","max_consumers":0,"max_age":604800000000000,"storage":"file","num_replicas":1,"duplicate_window":120000000000,"consumer_limits":null}"#,
];

const EVENT_PER_SUBJECT_CAP: i64 = 100_000;

fn module_plane_fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/module_plane.json")
}

fn module_plane_document() -> Value {
    let names = AccountNames::derive(GOLDEN_ACCOUNT).unwrap();
    let durables = bootstrap::module_durables::planned(&names).unwrap().into_iter()
        .map(|row| json!({
            "stream": row.stream, "durable": row.durable, "filter_subjects": row.filter_subjects,
            "ack_wait_secs": row.ack_wait.as_secs(), "max_deliver": row.max_deliver,
            "max_ack_pending": row.max_ack_pending,
        })).collect::<Vec<_>>();
    let buckets = cortexkit_bus_naming::MODULE_BUCKETS
        .iter()
        .map(|row| bootstrap::plane::module_bucket_config(&names, row).unwrap())
        .collect::<Vec<_>>();
    json!({"account": GOLDEN_ACCOUNT, "durables": durables, "buckets": buckets})
}

/// Server expectations come from the checked-in fixture, not a fresh planner run.
fn expected_module_durables(names: &AccountNames) -> Vec<(String, String, Vec<String>)> {
    let template = std::fs::read_to_string(module_plane_fixture_path())
        .unwrap()
        .replace(GOLDEN_ACCOUNT, names.account())
        .replace(
            &GOLDEN_ACCOUNT.to_ascii_uppercase(),
            &names.account().to_ascii_uppercase(),
        );
    let document: Value = serde_json::from_str(&template).unwrap();
    document["durables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["stream"].as_str().unwrap().to_owned(),
                row["durable"].as_str().unwrap().to_owned(),
                serde_json::from_value(row["filter_subjects"].clone()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn module_plane_plan_matches_regenerated_fixture() {
    let pinned: Value =
        serde_json::from_slice(&std::fs::read(module_plane_fixture_path()).unwrap()).unwrap();
    assert_eq!(module_plane_document(), pinned);
}

/// Run explicitly after a reviewed naming-table update; ordinary suites never write fixtures.
#[test]
#[ignore = "explicit fixture regeneration only"]
fn regenerate_module_plane_fixture() {
    std::fs::write(
        module_plane_fixture_path(),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&module_plane_document()).unwrap()
        ),
    )
    .unwrap();
}

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
fn older_stream_wire_configs_change_only_the_reviewed_delivery_policies() {
    let names = AccountNames::derive(GOLDEN_ACCOUNT).unwrap();
    let specs = shipped_streams(&names);
    let older: Vec<&StreamSpec> = specs
        .iter()
        .filter(|spec| {
            matches!(
                spec.kind,
                StreamKind::Room
                    | StreamKind::Wake
                    | StreamKind::Peer
                    | StreamKind::Effect
                    | StreamKind::EffectDead
            )
        })
        .collect();
    assert_eq!(older.len(), EARLIER_STREAM_CONFIGS.len());
    for (spec, earlier) in older.into_iter().zip(EARLIER_STREAM_CONFIGS) {
        let emitted = serde_json::to_value(bootstrap::plane::stream_config(spec)).unwrap();
        let mut expected: Value = serde_json::from_str(earlier).unwrap();
        if spec.kind != StreamKind::EffectDead {
            expected.as_object_mut().unwrap().remove("duplicate_window");
        }
        if spec.kind == StreamKind::Effect {
            expected["max_age"] = json!(0);
            expected["max_msg_size"] = json!(16 * 1024);
        }
        apply_expected_handoff_policy(&mut expected, GOLDEN_ACCOUNT, spec.kind);
        assert_eq!(
            emitted, expected,
            "{} changes only reviewed limits and optional-property omission",
            spec.name
        );
    }
}

fn apply_expected_handoff_policy(config: &mut Value, acct: &str, kind: StreamKind) {
    let extra: &[&str] = match kind {
        StreamKind::Room => &["room.fed.*.*.post"],
        StreamKind::Wake => &["wake.fed.*.*.handoff"],
        StreamKind::Peer => &["peer.fed.*.*.handoff", "peer.fed.*.custody"],
        _ => return,
    };
    config["discard"] = json!("new");
    config["max_age"] = json!(604_800_000_000_000_u64);
    config["subjects"].as_array_mut().unwrap().extend(
        extra
            .iter()
            .map(|suffix| json!(format!("ck.{acct}.{suffix}"))),
    );
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
    assert!(emitted.get("duplicate_window").is_none());
}

/// Whether a NATS binding can capture any descendant of the account's live root.
/// Check namespace overlap rather than a single example kind: an unrelated future
/// module or kind must not become stored just because a test did not name it.
fn may_capture_live(pattern: &str, account: &str) -> bool {
    let mut tokens = pattern.split('.');
    for fixed in ["ck", account, "live"] {
        match tokens.next() {
            Some(">") => return true,
            Some(token) if token == "*" || token == fixed => {}
            _ => return false,
        }
    }
    tokens.next().is_some()
}

#[test]
fn no_configured_stream_can_store_live_hints() {
    let names = AccountNames::derive("box_livefixture").unwrap();
    for pattern in [
        ">",
        "ck.>",
        "ck.*.>",
        "ck.box_livefixture.>",
        "ck.box_livefixture.*.>",
        "ck.box_livefixture.live.>",
        "ck.box_livefixture.live.other.future.kind.v9",
    ] {
        assert!(may_capture_live(pattern, names.account()), "{pattern}");
    }
    let mut configs = shipped_streams(&names)
        .iter()
        .map(bootstrap::plane::stream_config)
        .collect::<Vec<_>>();
    configs.push(bootstrap::plane::census_config(&names));
    for config in configs {
        for subject in config.subjects {
            assert!(
                !may_capture_live(&subject, names.account()),
                "stream {} would store live hints through {subject}",
                config.name
            );
        }
    }
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
    async fn delete_module_bucket(
        &self,
        _account: &AccountNames,
        _owner: &str,
        _name: &str,
    ) -> Result<bool, PlaneError> {
        Ok(false)
    }
    async fn ensure_module_bucket(
        &self,
        _account: &AccountNames,
        _bucket: &cortexkit_bus_naming::ModuleBucket,
    ) -> Result<(), PlaneError> {
        Ok(())
    }
    async fn ensure_census(&self, account: &AccountNames) -> Result<(), PlaneError> {
        self.0.ensure_census(account).await
    }
    async fn ensure_stream(&self, spec: &StreamSpec) -> Result<(), PlaneError> {
        if !matches!(
            spec.kind,
            StreamKind::Room
                | StreamKind::Wake
                | StreamKind::Peer
                | StreamKind::Effect
                | StreamKind::EffectDead
        ) {
            return Ok(());
        }
        let mut earlier = spec.clone();
        earlier.duplicate_window = Some(Duration::from_secs(120));
        if earlier.kind == StreamKind::Effect {
            earlier.max_age = Duration::from_secs(24 * 3600);
            earlier.max_message_size = None;
        }
        if matches!(
            earlier.kind,
            StreamKind::Room | StreamKind::Wake | StreamKind::Peer
        ) {
            earlier.subjects.truncate(1);
            earlier.discard = cortexkit_bus_naming::DiscardPolicy::Old;
            earlier.max_age = Duration::from_secs(24 * 3600);
        }
        self.0.ensure_stream(&earlier).await
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
        if durable == "m_prefrontal-core" {
            // This wrapper models a build before delivery readers were provisioned.
            return Ok(None);
        }
        self.0.consumer_state(stream, durable).await
    }
    async fn delete_durable(&self, stream: &str, durable: &str) -> Result<bool, PlaneError> {
        self.0.delete_durable(stream, durable).await
    }
    async fn purge_subject(&self, stream: &str, filter_subject: &str) -> Result<u64, PlaneError> {
        self.0.purge_subject(stream, filter_subject).await
    }
    async fn consumer_names(&self, stream: &str) -> Result<Vec<String>, PlaneError> {
        let _ = stream;
        Ok(vec![])
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
async fn stream_upgrade_keeps_old_room_wake_and_peer_messages_and_is_idempotent() {
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
    let ready = boot_with(&machine_id, &config, &broker, &deps).await;
    bootstrap::prefrontal_rename::reconcile(&ready, ["prefrontal"])
        .await
        .unwrap();
    for ((stream, _), earlier) in older.iter().zip(&before) {
        let mut expected = earlier.clone();
        let kind = match stream {
            name if name == &streams.room => StreamKind::Room,
            name if name == &streams.wake => StreamKind::Wake,
            name if name == &streams.peer => StreamKind::Peer,
            _ => StreamKind::Effect,
        };
        if kind != StreamKind::Effect {
            assert_eq!(expected.0["discard"], "old");
            assert_eq!(expected.0["max_age"], 86_400_000_000_000_u64);
            assert_eq!(expected.0["subjects"].as_array().unwrap().len(), 1);
            apply_expected_handoff_policy(&mut expected.0, names.account(), kind);
        }
        if stream == &streams.effect {
            assert_eq!(expected.0["max_age"], 86_400_000_000_000_u64);
            assert_eq!(expected.0["max_msg_size"], -1);
            expected.0["max_age"] = json!(0);
            expected.0["max_msg_size"] = json!(16 * 1024);
        }
        assert_eq!(
            &stream_state(&js, stream).await,
            &expected,
            "{stream} retains its creation time, messages and all non-upgraded settings"
        );
        let message = js
            .get_stream(stream)
            .await
            .unwrap()
            .get_raw_message(1)
            .await
            .unwrap();
        assert_eq!(message.payload.as_ref(), b"before the upgrade");
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

    for stream in [&streams.inbox, &streams.core_work] {
        let (config, _, count) = stream_state(&js, stream).await;
        assert_eq!(config["retention"], "workqueue");
        assert_eq!(config["discard"], "new");
        assert_eq!(config["max_age"], 0);
        assert_eq!(config["max_bytes"], 256 * 1024 * 1024);
        assert_eq!(config["max_msg_size"], 16 * 1024);
        assert_eq!(count, 0);
    }
    let (dead_config, _, _) = stream_state(&js, &streams.delivery_dead).await;
    assert_eq!(
        dead_config["subjects"],
        json!([format!("ck.{}.delivery.dead", names.account())])
    );
    assert_eq!(dead_config["retention"], "limits");
    assert_eq!(dead_config["discard"], "old");
    assert_eq!(dead_config["max_age"], 604_800_000_000_000_u64);
    assert_eq!(dead_config["max_bytes"], 64 * 1024 * 1024);
    assert_eq!(dead_config["duplicate_window"], 120_000_000_000_u64);

    let module_durables = expected_module_durables(&names);
    for (stream, durable, filter) in &module_durables {
        let (config, _, _) = consumer_state(&js, stream, durable).await;
        let filters = match config.get("filter_subjects") {
            Some(Value::Array(filters)) if !filters.is_empty() => Value::Array(filters.clone()),
            _ => serde_json::json!([config["filter_subject"]]),
        };
        assert_eq!(filters, serde_json::json!(filter), "{durable}'s filter");
        assert_eq!(config["durable_name"], *durable);
        assert_eq!(config["ack_policy"], "explicit", "{durable}");
        assert_eq!(config["deliver_policy"], "all", "{durable}");
        assert_eq!(config["ack_wait"], 30_000_000_000_u64, "{durable}");
        assert_eq!(config["max_deliver"], -1, "{durable}");
        let pending = if stream == &streams.inbox || stream == &streams.core_work {
            1
        } else {
            1000
        };
        assert_eq!(config["max_ack_pending"], pending, "{durable}");
        assert!(config.get("deliver_subject").is_none(), "{durable} is pull");
    }
    // The room post stored before `m_prefrontal` existed is waiting for it.
    let (_, _, room_pending) = consumer_state(&js, &streams.room, "m_prefrontal").await;
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
    let all = streams
        .all()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(all.len(), 11);
    let mut streams_before = Vec::new();
    for stream in &all {
        streams_before.push(stream_state(&js, stream).await);
    }
    let mut durables_before = Vec::new();
    for (stream, durable, _) in &module_durables {
        durables_before.push(consumer_state(&js, stream, durable).await);
    }
    assert_eq!(durables_before[0].2, 1, "m_basal holds the event for basal");
    let ready = boot_with(&machine_id, &config, &broker, &deps).await;
    bootstrap::prefrontal_rename::reconcile(&ready, ["prefrontal"])
        .await
        .unwrap();
    for (stream, earlier) in all.iter().zip(&streams_before) {
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
    async fn module_client(&self, module: &str) -> VerdictClient {
        let credentials = self.deps.credentials.clone();
        let user = credentials.custody.generate_user();
        let grant = grants::issued_grant(&self.names, module, &user, &[]).unwrap();
        let now = harness::signer::nats::unix_now();
        let jwt = sign_user_jwt(
            credentials.vault.as_ref(),
            &credentials.key_ids,
            &UserJwtRequest {
                root_credential_id: &bus::box_root_id(),
                user_public: &user,
                issuer_account: Some(&self.account_public),
                name: "module-bucket-conformance",
                issued_at: now,
                expires_at: now + 900,
                grant: &grant,
            },
        )
        .await
        .unwrap();
        let signer_user = user.clone();
        VerdictClient::connect(
            &self.server.url,
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
        .unwrap()
    }

    /// Starts the plane, or records the crate's usual `nats-server-absent` skip and
    /// returns `None` when no server binary is available (the Windows CI leg has none).
    async fn start() -> Option<Self> {
        let (bin, version) = match nats_server_bin() {
            Ok(found) => found,
            Err((gate, observation)) => {
                RowReport::skipped(Row::InstallBootstrap, gate, observation)
                    .served_by(ServedBy::HarnessSigner)
                    .emit(&signer_vocabulary());
                return None;
            }
        };
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
        Some(Self {
            server,
            _dir: dir,
            deps,
            config,
            machine_id,
            names,
            account_public,
            broker,
            js,
        })
    }

    async fn bootstrap(&self) -> Ready {
        boot_with(&self.machine_id, &self.config, &self.broker, &self.deps).await
    }

    async fn bootstrap_with_owner(&self, owner: &str) -> Ready {
        let ready = self.bootstrap().await;
        bootstrap::prefrontal_rename::reconcile(&ready, [owner])
            .await
            .unwrap();
        ready
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
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
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
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    drop(plane.bootstrap().await);
    let readers = expected_module_durables(&plane.names)
        .into_iter()
        .filter(|(stream, durable, _)| {
            stream == &plane.names.streams().event && durable != "m_basal"
        })
        .map(|(_, durable, filters)| (durable, filters))
        .collect::<Vec<_>>();
    assert_eq!(
        readers.len(),
        cortexkit_bus_naming::EVENT_CONSUMERS.len(),
        "the startup expectations cover every event-consumer table entry"
    );
    for (durable, expected_filters) in readers {
        let info = plane.consumer(&durable).await.get_info().await.unwrap();
        let config = info.config;
        let filters = if config.filter_subjects.is_empty() {
            vec![config.filter_subject]
        } else {
            config.filter_subjects
        };
        assert_eq!(filters, expected_filters,);
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
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    drop(plane.bootstrap().await);
    plane.store_event("broca", "run.started").await;
    plane.store_event("insula", "signal").await;
    plane
        .store_event("thalamus-harness", "compaction.started")
        .await;
    plane.store_event("broca", "compaction.started").await;
    plane
        .store_event("magic-context", "compaction.started")
        .await;
    plane.store_event("thalamus-harness", "turn_ended").await;
    let durables = [
        "m_basal",
        "m_prefrontal",
        "m_astrocyte",
        "m_striatum",
        "m_magic-context",
        "m_thalamus-harness",
        "m_thalamus",
        "m_broca",
        "m_cerebellum",
    ];
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
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    drop(plane.bootstrap().await);
    let sequence = plane.store_event("broca", "run.started").await;
    assert_eq!(plane.ack_one("m_prefrontal").await, sequence);
    let before = plane.cursor("m_prefrontal").await;
    let mut old = plane
        .consumer("m_prefrontal")
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
    assert_eq!(plane.cursor("m_prefrontal").await, before);
    plane.server.stop().await;
}

#[tokio::test]
async fn bootstrap_removes_striatum_step_filters_preserving_delivered_position() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    drop(plane.bootstrap().await);
    let durable = "m_striatum";
    let mut old = plane.consumer(durable).await.cached_info().config.clone();
    old.filter_subject.clear();
    old.filter_subjects = vec![
        format!("ck.{}.event.insula.>", plane.names.account()),
        format!("ck.{}.event.broca.step.finished.v1", plane.names.account()),
        format!(
            "ck.{}.event.thalamus-harness.step.finished.v1",
            plane.names.account()
        ),
    ];
    plane
        .js
        .update_consumer_on_stream(old, &plane.names.streams().event)
        .await
        .unwrap();
    let sequence = plane.store_event("insula", "signal").await;
    assert_eq!(plane.ack_one(durable).await, sequence);
    plane.store_event("broca", "step.finished").await;
    plane.store_event("thalamus-harness", "step.finished").await;
    let before = plane.cursor(durable).await;
    assert_eq!(before["num_pending"], 2);

    drop(plane.bootstrap().await);
    let after = plane.cursor(durable).await;
    assert_eq!(
        plane
            .consumer(durable)
            .await
            .get_info()
            .await
            .unwrap()
            .config
            .filter_subjects,
        [
            "insula.>",
            "broca.route_fact.v1",
            "thalamus-harness.route_fact.v1",
            "colliculus.route_fact.v1",
            "prefrontal-core.route_fact.v1",
            "prefrontal.route_fact.v1",
            "plexus.route_fact.v1",
            "fusiform.catalog_changed.v1",
        ]
        .map(|filter| format!("ck.{}.event.{filter}", plane.names.account()))
        .to_vec()
    );
    for field in ["created", "ack_floor", "delivered", "num_ack_pending"] {
        assert_eq!(after[field], before[field], "{durable} must keep {field}");
    }
    assert_eq!(after["num_pending"], 0);
    plane.store_event("broca", "step.finished").await;
    plane.store_event("thalamus-harness", "step.finished").await;
    assert_eq!(plane.cursor(durable).await["num_pending"], 0);
    let quota = plane.store_event("insula", "quota_sampled").await;
    assert_eq!(plane.ack_one(durable).await, quota);
    plane.server.stop().await;
}

#[tokio::test]
async fn bootstrap_refuses_immutable_event_config_without_deleting_an_acked_durable() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    drop(plane.bootstrap().await);
    plane
        .js
        .delete_consumer_from_stream("m_prefrontal", &plane.names.streams().event)
        .await
        .unwrap();
    plane
        .js
        .create_consumer_on_stream(
            pull::Config {
                durable_name: Some("m_prefrontal".to_string()),
                filter_subject: format!("ck.{}.event.broca.>", plane.names.account()),
                ack_policy: jetstream::consumer::AckPolicy::All,
                ..Default::default()
            },
            &plane.names.streams().event,
        )
        .await
        .unwrap();
    plane.store_event("broca", "run.started").await;
    assert!(plane.ack_one("m_prefrontal").await > 0);
    let before = plane.cursor("m_prefrontal").await;
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
        failure.message.contains("m_prefrontal"),
        "{}",
        failure.message
    );
    assert_eq!(plane.cursor("m_prefrontal").await, before);
    plane.server.stop().await;
}

#[tokio::test]
async fn prefrontal_event_grant_pulls_and_acks_seven_token_events_but_refuses_other_durables_and_create(
) {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
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
        .get_consumer_from_stream("m_prefrontal", &plane.names.streams().event)
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
                "$JS.API.CONSUMER.{verb}.{}.m_prefrontal",
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
    for durable in [
        "m_basal",
        "m_astrocyte",
        "m_striatum",
        "m_magic-context",
        "m_thalamus-harness",
        "m_thalamus",
        "m_thalamus-pi",
        "m_broca",
        "m_cerebellum",
    ] {
        for verb in ["INFO", "MSG.NEXT"] {
            let subject = format!(
                "$JS.API.CONSUMER.{verb}.{}.{durable}",
                plane.names.streams().event
            );
            client.publish(&subject, b"{}").await;
            client.expect_denied(&subject).await;
        }
        let subject = ack.replace(".m_prefrontal.", &format!(".{durable}."));
        assert_ne!(subject, ack);
        client.publish(&subject, b"").await;
        client.expect_denied(&subject).await;
    }
    for subject in [
        format!("$JS.API.CONSUMER.CREATE.{}", plane.names.streams().event),
        format!(
            "$JS.API.CONSUMER.CREATE.{}.m_prefrontal",
            plane.names.streams().event
        ),
        format!(
            "$JS.API.CONSUMER.DURABLE.CREATE.{}.m_prefrontal",
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
        // basal's flow-engine durable plus one per event-consumer table entry
        1 + cortexkit_bus_naming::EVENT_CONSUMERS.len(),
        "a denied create added no consumer"
    );
    plane.server.stop().await;
}

#[tokio::test]
async fn only_the_named_authority_can_info_pull_and_ack_each_federation_reader() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    drop(plane.bootstrap().await);
    let unrelated = plane.module_client("broca").await;
    let current = plane.module_client("prefrontal").await;
    let legacy = plane.module_client("prefrontal-core").await;
    for (module, owner, other) in [
        ("prefrontal", &current, &legacy),
        ("prefrontal-core", &legacy, &current),
    ] {
        let durable = AccountNames::module_consumer_name(module).unwrap();
        for stream in [&plane.names.streams().peer, &plane.names.streams().wake] {
            for verb in ["INFO", "MSG.NEXT"] {
                let subject = format!("$JS.API.CONSUMER.{verb}.{stream}.{durable}");
                for refused in [&unrelated, other] {
                    refused.publish(&subject, b"{}").await;
                    refused.expect_denied(&subject).await;
                }
            }
            let js = jetstream::new(owner.client.clone());
            let reader: jetstream::consumer::Consumer<pull::Config> =
                js.get_consumer_from_stream(&durable, stream).await.unwrap();
            let info = reader.get_info().await.unwrap();
            let filter = info
                .config
                .filter_subjects
                .first()
                .unwrap_or(&info.config.filter_subject);
            let subject = filter.replace(['*', '>'], "probe");
            plane
                .js
                .publish(subject, "handoff".into())
                .await
                .unwrap()
                .await
                .unwrap();
            let delivered = reader
                .fetch()
                .max_messages(1)
                .expires(Duration::from_secs(2))
                .messages()
                .await
                .unwrap()
                .next()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(delivered.payload.as_ref(), b"handoff");
            let ack = delivered.reply.as_ref().unwrap().as_str();
            for refused in [&unrelated, other] {
                refused.publish(ack, b"+ACK").await;
                refused.expect_denied(ack).await;
            }
            delivered.double_ack().await.unwrap();
            owner.expect_allowed(ack).await;
            assert_eq!(reader.get_info().await.unwrap().num_ack_pending, 0);
        }
    }
    plane.server.stop().await;
}

#[tokio::test]
async fn module_buckets_owner_put_delete_and_declared_reader_get_watch() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    drop(plane.bootstrap_with_owner("prefrontal").await);
    // Exercise every declaration, including both participant and authority readers.
    for row in cortexkit_bus_naming::MODULE_BUCKETS {
        let bucket = plane.names.module_bucket_name(row.owner, row.name).unwrap();
        let owner = plane.module_client(row.owner).await;
        let store = jetstream::new(owner.client.clone())
            .get_key_value(&bucket)
            .await
            .unwrap();
        for reader_module in row.readers {
            let reader = plane.module_client(reader_module).await;
            let read_store = jetstream::new(reader.client.clone())
                .get_key_value(&bucket)
                .await
                .unwrap();
            let mut watch = read_store
                .watch("key")
                .await
                .expect("declared reader may watch");
            let revision = store
                .put("key", "value".into())
                .await
                .expect("owner may put");
            assert_eq!(
                read_store.get("key").await.unwrap().unwrap().as_ref(),
                b"value"
            );
            let entry = tokio::time::timeout(Duration::from_secs(3), watch.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(entry.revision, revision);
            assert_eq!(entry.value.as_ref(), b"value");
            store.delete("key").await.expect("owner may delete");
            let deleted = tokio::time::timeout(Duration::from_secs(3), watch.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(deleted.operation, jetstream::kv::Operation::Delete);
            assert!(read_store.get("key").await.unwrap().is_none());
        }
    }
    plane.server.stop().await;
}

#[tokio::test]
async fn module_buckets_refuse_nonreaders_foreign_writers_and_reader_administration() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    drop(plane.bootstrap().await);
    let bucket = plane
        .names
        .module_bucket_name("astrocyte", "multiplier")
        .unwrap();
    let stream = format!("KV_{bucket}");
    for module in ["striatum", "aft", "unlisted"] {
        let client = plane.module_client(module).await;
        let value = format!("$KV.{bucket}.key");
        client.publish(&value, b"not the owner").await;
        client.expect_denied(&value).await;
        // A tombstone is the same forbidden publish, not a separate write API.
        let js = jetstream::new(client.client.clone());
        if module == "striatum" {
            let store = js.get_key_value(&bucket).await.unwrap();
            assert!(store.delete("key").await.is_err());
            client.expect_denied(&value).await;
        } else {
            for subject in [
                format!("$JS.API.STREAM.INFO.{stream}"),
                format!("$JS.API.DIRECT.GET.{stream}.$KV.{bucket}.key"),
                format!("$JS.API.STREAM.MSG.GET.{stream}"),
                format!("$JS.API.CONSUMER.CREATE.{stream}"),
            ] {
                client.publish(&subject, b"{}").await;
                client.expect_denied(&subject).await;
            }
        }
        for subject in [
            format!("$JS.API.STREAM.UPDATE.{stream}"),
            format!("$JS.API.STREAM.DELETE.{stream}"),
            format!("$JS.API.STREAM.PURGE.{stream}"),
            format!("$JS.API.CONSUMER.CREATE.{stream}.named"),
            format!("$JS.API.CONSUMER.DURABLE.CREATE.{stream}.named"),
            format!("$JS.API.CONSUMER.INFO.{stream}.named"),
            format!("$JS.API.CONSUMER.DELETE.{stream}.named"),
            format!("$JS.API.CONSUMER.NAMES.{stream}"),
        ] {
            client.publish(&subject, b"{}").await;
            client.expect_denied(&subject).await;
        }
        for stream in [
            &plane.names.streams().event,
            &plane.names.streams().room,
            &plane.names.streams().inbox,
            &plane.names.streams().core_work,
        ] {
            let subject = format!("$JS.API.CONSUMER.NAMES.{stream}");
            client.publish(&subject, b"{}").await;
            client.expect_denied(&subject).await;
        }
    }
    plane.server.stop().await;
}

#[tokio::test]
async fn module_buckets_unnamed_watch_api_rejects_durable_but_accepts_named_ephemeral_body() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    drop(plane.bootstrap().await);
    let bucket = plane
        .names
        .module_bucket_name("astrocyte", "multiplier")
        .unwrap();
    let stream = format!("KV_{bucket}");
    let reader = plane.module_client("striatum").await;
    for (field, name) in [
        ("durable_name", "crafted_durable"),
        ("name", "crafted_ephemeral"),
    ] {
        let mut config = json!({
            "deliver_subject": reader.client.new_inbox(),
            "filter_subject": format!("$KV.{bucket}.>"),
            "ack_policy": "none", "deliver_policy": "all", "max_deliver": 1,
            "inactive_threshold": 30_000_000_000_u64, "mem_storage": true,
        });
        config[field] = json!(name);
        let reply = reader
            .client
            .request(
                format!("$JS.API.CONSUMER.CREATE.{stream}"),
                serde_json::to_vec(&json!({"stream_name": stream, "config": config}))
                    .unwrap()
                    .into(),
            )
            .await
            .unwrap();
        let reply: Value = serde_json::from_slice(&reply.payload).unwrap();
        eprintln!("bare watch create with {field}: {reply}");
        if field == "durable_name" {
            assert_eq!(reply["error"]["err_code"], 10020);
        } else {
            assert!(reply.get("error").is_none(), "{reply}");
            assert_eq!(reply["name"], name);
        }
    }
    plane.server.stop().await;
}

#[tokio::test]
async fn module_buckets_reconcile_limits_in_place_and_refuse_immutable_storage() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    let ready = plane.bootstrap_with_owner("prefrontal").await;
    for row in cortexkit_bus_naming::MODULE_BUCKETS {
        let bucket = plane.names.module_bucket_name(row.owner, row.name).unwrap();
        let stream = format!("KV_{bucket}");
        let mut handle = plane.js.get_stream(&stream).await.unwrap();
        let original = handle.info().await.unwrap().clone();
        assert_eq!(original.config.max_message_size, row.max_value_bytes as i32);
        assert_eq!(
            original.config.max_messages_per_subject,
            i64::from(row.history)
        );
        assert_eq!(original.config.max_bytes, row.max_bytes as i64);
        assert_eq!(
            original.config.max_consumers as usize,
            8 * (1 + row.readers.len())
        );
        assert_eq!(
            original.config.max_age,
            row.ttl.map(Duration::from_secs).unwrap_or_default()
        );
        let store = plane.js.get_key_value(&bucket).await.unwrap();
        let revision = store.put("kept", "before".into()).await.unwrap();
        let mut altered = original.config.clone();
        altered.max_message_size *= 2;
        altered.max_bytes *= 2;
        altered.max_messages_per_subject = 2;
        altered.max_age = Duration::from_secs(60);
        altered.duplicate_window = Duration::from_secs(10);
        plane.js.update_stream(altered).await.unwrap();
        ready
            .box_plane
            .ensure_module_bucket(&plane.names, row)
            .await
            .unwrap();
        let current = handle.info().await.unwrap();
        assert_eq!(current.created, original.created);
        assert_eq!(
            current.config.max_message_size,
            original.config.max_message_size
        );
        assert_eq!(current.config.max_bytes, original.config.max_bytes);
        assert_eq!(
            current.config.max_messages_per_subject,
            original.config.max_messages_per_subject
        );
        assert_eq!(current.config.max_age, original.config.max_age);
        assert_eq!(
            store.entry("kept").await.unwrap().unwrap().revision,
            revision
        );
        ready
            .box_plane
            .ensure_module_bucket(&plane.names, row)
            .await
            .unwrap();
        assert_eq!(
            store.entry("kept").await.unwrap().unwrap().revision,
            revision
        );
    }
    let row = &cortexkit_bus_naming::MODULE_BUCKETS[1];
    let mut wrong = bootstrap::plane::module_bucket_config(&plane.names, row).unwrap();
    plane.js.delete_stream(&wrong.name).await.unwrap();
    wrong.storage = jetstream::stream::StorageType::Memory;
    plane.js.create_stream(wrong.clone()).await.unwrap();
    let before = stream_state(&plane.js, &wrong.name).await;
    let error = ready
        .box_plane
        .ensure_module_bucket(&plane.names, row)
        .await
        .unwrap_err();
    assert!(error.message.contains("module-bucket-immutable"), "{error}");
    assert!(error.message.contains("storage"), "{error}");
    assert_eq!(stream_state(&plane.js, &wrong.name).await, before);
    plane.server.stop().await;
}

#[tokio::test]
async fn module_buckets_consumer_cap_refuses_reader_growth_but_keeps_owner_watch_puts_and_reads() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    drop(plane.bootstrap().await);
    let bucket = plane
        .names
        .module_bucket_name("astrocyte", "multiplier")
        .unwrap();
    let stream = format!("KV_{bucket}");
    let owner = plane.module_client("astrocyte").await;
    let owner_store = jetstream::new(owner.client.clone())
        .get_key_value(&bucket)
        .await
        .unwrap();
    let mut owner_watch = owner_store.watch("key").await.unwrap();
    let reader = plane.module_client("striatum").await;
    let reader_js = jetstream::new(reader.client.clone());
    let mut handle = reader_js.get_stream(&stream).await.unwrap();

    // The owner holds one slot before the reader fills the remaining shared budget.
    for _ in 1..16 {
        let reply = reader
            .client
            .request(
                format!("$JS.API.CONSUMER.CREATE.{stream}"),
                serde_json::to_vec(&json!({"stream_name": stream, "config": {
                    "deliver_subject": reader.client.new_inbox(),
                    "filter_subject": format!("$KV.{bucket}.>"),
                    "ack_policy": "none", "deliver_policy": "new",
                    "inactive_threshold": 30_000_000_000_u64,
                }}))
                .unwrap()
                .into(),
            )
            .await
            .unwrap();
        let reply: Value = serde_json::from_slice(&reply.payload).unwrap();
        assert!(reply.get("error").is_none(), "{reply}");
    }
    assert_eq!(handle.info().await.unwrap().state.consumer_count, 16);
    for _ in 0..2 {
        let reply = reader
            .client
            .request(
                format!("$JS.API.CONSUMER.CREATE.{stream}"),
                serde_json::to_vec(&json!({"stream_name": stream, "config": {
                    "deliver_subject": reader.client.new_inbox(),
                    "filter_subject": format!("$KV.{bucket}.>"),
                    "ack_policy": "none", "deliver_policy": "new",
                    "inactive_threshold": 30_000_000_000_u64,
                }}))
                .unwrap()
                .into(),
            )
            .await
            .unwrap();
        let reply: Value = serde_json::from_slice(&reply.payload).unwrap();
        assert_eq!(reply["error"]["err_code"], 10026, "{reply}");
    }
    let revision = owner_store
        .put("key", "bounded".into())
        .await
        .expect("consumer saturation never prevents the owner's value writes");
    let update = tokio::time::timeout(Duration::from_secs(3), owner_watch.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(update.revision, revision);
    assert_eq!(update.value.as_ref(), b"bounded");
    let direct = reader
        .client
        .request(
            format!("$JS.API.DIRECT.GET.{stream}.$KV.{bucket}.key"),
            "".into(),
        )
        .await
        .unwrap();
    assert_eq!(direct.payload.as_ref(), b"bounded");
    let raw = handle
        .get_raw_message(revision)
        .await
        .expect("stream-message reads need no consumer slot");
    assert_eq!(raw.payload.as_ref(), b"bounded");
    assert_eq!(handle.info().await.unwrap().state.consumer_count, 16);
    let other_bucket = plane
        .names
        .module_bucket_name("astrocyte", "read-marks")
        .unwrap();
    let other = jetstream::new(owner.client.clone())
        .get_key_value(&other_bucket)
        .await
        .unwrap();
    let mut other_watch = other
        .watch("key")
        .await
        .expect("another bucket has its own watch budget");
    other.put("key", "independent".into()).await.unwrap();
    let update = tokio::time::timeout(Duration::from_secs(3), other_watch.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(update.value.as_ref(), b"independent");
    plane.server.stop().await;
}

#[tokio::test]
async fn module_buckets_reconcile_consumer_cap_in_place_without_recreating_keys() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    let row = &cortexkit_bus_naming::MODULE_BUCKETS[1];
    let desired = bootstrap::plane::module_bucket_config(&plane.names, row).unwrap();
    let mut unlimited = desired.clone();
    unlimited.max_consumers = -1;
    plane.js.create_stream(unlimited.clone()).await.unwrap();
    let bucket = plane.names.module_bucket_name(row.owner, row.name).unwrap();
    let store = plane.js.get_key_value(&bucket).await.unwrap();
    let revision = store.put("kept", "before".into()).await.unwrap();
    let before = stream_state(&plane.js, &desired.name).await;
    // Probe the server directly before checking bootstrap's reconciliation path.
    let updated = plane
        .js
        .update_stream(desired.clone())
        .await
        .expect("NATS 2.15.0 permits max_consumers updates in place");
    assert_eq!(updated.config.max_consumers, 16);
    let mut expected = before.clone();
    expected.0["max_consumers"] = json!(16);
    assert_eq!(stream_state(&plane.js, &desired.name).await, expected);
    plane.js.update_stream(unlimited).await.unwrap();
    assert_eq!(stream_state(&plane.js, &desired.name).await, before);
    let ready = plane.bootstrap().await;
    assert_eq!(ready.orphaned_module_durables, 0);
    assert_eq!(stream_state(&plane.js, &desired.name).await, expected);
    assert_eq!(
        store.entry("kept").await.unwrap().unwrap().revision,
        revision
    );
    plane.server.stop().await;
}

#[tokio::test]
async fn module_durable_plan_reports_orphans_without_deleting_their_cursor() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    drop(plane.bootstrap().await);
    let stream = &plane.names.streams().event;
    plane
        .js
        .create_consumer_on_stream(
            pull::Config {
                durable_name: Some("m_prefrontal-routing".into()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            },
            stream,
        )
        .await
        .unwrap();
    let sequence = plane.store_event("broca", "run").await;
    assert_eq!(plane.ack_one("m_prefrontal-routing").await, sequence);
    let before = plane.cursor("m_prefrontal-routing").await;
    let ready = plane.bootstrap().await;
    assert_eq!(ready.orphaned_module_durables, 1);
    let desired = bootstrap::module_durables::planned(&plane.names).unwrap();
    let output =
        bootstrap::module_durables::plan_output(&plane.names, ready.box_plane.as_ref(), &desired)
            .await
            .unwrap();
    assert_eq!(output.orphaned_module_durables, 1);
    assert!(output.rows.contains(&json!({
        "stream": stream, "durable": "m_prefrontal-routing", "state": "orphaned",
    })));
    assert!(desired.iter().any(|row| row.durable == "m_striatum"));
    assert_eq!(plane.cursor("m_prefrontal-routing").await, before);
    plane.server.stop().await;
}
