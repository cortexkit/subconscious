//! Ladder row "Install bootstrap and own users" (slice 4 of
//! `docs/specs/ck-bus-module.md`, with the first-boot rules of
//! `docs/designs/nats-install-trust-chain.md`).
//!
//! Serving sides, per arm: the boot, restart and kick arms are served by the
//! harness-signer; the refusing-signer control by the harness-stub; the loopback arm by
//! none (it opens no Claustrum or Callosum route).
//!
//! From an empty broker store and a vault holding only the roots, a supervised ck-bus
//! issues its own system and box users, creates the census bucket and the five streams
//! with their literal bindings, and publishes on its sentinel subject. A restart writes
//! a new `own_users.json` and revokes the previous incarnation's box user: the claims
//! read back carry its key, and a client presenting a JWT for it is refused as revoked.
//! The previous system user is not revoked (its seed died with that process). ck-bus's
//! system user kicks a harness client. Controls: with the signer refusing, ck-bus
//! creates nothing and retries once per sentinel period; with the system account root
//! refusing, it answers `sysaccount-absent` and builds no plane; and the local listener
//! refuses a connect from a non-loopback address.
//!
//! ck-bus's own census key needs the census key grammar, which the naming crate does not
//! construct at the pinned revision: that arm records `naming-constructor-absent`.

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
    net::{IpAddr, TcpStream, UdpSocket},
    path::Path,
    sync::Arc,
    time::Duration,
};

use async_nats::{jetstream, ConnectErrorKind};
use bootstrap::{
    cause,
    plane::{Broker, NatsBroker},
};
use cortexkit_bus_naming::{shipped_streams, AccountNames};
use credentials::{
    issue::{sign_user_jwt, UserJwtRequest},
    vault::ClaustrumRoute,
    Credentials,
};
use futures_util::StreamExt;
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    report::{Row, RowReport, ServedBy},
    signer::{
        nats::{nats_server_bin, unix_now},
        run::{ClaustrumSide, RunOptions, SignerRun},
        SIGNER_OPERATIONS,
    },
    stubs::CLAUSTRUM_OPERATIONS,
};
use nkeys::KeyPair;
use serde_json::Value;
use sha2::{Digest, Sha256};

const BOOT_LIMIT: Duration = Duration::from_secs(60);
const PERIOD_MS: u64 = 500;

fn vocabulary(operations: &[&str]) -> BTreeSet<String> {
    operations.iter().map(|op| (*op).to_string()).collect()
}

fn signer_passed() {
    RowReport::passed(Row::InstallBootstrap)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&vocabulary(SIGNER_OPERATIONS));
}

fn fresh_machine_id() -> String {
    let seed = format!("{:?}{}", std::time::SystemTime::now(), std::process::id());
    Sha256::digest(seed.as_bytes())[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn server_bin() -> Option<std::path::PathBuf> {
    match nats_server_bin() {
        Ok((bin, _)) => Some(bin),
        Err((gate, observation)) => {
            RowReport::skipped(Row::InstallBootstrap, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&vocabulary(SIGNER_OPERATIONS));
            None
        }
    }
}

struct Plane {
    trust: TrustChain,
    server: BusServer,
    run: SignerRun,
    machine_id: String,
}

/// Starts the server and a supervised ck-bus whose `claustrum` is `side`.
async fn start(side: impl FnOnce(&TrustChain) -> ClaustrumSide<'static>) -> Option<Plane> {
    let bin = server_bin()?;
    let trust = TrustChain::generate();
    let root = SignerRun::tree();
    let server = BusServer::start(&bin, &root.join("nats"), &trust, LOOPBACK).await;
    let machine_id = fresh_machine_id();
    let mut env = server.ckbus_env();
    env.push((
        "CKBUS_SENTINEL_PERIOD_MS".to_string(),
        PERIOD_MS.to_string(),
    ));
    let run = SignerRun::start_with(
        root,
        Path::new(env!("CARGO_BIN_EXE_ck-bus")),
        side(&trust),
        RunOptions {
            ckbus_env: env,
            machine_id: Some(machine_id.clone()),
        },
    )
    .await;
    Some(Plane {
        trust,
        server,
        run,
        machine_id,
    })
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

/// Subscribes and returns once a marker published on the same connection came back, so
/// the subscription is registered before anyone else publishes.
async fn subscribe_confirmed(client: &async_nats::Client, subject: &str) -> async_nats::Subscriber {
    let mut sub = client.subscribe(subject.to_string()).await.unwrap();
    let marker = format!("marker-{}", client.new_inbox());
    client
        .publish(subject.to_string(), marker.clone().into())
        .await
        .unwrap();
    client.flush().await.unwrap();
    let message = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("the marker comes back")
        .expect("subscription open");
    assert_eq!(message.payload.as_ref(), marker.as_bytes());
    sub
}

fn own_users(plane: &Plane) -> (Vec<String>, Vec<String>, Vec<String>) {
    let value = bus::read_json(&bus::own_users_json(plane.run.root.path()));
    let list = |name: &str| -> Vec<String> {
        value[name]
            .as_array()
            .unwrap_or_else(|| panic!("own_users.json has {name}"))
            .iter()
            .map(|key| key.as_str().unwrap().to_string())
            .collect()
    };
    (list("box"), list("system"), list("pending_revocation"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_boot_builds_the_plane_and_a_restart_revokes_the_previous_box_user() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(|trust| ClaustrumSide::Signer(trust.signer.clone())).await else {
        return;
    };
    let first = ready(&plane, 1).await;
    let (box_users, system_users, pending) = own_users(&plane);
    assert_eq!(
        box_users,
        vec![first["box_user"].as_str().unwrap().to_string()]
    );
    assert_eq!(
        system_users,
        vec![first["system_user"].as_str().unwrap().to_string()]
    );
    assert!(pending.is_empty());
    let old_box = box_users[0].clone();
    let old_system = system_users[0].clone();

    // The previous incarnation's box user can never be presented: its seed died with
    // that process, and the server verifies the nonce before it checks revocation. So
    // the harness also records, as a previous box user, a key whose seed it holds and
    // which is connected now; revoking it is what the server can be seen to enforce.
    let planted = KeyPair::new_user();
    let planted_jwt = plane.trust.user_jwt(
        &bus::box_root_id(),
        first["account_public"].as_str().unwrap(),
        &planted,
        unix_now() - 300,
    );
    let planted_seed = planted.seed().unwrap();
    let planted_client = bus::connect(
        &plane.server.url,
        planted_jwt.clone(),
        KeyPair::from_seed(&planted_seed).unwrap(),
    )
    .await
    .expect("the planted user connects before the restart");
    let mut recorded = bus::read_json(&bus::own_users_json(plane.run.root.path()));
    recorded["box"]
        .as_array_mut()
        .unwrap()
        .push(Value::String(planted.public_key()));
    std::fs::write(
        bus::own_users_json(plane.run.root.path()),
        serde_json::to_vec(&recorded).unwrap(),
    )
    .unwrap();

    let acct = format!("box_{}", plane.machine_id);
    let names = AccountNames::derive(&acct).unwrap();
    let account = first["account_public"].as_str().unwrap().to_string();
    let client = bus::box_client(&plane.trust, &plane.server, &account).await;
    let js = jetstream::new(client.clone());
    let mut expected: Vec<(String, Vec<String>)> = shipped_streams(&names)
        .into_iter()
        .map(|spec| (spec.name, spec.subjects))
        .collect();
    expected.push((
        names.buckets().census_stream.clone(),
        vec![format!("$KV.{}.>", names.buckets().census)],
    ));
    for (name, subjects) in &expected {
        let mut stream = js
            .get_stream(name)
            .await
            .unwrap_or_else(|error| panic!("{name} must exist: {error}"));
        assert_eq!(
            &stream.info().await.unwrap().config.subjects,
            subjects,
            "{name} carries its literal binding"
        );
    }
    let skipped = bus::events(plane.run.root.path(), "ckbus.bootstrap.census_key_skipped");
    assert_eq!(skipped.last().unwrap()["gate"], "naming-constructor-absent");

    // The sentinel publish of the next boot is observed on a confirmed subscription.
    let mut sentinel = subscribe_confirmed(&client, &names.sentinel_ping()).await;
    bus::restart_ckbus(&plane.run.connection_file).await;
    let second = ready(&plane, 2).await;
    let published = tokio::time::timeout(Duration::from_secs(10), sentinel.next())
        .await
        .expect("ck-bus publishes on its sentinel subject")
        .unwrap();
    let body: Value = serde_json::from_slice(&published.payload).unwrap();
    assert_eq!(body["incarnation"], second["incarnation"]);

    let (box_users, system_users, pending) = own_users(&plane);
    assert_eq!(
        box_users,
        vec![second["box_user"].as_str().unwrap().to_string()]
    );
    assert_eq!(
        system_users,
        vec![second["system_user"].as_str().unwrap().to_string()]
    );
    assert!(pending.is_empty(), "the revocation was read back");
    assert_ne!(box_users[0], old_box, "a restart issues a new box user");

    // The claims read back carry the previous box user, and not the previous system
    // user, which is never revoked.
    let system = bus::system_client(&plane.trust, &plane.server).await;
    let claims = bus::claims(&bus::lookup(&system, &account).await.unwrap());
    let revoked = claims["nats"]["revocations"]
        .as_object()
        .expect("the account JWT carries revocations");
    assert!(revoked.contains_key(&old_box), "{revoked:?}");
    assert!(revoked.contains_key(&planted.public_key()), "{revoked:?}");
    assert!(!revoked.contains_key(&box_users[0]));
    assert!(!revoked.contains_key(&old_system));

    // The server enforces it: the planted user was disconnected by the push, and its
    // JWT, presented again with its real seed, is refused as revoked.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while planted_client.connection_state() == async_nats::connection::State::Connected {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the revoked user stayed connected"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let control_user = KeyPair::new_user();
    let control_jwt = plane.trust.user_jwt(
        &bus::box_root_id(),
        &account,
        &control_user,
        unix_now() - 300,
    );
    bus::connect(&plane.server.url, control_jwt, control_user)
        .await
        .expect("control: an unrevoked JWT of the same shape and age connects");
    match bus::connect(
        &plane.server.url,
        planted_jwt,
        KeyPair::from_seed(&planted_seed).unwrap(),
    )
    .await
    {
        Ok(_) => panic!("a revoked previous box user's JWT must be refused"),
        Err(error) => assert_eq!(error.kind(), ConnectErrorKind::AuthorizationViolation),
    }
    assert!(
        plane
            .server
            .log_text()
            .contains("User authentication revoked"),
        "the server refused the previous box user as revoked: {}",
        plane.server.log_text()
    );

    plane.server.stop().await;
    plane.run.shutdown().await;
    signer_passed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn system_user_kicks_a_harness_client() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(|trust| ClaustrumSide::Signer(trust.signer.clone())).await else {
        return;
    };
    let first = ready(&plane, 1).await;
    let account = first["account_public"].as_str().unwrap().to_string();

    // ck-bus's system-account connection code (`NatsBroker::connect_system` and its
    // kick), run in this process with a system user signed by the same harness signer
    // the supervised ck-bus uses.
    let credentials = Arc::new(Credentials::new(Arc::new(ClaustrumRoute::new(
        plane.run.connection_file.clone(),
        None,
    ))));
    let names = AccountNames::derive(&format!("box_{}", plane.machine_id)).unwrap();
    let user = credentials.custody.generate_user();
    let grant = grants::system_account_grant(&names, &user).unwrap();
    let jwt = sign_user_jwt(
        credentials.vault.as_ref(),
        &credentials.key_ids,
        &UserJwtRequest {
            root_credential_id: &bus::system_root_id(),
            user_public: &user,
            issuer_account: Some(&plane.trust.system_account),
            name: "ckbus-system",
            issued_at: unix_now() - 60,
            grant: &grant,
        },
    )
    .await
    .unwrap();
    let broker = NatsBroker::new(plane.server.url.clone(), credentials.clone());
    let system = broker.connect_system(&jwt.jwt, &user).await.unwrap();

    let target = bus::box_client(&plane.trust, &plane.server, &account).await;
    target.flush().await.expect("the target is connected");
    let info = target.server_info();
    system
        .kick(&info.server_id, info.client_id)
        .await
        .expect("the kick is answered");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !plane.server.log_text().contains("Kicked") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server never recorded the kick: {}",
            plane.server.log_text()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    plane.server.stop().await;
    plane.run.shutdown().await;
    signer_passed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusing_signer_creates_nothing_and_retries_once_per_sentinel_period() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(|_| ClaustrumSide::Stub).await else {
        return;
    };
    let root = plane.run.root.path().to_path_buf();
    let last = bus::wait_event(&root, "ckbus.bootstrap.down", 5, BOOT_LIMIT).await;
    assert_eq!(last["cause"], cause::ROOT_KEY_UNREACHABLE);
    assert_eq!(last["retry"], true);
    let started = bus::events(&root, "ckbus.runtime.started");
    let period = started.last().expect("the start line")["sentinel_period_ms"]
        .as_u64()
        .unwrap();
    assert_eq!(period, PERIOD_MS);
    let times: Vec<u64> = bus::events(&root, "ckbus.bootstrap.down")
        .iter()
        .map(|event| event["at_ms"].as_u64().unwrap())
        .collect();
    for gap in times.windows(2).map(|pair| pair[1] - pair[0]) {
        assert!(
            gap >= period * 9 / 10 && gap <= period * 4,
            "one attempt per sentinel period ({period} ms), measured {gap} ms"
        );
    }
    assert!(!bus::account_json(&root).exists(), "no account recorded");
    assert!(!bus::own_users_json(&root).exists(), "no user recorded");
    assert_eq!(
        plane.server.stored_accounts(),
        BTreeSet::from([plane.trust.system_account.clone()]),
        "no account created"
    );
    plane.server.stop().await;
    plane.run.shutdown().await;
    RowReport::passed(Row::InstallBootstrap)
        .served_by_harness_stub("credential.public_key")
        .emit(&vocabulary(CLAUSTRUM_OPERATIONS));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn system_account_root_refusing_yields_sysaccount_absent_and_no_plane() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) =
        start(|trust| ClaustrumSide::Signer(trust.signer.without(&bus::system_root_id()))).await
    else {
        return;
    };
    let root = plane.run.root.path().to_path_buf();
    let metrics = bus::wait_health_cause(
        &plane.run.connection_file,
        cause::SYSACCOUNT_ABSENT,
        BOOT_LIMIT,
    )
    .await;
    assert!(
        metrics["message"]
            .as_str()
            .unwrap()
            .contains(&bus::system_root_id()),
        "the refusal names the root: {metrics}"
    );
    assert!(!bus::account_json(&root).exists(), "no account recorded");
    assert_eq!(
        plane.server.stored_accounts(),
        BTreeSet::from([plane.trust.system_account.clone()]),
        "no plane was built"
    );
    plane.server.stop().await;
    plane.run.shutdown().await;
    signer_passed();
}

/// This machine's address on its default route, which is not a loopback address. No
/// packet is sent: connecting a UDP socket only picks the local address.
fn non_loopback_address() -> IpAddr {
    let socket = UdpSocket::bind("0.0.0.0:0").expect("bind a probe socket");
    socket
        .connect("192.0.2.1:9")
        .expect("this machine has a route off loopback");
    let address = socket.local_addr().unwrap().ip();
    assert!(
        !address.is_loopback() && !address.is_unspecified(),
        "no non-loopback address to probe from: {address}"
    );
    address
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_listener_refuses_a_connect_from_a_non_loopback_address() {
    let _gate = harness::acceptance_gate().await;
    let Some(bin) = server_bin() else {
        return;
    };
    let trust = TrustChain::generate();
    let dir = tempfile::tempdir().unwrap();
    let server = BusServer::start(&bin, dir.path(), &trust, LOOPBACK).await;
    let conf = std::fs::read_to_string(dir.path().join("server.conf")).unwrap();
    assert!(conf.contains(&format!("listen: \"127.0.0.1:{}\"", server.port)));
    assert!(!conf.contains("0.0.0.0"), "never the wildcard address");

    TcpStream::connect(("127.0.0.1", server.port)).expect("a loopback connect is accepted");
    let outside = non_loopback_address();
    let refused = TcpStream::connect_timeout(
        &std::net::SocketAddr::new(outside, server.port),
        Duration::from_secs(2),
    );
    assert!(
        refused.is_err(),
        "a connect to {outside}:{} must be refused",
        server.port
    );
    server.stop().await;
    RowReport::passed(Row::InstallBootstrap)
        .served_by(ServedBy::None)
        .emit(&BTreeSet::new());
}
