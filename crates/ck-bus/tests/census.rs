//! Ladder row "A3 census write and issuance recovery" (slice 5 of
//! `docs/specs/ck-bus-module.md`). served-by: harness-signer, against a real
//! nats-server and the acceptance daemon.
//!
//! - Census write: a supervised participant's `ckbus.credential` writes exactly one
//!   census key for it, `AccountNames::census_key(module_id)`, before the answer (so
//!   before any publish of the participant's), carrying every field the census names. A
//!   refetch overwrites it at the next epoch and a respawn at a higher generation with
//!   epoch 0; `epoch_high_water.json` records each (generation, epoch).
//! - Crash arms, one per boundary of the issuance order: (i) after the high-water fsync
//!   and (ii) after signing leave no census entry and nothing usable, and the next issue
//!   is at a higher epoch; (iii) after the census write leaves an entry whose key nobody
//!   holds, a nonce signature for it is `ckbus_credential_superseded`, and the refetch
//!   issues the next epoch over it. A crash is a stop of the act at the boundary
//!   followed by a fresh issuance with fresh memory over the same store and server,
//!   which is what a restarted process has.
//! - (iv) A damaged high-water entry while the generation is live: no epoch is issued
//!   for it, the refusal is logged naming the file, the file is untouched, health is
//!   down/`Unavailable` through `supervisor.health_probe`, the open connection keeps
//!   serving, and the next generation issues normally.
//! - Longevity: no path in issuance expires a held key. Under an injected clock (tokio's
//!   paused time) advanced a day, a participant's key still signs and its census entry is
//!   unchanged. Issuance runs no sweep, so this arm can only show that nothing in it
//!   reaps a live key; the reconciliation sweep's own longevity arm is its area's.

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
#[path = "../src/runtime/seams.rs"]
mod runtime;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bootstrap::plane::{BoxPlane, Broker, DurableConsumer, NatsBroker, PlaneError};
use cortexkit_bus_naming::AccountNames;
use credentials::{
    issue::{sign_user_jwt, UserJwtRequest},
    vault::{VaultError, VaultSigning},
    wire::{self, VaultPublicKey, VaultSignature},
    Credentials,
};
use futures_util::StreamExt;
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    issuance::{self as rows, VerdictClient, PARTICIPANT},
    report::{Row, RowReport, ServedBy},
    signer::{
        nats::{nats_server_bin, unix_now},
        run::{ClaustrumSide, RunOptions, SignerRun},
        HarnessSigner, SIGNER_OPERATIONS,
    },
};
use issuance::{census::CensusValue, Issuance, LiveGenerations, Plane, PlaneSource, StopAfter};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_client_rs::HandlerOutcome;

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
    RowReport::passed(Row::Census)
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
    fn names(&self) -> AccountNames {
        grants::derive_account(self.ready["acct"].as_str().unwrap()).unwrap()
    }

    fn account_public(&self) -> String {
        self.ready["account_public"].as_str().unwrap().to_string()
    }

    fn store(&self) -> PathBuf {
        self.run.root.join("data/cortexkit/ckbus")
    }

    /// The census bucket, read by an unrestricted harness client in the box account.
    async fn census(&self) -> async_nats::jetstream::kv::Store {
        let observer = bus::box_client(&self.trust, &self.server, &self.account_public()).await;
        async_nats::jetstream::new(observer)
            .get_key_value(self.names().buckets().census.clone())
            .await
            .expect("the census bucket exists")
    }

    async fn census_value(&self, module_id: &str) -> Option<CensusValue> {
        let key = AccountNames::census_key(module_id).unwrap();
        self.census()
            .await
            .get(key)
            .await
            .expect("census get")
            .map(|bytes| CensusValue::parse(&bytes).expect("a census value ck-bus wrote"))
    }

    async fn census_keys(&self) -> Vec<String> {
        let mut keys = self.census().await.keys().await.expect("census keys");
        let mut found = Vec::new();
        while let Some(key) = keys.next().await {
            found.push(key.expect("census key"));
        }
        found
    }
}

async fn start(with_participant: bool) -> Option<Run> {
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err((gate, observation)) => {
            RowReport::skipped(Row::Census, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&vocabulary());
            return None;
        }
    };
    let trust = TrustChain::generate();
    let root = SignerRun::tree();
    let server = BusServer::start(&bin, &root.join("nats"), &trust, LOOPBACK).await;
    let run = SignerRun::start_with(
        root,
        Path::new(env!("CARGO_BIN_EXE_ck-bus")),
        ClaustrumSide::Signer(trust.signer.clone()),
        RunOptions {
            ckbus_env: server.ckbus_env(),
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

async fn credential(run: &SignerRun) -> Result<Value, (String, String)> {
    rows::relay(
        &run.connection_file,
        true,
        issuance::CREDENTIAL_OP,
        json!({}),
    )
    .await
}

fn high_water(run: &Run) -> Value {
    bus::read_json(&run.store().join("epoch_high_water.json"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_census_key_per_live_process_overwritten_by_refetch_and_respawn() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start(true).await else {
        return;
    };
    let first_generation = rows::live_generation(&run.run.connection_file, PARTICIPANT)
        .await
        .unwrap();
    assert_eq!(
        run.census_value(PARTICIPANT).await,
        None,
        "no entry before issue"
    );

    let first = credential(&run.run).await.expect("first credential");
    // Written before the answer, so before the participant could publish anything.
    let entry = run.census_value(PARTICIPANT).await.expect("census entry");
    assert_eq!(
        entry,
        CensusValue {
            credential_public: first["credential_public"].as_str().unwrap().to_string(),
            user_jwt_id: first["user_jwt_id"].as_str().unwrap().to_string(),
            spawn_generation: first_generation,
            credential_epoch: 0,
            identities: vec![],
            rooms: vec![],
        }
    );
    let raw = run
        .census()
        .await
        .get(PARTICIPANT)
        .await
        .unwrap()
        .expect("raw census value");
    let raw: Value = serde_json::from_slice(&raw).unwrap();
    for field in [
        "credential_public",
        "user_jwt_id",
        "spawn_generation",
        "credential_epoch",
        "schema_versions",
        "identities",
        "rooms",
    ] {
        assert!(
            raw.get(field).is_some(),
            "the census value lacks {field}: {raw}"
        );
    }
    assert_eq!(run.census_keys().await, vec![PARTICIPANT.to_string()]);

    // Refetch: same generation, next epoch, same key overwritten.
    let second = credential(&run.run).await.expect("refetch");
    assert_eq!(second["credential_epoch"], 1);
    assert_ne!(second["credential_public"], first["credential_public"]);
    let entry = run.census_value(PARTICIPANT).await.unwrap();
    assert_eq!(entry.credential_epoch, 1);
    assert_eq!(entry.spawn_generation, first_generation);
    assert_eq!(entry.credential_public, second["credential_public"]);
    assert_eq!(run.census_keys().await, vec![PARTICIPANT.to_string()]);
    // The superseded key is no longer signed for.
    let superseded = rows::relay(
        &run.run.connection_file,
        true,
        issuance::NONCE_SIGN_OP,
        json!({
            "nonce_b64": STANDARD.encode(b"nonce"),
            "credential_public": first["credential_public"],
        }),
    )
    .await
    .expect_err("the superseded key is not signed for");
    assert_eq!(superseded.0, issuance::code::CREDENTIAL_SUPERSEDED);

    // Respawn: a higher generation at epoch 0 overwrites the same key.
    let second_generation = rows::respawn_participant(&run.run).await;
    assert!(second_generation > first_generation);
    let third = credential(&run.run)
        .await
        .expect("credential after respawn");
    assert_eq!(third["spawn_generation"], second_generation);
    assert_eq!(third["credential_epoch"], 0);
    let entry = run.census_value(PARTICIPANT).await.unwrap();
    assert_eq!(entry.spawn_generation, second_generation);
    assert_eq!(entry.credential_epoch, 0);
    assert_eq!(entry.credential_public, third["credential_public"]);
    assert_eq!(run.census_keys().await, vec![PARTICIPANT.to_string()]);

    let recorded = high_water(&run);
    assert_eq!(
        recorded[format!("{PARTICIPANT}.g{first_generation}")],
        json!({"epoch": 1})
    );
    assert_eq!(
        recorded[format!("{PARTICIPANT}.g{second_generation}")],
        json!({"epoch": 0})
    );

    passed();
    run.server.stop().await;
    run.run.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_damaged_high_water_entry_refuses_its_generation_and_a_later_one_issues() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start(true).await else {
        return;
    };
    let generation = rows::live_generation(&run.run.connection_file, PARTICIPANT)
        .await
        .unwrap();
    let first = credential(&run.run).await.expect("first credential");
    let credential_public = first["credential_public"].as_str().unwrap().to_string();
    let connection_file = run.run.connection_file.clone();
    let signer_credential = credential_public.clone();
    let client = VerdictClient::connect(
        &run.server.url,
        first["jwt"].as_str().unwrap(),
        Arc::new(move |nonce: Vec<u8>| {
            let connection_file = connection_file.clone();
            let credential_public = signer_credential.clone();
            Box::pin(async move {
                let reply = rows::relay(
                    &connection_file,
                    true,
                    issuance::NONCE_SIGN_OP,
                    json!({"nonce_b64": STANDARD.encode(&nonce), "credential_public": credential_public}),
                )
                .await
                .map_err(|(code, message)| format!("{code}: {message}"))?;
                STANDARD
                    .decode(reply["signature_b64"].as_str().unwrap_or_default())
                    .map_err(|error| error.to_string())
            }) as futures_util::future::BoxFuture<'static, Result<Vec<u8>, String>>
        }),
        Some(format!("_INBOX.{credential_public}")),
    )
    .await
    .expect("the participant connects");

    // Damage this generation's entry.
    let path = run.store().join("epoch_high_water.json");
    let key = format!("{PARTICIPANT}.g{generation}");
    let mut recorded = high_water(&run);
    recorded[&key] = json!({"epoch": "not-a-number"});
    std::fs::write(&path, serde_json::to_vec(&recorded).unwrap()).unwrap();
    let damaged = std::fs::read(&path).unwrap();

    let (code, message) = credential(&run.run)
        .await
        .expect_err("no epoch is issued for a damaged generation");
    assert_eq!(code, issuance::code::EPOCH_HIGH_WATER_DAMAGED, "{message}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        damaged,
        "the file is untouched"
    );
    let refused =
        bus::wait_event(run.run.root.path(), "ckbus.issuance.refused", 1, BOOT_LIMIT).await;
    assert_eq!(refused["code"], issuance::code::EPOCH_HIGH_WATER_DAMAGED);
    assert_eq!(refused["path"], path.display().to_string());
    assert_eq!(refused["spawn_generation"], generation);
    // The census entry still names the credential issued before the damage.
    assert_eq!(
        run.census_value(PARTICIPANT)
            .await
            .unwrap()
            .credential_public,
        credential_public
    );

    let (status, detail, metrics) = bus::health(&run.run.connection_file).await;
    assert_eq!(status, "Failing");
    assert_eq!(detail.as_deref(), Some("bus.health.down"));
    assert_eq!(metrics["class"], "Unavailable");
    assert_eq!(
        metrics["cause"],
        issuance::handler::HIGH_WATER_DAMAGED_CAUSE
    );
    assert_eq!(metrics["path"], path.display().to_string());

    // The open connection keeps serving.
    let names = run.names();
    let dead = names.effect_dead();
    client.publish(&dead, b"still-serving").await;
    client.expect_allowed(&dead).await;
    let observer = bus::box_client(&run.trust, &run.server, &run.account_public()).await;
    let mut stream = async_nats::jetstream::new(observer)
        .get_stream(names.streams().effect_dead.clone())
        .await
        .unwrap();
    assert_eq!(stream.info().await.unwrap().state.messages, 1);

    // A later generation gets a fresh entry; the damaged one is kept verbatim.
    let later = rows::respawn_participant(&run.run).await;
    let issued = credential(&run.run)
        .await
        .expect("a later generation issues");
    assert_eq!(issued["spawn_generation"], later);
    assert_eq!(issued["credential_epoch"], 0);
    let recorded = high_water(&run);
    assert_eq!(recorded[&key], json!({"epoch": "not-a-number"}));
    assert_eq!(
        recorded[format!("{PARTICIPANT}.g{later}")],
        json!({"epoch": 0})
    );

    drop(client);
    passed();
    run.server.stop().await;
    run.run.shutdown().await;
}

/// The harness signer answered in-process, through the same wire ck-bus speaks to the
/// vault: the request is built by ck-bus's `wire`, answered by the harness signer, and
/// parsed back by ck-bus's `wire`.
struct InProcessSigner(HarnessSigner);

impl InProcessSigner {
    fn answer(&self, body: &[u8]) -> Result<Vec<u8>, VaultError> {
        match self.0.answer(body) {
            HandlerOutcome::Response(bytes) => Ok(bytes),
            other => Err(VaultError::Malformed(format!("{other:?}"))),
        }
    }
}

#[async_trait]
impl VaultSigning for InProcessSigner {
    async fn sign(
        &self,
        credential_id: &str,
        payload: &[u8],
    ) -> Result<VaultSignature, VaultError> {
        let body = wire::sign_request(credential_id, payload)
            .map_err(|error| VaultError::Malformed(format!("{error:?}")))?;
        wire::parse_sign_reply(&self.answer(&body)?)
            .map_err(|error| VaultError::Malformed(format!("{error:?}")))
    }

    async fn public_key(&self, credential_id: &str) -> Result<VaultPublicKey, VaultError> {
        wire::parse_public_key_reply(&self.answer(&wire::public_key_request(credential_id))?)
            .map_err(|error| VaultError::Malformed(format!("{error:?}")))
    }
}

struct FixedGeneration(u64);

#[async_trait]
impl LiveGenerations for FixedGeneration {
    async fn live_generation(&self, _module_id: &str) -> Result<Option<u64>, String> {
        Ok(Some(self.0))
    }
}

struct FixedPlane(Plane);

impl PlaneSource for FixedPlane {
    fn current(&self) -> Option<Plane> {
        Some(self.0.clone())
    }
}

/// ck-bus's own bus-module user in the run's box account, connected through ck-bus's
/// broker code: the connection census writes go through.
async fn bus_module_plane(run: &Run) -> Plane {
    let credentials = Arc::new(Credentials::new(Arc::new(InProcessSigner(
        run.trust.signer.clone(),
    ))));
    let user = credentials.custody.generate_user();
    let names = run.names();
    let grant = grants::bus_module_grant(&names, &user).unwrap();
    let account_public = run.account_public();
    let jwt = sign_user_jwt(
        credentials.vault.as_ref(),
        &credentials.key_ids,
        &UserJwtRequest {
            root_credential_id: &bus::box_root_id(),
            user_public: &user,
            issuer_account: Some(&account_public),
            name: "census-row-bus-module",
            issued_at: unix_now() - 60,
            grant: &grant,
        },
    )
    .await
    .expect("bus-module user JWT");
    let box_plane = NatsBroker::new(run.server.url.clone(), credentials)
        .connect_box(&jwt.jwt, &user)
        .await
        .unwrap_or_else(|error| panic!("bus-module user connects: {error}"));
    Plane {
        names,
        account_public,
        server_url: run.server.url.clone(),
        box_plane,
    }
}

/// A fresh issuance process: fresh memory (no held keys) over the given store.
fn process(
    run: &Run,
    store: &Path,
    plane: &Plane,
    generation: u64,
) -> (Issuance, Arc<Credentials>) {
    let credentials = Arc::new(Credentials::new(Arc::new(InProcessSigner(
        run.trust.signer.clone(),
    ))));
    let issuing = Issuance::new(
        credentials.clone(),
        store,
        Arc::new(FixedGeneration(generation)),
        Arc::new(FixedPlane(plane.clone())),
    );
    (issuing, credentials)
}

fn holds_no_key(credentials: &Credentials) -> bool {
    format!("{:?}", credentials.custody).contains("users: []")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_at_each_issuance_boundary_leaves_nothing_to_roll_back() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start(false).await else {
        return;
    };
    const MODULE: &str = "crashprobe";
    const GENERATION: u64 = 7;
    let plane = bus_module_plane(&run).await;
    let store = tempfile::tempdir().unwrap();
    let high_water = |store: &Path| bus::read_json(&store.join("epoch_high_water.json"));

    // (i) After the high-water fsync, before signing.
    let (first, memory) = process(&run, store.path(), &plane, GENERATION);
    *first.crash_after.lock().unwrap() = Some(StopAfter::HighWater);
    first
        .issue(MODULE)
        .await
        .expect_err("stopped at the boundary");
    assert_eq!(
        high_water(store.path())[format!("{MODULE}.g{GENERATION}")]["epoch"],
        0
    );
    assert_eq!(run.census_value(MODULE).await, None, "(i) no census entry");
    assert!(holds_no_key(&memory), "(i) no usable credential");

    // (ii) After signing, before the census write.
    let (second, memory) = process(&run, store.path(), &plane, GENERATION);
    *second.crash_after.lock().unwrap() = Some(StopAfter::Signing);
    second
        .issue(MODULE)
        .await
        .expect_err("stopped at the boundary");
    assert_eq!(
        high_water(store.path())[format!("{MODULE}.g{GENERATION}")]["epoch"],
        1
    );
    assert_eq!(run.census_value(MODULE).await, None, "(ii) no census entry");
    assert!(holds_no_key(&memory), "(ii) the signed JWT's seed is gone");

    // The next issue after a restart is at a higher epoch.
    let (third, _memory) = process(&run, store.path(), &plane, GENERATION);
    let issued = third
        .issue(MODULE)
        .await
        .expect("issues after (i) and (ii)");
    assert_eq!(issued.issued.credential_epoch, 2);
    assert_eq!(run.census_value(MODULE).await.unwrap().credential_epoch, 2);

    // (iii) After the census write, before the answer.
    *third.crash_after.lock().unwrap() = Some(StopAfter::Census);
    third
        .issue(MODULE)
        .await
        .expect_err("stopped at the boundary");
    let orphan = run.census_value(MODULE).await.unwrap();
    assert_eq!(
        orphan.credential_epoch, 3,
        "(iii) the entry names the new epoch"
    );
    let (restarted, memory) = process(&run, store.path(), &plane, GENERATION);
    assert!(
        !memory.custody.holds(&orphan.credential_public),
        "nobody holds its key"
    );
    let refused = restarted
        .sign_nonce(MODULE, Some(&orphan.credential_public), b"nonce")
        .await
        .expect_err("a key nobody holds is never signed for");
    assert_eq!(refused.code, issuance::code::CREDENTIAL_SUPERSEDED);
    let refetched = restarted.issue(MODULE).await.expect("the refetch issues");
    assert_eq!(refetched.issued.credential_epoch, 4);
    let entry = run.census_value(MODULE).await.unwrap();
    assert_eq!(entry.credential_epoch, 4);
    assert_ne!(entry.credential_public, orphan.credential_public);
    assert_eq!(entry.credential_public, refetched.issued.credential_public);

    passed();
    run.server.stop().await;
    run.run.shutdown().await;
}

/// A census plane that records writes in memory, for the paused-clock arm (a real
/// server connection cannot run under a paused clock).
#[derive(Default)]
struct RecordingPlane {
    census: Mutex<Vec<(String, Vec<u8>)>>,
}

#[async_trait]
impl BoxPlane for RecordingPlane {
    async fn ensure_census(&self, _account: &AccountNames) -> Result<(), PlaneError> {
        Ok(())
    }
    async fn ensure_stream(
        &self,
        _spec: &cortexkit_bus_naming::StreamSpec,
    ) -> Result<(), PlaneError> {
        Ok(())
    }
    async fn publish(&self, _subject: &str, _payload: Vec<u8>) -> Result<(), PlaneError> {
        Ok(())
    }
    async fn census_put(&self, subject: &str, value: Vec<u8>) -> Result<(), PlaneError> {
        self.census
            .lock()
            .unwrap()
            .push((subject.to_string(), value));
        Ok(())
    }
    async fn ensure_durable(&self, _durable: &DurableConsumer) -> Result<(), PlaneError> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn a_long_lived_participant_keeps_its_key_under_an_advanced_clock() {
    let signer = HarnessSigner::generated(&[&bus::box_root_id()]);
    let credentials = Arc::new(Credentials::new(Arc::new(InProcessSigner(signer))));
    let recording = Arc::new(RecordingPlane::default());
    let names = grants::derive_account("box_longevity").unwrap();
    let store = tempfile::tempdir().unwrap();
    let issuing = Issuance::new(
        credentials.clone(),
        store.path(),
        Arc::new(FixedGeneration(1)),
        Arc::new(FixedPlane(Plane {
            names,
            account_public: nkeys::KeyPair::new_account().public_key(),
            server_url: "nats://127.0.0.1:4222".to_string(),
            box_plane: recording.clone(),
        })),
    );
    let issued = issuing.issue(PARTICIPANT).await.expect("issues");
    let claims = bus::claims(&issued.jwt);
    assert!(
        claims.get("exp").is_none(),
        "no expiry is issued while the TTL is unpinned"
    );
    for _ in 0..24 {
        tokio::time::advance(Duration::from_secs(3600)).await;
    }
    issuing
        .sign_nonce(
            PARTICIPANT,
            Some(&issued.issued.credential_public),
            b"nonce-after-a-day",
        )
        .await
        .expect("the key is still held a day later");
    assert_eq!(issuing.current(PARTICIPANT), Some(issued.issued.clone()));
    assert_eq!(
        recording.census.lock().unwrap().len(),
        1,
        "no rewrite, no removal"
    );
    passed_in_process();
}

fn passed_in_process() {
    RowReport::passed(Row::Census)
        .served_by(ServedBy::HarnessSigner)
        .emit(&vocabulary());
}
