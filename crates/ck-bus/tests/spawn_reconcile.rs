//! Ladder row "Spawn reconciliation and census recovery" (slice 7 of
//! `docs/specs/ck-bus-module.md`), against a real nats-server, the acceptance daemon and
//! the supervised `ck-bus` binary.
//!
//! Serving side: harness-signer. Every JWT and revocation list the supervised ck-bus
//! signs is signed by the harness signer's fixture keys.
//!
//! Arms:
//! - An exit revokes the exited generation's credential by its fact: the revocation
//!   list carries the key, the census entry is gone, and the respawned generation has no
//!   entry until its first `ckbus.credential`, which issues it at epoch 0.
//! - A ck-bus that is down while a participant respawns (the subscription gap), with a
//!   census entry planted for a module that has no process: the restarted ck-bus resumes
//!   from its recorded cursor, reconciles, and revokes both the exited generation's
//!   credential and the planted one (whose live connection the server closes). After
//!   the refetch there is exactly one census identity for the live generation. A second
//!   restart while the participant keeps running revokes nothing it holds; its refetch
//!   then supersedes it within the same generation.
//! - The cursor is read back from the fixture store and names the daemon's incarnation;
//!   a restart resumes from it, and a corrupted `spawn_cursor.json` is reported and
//!   replaced by a fresh snapshot's cursor.

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
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use cortexkit_bus_naming::AccountNames;
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    control,
    issuance::{self as rows, PARTICIPANT},
    report::{Row, RowReport, ServedBy},
    signer::{
        nats::{nats_server_bin, unix_now},
        run::{ClaustrumSide, RunOptions, SignerRun},
        SIGNER_OPERATIONS,
    },
};
use issuance::census::CensusValue;
use nkeys::KeyPair;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_client_rs::consumer::SpawnCursor;
use subc_control::{ClientControlRequest, ClientControlResponse};

const BOOT_LIMIT: Duration = Duration::from_secs(60);

/// Runs only when the daemon starts this executable as the participant.
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
    RowReport::passed(Row::SpawnReconcile)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&vocabulary());
}

fn fresh_machine_id() -> String {
    let seed = format!("{:?}{}", std::time::SystemTime::now(), std::process::id());
    Sha256::digest(seed.as_bytes())[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct Run {
    trust: TrustChain,
    server: BusServer,
    run: SignerRun,
    ready: Value,
}

impl Run {
    fn root(&self) -> PathBuf {
        self.run.root.path().to_path_buf()
    }

    fn names(&self) -> AccountNames {
        grants::derive_account(self.ready["acct"].as_str().unwrap()).unwrap()
    }

    fn account_public(&self) -> String {
        self.ready["account_public"].as_str().unwrap().to_string()
    }

    fn cursor_path(&self) -> PathBuf {
        self.root()
            .join("data/cortexkit/ckbus")
            .join(spawn_consumer::cursor::CURSOR_FILE)
    }

    async fn revocations(&self) -> serde_json::Map<String, Value> {
        let system = bus::system_client(&self.trust, &self.server).await;
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
                "{user} was never revoked; spawn lines: {:?}",
                bus::events(&self.root(), "ckbus.spawn.event")
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn census_store(&self) -> async_nats::jetstream::kv::Store {
        let observer = bus::box_client(&self.trust, &self.server, &self.account_public()).await;
        async_nats::jetstream::new(observer)
            .get_key_value(self.names().buckets().census.clone())
            .await
            .expect("the census bucket exists")
    }

    async fn census_value(&self, module_id: &str) -> Option<CensusValue> {
        self.census_store()
            .await
            .get(AccountNames::census_key(module_id).unwrap())
            .await
            .expect("census get")
            .map(|bytes| CensusValue::parse(&bytes).expect("a census value"))
    }

    async fn set_ckbus_enabled(&self, enabled: bool) {
        let reply = control::rpc(
            &self.run.connection_file,
            ClientControlRequest::SupervisorSetEnabled {
                module_id: "ckbus".to_string(),
                enabled,
            },
        )
        .await;
        if let control::ControlReply::Error(error) = reply {
            panic!(
                "supervisor.set_enabled ckbus {enabled} refused: {} {}",
                error.code, error.message
            );
        }
    }

    /// Disables ck-bus and waits until the supervisor reports it stopped.
    async fn stop_ckbus(&self) {
        self.set_ckbus_enabled(false).await;
        let deadline = Instant::now() + BOOT_LIMIT;
        loop {
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
                .find(|module| module.module_id == "ckbus")
                .expect("supervisor.list lists ckbus");
            if !entry.enabled && !entry.live && entry.state != "running" {
                return;
            }
            assert!(Instant::now() < deadline, "ck-bus did not stop: {entry:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The `n`th `event` line in ck-bus's capture logs, waiting for it.
    async fn nth(&self, event: &str, n: usize) -> Value {
        bus::wait_event(&self.root(), event, n, BOOT_LIMIT).await;
        bus::events(&self.root(), event)[n - 1].clone()
    }
}

async fn start(with_participant: bool) -> Option<Run> {
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err((gate, observation)) => {
            RowReport::skipped(Row::SpawnReconcile, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&vocabulary());
            return None;
        }
    };
    let trust = TrustChain::generate();
    let root = SignerRun::tree();
    let server = BusServer::start(&bin, &root.join("nats"), &trust, LOOPBACK).await;
    let mut env = server.ckbus_env();
    env.push(("CKBUS_SENTINEL_PERIOD_MS".to_string(), "500".to_string()));
    let run = SignerRun::start_with(
        root,
        Path::new(env!("CARGO_BIN_EXE_ck-bus")),
        ClaustrumSide::Signer(trust.signer.clone()),
        RunOptions {
            ckbus_env: env,
            machine_id: Some(fresh_machine_id()),
        },
    )
    .await;
    let ready = bus::wait_event(run.root.path(), "ckbus.bootstrap.ready", 1, BOOT_LIMIT).await;
    if with_participant {
        rows::register_participant(&run).await;
    }
    Some(Run {
        trust,
        server,
        run,
        ready,
    })
}

async fn credential(run: &Run) -> Value {
    rows::relay(
        &run.run.connection_file,
        true,
        issuance::CREDENTIAL_OP,
        json!({}),
    )
    .await
    .expect("ckbus.credential answers")
}

fn key_of(answer: &Value) -> String {
    answer["credential_public"].as_str().unwrap().to_string()
}

/// A harness-held user recorded in the census as `module_id` at `generation`, a module
/// the supervisor has never run, and connected.
async fn plant(run: &Run, module_id: &str, generation: u64) -> (String, async_nats::Client) {
    let pair = KeyPair::new_user();
    let public = pair.public_key();
    let jwt = run.trust.user_jwt(
        &bus::box_root_id(),
        &run.account_public(),
        &pair,
        unix_now() - 60,
    );
    let value = CensusValue {
        credential_public: public.clone(),
        user_jwt_id: bus::claims(&jwt)["jti"].as_str().unwrap().to_string(),
        spawn_generation: generation,
        credential_epoch: 0,
        identities: vec![],
        rooms: vec![],
    };
    run.census_store()
        .await
        .put(
            AccountNames::census_key(module_id).unwrap(),
            value.to_bytes().into(),
        )
        .await
        .expect("the planted census entry is written");
    let client = bus::connect(&run.server.url, jwt, pair)
        .await
        .expect("the planted user connects before any reconciliation");
    client.flush().await.unwrap();
    (public, client)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exit_revokes_its_generation_and_the_respawn_is_issued_on_its_first_fetch() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start(true).await else {
        return;
    };
    run.nth("ckbus.spawn.subscribing", 1).await;
    let first = credential(&run).await;
    let first_key = key_of(&first);
    let generation = first["spawn_generation"].as_u64().unwrap();

    let respawned = rows::respawn_participant(&run.run).await;
    assert!(respawned > generation);
    run.wait_revoked(&first_key).await;
    assert_eq!(
        run.census_value(PARTICIPANT).await,
        None,
        "the exited generation's entry is gone and the respawn has none yet"
    );

    let second = credential(&run).await;
    assert_eq!(second["spawn_generation"], json!(respawned));
    assert_eq!(second["credential_epoch"], json!(0));
    let value = run
        .census_value(PARTICIPANT)
        .await
        .expect("the respawn's first fetch writes its entry");
    assert_eq!(
        (value.spawn_generation, value.credential_epoch),
        (respawned, 0)
    );
    assert_eq!(value.credential_public, key_of(&second));
    let revocations = run.revocations().await;
    assert!(revocations.contains_key(&first_key));
    assert!(!revocations.contains_key(&key_of(&second)));

    passed();
    run.server.stop().await;
    run.run.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_and_a_respawn_in_its_gap_end_with_one_identity_per_live_generation() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start(true).await else {
        return;
    };
    run.nth("ckbus.spawn.subscribing", 1).await;
    let first = credential(&run).await;
    let first_key = key_of(&first);
    let (ghost_key, ghost) = plant(&run, "ghost", 3).await;

    // The gap: ck-bus is down while the participant exits and respawns.
    run.stop_ckbus().await;
    let recorded: SpawnCursor =
        serde_json::from_slice(&std::fs::read(run.cursor_path()).unwrap()).unwrap();
    let respawned = rows::respawn_participant(&run.run).await;
    run.set_ckbus_enabled(true).await;
    run.nth("ckbus.bootstrap.ready", 2).await;
    let resumed = run.nth("ckbus.spawn.subscribing", 2).await;
    assert_eq!(
        resumed["since"],
        serde_json::to_value(&recorded).unwrap(),
        "the restarted ck-bus resumes from the recorded cursor"
    );

    run.wait_revoked(&first_key).await;
    run.wait_revoked(&ghost_key).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while ghost.connection_state() == async_nats::connection::State::Connected {
        assert!(
            Instant::now() < deadline,
            "the planted user stayed connected"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(run.census_value("ghost").await, None);
    assert_eq!(run.census_value(PARTICIPANT).await, None);

    let second = credential(&run).await;
    let second_key = key_of(&second);
    assert_eq!(second["spawn_generation"], json!(respawned));
    let value = run.census_value(PARTICIPANT).await.unwrap();
    assert_eq!(
        (value.spawn_generation, value.credential_public.as_str()),
        (respawned, second_key.as_str()),
        "one census identity for the live generation"
    );

    // A restart while the participant keeps running: the start reconciliation keeps
    // the live generation's entry and revokes nothing it holds.
    let reconciled_before = bus::events(&run.root(), "ckbus.spawn.reconciled").len();
    bus::restart_ckbus(&run.run.connection_file).await;
    run.nth("ckbus.bootstrap.ready", 3).await;
    let after_restart = run
        .nth("ckbus.spawn.reconciled", reconciled_before + 1)
        .await;
    assert_eq!(after_restart["reason"], "start");
    assert!(!run.revocations().await.contains_key(&second_key));
    assert_eq!(
        run.census_value(PARTICIPANT)
            .await
            .map(|value| value.credential_public),
        Some(second_key.clone())
    );

    // The restarted process holds no key for it, so the participant refetches: the next
    // epoch of the same generation, superseding the entry it replaces.
    let third = credential(&run).await;
    assert_eq!(third["spawn_generation"], json!(respawned));
    assert_eq!(third["credential_epoch"], json!(1));
    run.wait_revoked(&second_key).await;
    let value = run.census_value(PARTICIPANT).await.unwrap();
    assert_eq!(
        (
            value.spawn_generation,
            value.credential_epoch,
            value.credential_public
        ),
        (respawned, 1, key_of(&third))
    );
    let revocations = run.revocations().await;
    for key in [&first_key, &ghost_key, &second_key] {
        assert_eq!(
            revocations.keys().filter(|revoked| *revoked == key).count(),
            1,
            "{key} is revoked exactly once"
        );
    }
    assert!(!revocations.contains_key(&key_of(&third)));

    passed();
    run.server.stop().await;
    run.run.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cursor_reads_back_from_the_store_and_a_damaged_one_yields_a_fresh_snapshot() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start(false).await else {
        return;
    };
    let first = run.nth("ckbus.spawn.subscribing", 1).await;
    let stored: SpawnCursor = serde_json::from_slice(&std::fs::read(run.cursor_path()).unwrap())
        .expect("spawn_cursor.json holds a cursor");
    assert_eq!(first["since"], serde_json::to_value(&stored).unwrap());
    let response = control::response(
        &run.run.connection_file,
        ClientControlRequest::SupervisorSpawnSnapshot {},
    )
    .await;
    let ClientControlResponse::SupervisorSpawnSnapshot { snapshot } = response else {
        panic!("supervisor.spawn_snapshot must return its matching response variant");
    };
    assert_eq!(
        stored.daemon_incarnation, snapshot.cursor.daemon_incarnation,
        "the recorded cursor names this daemon"
    );
    assert!(stored.seq <= snapshot.cursor.seq);

    // Damage: the restarted ck-bus reports it and subscribes from a fresh snapshot.
    run.stop_ckbus().await;
    std::fs::write(run.cursor_path(), b"{\"daemon_incarnation\": ").unwrap();
    run.set_ckbus_enabled(true).await;
    let damaged = run.nth("ckbus.spawn.cursor_damaged", 1).await;
    assert_eq!(
        damaged["path"],
        run.cursor_path().display().to_string(),
        "the damaged file is named"
    );
    let subscribed = run.nth("ckbus.spawn.subscribing", 2).await;
    // The reconciliation just before that subscription is the restarted process's own.
    let fresh = bus::events(&run.root(), "ckbus.spawn.reconciled")
        .into_iter()
        .filter(|line| line["at_ms"].as_u64() <= subscribed["at_ms"].as_u64())
        .last()
        .expect("the restarted ck-bus reconciled");
    assert_eq!(fresh["reason"], "start");
    assert_eq!(
        subscribed["since"], fresh["cursor"],
        "a damaged cursor is replaced by the fresh snapshot's"
    );
    assert_ne!(subscribed["since"], serde_json::to_value(&stored).unwrap());
    let rewritten: SpawnCursor = serde_json::from_slice(&std::fs::read(run.cursor_path()).unwrap())
        .expect("the file holds a cursor again");
    assert_eq!(
        serde_json::to_value(&rewritten).unwrap(),
        subscribed["since"]
    );

    passed();
    run.server.stop().await;
    run.run.shutdown().await;
}
