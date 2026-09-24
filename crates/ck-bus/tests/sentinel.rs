//! Ladder row "A8 sentinel probe" (slice 8 of `docs/specs/ck-bus-module.md`), against a
//! real nats-server and the acceptance daemon.
//!
//! Serving sides, per arm:
//! - harness-signer: the up and stopped-server arm, the denied-reply arm and the restart
//!   arm. ck-bus's own users are signed by the harness signer's fixture roots.
//! - harness-stub: the refusing-signer arm, where the shape stub answers `claustrum`
//!   and refuses every signature.
//!
//! Every down transition is checked against `3 * period + timeout`, computed from the
//! `ckbus.runtime.started` line ck-bus logs (the harness sets 1000 ms and 200 ms). Arms:
//! - Server up: `bus.health.up` within two periods of bootstrap finishing, read through
//!   `supervisor.health_probe`, with the reply on ck-bus's own inbox prefix
//!   (`_INBOX.<box user>`). Server stopped: down, class `Unavailable`, within the bound
//!   after the stop.
//! - Reply publish removed against a healthy server: the sentinel's own probe code, in
//!   this process, over a box-account user whose grant lacks publish on its inbox,
//!   reports down with class `Denied` within the bound. The twin with the full grant is
//!   up, so the class comes from the removed permission alone.
//! - Before the first answer: a process whose predecessor persisted `up` answers
//!   down/`Unavailable` when the broker is stopped before it can probe, and the
//!   persisted verdict is logged as not used.
//! - Refusing signer: ck-bus stays `running`, its restart counters are unchanged after
//!   three periods, and the class is `Unavailable`.

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
#[allow(dead_code)]
#[path = "../src/sentinel/mod.rs"]
mod sentinel;

use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bootstrap::plane::{Broker, NatsBroker};
use cortexkit_bus_naming::{AccountNames, Operation};
use credentials::{
    issue::{sign_user_jwt, UserJwtRequest},
    vault::{VaultError, VaultSigning},
    wire::{self, VaultPublicKey, VaultSignature},
    Credentials,
};
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    report::{Row, RowReport, ServedBy},
    sentinel::{self as rows, Started},
    signer::{
        nats::{nats_server_bin, unix_now},
        run::{ClaustrumSide, RunOptions, SignerRun},
        HarnessSigner, SIGNER_OPERATIONS,
    },
    stubs::CLAUSTRUM_OPERATIONS,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subc_client_rs::HandlerOutcome;
use subc_protocol::session::HealthStatus;

const BOOT_LIMIT: Duration = Duration::from_secs(60);

/// How far a measured transition may run past its bound: the log line's clock read and
/// process scheduling, both far below one period.
const MEASUREMENT_SLACK: Duration = Duration::from_millis(250);

fn vocabulary(operations: &[&str]) -> BTreeSet<String> {
    operations.iter().map(|op| (*op).to_string()).collect()
}

fn signer_passed() {
    RowReport::passed(Row::Sentinel)
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

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

struct Plane {
    trust: TrustChain,
    server: BusServer,
    run: SignerRun,
    machine_id: String,
    started: Started,
    arm_started: Instant,
}

async fn start(side: impl FnOnce(&TrustChain) -> ClaustrumSide<'static>) -> Option<Plane> {
    let arm_started = Instant::now();
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err((gate, observation)) => {
            RowReport::skipped(Row::Sentinel, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&vocabulary(SIGNER_OPERATIONS));
            return None;
        }
    };
    let trust = TrustChain::generate();
    let root = SignerRun::tree();
    let server = BusServer::start(&bin, &root.join("nats"), &trust, LOOPBACK).await;
    let machine_id = fresh_machine_id();
    let mut env = server.ckbus_env();
    env.extend(rows::sentinel_env());
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
    let started = rows::started(run.root.path(), 1).await;
    assert_eq!(
        (started.period, started.timeout),
        (
            Duration::from_millis(rows::PERIOD_MS),
            Duration::from_millis(rows::TIMEOUT_MS)
        ),
        "ck-bus logged the harness sentinel values"
    );
    Some(Plane {
        trust,
        server,
        run,
        machine_id,
        started,
        arm_started,
    })
}

fn verdicts(run_root: &Path) -> Vec<Value> {
    bus::events(run_root, "ckbus.sentinel.verdict")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_up_reports_up_on_the_modules_own_inbox_and_a_stopped_server_reports_unavailable() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(|trust| ClaustrumSide::Signer(trust.signer.clone())).await else {
        return;
    };
    let ready = bus::wait_event(
        plane.run.root.path(),
        "ckbus.bootstrap.ready",
        1,
        BOOT_LIMIT,
    )
    .await;
    let (_, _, metrics, _) = rows::wait_health(
        &plane.run.connection_file,
        plane.started.period * 2 + Duration::from_secs(5),
        "bus.health.up",
        rows::is_up,
    )
    .await;
    // Timed by ck-bus's own clock: bootstrap's ready line to the sentinel's up line.
    let up = verdicts(plane.run.root.path())
        .into_iter()
        .find(|verdict| verdict["verdict"] == "up")
        .expect("the up verdict is logged");
    let took = up["at_ms"].as_u64().unwrap() - ready["at_ms"].as_u64().unwrap();
    assert!(
        Duration::from_millis(took) <= plane.started.period * 2,
        "bus.health.up within two periods of bootstrap: {took} ms"
    );
    assert!(
        metrics.get("class").is_none(),
        "no class while up: {metrics}"
    );
    assert_eq!(metrics["incarnation"], plane.started.incarnation.as_str());
    let inbox = format!("_INBOX.{}.", ready["box_user"].as_str().unwrap());
    let reply_subject = metrics["reply_subject"].as_str().unwrap();
    assert!(
        reply_subject.starts_with(&inbox),
        "the reply came back on ck-bus's own inbox {inbox}: {reply_subject}"
    );

    // The server stops; the probe reads down/Unavailable within the bound.
    let bound = plane.started.down_bound();
    let stopped_ms = unix_ms();
    let stopped = Instant::now();
    plane.server.stop().await;
    let (status, detail, metrics, seen) = rows::wait_health(
        &plane.run.connection_file,
        bound + Duration::from_secs(5),
        "down after the server stopped",
        |status, _, _| status == "Failing",
    )
    .await;
    assert_eq!(detail.as_deref(), Some("bus.health.down"));
    assert_eq!(status, "Failing");
    assert_eq!(metrics["class"], "Unavailable", "{metrics}");
    let down = verdicts(plane.run.root.path())
        .into_iter()
        .find(|verdict| {
            verdict["verdict"] == "down" && verdict["at_ms"].as_u64() >= Some(stopped_ms)
        })
        .expect("the down verdict is logged");
    let took = Duration::from_millis(down["at_ms"].as_u64().unwrap() - stopped_ms);
    eprintln!(
        "down after the stop: {took:?} by ck-bus's clock, {:?} through the probe; bound {bound:?}",
        seen - stopped
    );
    assert!(
        took <= bound + MEASUREMENT_SLACK,
        "down within 3 * period + timeout ({bound:?}): measured {took:?}"
    );
    assert_eq!(down["class"], "Unavailable");

    plane.run.shutdown().await;
    rows::within_budget(plane.arm_started, "server up and server stopped");
    signer_passed();
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

/// Runs ck-bus's sentinel code in this process over a box-account user whose grant is
/// `grant`, until the verdict first changes away from the initial one. Returns the
/// monitor and how long that took.
async fn probe_with(
    plane: &Plane,
    account_public: &str,
    grant: impl FnOnce(&AccountNames, &str) -> grants::Grant,
) -> (Arc<sentinel::Monitor>, String, Duration) {
    let credentials = Arc::new(Credentials::new(Arc::new(InProcessSigner(
        plane.trust.signer.clone(),
    ))));
    let names = AccountNames::derive(&format!("box_{}", plane.machine_id)).unwrap();
    let user = credentials.custody.generate_user();
    let jwt = sign_user_jwt(
        credentials.vault.as_ref(),
        &credentials.key_ids,
        &UserJwtRequest {
            root_credential_id: &bus::box_root_id(),
            user_public: &user,
            issuer_account: Some(account_public),
            name: "sentinel-row-bus-module",
            issued_at: unix_now() - 60,
            grant: &grant(&names, &user),
        },
    )
    .await
    .expect("bus-module user JWT");
    let broker = NatsBroker::new(plane.server.url.clone(), credentials.clone());
    let box_plane = broker
        .connect_box(&jwt.jwt, &user)
        .await
        .unwrap_or_else(|error| panic!("the bus-module user connects: {error}"));
    let link = box_plane
        .sentinel_link()
        .expect("a real box plane has a link");
    let monitor = Arc::new(sentinel::Monitor::new(
        "sentinel-row".to_string(),
        sentinel::Timing {
            period: plane.started.period,
            timeout: plane.started.timeout,
        },
    ));
    let probe = sentinel::NatsProbe::start(link, box_plane.clone(), names, "ckbus", "sentinel-row")
        .await
        .unwrap_or_else(|failure| panic!("the responder subscribes: {failure:?}"));
    let began = Instant::now();
    let first_change: Arc<Mutex<Option<Duration>>> = Arc::default();
    let driving = {
        let monitor = monitor.clone();
        let first_change = first_change.clone();
        tokio::spawn(async move {
            monitor
                .drive(&probe, &move |_verdict| {
                    first_change.lock().unwrap().get_or_insert(began.elapsed());
                })
                .await;
        })
    };
    let deadline = Instant::now() + plane.started.down_bound() + Duration::from_secs(2);
    let took = loop {
        if let Some(took) = *first_change.lock().unwrap() {
            break took;
        }
        assert!(
            Instant::now() < deadline,
            "the in-process verdict never changed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    driving.abort();
    (monitor, user, took)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reply_publish_removed_against_a_healthy_server_reports_denied() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(|trust| ClaustrumSide::Signer(trust.signer.clone())).await else {
        return;
    };
    let ready = bus::wait_event(
        plane.run.root.path(),
        "ckbus.bootstrap.ready",
        1,
        BOOT_LIMIT,
    )
    .await;
    let account = ready["account_public"].as_str().unwrap().to_string();
    // The server is healthy: the supervised ck-bus's own sentinel reports up.
    rows::wait_health(
        &plane.run.connection_file,
        Duration::from_secs(10),
        "bus.health.up",
        rows::is_up,
    )
    .await;

    // Twin: the full generated grant is up, on the user's own inbox.
    let (monitor, user, _) = probe_with(&plane, &account, |names, user| {
        grants::bus_module_grant(names, user).unwrap()
    })
    .await;
    let report = monitor.report();
    assert_eq!(report.status, HealthStatus::Ok, "{report:?}");
    let metrics = report.metrics.unwrap();
    assert!(metrics["reply_subject"]
        .as_str()
        .unwrap()
        .starts_with(&format!("_INBOX.{user}.")));

    // The same grant without publish on the user's inbox: the responder's reply is
    // refused by the server, and the verdict is Denied.
    let (monitor, user, took) = probe_with(&plane, &account, |names, user| {
        let inbox = format!("_INBOX.{user}.>");
        let entries = grants::bus_module_grant(names, user)
            .unwrap()
            .allow_entries()
            .into_iter()
            .filter(|entry| !(entry.operation == Operation::Publish && entry.subject == inbox))
            .collect();
        grants::Grant::from_entries(grants::GrantRole::BusModule, names, entries)
            .expect("the grant without reply publish is a valid grant")
    })
    .await;
    let report = monitor.report();
    assert_eq!(report.status, HealthStatus::Failing, "{report:?}");
    assert_eq!(report.detail.as_deref(), Some("bus.health.down"));
    let metrics = report.metrics.unwrap();
    assert_eq!(metrics["class"], "Denied", "{metrics}");
    assert_eq!(metrics["cause"], sentinel::cause::DENIED);
    assert!(
        metrics["message"]
            .as_str()
            .unwrap()
            .contains(&format!("_INBOX.{user}")),
        "the violation names the inbox: {metrics}"
    );
    let bound = plane.started.down_bound();
    assert!(
        took <= bound + MEASUREMENT_SLACK,
        "Denied within 3 * period + timeout ({bound:?}): measured {took:?}"
    );

    plane.server.stop().await;
    plane.run.shutdown().await;
    rows::within_budget(plane.arm_started, "reply publish removed");
    signer_passed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn before_its_first_answer_a_restarted_process_answers_down_unavailable() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(|trust| ClaustrumSide::Signer(trust.signer.clone())).await else {
        return;
    };
    rows::wait_health(
        &plane.run.connection_file,
        BOOT_LIMIT,
        "bus.health.up",
        rows::is_up,
    )
    .await;
    let verdict_file = plane
        .run
        .root
        .path()
        .join("data/cortexkit/ckbus/sentinel_verdict.json");
    let persisted = bus::read_json(&verdict_file);
    assert_eq!(persisted["verdict"], "up");
    assert_eq!(persisted["incarnation"], plane.started.incarnation.as_str());

    // The broker stops before the restarted process can probe at all.
    plane.server.stop().await;
    bus::restart_ckbus(&plane.run.connection_file).await;
    let second = rows::started(plane.run.root.path(), 2).await;
    assert_ne!(second.incarnation, plane.started.incarnation);
    let (status, detail, metrics, _) = rows::wait_health(
        &plane.run.connection_file,
        Duration::from_secs(10),
        "an answer from the restarted process",
        |_, _, _| true,
    )
    .await;
    assert_eq!(status, "Failing", "{metrics}");
    assert_eq!(detail.as_deref(), Some("bus.health.down"));
    assert_eq!(metrics["class"], "Unavailable", "{metrics}");
    // It keeps answering down for three periods: the persisted `up` never answers.
    let until = Instant::now() + second.period * 3;
    while Instant::now() < until {
        let (status, _, metrics) = bus::health(&plane.run.connection_file).await;
        assert_eq!(status, "Failing", "{metrics}");
        assert_eq!(metrics["class"], "Unavailable", "{metrics}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let previous = bus::events(plane.run.root.path(), "ckbus.sentinel.previous_verdict");
    let previous = previous
        .last()
        .expect("the restarted process names the verdict it found");
    assert_eq!(previous["previous"]["verdict"], "up");
    assert_eq!(
        previous["previous"]["incarnation"],
        plane.started.incarnation.as_str()
    );
    assert_eq!(previous["used"], false);
    assert_eq!(
        bus::read_json(&verdict_file)["incarnation"],
        plane.started.incarnation.as_str(),
        "the restarted process has written no verdict of its own"
    );

    plane.run.shutdown().await;
    rows::within_budget(plane.arm_started, "restart before the first answer");
    signer_passed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusing_signer_keeps_ckbus_running_unrestarted_and_unavailable() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(|_| ClaustrumSide::Stub).await else {
        return;
    };
    let (_, _, metrics, _) = rows::wait_health(
        &plane.run.connection_file,
        Duration::from_secs(10),
        "down/Unavailable",
        |status, _, metrics| status == "Failing" && metrics["class"] == "Unavailable",
    )
    .await;
    assert_eq!(metrics["cause"], bootstrap::cause::ROOT_KEY_UNREACHABLE);
    let before = rows::ckbus_list_entry(&plane.run.connection_file).await;
    assert_eq!(before["state"], "running", "{before}");
    tokio::time::sleep(plane.started.period * 3).await;
    let after = rows::ckbus_list_entry(&plane.run.connection_file).await;
    assert_eq!(after["state"], "running", "{after}");
    assert_eq!(after["restart_count"], before["restart_count"], "{after}");
    assert_eq!(
        after["lifetime_restarts"], before["lifetime_restarts"],
        "{after}"
    );
    assert_eq!(
        rows::started_count(plane.run.root.path()),
        1,
        "one ck-bus process for the whole arm"
    );
    let (status, _, metrics) = bus::health(&plane.run.connection_file).await;
    assert_eq!(status, "Failing");
    assert_eq!(metrics["class"], "Unavailable", "{metrics}");

    plane.server.stop().await;
    plane.run.shutdown().await;
    rows::within_budget(plane.arm_started, "refusing signer");
    RowReport::passed(Row::Sentinel)
        .served_by_harness_stub("credential.public_key")
        .emit(&vocabulary(CLAUSTRUM_OPERATIONS));
}
