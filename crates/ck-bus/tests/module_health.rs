//! Ladder row "Module health answer" (slice 8 of `docs/specs/ck-bus-module.md`), through
//! the daemon's round trip: every arm reads ck-bus's answer with
//! `supervisor.health_probe`, which relays the module's `health.check` metrics whole.
//!
//! Serving sides, per arm:
//! - harness-signer: the up answer, the failed census read, the hung broker, and the
//!   damaged `account.json` with the carrier control (bootstrap stops before any vault
//!   call there, so no Claustrum operation is reached).
//! - harness-stub: bootstrap's refusing signer.
//!
//! The row owns the health half of the deferral controls: bootstrap's refusing signer,
//! the failed census read and a damaged `account.json` each answer down, class
//! `Unavailable`, with `metrics.cause` naming the cause. Up carries `bus.health.up` and
//! no class. Control: a supervised module that does not advertise `health.check` is
//! `Unknown` in `supervisor.health` and is never probed (`health_not_advertised`).
//! A broker that hangs (stopped with SIGSTOP, its socket still open) never slows the
//! answer: it reads the sentinel's last result, and the hang is reported down.

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
    time::{Duration, Instant},
};

use async_nats::jetstream;
use cortexkit_bus_naming::AccountNames;
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    report::{Row, RowReport, ServedBy},
    sentinel::{self as rows, Started},
    signer::{
        nats::nats_server_bin,
        run::{ClaustrumSide, RunOptions, SignerRun},
        SIGNER_OPERATIONS,
    },
    stubs::CLAUSTRUM_OPERATIONS,
};
use sha2::{Digest, Sha256};

const BOOT_LIMIT: Duration = Duration::from_secs(60);

/// How far a measured transition may run past its bound: the log line's clock read and
/// process scheduling, both far below one period.
const MEASUREMENT_SLACK: Duration = Duration::from_millis(250);

/// Runs only when the daemon starts this executable as the healthless module.
#[test]
fn healthless_child() {
    rows::healthless_child_entry();
}

fn vocabulary(operations: &[&str]) -> BTreeSet<String> {
    operations.iter().map(|op| (*op).to_string()).collect()
}

fn signer_passed() {
    RowReport::passed(Row::ModuleHealth)
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

struct Plane {
    trust: TrustChain,
    server: BusServer,
    run: SignerRun,
    machine_id: String,
    started: Started,
    arm_started: Instant,
}

impl Plane {
    fn root(&self) -> &Path {
        self.run.root.path()
    }
}

/// Starts the server and a supervised ck-bus; `prepare` runs on the run's tree before
/// ck-bus starts.
async fn start(
    side: impl FnOnce(&TrustChain) -> ClaustrumSide<'static>,
    prepare: impl FnOnce(&Path),
) -> Option<Plane> {
    let arm_started = Instant::now();
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err((gate, observation)) => {
            RowReport::skipped(Row::ModuleHealth, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&vocabulary(SIGNER_OPERATIONS));
            return None;
        }
    };
    let trust = TrustChain::generate();
    let root = SignerRun::tree();
    prepare(root.path());
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

/// Asserts a down answer: `Failing`, `bus.health.down`, the byte-exact class
/// `Unavailable` at `metrics.class`, and `cause` at `metrics.cause`.
fn assert_down_unavailable(
    status: &str,
    detail: Option<&str>,
    metrics: &serde_json::Value,
    cause: &str,
) {
    assert_eq!(status, "Failing", "{metrics}");
    assert_eq!(detail, Some("bus.health.down"), "{metrics}");
    assert_eq!(
        serde_json::to_string(&metrics["class"]).unwrap(),
        "\"Unavailable\"",
        "the class the module wrote, byte for byte: {metrics}"
    );
    assert_eq!(metrics["cause"], cause, "{metrics}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damaged_account_json_is_down_through_the_probe_and_an_unadvertised_module_is_unknown() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(
        |trust| ClaustrumSide::Signer(trust.signer.clone()),
        |root| {
            let path = bus::account_json(root);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"{\"machine_id\":").unwrap();
        },
    )
    .await
    else {
        return;
    };
    let (status, detail, metrics, _) = rows::wait_health(
        &plane.run.connection_file,
        Duration::from_secs(10),
        "account-json-damaged",
        |_, _, metrics| metrics["cause"] == bootstrap::cause::ACCOUNT_JSON_DAMAGED,
    )
    .await;
    assert_down_unavailable(
        &status,
        detail.as_deref(),
        &metrics,
        bootstrap::cause::ACCOUNT_JSON_DAMAGED,
    );
    assert!(
        metrics["message"]
            .as_str()
            .unwrap()
            .contains("account.json"),
        "the answer names the file: {metrics}"
    );

    // Control: the same supervisor, a module registered without advertising
    // `health.check`.
    rows::register_healthless(&plane.run).await;
    assert_eq!(
        rows::probe(&plane.run.connection_file, rows::HEALTHLESS).await,
        Err("health_not_advertised".to_string()),
        "a module that does not advertise health.check is never probed"
    );
    // It stays Unknown across more than one of the supervisor's probe cycles for ckbus.
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        assert_eq!(
            rows::cached_health_status(&plane.run.connection_file, rows::HEALTHLESS).await,
            "Unknown"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    plane.server.stop().await;
    plane.run.shutdown().await;
    rows::within_budget(
        plane.arm_started,
        "damaged account.json and the carrier control",
    );
    RowReport::passed(Row::ModuleHealth)
        .served_by(ServedBy::HarnessSigner)
        .emit(&vocabulary(SIGNER_OPERATIONS));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn up_carries_no_class_and_a_failed_census_read_is_down_unavailable() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(|trust| ClaustrumSide::Signer(trust.signer.clone()), |_| {}).await
    else {
        return;
    };
    let ready = bus::wait_event(plane.root(), "ckbus.bootstrap.ready", 1, BOOT_LIMIT).await;
    let (status, detail, metrics, _) = rows::wait_health(
        &plane.run.connection_file,
        Duration::from_secs(10),
        "bus.health.up",
        rows::is_up,
    )
    .await;
    assert_eq!(status, "Ok");
    assert_eq!(detail.as_deref(), Some("bus.health.up"));
    assert!(
        metrics.get("class").is_none(),
        "no class when up: {metrics}"
    );
    assert_eq!(metrics["incarnation"], plane.started.incarnation.as_str());

    // The census becomes unreadable: the harness deletes its backing stream.
    let names = AccountNames::derive(&format!("box_{}", plane.machine_id)).unwrap();
    let account = ready["account_public"].as_str().unwrap();
    let client = bus::box_client(&plane.trust, &plane.server, account).await;
    jetstream::new(client)
        .delete_stream(&names.buckets().census_stream)
        .await
        .expect("the harness deletes the census stream");
    let deleted = Instant::now();
    let deleted_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let bound = plane.started.down_bound();
    let (status, detail, metrics, seen) = rows::wait_health(
        &plane.run.connection_file,
        bound + Duration::from_secs(5),
        "census-read-failed",
        |_, _, metrics| metrics["cause"] == sentinel::cause::CENSUS_READ_FAILED,
    )
    .await;
    assert_down_unavailable(
        &status,
        detail.as_deref(),
        &metrics,
        sentinel::cause::CENSUS_READ_FAILED,
    );
    eprintln!(
        "census-read-failed seen {:?} after the delete; bound {bound:?}",
        seen - deleted
    );
    let down = bus::events(plane.root(), "ckbus.sentinel.verdict")
        .into_iter()
        .find(|verdict| verdict["cause"] == sentinel::cause::CENSUS_READ_FAILED)
        .expect("the census-read-failed verdict is logged");
    let took = Duration::from_millis(down["at_ms"].as_u64().unwrap().saturating_sub(deleted_ms));
    assert!(
        took <= bound + MEASUREMENT_SLACK,
        "down within 3 * period + timeout ({bound:?}) of the delete, by ck-bus's clock: {took:?}"
    );

    plane.server.stop().await;
    plane.run.shutdown().await;
    rows::within_budget(plane.arm_started, "up and the failed census read");
    signer_passed();
}

/// Resumes the stopped server however the arm ends, so no stopped process outlives it.
struct Resume(u32);

impl Drop for Resume {
    fn drop(&mut self) {
        rows::signal(self.0, "CONT");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hung_broker_never_slows_the_health_answer() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(|trust| ClaustrumSide::Signer(trust.signer.clone()), |_| {}).await
    else {
        return;
    };
    rows::wait_health(
        &plane.run.connection_file,
        BOOT_LIMIT,
        "bus.health.up",
        rows::is_up,
    )
    .await;
    let pid = rows::nats_server_pid(&plane.server.dir);
    rows::signal(pid, "STOP");
    let resume = Resume(pid);
    let hung = Instant::now();
    let bound = plane.started.down_bound();
    let mut slowest = Duration::ZERO;
    let mut down = None;
    while hung.elapsed() < bound + plane.started.period * 2 {
        let asked = Instant::now();
        let (status, _, metrics) = bus::health(&plane.run.connection_file).await;
        slowest = slowest.max(asked.elapsed());
        if status == "Failing" && down.is_none() {
            down = Some((metrics, hung.elapsed()));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(resume);
    eprintln!("slowest health answer while the broker hung: {slowest:?}");
    assert!(
        slowest < Duration::from_millis(500),
        "the health answer never waits on the hung broker: slowest {slowest:?}"
    );
    let (metrics, after) = down.expect("the hung broker is reported down");
    assert_eq!(metrics["class"], "Unavailable", "{metrics}");
    assert!(
        after <= bound + Duration::from_millis(500),
        "down within the bound ({bound:?}) plus one poll: {after:?}"
    );
    // Resumed, the bus answers again and the sentinel says so.
    rows::wait_health(
        &plane.run.connection_file,
        plane.started.period * 3,
        "bus.health.up after the broker resumed",
        rows::is_up,
    )
    .await;

    plane.server.stop().await;
    plane.run.shutdown().await;
    rows::within_budget(plane.arm_started, "hung broker");
    signer_passed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstraps_refusing_signer_is_down_unavailable_naming_its_cause() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start(|_| ClaustrumSide::Stub, |_| {}).await else {
        return;
    };
    let (status, detail, metrics, _) = rows::wait_health(
        &plane.run.connection_file,
        Duration::from_secs(10),
        "root-key-unreachable",
        |_, _, metrics| metrics["cause"] == bootstrap::cause::ROOT_KEY_UNREACHABLE,
    )
    .await;
    assert_down_unavailable(
        &status,
        detail.as_deref(),
        &metrics,
        bootstrap::cause::ROOT_KEY_UNREACHABLE,
    );
    plane.server.stop().await;
    plane.run.shutdown().await;
    rows::within_budget(plane.arm_started, "refusing signer");
    RowReport::passed(Row::ModuleHealth)
        .served_by_harness_stub("credential.public_key")
        .emit(&vocabulary(CLAUSTRUM_OPERATIONS));
}
