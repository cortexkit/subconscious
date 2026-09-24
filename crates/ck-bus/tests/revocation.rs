//! Ladder row "A3 revocation" (slice 6 of `docs/specs/ck-bus-module.md`), against a real
//! nats-server and the acceptance daemon.
//!
//! Serving sides, per arm:
//! - harness-signer: every arm but the last. The operator signer that signs each
//!   revocation list is the harness signer's fixture key; the operator ROOT (which signed
//!   the operator JWT) is never asked for anything.
//! - claustrum-binary: `the_operator_signer_grant_authorizes_the_revocation_signature`,
//!   the vault-permission arm. It asserts vault authority, so it runs only against a
//!   real claustrum and otherwise records `claustrum-binary-absent` loudly and reports
//!   SKIP. It reports under the Vault authorization row, whose gates cell names that
//!   skip; the A3 revocation row's cell names none.
//!
//! Arms:
//! - The three steps: the revoked user's live socket is severed, the server emits one
//!   `$SYS` disconnect event for it, an explicit reconnect within 5 s is refused as
//!   revoked, and after a server restart it is still refused (the resolver persisted the
//!   claims). The claims read back carry the key exactly once, signed by the operator
//!   signer. A replay changes nothing: the account JWT is byte-identical and no new
//!   disconnect event appears.
//! - (i) A stop at every boundary (just after a record, and just after a step's effect
//!   and before its record), then a fresh process over the same store and server, ends
//!   with one revocation entry, one disconnect event and no record. The stop is
//!   simulated in-process at the exact boundary (as the census row does): a real kill
//!   cannot be aimed between two statements. The restart arm below uses a real kill.
//! - (ii) Progress corrupted after step (1), census entry present: the inputs are
//!   re-derived from the census and the steps replay. (iii) Corrupted after step (2):
//!   the record is cleared and nothing is pushed. (iv) Corrupted with the census read
//!   failing: recovery defers and leaves the file as it is.
//! - Restart (real `SIGKILL` of the supervised ck-bus): established connections survive
//!   and the restarted ck-bus revokes nothing on its own. The participant's next connect
//!   fails inside `ckbus.nonce_sign` (`ckbus_credential_superseded`) without the server
//!   seeing a credential. Its refetch revokes the superseded user, which the restarted
//!   process found in the census. That user connected before the restarted process was
//!   watching, so there was nothing to kick: the revocation list alone closed the live
//!   connection. A second refetch revokes a user whose connection the process did see,
//!   and that one is recorded with its kick target.
//!
//! Controls: an unrevoked bystander keeps working; the revoked client runs no census
//! watch (the trait layer's) and is refused all the same; a kick without the claims
//! update reconnects; a bus-module user without census read fails step (1) outright; a
//! refused operator signature fails step (1), pushes nothing and leaves the census
//! entry as it is.

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
#[path = "../src/membership/mod.rs"]
mod membership;
#[allow(dead_code)]
#[path = "../src/revocation/mod.rs"]
mod revocation;
#[allow(dead_code)]
#[path = "../src/runtime/seams.rs"]
mod runtime;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_nats::ConnectErrorKind;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bootstrap::plane::{
    BoxPlane, Broker, CensusRecord, ConnectionEvent, DurableConsumer, NatsBroker, PlaneError,
    SystemPlane,
};
use cortexkit_bus_naming::{AccountNames, Operation, StreamSpec};
use credentials::{
    issue::{sign_user_jwt, UserJwtRequest},
    vault::{VaultError, VaultSigning},
    wire::{self, VaultPublicKey, VaultSignature},
    Credentials,
};
use futures_util::StreamExt;
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    issuance::{self as rows, PARTICIPANT},
    report::{Row, RowReport, ServedBy},
    signer::{
        nats::{nats_server_bin, unix_now},
        run::{ClaustrumSide, RunOptions, SignerRun},
        HarnessSigner, SIGNER_OPERATIONS,
    },
};
use issuance::census::CensusValue;
use nkeys::KeyPair;
use revocation::{
    cause,
    connections::Connections,
    progress::{self, Identity, KickTarget, ProgressStore, Record},
    Boundary, Completed, RevocationError, RevocationPlane, Revoker, Target,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_client_rs::HandlerOutcome;

const BOOT_LIMIT: Duration = Duration::from_secs(60);
/// The spec's bound for the explicit reconnect after a revocation.
const RECONNECT_BOUND: Duration = Duration::from_secs(5);
/// The `$SYS` disconnect reason nats-server 2.15.0 reports when a claims update revokes
/// a connected user.
const REVOKED_REASON: &str = "Credentials Revoked";

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
    RowReport::passed(Row::Revocation)
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

/// A supervised ck-bus that booted (so the box account exists) beside a real server.
struct Run {
    trust: TrustChain,
    server: BusServer,
    run: SignerRun,
    ready: Value,
    bin: PathBuf,
}

impl Run {
    fn names(&self) -> AccountNames {
        grants::derive_account(self.ready["acct"].as_str().unwrap()).unwrap()
    }

    fn account_public(&self) -> String {
        self.ready["account_public"].as_str().unwrap().to_string()
    }

    async fn system_observer(&self) -> async_nats::Client {
        bus::system_client(&self.trust, &self.server).await
    }

    /// The box account JWT the resolver stores now.
    async fn account_jwt(&self) -> String {
        bus::lookup(&self.system_observer().await, &self.account_public())
            .await
            .expect("the box account JWT is stored")
    }

    async fn revocations(&self) -> serde_json::Map<String, Value> {
        bus::claims(&self.account_jwt().await)["nats"]["revocations"]
            .as_object()
            .cloned()
            .unwrap_or_default()
    }

    async fn census_value(&self, module_id: &str) -> Option<CensusValue> {
        let observer = bus::box_client(&self.trust, &self.server, &self.account_public()).await;
        async_nats::jetstream::new(observer)
            .get_key_value(self.names().buckets().census.clone())
            .await
            .expect("the census bucket exists")
            .get(AccountNames::census_key(module_id).unwrap())
            .await
            .expect("census get")
            .map(|bytes| CensusValue::parse(&bytes).expect("a census value"))
    }
}

async fn start(with_participant: bool) -> Option<Run> {
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err((gate, observation)) => {
            RowReport::skipped(Row::Revocation, gate, observation)
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
        bin,
    })
}

/// Disables the supervised ck-bus once it has booted the box account, and waits until
/// its process is gone. The arms that call this plant census entries for modules that
/// have no live process and revoke them with a `Revoker` in this process. A running
/// ck-bus reconciles its census against the spawn snapshot and would revoke those
/// entries first, so the test's revoker must be the only one.
async fn retire_supervised_ckbus(run: &Run) {
    let pid = run.run.supervised_pid("ckbus").await;
    let reply = harness::control::rpc(
        &run.run.connection_file,
        subc_control::ClientControlRequest::SupervisorSetEnabled {
            module_id: "ckbus".to_string(),
            enabled: false,
        },
    )
    .await;
    if let harness::control::ControlReply::Error(error) = reply {
        panic!(
            "supervisor.set_enabled ckbus false refused: {} {}",
            error.code, error.message
        );
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let response = harness::control::response(
            &run.run.connection_file,
            subc_control::ClientControlRequest::SupervisorList {},
        )
        .await;
        let subc_control::ClientControlResponse::SupervisorList { modules, .. } = response else {
            panic!("supervisor.list must return its matching response variant");
        };
        let entry = modules
            .iter()
            .find(|module| module.module_id == "ckbus")
            .expect("supervisor.list lists ckbus");
        let exited = !entry.live && entry.state != "running" && !process_exists(pid);
        if !entry.enabled && exited {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the supervised ck-bus (pid {pid}) did not stop: {entry:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether a process with `pid` still exists (`kill -0` succeeds). Off unix the
/// supervisor's report alone is used.
fn process_exists(pid: u32) -> bool {
    if !cfg!(unix) {
        return false;
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The harness signer answered in-process through ck-bus's own vault wire.
struct InProcessSigner(HarnessSigner);

#[async_trait]
impl VaultSigning for InProcessSigner {
    async fn sign(
        &self,
        credential_id: &str,
        payload: &[u8],
    ) -> Result<VaultSignature, VaultError> {
        let body = wire::sign_request(credential_id, payload)
            .map_err(|error| VaultError::Malformed(format!("{error:?}")))?;
        match self.0.answer(&body) {
            HandlerOutcome::Response(bytes) => wire::parse_sign_reply(&bytes)
                .map_err(|error| VaultError::Malformed(format!("{error:?}"))),
            other => Err(VaultError::Malformed(format!("{other:?}"))),
        }
    }

    async fn public_key(&self, credential_id: &str) -> Result<VaultPublicKey, VaultError> {
        match self.0.answer(&wire::public_key_request(credential_id)) {
            HandlerOutcome::Response(bytes) => wire::parse_public_key_reply(&bytes)
                .map_err(|error| VaultError::Malformed(format!("{error:?}"))),
            other => Err(VaultError::Malformed(format!("{other:?}"))),
        }
    }
}

fn credentials_over(signer: HarnessSigner) -> Arc<Credentials> {
    Arc::new(Credentials::new(Arc::new(InProcessSigner(signer))))
}

/// ck-bus's own connection code, in this process, for users the harness signer signs:
/// a system user with the generated system grant, and a bus-module user with `grant`.
async fn plane_with(
    run: &Run,
    box_grant: impl FnOnce(&AccountNames, &str) -> grants::Grant,
) -> RevocationPlane {
    let credentials = credentials_over(run.trust.signer.clone());
    let names = run.names();
    let account_public = run.account_public();
    let broker = NatsBroker::new(run.server.url.clone(), credentials.clone());

    let system_user = credentials.custody.generate_user();
    let system_jwt = sign_user_jwt(
        credentials.vault.as_ref(),
        &credentials.key_ids,
        &UserJwtRequest {
            root_credential_id: &bus::system_root_id(),
            user_public: &system_user,
            issuer_account: Some(&run.trust.system_account),
            name: "revocation-row-system",
            issued_at: unix_now() - 60,
            grant: &grants::system_account_grant(&names, &system_user).unwrap(),
        },
    )
    .await
    .expect("system user JWT");
    let system = broker
        .connect_system(&system_jwt.jwt, &system_user)
        .await
        .unwrap_or_else(|error| panic!("system user connects: {error}"));

    let box_user = credentials.custody.generate_user();
    let box_jwt = sign_user_jwt(
        credentials.vault.as_ref(),
        &credentials.key_ids,
        &UserJwtRequest {
            root_credential_id: &bus::box_root_id(),
            user_public: &box_user,
            issuer_account: Some(&account_public),
            name: "revocation-row-bus-module",
            issued_at: unix_now() - 60,
            grant: &box_grant(&names, &box_user),
        },
    )
    .await
    .expect("bus-module user JWT");
    let box_plane = broker
        .connect_box(&box_jwt.jwt, &box_user)
        .await
        .unwrap_or_else(|error| panic!("bus-module user connects: {error}"));
    RevocationPlane {
        names,
        account_public,
        system,
        box_plane,
    }
}

async fn plane(run: &Run) -> RevocationPlane {
    plane_with(run, |names, user| {
        grants::bus_module_grant(names, user).unwrap()
    })
    .await
}

/// A revoker with fresh memory over `store`, as a restarted process has, following the
/// server's connect events from now on.
async fn process(plane: &RevocationPlane, signer: HarnessSigner, store: &Path) -> Arc<Revoker> {
    let connections = Arc::new(Connections::default());
    let events = plane
        .system
        .watch_connections(&plane.account_public)
        .await
        .expect("the system user watches connections");
    let following = connections.clone();
    tokio::spawn(async move { following.follow(events).await });
    Arc::new(Revoker::new(credentials_over(signer), store, connections))
}

/// Every `$SYS` disconnect event for the box account, as a harness system client sees it.
struct Disconnects {
    seen: Arc<Mutex<Vec<Value>>>,
    _client: async_nats::Client,
}

impl Disconnects {
    async fn watch(run: &Run) -> Self {
        let client = run.system_observer().await;
        let mut sub = client
            .subscribe(format!("$SYS.ACCOUNT.{}.DISCONNECT", run.account_public()))
            .await
            .unwrap();
        client.flush().await.unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recording = seen.clone();
        tokio::spawn(async move {
            while let Some(message) = sub.next().await {
                if let Ok(value) = serde_json::from_slice::<Value>(&message.payload) {
                    recording.lock().unwrap().push(value);
                }
            }
        });
        Self {
            seen,
            _client: client,
        }
    }

    fn for_user(&self, user: &str) -> Vec<Value> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event["client"]["user"] == user || event["client"]["nkey"] == user)
            .cloned()
            .collect()
    }

    /// Waits for the first event for `user`, then a settling second, and returns all.
    async fn settled_for(&self, user: &str) -> Vec<Value> {
        let deadline = Instant::now() + RECONNECT_BOUND;
        while self.for_user(user).is_empty() {
            assert!(
                Instant::now() < deadline,
                "no $SYS disconnect event for {user}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        self.for_user(user)
    }
}

/// A client whose connection events are recorded and which never reconnects by itself,
/// so every connection the server sees from it is one the arm made.
async fn connect_once(
    url: &str,
    jwt: &str,
    sign: impl Fn(Vec<u8>) -> futures_util::future::BoxFuture<'static, Result<Vec<u8>, String>>
        + Send
        + Sync
        + 'static,
) -> Result<(async_nats::Client, Arc<Mutex<Vec<String>>>), async_nats::ConnectError> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = events.clone();
    let sign = Arc::new(sign);
    let client = async_nats::ConnectOptions::with_jwt(jwt.to_string(), move |nonce| {
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
    .max_reconnects(0)
    .connection_timeout(Duration::from_secs(5))
    .connect(url)
    .await?;
    Ok((client, events))
}

fn seed_signer(
    seed: String,
) -> impl Fn(Vec<u8>) -> futures_util::future::BoxFuture<'static, Result<Vec<u8>, String>> + Send + Sync
{
    move |nonce| {
        let seed = seed.clone();
        Box::pin(async move {
            KeyPair::from_seed(&seed)
                .and_then(|pair| pair.sign(&nonce))
                .map_err(|error| error.to_string())
        })
    }
}

/// A harness-held user in the box account, recorded in the census as module `module`
/// at (`generation`, 0), and connected.
struct Victim {
    module: String,
    seed: String,
    public: String,
    jwt: String,
    jti: String,
    generation: u64,
    client: async_nats::Client,
    events: Arc<Mutex<Vec<String>>>,
}

impl Victim {
    async fn enter(run: &Run, plane: &RevocationPlane, module: &str, generation: u64) -> Self {
        let pair = KeyPair::new_user();
        let seed = pair.seed().unwrap();
        let public = pair.public_key();
        let jwt = run.trust.user_jwt(
            &bus::box_root_id(),
            &run.account_public(),
            &pair,
            unix_now() - 60,
        );
        let jti = bus::claims(&jwt)["jti"].as_str().unwrap().to_string();
        let value = CensusValue {
            credential_public: public.clone(),
            user_jwt_id: jti.clone(),
            spawn_generation: generation,
            credential_epoch: 0,
            identities: vec![],
            rooms: vec![],
        };
        plane
            .box_plane
            .census_put(
                &plane.names.census_subject(module).unwrap(),
                value.to_bytes(),
            )
            .await
            .expect("the census entry is written");
        let (client, events) = connect_once(&run.server.url, &jwt, seed_signer(seed.clone()))
            .await
            .expect("the victim connects before its revocation");
        client.flush().await.unwrap();
        Self {
            module: module.to_string(),
            seed,
            public,
            jwt,
            jti,
            generation,
            client,
            events,
        }
    }

    fn target(&self) -> Target {
        Target {
            identity: self.identity(),
            user_public: self.public.clone(),
            user_jwt_id: self.jti.clone(),
        }
    }

    fn identity(&self) -> Identity {
        Identity {
            module_id: self.module.clone(),
            spawn_generation: self.generation,
            credential_epoch: 0,
        }
    }

    /// Waits until the revoker has seen this victim's connect event.
    async fn tracked_by(&self, revoker: &Revoker) {
        let deadline = Instant::now() + RECONNECT_BOUND;
        while revoker.connections().targets(&self.public).is_empty() {
            assert!(
                Instant::now() < deadline,
                "the connect event for {} never arrived",
                self.public
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// "Unavailable on the severed socket": the connection is closed by the server and
    /// the client reports it.
    async fn expect_severed(&self) {
        let deadline = Instant::now() + RECONNECT_BOUND;
        while self.client.connection_state() == async_nats::connection::State::Connected {
            assert!(
                Instant::now() < deadline,
                "the revoked user stayed connected: {:?}",
                self.events.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            self.events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.to_ascii_lowercase().contains("disconnected")),
            "the client reported no disconnect: {:?}",
            self.events.lock().unwrap()
        );
    }

    /// An explicit reconnect with the same JWT and seed is refused as revoked.
    async fn expect_refused(&self, url: &str, server: &BusServer) {
        let started = Instant::now();
        match connect_once(url, &self.jwt, seed_signer(self.seed.clone())).await {
            Ok(_) => panic!("{} reconnected after its revocation", self.public),
            Err(error) => assert_eq!(error.kind(), ConnectErrorKind::AuthorizationViolation),
        }
        assert!(started.elapsed() < RECONNECT_BOUND);
        assert!(
            server.log_text().contains("User authentication revoked"),
            "the server refused the reconnect as revoked"
        );
    }

    async fn expect_still_connected(&self) {
        self.client
            .flush()
            .await
            .expect("an unrevoked client still round-trips");
        assert_eq!(
            self.client.connection_state(),
            async_nats::connection::State::Connected
        );
    }
}

fn revoked_once(revocations: &serde_json::Map<String, Value>, user: &str) {
    assert_eq!(
        revocations
            .keys()
            .filter(|key| key.as_str() == user)
            .count(),
        1,
        "exactly one revocation entry for {user}: {revocations:?}"
    );
}

async fn progress_is_empty(store: &Path) {
    assert_eq!(
        ProgressStore::new(store).list().unwrap(),
        vec![],
        "no revocation record is left"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_three_steps_sever_refuse_and_survive_a_server_restart() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start(false).await else {
        return;
    };
    // This arm's census entries name modules with no live process; the supervised
    // ck-bus would revoke them by reconciliation before this arm's own revoker could.
    retire_supervised_ckbus(&run).await;
    let plane = plane(&run).await;
    let store = tempfile::tempdir().unwrap();
    let revoker = process(&plane, run.trust.signer.clone(), store.path()).await;
    let disconnects = Disconnects::watch(&run).await;
    let victim = Victim::enter(&run, &plane, "victim", 3).await;
    let bystander = Victim::enter(&run, &plane, "bystander", 5).await;
    victim.tracked_by(&revoker).await;
    let before = run.account_jwt().await;

    let completed = revoker
        .revoke_module(&plane, "victim")
        .await
        .expect("the revocation completes")
        .expect("the census names the victim");
    assert!(
        completed.pushed && completed.census_deleted,
        "{completed:?}"
    );

    // Step (1): read back, signed by the operator signer and never by the root.
    let after = run.account_jwt().await;
    assert_ne!(after, before);
    let claims = bus::claims(&after);
    assert_eq!(claims["iss"], run.trust.signer_public());
    assert_ne!(claims["iss"], bus::claims(&run.trust.operator_jwt())["iss"]);
    revoked_once(&run.revocations().await, &victim.public);
    // Step (2): the census key is gone; the bystander's entry is untouched.
    assert_eq!(run.census_value("victim").await, None);
    assert_eq!(
        run.census_value("bystander")
            .await
            .unwrap()
            .credential_public,
        bystander.public
    );
    // The severed socket, one disconnect event, and the refused reconnect. The client
    // runs no census watch of its own; the server refuses it all the same.
    victim.expect_severed().await;
    let events = disconnects.settled_for(&victim.public).await;
    assert_eq!(events.len(), 1, "exactly one disconnect event: {events:?}");
    // The push itself closed the connection (nats-server 2.15.0 names the reason
    // "Credentials Revoked"), so the kick, step (3), found nothing left to close.
    assert_eq!(events[0]["reason"], REVOKED_REASON);
    assert_eq!(completed.kicked, 0);
    victim.expect_refused(&run.server.url, &run.server).await;
    progress_is_empty(store.path()).await;
    // Control: the unrevoked bystander keeps working.
    bystander.expect_still_connected().await;

    // A replay is a no-op on every observable.
    let replay = revoker
        .revoke(&plane, victim.target())
        .await
        .expect("a replay completes");
    assert_eq!(replay, Completed::default());
    assert_eq!(run.account_jwt().await, after, "the claims are unchanged");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(disconnects.for_user(&victim.public).len(), 1);

    // A server restart keeps the refusal: the resolver persisted the claims update.
    let Run {
        trust,
        server,
        run: supervised,
        bin,
        ..
    } = run;
    let dir = server.dir.clone();
    server.stop().await;
    let restarted = BusServer::start(&bin, &dir, &trust, LOOPBACK).await;
    victim.expect_refused(&restarted.url, &restarted).await;
    let bystander_again = connect_once(
        &restarted.url,
        &bystander.jwt,
        seed_signer(bystander.seed.clone()),
    )
    .await;
    assert!(
        bystander_again.is_ok(),
        "control: the unrevoked user reconnects after the restart"
    );

    passed();
    restarted.stop().await;
    supervised.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_at_every_boundary_ends_with_one_revocation_and_one_disconnect() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start(false).await else {
        return;
    };
    // This arm's census entries name modules with no live process; the supervised
    // ck-bus would revoke them by reconciliation before this arm's own revoker could.
    retire_supervised_ckbus(&run).await;
    let plane = plane(&run).await;
    let disconnects = Disconnects::watch(&run).await;
    for (index, boundary) in [
        Boundary::ZeroRecorded,
        Boundary::Step1Done,
        Boundary::Step1Recorded,
        Boundary::Step2Done,
        Boundary::Step2Recorded,
        Boundary::Step3Done,
    ]
    .into_iter()
    .enumerate()
    {
        let store = tempfile::tempdir().unwrap();
        let module = format!("crash{}", ["a", "b", "c", "d", "e", "f"][index]);
        // The process watches connections before the victim connects, so its record
        // carries a kick target.
        let first = process(&plane, run.trust.signer.clone(), store.path()).await;
        let victim = Victim::enter(&run, &plane, &module, 1).await;
        victim.tracked_by(&first).await;
        *first.stop_at.lock().unwrap() = Some(boundary);
        let stopped = first
            .revoke_module(&plane, &module)
            .await
            .expect_err("the act stops at the boundary");
        assert_eq!(stopped, RevocationError::Stopped(boundary));
        assert!(
            ProgressStore::new(store.path())
                .read(&victim.identity())
                .is_some(),
            "{boundary:?}: the record survives the stop"
        );
        drop(first);

        // A fresh process (fresh memory, no connection seen) over the same store.
        let second = process(&plane, run.trust.signer.clone(), store.path()).await;
        let outcomes = second.resume_all(&plane).await;
        assert_eq!(outcomes.len(), 1, "{boundary:?}: {outcomes:?}");
        assert!(outcomes[0].1.is_ok(), "{boundary:?}: {outcomes:?}");
        revoked_once(&run.revocations().await, &victim.public);
        assert_eq!(run.census_value(&module).await, None, "{boundary:?}");
        victim.expect_severed().await;
        let events = disconnects.settled_for(&victim.public).await;
        assert_eq!(events.len(), 1, "{boundary:?}: {events:?}");
        assert_eq!(events[0]["reason"], REVOKED_REASON, "{boundary:?}");
        victim.expect_refused(&run.server.url, &run.server).await;
        progress_is_empty(store.path()).await;

        // Replays are no-ops.
        let jwt = run.account_jwt().await;
        assert!(second.resume_all(&plane).await.is_empty());
        assert_eq!(
            second.revoke(&plane, victim.target()).await,
            Ok(Completed::default()),
            "{boundary:?}"
        );
        assert_eq!(run.account_jwt().await, jwt, "{boundary:?}");
        assert_eq!(
            disconnects.for_user(&victim.public).len(),
            1,
            "{boundary:?}"
        );
    }

    passed();
    run.server.stop().await;
    run.run.shutdown().await;
}

/// A census plane whose reads fail and whose every other call goes to `inner`.
struct UnreadableCensus(Arc<dyn BoxPlane>);

#[async_trait]
impl BoxPlane for UnreadableCensus {
    async fn ensure_census(&self, account: &AccountNames) -> Result<(), PlaneError> {
        self.0.ensure_census(account).await
    }
    async fn ensure_stream(&self, spec: &StreamSpec) -> Result<(), PlaneError> {
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
        _account: &AccountNames,
        key: &str,
    ) -> Result<Option<CensusRecord>, PlaneError> {
        Err(PlaneError::new(format!(
            "census get {key}: the harness made the census unreadable"
        )))
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
        Err(PlaneError::new("UnreadableCensus serves no durable create"))
    }
    async fn consumer_state(
        &self,
        _stream: &str,
        _durable: &str,
    ) -> Result<Option<bootstrap::plane::ConsumerState>, PlaneError> {
        Err(PlaneError::new("UnreadableCensus serves no consumer read"))
    }
    async fn delete_durable(&self, _stream: &str, _durable: &str) -> Result<bool, PlaneError> {
        Err(PlaneError::new("UnreadableCensus serves no durable delete"))
    }
    async fn purge_subject(&self, _stream: &str, _filter_subject: &str) -> Result<u64, PlaneError> {
        Err(PlaneError::new("UnreadableCensus serves no purge"))
    }
    async fn consumer_names(&self, _stream: &str) -> Result<Vec<String>, PlaneError> {
        Err(PlaneError::new("UnreadableCensus serves no consumer listing"))
    }
}

/// Enters a victim for `module`, stops its revocation at `boundary`, then damages its
/// record. Returns the victim and the damaged record's path.
async fn stop_and_damage(
    plane: &RevocationPlane,
    run: &Run,
    module: &str,
    store: &Path,
    boundary: Boundary,
) -> (Victim, PathBuf) {
    let revoker = process(plane, run.trust.signer.clone(), store).await;
    let victim = Victim::enter(run, plane, module, 2).await;
    victim.tracked_by(&revoker).await;
    *revoker.stop_at.lock().unwrap() = Some(boundary);
    revoker
        .revoke_module(plane, &victim.module)
        .await
        .expect_err("the act stops at the boundary");
    let path = ProgressStore::new(store).path(&victim.identity());
    std::fs::write(&path, b"{\"highest_completed_step\": 1").unwrap();
    (victim, path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_damaged_record_is_recovered_from_the_census_or_deferred() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start(false).await else {
        return;
    };
    // This arm's census entries name modules with no live process; the supervised
    // ck-bus would revoke them by reconciliation before this arm's own revoker could.
    retire_supervised_ckbus(&run).await;
    let plane = plane(&run).await;
    let disconnects = Disconnects::watch(&run).await;

    // (ii) Damaged after step (1), census entry present: re-derived and replayed.
    let store = tempfile::tempdir().unwrap();
    let (victim, _) = stop_and_damage(
        &plane,
        &run,
        "damagedone",
        store.path(),
        Boundary::Step1Recorded,
    )
    .await;
    let jwt = run.account_jwt().await;
    let recovering = process(&plane, run.trust.signer.clone(), store.path()).await;
    let outcomes = recovering.resume_all(&plane).await;
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    let completed = outcomes[0].1.clone().expect("recovery replays the steps");
    assert!(!completed.pushed, "step (1) had committed: {completed:?}");
    assert!(completed.census_deleted, "{completed:?}");
    assert_eq!(run.account_jwt().await, jwt);
    revoked_once(&run.revocations().await, &victim.public);
    assert_eq!(run.census_value(&victim.module).await, None);
    victim.expect_severed().await;
    assert_eq!(disconnects.settled_for(&victim.public).await.len(), 1);
    progress_is_empty(store.path()).await;

    // (iii) Damaged after step (2): cleared, and nothing is pushed.
    let store = tempfile::tempdir().unwrap();
    let (victim, _) = stop_and_damage(
        &plane,
        &run,
        "damagedtwo",
        store.path(),
        Boundary::Step2Recorded,
    )
    .await;
    let jwt = run.account_jwt().await;
    let recovering = process(&plane, run.trust.signer.clone(), store.path()).await;
    let outcomes = recovering.resume_all(&plane).await;
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0].1, Ok(Completed::default()));
    assert_eq!(run.account_jwt().await, jwt, "nothing was pushed");
    revoked_once(&run.revocations().await, &victim.public);
    progress_is_empty(store.path()).await;

    // (iv) Damaged with the census read failing: deferred, the file left as it is.
    let store = tempfile::tempdir().unwrap();
    let (victim, path) = stop_and_damage(
        &plane,
        &run,
        "damagedthree",
        store.path(),
        Boundary::Step1Recorded,
    )
    .await;
    let damaged = std::fs::read(&path).unwrap();
    let unreadable = RevocationPlane {
        box_plane: Arc::new(UnreadableCensus(plane.box_plane.clone())),
        ..plane.clone()
    };
    let recovering = process(&unreadable, run.trust.signer.clone(), store.path()).await;
    let outcomes = recovering.resume_all(&unreadable).await;
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    let Err(RevocationError::Deferred { cause: why, .. }) = &outcomes[0].1 else {
        panic!("recovery must defer: {outcomes:?}");
    };
    assert_eq!(*why, cause::CENSUS_READ_FAILED);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        damaged,
        "the file is untouched"
    );
    // Once the census reads again, the same record recovers.
    let outcomes = recovering.resume_all(&plane).await;
    assert!(outcomes[0].1.is_ok(), "{outcomes:?}");
    assert_eq!(run.census_value(&victim.module).await, None);
    progress_is_empty(store.path()).await;

    passed();
    run.server.stop().await;
    run.run.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusals_defer_their_step_and_a_kick_alone_revokes_nothing() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start(false).await else {
        return;
    };
    // This arm's census entries name modules with no live process; the supervised
    // ck-bus would revoke them by reconciliation before this arm's own revoker could.
    retire_supervised_ckbus(&run).await;
    let plane = plane(&run).await;

    // A refused operator-key signature: step (1) fails, nothing is pushed, and the
    // census entry is left as it is.
    let store = tempfile::tempdir().unwrap();
    let victim = Victim::enter(&run, &plane, "refusedsigner", 4).await;
    let jwt = run.account_jwt().await;
    let refusing = process(
        &plane,
        run.trust.signer.without(&bus::signer_root_id()),
        store.path(),
    )
    .await;
    let refused = refusing
        .revoke_module(&plane, &victim.module)
        .await
        .expect_err("a refused signature fails step (1)");
    let RevocationError::Deferred {
        completed_step,
        cause: why,
        ..
    } = &refused
    else {
        panic!("expected a deferral, got {refused:?}");
    };
    assert_eq!(
        (*completed_step, *why),
        (0, cause::OPERATOR_SIGNATURE_REFUSED)
    );
    assert_eq!(run.account_jwt().await, jwt, "nothing was pushed");
    assert_eq!(
        run.census_value(&victim.module)
            .await
            .unwrap()
            .credential_public,
        victim.public,
        "the census entry is untouched"
    );
    victim.expect_still_connected().await;

    // A bus-module user generated without census read: step (1) cannot read its input
    // and fails outright, writing nothing.
    let without_read = plane_with(&run, |names, user| {
        let census_stream = names.buckets().census_stream.clone();
        let entries = grants::bus_module_grant(names, user)
            .unwrap()
            .allow_entries()
            .into_iter()
            .filter(|entry| {
                let read_only_census = [
                    format!("$JS.API.DIRECT.GET.{census_stream}"),
                    format!("$JS.API.STREAM.MSG.GET.{census_stream}"),
                    format!("$JS.API.STREAM.INFO.{census_stream}"),
                    format!("$JS.API.CONSUMER.CREATE.{census_stream}"),
                    format!("$JS.API.CONSUMER.DURABLE.CREATE.{census_stream}"),
                    format!("$JS.API.CONSUMER.DELETE.{census_stream}"),
                ];
                let reads = entry.operation == Operation::Subscribe
                    && entry.subject.starts_with("$KV.")
                    || read_only_census
                        .iter()
                        .any(|prefix| entry.subject.starts_with(prefix.as_str()));
                !reads
            })
            .collect();
        grants::Grant::from_entries(grants::GrantRole::BusModule, names, entries)
            .expect("the grant without census read is a valid grant")
    })
    .await;
    let store = tempfile::tempdir().unwrap();
    let blind = process(&without_read, run.trust.signer.clone(), store.path()).await;
    let refused = blind
        .revoke_module(&without_read, &victim.module)
        .await
        .expect_err("no census read, no revocation");
    assert!(
        matches!(refused, RevocationError::CensusUnreadable(_)),
        "{refused:?}"
    );
    assert_eq!(run.account_jwt().await, jwt, "nothing was pushed");
    progress_is_empty(store.path()).await;
    victim.expect_still_connected().await;

    // A kick without the claims update closes one socket and revokes nothing: the
    // same JWT reconnects.
    let kicked = bus::box_client(&run.trust, &run.server, &run.account_public()).await;
    kicked.flush().await.unwrap();
    let info = kicked.server_info();
    plane
        .system
        .kick(&info.server_id, info.client_id)
        .await
        .expect("the kick is answered");
    let deadline = Instant::now() + RECONNECT_BOUND;
    loop {
        let reconnected = kicked.server_info().client_id != info.client_id
            && kicked.connection_state() == async_nats::connection::State::Connected;
        if reconnected {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the kicked client never reconnected"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    kicked.flush().await.expect("the kicked client works again");
    assert_eq!(run.account_jwt().await, jwt, "a kick pushes nothing");

    // A kick refused for any reason but the server's "no such client" answer fails
    // step (3): the record stays at step (2) for the next pass. The target names a
    // server that does not exist, so the kick request has no responder.
    let store = tempfile::tempdir().unwrap();
    let progress = ProgressStore::new(store.path());
    let stuck = Record {
        identity: Identity {
            module_id: "kickrefused".to_string(),
            spawn_generation: 1,
            credential_epoch: 0,
        },
        highest_completed_step: 2,
        user_public: KeyPair::new_user().public_key(),
        user_jwt_id: "JTI".to_string(),
        kick: BTreeSet::from([KickTarget {
            server_id: "NNOSUCHSERVER".to_string(),
            client_id: 1,
        }]),
    };
    progress.write(&stuck).unwrap();
    let resuming = process(&plane, run.trust.signer.clone(), store.path()).await;
    let outcomes = resuming.resume_all(&plane).await;
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    let Err(RevocationError::Deferred {
        completed_step,
        cause: why,
        ..
    }) = &outcomes[0].1
    else {
        panic!("a refused kick must defer: {outcomes:?}");
    };
    assert_eq!((*completed_step, *why), (2, cause::KICK_FAILED));
    assert_eq!(
        progress.read(&stuck.identity),
        Some(progress::Entry::Present(stuck.clone())),
        "the record stays at step (2)"
    );

    passed();
    run.server.stop().await;
    run.run.shutdown().await;
}

async fn credential(run: &SignerRun) -> Value {
    rows::relay(
        &run.connection_file,
        true,
        issuance::CREDENTIAL_OP,
        json!({}),
    )
    .await
    .expect("ckbus.credential answers")
}

/// Connects as the participant's credential in `answer`, the nonce signed by ck-bus
/// through the participant's own `ckbus.nonce_sign`.
async fn connect_participant(
    run: &Run,
    answer: &Value,
) -> Result<(async_nats::Client, Arc<Mutex<Vec<String>>>), async_nats::ConnectError> {
    let connection_file = run.run.connection_file.clone();
    let credential_public = answer["credential_public"].as_str().unwrap().to_string();
    connect_once(
        &run.server.url,
        answer["jwt"].as_str().unwrap(),
        move |nonce: Vec<u8>| {
            let connection_file = connection_file.clone();
            let credential_public = credential_public.clone();
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
            })
        },
    )
    .await
}

/// The `ckbus.revocation.completed` line for `user`, waiting for it.
async fn completed_for(run: &Run, user: &str) -> Value {
    let deadline = Instant::now() + BOOT_LIMIT;
    loop {
        if let Some(line) = bus::events(run.run.root.path(), "ckbus.revocation.completed")
            .into_iter()
            .find(|line| line["user_public"] == user)
        {
            return line;
        }
        assert!(
            Instant::now() < deadline,
            "no completed revocation for {user}; deferrals: {:?}",
            bus::events(run.run.root.path(), "ckbus.revocation.deferred")
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_severed(client: &async_nats::Client) {
    let deadline = Instant::now() + RECONNECT_BOUND;
    while client.connection_state() == async_nats::connection::State::Connected {
        assert!(
            Instant::now() < deadline,
            "the revoked user stayed connected"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_ckbus_restarts_and_revokes_the_superseded_user_it_finds_in_the_census() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start(true).await else {
        return;
    };
    let root = run.run.root.path().to_path_buf();
    bus::wait_event(&root, "ckbus.revocation.watching", 1, BOOT_LIMIT).await;
    let disconnects = Disconnects::watch(&run).await;
    let first = credential(&run.run).await;
    let first_key = first["credential_public"].as_str().unwrap().to_string();
    let (first_client, _) = connect_participant(&run, &first)
        .await
        .expect("the participant connects through ckbus.nonce_sign");
    first_client.flush().await.unwrap();

    // A real kill of the supervised process; the supervisor starts a new one.
    let pid = run.run.supervised_pid("ckbus").await;
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    bus::wait_event(&root, "ckbus.bootstrap.ready", 2, BOOT_LIMIT).await;
    bus::wait_event(&root, "ckbus.revocation.watching", 2, BOOT_LIMIT).await;
    assert_ne!(run.run.supervised_pid("ckbus").await, pid);

    // The established connection survives, and the restarted process revokes nothing
    // by itself: it cannot sign for the user, but revoking would disconnect it.
    first_client
        .flush()
        .await
        .expect("an established connection survives the kill");
    assert!(!run.revocations().await.contains_key(&first_key));

    // Control: ck-bus merely no longer holding the key fails the next connect inside
    // ckbus.nonce_sign, and the server never sees a credential for it.
    let refusals_before = run
        .server
        .log_text()
        .matches("Authorization Violation")
        .count();
    let (code, _) = rows::relay(
        &run.run.connection_file,
        true,
        issuance::NONCE_SIGN_OP,
        json!({"nonce_b64": STANDARD.encode(b"nonce"), "credential_public": first_key}),
    )
    .await
    .expect_err("ck-bus holds no key for the first credential");
    assert_eq!(code, issuance::code::CREDENTIAL_SUPERSEDED);
    let local = connect_participant(&run, &first)
        .await
        .expect_err("a key ck-bus no longer holds cannot connect");
    // The client's own signing callback failed (its nonce_sign got the refusal above);
    // a server refusal would be an authorization violation instead.
    assert_eq!(local.kind(), ConnectErrorKind::Authentication, "{local:?}");
    assert_eq!(
        run.server
            .log_text()
            .matches("Authorization Violation")
            .count(),
        refusals_before,
        "the refusal happened in ck-bus, not at the server"
    );

    // The refetch revokes the user the census named, found by the restarted process.
    let second = credential(&run.run).await;
    let second_key = second["credential_public"].as_str().unwrap().to_string();
    assert_ne!(second_key, first_key);
    let revoked = completed_for(&run, &first_key).await;
    assert_eq!(revoked["pushed"], true);
    // The superseded entry had been overwritten by the refetch: nothing to delete.
    assert_eq!(revoked["census_deleted"], false);
    // It connected before this process watched, so nothing was known to kick; the
    // revocation list alone closes its live connection.
    assert_eq!(revoked["kick_targets"], 0);
    assert_eq!(revoked["kicked"], 0);
    revoked_once(&run.revocations().await, &first_key);
    wait_severed(&first_client).await;
    let events = disconnects.settled_for(&first_key).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["reason"], REVOKED_REASON);
    assert_eq!(
        run.census_value(PARTICIPANT)
            .await
            .unwrap()
            .credential_public,
        second_key
    );

    // A user whose connection the restarted process did see is recorded with its kick
    // target when a later refetch supersedes it.
    let (second_client, _) = connect_participant(&run, &second)
        .await
        .expect("the refetched credential connects");
    second_client.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let third = credential(&run.run).await;
    let revoked = completed_for(&run, &second_key).await;
    assert_eq!(revoked["kick_targets"], 1);
    revoked_once(&run.revocations().await, &second_key);
    wait_severed(&second_client).await;
    assert_eq!(disconnects.settled_for(&second_key).await.len(), 1);
    let (third_client, _) = connect_participant(&run, &third)
        .await
        .expect("the current credential connects");
    third_client
        .flush()
        .await
        .expect("the current credential works");

    passed();
    run.server.stop().await;
    run.run.shutdown().await;
}

/// A system plane holding one stored account JWT, recording every update. Nothing in
/// the foreign-claim arm lists accounts, kicks or watches, so those calls fail loudly.
struct StoredAccount {
    jwt: Mutex<String>,
    updates: Mutex<Vec<String>>,
}

#[async_trait]
impl SystemPlane for StoredAccount {
    async fn list_accounts(&self) -> Result<Vec<String>, PlaneError> {
        Err(PlaneError::new("StoredAccount serves no claims list"))
    }
    async fn lookup(&self, _account_public: &str) -> Result<Option<String>, PlaneError> {
        Ok(Some(self.jwt.lock().unwrap().clone()))
    }
    async fn update(&self, jwt: &str) -> Result<(), PlaneError> {
        self.updates.lock().unwrap().push(jwt.to_string());
        *self.jwt.lock().unwrap() = jwt.to_string();
        Ok(())
    }
    async fn kick(&self, _server_id: &str, _client_id: u64) -> Result<(), PlaneError> {
        Err(PlaneError::new("StoredAccount serves no kick"))
    }
    async fn watch_connections(
        &self,
        _account_public: &str,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<ConnectionEvent>, PlaneError> {
        Err(PlaneError::new("StoredAccount serves no connection events"))
    }
}

/// A box plane whose census holds nothing; every other call fails loudly.
struct EmptyCensus;

#[async_trait]
impl BoxPlane for EmptyCensus {
    async fn ensure_census(&self, _account: &AccountNames) -> Result<(), PlaneError> {
        Err(PlaneError::new("EmptyCensus creates nothing"))
    }
    async fn ensure_stream(&self, _spec: &StreamSpec) -> Result<(), PlaneError> {
        Err(PlaneError::new("EmptyCensus creates nothing"))
    }
    async fn publish(&self, _subject: &str, _payload: Vec<u8>) -> Result<(), PlaneError> {
        Err(PlaneError::new("EmptyCensus publishes nothing"))
    }
    async fn census_put(&self, _subject: &str, _value: Vec<u8>) -> Result<(), PlaneError> {
        Err(PlaneError::new("EmptyCensus writes nothing"))
    }
    async fn census_get(
        &self,
        _account: &AccountNames,
        _key: &str,
    ) -> Result<Option<CensusRecord>, PlaneError> {
        Ok(None)
    }
    async fn census_delete(
        &self,
        _account: &AccountNames,
        _key: &str,
        _revision: u64,
    ) -> Result<(), PlaneError> {
        Err(PlaneError::new("EmptyCensus deletes nothing"))
    }
    async fn create_durable(&self, _durable: &DurableConsumer) -> Result<(), PlaneError> {
        Err(PlaneError::new("EmptyCensus serves no durable create"))
    }
    async fn consumer_state(
        &self,
        _stream: &str,
        _durable: &str,
    ) -> Result<Option<bootstrap::plane::ConsumerState>, PlaneError> {
        Err(PlaneError::new("EmptyCensus serves no consumer read"))
    }
    async fn delete_durable(&self, _stream: &str, _durable: &str) -> Result<bool, PlaneError> {
        Err(PlaneError::new("EmptyCensus serves no durable delete"))
    }
    async fn purge_subject(&self, _stream: &str, _filter_subject: &str) -> Result<u64, PlaneError> {
        Err(PlaneError::new("EmptyCensus serves no purge"))
    }
    async fn consumer_names(&self, _stream: &str) -> Result<Vec<String>, PlaneError> {
        Err(PlaneError::new("EmptyCensus serves no consumer listing"))
    }
}

/// `jwt` with its claims replaced by `claims`; the signature is kept, since step (1)
/// reads the looked-up claims without checking it.
fn with_claims(jwt: &str, claims: &Value) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let parts: Vec<&str> = jwt.split('.').collect();
    format!(
        "{}.{}.{}",
        parts[0],
        URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes()),
        parts[2]
    )
}

#[tokio::test]
async fn a_claim_ck_bus_did_not_write_refuses_the_push_and_is_named() {
    use bootstrap::account_jwt::{sign_account_jwt, AccountClaims};

    let trust = TrustChain::generate();
    let credentials = credentials_over(trust.signer.clone());
    let account_public = KeyPair::new_account().public_key();
    let written = AccountClaims {
        account_public: account_public.clone(),
        name: "box_foreignclaim".to_string(),
        signing_keys: vec![trust.box_root_public()],
        revocations: Default::default(),
        issued_at: unix_now() - 60,
    };
    let own_jwt = sign_account_jwt(
        credentials.vault.as_ref(),
        &credentials.key_ids,
        &bus::signer_root_id(),
        &written,
    )
    .await
    .expect("the operator signer signs the account JWT");
    let mut foreign = bus::claims(&own_jwt);
    foreign["nats"]["imports"] = json!([{
        "name": "planted",
        "subject": "planted.>",
        "account": KeyPair::new_account().public_key(),
        "type": "stream",
    }]);
    let target = |module: &str| Target {
        identity: Identity {
            module_id: module.to_string(),
            spawn_generation: 1,
            credential_epoch: 0,
        },
        user_public: KeyPair::new_user().public_key(),
        user_jwt_id: "JTI".to_string(),
    };
    let revoke = |stored: Arc<StoredAccount>, module: &'static str| {
        let credentials = credentials.clone();
        let account_public = account_public.clone();
        async move {
            let store = tempfile::tempdir().unwrap();
            let plane = RevocationPlane {
                names: grants::derive_account("box_foreignclaim").unwrap(),
                account_public,
                system: stored,
                box_plane: Arc::new(EmptyCensus),
            };
            Revoker::new(credentials, store.path(), Arc::new(Connections::default()))
                .revoke(&plane, target(module))
                .await
        }
    };

    // Control: the account JWT as ck-bus wrote it is updated.
    let own = Arc::new(StoredAccount {
        jwt: Mutex::new(own_jwt.clone()),
        updates: Mutex::new(Vec::new()),
    });
    revoke(own.clone(), "ownclaims")
        .await
        .expect("an account JWT with only ck-bus's own claims is updated");
    assert_eq!(own.updates.lock().unwrap().len(), 1);

    // A claim ck-bus did not write: nothing is pushed, and the claim is named.
    let planted = Arc::new(StoredAccount {
        jwt: Mutex::new(with_claims(&own_jwt, &foreign)),
        updates: Mutex::new(Vec::new()),
    });
    let refused = revoke(planted.clone(), "foreignclaims")
        .await
        .expect_err("a foreign claim refuses the push");
    let RevocationError::Deferred {
        completed_step,
        cause: why,
        message,
    } = &refused
    else {
        panic!("expected a deferral, got {refused:?}");
    };
    assert_eq!(
        (*completed_step, *why),
        (0, cause::ACCOUNT_JWT_FOREIGN_CLAIM)
    );
    assert!(message.contains("nats.imports"), "{message}");
    assert!(
        planted.updates.lock().unwrap().is_empty(),
        "nothing was pushed"
    );
    passed_in_process();
}

fn passed_in_process() {
    RowReport::passed(Row::Revocation)
        .served_by(ServedBy::HarnessSigner)
        .emit(&vocabulary());
}

/// ck-bus's own census entry is written by bootstrap, which cannot name issuance's type;
/// it must stay in the layout issuance's reader accepts.
#[test]
fn bootstraps_own_census_value_is_a_census_value() {
    let user = KeyPair::new_user().public_key();
    let value = bootstrap::own_census_value(&user, "JTI", 7);
    let parsed = CensusValue::parse(&serde_json::to_vec(&value).unwrap())
        .expect("issuance's census reader accepts bootstrap's own value");
    assert_eq!(
        parsed,
        CensusValue {
            credential_public: user,
            user_jwt_id: "JTI".to_string(),
            spawn_generation: 7,
            credential_epoch: 0,
            identities: vec![],
            rooms: vec![],
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_operator_signer_grant_authorizes_the_revocation_signature() {
    use credentials::{
        nkey::{encode_public, NkeyRole},
        roots::KeyIdLedger,
        vault::ClaustrumRoute,
    };
    use harness::signer::{
        claustrum::{RealClaustrum, CLAUSTRUM_BINARY_ABSENT},
        seeds,
    };
    use subc_client_rs::ConsumerIdentity;

    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let tree = SignerRun::tree();
    let real = match RealClaustrum::discover(&SignerRun::data_home(&tree), &tree.join("keys")) {
        Ok(real) => real,
        Err(observation) => {
            RowReport::skipped(
                Row::VaultAuthorization,
                CLAUSTRUM_BINARY_ABSENT,
                format!("revocation's operator-signer arm: {observation}"),
            )
            .served_by(ServedBy::ClaustrumBinary)
            .emit(&BTreeSet::new());
            return;
        }
    };
    let signer_id = bus::signer_root_id();
    std::fs::create_dir_all(tree.join("keys")).unwrap();
    let (signer_hex, signer_key_id) = real.bootstrap_and_mint(&signer_id);
    real.grant(&signer_id, "sign");
    real.grant(&signer_id, "read");

    let run = SignerRun::start(
        tree,
        Path::new(env!("CARGO_BIN_EXE_ck-bus")),
        ClaustrumSide::Binary(&real),
    )
    .await;
    let pid = run.supervised_pid("ckbus").await;
    let identity = ConsumerIdentity {
        module_id: "ckbus".to_string(),
        launch_nonce: seeds::environment_value(
            &seeds::process_environment(pid),
            "SUBC_LAUNCH_NONCE",
        )
        .expect("the supervised ckbus carries SUBC_LAUNCH_NONCE"),
    };
    let revocation_list = bootstrap::account_jwt::AccountClaims {
        account_public: KeyPair::new_account().public_key(),
        name: "box_revocationgrant".to_string(),
        signing_keys: vec![KeyPair::new_account().public_key()],
        revocations: [(KeyPair::new_user().public_key(), unix_now())]
            .into_iter()
            .collect(),
        issued_at: unix_now(),
    };

    // Reserved: ck-bus's exact grants on the operator signer sign the revocation list,
    // issued by the signer (O...), verifiable under the vault's public key.
    let reserved = ClaustrumRoute::new(run.connection_file.clone(), Some(identity));
    let jwt = bootstrap::account_jwt::sign_account_jwt(
        &reserved,
        &KeyIdLedger::new(),
        &signer_id,
        &revocation_list,
    )
    .await
    .expect("reserved:ckbus signs the revocation list with the operator signer");
    let claims = bootstrap::account_jwt::verify_self_named_issuer(&jwt)
        .expect("the signature verifies under the named issuer");
    let signer_public: [u8; 32] = wire::decode_hex_lower(&signer_hex)
        .and_then(|bytes| bytes.try_into().ok())
        .expect("the printed public key is 32 bytes of hex");
    assert_eq!(
        claims["iss"],
        encode_public(NkeyRole::Operator, &signer_public)
    );
    assert_eq!(wire::key_id_for(&signer_public), signer_key_id);

    // Direct twin: the same call without ck-bus's identity gets not_found.
    let direct = ClaustrumRoute::new(run.connection_file.clone(), None);
    let refused = bootstrap::account_jwt::sign_account_jwt(
        &direct,
        &KeyIdLedger::new(),
        &signer_id,
        &revocation_list,
    )
    .await
    .expect_err("a Direct caller cannot sign with the operator signer");
    assert!(
        matches!(
            refused,
            bootstrap::account_jwt::AccountSignError::Vault(VaultError::RootKeyUnreachable { .. })
        ),
        "{refused:?}"
    );

    run.shutdown().await;
    RowReport::passed(Row::VaultAuthorization)
        .served_by(ServedBy::ClaustrumBinary)
        .asserts_authorization()
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&vocabulary());
}
