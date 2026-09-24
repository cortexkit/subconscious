//! Ladder row "Machine id and account" (slice 4 of `docs/specs/ck-bus-module.md`, with
//! the key and first-boot rules of `docs/designs/nats-install-trust-chain.md`).
//! served-by: harness-signer.
//!
//! A supervised ck-bus boots against a real `nats-server` configured the way `ck setup`
//! configures it (operator and system account JWTs from fixture roots, full resolver,
//! no box account). ck-bus names the box account `box_<HELLO_ACK machine id>`, creates it
//! through the operator signer, and creates the census bucket and the five streams in
//! it. The arms:
//!
//! - the account, bucket and streams carry the machine id;
//! - a supervised restart keeps the account id, so a durable consumer resumes at its
//!   cursor;
//! - with `account.json` deleted, boot finds the account by name and adopts it;
//! - a different machine id while the old box account exists is refused, health down
//!   naming both ids, with no second account;
//! - a damaged `account.json` fails closed, health down naming it, the file untouched;
//! - with no machine id nothing is created (unit level: the acceptance daemon always
//!   serves one);
//! - a claims update whose read-back differs from the push is not applied and is
//!   reported (unit level: a real resolver cannot be made to answer wrongly).

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

use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_nats::jetstream::{self, consumer::pull};
use async_trait::async_trait;
use bootstrap::{
    boot, cause,
    plane::{
        apply_account_jwt, ApplyError, BoxPlane, Broker, ConnectionEvent, PlaneError, SystemPlane,
    },
    store::Store,
    BootDeps, BrokerInputs, OwnProcess, OwnSpawn,
};
use cortexkit_bus_naming::{shipped_streams, AccountNames, StreamSpec};
use credentials::{
    vault::{VaultError, VaultSigning},
    wire::{self, VaultPublicKey, VaultSignature},
    Credentials,
};
use futures_util::StreamExt;
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    report::{Row, RowReport, ServedBy},
    signer::{
        nats::nats_server_bin,
        run::{ClaustrumSide, RunOptions, SignerRun},
        HarnessSigner, SIGNER_OPERATIONS,
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subc_client_rs::HandlerOutcome;
use subc_protocol::MachineId;

const BOOT_LIMIT: Duration = Duration::from_secs(60);

fn signer_vocabulary() -> BTreeSet<String> {
    SIGNER_OPERATIONS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

fn passed() {
    RowReport::passed(Row::AccountIdentity)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&signer_vocabulary());
}

fn fresh_machine_id() -> String {
    let seed = format!("{:?}{}", std::time::SystemTime::now(), std::process::id());
    let digest = Sha256::digest(seed.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// The server, a daemon serving `machine_id`, and a supervised ck-bus pointed at both.
struct Plane {
    trust: TrustChain,
    server: BusServer,
    run: SignerRun,
}

async fn start(machine_id: &str) -> Option<Plane> {
    let (bin, version) = match nats_server_bin() {
        Ok(found) => found,
        Err((gate, observation)) => {
            RowReport::skipped(Row::AccountIdentity, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&signer_vocabulary());
            return None;
        }
    };
    eprintln!("nats-server {version}");
    let trust = TrustChain::generate();
    let root = SignerRun::tree();
    let server = BusServer::start(&bin, &root.join("nats"), &trust, LOOPBACK).await;
    let run = start_run(root, &trust, &server, machine_id).await;
    Some(Plane { trust, server, run })
}

async fn start_run(
    root: subc_daemon::test_support::TestTempDir,
    trust: &TrustChain,
    server: &BusServer,
    machine_id: &str,
) -> SignerRun {
    let mut env = server.ckbus_env();
    env.push(("CKBUS_SENTINEL_PERIOD_MS".to_string(), "500".to_string()));
    SignerRun::start_with(
        root,
        Path::new(env!("CARGO_BIN_EXE_ck-bus")),
        ClaustrumSide::Signer(trust.signer.clone()),
        RunOptions {
            ckbus_env: env,
            machine_id: Some(machine_id.to_string()),
        },
    )
    .await
}

async fn ready(plane: &Plane, count: usize) -> Value {
    bus::wait_event(
        plane.run.root.path(),
        "ckbus.bootstrap.ready",
        count,
        BOOT_LIMIT,
    )
    .await
}

fn account_public(plane: &Plane) -> String {
    bus::read_json(&bus::account_json(plane.run.root.path()))["account_public"]
        .as_str()
        .expect("account.json names the account id")
        .to_string()
}

async fn assert_bucket_and_streams(plane: &Plane, account_public: &str, names: &AccountNames) {
    let client = bus::box_client(&plane.trust, &plane.server, account_public).await;
    let js = jetstream::new(client);
    let mut expected: Vec<(String, Vec<String>)> = shipped_streams(names)
        .into_iter()
        .map(|spec: StreamSpec| (spec.name, spec.subjects))
        .collect();
    expected.push((
        names.buckets().census_stream.clone(),
        vec![format!("$KV.{}.>", names.buckets().census)],
    ));
    for (name, subjects) in expected {
        let mut stream = js
            .get_stream(&name)
            .await
            .unwrap_or_else(|error| panic!("stream {name} must exist: {error}"));
        let info = stream.info().await.expect("stream info");
        assert_eq!(
            info.config.subjects, subjects,
            "{name} carries its literal binding"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_is_named_for_the_machine_id_with_its_bucket_and_streams() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let machine_id = fresh_machine_id();
    let Some(plane) = start(&machine_id).await else {
        return;
    };
    let event = ready(&plane, 1).await;
    let acct = format!("box_{machine_id}");
    assert_eq!(event["acct"], acct.as_str());
    let record = bus::read_json(&bus::account_json(plane.run.root.path()));
    assert_eq!(record["machine_id"], machine_id.as_str());
    assert_eq!(record["acct"], acct.as_str());
    let account = account_public(&plane);
    assert_eq!(
        plane.server.stored_accounts(),
        BTreeSet::from([plane.trust.system_account.clone(), account.clone()]),
        "the resolver holds the system account and exactly one box account"
    );

    // The account JWT as the server stores it: named for the machine id, issued by the
    // operator signer (never the root), listing the box account root.
    let system = bus::system_client(&plane.trust, &plane.server).await;
    let stored = bus::lookup(&system, &account)
        .await
        .expect("the box account JWT is stored");
    let claims = bus::claims(&stored);
    assert_eq!(claims["name"], acct.as_str());
    assert_eq!(claims["iss"], plane.trust.signer_public().as_str());
    assert_eq!(
        claims["nats"]["signing_keys"],
        serde_json::json!([plane.trust.box_root_public()])
    );

    let names = AccountNames::derive(&acct).unwrap();
    assert_bucket_and_streams(&plane, &account, &names).await;

    plane.server.stop().await;
    plane.run.shutdown().await;
    passed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_keeps_streams_and_the_consumer_cursor_on_the_same_account_id() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let machine_id = fresh_machine_id();
    let Some(plane) = start(&machine_id).await else {
        return;
    };
    let first = ready(&plane, 1).await;
    let account = account_public(&plane);
    let names = AccountNames::derive(&format!("box_{machine_id}")).unwrap();
    let subject = names.room_post("room_cursor").unwrap();

    let client = bus::box_client(&plane.trust, &plane.server, &account).await;
    let js = jetstream::new(client.clone());
    let stream = js.get_stream(&names.streams().room).await.unwrap();
    let consumer: jetstream::consumer::Consumer<pull::Config> = stream
        .create_consumer(pull::Config {
            durable_name: Some("c_harness_cursor".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    for body in ["one", "two", "three"] {
        js.publish(subject.clone(), body.into())
            .await
            .unwrap()
            .await
            .unwrap();
    }
    let mut batch = consumer.fetch().max_messages(1).messages().await.unwrap();
    let message = batch.next().await.expect("one message").unwrap();
    assert_eq!(message.payload.as_ref(), b"one");
    message.double_ack().await.unwrap();
    drop(batch);
    drop(client);

    bus::restart_ckbus(&plane.run.connection_file).await;
    let second = ready(&plane, 2).await;
    assert_ne!(
        first["incarnation"], second["incarnation"],
        "a new process booted"
    );
    assert_eq!(
        second["account_public"],
        account.as_str(),
        "the restarted ck-bus kept the account id"
    );
    assert_eq!(account_public(&plane), account, "account.json kept the id");
    assert_eq!(
        plane.server.stored_accounts().len(),
        2,
        "no second box account"
    );

    // Resume through the account the restarted ck-bus names.
    let client = bus::box_client(&plane.trust, &plane.server, &account_public(&plane)).await;
    let js = jetstream::new(client);
    let stream = js
        .get_stream(&names.streams().room)
        .await
        .expect("the room stream survived the restart");
    let consumer: jetstream::consumer::Consumer<pull::Config> = stream
        .get_consumer("c_harness_cursor")
        .await
        .expect("the durable consumer survived the restart");
    let mut batch = consumer.fetch().max_messages(1).messages().await.unwrap();
    let message = batch.next().await.expect("the next message").unwrap();
    assert_eq!(
        message.payload.as_ref(),
        b"two",
        "the consumer resumes at its cursor"
    );

    plane.server.stop().await;
    plane.run.shutdown().await;
    passed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn absent_account_json_adopts_the_existing_server_account() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let machine_id = fresh_machine_id();
    let Some(plane) = start(&machine_id).await else {
        return;
    };
    ready(&plane, 1).await;
    let account = account_public(&plane);
    std::fs::remove_file(bus::account_json(plane.run.root.path())).unwrap();

    bus::restart_ckbus(&plane.run.connection_file).await;
    let second = ready(&plane, 2).await;
    assert_eq!(
        second["account_public"],
        account.as_str(),
        "the account found by name was adopted"
    );
    assert_eq!(
        account_public(&plane),
        account,
        "account.json re-recorded it"
    );
    assert_eq!(
        plane.server.stored_accounts(),
        BTreeSet::from([plane.trust.system_account.clone(), account.clone()]),
        "no second box account was created"
    );
    let recorded = bus::events(plane.run.root.path(), "ckbus.bootstrap.account_recorded");
    assert_eq!(recorded.last().unwrap()["adopted"], true);

    plane.server.stop().await;
    plane.run.shutdown().await;
    passed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn machine_id_change_is_refused_naming_both_ids_and_creates_no_second_account() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let old_id = fresh_machine_id();
    let Some(plane) = start(&old_id).await else {
        return;
    };
    ready(&plane, 1).await;
    let account = account_public(&plane);
    let account_bytes = std::fs::read(bus::account_json(plane.run.root.path())).unwrap();
    let Plane { trust, server, run } = plane;

    // A new daemon on the same tree serves another machine id, as after `ck machine
    // adopt`.
    let new_id = fresh_machine_id();
    assert_ne!(old_id, new_id);
    let root = run.shutdown().await;
    let run = start_run(root, &trust, &server, &new_id).await;
    for with_account_json in [true, false] {
        if !with_account_json {
            // The second path: no account.json, so the old account is found by name.
            std::fs::remove_file(bus::account_json(run.root.path())).unwrap();
            bus::restart_ckbus(&run.connection_file).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let metrics =
            bus::wait_health_cause(&run.connection_file, cause::MACHINE_ID_CHANGED, BOOT_LIMIT)
                .await;
        let message = metrics["message"].as_str().unwrap();
        assert!(
            message.contains(&old_id) && message.contains(&new_id),
            "health names both machine ids: {message}"
        );
        assert_eq!(metrics["class"], "Unavailable");
        assert_eq!(
            server.stored_accounts(),
            BTreeSet::from([trust.system_account.clone(), account.clone()]),
            "no second box account appears"
        );
        if with_account_json {
            assert_eq!(
                std::fs::read(bus::account_json(run.root.path())).unwrap(),
                account_bytes,
                "account.json is left as it was"
            );
        } else {
            assert!(
                !bus::account_json(run.root.path()).exists(),
                "nothing is recorded for the new machine id"
            );
        }
    }

    server.stop().await;
    run.shutdown().await;
    passed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damaged_account_json_fails_closed_naming_the_file() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let machine_id = fresh_machine_id();
    let Some(plane) = start(&machine_id).await else {
        return;
    };
    ready(&plane, 1).await;
    let path = bus::account_json(plane.run.root.path());
    std::fs::write(&path, b"{\"machine_id\": ").unwrap();
    bus::restart_ckbus(&plane.run.connection_file).await;
    let metrics = bus::wait_health_cause(
        &plane.run.connection_file,
        cause::ACCOUNT_JSON_DAMAGED,
        BOOT_LIMIT,
    )
    .await;
    assert!(
        metrics["message"]
            .as_str()
            .unwrap()
            .contains(&path.display().to_string()),
        "health names the file: {metrics}"
    );
    let (status, detail, _) = bus::health(&plane.run.connection_file).await;
    assert_eq!(status, "Failing");
    assert_eq!(detail.as_deref(), Some("bus.health.down"));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"{\"machine_id\": ",
        "never rewritten"
    );
    assert_eq!(
        bus::events(plane.run.root.path(), "ckbus.bootstrap.ready").len(),
        1,
        "the restarted process issued nothing"
    );

    plane.server.stop().await;
    plane.run.shutdown().await;
    passed();
}

// ---------------------------------------------------------------------------------
// Unit level, against the boot sequence's seams.
// ---------------------------------------------------------------------------------

/// The harness signer answering in-process, through claustrum's exact wire.
struct InProcessVault {
    signer: HarnessSigner,
    calls: Arc<Mutex<Vec<String>>>,
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
        self.calls
            .lock()
            .unwrap()
            .push(format!("sign {credential_id}"));
        let body = self.body(wire::sign_request(credential_id, payload).unwrap());
        wire::parse_sign_reply(&body).map_err(|error| VaultError::Malformed(error.to_string()))
    }

    async fn public_key(&self, credential_id: &str) -> Result<VaultPublicKey, VaultError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("public_key {credential_id}"));
        let body = self.body(wire::public_key_request(credential_id));
        wire::parse_public_key_reply(&body)
            .map_err(|error| VaultError::Malformed(error.to_string()))
    }
}

/// A broker that records every call and whose resolver answers the lookup after an
/// update with `read_back` instead of what was pushed.
#[derive(Clone, Default)]
struct FakeBroker {
    calls: Arc<Mutex<Vec<String>>>,
    read_back: Option<String>,
}

#[async_trait]
impl Broker for FakeBroker {
    async fn connect_system(
        &self,
        _jwt: &str,
        _user: &str,
    ) -> Result<Arc<dyn SystemPlane>, PlaneError> {
        self.calls
            .lock()
            .unwrap()
            .push("connect_system".to_string());
        Ok(Arc::new(self.clone()))
    }

    async fn connect_box(&self, _jwt: &str, _user: &str) -> Result<Arc<dyn BoxPlane>, PlaneError> {
        self.calls.lock().unwrap().push("connect_box".to_string());
        Err(PlaneError::new("the fake broker holds no box account"))
    }
}

#[async_trait]
impl SystemPlane for FakeBroker {
    async fn list_accounts(&self) -> Result<Vec<String>, PlaneError> {
        self.calls.lock().unwrap().push("list".to_string());
        Ok(Vec::new())
    }

    async fn lookup(&self, account_public: &str) -> Result<Option<String>, PlaneError> {
        let mut calls = self.calls.lock().unwrap();
        let after_update = calls.iter().any(|call| call == "update");
        calls.push(format!("lookup {account_public}"));
        Ok(if after_update {
            self.read_back.clone()
        } else {
            None
        })
    }

    async fn update(&self, _jwt: &str) -> Result<(), PlaneError> {
        self.calls.lock().unwrap().push("update".to_string());
        Ok(())
    }

    async fn kick(&self, _server_id: &str, _client_id: u64) -> Result<(), PlaneError> {
        self.calls.lock().unwrap().push("kick".to_string());
        Ok(())
    }

    async fn watch_connections(
        &self,
        _account_public: &str,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<ConnectionEvent>, PlaneError> {
        // Bootstrap never watches connections; a call here is a bug the test must see.
        self.calls
            .lock()
            .unwrap()
            .push("watch_connections".to_string());
        Err(PlaneError::new(
            "the account-identity FakeBroker does not serve connection events",
        ))
    }
}

/// Bootstrap's own-spawn source for the in-process boot tests in this file. They all
/// stop before bootstrap writes ck-bus's own census key, so any call returns an error
/// naming this fake.
struct NoOwnSpawn;

#[async_trait]
impl OwnSpawn for NoOwnSpawn {
    async fn own_process(&self) -> Result<OwnProcess, String> {
        Err("the account-identity unit arms serve no spawn snapshot".to_string())
    }
}

fn unit_deps(trust: &TrustChain, store: &Path, calls: Arc<Mutex<Vec<String>>>) -> BootDeps {
    BootDeps {
        credentials: Arc::new(Credentials::new(Arc::new(InProcessVault {
            signer: trust.signer.clone(),
            calls,
        }))),
        grants: Arc::new(grants::GrantSeam),
        store: Store::new(store.to_path_buf()),
        incarnation: "unit".to_string(),
        own_spawn: Arc::new(NoOwnSpawn),
    }
}

/// Writes an operator JWT the way setup does and returns the broker inputs naming it.
fn unit_config(trust: &TrustChain, dir: &Path) -> bootstrap::config::BrokerConfig {
    let path = dir.join("operator.jwt");
    std::fs::write(&path, trust.operator_jwt()).unwrap();
    bootstrap::config::BrokerConfig {
        url: "nats://127.0.0.1:4222".to_string(),
        operator_jwt_path: path,
        system_account: trust.system_account.clone(),
    }
}

#[tokio::test]
async fn machine_id_absent_creates_and_issues_nothing() {
    let trust = TrustChain::generate();
    let dir = tempfile::tempdir().unwrap();
    let vault_calls = Arc::new(Mutex::new(Vec::new()));
    let deps = unit_deps(&trust, &dir.path().join("store"), vault_calls.clone());
    let broker = FakeBroker::default();
    let config = Ok(unit_config(&trust, dir.path()));
    let failure = boot(
        None,
        BrokerInputs {
            config: &config,
            broker: &broker,
        },
        &deps,
    )
    .await
    .err()
    .expect("no machine id must stop the boot");
    assert_eq!(failure.cause, cause::MACHINE_ID_ABSENT);
    assert!(!failure.retry, "an absent machine id is not retried");
    assert!(vault_calls.lock().unwrap().is_empty(), "nothing was signed");
    assert!(
        broker.calls.lock().unwrap().is_empty(),
        "the broker was never reached"
    );
    assert!(
        !dir.path().join("store").exists(),
        "nothing was written to the store"
    );
    passed();
}

#[tokio::test]
async fn claims_read_back_mismatch_is_not_applied_and_reported() {
    // Directly: a lookup answering another token, or nothing, is not applied.
    let pushed = "e30.eyJqdGkiOiJQVVNIRUQifQ.c2ln";
    let other = "e30.eyJqdGkiOiJPVEhFUiJ9.c2ln";
    for (read_back, named) in [(Some(other.to_string()), "jti OTHER"), (None, "nothing")] {
        let plane = FakeBroker {
            read_back,
            ..FakeBroker::default()
        };
        let error = apply_account_jwt(&plane, "ACCOUNT", pushed)
            .await
            .expect_err("a mismatching read-back is not applied");
        let ApplyError::ReadBackMismatch {
            pushed_jti,
            read_back,
            ..
        } = &error
        else {
            panic!("expected a read-back mismatch, got {error:?}");
        };
        assert_eq!(pushed_jti, "PUSHED");
        assert!(read_back.contains(named), "{read_back}");
    }
    let exact = FakeBroker {
        read_back: Some(pushed.to_string()),
        ..FakeBroker::default()
    };
    apply_account_jwt(&exact, "ACCOUNT", pushed)
        .await
        .expect("an exact read-back is applied");

    // Through the boot sequence: the mismatch stops the boot before any box user
    // connects, and is reported under its cause and retried.
    let trust = TrustChain::generate();
    let dir = tempfile::tempdir().unwrap();
    let deps = unit_deps(
        &trust,
        &dir.path().join("store"),
        Arc::new(Mutex::new(Vec::new())),
    );
    let broker = FakeBroker {
        read_back: Some(other.to_string()),
        ..FakeBroker::default()
    };
    let config = Ok(unit_config(&trust, dir.path()));
    let machine_id = MachineId::parse(&fresh_machine_id()).unwrap();
    let failure = boot(
        Some(&machine_id),
        BrokerInputs {
            config: &config,
            broker: &broker,
        },
        &deps,
    )
    .await
    .err()
    .expect("a mismatching read-back must stop the boot");
    assert_eq!(failure.cause, cause::CLAIMS_READBACK_MISMATCH);
    assert!(failure.retry, "a read-back mismatch is retried");
    assert!(
        failure.message.contains("not applied"),
        "{}",
        failure.message
    );
    let calls = broker.calls.lock().unwrap().clone();
    assert!(calls.contains(&"update".to_string()), "{calls:?}");
    assert!(
        !calls.contains(&"connect_box".to_string()),
        "no box user connects on an unapplied account: {calls:?}"
    );
    passed();
}
