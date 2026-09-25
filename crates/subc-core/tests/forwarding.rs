use std::{
    collections::BTreeSet,
    fs,
    net::Shutdown,
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::Value;
use subc_control::{
    ClientControlRequest, ClientControlResponse, ConsumerIdentity, ModuleProtocol, PollKind,
    SupervisorHealthStatus,
};
use subc_daemon::{
    read_frame, server::MAX_PENDING_ROUTE_OPENS_PER_CONNECTION, stderr_tail::TailEntry,
    write_frame, ExitKind, ForwardingTable, Frame, HealthAction, HealthConfig, ModuleOverlap,
    ModuleSpec, ModuleState, ModuleStatus, Registry, RestartPolicy, SuperviseError,
    SupervisedModule, Supervisor, SupervisorHandle, SupervisorProcessLiveness, SwapFailureArm,
};
use subc_protocol::{
    error_codes,
    manifest::{Concurrency, ExecutionMode, IdentityScope, ModuleManifest, ProviderRole, Tool},
    session::HealthStatus,
    BindIdentity, ErrorBody, Flags, FrameType, ModuleHelloAckBody, ModuleHelloBody, Priority,
    RouteTarget, FLAG_SUBSCRIPTION, PROTOCOL_VERSION,
};
use subc_test_support::TestTempDir;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, sleep_until, timeout, Instant},
};

mod common;
use common::{
    connect_authed_client, start_test_daemon_with_bind_timeout,
    start_test_daemon_with_bind_timeout_and_breaker,
    start_test_daemon_with_process_liveness_and_supervisor,
    start_test_daemon_with_route_bind_relay_overrides, TestDaemon,
};

const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// How long supervisor_reload_rejects_new_work_during_drain's in-flight
/// request holds the drain open. Every assertion in that test runs while the
/// module is draining, so this must outlast the observe-then-act window; any
/// read that can wait for the held Response must allow this PLUS its margin.
const IN_FLIGHT_HOLD: Duration = Duration::from_millis(2000);
/// Deadline for setup hang-guards (poll-until-subc-observable-state helpers:
/// registration, binding count, stub events, status). These are deadlock
/// detectors, NOT latency assertions — sized generously so a spawn/connect/auth
/// constellation under heavy parallel CI load (many subprocess-heavy test
/// binaries at once) cannot trip them. Latency/behavioral bounds are separate
/// and stay tight.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

struct TestServer {
    daemon: TestDaemon,
    process_liveness: Arc<SupervisorProcessLiveness>,
    supervisor_handle: SupervisorHandle,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_inner(None).await
    }

    /// Start with a short route.bind relay timeout so the timeout-path tests fire
    /// quickly instead of waiting on the production-safe default.
    async fn start_with_bind_timeout(bind_timeout: Duration) -> Self {
        Self::start_inner(Some(bind_timeout)).await
    }

    /// Start with a short relay timeout AND a test-sized bind-relay breaker
    /// policy. The production breaker needs three full 12s budgets to open and
    /// then holds for 20s; the constants carry the reasoning for those numbers
    /// and these tests pin the mechanism.
    async fn start_with_bind_timeout_and_breaker(
        bind_timeout: Duration,
        threshold: u32,
        cooldown: Duration,
    ) -> Self {
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let supervisor_handle = SupervisorHandle::new();
        let daemon = start_test_daemon_with_bind_timeout_and_breaker(
            "forwarding-server",
            process_liveness.clone(),
            supervisor_handle.clone(),
            bind_timeout,
            (threshold, cooldown),
        )
        .await;
        Self {
            daemon,
            process_liveness,
            supervisor_handle,
        }
    }

    async fn start_inner(bind_timeout: Option<Duration>) -> Self {
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let supervisor_handle = SupervisorHandle::new();
        let daemon = match bind_timeout {
            Some(timeout) => {
                start_test_daemon_with_bind_timeout(
                    "forwarding-server",
                    process_liveness.clone(),
                    supervisor_handle.clone(),
                    timeout,
                )
                .await
            }
            None => {
                start_test_daemon_with_process_liveness_and_supervisor(
                    "forwarding-server",
                    process_liveness.clone(),
                    supervisor_handle.clone(),
                )
                .await
            }
        };
        Self {
            daemon,
            process_liveness,
            supervisor_handle,
        }
    }

    fn stub_events_path(&self, label: &str) -> PathBuf {
        self.temp_dir.join(format!("{label}-events.jsonl"))
    }
}

impl Deref for TestServer {
    type Target = TestDaemon;

    fn deref(&self) -> &Self::Target {
        &self.daemon
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_health_probe_refuses_old_module_without_sending_health_check() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-old-health";
    let (module, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, module_id, "old-health").await;

    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &control_request_frame(
            120,
            ClientControlRequest::SupervisorHealthProbe {
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let frame = read_frame_timeout(&mut client).await;
    assert_error(&frame, 0, 120, "health_not_advertised");
    assert_no_stub_event_within(&events_path, Duration::from_millis(100), |event| {
        event["kind"] == "health_check"
    })
    .await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_health_probe_carries_degraded_report_verbatim() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-degraded-health";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "degraded-health",
        [
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "degraded"),
            ("FAKE_AFT_HEALTH_DETAIL", "warming model"),
            (
                "FAKE_AFT_HEALTH_METRICS",
                r#"{"queue_depth":3,"phase":"load"}"#,
            ),
        ],
    )
    .await;

    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &control_request_frame(
            121,
            ClientControlRequest::SupervisorHealthProbe {
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let frame = read_frame_timeout(&mut client).await;
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, 121);
    match serde_json::from_slice::<ClientControlResponse>(&frame.body).unwrap() {
        ClientControlResponse::SupervisorHealthProbe {
            module_id: response_module_id,
            status,
            detail,
            metrics,
        } => {
            assert_eq!(response_module_id, module_id);
            assert_eq!(status, HealthStatus::Degraded);
            assert_eq!(detail.as_deref(), Some("warming model"));
            assert_eq!(
                metrics,
                Some(serde_json::json!({"queue_depth": 3, "phase": "load"}))
            );
        }
        other => panic!("unexpected health response: {other:?}"),
    }
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "health_check"
    })
    .await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_prober_restarts_unresponsive_module_and_recovers_ok() {
    let server = TestServer::start().await;
    let supervisor =
        supervisor(&server, 2, Duration::from_millis(10)).with_health_config(health_config(
            Duration::from_millis(20),
            // DEADLINE SIZED FOR A RESPAWN, NOT FOR THE INDUCED FAILURE.
            //
            // This was 200ms and went red on a Windows runner (2026-09-19),
            // first failure in eight runs, on a commit that touched an
            // unrelated path. The mechanism is not flakiness in the usual
            // sense: after the restart this test induces, the module must
            // spawn, connect, HELLO and register before it can answer
            // anything -- and a probe against a module with no forwarding
            // connection returns `lane_dead`, THE MOST SEVERE CLASSIFICATION
            // THERE IS. So with cadence 20ms, deadline 200ms and threshold 2,
            // the respawned process had 440ms to come all the way up before
            // the prober restarted it AGAIN, and the assertion below saw
            // restart_count 2.
            //
            // 2s gives a respawn on a loaded runner room to register. It also
            // makes the FIRST restart take ~4s, which is the honest cost: the
            // induced failure is detected by waiting out two deadlines, so
            // sizing the deadline for the slowest leg necessarily slows the
            // fastest one.
            //
            // THE PRODUCTION SHAPE BEHIND THIS IS NOT A TEST BUG and is
            // recorded rather than fixed here: `schedule_next` re-arms the
            // first post-restart probe at the ordinary cadence with NO GRACE
            // for spawn and registration, while the daemon already has a
            // `module_warming` concept for exactly that window -- it is used
            // on the route.open path and not on the probe path. Production
            // cadences are seconds rather than 20ms so the margin is wide,
            // but the asymmetry is real.
            Duration::from_secs(2),
            2,
            HealthAction::Report,
            HealthAction::Report,
            false,
        ));
    let module_id = "fake-aft-prober-recover";
    let marker = server.temp_dir.join("health-first-wedge");
    let (module, _events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "prober-recover",
        [
            ("FAKE_AFT_ADVERTISE_HEALTH", "1".to_string()),
            (
                "FAKE_AFT_HEALTH_NEVER_REPLY_FIRST_PATH",
                marker.to_string_lossy().into_owned(),
            ),
        ],
    )
    .await;

    // PREDICATE AND ASSERTION MUST DEMAND THE SAME THING. This waited for
    // `>= 1` and then asserted `== 1`, so the wait ACCEPTED a state the
    // assertion rejects: any snapshot taken after a second restart satisfies
    // the predicate and fails the assert. A mismatch like that turns a timing
    // difference into an assertion failure, which reads like a broken
    // invariant rather than a slow machine.
    //
    // Now both say exactly one. If a second restart ever happens the test
    // times out instead -- slower, and honest about which thing went wrong.
    let status = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.restart_count == 1
            && status.health.status == SupervisorHealthStatus::Ok
            && status.live
    })
    .await;
    assert_eq!(status.restart_count, 1);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_prober_failing_restart_exhausts_budget_to_disabled() {
    let server = TestServer::start().await;
    let supervisor =
        supervisor(&server, 1, Duration::from_millis(10)).with_health_config(health_config(
            Duration::from_millis(20),
            Duration::from_millis(20),
            1,
            HealthAction::Report,
            HealthAction::Restart,
            false,
        ));
    let module_id = "fake-aft-failing-budget";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "failing"),
            ("FAKE_AFT_HEALTH_DETAIL", "permanent failure"),
        ],
    )
    .await;

    // Health fields ride the same poll predicate: Disabled and the health
    // stamp are separate writes, so asserting them after a state-only wait
    // reads a mid-transition snapshot (flaked once in CI on ubuntu).
    //
    // The terminal health status is deliberately NOT pinned to Failing: with the
    // injected 20ms probe deadline, a loaded runner can time the probe out before
    // even this always-failing stub replies, so the budget-exhausting probe may
    // classify as Unresponsive instead of the domain-reported Failing. Both are
    // failing-class triggers; the mechanism under test is budget exhaustion to
    // Disabled, not which trigger class fired last.
    wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Disabled
            && status.restart_count == 1
            && !status.live
            && status.health.status != SupervisorHealthStatus::Ok
            && status.health.last_action.as_deref() == Some("disabled")
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_restart_spawn_failure_marks_failed_and_start_recovers() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = TestServer::start().await;
    let supervisor =
        supervisor(&server, 10, Duration::from_millis(10)).with_health_config(health_config(
            Duration::from_secs(1),
            Duration::from_millis(200),
            1,
            HealthAction::Report,
            HealthAction::Restart,
            false,
        ));
    let module_id = "fake-aft-health-respawn-fails";
    let program = server.temp_dir.join("health-respawn-program");
    let real_program = Path::new(env!("CARGO_BIN_EXE_fake-aft-stub"));
    common::copy_executable(real_program, &program);
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();

    let mut spec = stub_spec_with_env(
        &server,
        module_id,
        [
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "failing"),
        ],
    );
    spec.program = program.clone();
    let module = supervisor.spawn(spec).unwrap();
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;

    // The running process keeps its executable mapped after unlink on Unix, while
    // the health-triggered replacement deterministically fails at Command::spawn.
    fs::remove_file(&program).unwrap();
    let failed = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Failed && !status.registration_active
    })
    .await;
    assert!(failed.enabled);
    assert_eq!(failed.pid, None);
    assert_eq!(
        subc_daemon::ModuleProcessLiveness::process_live(
            server.process_liveness.as_ref(),
            module_id
        ),
        None,
        "failed health respawn must be removed from process-liveness tracking"
    );

    common::copy_executable(real_program, &program);
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(module.set_enabled(true).await.unwrap());
    wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_prober_degraded_report_is_not_restarted_and_is_observable() {
    let server = TestServer::start().await;
    let supervisor =
        supervisor(&server, 2, Duration::from_millis(10)).with_health_config(health_config(
            Duration::from_millis(20),
            Duration::from_millis(20),
            2,
            HealthAction::Report,
            HealthAction::Report,
            false,
        ));
    let module_id = "fake-aft-prober-degraded";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "degraded"),
            ("FAKE_AFT_HEALTH_DETAIL", "warming cache"),
        ],
    )
    .await;

    wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.health.status == SupervisorHealthStatus::Degraded
    })
    .await;
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &control_request_frame(122, ClientControlRequest::SupervisorHealth {}),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let frame = read_frame_timeout(&mut client).await;
    match serde_json::from_slice::<ClientControlResponse>(&frame.body).unwrap() {
        ClientControlResponse::SupervisorHealth { modules, .. } => {
            let entry = modules
                .into_iter()
                .find(|entry| entry.module_id == module_id)
                .expect("module health entry");
            assert_eq!(entry.status, SupervisorHealthStatus::Degraded);
            assert_eq!(entry.detail.as_deref(), Some("warming cache"));
            assert_eq!(entry.last_action.as_deref(), Some("report"));
        }
        other => panic!("unexpected supervisor.health response: {other:?}"),
    }
    write_frame(
        &mut client,
        &control_request_frame(123, ClientControlRequest::SupervisorList {}),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let frame = read_frame_timeout(&mut client).await;
    match serde_json::from_slice::<ClientControlResponse>(&frame.body).unwrap() {
        ClientControlResponse::SupervisorList { modules, .. } => {
            let entry = modules
                .into_iter()
                .find(|entry| entry.module_id == module_id)
                .expect("module supervisor entry");
            assert_eq!(entry.health, SupervisorHealthStatus::Degraded);
            assert!(entry.last_probe_ms.is_some());
        }
        other => panic!("unexpected supervisor.list response: {other:?}"),
    }
    assert_eq!(module.status().unwrap().restart_count, 0);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_advertising_module_is_unknown_and_never_probed() {
    let server = TestServer::start().await;
    let supervisor =
        supervisor(&server, 1, Duration::from_millis(10)).with_health_config(health_config(
            Duration::from_millis(20),
            Duration::from_millis(20),
            1,
            HealthAction::Report,
            HealthAction::Report,
            false,
        ));
    let module_id = "fake-aft-no-health-ad";
    let (module, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, module_id, "no-health-ad").await;

    assert_no_stub_event_within(&events_path, Duration::from_millis(120), |event| {
        event["kind"] == "health_check"
    })
    .await;
    let status = module.status().unwrap();
    assert_eq!(status.health.status, SupervisorHealthStatus::Unknown);
    assert_eq!(status.health.last_probe_ms, None);

    module.stop().await.unwrap();
}

/// The defect and the fix in one test, because either half alone is unreadable.
///
/// A `protocol: "none"` module never registers and never answers `health.check`.
/// Under a prober that classifies silence, that is a module being restarted on
/// its crash budget for doing exactly what it was configured to do.
///
/// THE THREE ARMS ANSWER THREE DIFFERENT QUESTIONS:
///
/// * `never_connects_none` is the PRODUCTION SHAPE -- a third-party server that
///   never dials subc, declared `none`. It must sit at `Running` with an
///   untouched budget and an empty terminal ring.
/// * `registers_but_silent_subc` is the CONTROL THAT PROVES THE PROBER IS LIVE
///   at this cadence and threshold. Without it, every "was not restarted"
///   assertion above could be satisfied by a prober that does nothing at all.
/// * `registers_but_silent_none` is THE ARM THE GATE IS TESTED BY. It is the
///   same fixture as the control, differing only in the declaration, so it can
///   only stay unrestarted if the suppression is reached.
///
/// The third arm exists because the first CANNOT detect the gate being removed:
/// today's prober only ever arms for a module that is registered AND advertises
/// `health.check`, so a never-connecting module is never probed under either
/// protocol. That makes the production shape safe today by accident of the
/// scheduler rather than by this declaration -- which is precisely why the
/// declaration needs an arm that fails without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_protocol_none_module_is_never_probed_while_its_subc_twin_is_restarted() {
    let server = TestServer::start().await;
    let supervisor =
        supervisor(&server, 3, Duration::from_millis(10)).with_health_config(health_config(
            Duration::from_millis(20),
            Duration::from_millis(20),
            1,
            HealthAction::Report,
            HealthAction::Report,
            false,
        ));

    // Ten cadences of the 20ms probe clock. A module the prober is willing to
    // touch fails its single-probe threshold long before this elapses, which is
    // what the control arm below demonstrates on the same budget.
    const TEN_CADENCES: Duration = Duration::from_millis(200);

    let never_connects_none = supervisor
        .spawn(never_connecting_spec("nats-like-none", None, SigtermMode::Default).0)
        .unwrap();

    let mut silent_subc_spec = stub_spec_with_env(
        &server,
        "silent-subc-twin",
        [
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_NEVER_REPLY", "1"),
        ],
    );
    silent_subc_spec.protocol = ModuleProtocol::Subc;
    let registers_but_silent_subc = supervisor.spawn(silent_subc_spec).unwrap();

    let mut silent_none_spec = stub_spec_with_env(
        &server,
        "silent-none-twin",
        [
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_NEVER_REPLY", "1"),
        ],
    );
    silent_none_spec.protocol = ModuleProtocol::None;
    // A protocol-none spawn carries no `--subc` (a stock binary would exit on
    // the unknown flag). This twin is deliberately a subc-wire stub DECLARED
    // none, so it must be handed the connection file itself to register at all;
    // the arm proves the declaration governs probing even for a process that
    // does speak the wire.
    silent_none_spec.args = vec![
        "--subc".to_string(),
        server.connection_file_path.to_string_lossy().into_owned(),
    ];
    let registers_but_silent_none = supervisor.spawn(silent_none_spec).unwrap();

    // The control first: it is the only arm that can fail by waiting, and it
    // establishes that a prober capable of restarting is running before the two
    // negative arms claim anything from silence.
    let restarted = wait_for_status(&registers_but_silent_subc, SETUP_TIMEOUT, |status| {
        status.restart_count >= 1
    })
    .await;
    assert!(
        restarted.restart_count >= 1,
        "a subc module that never answers health.check must be restarted; this is the \
         behaviour protocol: none exists to suppress"
    );

    sleep(TEN_CADENCES).await;

    for module in [&never_connects_none, &registers_but_silent_none] {
        let status = module.status().unwrap();
        assert_eq!(
            status.state,
            ModuleState::Running,
            "{} must still be running: nothing about it is a fault",
            module.module_id()
        );
        assert_eq!(
            status.restart_count,
            0,
            "{} spent crash budget on a protocol it does not speak",
            module.module_id()
        );
        assert_eq!(
            status.health.last_probe_ms,
            None,
            "{} was probed despite declaring no subc wire",
            module.module_id()
        );
        assert!(
            module.terminal_history().entries.is_empty(),
            "{} has a terminal record, so something tore it down",
            module.module_id()
        );
    }

    // `live` for a module with no wire to register on: the process is up, and
    // that is the whole claim.
    let status = never_connects_none.status().unwrap();
    assert!(
        status.live,
        "a running protocol: none module must not read as not-live forever"
    );
    assert!(
        !status.registration_active,
        "precondition: this module must never have registered"
    );

    never_connects_none.stop().await.unwrap();
    registers_but_silent_none.stop().await.unwrap();
    registers_but_silent_subc.stop().await.unwrap();
}

/// Teardown asks before it forces, and the two arms are the difference between
/// asking and pretending to.
///
/// A subc module is asked over its own connection. A module that speaks no subc
/// wire has no connection to be asked over, so without a signal the drain budget
/// is pure delay in front of a SIGKILL -- which, for the store this mode exists
/// to supervise, turns every ordinary restart into a recovery on next start.
///
/// The arms differ ONLY in what the child does with the signal:
///
/// * handled -> a marker file that cannot exist unless SIGTERM was delivered,
///   plus `exit 0`, plus an elapsed time well inside the budget.
/// * ignored -> `signal 9` after the FULL budget, which is what the old
///   behaviour looked like for every `none` module on every teardown.
// Unix only by the daemon's own contract: Windows has no SIGTERM and no
// portable stand-in, so `supervise.rs` documents the graceful-stop step as
// absent there and the arms this test distinguishes collapse into one.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_none_teardown_sigterms_first_and_only_kills_a_child_that_ignores_it() {
    const DRAIN_BUDGET: Duration = Duration::from_millis(900);

    let server = TestServer::start().await;
    let supervisor =
        supervisor_with_drain_timeout(&server, 2, Duration::from_millis(10), DRAIN_BUDGET);

    let marker = server.temp_dir.join("sigterm-handled.marker");
    let (handled_spec, handled_ready) = never_connecting_spec(
        "none-graceful",
        Some(&server.temp_dir),
        SigtermMode::WriteMarkerAndExitZero(&marker),
    );
    let handled = supervisor.spawn(handled_spec).unwrap();
    wait_for_path(
        &handled_ready.expect("ready path configured"),
        SETUP_TIMEOUT,
    )
    .await;

    let started = Instant::now();
    handled.restart(None).await.unwrap();
    let status =
        wait_for_status(&handled, SETUP_TIMEOUT, |status| status.last_exit.is_some()).await;
    let handled_elapsed = started.elapsed();
    let exit = status.last_exit.expect("waited for an exit");

    assert!(
        marker.exists(),
        "no SIGTERM reached the child: the marker file is written by its handler and \
         cannot appear any other way"
    );
    assert_eq!(
        (exit.code, exit.signal),
        (Some(0), None),
        "a child that handled SIGTERM must be recorded as exiting cleanly, not as killed"
    );
    assert!(
        handled_elapsed < DRAIN_BUDGET,
        "a cooperative stop took {handled_elapsed:?}, at or past the {DRAIN_BUDGET:?} budget, \
         which is what waiting-then-killing looks like"
    );

    let (ignored_spec, ignored_ready) =
        never_connecting_spec("none-stubborn", Some(&server.temp_dir), SigtermMode::Ignore);
    let ignored = supervisor.spawn(ignored_spec).unwrap();
    wait_for_path(
        &ignored_ready.expect("ready path configured"),
        SETUP_TIMEOUT,
    )
    .await;

    let started = Instant::now();
    ignored.restart(None).await.unwrap();
    let status =
        wait_for_status(&ignored, SETUP_TIMEOUT, |status| status.last_exit.is_some()).await;
    let ignored_elapsed = started.elapsed();
    let exit = status.last_exit.expect("waited for an exit");

    assert_eq!(
        exit.signal,
        Some(9),
        "a child that ignores SIGTERM must still be killed once the budget is spent"
    );
    assert!(
        ignored_elapsed >= DRAIN_BUDGET,
        "the kill came after {ignored_elapsed:?}, before the {DRAIN_BUDGET:?} budget was spent: \
         the drain budget is not being honoured"
    );

    handled.stop().await.unwrap();
    ignored.stop().await.unwrap();
}

/// A caller asking a `protocol: "none"` module for a route is asking for
/// something that can never exist, and the refusal has to say so in a code the
/// SDKs classify as terminal.
///
/// `target_unavailable` is RETRYABLE and `unknown_module` only means "no module
/// of this id is registered or supervised here", and both would be lies here:
/// the module is configured, running, and supervised, and no amount of waiting
/// changes the answer. A retryable code turns every consumer that reaches for
/// this module into a retry loop against the daemon for the lifetime of the
/// process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_to_a_protocol_none_module_is_refused_as_terminal_and_counted() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "none-no-routes";
    let module = supervisor
        .spawn(never_connecting_spec(module_id, None, SigtermMode::Default).0)
        .unwrap();
    wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running
    })
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let before = server_counter_key(
        &mut client,
        700,
        "route_open_refused_by_code",
        "module_no_protocol",
    )
    .await;

    let error =
        attach_error_on_stream(&mut client, &project, 701, "ses-no-protocol", module_id).await;

    assert_eq!(
        error.code,
        subc_protocol::error_codes::MODULE_NO_PROTOCOL,
        "a module that speaks no subc wire must be refused by its own code, not by one \
         that invites a retry"
    );
    assert!(
        !subc_protocol::error_codes::is_retryable_route_open(&error.code),
        "the refusal code must classify terminal in the shared predicate the SDKs call"
    );
    let after = server_counter_key(
        &mut client,
        702,
        "route_open_refused_by_code",
        "module_no_protocol",
    )
    .await;
    assert_eq!(
        after,
        before + 1,
        "the refusal must be counted under its own key, or an operator cannot see \
         consumers reaching for a module that serves nothing"
    );

    module.stop().await.unwrap();
}

/// `supervisor.list` has to carry the declaration, because `live` cannot be read
/// without it: `true` means "registered and routable" for one protocol and
/// "the process is up" for the other, and a reader with only the boolean cannot
/// tell which claim it is holding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_list_carries_the_declared_protocol_beside_live() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let none_module = supervisor
        .spawn(never_connecting_spec("listed-none", None, SigtermMode::Default).0)
        .unwrap();
    let subc_module = spawn_stub(&server, &supervisor, "listed-subc").await;
    wait_for_status(&none_module, SETUP_TIMEOUT, |status| status.live).await;
    wait_for_status(&subc_module, SETUP_TIMEOUT, |status| status.live).await;

    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &control_request_frame(710, ClientControlRequest::SupervisorList {}),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let frame = read_frame_timeout(&mut client).await;
    let modules = match serde_json::from_slice::<ClientControlResponse>(&frame.body).unwrap() {
        ClientControlResponse::SupervisorList { modules, .. } => modules,
        other => panic!("unexpected supervisor.list response: {other:?}"),
    };
    let entry = |module_id: &str| {
        modules
            .iter()
            .find(|entry| entry.module_id == module_id)
            .unwrap_or_else(|| panic!("{module_id} missing from supervisor.list"))
            .clone()
    };

    let none_entry = entry("listed-none");
    assert_eq!(none_entry.protocol, ModuleProtocol::None);
    assert!(
        none_entry.live,
        "a running protocol: none module is as live as the daemon can say it is"
    );

    let subc_entry = entry("listed-subc");
    assert_eq!(
        subc_entry.protocol,
        ModuleProtocol::Subc,
        "a module that declares nothing is a subc module, exactly as it was before \
         this field existed"
    );
    assert!(subc_entry.live);

    none_module.stop().await.unwrap();
    subc_module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_describe_connected_clients_tracks_auth_connections() {
    let server = TestServer::start().await;
    let mut first = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    assert_eq!(connected_client_count(&mut first, 130).await, 1);

    let second = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let deadline = Instant::now() + SETUP_TIMEOUT;
    let mut corr = 131;
    loop {
        if connected_client_count(&mut first, corr).await == 2 {
            break;
        }
        corr += 1;
        assert!(
            Instant::now() < deadline,
            "connected_clients did not increment after peer authentication"
        );
        sleep(Duration::from_millis(10)).await;
    }
    drop(second);

    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        corr += 1;
        if connected_client_count(&mut first, corr).await == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "connected_clients did not decrement after peer disconnect"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_round_trip_via_tagged_shape_forwards_through_stub() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-forwarding";
    let (module, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, module_id, "attach-forwarding").await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 101, "ses-forwarding").await;
    let attach_event = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach"
    })
    .await;
    assert_eq!(
        attach_event["target"]["kind"].as_str(),
        Some("tool_provider")
    );
    assert_eq!(
        attach_event["target"]["module_id"].as_str(),
        Some(module_id)
    );
    assert_eq!(attach_event["principal"]["kind"].as_str(), Some("direct"));
    // subc canonicalizes project_root via cortexkit-paths (ProjectRootId) before
    // relaying — NOT raw fs::canonicalize, which keeps Windows' verbatim \\?\ prefix.
    // Assert against the same canonicalization subc uses so this holds on every OS.
    let canonical_project = subc_daemon::ProjectRootId::from_path(project.path())
        .unwrap()
        .as_path()
        .to_path_buf();
    assert_eq!(
        attach_event["identity"]["project_root"].as_str(),
        canonical_project.to_str()
    );
    assert_eq!(
        attach_event["identity"]["session"].as_str(),
        Some("ses-forwarding")
    );
    assert!(attach_event.get("config").is_none());
    assert!(ack.route_channel > 0);
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);
    assert!(server
        .forwarding
        .has_route_channel(ack.route_channel)
        .unwrap());

    let payload = br#"{"jsonrpc":"2.0","id":7,"method":"read","params":{"path":"Cargo.toml"}}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, 202, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, ack.route_channel);
    assert_eq!(response.header.corr, 202);
    assert_eq!(response.body, payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_non_reserved_consumer_identity_stamps_reserved_principal() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let target_module_id = "fake-aft-principal-non-reserved-target";
    let (target, events_path) = spawn_stub_with_events_path(
        &server,
        &supervisor,
        target_module_id,
        "principal-non-reserved",
    )
    .await;

    let consumer_module_id = "fake-consumer-non-reserved";
    let consumer = spawn_stub(&server, &supervisor, consumer_module_id).await;
    let launch_nonce = server
        .supervisor_handle
        .spawn_launch_nonce_for(consumer_module_id)
        .expect("every supervised spawn records a consumer launch nonce");
    assert!(
        server
            .supervisor_handle
            .reserved_launch_nonce_for(consumer_module_id)
            .is_none(),
        "non-reserved modules must not gain HELLO-gating reserved nonce state"
    );

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &attach_frame(
            150,
            attach_request_with_consumer_identity(
                &project,
                "ses-principal-non-reserved",
                target_module_id,
                Some(ConsumerIdentity {
                    module_id: consumer_module_id.to_string(),
                    launch_nonce,
                }),
            ),
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let ack_frame = read_frame_timeout(&mut client).await;
    assert_eq!(ack_frame.header.ty, FrameType::Response);
    assert_eq!(ack_frame.header.corr, 150);
    let attach_event = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach"
    })
    .await;
    assert_eq!(attach_event["principal"]["kind"].as_str(), Some("reserved"));
    assert_eq!(
        attach_event["principal"]["module_id"].as_str(),
        Some(consumer_module_id)
    );

    target.stop().await.unwrap();
    consumer.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserved_consumer_identity_stamps_reserved_principal() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let target_module_id = "fake-aft-principal-target";
    let (target, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, target_module_id, "principal-reserved")
            .await;

    let consumer_module_id = "fake-consumer-reserved";
    let consumer = supervisor
        .spawn(reserved_stub_spec(&server, consumer_module_id))
        .unwrap();
    wait_for_registration(&server.registry, consumer_module_id, SETUP_TIMEOUT).await;
    let launch_nonce = server
        .supervisor_handle
        .reserved_launch_nonce_for(consumer_module_id)
        .expect("reserved spawn records HELLO-gating launch nonce");
    assert_eq!(
        server
            .supervisor_handle
            .spawn_launch_nonce_for(consumer_module_id)
            .as_deref(),
        Some(launch_nonce.as_str()),
        "reserved modules record the same nonce for spawn attestation and HELLO gating"
    );

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &attach_frame(
            151,
            attach_request_with_consumer_identity(
                &project,
                "ses-principal-reserved",
                target_module_id,
                Some(ConsumerIdentity {
                    module_id: consumer_module_id.to_string(),
                    launch_nonce,
                }),
            ),
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let ack_frame = read_frame_timeout(&mut client).await;
    assert_eq!(ack_frame.header.ty, FrameType::Response);
    assert_eq!(ack_frame.header.corr, 151);
    let attach_event = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach"
    })
    .await;
    assert_eq!(attach_event["principal"]["kind"].as_str(), Some("reserved"));
    assert_eq!(
        attach_event["principal"]["module_id"].as_str(),
        Some(consumer_module_id)
    );

    target.stop().await.unwrap();
    consumer.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatched_consumer_identity_rejects_without_route_bind() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let target_module_id = "fake-aft-principal-mismatch-target";
    let (target, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, target_module_id, "principal-mismatch")
            .await;

    let consumer_module_id = "fake-consumer-mismatch";
    let consumer = supervisor
        .spawn(reserved_stub_spec(&server, consumer_module_id))
        .unwrap();
    wait_for_registration(&server.registry, consumer_module_id, SETUP_TIMEOUT).await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &attach_frame(
            152,
            attach_request_with_consumer_identity(
                &project,
                "ses-principal-mismatch",
                target_module_id,
                Some(ConsumerIdentity {
                    module_id: consumer_module_id.to_string(),
                    launch_nonce: "wrong-nonce".to_string(),
                }),
            ),
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let error_frame = read_frame_timeout(&mut client).await;
    let error = assert_error(&error_frame, 0, 152, "bad_consumer_identity");
    assert!(error.message.contains(consumer_module_id));
    assert_no_stub_event_within(&events_path, Duration::from_millis(100), |event| {
        event["kind"] == "attach"
    })
    .await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 0);

    target.stop().await.unwrap();
    consumer.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_spawn_nonce_rejects_without_route_bind_after_respawn() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let target_module_id = "fake-aft-principal-stale-target";
    let (target, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, target_module_id, "principal-stale")
            .await;

    let consumer_module_id = "fake-consumer-stale";
    let first = spawn_stub(&server, &supervisor, consumer_module_id).await;
    let stale_nonce = server
        .supervisor_handle
        .spawn_launch_nonce_for(consumer_module_id)
        .expect("first spawn records nonce");
    first.stop().await.unwrap();
    wait_for_registration_absent(&server.registry, consumer_module_id, SETUP_TIMEOUT).await;

    let second = spawn_stub(&server, &supervisor, consumer_module_id).await;
    let current_nonce = server
        .supervisor_handle
        .spawn_launch_nonce_for(consumer_module_id)
        .expect("respawn records nonce");
    assert_ne!(
        stale_nonce, current_nonce,
        "respawn must rotate launch nonce"
    );

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &attach_frame(
            154,
            attach_request_with_consumer_identity(
                &project,
                "ses-principal-stale",
                target_module_id,
                Some(ConsumerIdentity {
                    module_id: consumer_module_id.to_string(),
                    launch_nonce: stale_nonce,
                }),
            ),
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let error_frame = read_frame_timeout(&mut client).await;
    let error = assert_error(&error_frame, 0, 154, "bad_consumer_identity");
    assert!(error.message.contains(consumer_module_id));
    assert_no_stub_event_within(&events_path, Duration::from_millis(100), |event| {
        event["kind"] == "attach"
    })
    .await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 0);

    target.stop().await.unwrap();
    second.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_consumer_identity_rejects_without_route_bind() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let target_module_id = "fake-aft-principal-unknown-target";
    let (target, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, target_module_id, "principal-unknown")
            .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &attach_frame(
            153,
            attach_request_with_consumer_identity(
                &project,
                "ses-principal-unknown",
                target_module_id,
                Some(ConsumerIdentity {
                    module_id: "missing-consumer".to_string(),
                    launch_nonce: "nonce".to_string(),
                }),
            ),
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let error_frame = read_frame_timeout(&mut client).await;
    assert_error(&error_frame, 0, 153, "bad_consumer_identity");
    assert_no_stub_event_within(&events_path, Duration::from_millis(100), |event| {
        event["kind"] == "attach"
    })
    .await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 0);

    target.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_tool_provider_hello_registers_without_hijacking_active_forwarding_module() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let provider_id = "fake-aft-role-aware-provider";
    let (provider, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, provider_id, "role-aware-provider").await;

    let consumer_id = "subc-mcp-consumer-role-aware";
    let mut consumer = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let consumer_ack =
        register_manifest_on_stream(&mut consumer, consumer_manifest(consumer_id), 301).await;
    let consumer_registration =
        wait_for_registration(&server.registry, consumer_id, SETUP_TIMEOUT).await;
    assert_eq!(consumer_ack.negotiated_ver, PROTOCOL_VERSION);
    assert_eq!(consumer_registration.manifest.module_id, consumer_id);

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 303, "ses-role-aware").await;
    let attach = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach"
            && event["route_channel"].as_u64() == Some(u64::from(ack.route_channel))
    })
    .await;
    assert_eq!(
        attach["route_channel"].as_u64(),
        Some(u64::from(ack.route_channel))
    );

    let payload = br#"{"jsonrpc":"2.0","id":"role-aware"}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, 304, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_response(&response, ack.route_channel, 304, payload);

    drop(consumer);
    provider.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reserved_module_spawned_by_supervisor_registers_but_foreign_hello_is_rejected() {
    // The credential-vault protection, proven end-to-end through real process spawn +
    // env injection (not just the unit-level handle): a reserved module supervised by
    // the daemon registers because it echoes the launch nonce subc injected, but a
    // foreign authenticated connection claiming the SAME reserved module_id (e.g. a
    // key-holder impersonating the vault) is rejected.
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "reserved-vault";

    // The supervisor spawns the reserved stub; it reads SUBC_LAUNCH_NONCE and echoes
    // it, so it registers. (spawn waits for registration, so reaching here proves the
    // real nonce round-trip succeeded.)
    let module = supervisor
        .spawn(reserved_stub_spec(&server, module_id))
        .unwrap();
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;

    // A foreign authed connection claiming the same reserved module_id, with NO
    // nonce, is rejected reserved_module — it cannot impersonate the vault.
    let mut foreign = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut foreign,
        &hello_frame(tool_provider_manifest(module_id), 701),
    )
    .await
    .unwrap();
    foreign.flush().await.unwrap();
    let rejection = read_frame_timeout(&mut foreign).await;
    assert_eq!(rejection.header.ty, FrameType::Error);
    let body: ErrorBody = serde_json::from_slice(&rejection.body).unwrap();
    assert_eq!(body.code, "reserved_module");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_aware_channel_zero_misuse_is_rejected_over_real_connections() {
    let server = TestServer::start().await;

    let module_id = "fake-aft-ch0-misuse";
    let mut module = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    register_manifest_on_stream(&mut module, tool_provider_manifest(module_id), 501).await;
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;

    write_frame(
        &mut module,
        &control_request_frame(502, ClientControlRequest::ServerDescribe {}),
    )
    .await
    .unwrap();
    module.flush().await.unwrap();
    let module_error =
        read_control_error_on_stream(&mut module, 502, "unsupported_control_frame").await;
    assert!(module_error
        .message
        .contains("module-originated channel-0 REQUEST"));

    let module_ping = Frame::build(
        FrameType::Ping,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        503,
        Vec::new(),
    )
    .unwrap();
    write_frame(&mut module, &module_ping).await.unwrap();
    module.flush().await.unwrap();
    let module_pong = read_frame_timeout(&mut module).await;
    assert_eq!(module_pong.header.ty, FrameType::Pong);
    assert_eq!(module_pong.header.corr, 503);

    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let client_push = Frame::build(
        FrameType::Push,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        504,
        b"client ch0 push".to_vec(),
    )
    .unwrap();
    write_frame(&mut client, &client_push).await.unwrap();
    client.flush().await.unwrap();
    let client_error =
        read_control_error_on_stream(&mut client, 504, "unsupported_control_frame").await;
    assert!(client_error
        .message
        .contains("client-originated channel-0 PUSH"));

    let client_ping = Frame::build(
        FrameType::Ping,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        505,
        Vec::new(),
    )
    .unwrap();
    write_frame(&mut client, &client_ping).await.unwrap();
    client.flush().await.unwrap();
    let client_pong = read_frame_timeout(&mut client).await;
    assert_eq!(client_pong.header.ty, FrameType::Pong);
    assert_eq!(client_pong.header.corr, 505);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_invalid_manifest_is_rejected_without_registration() {
    let server = TestServer::start().await;

    let mismatched_id = "fake-aft-invalid-hello-protocol";
    let mut mismatched = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let mismatched_manifest = tool_provider_manifest(mismatched_id);
    write_frame(
        &mut mismatched,
        &hello_frame_with_protocol(mismatched_manifest, PROTOCOL_VERSION + 1, 511),
    )
    .await
    .unwrap();
    mismatched.flush().await.unwrap();
    let protocol_error =
        read_control_error_on_stream(&mut mismatched, 511, "invalid_manifest").await;
    assert!(protocol_error.message.contains("does not match"));
    assert_eq!(server.registry.active_registration_count().unwrap(), 0);
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let target_error = attach_error_on_stream(
        &mut client,
        &project,
        513,
        "ses-invalid-hello-protocol",
        mismatched_id,
    )
    .await;
    assert_eq!(target_error.code, "unknown_module");

    let blank_id = "   ";
    let mut blank = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut blank,
        &hello_frame(tool_provider_manifest(blank_id), 512),
    )
    .await
    .unwrap();
    blank.flush().await.unwrap();
    let blank_error = read_control_error_on_stream(&mut blank, 512, "invalid_manifest").await;
    assert!(blank_error.message.contains("module_id must not be empty"));
    assert_eq!(server.registry.active_registration_count().unwrap(), 0);
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    let blank_target_error = attach_error_on_stream(
        &mut client,
        &project,
        514,
        "ses-invalid-hello-blank",
        blank_id,
    )
    .await;
    assert_eq!(blank_target_error.code, "unknown_module");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_poll_produces_zero_module_frames() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-status-cache";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "status-cache",
        [("FAKE_AFT_STATUS", "indexing")],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 111, "ses-status-cache").await;
    let status_event = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "status_published"
            && event["route_channel"].as_u64() == Some(u64::from(ack.route_channel))
    })
    .await;
    assert_eq!(status_event["status"].as_str(), Some("indexing"));

    // Poll until subc has cached the status (see wait_for_cached_status: the
    // stub-side status_published event does not guarantee subc received+cached
    // the PUSH yet). Every poll is answered locally — asserted below by the
    // absence of forwarded module frames.
    wait_for_cached_status(&mut client, ack.route_channel, "indexing", 112).await;

    let events = stub_events(&events_path);
    assert!(
        events.iter().all(|event| matches!(
            event.get("kind").and_then(Value::as_str),
            Some("attach" | "status_published")
        )),
        "status poll should be answered locally; unexpected stub events: {events:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_and_liveness_polls_are_fast_while_serial_module_is_busy() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-busy-local-poll";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "busy-local-poll",
        [
            ("FAKE_AFT_CONCURRENCY", "serial"),
            ("FAKE_AFT_DELAY_FROM_BODY", "1"),
            ("FAKE_AFT_STATUS", "scanning"),
        ],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 121, "ses-busy-local-poll").await;
    // Warm the status cache BEFORE the measured window so the later timed poll is
    // guaranteed a cache hit (the stub-side status_published event does not prove
    // subc cached it yet). These warm-up polls are outside the latency measurement.
    wait_for_cached_status(&mut client, ack.route_channel, "scanning", 130).await;

    let data_corr = 122;
    let status_corr = 123;
    let liveness_corr = 124;
    let payload = br#"{"delay_ms":2000,"jsonrpc":"2.0","id":"busy"}"#;
    let data_sent = Instant::now();
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, data_corr, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, data_corr)
    })
    .await;

    let polls_sent = Instant::now();
    write_frame(
        &mut client,
        &route_poll_frame(status_corr, PollKind::Status, ack.route_channel),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &route_poll_frame(liveness_corr, PollKind::Liveness, ack.route_channel),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let status_response = read_frame_timeout(&mut client).await;
    let liveness_response = read_frame_timeout(&mut client).await;
    let poll_latency = Instant::now().duration_since(polls_sent);
    eprintln!("busy-module local poll latency: {poll_latency:?}");
    assert_status_reply(&status_response, status_corr, "scanning");
    assert_liveness_reply(&liveness_response, liveness_corr, true);

    // Correctness ("polls aren't queued behind the busy module") is proven
    // structurally, NOT by an absolute latency bound (which is a perf claim that
    // flakes on a slow/oversubscribed CI runner): both poll responses are read
    // here BEFORE the data response, the stub never observed the poll corrs
    // (assert_stub_did_not_observe_corrs below = answered locally, never
    // forwarded), and the data request that DID reach the module still takes its
    // full ~2000ms delay (asserted further down). poll_latency is logged only.
    let events = stub_events(&events_path);
    assert_stub_did_not_observe_corrs(&events, &[status_corr, liveness_corr]);

    let data_response = read_frame_timeout_for(&mut client, Duration::from_secs(3)).await;
    let data_latency = Instant::now().duration_since(data_sent);
    eprintln!("busy-module data response latency: {data_latency:?}");
    assert_response(&data_response, ack.route_channel, data_corr, payload);
    assert!(
        data_latency >= Duration::from_millis(1_800),
        "busy request did not exercise the requested delay: data_latency={data_latency:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_poll_status_cache_miss_returns_none() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-status-miss";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 131, "ses-status-miss").await;

    let poll_corr = 132;
    write_frame(
        &mut client,
        &route_poll_frame(poll_corr, PollKind::Status, ack.route_channel),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let response = read_frame_timeout(&mut client).await;
    assert_status_none_reply(&response, poll_corr);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_cache_is_evicted_when_client_detaches() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-status-eviction";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "status-eviction",
        [("FAKE_AFT_STATUS", "evict-me")],
    )
    .await;

    let project = TestProject::new();
    let (mut first, first_ack) = attach_client(&server, &project, 141, "ses-status-evict-1").await;
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "status_published"
            && event["route_channel"].as_u64() == Some(u64::from(first_ack.route_channel))
    })
    .await;

    wait_for_cached_status(&mut first, first_ack.route_channel, "evict-me", 142).await;

    drop(first);
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "detach"
            && event["route_channel"].as_u64() == Some(u64::from(first_ack.route_channel))
    })
    .await;

    let (mut second, second_ack) =
        attach_client(&server, &project, 143, "ses-status-evict-2").await;
    let second_attach = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-status-evict-2"
    })
    .await;
    let second_module_channel = second_attach["route_channel"].as_u64().unwrap();
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "status_published"
            && event["route_channel"].as_u64() == Some(second_module_channel)
    })
    .await;

    wait_for_cached_status(&mut second, second_ack.route_channel, "evict-me", 145).await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_poll_returns_false_after_module_connection_is_gone() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-liveness-gone";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 151, "ses-liveness-gone").await;
    assert_liveness_reply(
        &poll_liveness(&mut client, 152, ack.route_channel).await,
        152,
        true,
    );

    module.stop().await.unwrap();
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    let closed = read_frame_timeout(&mut client).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_id,
        "crash",
        Some(false),
        Some(0),
        Some(false),
    );
    let goodbye = read_frame_timeout(&mut client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, ack.route_channel);

    let response = poll_liveness(&mut client, 153, ack.route_channel).await;
    assert_liveness_reply(&response, 153, false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_client_receives_unsolicited_push_and_response_on_bound_route() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-push-single";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_PUSH_ON_REQUEST", "1")],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 151, "ses-push-single").await;
    assert!(ack.route_channel > 0);

    let payload = br#"{"jsonrpc":"2.0","id":"push-single","method":"read"}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, 152, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let (push, _response) =
        read_until_push_and_response(&mut client, ack.route_channel, 152, payload).await;
    assert_eq!(push.header.channel, ack.route_channel);
    assert_eq!(push.body, b"push-event");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_drop_sends_route_goodbye_and_removes_binding() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-detach";
    let (module, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, module_id, "detach").await;

    let project = TestProject::new();
    let (client, ack) = attach_client(&server, &project, 201, "ses-detach").await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);
    assert!(server
        .forwarding
        .has_route_channel(ack.route_channel)
        .unwrap());

    drop(client);

    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    let detach = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "detach"
            && event["route_channel"].as_u64() == Some(u64::from(ack.route_channel))
    })
    .await;
    assert_eq!(
        detach["route_channel"].as_u64(),
        Some(u64::from(ack.route_channel))
    );
    assert!(!server
        .forwarding
        .has_route_channel(ack.route_channel)
        .unwrap());

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_list_enumerates_supervised_modules() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-supervisor-list";
    let module = spawn_stub(&server, &supervisor, module_id).await;
    wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running && status.enabled && status.live
    })
    .await;

    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &control_request_frame(301, ClientControlRequest::SupervisorList {}),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let frame = read_frame_timeout(&mut client).await;
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, 301);
    match serde_json::from_slice(&frame.body).unwrap() {
        ClientControlResponse::SupervisorList {
            generation,
            modules,
        } => {
            assert_eq!(generation, server.registry.generation().unwrap());
            let entry = modules
                .iter()
                .find(|entry| entry.module_id == module_id)
                .expect("supervisor.list should include spawned module");
            assert_eq!(entry.state, "running");
            assert!(entry.enabled);
            assert!(entry.live);
        }
        other => panic!("unexpected supervisor.list response: {other:?}"),
    }

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_restart_bumps_generation_and_goodbyes_open_routes() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-supervisor-restart";
    let module = spawn_stub(&server, &supervisor, module_id).await;
    let generation_before = server.registry.generation().unwrap();

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(
        &mut client,
        &project,
        311,
        "ses-supervisor-restart",
        module_id,
    )
    .await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);

    write_frame(
        &mut client,
        &control_request_frame(
            312,
            ClientControlRequest::SupervisorRestart {
                drain_timeout_ms: None,
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let applied = read_supervisor_ack_and_goodbye(
        &mut client,
        312,
        module_id,
        ack.route_channel,
        "restart",
        false,
    )
    .await;
    assert!(applied);
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    let status = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;
    assert_eq!(status.restart_count, 0);
    assert_eq!(
        status.last_exit.as_ref().map(|exit| exit.kind),
        Some(ExitKind::Clean)
    );
    assert!(server.registry.generation().unwrap() > generation_before);

    module.stop().await.unwrap();
}

/// THE SELF-LANE RESTART DEADLOCK, held dead: `supervisor.restart` must ACK AT
/// INITIATION, while the module still has a request in flight.
///
/// The blocking form replied only after drain + kill + respawn. A caller whose
/// own tool lane rides the target module (an agent bouncing the module its bash
/// runs on) then deadlocks the drain BY CONSTRUCTION: its in-flight request
/// cannot settle until the restart RPC replies, and the reply waits on the
/// drain that is waiting on the request. The drain always timed out and cut the
/// initiator with a GOODBYE -- on a HEALTHY module (incident 2026-08-17).
///
/// TIMING JUSTIFICATION (this test is deliberately timing-shaped, which #6838's
/// no-latency-bounds rule otherwise forbids): the contract under test IS reply
/// timing. The discriminator is tied to the test's own controlled delay, not to
/// machine speed -- the in-flight request holds a 6_000ms delay while the ACK
/// must arrive within the 2s helper window. Old semantics structurally cannot
/// reply before its own drain settles the 6s request, so this test fails by
/// ACK-read timeout under the blocking form (verified by mutation); new
/// semantics reply in one control round trip regardless of load.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_restart_acks_at_initiation_and_still_drains_the_inflight_request() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_drain_timeout(
        &server,
        1,
        Duration::from_millis(10),
        Duration::from_secs(8),
    );
    let module_id = "fake-aft-restart-initiation-ack";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [
            ("FAKE_AFT_DELAY_FROM_BODY", "1"),
            // Receipt push proves the slow request is genuinely in flight before
            // the restart is issued (same in-flightness discipline as the reload
            // drain test above; issue #11).
            ("FAKE_AFT_PUSH_ON_REQUEST", "1"),
        ],
    )
    .await;
    let generation_before = server.registry.generation().unwrap();

    let project = TestProject::new();
    let mut route_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(
        &mut route_client,
        &project,
        421,
        "ses-restart-initiation",
        module_id,
    )
    .await;
    let slow_corr = 422;
    let slow_payload = br#"{"delay_ms":6000,"jsonrpc":"2.0","id":"restart-initiation"}"#;
    write_frame(
        &mut route_client,
        &data_request(ack.route_channel, ack.route_epoch, slow_corr, slow_payload),
    )
    .await
    .unwrap();
    route_client.flush().await.unwrap();
    let receipt = read_push(&mut route_client, ack.route_channel).await;
    assert_eq!(receipt.header.epoch, ack.route_epoch);

    let mut control_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut control_client,
        &control_request_frame(
            423,
            ClientControlRequest::SupervisorRestart {
                drain_timeout_ms: None,
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    control_client.flush().await.unwrap();

    // The load-bearing read: the ACK lands inside the 2s helper window while the
    // in-flight request still holds ~6s of delay. Under completion semantics
    // this read times out -- that is the deadlock class, observed.
    let applied = read_supervisor_ack_on_stream(&mut control_client, 423, module_id).await;
    assert!(applied);

    // And the drain still does its job for the initiator, with the #31 lifecycle
    // pushes bracketing it: route.closing lands first (enqueued before the drain
    // starts -- i.e. before the slow request settles, which pins the ACK-before-
    // drain ordering from the route client's own stream), then the slow request
    // SETTLES with its real response (never cut), then route.closed with
    // drained=true, then the teardown GOODBYE.
    let closing = read_frame_timeout_for(&mut route_client, Duration::from_secs(20)).await;
    assert_route_lifecycle_push(
        &closing,
        "route.closing",
        module_id,
        "restart",
        None,
        None,
        None,
    );
    let response = read_frame_timeout_for(&mut route_client, Duration::from_secs(20)).await;
    assert_response(&response, ack.route_channel, slow_corr, slow_payload);
    let closed = read_frame_timeout_for(&mut route_client, Duration::from_secs(10)).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_id,
        "restart",
        Some(true),
        Some(0),
        Some(false),
    );
    let goodbye = read_frame_timeout_for(&mut route_client, Duration::from_secs(10)).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, ack.route_channel);

    // Completion is observable state, not a reply. With an initiation-shaped
    // ACK the respawn is genuinely async, so registration alone is not enough:
    // the OLD entry can still be registered at first sample. Poll the
    // generation itself -- it advances only when the NEW process re-registers.
    let deadline = tokio::time::Instant::now() + SETUP_TIMEOUT;
    loop {
        if server.registry.generation().unwrap() > generation_before {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "module did not re-register with a new generation after initiation-acked restart"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;

    module.stop().await.unwrap();
}

/// The wedge-bounce escape: `supervisor.restart{drain_timeout_ms: 0}` must CUT
/// the in-flight request instead of waiting the module's configured budget.
///
/// Discriminates the override's REACHABILITY by construction: the supervisor is
/// configured with an 8s drain and the stub holds a 6s request, so ignoring the
/// override would settle the request and a Response frame would appear where
/// this stream asserts `route.closed` -- the frame-by-frame read fails on the
/// frame TYPE, not on a latency bound (#6838). The paired default-path test
/// above proves the same 6s request SETTLES when no override is sent; together
/// they fence both polarities of the override plumbing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_restart_with_zero_drain_override_cuts_the_inflight_request() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_drain_timeout(
        &server,
        1,
        Duration::from_millis(10),
        Duration::from_secs(8),
    );
    let module_id = "fake-aft-restart-drain-now";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [
            ("FAKE_AFT_DELAY_FROM_BODY", "1"),
            ("FAKE_AFT_PUSH_ON_REQUEST", "1"),
        ],
    )
    .await;

    let project = TestProject::new();
    let mut route_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(
        &mut route_client,
        &project,
        431,
        "ses-restart-drain-now",
        module_id,
    )
    .await;
    let slow_corr = 432;
    let slow_payload = br#"{"delay_ms":6000,"jsonrpc":"2.0","id":"restart-drain-now"}"#;
    write_frame(
        &mut route_client,
        &data_request(ack.route_channel, ack.route_epoch, slow_corr, slow_payload),
    )
    .await
    .unwrap();
    route_client.flush().await.unwrap();
    let receipt = read_push(&mut route_client, ack.route_channel).await;
    assert_eq!(receipt.header.epoch, ack.route_epoch);

    let mut control_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut control_client,
        &control_request_frame(
            433,
            ClientControlRequest::SupervisorRestart {
                drain_timeout_ms: Some(0),
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    control_client.flush().await.unwrap();
    let applied = read_supervisor_ack_on_stream(&mut control_client, 433, module_id).await;
    assert!(applied);

    // closing, then IMMEDIATELY closed{drained:false} -- the 6s request is
    // still pending, so any Response here means the override never reached the
    // drain and the configured 8s budget ran instead.
    let closing = read_frame_timeout_for(&mut route_client, Duration::from_secs(10)).await;
    assert_route_lifecycle_push(
        &closing,
        "route.closing",
        module_id,
        "restart",
        None,
        None,
        None,
    );
    let closed = read_frame_timeout_for(&mut route_client, Duration::from_secs(10)).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_id,
        "restart",
        Some(false),
        Some(0),
        Some(false),
    );
    let goodbye = read_frame_timeout_for(&mut route_client, Duration::from_secs(10)).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, ack.route_channel);

    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draining_notice_precedes_quiescence_wait_and_route_lifecycle_stays_ordered() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_drain_timeout(
        &server,
        1,
        Duration::from_millis(10),
        DRAIN_BUDGET_COVERING_AN_INFLIGHT_REQUEST,
    );
    let module_id = "fake-aft-supervisor-reload-happy";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "draining-before-wait",
        [
            ("FAKE_AFT_DELAY_FROM_BODY", "1"),
            // Receipt evidence for the drain race below: the stub PUSHes on the
            // request channel BEFORE its delay, so reading that push proves the
            // slow request crossed client -> daemon -> module and is genuinely
            // in flight before the reload is issued.
            ("FAKE_AFT_PUSH_ON_REQUEST", "1"),
        ],
    )
    .await;
    let generation_before = server.registry.generation().unwrap();

    let project = TestProject::new();
    let mut route_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(
        &mut route_client,
        &project,
        401,
        "ses-reload-happy",
        module_id,
    )
    .await;
    let slow_corr = 402;
    let slow_payload = br#"{"delay_ms":50,"jsonrpc":"2.0","id":"reload-happy"}"#;
    write_frame(
        &mut route_client,
        &data_request(ack.route_channel, ack.route_epoch, slow_corr, slow_payload),
    )
    .await
    .unwrap();
    route_client.flush().await.unwrap();

    // Wait for the module's receipt push before reloading. Without it the test
    // writes the request on one connection and the reload on another, and the
    // daemon's two reader tasks race: ~half the time the drain's phase one lands
    // before the request frame is even read, the request is never in flight, the
    // drain settles on an empty route, and the client's first frame is the
    // teardown GOODBYE instead of the Response (issue #11, 10/20 on master).
    // The drain contract is "drain what is in flight at drain start" -- the test
    // must establish in-flightness, not assume its own write raced ahead.
    let receipt = read_push(&mut route_client, ack.route_channel).await;
    assert_eq!(receipt.header.epoch, ack.route_epoch);

    let mut control_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut control_client,
        &control_request_frame(
            403,
            ClientControlRequest::SupervisorReload {
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    control_client.flush().await.unwrap();

    let closing = read_frame_timeout(&mut route_client).await;
    assert_route_lifecycle_push(
        &closing,
        "route.closing",
        module_id,
        "reload",
        None,
        None,
        None,
    );
    let response = read_frame_timeout(&mut route_client).await;
    assert_response(&response, ack.route_channel, slow_corr, slow_payload);
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_terminal(event, "response", ack.route_channel, slow_corr)
    })
    .await;
    let events = stub_events(&events_path);
    let draining_position = event_position(&events, |event| event["kind"] == "draining");
    let terminal_position = event_position(&events, |event| {
        event_is_terminal(event, "response", ack.route_channel, slow_corr)
    });
    assert!(
        draining_position < terminal_position,
        "draining command must reach the module before the in-flight request settles"
    );
    let closed = read_frame_timeout(&mut route_client).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_id,
        "reload",
        Some(true),
        Some(0),
        Some(false),
    );
    let goodbye = read_frame_timeout(&mut route_client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, ack.route_channel);

    let applied = read_supervisor_ack_on_stream(&mut control_client, 403, module_id).await;
    assert!(applied);
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    assert!(server.registry.generation().unwrap() > generation_before);

    let mut fresh_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let fresh_ack = attach_on_stream(
        &mut fresh_client,
        &project,
        404,
        "ses-reload-fresh",
        module_id,
    )
    .await;
    let fresh_payload = br#"{"jsonrpc":"2.0","id":"after-reload"}"#;
    write_frame(
        &mut fresh_client,
        &data_request(
            fresh_ack.route_channel,
            fresh_ack.route_epoch,
            405,
            fresh_payload,
        ),
    )
    .await
    .unwrap();
    fresh_client.flush().await.unwrap();
    // The respawned stub runs with the same env, so this request also gets a
    // receipt push ahead of its response.
    let _fresh_receipt = read_push(&mut fresh_client, fresh_ack.route_channel).await;
    let fresh_response = read_frame_timeout(&mut fresh_client).await;
    assert_response(&fresh_response, fresh_ack.route_channel, 405, fresh_payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_lifecycle_sends_one_push_per_connection_when_routes_share_client() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_drain_timeout(
        &server,
        1,
        Duration::from_millis(10),
        DRAIN_BUDGET_COVERING_AN_INFLIGHT_REQUEST,
    );
    let module_id = "fake-aft-supervisor-reload-dedup";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let project = TestProject::new();
    let mut route_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let first = attach_on_stream(
        &mut route_client,
        &project,
        406,
        "ses-reload-dedup-first",
        module_id,
    )
    .await;
    let second = attach_on_stream(
        &mut route_client,
        &project,
        407,
        "ses-reload-dedup-second",
        module_id,
    )
    .await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 2);

    let mut control_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut control_client,
        &control_request_frame(
            408,
            ClientControlRequest::SupervisorReload {
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    control_client.flush().await.unwrap();

    let mut closing_count = 0;
    let mut closed_count = 0;
    let mut goodbye_channels = BTreeSet::new();
    for _ in 0..4 {
        let frame = read_frame_timeout(&mut route_client).await;
        match frame.header.ty {
            FrameType::Push => {
                match serde_json::from_slice::<Value>(&frame.body).unwrap()["op"].as_str() {
                    Some("route.closing") => {
                        assert_route_lifecycle_push(
                            &frame,
                            "route.closing",
                            module_id,
                            "reload",
                            None,
                            None,
                            None,
                        );
                        closing_count += 1;
                    }
                    Some("route.closed") => {
                        assert_route_lifecycle_push(
                            &frame,
                            "route.closed",
                            module_id,
                            "reload",
                            Some(true),
                            Some(0),
                            Some(false),
                        );
                        closed_count += 1;
                    }
                    op => panic!("unexpected channel-0 PUSH op: {op:?}"),
                }
            }
            FrameType::Goodbye => {
                goodbye_channels.insert(frame.header.channel);
            }
            ty => panic!("unexpected frame during route lifecycle teardown: {ty:?}"),
        }
    }
    assert_eq!(closing_count, 1, "one route.closing per client connection");
    assert_eq!(closed_count, 1, "one route.closed per client connection");
    assert_eq!(
        goodbye_channels,
        BTreeSet::from([first.route_channel, second.route_channel])
    );
    assert_no_frame_within(&mut route_client, Duration::from_millis(100)).await;

    let applied = read_supervisor_ack_on_stream(&mut control_client, 408, module_id).await;
    assert!(applied);
    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_reload_rejects_new_work_during_drain() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_drain_timeout(
        &server,
        1,
        Duration::from_millis(10),
        DRAIN_BUDGET_COVERING_AN_INFLIGHT_REQUEST,
    );
    let module_id = "fake-aft-supervisor-reload-rejects";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "reload-rejects",
        [("FAKE_AFT_DELAY_FROM_BODY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut route_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(
        &mut route_client,
        &project,
        411,
        "ses-reload-rejects",
        module_id,
    )
    .await;
    let slow_corr = 412;
    // The in-flight request is what HOLDS THE DRAIN OPEN, and everything this
    // test asserts happens while the module is draining. The drain's completion
    // condition is "in-flight settles", so this delay is not a timing margin --
    // it is the structural guarantee that the state under test still exists when
    // the assertions run. At 150 ms the drain could finish, the module respawn,
    // and the route.open below be refused `module_warming` instead of
    // `module_reloading`: a LATER PHASE OF THE SAME RELOAD, so the test read as
    // a code regression when the real fault was that it had outrun its subject.
    // Well inside DRAIN_BUDGET_COVERING_AN_INFLIGHT_REQUEST, so the drain still
    // completes and the `drained: true` assertion below is unchanged.
    let slow_payload = format!(
        r#"{{"delay_ms":{},"jsonrpc":"2.0","id":"reload-rejects"}}"#,
        IN_FLIGHT_HOLD.as_millis()
    );
    let slow_payload = slow_payload.as_bytes();
    write_frame(
        &mut route_client,
        &data_request(ack.route_channel, ack.route_epoch, slow_corr, slow_payload),
    )
    .await
    .unwrap();
    route_client.flush().await.unwrap();
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, slow_corr)
    })
    .await;

    let mut control_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut control_client,
        &control_request_frame(
            413,
            ClientControlRequest::SupervisorReload {
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    control_client.flush().await.unwrap();
    // Connected BEFORE the drain is observed, so the window between observing
    // `Draining` and the route.open landing holds one frame write rather than a
    // TCP connect plus an HMAC handshake. `Draining` is a transient state, and
    // observe-then-act across a round trip is a race however long the state
    // usually lasts -- this removes the work from the window instead of hoping
    // the window is wide enough.
    let mut open_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Draining
    })
    .await;

    let route_open_error = attach_error_on_stream(
        &mut open_client,
        &project,
        414,
        "ses-reload-rejected-open",
        module_id,
    )
    .await;
    assert_eq!(route_open_error.code, "module_reloading");

    let rejected_corr = 415;
    let rejected_payload = br#"{"jsonrpc":"2.0","id":"should-reject"}"#;
    write_frame(
        &mut route_client,
        &data_request(
            ack.route_channel,
            ack.route_epoch,
            rejected_corr,
            rejected_payload,
        ),
    )
    .await
    .unwrap();
    route_client.flush().await.unwrap();
    // Five frames arrive: two channel-0 lifecycle pushes plus the
    // in-flight slow Response (slow_corr, held IN_FLIGHT_HOLD) and the rejection
    // ERROR (rejected_corr) race -- the slow call was already in flight when drain
    // began, so on a slow/oversubscribed runner its Response can land before the
    // rejection. The GOODBYE follows route.closed once the route drains. Classify by
    // corr/type instead of assuming arrival order (which flaked on Windows CI).
    //
    // Each read may wait for the slow Response, which lands a full
    // IN_FLIGHT_HOLD after it was sent, so the per-frame wait is the hold PLUS
    // the ordinary read margin. A bare READ_TIMEOUT equal to the hold made that
    // read race its own timeout whenever the earlier frames arrived promptly.
    let mut saw_rejected = false;
    let mut saw_slow_response = false;
    let mut saw_goodbye = false;
    let mut saw_closing = false;
    let mut saw_closed = false;
    for _ in 0..5 {
        let frame = read_frame_timeout_for(&mut route_client, IN_FLIGHT_HOLD + READ_TIMEOUT).await;
        if frame.header.channel == 0 && frame.header.ty == FrameType::Push {
            if !saw_closing {
                assert_route_lifecycle_push(
                    &frame,
                    "route.closing",
                    module_id,
                    "reload",
                    None,
                    None,
                    None,
                );
                saw_closing = true;
            } else {
                assert_route_lifecycle_push(
                    &frame,
                    "route.closed",
                    module_id,
                    "reload",
                    Some(true),
                    Some(0),
                    Some(false),
                );
                saw_closed = true;
            }
            continue;
        }
        assert_eq!(frame.header.channel, ack.route_channel);
        match frame.header.ty {
            FrameType::Error if frame.header.corr == rejected_corr => {
                assert_error(&frame, ack.route_channel, rejected_corr, "module_reloading");
                saw_rejected = true;
            }
            FrameType::Response if frame.header.corr == slow_corr => {
                assert_response(&frame, ack.route_channel, slow_corr, slow_payload);
                saw_slow_response = true;
            }
            FrameType::Goodbye => {
                // GOODBYE only after the in-flight slow call drained.
                assert!(
                    saw_slow_response && saw_closed,
                    "route GOODBYE arrived before route.closed or the in-flight slow response drained"
                );
                saw_goodbye = true;
            }
            other => panic!(
                "unexpected frame during reload drain: ty={other:?} corr={}",
                frame.header.corr
            ),
        }
    }
    assert!(
        saw_closing && saw_rejected && saw_slow_response && saw_closed && saw_goodbye,
        "expected route lifecycle pushes, rejection ERROR ({rejected_corr}), slow Response ({slow_corr}), and route GOODBYE; got closing={saw_closing} rejected={saw_rejected} slow={saw_slow_response} closed={saw_closed} goodbye={saw_goodbye}"
    );
    let applied = read_supervisor_ack_on_stream(&mut control_client, 413, module_id).await;
    assert!(applied);

    // The data-plane `module_reloading` is a pre-send guarantee callers retry
    // on (see `error_codes::MODULE_RELOADING`): the module must never have
    // received the request the daemon rejected. The slow request is the
    // positive control, since the same journal records it.
    let events = stub_events(&events_path);
    assert!(
        events
            .iter()
            .any(|event| event_is_request_received(event, ack.route_channel, slow_corr)),
        "control: the stub journal should record the in-flight slow request"
    );
    assert!(
        !events.iter().any(|event| event_is_request_received(
            event,
            ack.route_channel,
            rejected_corr
        )),
        "module_reloading must be answered before forwarding, but the module received \
         the rejected request (corr {rejected_corr})"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_supervisor_ops_remain_coherent() {
    let server = TestServer::start().await;
    // The drain budget must comfortably cover the 100 ms slow request below,
    // because this test asserts `drained: true` -- that the drain WAITED for
    // the in-flight request rather than timing out on it. 500 ms was enough on
    // every developer machine and on the CI push run, and not enough on one
    // hosted Windows release-verify run (0.17.45), where the stub's reply plus
    // its event-file write crossed the budget and the frame read
    // `drained: false`. The margin failed, not the mechanism: the drain still
    // completes in ~100 ms when the request is fast, so widening the ceiling
    // costs nothing on the passing path.
    let supervisor = supervisor_with_drain_timeout(
        &server,
        1,
        Duration::from_millis(10),
        Duration::from_secs(5),
    );
    let module_id = "fake-aft-supervisor-concurrent";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "supervisor-concurrent",
        [("FAKE_AFT_DELAY_FROM_BODY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut route_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(
        &mut route_client,
        &project,
        431,
        "ses-supervisor-concurrent",
        module_id,
    )
    .await;
    let slow_corr = 432;
    let slow_payload = br#"{"delay_ms":100,"jsonrpc":"2.0","id":"supervisor-concurrent"}"#;
    write_frame(
        &mut route_client,
        &data_request(ack.route_channel, ack.route_epoch, slow_corr, slow_payload),
    )
    .await
    .unwrap();
    route_client.flush().await.unwrap();
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, slow_corr)
    })
    .await;

    let mut reload_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let mut disable_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let mut restart_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();

    let reload = async {
        write_frame(
            &mut reload_client,
            &control_request_frame(
                433,
                ClientControlRequest::SupervisorReload {
                    module_id: module_id.to_string(),
                },
            ),
        )
        .await
        .unwrap();
        reload_client.flush().await.unwrap();
        read_frame_timeout_for(&mut reload_client, SETUP_TIMEOUT).await
    };
    let disable = async {
        write_frame(
            &mut disable_client,
            &control_request_frame(
                434,
                ClientControlRequest::SupervisorSetEnabled {
                    module_id: module_id.to_string(),
                    enabled: false,
                },
            ),
        )
        .await
        .unwrap();
        disable_client.flush().await.unwrap();
        read_frame_timeout_for(&mut disable_client, SETUP_TIMEOUT).await
    };
    let restart = async {
        write_frame(
            &mut restart_client,
            &control_request_frame(
                435,
                ClientControlRequest::SupervisorRestart {
                    drain_timeout_ms: None,
                    module_id: module_id.to_string(),
                },
            ),
        )
        .await
        .unwrap();
        restart_client.flush().await.unwrap();
        read_frame_timeout_for(&mut restart_client, SETUP_TIMEOUT).await
    };
    let (reload_frame, disable_frame, restart_frame) = tokio::join!(reload, disable, restart);

    let mut saw_disable_ack = false;
    for (frame, corr) in [
        (reload_frame, 433_u64),
        (disable_frame, 434_u64),
        (restart_frame, 435_u64),
    ] {
        assert_eq!(frame.header.channel, 0);
        assert_eq!(frame.header.corr, corr);
        match frame.header.ty {
            FrameType::Response => {
                let response: ClientControlResponse = serde_json::from_slice(&frame.body).unwrap();
                match response {
                    ClientControlResponse::SupervisorAck {
                        module_id: response_module_id,
                        applied,
                    } => {
                        assert_eq!(response_module_id, module_id);
                        if corr == 434 {
                            assert!(applied, "disable should apply exactly once");
                            saw_disable_ack = true;
                        } else {
                            assert!(applied, "reload/restart acks should report applied=true");
                        }
                    }
                    other => panic!("unexpected supervisor response: {other:?}"),
                }
            }
            FrameType::Error => {
                assert_ne!(corr, 434, "disable must not fail: {frame:?}");
                assert_error(&frame, 0, corr, "module_disabled");
            }
            other => panic!("unexpected supervisor control frame type: {other:?}"),
        }
    }
    assert!(
        saw_disable_ack,
        "disable control response must not hang or disappear"
    );

    let mut saw_route_terminal = false;
    let mut saw_goodbye = false;
    let mut closing_reason = None;
    let mut saw_closed = false;
    for _ in 0..5 {
        if saw_route_terminal && saw_closed && saw_goodbye {
            break;
        }
        let frame = read_frame_timeout_for(&mut route_client, SETUP_TIMEOUT).await;
        if frame.header.channel == 0 && frame.header.ty == FrameType::Push {
            let body: Value = serde_json::from_slice(&frame.body).unwrap();
            let reason = body["reason"].as_str().expect("route lifecycle reason");
            assert!(matches!(reason, "reload" | "restart" | "disable"));
            match body["op"].as_str() {
                Some("route.closing") => closing_reason = Some(reason.to_string()),
                Some("route.closed") => {
                    assert_eq!(closing_reason.as_deref(), Some(reason));
                    assert_eq!(body["drained"], Value::Bool(true));
                    assert_eq!(body["abandoned"], Value::from(0));
                    saw_closed = true;
                }
                op => panic!("unexpected route lifecycle op: {op:?}"),
            }
            continue;
        }
        assert_eq!(frame.header.channel, ack.route_channel);
        match frame.header.ty {
            FrameType::Response if frame.header.corr == slow_corr => {
                assert_response(&frame, ack.route_channel, slow_corr, slow_payload);
                saw_route_terminal = true;
            }
            FrameType::Error if frame.header.corr == slow_corr => {
                let body: ErrorBody = serde_json::from_slice(&frame.body).unwrap();
                assert!(
                    matches!(
                        body.code.as_str(),
                        "module_reloading" | "target_unavailable" | "module_disabled"
                    ),
                    "unexpected typed route error during supervisor race: {body:?}"
                );
                saw_route_terminal = true;
            }
            FrameType::Goodbye => {
                assert!(saw_closed, "route GOODBYE arrived before route.closed");
                saw_goodbye = true;
            }
            other => panic!(
                "unexpected route frame during supervisor race: ty={other:?} corr={}",
                frame.header.corr
            ),
        }
    }
    assert!(
        closing_reason.is_some() && saw_route_terminal && saw_closed && saw_goodbye,
        "route client should observe lifecycle pushes, a terminal response/error, and GOODBYE; closing={closing_reason:?} terminal={saw_route_terminal} closed={saw_closed} goodbye={saw_goodbye}"
    );

    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;
    let final_status = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Disabled
            && !status.enabled
            && !status.process_alive
            && !status.live
    })
    .await;
    assert_eq!(final_status.pid, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_long_running_request_is_not_excluded_and_holds_drain() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_drain_timeout(
        &server,
        1,
        Duration::from_millis(10),
        Duration::from_millis(25),
    );
    let module_id = "fake-aft-supervisor-reload-timeout";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "reload-timeout",
        [("FAKE_AFT_DELAY_FROM_BODY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut route_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(
        &mut route_client,
        &project,
        421,
        "ses-reload-timeout",
        module_id,
    )
    .await;
    let held_corr = 422;
    let held_payload =
        br#"{"delay_ms":500,"uncancellable":true,"jsonrpc":"2.0","id":"reload-timeout"}"#;
    write_frame(
        &mut route_client,
        &data_request(ack.route_channel, ack.route_epoch, held_corr, held_payload),
    )
    .await
    .unwrap();
    route_client.flush().await.unwrap();
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, held_corr)
    })
    .await;

    let mut control_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut control_client,
        &control_request_frame(
            423,
            ClientControlRequest::SupervisorReload {
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    control_client.flush().await.unwrap();

    let closing = read_frame_timeout(&mut route_client).await;
    assert_route_lifecycle_push(
        &closing,
        "route.closing",
        module_id,
        "reload",
        None,
        None,
        None,
    );
    let closed = read_frame_timeout(&mut route_client).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_id,
        "reload",
        Some(false),
        Some(0),
        Some(false),
    );
    let goodbye = read_frame_timeout(&mut route_client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, ack.route_channel);
    let applied = read_supervisor_ack_on_stream(&mut control_client, 423, module_id).await;
    assert!(applied);
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bit_set_subscription_is_excluded_and_reported_by_route_closed() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_drain_timeout(
        &server,
        1,
        Duration::from_millis(10),
        Duration::from_millis(25),
    );
    let module_id = "fake-aft-subscription-drain";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "subscription-drain",
        [("FAKE_AFT_DELAY_FROM_BODY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut route_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(
        &mut route_client,
        &project,
        461,
        "ses-subscription-drain",
        module_id,
    )
    .await;
    let held_corr = 462;
    let held_payload =
        br#"{"delay_ms":500,"uncancellable":true,"jsonrpc":"2.0","id":"subscription"}"#;
    write_frame(
        &mut route_client,
        &subscription_request(ack.route_channel, ack.route_epoch, held_corr, held_payload),
    )
    .await
    .unwrap();
    route_client.flush().await.unwrap();
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, held_corr)
    })
    .await;

    let mut control_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut control_client,
        &control_request_frame(
            463,
            ClientControlRequest::SupervisorReload {
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    control_client.flush().await.unwrap();

    let closing = read_frame_timeout(&mut route_client).await;
    assert_route_lifecycle_push(
        &closing,
        "route.closing",
        module_id,
        "reload",
        None,
        None,
        None,
    );
    let closed = read_frame_timeout(&mut route_client).await;
    assert_route_closed_with_excluded_subscriptions(&closed, module_id, true, 1);
    let goodbye = read_frame_timeout(&mut route_client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, ack.route_channel);

    let applied = read_supervisor_ack_on_stream(&mut control_client, 463, module_id).await;
    assert!(applied);
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn omitted_declared_gauge_is_busy_and_increments_counter() {
    let (drained, undeclared_gauge_drains, _) =
        exercise_declared_busy_drain("fake-aft-busy-omitted", "runs_in_flight", "{}").await;

    assert!(
        !drained,
        "an omitted declared gauge must keep the drain busy"
    );
    assert_eq!(
        undeclared_gauge_drains, 1,
        "the omission must increment the operator-visible counter"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_integer_declared_gauge_is_busy() {
    let (drained, undeclared_gauge_drains, _) = exercise_declared_busy_drain(
        "fake-aft-busy-non-integer",
        "runs_in_flight",
        r#"{"runs_in_flight":0.5}"#,
    )
    .await;

    assert!(!drained);
    assert_eq!(undeclared_gauge_drains, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_declared_busy_gauges_are_summed() {
    let (drained_while_opening, counter, _) = exercise_declared_busy_drain(
        "fake-aft-busy-two-positive",
        "runs_in_flight,opening",
        r#"{"runs_in_flight":0,"opening":1}"#,
    )
    .await;
    assert!(!drained_while_opening);
    assert_eq!(counter, 0);

    let (drained_when_both_zero, counter, _) = exercise_declared_busy_drain(
        "fake-aft-busy-two-zero",
        "runs_in_flight,opening",
        r#"{"runs_in_flight":0,"opening":0}"#,
    )
    .await;
    assert!(drained_when_both_zero);
    assert_eq!(counter, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_forever_is_still_torn_down_at_the_drain_ceiling() {
    let (drained, undeclared_gauge_drains, elapsed) = exercise_declared_busy_drain(
        "fake-aft-busy-forever",
        "runs_in_flight",
        r#"{"runs_in_flight":1}"#,
    )
    .await;

    assert!(!drained);
    assert_eq!(undeclared_gauge_drains, 0);
    assert!(
        elapsed >= Duration::from_millis(40) && elapsed < Duration::from_secs(1),
        "a perpetually busy module must be torn down at the configured drain ceiling: {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiesced_drain_reports_abandoned_bindings_without_claiming_they_drained() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_drain_timeout(
        &server,
        1,
        Duration::from_millis(10),
        DRAIN_BUDGET_COVERING_AN_INFLIGHT_REQUEST,
    );
    let module_id = "fake-aft-supervisor-reload-abandoned";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "reload-abandoned",
        [("FAKE_AFT_BIND_NEVER_REPLY_AFTER", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut live_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let live_ack = attach_on_stream(
        &mut live_client,
        &project,
        431,
        "ses-reload-abandoned-live",
        module_id,
    )
    .await;

    let mut pending_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut pending_client,
        &attach_frame(
            432,
            attach_request(&project, "ses-reload-abandoned-pending", module_id),
        ),
    )
    .await
    .unwrap();
    pending_client.flush().await.unwrap();
    wait_for_stub_event_count(
        &events_path,
        SETUP_TIMEOUT,
        |event| event["kind"] == "attach",
        2,
    )
    .await;

    let mut control_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut control_client,
        &control_request_frame(
            433,
            ClientControlRequest::SupervisorReload {
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    control_client.flush().await.unwrap();

    let closing = read_frame_timeout(&mut live_client).await;
    assert_route_lifecycle_push(
        &closing,
        "route.closing",
        module_id,
        "reload",
        None,
        None,
        None,
    );
    let closed = read_frame_timeout(&mut live_client).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_id,
        "reload",
        Some(true),
        Some(1),
        Some(false),
    );
    let goodbye = read_frame_timeout(&mut live_client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, live_ack.route_channel);
    let pending_error = read_frame_timeout(&mut pending_client).await;
    assert_error(&pending_error, 0, 432, "module_reloading");

    let applied = read_supervisor_ack_on_stream(&mut control_client, 433, module_id).await;
    assert!(applied);
    module.stop().await.unwrap();
}

/// A module connection lost while a route.open to it is still waiting on the
/// module's bind reply reports that open in `abandoned`, the same as a drain
/// does, instead of a constant zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_connection_loss_reports_pending_route_open_as_abandoned() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-crash-abandoned";
    // The stub answers the first bind and never answers any later one, so the
    // second client's route.open stays pending until the module goes away.
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "crash-abandoned",
        [("FAKE_AFT_BIND_NEVER_REPLY_AFTER", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut live_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let live_ack = attach_on_stream(
        &mut live_client,
        &project,
        441,
        "ses-crash-abandoned-live",
        module_id,
    )
    .await;

    let mut pending_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut pending_client,
        &attach_frame(
            442,
            attach_request(&project, "ses-crash-abandoned-pending", module_id),
        ),
    )
    .await
    .unwrap();
    pending_client.flush().await.unwrap();
    wait_for_stub_event_count(
        &events_path,
        SETUP_TIMEOUT,
        |event| event["kind"] == "attach",
        2,
    )
    .await;

    module.stop().await.unwrap();

    let closed = read_frame_timeout(&mut live_client).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_id,
        "crash",
        Some(false),
        Some(1),
        Some(false),
    );
    let goodbye = read_frame_timeout(&mut live_client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, live_ack.route_channel);
    let pending_error = read_frame_timeout(&mut pending_client).await;
    assert_eq!(pending_error.header.ty, FrameType::Error);
    assert_eq!(pending_error.header.corr, 442);
}

/// The zero arm of the test above: a lost module connection with no route.open
/// pending reports `abandoned: 0`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_connection_loss_without_pending_route_open_reports_zero_abandoned() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-crash-none-abandoned";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 451, "ses-crash-none-abandoned").await;

    module.stop().await.unwrap();

    let closed = read_frame_timeout(&mut client).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_id,
        "crash",
        Some(false),
        Some(0),
        Some(false),
    );
    let goodbye = read_frame_timeout(&mut client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, ack.route_channel);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_reload_new_binary_failure_returns_reload_failed_and_counts_crash_cap() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_drain_timeout(
        &server,
        1,
        Duration::from_millis(10),
        Duration::from_millis(100),
    );
    let module_id = "fake-aft-supervisor-reload-fails";
    let marker = server.temp_dir.join("fail-registration-after-first.marker");
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [(
            "FAKE_AFT_FAIL_REGISTRATION_AFTER_FIRST_PATH",
            marker.to_str().unwrap(),
        )],
    )
    .await;

    let mut control_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut control_client,
        &control_request_frame(
            431,
            ClientControlRequest::SupervisorReload {
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    control_client.flush().await.unwrap();
    let err = read_control_error_on_stream(&mut control_client, 431, "reload_failed").await;
    assert!(err.message.contains("new child exited before registering"));
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;
    let status = wait_for_status(&module, Duration::from_secs(2), |status| {
        status.state == ModuleState::Failed && !status.registration_active
    })
    .await;
    assert_eq!(status.restart_count, 1);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_reload_unknown_module_returns_unknown_module() {
    let server = TestServer::start().await;
    let mut control_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut control_client,
        &control_request_frame(
            441,
            ClientControlRequest::SupervisorReload {
                module_id: "missing-supervised-module".to_string(),
            },
        ),
    )
    .await
    .unwrap();
    control_client.flush().await.unwrap();
    read_control_error_on_stream(&mut control_client, 441, "unknown_module").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_set_enabled_disable_tears_down_blocks_then_enable_respawns() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-supervisor-enabled";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(
        &mut client,
        &project,
        321,
        "ses-supervisor-disable",
        module_id,
    )
    .await;

    write_frame(
        &mut client,
        &control_request_frame(
            322,
            ClientControlRequest::SupervisorSetEnabled {
                module_id: module_id.to_string(),
                enabled: false,
            },
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let applied = read_supervisor_ack_and_goodbye(
        &mut client,
        322,
        module_id,
        ack.route_channel,
        "disable",
        true,
    )
    .await;
    assert!(applied);
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    let status = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Disabled && !status.enabled && !status.live
    })
    .await;
    assert_eq!(
        status.last_exit.as_ref().map(|exit| exit.kind),
        Some(ExitKind::Clean)
    );

    let error = attach_error_on_stream(&mut client, &project, 323, "ses-disabled", module_id).await;
    assert_eq!(error.code, "target_unavailable");

    let applied = supervisor_ack_on_stream(
        &mut client,
        324,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: module_id.to_string(),
            enabled: true,
        },
        module_id,
    )
    .await;
    assert!(applied);
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running && status.enabled && status.live
    })
    .await;

    let second = attach_on_stream(&mut client, &project, 325, "ses-reenabled", module_id).await;
    assert!(second.route_channel > 0);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_supervisor_restart_reload_return_module_disabled_on_wire() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-supervisor-disabled-wire";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let disabled = supervisor_ack_on_stream(
        &mut client,
        341,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: module_id.to_string(),
            enabled: false,
        },
        module_id,
    )
    .await;
    assert!(disabled);
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    let disabled_status = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Disabled && !status.process_alive && !status.enabled
    })
    .await;
    assert_eq!(disabled_status.pid, None);

    write_frame(
        &mut client,
        &control_request_frame(
            342,
            ClientControlRequest::SupervisorRestart {
                drain_timeout_ms: None,
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    read_control_error_on_stream(&mut client, 342, "module_disabled").await;
    let after_restart = module.status().unwrap();
    assert_eq!(after_restart.state, ModuleState::Disabled);
    assert!(!after_restart.process_alive);
    assert_eq!(after_restart.pid, None);
    assert_eq!(after_restart.restart_count, disabled_status.restart_count);

    write_frame(
        &mut client,
        &control_request_frame(
            343,
            ClientControlRequest::SupervisorReload {
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    read_control_error_on_stream(&mut client, 343, "module_disabled").await;
    let after_reload = module.status().unwrap();
    assert_eq!(after_reload.state, ModuleState::Disabled);
    assert!(!after_reload.process_alive);
    assert_eq!(after_reload.pid, None);
    assert_eq!(after_reload.restart_count, disabled_status.restart_count);

    let disabled_again = supervisor_ack_on_stream(
        &mut client,
        344,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: module_id.to_string(),
            enabled: false,
        },
        module_id,
    )
    .await;
    assert!(
        !disabled_again,
        "idempotent supervisor.set_enabled(false) should report applied=false"
    );
    let after_idempotent = module.status().unwrap();
    assert_eq!(after_idempotent.state, ModuleState::Disabled);
    assert!(!after_idempotent.process_alive);
    assert_eq!(after_idempotent.pid, None);
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_ops_unknown_module_returns_unknown_module() {
    let server = TestServer::start().await;
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();

    write_frame(
        &mut client,
        &control_request_frame(
            331,
            ClientControlRequest::SupervisorRestart {
                drain_timeout_ms: None,
                module_id: "missing-supervised-module".to_string(),
            },
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let frame = read_frame_timeout(&mut client).await;
    assert_error(&frame, 0, 331, "unknown_module");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonzero_goodbye_detaches_one_route_and_leaves_sibling_route_live() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-route-goodbye";
    let (module, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, module_id, "route-goodbye").await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let first_ack = attach_on_stream(&mut client, &project, 601, "ses-route-a", module_id).await;
    let second_ack = attach_on_stream(&mut client, &project, 602, "ses-route-b", module_id).await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 2);

    write_frame(
        &mut client,
        &goodbye_frame(first_ack.route_channel, first_ack.route_epoch, 603),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    wait_for_binding_count(&server.forwarding, 1, SETUP_TIMEOUT).await;
    let detach = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "detach"
            && event["route_channel"].as_u64() == Some(u64::from(first_ack.route_channel))
    })
    .await;
    assert_eq!(
        detach["route_channel"].as_u64(),
        Some(u64::from(first_ack.route_channel))
    );
    assert!(!server
        .forwarding
        .has_route_channel(first_ack.route_channel)
        .unwrap());
    assert!(server
        .forwarding
        .has_route_channel(second_ack.route_channel)
        .unwrap());

    let stale_payload = br#"{"jsonrpc":"2.0","id":"route-a-after-goodbye"}"#;
    write_frame(
        &mut client,
        &data_request(
            first_ack.route_channel,
            first_ack.route_epoch,
            604,
            stale_payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let error_frame = read_frame_timeout(&mut client).await;
    let body = assert_error(
        &error_frame,
        first_ack.route_channel,
        604,
        "unknown_channel",
    );
    assert!(body.message.contains("unknown channel"));

    let live_payload = br#"{"jsonrpc":"2.0","id":"route-b-live"}"#;
    write_frame(
        &mut client,
        &data_request(
            second_ack.route_channel,
            second_ack.route_epoch,
            605,
            live_payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_response(&response, second_ack.route_channel, 605, live_payload);

    let mut unknown_channel = u16::MAX;
    while unknown_channel == first_ack.route_channel || unknown_channel == second_ack.route_channel
    {
        unknown_channel -= 1;
    }
    write_frame(&mut client, &goodbye_frame(unknown_channel, 1, 606))
        .await
        .unwrap();
    client.flush().await.unwrap();
    assert_no_frame_within(&mut client, Duration::from_millis(100)).await;

    let after_unknown_payload = br#"{"jsonrpc":"2.0","id":"route-b-after-unknown"}"#;
    write_frame(
        &mut client,
        &data_request(
            second_ack.route_channel,
            second_ack.route_epoch,
            607,
            after_unknown_payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_response(
        &response,
        second_ack.route_channel,
        607,
        after_unknown_payload,
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_request_with_mismatched_route_epoch_emits_stale_route_epoch_without_forwarding() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-stale-route-epoch";
    let (module, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, module_id, "stale-route-epoch").await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 608, "ses-stale-route-epoch").await;
    let stale_corr = 609;
    write_frame(
        &mut client,
        &data_request(
            ack.route_channel,
            ack.route_epoch + 1,
            stale_corr,
            b"stale-route-epoch",
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let error = read_frame_timeout(&mut client).await;
    assert_eq!(error.header.epoch, ack.route_epoch + 1);
    assert_error(&error, ack.route_channel, stale_corr, "stale_route_epoch");
    assert_no_stub_event_within(&events_path, Duration::from_millis(100), |event| {
        event["kind"] == "request_received" && event["corr"].as_u64() == Some(stale_corr)
    })
    .await;

    let live_payload = b"live-route-epoch";
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, 610, live_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_response(&response, ack.route_channel, 610, live_payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_frame_after_client_detach_is_dropped_and_connection_survives() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-stale-route";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "stale-route",
        [("FAKE_AFT_EMIT_AFTER_DETACH", "1")],
    )
    .await;

    let project = TestProject::new();
    let (client, ack) = attach_client(&server, &project, 301, "ses-stale-route").await;
    drop(client);

    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "stale_emit"
            && event["route_channel"].as_u64() == Some(u64::from(ack.route_channel))
    })
    .await;
    sleep(Duration::from_millis(100)).await;
    let errors: Vec<_> = stub_events(&events_path)
        .into_iter()
        .filter(|event| event.get("kind").and_then(Value::as_str) == Some("error"))
        .collect();
    assert!(
        errors.is_empty(),
        "unexpected module ERROR frames: {errors:?}"
    );

    let (mut next_client, next_ack) =
        attach_client(&server, &project, 302, "ses-stale-route-2").await;
    assert!(next_ack.route_channel > 0);
    let payload = br#"{"jsonrpc":"2.0","id":8,"method":"read"}"#;
    write_frame(
        &mut next_client,
        &data_request(next_ack.route_channel, next_ack.route_epoch, 303, payload),
    )
    .await
    .unwrap();
    next_client.flush().await.unwrap();
    let response = read_frame_timeout(&mut next_client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, next_ack.route_channel);
    assert_eq!(response.header.corr, 303);
    assert_eq!(response.body, payload);

    module.stop().await.unwrap();
}

// A vanished root must ADMIT. This test asserted the opposite until the bind
// relaxation landed, and the inversion is deliberate rather than a weakening:
// refusing here closed the only exit from a paused run, because cancel needs a
// bound route and a renamed directory made that route unopenable forever. The
// aliasing guarantee the old refusal protected now lives in the engine, which
// refuses the two operations that create durable state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_vanished_project_root_attaches_under_its_recorded_identity() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-vanished-project-root";
    let (module, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, module_id, "vanished-project-root").await;

    // Create the root, mint the identity a run would have been recorded under,
    // then delete it. Minting BEFORE deletion is the whole point: the assertion
    // below is that a cancel arriving afterwards addresses that same identity.
    let project = TestProject::new();
    let vanished_root = project.path().join("worktree");
    std::fs::create_dir(&vanished_root).unwrap();
    let identity_while_present = subc_daemon::ProjectRootId::from_path(&vanished_root)
        .unwrap()
        .as_path()
        .to_path_buf();
    std::fs::remove_dir(&vanished_root).unwrap();

    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let request = ClientControlRequest::RouteOpen {
        target: RouteTarget::ToolProvider {
            module_id: module_id.to_string(),
        },
        identity: BindIdentity::new(
            vanished_root,
            "opencode".to_string(),
            "ses-vanished-project-root".to_string(),
        ),
        consumer_identity: None,
        consumer_capabilities: None,
        admission_facts: None,
    };
    write_frame(&mut client, &control_request_frame(481, request))
        .await
        .unwrap();
    client.flush().await.unwrap();

    let frame = read_frame_timeout(&mut client).await;
    assert_eq!(
        frame.header.ty,
        FrameType::Response,
        "a vanished root must not close the route that cancel arrives on"
    );

    let attach_event = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach"
    })
    .await;
    // The identity must equal the one minted while the directory existed. A
    // lexically-normalized path would differ here on any host where the root is
    // reached through a symlink, and the caller would address an empty lineage
    // and be told, confidently, that the run does not exist.
    assert_eq!(
        attach_event["identity"]["project_root"].as_str(),
        identity_while_present.to_str(),
        "a run must keep the identity it was recorded under once its root vanishes"
    );

    module.stop().await.unwrap();
}

// The relaxation is not blanket. A tail with no file name cannot be honestly
// re-appended once the path stops existing, so it is still refused -- and it is
// refused BEFORE the provider is attached, which is the property the original
// test was written to hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_unreconstructable_project_root_returns_error_without_provider_attach() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-invalid-project-root";
    let (module, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, module_id, "invalid-project-root").await;

    let project = TestProject::new();
    let unreconstructable = project.path().join("definitely").join("missing").join("..");
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let request = ClientControlRequest::RouteOpen {
        target: RouteTarget::ToolProvider {
            module_id: module_id.to_string(),
        },
        identity: BindIdentity::new(
            unreconstructable,
            "opencode".to_string(),
            "ses-invalid-project-root".to_string(),
        ),
        consumer_identity: None,
        consumer_capabilities: None,
        admission_facts: None,
    };
    write_frame(&mut client, &control_request_frame(481, request))
        .await
        .unwrap();
    client.flush().await.unwrap();

    let error = read_control_error_on_stream(&mut client, 481, "invalid_project_root").await;
    assert!(!error.message.is_empty());
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    let events = stub_events(&events_path);
    assert!(
        events.iter().all(|event| event["kind"] != "attach"),
        "an unreconstructable project_root must be rejected before route.bind attach; events: {events:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_error_lane_rejection_is_relayed_verbatim_without_committing_binding_then_accepts_later(
) {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-reject";
    let rejecting = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_REJECT_ATTACH", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let error = attach_error_on_stream(&mut client, &project, 401, "ses-reject", module_id).await;
    assert_eq!(error.code, "config_divergence");
    assert_eq!(
        error.message,
        "fake AFT rejected route.bind by FAKE_AFT_REJECT_ATTACH"
    );
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 0);
    drop(client);

    rejecting.stop().await.unwrap();
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;

    let accepting = spawn_stub(&server, &supervisor, module_id).await;
    let (mut accepted_client, ack) = attach_client(&server, &project, 402, "ses-accept").await;
    assert!(ack.route_channel > 0);
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);

    let payload = br#"{"jsonrpc":"2.0","id":9,"method":"read"}"#;
    write_frame(
        &mut accepted_client,
        &data_request(ack.route_channel, ack.route_epoch, 403, payload),
    )
    .await
    .unwrap();
    accepted_client.flush().await.unwrap();
    let response = read_frame_timeout(&mut accepted_client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, ack.route_channel);
    assert_eq!(response.header.corr, 403);
    assert_eq!(response.body, payload);

    accepting.stop().await.unwrap();
}

/// Dropping a connection aborts its spawned bind tails immediately. The module's
/// matching detach and a later successful open prove the synchronous reservation
/// guard left neither a slot pair nor per-target admission behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_drop_during_pending_route_open_releases_reservation() {
    // Keep the relay budget beyond the cleanup deadline. The test therefore
    // proves connection-owned task cancellation ran the synchronous reservation
    // guard; it cannot pass by waiting for the ordinary relay timeout.
    let server = TestServer::start_with_bind_timeout(Duration::from_secs(6)).await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-pending-client-drop";
    let (pending, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "pending-client-drop",
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &attach_frame(491, attach_request(&project, "ses-pending-drop", module_id)),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let attach = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach"
    })
    .await;
    drop(client);

    let detach = wait_for_stub_event(&events_path, HELD_READER_DEADLINE, |event| {
        event["kind"] == "detach" && event["route_channel"] == attach["route_channel"]
    })
    .await;
    assert_eq!(attach["route_channel"], detach["route_channel"]);
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;

    pending.stop().await.unwrap();
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;
    let healthy = spawn_stub(&server, &supervisor, module_id).await;
    let mut later_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let later = attach_on_stream(
        &mut later_client,
        &project,
        492,
        "ses-pending-drop-later",
        module_id,
    )
    .await;
    assert!(later.route_channel > 0);
    wait_for_binding_count(&server.forwarding, 1, SETUP_TIMEOUT).await;

    healthy.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_death_during_route_bind_returns_target_unavailable() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-pending-module-death";
    let (pending, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "pending-module-death",
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut client,
        &attach_frame(501, attach_request(&project, "ses-module-death", module_id)),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach"
    })
    .await;

    pending.stop().await.unwrap();
    let error_frame = read_frame_timeout_for(&mut client, SETUP_TIMEOUT).await;
    assert_error(&error_frame, 0, 501, "target_unavailable");
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;

    let replacement = spawn_stub(&server, &supervisor, module_id).await;
    let later = attach_on_stream(
        &mut client,
        &project,
        502,
        "ses-module-death-replacement",
        module_id,
    )
    .await;
    assert!(later.route_channel > 0);
    wait_for_binding_count(&server.forwarding, 1, SETUP_TIMEOUT).await;

    replacement.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_timeout_releases_reservation_and_later_open_succeeds() {
    let server = TestServer::start_with_bind_timeout(Duration::from_millis(500)).await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-bind-timeout";
    let timing_out = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let error = attach_error_on_stream_with_wait(
        &mut client,
        &project,
        451,
        "ses-bind-timeout",
        module_id,
        SETUP_TIMEOUT,
    )
    .await;
    assert_eq!(error.code, "module_timeout");
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;

    timing_out.stop().await.unwrap();
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;
    let healthy = spawn_stub(&server, &supervisor, module_id).await;
    let ack = attach_on_stream(
        &mut client,
        &project,
        452,
        "ses-bind-timeout-healthy",
        module_id,
    )
    .await;
    assert!(ack.route_channel > 0);
    wait_for_binding_count(&server.forwarding, 1, SETUP_TIMEOUT).await;

    healthy.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_uses_per_module_timeout_override() {
    // The per-module budget is the load-bearing part of #38: with a long
    // daemon-wide default and a tight per-module override, the bind path MUST
    // honor the per-module value rather than the daemon-wide one. Failure
    // here is the exact regression this change is supposed to fix -- a fast
    // daemon-wide default would mask it. Pair with `route_bind_relay_timeout_for`
    // in the unit suite for the resolver itself.
    //
    // Daemon-wide: 30s. Per-module for the target: 100ms. Stub never replies.
    // Bind must time out inside ~2s and the error body must name the per-
    // module budget (100ms), not the daemon-wide 30s.
    let module_id = "fake-aft-per-module-bind-timeout";
    let process_liveness = Arc::new(SupervisorProcessLiveness::new());
    let supervisor_handle = SupervisorHandle::new();
    let daemon = start_test_daemon_with_route_bind_relay_overrides(
        "forwarding-per-module-bind-timeout",
        process_liveness.clone(),
        supervisor_handle.clone(),
        Duration::from_secs(30),
        vec![(module_id.to_string(), Duration::from_millis(100))],
    )
    .await;
    let server = TestServer {
        daemon,
        process_liveness,
        supervisor_handle,
    };
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let timing_out = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let started = Instant::now();
    let error = attach_error_on_stream_with_wait(
        &mut client,
        &project,
        471,
        "ses-per-module-bind-timeout",
        module_id,
        Duration::from_secs(3),
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(error.code, "module_timeout");
    // The bind path must have used the 100ms per-module budget, not the 30s
    // daemon-wide fallback. 2s is a generous ceiling (100ms + RTT + slack)
    // that still fails loudly if the per-module override is ignored -- a
    // daemon-wide path would hang near 30s and the test would time out the
    // outer `attach_error_on_stream_with_wait` first, surfaced as a hang.
    assert!(
        elapsed < Duration::from_secs(2),
        "bind should have honored the per-module 100ms budget, took {elapsed:?}"
    );
    // The error body must name the per-module budget, not the daemon-wide one.
    // `Duration::from_millis(100)` formats as `100ms`; `30s` is the daemon-wide
    // value. Asserting on substring avoids coupling to exact punctuation.
    assert!(
        error.message.contains("100ms"),
        "error message must name the per-module budget (100ms), got: {}",
        error.message
    );
    assert!(
        !error.message.contains("30s"),
        "error message must NOT name the daemon-wide budget (30s), got: {}",
        error.message
    );

    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    timing_out.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_timeout_sends_module_goodbye_for_abandoned_bind() {
    // When route.open's relay was delivered to the module but the bind times out,
    // subc must tell the module to drop any binding it may create late — otherwise
    // a late-accepting module keeps a route subc has torn down. The stub records a
    // `detach` event for every route GOODBYE it receives, so observing that event
    // proves subc sent the abandoned-bind GOODBYE on the module channel.
    let server = TestServer::start_with_bind_timeout(Duration::from_millis(500)).await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-bind-timeout-goodbye";
    let (timing_out, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "bind-timeout-goodbye",
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let error = attach_error_on_stream_with_wait(
        &mut client,
        &project,
        471,
        "ses-bind-timeout-goodbye",
        module_id,
        SETUP_TIMEOUT,
    )
    .await;
    assert_eq!(error.code, "module_timeout");

    // The module received the route.bind relay (attach), then a route GOODBYE
    // (detach) for the SAME module channel once subc abandoned the bind.
    let attach = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach"
    })
    .await;
    let detach = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "detach"
    })
    .await;
    assert_eq!(
        attach["route_channel"], detach["route_channel"],
        "abandoned-bind GOODBYE must target the same module channel the bind reserved"
    );

    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    timing_out.stop().await.unwrap();
}

/// Deadline for the two frames of the head-of-line arm below. A DEADLOCK
/// DETECTOR, not a latency bound: it is a third of that test's bind budget and
/// hundreds of times the loopback round trip it actually measures, so it fires
/// only when a reader is being held across the bind budget.
const HELD_READER_DEADLINE: Duration = Duration::from_secs(2);

/// A pending `route.open` cannot hold a later data frame on the same socket.
/// This assertion is intentionally frame order, not elapsed time. Before the
/// route.open tail was spawned the test was red by construction: the reader did
/// not read the data request until the never-replying bind produced its refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_route_open_cannot_hold_a_bound_neighbours_response() {
    let server = TestServer::start_with_bind_timeout(Duration::from_secs(6)).await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let wedged_id = "fake-aft-spawned-open-wedged";
    let neighbour_id = "fake-aft-spawned-open-neighbour";
    let wedged = spawn_stub_with_env(
        &server,
        &supervisor,
        wedged_id,
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;
    let neighbour = spawn_stub(&server, &supervisor, neighbour_id).await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let neighbour_ack = attach_on_stream(
        &mut client,
        &project,
        610,
        "ses-spawned-open-neighbour",
        neighbour_id,
    )
    .await;

    let payload = br#"{"jsonrpc":"2.0","id":1,"method":"read"}"#;
    write_frame(
        &mut client,
        &attach_frame(
            611,
            attach_request(&project, "ses-spawned-open-wedged", wedged_id),
        ),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &data_request(
            neighbour_ack.route_channel,
            neighbour_ack.route_epoch,
            612,
            payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let response = read_frame_timeout_for(&mut client, SETUP_TIMEOUT).await;
    assert_response(&response, neighbour_ack.route_channel, 612, payload);

    let refusal = read_frame_timeout_for(&mut client, SETUP_TIMEOUT).await;
    assert_eq!(refusal.header.ty, FrameType::Error);
    assert_eq!(refusal.header.channel, 0);
    assert_eq!(refusal.header.corr, 611);
    assert_eq!(
        serde_json::from_slice::<ErrorBody>(&refusal.body)
            .unwrap()
            .code,
        "module_timeout"
    );

    wedged.stop().await.unwrap();
    neighbour.stop().await.unwrap();
}

/// The connection ceiling is a refusal gate, not a waiting semaphore. The
/// overflow refusal must be the first returned frame while every admitted bind
/// is still waiting on the never-replying module.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn route_open_connection_ceiling_refuses_without_queueing() {
    let server = TestServer::start_with_bind_timeout(Duration::from_secs(6)).await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-open-connection-ceiling";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "open-connection-ceiling",
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;
    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let limit = MAX_PENDING_ROUTE_OPENS_PER_CONNECTION;
    let first_corr = 700u64;

    for offset in 0..=limit {
        write_frame(
            &mut client,
            &attach_frame(
                first_corr + offset as u64,
                attach_request(
                    &project,
                    &format!("ses-open-connection-ceiling-{offset}"),
                    module_id,
                ),
            ),
        )
        .await
        .unwrap();
    }
    client.flush().await.unwrap();

    let refusal = read_frame_timeout_for(&mut client, SETUP_TIMEOUT).await;
    assert_eq!(refusal.header.ty, FrameType::Error);
    assert_eq!(refusal.header.corr, first_corr + limit as u64);
    let refusal_body = serde_json::from_slice::<ErrorBody>(&refusal.body).unwrap();
    assert!(
        error_codes::is_retryable_route_open(&refusal_body.code),
        "connection admission refusal must stay retryable, got {}",
        refusal_body.code
    );
    wait_for_stub_event_count(&events_path, SETUP_TIMEOUT, is_attach_event, limit).await;
    assert_eq!(attach_event_count(&events_path), limit);

    drop(client);
    module.stop().await.unwrap();
}

/// Target admission is daemon-wide rather than per connection: spreading a
/// reconnect herd over separate sockets still permits only two safe connection
/// bursts to wait on one module.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn route_open_target_ceiling_spans_client_connections() {
    let server = TestServer::start_with_bind_timeout(Duration::from_secs(30)).await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-open-target-ceiling";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "open-target-ceiling",
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;
    let project = TestProject::new();
    let limit = MAX_PENDING_ROUTE_OPENS_PER_CONNECTION * 2;
    let mut admitted_clients = Vec::with_capacity(limit);

    for offset in 0..limit {
        let mut client = connect_authed_client(&server.connection_file_path)
            .await
            .unwrap();
        write_frame(
            &mut client,
            &attach_frame(
                800 + offset as u64,
                attach_request(
                    &project,
                    &format!("ses-open-target-ceiling-{offset}"),
                    module_id,
                ),
            ),
        )
        .await
        .unwrap();
        client.flush().await.unwrap();
        wait_for_stub_event_count(&events_path, SETUP_TIMEOUT, is_attach_event, offset + 1).await;
        admitted_clients.push(client);
    }

    let mut overflow = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut overflow,
        &attach_frame(
            899,
            attach_request(&project, "ses-open-target-ceiling-overflow", module_id),
        ),
    )
    .await
    .unwrap();
    overflow.flush().await.unwrap();
    let refusal = read_frame_timeout_for(&mut overflow, SETUP_TIMEOUT).await;
    assert_eq!(refusal.header.ty, FrameType::Error);
    assert_eq!(refusal.header.corr, 899);
    let refusal_body = serde_json::from_slice::<ErrorBody>(&refusal.body).unwrap();
    assert!(
        error_codes::is_retryable_route_open(&refusal_body.code),
        "target admission refusal must stay retryable, got {}",
        refusal_body.code
    );
    assert_eq!(attach_event_count(&events_path), limit);

    drop(overflow);
    drop(admitted_clients);
    module.stop().await.unwrap();
}

/// Spawning changes only overlap: clients that issue route.open sequentially
/// still receive one accepted route before sending the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_route_open_acceptance_is_unchanged() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-open-sequential";
    let module = spawn_stub(&server, &supervisor, module_id).await;
    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();

    let first = attach_on_stream(
        &mut client,
        &project,
        900,
        "ses-open-sequential-first",
        module_id,
    )
    .await;
    let second = attach_on_stream(
        &mut client,
        &project,
        901,
        "ses-open-sequential-second",
        module_id,
    )
    .await;

    assert_ne!(first.route_channel, second.route_channel);
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 2);
    module.stop().await.unwrap();
}

/// While a module's breaker is open, an open to it neither relays nor spends
/// the bind budget, so the frames behind it on the same socket are served
/// instead of waiting the budget out.
///
/// This test does not constrain which of the fast breaker refusal and neighbour
/// response arrives first. `route.open` now runs independently of the reader, so
/// channel-0 and data responses are correlated by id rather than arrival order.
///
/// The witnesses here are structural: no relay was sent (the stub's attach
/// count is unchanged across the refusal), the refusal was counted under the
/// breaker's own key, and both frames came back inside a deadline well under
/// the bind budget -- which is what fails if the breaker check is deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_breaker_keeps_a_wedged_module_from_holding_later_frames_on_the_same_socket() {
    // Budget 6s against a 2s read deadline. Deleting the breaker check makes
    // the second open pay the budget and the neighbour's response miss the
    // deadline by 4s; no ordinary slowness closes a gap that size. Threshold 1
    // so the setup pays that budget once rather than three times.
    let server = TestServer::start_with_bind_timeout_and_breaker(
        Duration::from_secs(6),
        1,
        Duration::from_secs(30),
    )
    .await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let wedged_id = "fake-aft-breaker-wedged";
    let neighbour_id = "fake-aft-breaker-neighbour";
    let (wedged, wedged_events) = spawn_stub_with_events(
        &server,
        &supervisor,
        wedged_id,
        "breaker-head-of-line",
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;
    let neighbour = spawn_stub(&server, &supervisor, neighbour_id).await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let neighbour_ack = attach_on_stream(
        &mut client,
        &project,
        620,
        "ses-breaker-neighbour",
        neighbour_id,
    )
    .await;

    // One full-budget timeout opens this daemon's breaker.
    let opening = attach_error_on_stream_with_wait(
        &mut client,
        &project,
        621,
        "ses-breaker-open",
        wedged_id,
        SETUP_TIMEOUT,
    )
    .await;
    assert_eq!(opening.code, "module_timeout");
    wait_for_stub_event_count(&wedged_events, SETUP_TIMEOUT, is_attach_event, 1).await;
    let relays_before = attach_event_count(&wedged_events);

    let mut diagnostic = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let refusals_before = server_counter_key(
        &mut diagnostic,
        622,
        "route_open_refused_by_code",
        "module_timeout_breaker_open",
    )
    .await;

    // Pipelined on ONE socket: the open to the wedged module, then a call to
    // the already-bound neighbour.
    let payload = br#"{"jsonrpc":"2.0","id":1,"method":"read"}"#;
    write_frame(
        &mut client,
        &attach_frame(
            623,
            attach_request(&project, "ses-breaker-refused", wedged_id),
        ),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &data_request(
            neighbour_ack.route_channel,
            neighbour_ack.route_epoch,
            624,
            payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let first = read_frame_timeout_for(&mut client, HELD_READER_DEADLINE).await;
    let second = read_frame_timeout_for(&mut client, HELD_READER_DEADLINE).await;
    let frames = [&first, &second];
    let refusal = frames
        .iter()
        .find(|frame| frame.header.ty == FrameType::Error && frame.header.corr == 623)
        .expect("breaker refusal is returned");
    assert_eq!(
        serde_json::from_slice::<ErrorBody>(&refusal.body)
            .unwrap()
            .code,
        "module_timeout"
    );
    let response = frames
        .iter()
        .find(|frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == neighbour_ack.route_channel
                && frame.header.corr == 624
        })
        .expect("neighbour response is returned");
    assert_eq!(response.body, payload);

    assert_eq!(
        attach_event_count(&wedged_events),
        relays_before,
        "a refusal from an open breaker must not relay route.bind to the wedged module"
    );
    let refusals_after = server_counter_key(
        &mut diagnostic,
        625,
        "route_open_refused_by_code",
        "module_timeout_breaker_open",
    )
    .await;
    assert_eq!(
        refusals_after,
        refusals_before + 1,
        "the fast refusal must be counted under the breaker's own key, or an operator \
         cannot tell a module that is refusing instantly from one that is fine"
    );

    wedged.stop().await.unwrap();
    neighbour.stop().await.unwrap();
}

/// A wedged module costs the WHOLE DAEMON at most one full-budget wait per
/// cooldown, rather than one per route.open per connection. This is the
/// fleet-level property: without it, every connection that calls a wedged
/// module pays its own budget and holds its own later frames.
///
/// Four client connections, because the breaker is per target module and a
/// two-connection test could pass against a per-connection one by accident.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wedged_module_costs_at_most_one_full_budget_wait_per_cooldown_across_connections() {
    let bind_budget = Duration::from_millis(400);
    let cooldown = Duration::from_secs(4);
    let server = TestServer::start_with_bind_timeout_and_breaker(bind_budget, 3, cooldown).await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-breaker-fleet";
    let (wedged, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "breaker-fleet",
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut opener = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    for corr in 630..633 {
        let error = attach_error_on_stream_with_wait(
            &mut opener,
            &project,
            corr,
            "ses-breaker-fleet-open",
            module_id,
            SETUP_TIMEOUT,
        )
        .await;
        assert_eq!(error.code, "module_timeout");
    }
    let opened_at = Instant::now();
    wait_for_stub_event_count(&events_path, SETUP_TIMEOUT, is_attach_event, 3).await;
    assert_eq!(attach_event_count(&events_path), 3);

    let mut connections = Vec::new();
    for _ in 0..4 {
        connections.push(
            connect_authed_client(&server.connection_file_path)
                .await
                .unwrap(),
        );
    }

    // Twelve opens from four other connections while the breaker is open. Not
    // one of them may reach the module.
    let mut corr = 640;
    for round in 0..3 {
        for connection in &mut connections {
            let error = attach_error_on_stream_with_wait(
                connection,
                &project,
                corr,
                &format!("ses-breaker-fleet-{round}"),
                module_id,
                SETUP_TIMEOUT,
            )
            .await;
            assert_eq!(error.code, "module_timeout");
            corr += 1;
        }
    }
    assert_eq!(
        attach_event_count(&events_path),
        3,
        "while the breaker is open a wedged module must receive no relay at all, \
         from any connection"
    );

    // Past the cooldown, the four connections between them get exactly ONE
    // probe -- not one each.
    sleep_until(opened_at + cooldown + Duration::from_millis(200)).await;
    for connection in &mut connections {
        let error = attach_error_on_stream_with_wait(
            connection,
            &project,
            corr,
            "ses-breaker-fleet-probe",
            module_id,
            SETUP_TIMEOUT,
        )
        .await;
        assert_eq!(error.code, "module_timeout");
        corr += 1;
    }
    assert_eq!(
        attach_event_count(&events_path),
        4,
        "one cooldown must buy the module exactly one more full-budget wait, \
         across all connections"
    );

    wedged.stop().await.unwrap();
}

/// ARM 2: the breaker opens at the threshold and the next open is refused
/// WITHOUT A RELAY BEING SENT. Uses the production threshold, so the constant
/// itself is under test and not just the mechanism.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bind_relay_breaker_opens_at_the_threshold_and_refuses_before_relaying() {
    let bind_budget = Duration::from_millis(300);
    let server = TestServer::start_with_bind_timeout_and_breaker(
        bind_budget,
        3,
        // Long enough that no probe can interfere with the assertions below.
        Duration::from_secs(30),
    )
    .await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-breaker-threshold";
    let (wedged, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "breaker-threshold",
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let mut diagnostic = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();

    for corr in 650..653 {
        let error = attach_error_on_stream_with_wait(
            &mut client,
            &project,
            corr,
            "ses-breaker-threshold",
            module_id,
            SETUP_TIMEOUT,
        )
        .await;
        assert_eq!(error.code, "module_timeout");
    }
    wait_for_stub_event_count(&events_path, SETUP_TIMEOUT, is_attach_event, 3).await;
    assert_eq!(
        attach_event_count(&events_path),
        3,
        "every open below the threshold must still be relayed"
    );
    let timeouts_at_threshold = server_counter_key(
        &mut diagnostic,
        653,
        "route_open_refused_by_code",
        "module_timeout",
    )
    .await;
    assert_eq!(timeouts_at_threshold, 3);

    let refused = attach_error_on_stream_with_wait(
        &mut client,
        &project,
        654,
        "ses-breaker-threshold-refused",
        module_id,
        SETUP_TIMEOUT,
    )
    .await;
    assert_eq!(refused.code, "module_timeout");

    // Wait out more than a whole budget: a relay, had one been sent, would
    // have reached the stub and been recorded long before this returns.
    sleep(bind_budget * 2).await;
    assert_eq!(
        attach_event_count(&events_path),
        3,
        "the refusal after the threshold must not have relayed route.bind at all"
    );
    assert_eq!(
        server_counter_key(
            &mut diagnostic,
            655,
            "route_open_refused_by_code",
            "module_timeout"
        )
        .await,
        timeouts_at_threshold,
        "a fast refusal must not be counted as another budget burned"
    );
    assert_eq!(
        server_counter_key(
            &mut diagnostic,
            656,
            "route_open_refused_by_code",
            "module_timeout_breaker_open"
        )
        .await,
        1
    );

    // The operator surface: an open breaker is nameable from server.describe.
    let open_breaker = server_counter_key_value(&mut diagnostic, 657, "route_bind_breakers_open")
        .await
        .unwrap_or_else(|| panic!("server.describe must name the open breaker"));
    assert_eq!(open_breaker[module_id]["consecutive_timeouts"], 3);

    wedged.stop().await.unwrap();
}

/// ARM 3: a module that says no quickly is healthy. Rejection is a different
/// condition with its own refusal and must never trip the breaker, however
/// often it happens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_module_that_rejects_binds_quickly_never_trips_the_breaker() {
    // The budget is deliberately generous. This test is about what a REFUSAL
    // does to the breaker, and a refusal that outlasts the budget is reported
    // as `module_timeout` instead, so a short budget made the subject depend on
    // how fast a loaded runner delivered the stub's refusal (300 ms lost that
    // race on Windows CI). Whether refusals count is still decided by the
    // threshold of 3: if they counted, opens 4 to 6 would read
    // `module_timeout_breaker_open`, whatever the budget.
    let server = TestServer::start_with_bind_timeout_and_breaker(
        Duration::from_secs(10),
        3,
        Duration::from_secs(30),
    )
    .await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-breaker-rejects";
    let (rejecting, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "breaker-rejects",
        [("FAKE_AFT_REJECT_ATTACH", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let rejections = 6;
    for corr in 660..(660 + rejections) {
        let error = attach_error_on_stream_with_wait(
            &mut client,
            &project,
            corr,
            "ses-breaker-rejects",
            module_id,
            SETUP_TIMEOUT,
        )
        .await;
        assert_eq!(error.code, "config_divergence");
    }

    assert_eq!(
        attach_event_count(&events_path),
        rejections as usize,
        "every rejected open must still have been relayed: twice the threshold of \
         rejections must leave the breaker closed"
    );
    let mut diagnostic = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    assert_eq!(
        server_counter_key(
            &mut diagnostic,
            670,
            "route_open_refused_by_code",
            "module_timeout_breaker_open"
        )
        .await,
        0
    );
    assert!(
        server_counter_key_value(&mut diagnostic, 671, "route_bind_breakers_open")
            .await
            .is_none(),
        "no breaker may be open for a module that answers every bind"
    );

    rejecting.stop().await.unwrap();
}

/// ARM 4: at the cooldown boundary the breaker admits EXACTLY ONE probe, not
/// every arrival that happens to find the cooldown expired.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn half_open_admits_exactly_one_probe_under_concurrent_arrivals() {
    let bind_budget = Duration::from_millis(300);
    let cooldown = Duration::from_millis(1500);
    let server = TestServer::start_with_bind_timeout_and_breaker(bind_budget, 3, cooldown).await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-breaker-probe";
    let (wedged, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "breaker-probe",
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut opener = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    for corr in 680..683 {
        let error = attach_error_on_stream_with_wait(
            &mut opener,
            &project,
            corr,
            "ses-breaker-probe-open",
            module_id,
            SETUP_TIMEOUT,
        )
        .await;
        assert_eq!(error.code, "module_timeout");
    }
    let opened_at = Instant::now();
    wait_for_stub_event_count(&events_path, SETUP_TIMEOUT, is_attach_event, 3).await;

    // Connect and build every frame BEFORE the cooldown expires, so the four
    // arrivals really do race the boundary instead of trickling in behind each
    // other's setup cost.
    let mut racers = Vec::new();
    for (index, corr) in (690..694).enumerate() {
        let connection = connect_authed_client(&server.connection_file_path)
            .await
            .unwrap();
        let frame = attach_frame(
            corr,
            attach_request(&project, &format!("ses-breaker-race-{index}"), module_id),
        );
        racers.push((connection, frame));
    }

    sleep_until(opened_at + cooldown + Duration::from_millis(100)).await;
    let mut inflight = Vec::new();
    for (mut connection, frame) in racers {
        inflight.push(tokio::spawn(async move {
            write_frame(&mut connection, &frame).await.unwrap();
            connection.flush().await.unwrap();
            read_frame_timeout_for(&mut connection, SETUP_TIMEOUT).await
        }));
    }
    for task in inflight {
        let frame = task.await.unwrap();
        assert_eq!(frame.header.ty, FrameType::Error);
        assert_eq!(
            serde_json::from_slice::<ErrorBody>(&frame.body)
                .unwrap()
                .code,
            "module_timeout"
        );
    }

    assert_eq!(
        attach_event_count(&events_path),
        4,
        "four opens racing the cooldown boundary must yield exactly one probe"
    );

    wedged.stop().await.unwrap();
}

/// ARM 5: an accepted probe closes the breaker and binds flow normally again.
///
/// The module recovers INSIDE ONE PROCESS (`FAKE_AFT_BIND_NEVER_REPLY_FIRST`)
/// rather than by being restarted. A restart would also replace the module
/// connection, which is independently a reason the daemon discards what it
/// learned, so a restart cannot tell recovery from amnesia.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_probe_closes_the_breaker_and_binds_flow_again() {
    let bind_budget = Duration::from_millis(300);
    let cooldown = Duration::from_millis(800);
    let server = TestServer::start_with_bind_timeout_and_breaker(bind_budget, 3, cooldown).await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-breaker-recovers";
    let (recovering, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "breaker-recovers",
        [("FAKE_AFT_BIND_NEVER_REPLY_FIRST", "3")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    for corr in 700..703 {
        let error = attach_error_on_stream_with_wait(
            &mut client,
            &project,
            corr,
            "ses-breaker-recover-open",
            module_id,
            SETUP_TIMEOUT,
        )
        .await;
        assert_eq!(error.code, "module_timeout");
    }
    let opened_at = Instant::now();
    wait_for_stub_event_count(&events_path, SETUP_TIMEOUT, is_attach_event, 3).await;

    let mut diagnostic = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let refusals_before = server_counter_key(
        &mut diagnostic,
        703,
        "route_open_refused_by_code",
        "module_timeout_breaker_open",
    )
    .await;
    assert!(
        server_counter_key_value(&mut diagnostic, 704, "route_bind_breakers_open")
            .await
            .is_some(),
        "the breaker must be open before recovery is claimed"
    );

    sleep_until(opened_at + cooldown + Duration::from_millis(100)).await;
    let probe = attach_on_stream(
        &mut client,
        &project,
        705,
        "ses-breaker-probe-ok",
        module_id,
    )
    .await;
    assert!(probe.route_channel > 0);

    let after = attach_on_stream(&mut client, &project, 706, "ses-breaker-closed", module_id).await;
    assert!(after.route_channel > 0);

    assert_eq!(
        attach_event_count(&events_path),
        5,
        "once the probe is accepted the breaker must be closed, so the next open \
         relays normally instead of being refused"
    );
    assert_eq!(
        server_counter_key(
            &mut diagnostic,
            707,
            "route_open_refused_by_code",
            "module_timeout_breaker_open"
        )
        .await,
        refusals_before,
        "no open after the accepted probe may be fast-refused"
    );
    assert!(
        server_counter_key_value(&mut diagnostic, 708, "route_bind_breakers_open")
            .await
            .is_none(),
        "a closed breaker must disappear from the operator surface"
    );

    recovering.stop().await.unwrap();
}

/// A BREAKER IS A CACHED VERDICT ABOUT A PROCESS, NOT ABOUT A NAME. When a new
/// module connection registers under the id, the process the verdict describes
/// is gone, so the verdict goes with it: the replacement is not made to serve
/// its predecessor's cooldown.
///
/// The cooldown here is 30s against a test that finishes in seconds, so the
/// open after the respawn can only be relayed if the registration discarded the
/// state -- waiting it out is not available to this test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_module_connection_discards_the_breaker_state_of_the_process_it_replaced() {
    let server = TestServer::start_with_bind_timeout_and_breaker(
        Duration::from_millis(300),
        3,
        Duration::from_secs(30),
    )
    .await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-breaker-respawn";
    let (wedged, _wedged_events) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "breaker-respawn-wedged",
        [("FAKE_AFT_BIND_NEVER_REPLY", "1")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    for corr in 720..723 {
        let error = attach_error_on_stream_with_wait(
            &mut client,
            &project,
            corr,
            "ses-breaker-respawn-open",
            module_id,
            SETUP_TIMEOUT,
        )
        .await;
        assert_eq!(error.code, "module_timeout");
    }
    let mut diagnostic = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    assert!(
        server_counter_key_value(&mut diagnostic, 723, "route_bind_breakers_open")
            .await
            .is_some(),
        "the breaker must be open before the respawn, or this proves nothing"
    );

    wedged.stop().await.unwrap();
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;
    let healthy = spawn_stub(&server, &supervisor, module_id).await;

    let ack = attach_on_stream(
        &mut client,
        &project,
        724,
        "ses-breaker-respawn-healthy",
        module_id,
    )
    .await;
    assert!(ack.route_channel > 0);
    assert!(
        server_counter_key_value(&mut diagnostic, 725, "route_bind_breakers_open")
            .await
            .is_none(),
        "a respawned module must not inherit the open breaker of the process it replaced"
    );

    healthy.stop().await.unwrap();
}

fn is_attach_event(event: &Value) -> bool {
    event["kind"] == "attach"
}

/// How many route.bind relays the stub has actually received. The structural
/// witness for "no relay was sent", which a latency bound cannot give.
fn attach_event_count(path: &Path) -> usize {
    stub_events(path)
        .iter()
        .filter(|event| is_attach_event(event))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_route_bind_reply_settles_and_later_open_succeeds() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-bind-malformed";
    let malformed = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_MALFORMED_BIND_REPLY", "response")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let error =
        attach_error_on_stream(&mut client, &project, 461, "ses-bind-malformed", module_id).await;
    assert_eq!(error.code, "target_unavailable");
    assert!(
        error.message.contains("malformed route.bind response body"),
        "unexpected malformed route.bind error: {error:?}"
    );
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;

    malformed.stop().await.unwrap();
    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;
    let healthy = spawn_stub(&server, &supervisor, module_id).await;
    let ack = attach_on_stream(
        &mut client,
        &project,
        462,
        "ses-bind-malformed-healthy",
        module_id,
    )
    .await;
    assert!(ack.route_channel > 0);
    wait_for_binding_count(&server.forwarding, 1, SETUP_TIMEOUT).await;

    healthy.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_delivery_failure_closes_dead_client_without_erroring_module_or_cotenant() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-dead-client";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "dead-client",
        [("FAKE_AFT_CONCURRENCY", "stateless_parallel")],
    )
    .await;

    let project = TestProject::new();
    let (first, first_ack) = attach_client(&server, &project, 471, "ses-dead-client-a").await;
    let (mut second, second_ack) = attach_client(&server, &project, 472, "ses-dead-client-b").await;
    wait_for_binding_count(&server.forwarding, 2, SETUP_TIMEOUT).await;

    // Make the client's read half dead so module->client delivery fails, but keep
    // its write half open so subc keeps reading its requests (this exercises the
    // module-delivery-failure close path, not the read-EOF teardown path).
    let first_std = first.into_std().unwrap();
    first_std.shutdown(Shutdown::Read).unwrap();
    let first = TcpStream::from_std(first_std).unwrap();
    // Flood far more responses than the 64-slot client egress can hold so the
    // module->client try_send deterministically hits Full and triggers the
    // dead-client close on EVERY platform. A small burst only worked on
    // macOS/Windows (where Shutdown::Read also surfaces as a write error/Closed);
    // on Linux a half-read-dead peer neither errors the write nor fills a 64-slot
    // queue with too few frames, so the close never fired. stateless_parallel
    // lifts the admission window so all flooded requests are in flight at once.
    let dead_payload = vec![b'x'; 512 * 1024];
    let flood = tokio::spawn(async move {
        let mut first = first;
        for offset in 0..256_u64 {
            if write_frame(
                &mut first,
                &data_request(
                    first_ack.route_channel,
                    first_ack.route_epoch,
                    473 + offset,
                    &dead_payload,
                ),
            )
            .await
            .is_err()
            {
                break;
            }
            if first.flush().await.is_err() {
                break;
            }
        }
    });

    wait_for_binding_count(&server.forwarding, 1, SETUP_TIMEOUT).await;
    flood.abort();

    let payload = br#"{"jsonrpc":"2.0","id":"cotenant"}"#;
    write_frame(
        &mut second,
        &data_request(
            second_ack.route_channel,
            second_ack.route_epoch,
            490,
            payload,
        ),
    )
    .await
    .unwrap();
    second.flush().await.unwrap();
    let response = read_frame_timeout_for(&mut second, SETUP_TIMEOUT).await;
    assert_response(&response, second_ack.route_channel, 490, payload);

    let errors: Vec<_> = stub_events(&events_path)
        .into_iter()
        .filter(|event| event.get("kind").and_then(Value::as_str) == Some("error"))
        .collect();
    assert!(
        errors.is_empty(),
        "unexpected module ERROR frames after dead-client delivery failure: {errors:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backpressured_client_does_not_hol_block_cotenant_and_is_cleaned_up() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-hol-backpressure";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_CONCURRENCY", "stateless_parallel")],
    )
    .await;

    let project = TestProject::new();
    let (mut clogged, clogged_ack) = attach_client(&server, &project, 481, "ses-hol-a").await;
    let (mut cotenant, cotenant_ack) = attach_client(&server, &project, 482, "ses-hol-b").await;
    wait_for_binding_count(&server.forwarding, 2, SETUP_TIMEOUT).await;

    let flood_payload = vec![b'z'; 512 * 1024];
    let flood = tokio::spawn(async move {
        for offset in 0..256_u64 {
            if write_frame(
                &mut clogged,
                &data_request(
                    clogged_ack.route_channel,
                    clogged_ack.route_epoch,
                    20_000 + offset,
                    &flood_payload,
                ),
            )
            .await
            .is_err()
            {
                break;
            }
            if clogged.flush().await.is_err() {
                break;
            }
        }
        clogged
    });

    let cotenant_payload = br#"{"jsonrpc":"2.0","id":"not-blocked"}"#;
    for offset in 0..4_u64 {
        write_frame(
            &mut cotenant,
            &data_request(
                cotenant_ack.route_channel,
                cotenant_ack.route_epoch,
                21_000 + offset,
                cotenant_payload,
            ),
        )
        .await
        .unwrap();
        cotenant.flush().await.unwrap();
        let response = read_frame_timeout_for(&mut cotenant, SETUP_TIMEOUT).await;
        assert_response(
            &response,
            cotenant_ack.route_channel,
            21_000 + offset,
            cotenant_payload,
        );
    }

    wait_for_binding_count(&server.forwarding, 1, SETUP_TIMEOUT).await;
    flood.abort();

    let payload = br#"{"jsonrpc":"2.0","id":"survives"}"#;
    write_frame(
        &mut cotenant,
        &data_request(
            cotenant_ack.route_channel,
            cotenant_ack.route_epoch,
            21_100,
            payload,
        ),
    )
    .await
    .unwrap();
    cotenant.flush().await.unwrap();
    let response = read_frame_timeout_for(&mut cotenant, SETUP_TIMEOUT).await;
    assert_response(&response, cotenant_ack.route_channel, 21_100, payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_attach_same_module_and_round_trip_independently() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-two-clients";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let project = TestProject::new();
    let (mut first, first_ack) = attach_client(&server, &project, 501, "ses-one").await;
    let (mut second, second_ack) = attach_client(&server, &project, 502, "ses-two").await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 2);

    let first_payload = br#"{"jsonrpc":"2.0","id":"first"}"#;
    let second_payload = br#"{"jsonrpc":"2.0","id":"second"}"#;
    write_frame(
        &mut first,
        &data_request(
            first_ack.route_channel,
            first_ack.route_epoch,
            503,
            first_payload,
        ),
    )
    .await
    .unwrap();
    write_frame(
        &mut second,
        &data_request(
            second_ack.route_channel,
            second_ack.route_epoch,
            504,
            second_payload,
        ),
    )
    .await
    .unwrap();
    first.flush().await.unwrap();
    second.flush().await.unwrap();

    let first_response = read_frame_timeout(&mut first).await;
    let second_response = read_frame_timeout(&mut second).await;
    assert_eq!(first_response.header.ty, FrameType::Response);
    assert_eq!(first_response.header.channel, first_ack.route_channel);
    assert_eq!(first_response.header.corr, 503);
    assert_eq!(first_response.body, first_payload);
    assert_eq!(second_response.header.ty, FrameType::Response);
    assert_eq!(second_response.header.channel, second_ack.route_channel);
    assert_eq!(second_response.header.corr, 504);
    assert_eq!(second_response.body, second_payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_session_slow_call_does_not_block_fast_call() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-cross-session-delay";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_DELAY_FROM_BODY", "1")],
    )
    .await;

    let project = TestProject::new();
    let (mut slow_client, slow_ack) = attach_client(&server, &project, 525, "ses-cross-slow").await;
    let (mut fast_client, fast_ack) = attach_client(&server, &project, 526, "ses-cross-fast").await;
    let slow_payload = br#"{"delay_ms":500,"jsonrpc":"2.0","id":"slow"}"#;
    let fast_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"fast"}"#;
    write_frame(
        &mut slow_client,
        &data_request(
            slow_ack.route_channel,
            slow_ack.route_epoch,
            527,
            slow_payload,
        ),
    )
    .await
    .unwrap();
    slow_client.flush().await.unwrap();
    let slow_sent = Instant::now();

    write_frame(
        &mut fast_client,
        &data_request(
            fast_ack.route_channel,
            fast_ack.route_epoch,
            528,
            fast_payload,
        ),
    )
    .await
    .unwrap();
    fast_client.flush().await.unwrap();
    let fast_sent = Instant::now();

    let ((slow_received_at, slow_response), (fast_received_at, fast_response)) =
        timeout(Duration::from_secs(2), async {
            tokio::join!(
                async {
                    let response = read_frame_timeout(&mut slow_client).await;
                    (Instant::now(), response)
                },
                async {
                    let response = read_frame_timeout(&mut fast_client).await;
                    (Instant::now(), response)
                }
            )
        })
        .await
        .expect("timed out waiting for cross-session responses");

    assert_response(&slow_response, slow_ack.route_channel, 527, slow_payload);
    assert_response(&fast_response, fast_ack.route_channel, 528, fast_payload);

    let slow_latency = slow_received_at.duration_since(slow_sent);
    let fast_latency = fast_received_at.duration_since(fast_sent);
    eprintln!("cross-session latencies: fast={fast_latency:?}, slow={slow_latency:?}");
    // Concurrency is proven structurally: the fast call arrives BEFORE the slow
    // one even though the fast request was sent second, AND the slow call still
    // took its full ≥450ms delay. If the fast call were serialized behind the
    // slow one it would arrive after it (failing the assert below). An absolute
    // fast-latency bound was removed: it added nothing over the ordering check
    // and was the only part fragile to scheduling load on a busy CI runner.
    assert!(
        fast_received_at < slow_received_at,
        "fast response should arrive before slow: fast_latency={fast_latency:?}, slow_latency={slow_latency:?}"
    );
    assert!(
        slow_latency >= Duration::from_millis(450),
        "slow call did not exercise the requested 500ms delay: slow_latency={slow_latency:?}, fast_latency={fast_latency:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_channel_responses_return_out_of_order_by_corr() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-same-channel-oood";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "same-channel-oood",
        [("FAKE_AFT_DELAY_FROM_BODY", "1")],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 631, "ses-same-channel-oood").await;

    const CA: u64 = 632;
    const CB: u64 = 633;
    // A's delay is the whole margin of this test: B can only lose the race if
    // its arrival at the stub lags A's by more than this. The stub appends a
    // request-received event to disk before dispatching each frame, and a
    // hosted Windows runner has stalled that append past 300 ms (release
    // verify, 2026-09-12: A answered first). Three seconds is beyond any
    // scheduler or disk stall a runner shows while still failing fast if B
    // were truly serialized behind A.
    let payload_a = br#"{"delay_ms":3000,"jsonrpc":"2.0","id":"req-a"}"#;
    let payload_b = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"req-b"}"#;

    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, CA, payload_a),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, CB, payload_b),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let sent = Instant::now();

    let first_response = read_frame_timeout(&mut client).await;
    let first_received_at = Instant::now();
    // A is the 3 s request; the default 2 s read budget would time out on it.
    let second_response = read_frame_timeout_for(&mut client, Duration::from_secs(10)).await;
    let second_received_at = Instant::now();

    assert_response(&first_response, ack.route_channel, CB, payload_b);
    assert_response(&second_response, ack.route_channel, CA, payload_a);

    let b_latency = first_received_at.duration_since(sent);
    let a_latency = second_received_at.duration_since(sent);
    eprintln!(
        "same-channel out-of-order latencies: B(corr={CB})={b_latency:?}, A(corr={CA})={a_latency:?}"
    );
    // Out-of-order completion is proven structurally: B (corr=CB, sent second,
    // 0ms delay) arrives BEFORE A (corr=CA, sent first, 3 s delay), and A
    // still takes its full delay. If B were serialized behind A it would arrive
    // after it (failing the ordering assert). The absolute b_latency<80ms bound
    // was a perf claim that flaked on slow CI runners — removed; latencies logged.
    assert!(
        first_received_at < second_received_at,
        "B should arrive before A: b_latency={b_latency:?}, a_latency={a_latency:?}"
    );
    assert!(
        a_latency >= Duration::from_millis(2500),
        "slow request A should reflect its 3 s delay: a_latency={a_latency:?}"
    );

    let events = stub_events(&events_path);
    let a_recv_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, CA)
    });
    let b_recv_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, CB)
    });
    let a_terminal_pos = event_position(&events, |event| {
        event_is_terminal(event, "response", ack.route_channel, CA)
    });
    assert!(
        a_recv_pos < a_terminal_pos && b_recv_pos < a_terminal_pos,
        "A and B should both be in-flight at the stub before A's terminal: {events:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_before_response_for_cancellable_request_returns_cancelled_error() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-cancel-before";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "cancel-before",
        [("FAKE_AFT_DELAY_FROM_BODY", "1")],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 801, "ses-cancel-before").await;
    let corr = 802;
    let payload = br#"{"delay_ms":500,"jsonrpc":"2.0","id":"cancel-before"}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, corr, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let cancel_sent = Instant::now();
    write_frame(
        &mut client,
        &cancel_frame(ack.route_channel, ack.route_epoch, corr),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let terminal = read_frame_timeout(&mut client).await;
    let cancel_latency = Instant::now().duration_since(cancel_sent);
    eprintln!("cancel-before-response latency: {cancel_latency:?}");
    let body = assert_error(&terminal, ack.route_channel, corr, "cancelled");
    assert!(body.message.contains("cancelled"));
    // Correctness = the terminal is a cancelled ERROR (asserted above). The
    // previous absolute "<250ms, before the 500ms delay" bound was a perf claim
    // that flaked on slow CI runners; the cancel arriving as the terminal (not a
    // late normal Response after the full delay) is the real signal. Logged only.

    let cancel_event = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(true)
    })
    .await;
    assert_eq!(cancel_event["claimed"].as_bool(), Some(true));
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "terminal"
            && event["terminal"] == "error"
            && event["code"] == "cancelled"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
    })
    .await;
    assert_no_frame_within(&mut client, Duration::from_millis(550)).await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_after_response_is_idempotent_noop() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-cancel-after";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "cancel-after",
        [("FAKE_AFT_DELAY_FROM_BODY", "1")],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 811, "ses-cancel-after").await;
    let corr = 812;
    let payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"cancel-after"}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, corr, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_response(&response, ack.route_channel, corr, payload);

    write_frame(
        &mut client,
        &cancel_frame(ack.route_channel, ack.route_epoch, corr),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let cancel_event = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(false)
    })
    .await;
    assert_eq!(cancel_event["claimed"].as_bool(), Some(false));
    assert_no_frame_within(&mut client, Duration::from_millis(100)).await;

    let followup_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"cancel-after-followup"}"#;
    write_frame(
        &mut client,
        &data_request(
            ack.route_channel,
            ack.route_epoch,
            corr + 1,
            followup_payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let followup = read_frame_timeout(&mut client).await;
    assert_response(&followup, ack.route_channel, corr + 1, followup_payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_cancel_emits_exactly_one_cancelled_error() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-double-cancel";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "double-cancel",
        [("FAKE_AFT_DELAY_FROM_BODY", "1")],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 821, "ses-double-cancel").await;
    let corr = 822;
    let payload = br#"{"delay_ms":500,"jsonrpc":"2.0","id":"double-cancel"}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, corr, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    write_frame(
        &mut client,
        &cancel_frame(ack.route_channel, ack.route_epoch, corr),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &cancel_frame(ack.route_channel, ack.route_epoch, corr),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let terminal = read_frame_timeout(&mut client).await;
    assert_error(&terminal, ack.route_channel, corr, "cancelled");
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(true)
    })
    .await;
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(false)
    })
    .await;
    assert_no_frame_within(&mut client, Duration::from_millis(550)).await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_for_uncancellable_delayed_request_allows_normal_response() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-uncancellable";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "uncancellable",
        [("FAKE_AFT_DELAY_FROM_BODY", "1")],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 831, "ses-uncancellable").await;
    let corr = 832;
    let payload = br#"{"delay_ms":200,"uncancellable":true,"jsonrpc":"2.0","id":"uncancellable"}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, corr, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let cancel_sent = Instant::now();
    write_frame(
        &mut client,
        &cancel_frame(ack.route_channel, ack.route_epoch, corr),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let cancel_event = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(false)
    })
    .await;
    assert_eq!(cancel_event["claimed"].as_bool(), Some(false));

    let response = read_frame_timeout(&mut client).await;
    let latency = Instant::now().duration_since(cancel_sent);
    assert_response(&response, ack.route_channel, corr, payload);
    assert!(
        latency >= Duration::from_millis(150),
        "uncancellable request should complete after its configured delay; latency={latency:?}"
    );
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "terminal"
            && event["terminal"] == "response"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
    })
    .await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_channel_cancel_drops_silently_and_connection_survives() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-unknown-cancel";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 841, "ses-unknown-cancel").await;
    let unknown_channel = if ack.route_channel == u16::MAX {
        u16::MAX - 1
    } else {
        u16::MAX
    };
    assert_ne!(unknown_channel, ack.route_channel);

    let unknown_corr = 842;
    write_frame(&mut client, &cancel_frame(unknown_channel, 1, unknown_corr))
        .await
        .unwrap();
    client.flush().await.unwrap();
    assert_no_frame_within(&mut client, Duration::from_millis(100)).await;

    let payload = br#"{"jsonrpc":"2.0","id":"after-unknown-cancel"}"#;
    write_frame(
        &mut client,
        &data_request(
            ack.route_channel,
            ack.route_epoch,
            unknown_corr + 1,
            payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_response(&response, ack.route_channel, unknown_corr + 1, payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_client_fanout_pushes_route_to_each_bound_client() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-push-fanout";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_FANOUT_ON_REQUEST", "1")],
    )
    .await;

    let project = TestProject::new();
    let (mut first, first_ack) = attach_client(&server, &project, 551, "ses-fanout-one").await;
    let (mut second, second_ack) = attach_client(&server, &project, 552, "ses-fanout-two").await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 2);

    let first_payload = br#"{"jsonrpc":"2.0","id":"fanout-trigger"}"#;
    write_frame(
        &mut first,
        &data_request(
            first_ack.route_channel,
            first_ack.route_epoch,
            553,
            first_payload,
        ),
    )
    .await
    .unwrap();
    first.flush().await.unwrap();

    let (first_push, _first_response) =
        read_until_push_and_response(&mut first, first_ack.route_channel, 553, first_payload).await;
    let second_push = read_push(&mut second, second_ack.route_channel).await;
    assert_eq!(first_push.header.channel, first_ack.route_channel);
    assert_eq!(second_push.header.channel, second_ack.route_channel);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_client_pipelined_requests_preserve_corr_fifo_order() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-fifo";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 601, "ses-fifo").await;
    let request_count = 8u64;
    for corr in 1..=request_count {
        let body = format!(r#"{{"jsonrpc":"2.0","id":{corr}}}"#);
        write_frame(
            &mut client,
            &data_request(ack.route_channel, ack.route_epoch, corr, body.as_bytes()),
        )
        .await
        .unwrap();
    }
    client.flush().await.unwrap();

    let mut response_corrs = Vec::new();
    for expected_corr in 1..=request_count {
        let response = read_frame_timeout(&mut client).await;
        assert_eq!(response.header.ty, FrameType::Response);
        assert_eq!(response.header.channel, ack.route_channel);
        assert_eq!(
            response.body,
            format!(r#"{{"jsonrpc":"2.0","id":{expected_corr}}}"#).as_bytes()
        );
        response_corrs.push(response.header.corr);
    }
    assert_eq!(response_corrs, (1..=request_count).collect::<Vec<_>>());

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serial_flow_control_window_holds_second_request_until_terminal() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-serial-flow";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "serial-flow",
        [
            ("FAKE_AFT_CONCURRENCY", "serial"),
            ("FAKE_AFT_DELAY_FROM_BODY", "1"),
        ],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 901, "ses-serial-flow").await;
    let first_corr = 902;
    let second_corr = 903;
    let first_payload = br#"{"delay_ms":300,"jsonrpc":"2.0","id":"serial-1"}"#;
    let second_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"serial-2"}"#;

    write_frame(
        &mut client,
        &data_request(
            ack.route_channel,
            ack.route_epoch,
            first_corr,
            first_payload,
        ),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &data_request(
            ack.route_channel,
            ack.route_epoch,
            second_corr,
            second_payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, first_corr)
    })
    .await;
    assert_no_stub_event_within(&events_path, Duration::from_millis(100), |event| {
        event_is_request_received(event, ack.route_channel, second_corr)
    })
    .await;
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_terminal(event, "response", ack.route_channel, first_corr)
    })
    .await;
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, second_corr)
    })
    .await;

    let first_response = read_frame_timeout(&mut client).await;
    assert_response(
        &first_response,
        ack.route_channel,
        first_corr,
        first_payload,
    );
    let second_response = read_frame_timeout(&mut client).await;
    assert_response(
        &second_response,
        ack.route_channel,
        second_corr,
        second_payload,
    );

    let events = stub_events(&events_path);
    let first_request_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, first_corr)
    });
    let first_terminal_pos = event_position(&events, |event| {
        event_is_terminal(event, "response", ack.route_channel, first_corr)
    });
    let second_request_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, second_corr)
    });
    assert!(
        first_request_pos < first_terminal_pos && first_terminal_pos < second_request_pos,
        "serial flow-control ordering violated: {events:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_bypasses_full_flow_control_window_and_credit_frees_on_terminal() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-cancel-flow";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "cancel-flow",
        [
            ("FAKE_AFT_CONCURRENCY", "serial"),
            ("FAKE_AFT_DELAY_FROM_BODY", "1"),
        ],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 911, "ses-cancel-flow").await;
    let cancelled_corr = 912;
    let followup_corr = 913;
    let cancellable_payload = br#"{"delay_ms":500,"jsonrpc":"2.0","id":"cancel-flow"}"#;
    let followup_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"after-cancel-flow"}"#;

    write_frame(
        &mut client,
        &data_request(
            ack.route_channel,
            ack.route_epoch,
            cancelled_corr,
            cancellable_payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, cancelled_corr)
    })
    .await;
    sleep(Duration::from_millis(20)).await;

    write_frame(
        &mut client,
        &cancel_frame(ack.route_channel, ack.route_epoch, cancelled_corr),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &data_request(
            ack.route_channel,
            ack.route_epoch,
            followup_corr,
            followup_payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    // "Cancel bypasses the full flow-control window" is proven structurally: the
    // cancelled ERROR is delivered AND the follow-up Response then arrives (the
    // cancel freed the window credit for it). The previous absolute "<250ms"
    // bound was a perf claim that flaked on slow CI runners — removed.
    let cancelled = read_frame_timeout(&mut client).await;
    assert_error(&cancelled, ack.route_channel, cancelled_corr, "cancelled");
    let followup = read_frame_timeout(&mut client).await;
    assert_response(
        &followup,
        ack.route_channel,
        followup_corr,
        followup_payload,
    );

    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_terminal(event, "error", ack.route_channel, cancelled_corr)
            && event["code"] == "cancelled"
    })
    .await;
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, followup_corr)
    })
    .await;
    let events = stub_events(&events_path);
    let cancelled_terminal_pos = event_position(&events, |event| {
        event_is_terminal(event, "error", ack.route_channel, cancelled_corr)
    });
    let followup_request_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, followup_corr)
    });
    assert!(
        cancelled_terminal_pos < followup_request_pos,
        "follow-up request should wait for cancelled terminal credit release: {events:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flow_control_over_release_guard_does_not_grow_serial_window() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-over-release";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "over-release",
        [
            ("FAKE_AFT_CONCURRENCY", "serial"),
            ("FAKE_AFT_DELAY_FROM_BODY", "1"),
            ("FAKE_AFT_DOUBLE_TERMINAL", "1"),
        ],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 921, "ses-over-release").await;
    let warmup_corr = 922;
    let warmup_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"warmup"}"#;
    write_frame(
        &mut client,
        &data_request(
            ack.route_channel,
            ack.route_epoch,
            warmup_corr,
            warmup_payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let warmup_first = read_frame_timeout(&mut client).await;
    assert_response(
        &warmup_first,
        ack.route_channel,
        warmup_corr,
        warmup_payload,
    );
    let warmup_duplicate = read_frame_timeout(&mut client).await;
    assert_response(
        &warmup_duplicate,
        ack.route_channel,
        warmup_corr,
        warmup_payload,
    );

    let slow_corr = 923;
    let fast_corr = 924;
    let slow_payload = br#"{"delay_ms":300,"jsonrpc":"2.0","id":"over-release-slow"}"#;
    let fast_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"over-release-fast"}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, slow_corr, slow_payload),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &data_request(ack.route_channel, ack.route_epoch, fast_corr, fast_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, slow_corr)
    })
    .await;
    assert_no_stub_event_within(&events_path, Duration::from_millis(100), |event| {
        event_is_request_received(event, ack.route_channel, fast_corr)
    })
    .await;
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_terminal(event, "response", ack.route_channel, slow_corr)
    })
    .await;
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, fast_corr)
    })
    .await;

    let events = stub_events(&events_path);
    let slow_terminal_pos = event_position(&events, |event| {
        event_is_terminal(event, "response", ack.route_channel, slow_corr)
    });
    let fast_request_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, fast_corr)
    });
    assert!(
        slow_terminal_pos < fast_request_pos,
        "double terminal grew serial window beyond one credit: {events:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_flow_control_acquire_wakes_when_module_tears_down() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-flow-teardown";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "flow-teardown",
        [
            ("FAKE_AFT_CONCURRENCY", "serial"),
            ("FAKE_AFT_DELAY_FROM_BODY", "1"),
        ],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 931, "ses-flow-teardown").await;
    let inflight_corr = 932;
    let blocked_corr = 933;
    let inflight_payload = br#"{"delay_ms":5000,"jsonrpc":"2.0","id":"inflight"}"#;
    let blocked_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"blocked"}"#;

    write_frame(
        &mut client,
        &data_request(
            ack.route_channel,
            ack.route_epoch,
            inflight_corr,
            inflight_payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, ack.route_channel, inflight_corr)
    })
    .await;

    write_frame(
        &mut client,
        &data_request(
            ack.route_channel,
            ack.route_epoch,
            blocked_corr,
            blocked_payload,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    sleep(Duration::from_millis(50)).await;
    assert_no_stub_event_within(&events_path, Duration::from_millis(100), |event| {
        event_is_request_received(event, ack.route_channel, blocked_corr)
    })
    .await;

    module.stop().await.unwrap();
    let outcome = read_frame_or_close_timeout(&mut client, Duration::from_secs(2)).await;
    if let Some(frame) = outcome {
        if frame.header.ty == FrameType::Push {
            assert_route_lifecycle_push(
                &frame,
                "route.closed",
                module_id,
                "crash",
                Some(false),
                Some(0),
                Some(false),
            );
            let terminal = read_frame_or_close_timeout(&mut client, Duration::from_secs(2)).await;
            if let Some(terminal) = terminal {
                if terminal.header.ty == FrameType::Goodbye {
                    assert_eq!(terminal.header.channel, ack.route_channel);
                } else {
                    assert_error(&terminal, ack.route_channel, blocked_corr, "backend_error");
                }
            }
        } else if frame.header.ty == FrameType::Goodbye {
            assert_eq!(frame.header.channel, ack.route_channel);
        } else {
            assert_error(&frame, ack.route_channel, blocked_corr, "backend_error");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_restart_invalidates_old_generation_route_and_fresh_attach_succeeds() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 4, Duration::from_millis(20));
    let module_id = "fake-aft-generation";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_CRASH_AFTER_MS", "1500")],
    )
    .await;

    let project = TestProject::new();
    let (mut client, old_ack) = attach_client(&server, &project, 701, "ses-old-generation").await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);

    let status = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.restart_count >= 1 && status.state == ModuleState::Running && status.live
    })
    .await;
    assert!(status.restart_count >= 1);
    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    let closed = read_frame_timeout(&mut client).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_id,
        "crash",
        Some(false),
        Some(0),
        Some(false),
    );
    let goodbye = read_frame_timeout(&mut client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, old_ack.route_channel);

    let stale_request = data_request(
        old_ack.route_channel,
        old_ack.route_epoch,
        702,
        br#"{"jsonrpc":"2.0","id":"old"}"#,
    );
    write_frame(&mut client, &stale_request).await.unwrap();
    client.flush().await.unwrap();
    let stale_error = read_frame_timeout(&mut client).await;
    assert_eq!(stale_error.header.ty, FrameType::Error);
    assert_eq!(stale_error.header.channel, old_ack.route_channel);
    let body: ErrorBody = serde_json::from_slice(&stale_error.body).unwrap();
    assert!(matches!(
        body.code.as_str(),
        "unknown_channel" | "target_unavailable"
    ));
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 0);

    let fresh_ack =
        attach_on_stream(&mut client, &project, 703, "ses-new-generation", module_id).await;
    assert!(fresh_ack.route_channel > 0);
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);
    let payload = br#"{"jsonrpc":"2.0","id":"new"}"#;
    write_frame(
        &mut client,
        &data_request(fresh_ack.route_channel, fresh_ack.route_epoch, 704, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, fresh_ack.route_channel);
    assert_eq!(response.header.corr, 704);
    assert_eq!(response.body, payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_one_client_two_providers_rewrites_independent_channel_spaces() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_a = "fake-aft-mp-one-client-a";
    let module_b = "fake-aft-mp-one-client-b";
    let (provider_a, events_a) =
        spawn_stub_with_events_path(&server, &supervisor, module_a, "mp-one-client-a").await;
    let (provider_b, events_b) =
        spawn_stub_with_events_path(&server, &supervisor, module_b, "mp-one-client-b").await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack_a = attach_on_stream(&mut client, &project, 1001, "ses-mp-a", module_a).await;
    let ack_b = attach_on_stream(&mut client, &project, 1002, "ses-mp-b", module_b).await;
    assert_ne!(ack_a.route_channel, ack_b.route_channel);
    let attach_a = wait_for_stub_event(&events_a, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-mp-a"
    })
    .await;
    let attach_b = wait_for_stub_event(&events_b, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-mp-b"
    })
    .await;
    assert_eq!(attach_a["route_channel"].as_u64(), Some(1));
    assert_eq!(attach_b["route_channel"].as_u64(), Some(1));

    let payload_a = br#"{"jsonrpc":"2.0","id":"provider-a"}"#;
    let payload_b = br#"{"jsonrpc":"2.0","id":"provider-b"}"#;
    write_frame(
        &mut client,
        &data_request(ack_a.route_channel, ack_a.route_epoch, 1003, payload_a),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &data_request(ack_b.route_channel, ack_b.route_epoch, 1004, payload_b),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let first = read_frame_timeout(&mut client).await;
    let second = read_frame_timeout(&mut client).await;
    let responses = [&first, &second];
    assert!(responses
        .iter()
        .any(|frame| frame.header.channel == ack_a.route_channel
            && frame.header.corr == 1003
            && frame.body == payload_a));
    assert!(responses
        .iter()
        .any(|frame| frame.header.channel == ack_b.route_channel
            && frame.header.corr == 1004
            && frame.body == payload_b));

    provider_a.stop().await.unwrap();
    provider_b.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_two_clients_one_provider_get_distinct_module_channels() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-mp-two-clients";
    let (module, events_path) =
        spawn_stub_with_events_path(&server, &supervisor, module_id, "mp-two-clients").await;

    let project = TestProject::new();
    let (mut first, first_ack) = attach_client(&server, &project, 1011, "ses-mp-two-one").await;
    let (mut second, second_ack) = attach_client(&server, &project, 1012, "ses-mp-two-two").await;
    let first_attach = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-mp-two-one"
    })
    .await;
    let second_attach = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-mp-two-two"
    })
    .await;
    let first_module_channel = first_attach["route_channel"].as_u64().unwrap();
    let second_module_channel = second_attach["route_channel"].as_u64().unwrap();
    assert_ne!(first_module_channel, second_module_channel);

    let first_payload = br#"{"jsonrpc":"2.0","id":"first-client"}"#;
    let second_payload = br#"{"jsonrpc":"2.0","id":"second-client"}"#;
    write_frame(
        &mut first,
        &data_request(
            first_ack.route_channel,
            first_ack.route_epoch,
            1013,
            first_payload,
        ),
    )
    .await
    .unwrap();
    write_frame(
        &mut second,
        &data_request(
            second_ack.route_channel,
            second_ack.route_epoch,
            1014,
            second_payload,
        ),
    )
    .await
    .unwrap();
    first.flush().await.unwrap();
    second.flush().await.unwrap();
    assert_response(
        &read_frame_timeout(&mut first).await,
        first_ack.route_channel,
        1013,
        first_payload,
    );
    assert_response(
        &read_frame_timeout(&mut second).await,
        second_ack.route_channel,
        1014,
        second_payload,
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_status_cache_translates_same_module_channel_to_client_routes() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_a = "fake-aft-mp-status-a";
    let module_b = "fake-aft-mp-status-b";
    let (provider_a, events_a) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_a,
        "mp-status-a",
        [("FAKE_AFT_STATUS", "status-a")],
    )
    .await;
    let (provider_b, events_b) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_b,
        "mp-status-b",
        [("FAKE_AFT_STATUS", "status-b")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack_a = attach_on_stream(&mut client, &project, 1021, "ses-status-a", module_a).await;
    let ack_b = attach_on_stream(&mut client, &project, 1022, "ses-status-b", module_b).await;
    wait_for_stub_event(&events_a, SETUP_TIMEOUT, |event| {
        event["kind"] == "status_published" && event["route_channel"].as_u64() == Some(1)
    })
    .await;
    wait_for_stub_event(&events_b, SETUP_TIMEOUT, |event| {
        event["kind"] == "status_published" && event["route_channel"].as_u64() == Some(1)
    })
    .await;

    wait_for_cached_status(&mut client, ack_a.route_channel, "status-a", 1023).await;
    wait_for_cached_status(&mut client, ack_b.route_channel, "status-b", 1100).await;

    provider_a.stop().await.unwrap();
    provider_b.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_cancel_rewrites_divergent_client_and_module_channels() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-mp-cancel";
    let (module, events_path) = spawn_stub_with_events(
        &server,
        &supervisor,
        module_id,
        "mp-cancel",
        [("FAKE_AFT_DELAY_FROM_BODY", "1")],
    )
    .await;

    let project = TestProject::new();
    let (_first, _first_ack) = attach_client(&server, &project, 1031, "ses-cancel-primer").await;
    let (mut second, second_ack) =
        attach_client(&server, &project, 1032, "ses-cancel-divergent").await;
    let attach = wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-cancel-divergent"
    })
    .await;
    let module_channel = attach["route_channel"].as_u64().unwrap() as u16;
    assert_ne!(second_ack.route_channel, module_channel);

    let corr = 1033;
    let payload = br#"{"delay_ms":500,"jsonrpc":"2.0","id":"cancel-divergent"}"#;
    write_frame(
        &mut second,
        &data_request(
            second_ack.route_channel,
            second_ack.route_epoch,
            corr,
            payload,
        ),
    )
    .await
    .unwrap();
    second.flush().await.unwrap();
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event_is_request_received(event, module_channel, corr)
    })
    .await;
    write_frame(
        &mut second,
        &cancel_frame(second_ack.route_channel, second_ack.route_epoch, corr),
    )
    .await
    .unwrap();
    second.flush().await.unwrap();
    assert_error(
        &read_frame_timeout(&mut second).await,
        second_ack.route_channel,
        corr,
        "cancelled",
    );
    wait_for_stub_event(&events_path, SETUP_TIMEOUT, |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(module_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(true)
    })
    .await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_generation_invalidation_goodbyes_restarted_provider_only() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 4, Duration::from_millis(20));
    let module_a = "fake-aft-mp-generation-a";
    let module_b = "fake-aft-mp-generation-b";
    let provider_a = spawn_stub(&server, &supervisor, module_a).await;
    let provider_b = spawn_stub_with_env(
        &server,
        &supervisor,
        module_b,
        [("FAKE_AFT_CRASH_AFTER_MS", "800")],
    )
    .await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack_a = attach_on_stream(&mut client, &project, 1041, "ses-gen-a", module_a).await;
    let old_ack_b = attach_on_stream(&mut client, &project, 1042, "ses-gen-b", module_b).await;

    wait_for_status(&provider_b, Duration::from_secs(3), |status| {
        status.restart_count >= 1 && status.state == ModuleState::Running && status.live
    })
    .await;
    let closed = read_frame_timeout(&mut client).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_b,
        "crash",
        Some(false),
        Some(0),
        Some(false),
    );
    let goodbye = read_frame_timeout(&mut client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, old_ack_b.route_channel);

    let payload_a = br#"{"jsonrpc":"2.0","id":"a-still-live"}"#;
    write_frame(
        &mut client,
        &data_request(ack_a.route_channel, ack_a.route_epoch, 1043, payload_a),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    assert_response(
        &read_frame_timeout(&mut client).await,
        ack_a.route_channel,
        1043,
        payload_a,
    );
    let fresh_ack_b =
        attach_on_stream(&mut client, &project, 1044, "ses-gen-b-fresh", module_b).await;
    assert_ne!(fresh_ack_b.route_channel, old_ack_b.route_channel);

    provider_a.stop().await.unwrap();
    provider_b.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_module_death_sends_goodbye_to_each_affected_client() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-mp-death";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_CRASH_AFTER_MS", "250")],
    )
    .await;

    let project = TestProject::new();
    let (mut first, first_ack) = attach_client(&server, &project, 1051, "ses-death-one").await;
    let (mut second, second_ack) = attach_client(&server, &project, 1052, "ses-death-two").await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 2);

    wait_for_binding_count(&server.forwarding, 0, SETUP_TIMEOUT).await;
    let first_closed = read_frame_timeout(&mut first).await;
    let second_closed = read_frame_timeout(&mut second).await;
    assert_route_lifecycle_push(
        &first_closed,
        "route.closed",
        module_id,
        "crash",
        Some(false),
        Some(0),
        Some(false),
    );
    assert_route_lifecycle_push(
        &second_closed,
        "route.closed",
        module_id,
        "crash",
        Some(false),
        Some(0),
        Some(false),
    );
    let first_goodbye = read_frame_timeout(&mut first).await;
    let second_goodbye = read_frame_timeout(&mut second).await;
    assert_eq!(first_goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(first_goodbye.header.channel, first_ack.route_channel);
    assert_eq!(second_goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(second_goodbye.header.channel, second_ack.route_channel);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_closed_crash_at_max_minus_one_is_non_terminal() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-crash-one-restart";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_CRASH_AFTER_MS", "750")],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 1061, "ses-crash-one-restart").await;
    let closed = read_frame_timeout(&mut client).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_id,
        "crash",
        Some(false),
        Some(0),
        Some(false),
    );
    let goodbye = read_frame_timeout(&mut client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, ack.route_channel);

    let status = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running && status.restart_count == 1 && status.live
    })
    .await;
    assert_eq!(status.restart_count, 1);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_closed_crash_at_max_is_terminal() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 0, Duration::from_millis(10));
    let module_id = "fake-aft-crash-no-restarts";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_CRASH_AFTER_MS", "750")],
    )
    .await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 1062, "ses-crash-no-restarts").await;
    let closed = read_frame_timeout(&mut client).await;
    assert_route_lifecycle_push(
        &closed,
        "route.closed",
        module_id,
        "crash",
        Some(false),
        Some(0),
        Some(true),
    );
    let goodbye = read_frame_timeout(&mut client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, ack.route_channel);

    wait_for_registration_absent(&server.registry, module_id, SETUP_TIMEOUT).await;
    let status = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Failed && !status.registration_active && !status.live
    })
    .await;
    assert_eq!(status.restart_count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_route_open_error_mapping_unknown_unavailable_and_verbatim_rejection() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let project = TestProject::new();

    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let unknown = attach_error_on_stream(
        &mut client,
        &project,
        1061,
        "ses-unknown",
        "missing-provider",
    )
    .await;
    assert_eq!(unknown.code, "unknown_module");

    let mut consumer = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    register_manifest_on_stream(
        &mut consumer,
        consumer_manifest("consumer-only-provider"),
        1062,
    )
    .await;
    let unavailable = attach_error_on_stream(
        &mut client,
        &project,
        1063,
        "ses-wrong-role",
        "consumer-only-provider",
    )
    .await;
    assert_eq!(unavailable.code, "target_unavailable");

    let rejecting_id = "fake-aft-mp-reject";
    let rejecting = spawn_stub_with_env(
        &server,
        &supervisor,
        rejecting_id,
        [("FAKE_AFT_REJECT_ATTACH", "1")],
    )
    .await;
    let rejected =
        attach_error_on_stream(&mut client, &project, 1064, "ses-reject", rejecting_id).await;
    assert_eq!(rejected.code, "config_divergence");
    assert_eq!(
        rejected.message,
        "fake AFT rejected route.bind by FAKE_AFT_REJECT_ATTACH"
    );
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 0);

    drop(consumer);
    rejecting.stop().await.unwrap();
}

#[derive(Debug, Clone, Copy)]
struct RouteOpenAck {
    route_channel: u16,
    route_epoch: u32,
}

async fn attach_client(
    server: &TestServer,
    project: &TestProject,
    corr: u64,
    session: &str,
) -> (TcpStream, RouteOpenAck) {
    let module_id = active_tool_provider_module_id(&server.registry);
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(&mut client, project, corr, session, &module_id).await;
    (client, ack)
}

async fn attach_on_stream<S>(
    client: &mut S,
    project: &TestProject,
    corr: u64,
    session: &str,
    module_id: &str,
) -> RouteOpenAck
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(
        client,
        &attach_frame(corr, attach_request(project, session, module_id)),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let ack_frame = read_frame_timeout(client).await;
    assert_eq!(ack_frame.header.ty, FrameType::Response);
    assert_eq!(ack_frame.header.channel, 0);
    assert_eq!(ack_frame.header.corr, corr);
    match serde_json::from_slice(&ack_frame.body).unwrap() {
        ClientControlResponse::RouteOpen {
            route_channel,
            route_epoch,
        } => RouteOpenAck {
            route_channel,
            route_epoch,
        },
        other => panic!("unexpected route.open response: {other:?}"),
    }
}

async fn attach_error_on_stream<S>(
    client: &mut S,
    project: &TestProject,
    corr: u64,
    session: &str,
    module_id: &str,
) -> ErrorBody
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(
        client,
        &attach_frame(corr, attach_request(project, session, module_id)),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let error_frame = read_frame_timeout(client).await;
    assert_eq!(error_frame.header.ty, FrameType::Error);
    assert_eq!(error_frame.header.channel, 0);
    assert_eq!(error_frame.header.corr, corr);
    serde_json::from_slice(&error_frame.body).unwrap()
}

async fn attach_error_on_stream_with_wait<S>(
    client: &mut S,
    project: &TestProject,
    corr: u64,
    session: &str,
    module_id: &str,
    wait: Duration,
) -> ErrorBody
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(
        client,
        &attach_frame(corr, attach_request(project, session, module_id)),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let error_frame = read_frame_timeout_for(client, wait).await;
    assert_eq!(error_frame.header.ty, FrameType::Error);
    assert_eq!(error_frame.header.channel, 0);
    assert_eq!(error_frame.header.corr, corr);
    serde_json::from_slice(&error_frame.body).unwrap()
}

fn active_tool_provider_module_id(registry: &Registry) -> String {
    let (_generation, modules) = registry.list_modules().unwrap();
    modules
        .into_iter()
        .find(|registration| {
            registration
                .manifest
                .provides
                .iter()
                .any(|role| matches!(role, ProviderRole::ToolProvider { .. }))
        })
        .map(|registration| registration.manifest.module_id)
        .expect("test should have a registered tool provider")
}

fn consumer_manifest(module_id: &str) -> ModuleManifest {
    ModuleManifest::builder(module_id, "0.0.0-consumer").build()
}

fn tool_provider_manifest(module_id: &str) -> ModuleManifest {
    let mut manifest = consumer_manifest(module_id);
    manifest.provides = vec![ProviderRole::ToolProvider {
        tools: vec![Tool {
            name: "read".to_string(),
            description: None,
            execution_mode: ExecutionMode::Pure,
            schema: serde_json::json!({"type": "object"}),
        }],
        identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
        concurrency: Concurrency::ModuleManaged,
        emits_push: true,
        sub_supervises: true,
    }];
    manifest
}

fn hello_frame(manifest: ModuleManifest, corr: u64) -> Frame {
    let protocol_ver = manifest.protocol_ver;
    hello_frame_with_protocol(manifest, protocol_ver, corr)
}

fn hello_frame_with_protocol(manifest: ModuleManifest, protocol_ver: u8, corr: u64) -> Frame {
    let body = serde_json::to_vec(&ModuleHelloBody {
        manifest,
        protocol_ver,
        control_ops: None,
        launch_nonce: None,
    })
    .unwrap();
    Frame::build(
        FrameType::Hello,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap()
}

async fn register_manifest_on_stream<S>(
    stream: &mut S,
    manifest: ModuleManifest,
    corr: u64,
) -> ModuleHelloAckBody
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &hello_frame(manifest, corr))
        .await
        .unwrap();
    stream.flush().await.unwrap();

    let ack_frame = read_frame_timeout(stream).await;
    assert_eq!(ack_frame.header.ty, FrameType::HelloAck);
    assert_eq!(ack_frame.header.channel, 0);
    assert_eq!(ack_frame.header.corr, corr);
    serde_json::from_slice(&ack_frame.body).unwrap()
}

fn attach_request(project: &TestProject, session: &str, module_id: &str) -> ClientControlRequest {
    attach_request_with_consumer_identity(project, session, module_id, None)
}

fn attach_request_with_consumer_identity(
    project: &TestProject,
    session: &str,
    module_id: &str,
    consumer_identity: Option<ConsumerIdentity>,
) -> ClientControlRequest {
    ClientControlRequest::RouteOpen {
        target: RouteTarget::ToolProvider {
            module_id: module_id.to_string(),
        },
        identity: BindIdentity::new(
            project.path().to_path_buf(),
            "opencode".to_string(),
            session.to_string(),
        ),
        consumer_identity,
        consumer_capabilities: None,
        admission_facts: None,
    }
}

fn attach_frame(corr: u64, attach: ClientControlRequest) -> Frame {
    let body = serde_json::to_vec(&attach).unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap()
}

fn data_request(channel: u16, epoch: u32, corr: u64, body: &[u8]) -> Frame {
    data_request_with_flags(
        channel,
        epoch,
        corr,
        body,
        Flags::new(false, Priority::Interactive, false),
    )
}

fn subscription_request(channel: u16, epoch: u32, corr: u64, body: &[u8]) -> Frame {
    data_request_with_flags(
        channel,
        epoch,
        corr,
        body,
        Flags(Flags::new(false, Priority::Interactive, false).0 | FLAG_SUBSCRIPTION),
    )
}

fn data_request_with_flags(
    channel: u16,
    epoch: u32,
    corr: u64,
    body: &[u8],
    flags: Flags,
) -> Frame {
    Frame::build(
        FrameType::Request,
        flags,
        channel,
        epoch,
        corr,
        body.to_vec(),
    )
    .unwrap()
}

fn goodbye_frame(channel: u16, epoch: u32, corr: u64) -> Frame {
    let frame = Frame::build(
        FrameType::Goodbye,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        corr,
        Vec::new(),
    )
    .unwrap();
    assert_eq!(frame.header.len, 0);
    assert!(frame.body.is_empty());
    frame
}

fn cancel_frame(channel: u16, epoch: u32, corr: u64) -> Frame {
    let frame = Frame::build(
        FrameType::Cancel,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        corr,
        Vec::new(),
    )
    .unwrap();
    assert_eq!(frame.header.len, 0);
    assert!(frame.body.is_empty());
    frame
}

fn control_request_frame(corr: u64, request: ClientControlRequest) -> Frame {
    let body = serde_json::to_vec(&request).unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap()
}

async fn supervisor_ack_on_stream<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
    module_id: &str,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &control_request_frame(corr, request))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let frame = read_frame_timeout(stream).await;
    assert_supervisor_ack(&frame, corr, module_id)
}

async fn read_supervisor_ack_on_stream<S>(stream: &mut S, corr: u64, module_id: &str) -> bool
where
    S: AsyncRead + Unpin,
{
    let frame = read_frame_timeout(stream).await;
    assert_supervisor_ack(&frame, corr, module_id)
}

async fn read_control_error_on_stream<S>(stream: &mut S, corr: u64, code: &str) -> ErrorBody
where
    S: AsyncRead + Unpin,
{
    let frame = read_frame_timeout(stream).await;
    assert_eq!(frame.header.ty, FrameType::Error);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    let body: ErrorBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(body.code, code);
    body
}

async fn read_supervisor_ack_and_goodbye<S>(
    stream: &mut S,
    corr: u64,
    module_id: &str,
    route_channel: u16,
    reason: &str,
    terminal: bool,
) -> bool
where
    S: AsyncRead + Unpin,
{
    let mut applied = None;
    let mut closing_seen = false;
    let mut closed_seen = false;
    let mut goodbye_seen = false;
    for _ in 0..4 {
        let frame = read_frame_timeout(stream).await;
        if frame.header.channel == 0 && frame.header.corr == corr {
            applied = Some(assert_supervisor_ack(&frame, corr, module_id));
        } else if frame.header.channel == 0 && frame.header.ty == FrameType::Push {
            if !closing_seen {
                assert_route_lifecycle_push(
                    &frame,
                    "route.closing",
                    module_id,
                    reason,
                    None,
                    None,
                    None,
                );
                closing_seen = true;
            } else {
                assert_route_lifecycle_push(
                    &frame,
                    "route.closed",
                    module_id,
                    reason,
                    Some(true),
                    Some(0),
                    Some(terminal),
                );
                closed_seen = true;
            }
        } else if frame.header.ty == FrameType::Goodbye && frame.header.channel == route_channel {
            assert!(closed_seen, "route GOODBYE arrived before route.closed");
            assert_eq!(frame.header.corr, 0);
            assert!(frame.body.is_empty());
            goodbye_seen = true;
        } else {
            panic!(
                "unexpected frame while waiting for supervisor ACK and route GOODBYE: {frame:?}"
            );
        }
    }
    assert!(
        closing_seen && closed_seen,
        "supervisor side-effect should emit route.closing and route.closed"
    );
    assert!(
        goodbye_seen,
        "supervisor side-effect should emit route GOODBYE"
    );
    applied.expect("missing supervisor ACK")
}

fn assert_supervisor_ack(frame: &Frame, corr: u64, module_id: &str) -> bool {
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    match serde_json::from_slice(&frame.body).unwrap() {
        ClientControlResponse::SupervisorAck {
            module_id: actual,
            applied,
        } => {
            assert_eq!(actual, module_id);
            applied
        }
        other => panic!("unexpected supervisor ACK response: {other:?}"),
    }
}

fn route_poll_frame(corr: u64, kind: PollKind, route_channel: u16) -> Frame {
    let body = serde_json::to_vec(&ClientControlRequest::RoutePoll {
        route_channel,
        route_epoch: 1,
        kind,
    })
    .unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap()
}

async fn poll_liveness<S>(stream: &mut S, corr: u64, route_channel: u16) -> Frame
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(
        stream,
        &route_poll_frame(corr, PollKind::Liveness, route_channel),
    )
    .await
    .unwrap();
    stream.flush().await.unwrap();
    read_frame_timeout(stream).await
}

async fn read_until_push_and_response<S>(
    stream: &mut S,
    route_channel: u16,
    response_corr: u64,
    response_body: &[u8],
) -> (Frame, Frame)
where
    S: AsyncRead + Unpin,
{
    let mut push = None;
    let mut response = None;

    for _ in 0..2 {
        let frame = read_frame_timeout(stream).await;
        assert_eq!(frame.header.channel, route_channel);
        match frame.header.ty {
            FrameType::Push if push.is_none() => {
                assert_push(&frame, route_channel);
                push = Some(frame);
            }
            FrameType::Response if response.is_none() => {
                assert_response(&frame, route_channel, response_corr, response_body);
                response = Some(frame);
            }
            ty => panic!("unexpected frame type while waiting for PUSH and Response: {ty:?}"),
        }
    }

    (
        push.expect("missing PUSH frame"),
        response.expect("missing Response frame"),
    )
}

async fn read_push<S>(stream: &mut S, route_channel: u16) -> Frame
where
    S: AsyncRead + Unpin,
{
    let frame = read_frame_timeout(stream).await;
    assert_push(&frame, route_channel);
    frame
}

fn assert_route_lifecycle_push(
    frame: &Frame,
    op: &str,
    module_id: &str,
    reason: &str,
    drained: Option<bool>,
    abandoned: Option<u32>,
    terminal: Option<bool>,
) {
    assert_eq!(frame.header.ty, FrameType::Push);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.epoch, 0);
    assert_eq!(frame.header.corr, 0);

    let mut expected = serde_json::json!({
        "op": op,
        "module_id": module_id,
        "reason": reason,
    });
    if let Some(drained) = drained {
        expected["drained"] = Value::Bool(drained);
    }
    if let Some(abandoned) = abandoned {
        expected["abandoned"] = serde_json::json!(abandoned);
    }
    if op == "route.closed" {
        expected["excluded_subscriptions"] = serde_json::json!(0);
    }
    if let Some(terminal) = terminal {
        expected["terminal"] = Value::Bool(terminal);
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&frame.body).unwrap(),
        expected
    );
}

fn assert_route_closed_with_excluded_subscriptions(
    frame: &Frame,
    module_id: &str,
    drained: bool,
    excluded_subscriptions: u32,
) {
    assert_eq!(frame.header.ty, FrameType::Push);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&frame.body).unwrap(),
        serde_json::json!({
            "op": "route.closed",
            "module_id": module_id,
            "reason": "reload",
            "drained": drained,
            "abandoned": 0,
            "excluded_subscriptions": excluded_subscriptions,
            "terminal": false,
        })
    );
}

/// A drain budget that comfortably covers an in-flight request, for tests that
/// assert the drain WAITED rather than timed out.
///
/// Any test reading a `Response` after the `route.closing` push depends on the
/// drain succeeding. The budget must therefore exceed the stub's delay by a
/// margin that survives the SLOWEST runner, not the developer machine — the
/// stub's reply plus its event-file write both land inside the budget, and a
/// loaded hosted Windows runner has crossed a 5x margin twice:
///
///   0.17.45   concurrent_supervisor_ops_remain_coherent            500 ms vs 100 ms
///   0.18.9    draining_notice_precedes_quiescence_wait_and_...     250 ms vs  50 ms
///
/// The second failed one line over from the first's fix, which is why this is a
/// named constant rather than a third widened literal: the next test that needs
/// a successful drain picks the right value up by name instead of copying a
/// number whose margin nobody re-derives.
///
/// Widening costs nothing on the passing path — the drain still completes in
/// roughly the stub's delay when the request is fast, so the assertion is
/// unchanged and a genuine ordering defect still fails it. Tests that assert
/// `drained: false` deliberately use a SHORT budget and must not use this.
const DRAIN_BUDGET_COVERING_AN_INFLIGHT_REQUEST: Duration = Duration::from_secs(5);

fn assert_push(frame: &Frame, route_channel: u16) {
    assert_eq!(frame.header.ty, FrameType::Push);
    assert_eq!(frame.header.channel, route_channel);
    assert_eq!(frame.header.corr, 0);
    assert_eq!(frame.body, b"push-event");
}

fn assert_response(frame: &Frame, route_channel: u16, corr: u64, body: &[u8]) {
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, route_channel);
    assert_eq!(frame.header.corr, corr);
    assert_eq!(frame.body, body);
}

/// Poll `route.poll{Status}` until subc reports the expected cached status,
/// bounded by a timeout. subc caches a module's status only once its
/// `route.status` PUSH has crossed the socket — the stub-side `status_published`
/// event does NOT prove subc has received it yet, so an immediate poll can
/// legitimately see `None` (see `route_poll_status_cache_miss_returns_none`).
/// A real client re-polls; this helper encodes that. Every poll is answered
/// locally by subc (zero module frames), so callers can still assert the module
/// observed no poll corrs afterward. `base_corr` and the next 31 corrs are used.
async fn wait_for_cached_status<S>(
    client: &mut S,
    route_channel: u16,
    expected_status: &str,
    base_corr: u64,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut attempt = 0u64;
    loop {
        let poll_corr = base_corr + attempt;
        write_frame(
            client,
            &route_poll_frame(poll_corr, PollKind::Status, route_channel),
        )
        .await
        .unwrap();
        client.flush().await.unwrap();

        let frame = read_frame_timeout(client).await;
        assert_eq!(frame.header.ty, FrameType::Response);
        assert_eq!(frame.header.channel, 0);
        assert_eq!(frame.header.corr, poll_corr);
        match serde_json::from_slice(&frame.body).unwrap() {
            ClientControlResponse::RoutePoll {
                route_channel: _route_channel,
                route_epoch: _route_epoch,
                status: Some(status),
                live: None,
            } => {
                assert_eq!(status, expected_status);
                return;
            }
            ClientControlResponse::RoutePoll {
                route_channel: _route_channel,
                route_epoch: _route_epoch,
                status: None,
                live: None,
            } => {
                assert!(
                    Instant::now() < deadline,
                    "route.poll status cache was never populated with {expected_status:?} within the timeout"
                );
                sleep(Duration::from_millis(20)).await;
                attempt += 1;
            }
            other => panic!("unexpected route.poll status response: {other:?}"),
        }
    }
}

fn assert_status_reply(frame: &Frame, corr: u64, expected_status: &str) {
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    match serde_json::from_slice(&frame.body).unwrap() {
        ClientControlResponse::RoutePoll {
            route_channel: _route_channel,
            route_epoch: _route_epoch,
            status: Some(status),
            live: None,
        } => assert_eq!(status, expected_status),
        other => panic!("unexpected route.poll status response: {other:?}"),
    }
}

fn assert_status_none_reply(frame: &Frame, corr: u64) {
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    match serde_json::from_slice(&frame.body).unwrap() {
        ClientControlResponse::RoutePoll {
            route_channel: _route_channel,
            route_epoch: _route_epoch,
            status: None,
            live: None,
        } => {}
        other => panic!("unexpected route.poll missing-status response: {other:?}"),
    }
}

fn assert_liveness_reply(frame: &Frame, corr: u64, expected_live: bool) {
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    match serde_json::from_slice(&frame.body).unwrap() {
        ClientControlResponse::RoutePoll {
            route_channel: _route_channel,
            route_epoch: _route_epoch,
            status: None,
            live: Some(live),
        } => assert_eq!(live, expected_live),
        other => panic!("unexpected route.poll liveness response: {other:?}"),
    }
}

fn assert_error(frame: &Frame, route_channel: u16, corr: u64, code: &str) -> ErrorBody {
    assert_eq!(frame.header.ty, FrameType::Error);
    assert_eq!(frame.header.channel, route_channel);
    assert_eq!(frame.header.corr, corr);
    let body: ErrorBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(body.code, code);
    body
}

async fn read_frame_timeout<S>(stream: &mut S) -> Frame
where
    S: AsyncRead + Unpin,
{
    timeout(READ_TIMEOUT, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for frame")
}

async fn read_frame_timeout_for<S>(stream: &mut S, wait: Duration) -> Frame
where
    S: AsyncRead + Unpin,
{
    timeout(wait, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for frame")
}

async fn assert_no_frame_within<S>(stream: &mut S, wait: Duration)
where
    S: AsyncRead + Unpin,
{
    match timeout(wait, read_frame(stream)).await {
        Err(_) => {}
        Ok(Ok(Some(frame))) => panic!("unexpected frame within {wait:?}: {frame:?}"),
        Ok(Ok(None)) => panic!("connection closed while expecting no frame for {wait:?}"),
        Ok(Err(err)) => panic!("frame read failed while expecting no frame for {wait:?}: {err}"),
    }
}

fn supervisor(server: &TestServer, max_restarts: u32, backoff: Duration) -> Supervisor {
    supervisor_with_drain_timeout(server, max_restarts, backoff, Duration::from_millis(25))
}

fn supervisor_with_drain_timeout(
    server: &TestServer,
    max_restarts: u32,
    backoff: Duration,
    drain_timeout: Duration,
) -> Supervisor {
    Supervisor::new(
        Arc::clone(&server.registry),
        RestartPolicy::new(max_restarts, backoff),
    )
    .with_process_liveness(Arc::clone(&server.process_liveness))
    .with_forwarding(Arc::clone(&server.forwarding))
    .with_handle(server.supervisor_handle.clone())
    .with_drain_timeout(drain_timeout)
    .with_connection_file_path(server.connection_file_path.clone())
}

fn health_config(
    cadence: Duration,
    deadline: Duration,
    failure_threshold: u32,
    on_degraded: HealthAction,
    on_failing: HealthAction,
    critical: bool,
) -> HealthConfig {
    HealthConfig {
        cadence,
        deadline,
        failure_threshold,
        on_degraded,
        on_failing,
        critical,
    }
}

async fn exercise_declared_busy_drain(
    module_id: &str,
    busy_gauges: &str,
    health_metrics: &str,
) -> (bool, u64, Duration) {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_drain_timeout(
        &server,
        1,
        Duration::from_millis(10),
        Duration::from_millis(40),
    );
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_BUSY_GAUGES", busy_gauges),
            ("FAKE_AFT_HEALTH_METRICS", health_metrics),
        ],
    )
    .await;

    let project = TestProject::new();
    let mut route_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(
        &mut route_client,
        &project,
        801,
        "ses-busy-drain",
        module_id,
    )
    .await;
    let mut control_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let drain_started_at = Instant::now();
    write_frame(
        &mut control_client,
        &control_request_frame(
            802,
            ClientControlRequest::SupervisorReload {
                module_id: module_id.to_string(),
            },
        ),
    )
    .await
    .unwrap();
    control_client.flush().await.unwrap();

    let closing = read_frame_timeout(&mut route_client).await;
    assert_route_lifecycle_push(
        &closing,
        "route.closing",
        module_id,
        "reload",
        None,
        None,
        None,
    );
    let closed = read_frame_timeout(&mut route_client).await;
    let closed_body = serde_json::from_slice::<Value>(&closed.body).unwrap();
    let drained = closed_body["drained"].as_bool().unwrap();
    assert_eq!(closed_body["excluded_subscriptions"], 0);
    let goodbye = read_frame_timeout(&mut route_client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, ack.route_channel);

    let applied = read_supervisor_ack_on_stream(&mut control_client, 802, module_id).await;
    assert!(applied);
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;

    let mut diagnostic_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let counter = server_counter(&mut diagnostic_client, 803, "drains_with_undeclared_gauge").await;
    let elapsed = drain_started_at.elapsed();
    module.stop().await.unwrap();
    (drained, counter, elapsed)
}

/// An ACCEPTED route.open is recorded. Refusals have been logged and counted
/// since the attestation work; accepts were invisible, so the daemon stamped a
/// principal on every bind and wrote none of them down -- which left a
/// credential vault unable to name the sender of a call that reached it.
///
/// The assertion is on the COUNTER KEY, which is the principal the daemon
/// stamped, not on a total: a count alone cannot tell an attested supervised
/// caller from an unattested key-holder, and that distinction is the whole
/// reason the trace exists. This client presents no consumer_identity, so it is
/// `direct` by definition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_route_open_is_counted_under_the_stamped_principal() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let (module, _events_path) = spawn_stub_with_events_path(
        &server,
        &supervisor,
        "fake-aft-accept-trace",
        "accept-trace",
    )
    .await;

    let mut diagnostic_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let before = server_counter_key(
        &mut diagnostic_client,
        900,
        "route_open_accepted_by_principal",
        "direct",
    )
    .await;

    let project = TestProject::new();
    let (_client, _ack) = attach_client(&server, &project, 901, "ses-accept-trace").await;

    let after = server_counter_key(
        &mut diagnostic_client,
        902,
        "route_open_accepted_by_principal",
        "direct",
    )
    .await;
    module.stop().await.unwrap();

    assert_eq!(
        after,
        before + 1,
        "an accepted route.open must be counted under the principal the daemon stamped"
    );
}

/// Read one key out of a keyed counter map, 0 when the map or key is absent.
async fn server_counter_key(stream: &mut TcpStream, corr: u64, map: &str, key: &str) -> u64 {
    write_frame(
        stream,
        &control_request_frame(corr, ClientControlRequest::ServerDescribe {}),
    )
    .await
    .unwrap();
    stream.flush().await.unwrap();
    let frame = read_frame_timeout(stream).await;
    match serde_json::from_slice::<ClientControlResponse>(&frame.body).unwrap() {
        ClientControlResponse::ServerDescribe { counters, .. } => counters
            .and_then(|value| value.get(map).cloned())
            .and_then(|value| value.get(key).cloned())
            .and_then(|value| value.as_u64())
            .unwrap_or(0),
        other => panic!("unexpected server.describe response: {other:?}"),
    }
}

/// Read a whole counters entry, for the ones that are objects rather than
/// totals. `None` when the key is absent, which for the breaker surface is
/// itself the assertion that nothing is open.
async fn server_counter_key_value(stream: &mut TcpStream, corr: u64, key: &str) -> Option<Value> {
    write_frame(
        stream,
        &control_request_frame(corr, ClientControlRequest::ServerDescribe {}),
    )
    .await
    .unwrap();
    stream.flush().await.unwrap();
    let frame = read_frame_timeout(stream).await;
    match serde_json::from_slice::<ClientControlResponse>(&frame.body).unwrap() {
        ClientControlResponse::ServerDescribe { counters, .. } => {
            counters.and_then(|value| value.get(key).cloned())
        }
        other => panic!("unexpected server.describe response: {other:?}"),
    }
}

async fn server_counter(stream: &mut TcpStream, corr: u64, name: &str) -> u64 {
    write_frame(
        stream,
        &control_request_frame(corr, ClientControlRequest::ServerDescribe {}),
    )
    .await
    .unwrap();
    stream.flush().await.unwrap();
    let frame = read_frame_timeout(stream).await;
    match serde_json::from_slice::<ClientControlResponse>(&frame.body).unwrap() {
        ClientControlResponse::ServerDescribe { counters, .. } => counters
            .and_then(|value| value.get(name).cloned())
            .and_then(|value| value.as_u64())
            .unwrap_or(0),
        other => panic!("unexpected server.describe response: {other:?}"),
    }
}

async fn connected_client_count(stream: &mut TcpStream, corr: u64) -> u64 {
    write_frame(
        stream,
        &control_request_frame(corr, ClientControlRequest::ServerDescribe {}),
    )
    .await
    .unwrap();
    stream.flush().await.unwrap();
    let frame = read_frame_timeout(stream).await;
    match serde_json::from_slice::<ClientControlResponse>(&frame.body).unwrap() {
        ClientControlResponse::ServerDescribe {
            connected_clients, ..
        } => connected_clients,
        other => panic!("unexpected server.describe response: {other:?}"),
    }
}

async fn spawn_stub(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
) -> SupervisedModule {
    let module = supervisor.spawn(stub_spec(server, module_id)).unwrap();
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    module
}

async fn spawn_stub_with_env<K, V, I>(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
    extra_env: I,
) -> SupervisedModule
where
    K: Into<String>,
    V: Into<String>,
    I: IntoIterator<Item = (K, V)>,
{
    let module = supervisor
        .spawn(stub_spec_with_env(server, module_id, extra_env))
        .unwrap();
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    module
}

async fn spawn_stub_with_events_path(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
    label: &str,
) -> (SupervisedModule, PathBuf) {
    spawn_stub_with_events(
        server,
        supervisor,
        module_id,
        label,
        std::iter::empty::<(&str, &str)>(),
    )
    .await
}

async fn spawn_stub_with_events<K, V, I>(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
    label: &str,
    extra_env: I,
) -> (SupervisedModule, PathBuf)
where
    K: Into<String>,
    V: Into<String>,
    I: IntoIterator<Item = (K, V)>,
{
    let events_path = server.stub_events_path(label);
    let mut env = vec![(
        "FAKE_AFT_EVENTS_PATH".to_string(),
        events_path.to_string_lossy().into_owned(),
    )];
    env.extend(
        extra_env
            .into_iter()
            .map(|(key, value)| (key.into(), value.into())),
    );
    let module = spawn_stub_with_env(server, supervisor, module_id, env).await;
    (module, events_path)
}

fn stub_spec(server: &TestServer, module_id: &str) -> ModuleSpec {
    stub_spec_with_env(server, module_id, std::iter::empty::<(&str, &str)>())
}

/// What the never-connecting stub does when SIGTERM arrives.
///
/// Every variant is a different observation about the supervisor: whether it
/// signalled at all, whether it waited for the child's own exit, and whether it
/// still kills a child that will not take the hint.
// `WriteMarkerAndExitZero` and `Ignore` are only constructed by the SIGTERM
// teardown test, which is unix-only because the daemon sends no signal on
// Windows; the match below stays exhaustive on every platform so the variants
// stay declared there too.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy)]
enum SigtermMode<'a> {
    /// No handler, so SIGTERM keeps its default disposition and the process dies
    /// of signal 15. The shape of an ordinary third-party server.
    Default,
    /// Write this marker, then exit 0.
    WriteMarkerAndExitZero(&'a Path),
    /// Install a handler that does nothing, so the signal is delivered and
    /// deliberately not acted on.
    Ignore,
}

/// A module that never dials subc: no connect, no HELLO, no registration, for
/// its whole life. This is what `protocol: "none"` is for, and it is why the
/// stub grew a mode rather than these tests spawning `sleep` -- `sleep` is alive
/// and unregistered too, but it cannot report what it did with a signal.
///
/// Returns the readiness path when `ready_dir` is given. A test that signals the
/// child must wait for it: the stub installs its SIGTERM handler during startup,
/// and a signal arriving before that gets the DEFAULT disposition, which would
/// make a cooperative-stop assertion fail for a reason that has nothing to do
/// with the supervisor.
fn never_connecting_spec(
    module_id: &str,
    ready_dir: Option<&Path>,
    sigterm: SigtermMode<'_>,
) -> (ModuleSpec, Option<PathBuf>) {
    let mut env = vec![
        ("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()),
        ("FAKE_AFT_NEVER_CONNECT".to_string(), "1".to_string()),
    ];
    match sigterm {
        SigtermMode::Default => {}
        SigtermMode::WriteMarkerAndExitZero(marker) => env.push((
            "FAKE_AFT_SIGTERM_MARKER_PATH".to_string(),
            marker.to_string_lossy().into_owned(),
        )),
        SigtermMode::Ignore => env.push(("FAKE_AFT_IGNORE_SIGTERM".to_string(), "1".to_string())),
    }
    let ready = ready_dir.map(|dir| dir.join(format!("{module_id}.ready")));
    if let Some(ready) = ready.as_ref() {
        env.push((
            "FAKE_AFT_NEVER_CONNECT_READY_PATH".to_string(),
            ready.to_string_lossy().into_owned(),
        ));
    }

    (
        ModuleSpec {
            module_id: module_id.to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
            args: Vec::new(),
            env,
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
        },
        ready,
    )
}

// Only the unix-only SIGTERM teardown test waits on a marker file.
#[cfg(unix)]
async fn wait_for_path(path: &Path, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if path.exists() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} did not appear within {wait:?}",
            path.display()
        );
        sleep(Duration::from_millis(10)).await;
    }
}

fn reserved_stub_spec(server: &TestServer, module_id: &str) -> ModuleSpec {
    let mut spec = stub_spec(server, module_id);
    spec.reserved = true;
    spec
}

fn stub_spec_with_env<K, V, I>(_server: &TestServer, module_id: &str, extra_env: I) -> ModuleSpec
where
    K: Into<String>,
    V: Into<String>,
    I: IntoIterator<Item = (K, V)>,
{
    let mut env = vec![("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string())];
    env.extend(
        extra_env
            .into_iter()
            .map(|(key, value)| (key.into(), value.into())),
    );

    ModuleSpec {
        module_id: module_id.to_string(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
        args: Vec::new(),
        env,
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
    }
}

async fn wait_for_registration(
    registry: &Registry,
    module_id: &str,
    wait: Duration,
) -> subc_daemon::ModuleRegistration {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(registration) = registry.get_module(module_id).unwrap() {
            return registration;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not register within {wait:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_registration_absent(registry: &Registry, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if registry.get_module(module_id).unwrap().is_none() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} registration did not release within {wait:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_binding_count(forwarding: &ForwardingTable, expected: usize, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        let count = forwarding.active_binding_count().unwrap();
        if count == expected {
            return;
        }
        if Instant::now() >= deadline {
            panic!("forwarding binding count {count} did not become {expected} within {wait:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_status(
    module: &SupervisedModule,
    wait: Duration,
    matches: impl Fn(&ModuleStatus) -> bool,
) -> ModuleStatus {
    let deadline = Instant::now() + wait;
    loop {
        let status = module.status().unwrap();
        if matches(&status) {
            return status;
        }
        if Instant::now() >= deadline {
            panic!(
                "module {} did not reach expected status within {wait:?}; last status: {status:?}",
                module.module_id()
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_stub_event<F>(path: &Path, wait: Duration, matches: F) -> Value
where
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + wait;
    loop {
        for event in stub_events(path) {
            if matches(&event) {
                return event;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "stub event did not appear within {wait:?}; events: {:?}",
                stub_events(path)
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_stub_event_count<F>(path: &Path, wait: Duration, matches: F, expected: usize)
where
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + wait;
    loop {
        if stub_events(path)
            .iter()
            .filter(|event| matches(event))
            .count()
            >= expected
        {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "stub event count did not reach {expected} within {wait:?}; events: {:?}",
                stub_events(path)
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn assert_no_stub_event_within<F>(path: &Path, wait: Duration, matches: F)
where
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + wait;
    loop {
        let events = stub_events(path);
        if let Some(event) = events.iter().find(|event| matches(event)) {
            panic!("unexpected stub event within {wait:?}: {event:?}; events: {events:?}");
        }
        if Instant::now() >= deadline {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn event_is_request_received(event: &Value, channel: u16, corr: u64) -> bool {
    event["kind"] == "request_received"
        && event["channel"].as_u64() == Some(u64::from(channel))
        && event["corr"].as_u64() == Some(corr)
}

fn event_is_terminal(event: &Value, terminal: &str, channel: u16, corr: u64) -> bool {
    event["kind"] == "terminal"
        && event["terminal"] == terminal
        && event["channel"].as_u64() == Some(u64::from(channel))
        && event["corr"].as_u64() == Some(corr)
}

fn assert_stub_did_not_observe_corrs(events: &[Value], forbidden_corrs: &[u64]) {
    for event in events {
        if let Some(corr) = event.get("corr").and_then(Value::as_u64) {
            assert!(
                !forbidden_corrs.contains(&corr),
                "stub observed a passive poll corr {corr}; events: {events:?}"
            );
        }
    }
}

fn event_position<F>(events: &[Value], matches: F) -> usize
where
    F: FnMut(&Value) -> bool,
{
    events
        .iter()
        .position(matches)
        .unwrap_or_else(|| panic!("stub event missing from {events:?}"))
}

async fn read_frame_or_close_timeout<S>(stream: &mut S, wait: Duration) -> Option<Frame>
where
    S: AsyncRead + Unpin,
{
    timeout(wait, read_frame(stream))
        .await
        .expect("timed out waiting for frame or clean close")
        .expect("frame read failed while waiting for frame or clean close")
}

/// Reads the stub's event file, DROPPING any line that does not parse.
///
/// The drop is deliberate: a reader polling a file another process is
/// appending to will occasionally catch a torn tail, and failing the whole
/// read on it would turn every poll into a coin flip. But the drop is also
/// SILENT, which makes a corrupted line indistinguishable from an event that
/// never happened -- the waiter then blocks until its timeout and reports an
/// absence.
///
/// The writer is what makes that safe. `append_json_line` in the stub emits
/// one `write_all` per event, so concurrent appenders interleave only BETWEEN
/// lines. If it ever goes back to `writeln!`, this filter silently eats the
/// evidence: measured at 1576 of 1600 events lost with 8 concurrent writers.
fn stub_events(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .ok()
        .into_iter()
        .flat_map(|contents| {
            contents
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

struct TestProject {
    temp: TestTempDir,
}

impl TestProject {
    fn new() -> Self {
        Self {
            temp: TestTempDir::new("forwarding-project"),
        }
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }
}

/// Arm 2 of the forwarding contention benchmark: loopback TCP + fake-aft-stub.
/// Run: `cargo test -p subc-core forwarding_bench_e2e_arm2 -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "forwarding contention baseline; run with --ignored --nocapture"]
async fn forwarding_bench_e2e_arm2() {
    const WARMUP: u64 = 100;
    const MEASURE: u64 = 1_000;
    const PAYLOAD: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"read","params":{}}"#;
    let client_sweep: &[(usize, usize)] = &[(1, 1), (8, 1), (32, 1), (8, 8), (32, 8)];

    eprintln!("=== subc forwarding bench arm2 (BASELINE / loopback e2e) ===");
    eprintln!(
        "logical CPUs: {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    eprintln!("warmup={WARMUP} measure={MEASURE} per client\n");
    eprintln!(
        "{:<8} {:<8} {:>10} {:>10} {:>14}",
        "clients", "routes", "p50_ms", "p99_ms", "calls/s"
    );
    eprintln!("{}", "-".repeat(58));

    for &(num_clients, routes_per_client) in client_sweep {
        let snap =
            run_e2e_bench_cell(num_clients, routes_per_client, WARMUP, MEASURE, PAYLOAD).await;
        eprintln!(
            "{:<8} {:<8} {:>10.3} {:>10.3} {:>14.0}",
            num_clients, routes_per_client, snap.p50_ms, snap.p99_ms, snap.throughput
        );
    }
}

#[derive(Clone, Copy)]
struct E2eLatencySnapshot {
    p50_ms: f64,
    p99_ms: f64,
    throughput: f64,
}

async fn run_e2e_bench_cell(
    num_clients: usize,
    routes_per_client: usize,
    warmup: u64,
    measure: u64,
    payload: &[u8],
) -> E2eLatencySnapshot {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-bench-e2e";
    let module = spawn_stub(&server, &supervisor, module_id).await;
    let project = TestProject::new();

    let mut clients = Vec::with_capacity(num_clients);
    for client_index in 0..num_clients {
        let mut stream = connect_authed_client(&server.connection_file_path)
            .await
            .unwrap();
        let mut channels = Vec::with_capacity(routes_per_client);
        for route in 0..routes_per_client {
            let session = format!("bench-e2e-{client_index}-{route}");
            let corr = 10_000 + client_index as u64 * 100 + route as u64;
            let ack = attach_on_stream(&mut stream, &project, corr, &session, module_id).await;
            channels.push(ack.route_channel);
        }
        clients.push((client_index, stream, channels));
    }

    let wall_start = Instant::now();
    let mut worker_handles = Vec::with_capacity(num_clients);
    for (client_index, mut stream, channels) in clients {
        let payload = payload.to_vec();
        worker_handles.push(tokio::spawn(async move {
            let mut latencies_ns = Vec::with_capacity(measure as usize);
            let routes = channels.len();
            let mut corr = client_index as u64 * 1_000_000;
            for _ in 0..warmup {
                let ch = channels[corr as usize % routes];
                write_frame(&mut stream, &data_request(ch, 1, corr, &payload))
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
                let _ = read_frame_timeout(&mut stream).await;
                corr = corr.wrapping_add(1);
            }
            for _ in 0..measure {
                let ch = channels[corr as usize % routes];
                let t0 = Instant::now();
                write_frame(&mut stream, &data_request(ch, 1, corr, &payload))
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
                let response = read_frame_timeout(&mut stream).await;
                assert_eq!(response.header.ty, FrameType::Response);
                assert_eq!(response.body, payload);
                latencies_ns.push(t0.elapsed().as_nanos() as u64);
                corr = corr.wrapping_add(1);
            }
            latencies_ns
        }));
    }

    let mut all = Vec::new();
    for handle in worker_handles {
        all.extend(handle.await.unwrap());
    }
    let wall = wall_start.elapsed();
    all.sort_unstable();
    let n = all.len();
    let idx = |p: f64| ((p * (n as f64 - 1.0)).round() as usize).min(n.saturating_sub(1));
    let to_ms = |v: u64| v as f64 / 1_000_000.0;
    let total_calls = (num_clients as u64) * measure;
    module.stop().await.unwrap();

    E2eLatencySnapshot {
        p50_ms: to_ms(all[idx(0.50)]),
        p99_ms: to_ms(all[idx(0.99)]),
        throughput: total_calls as f64 / wall.as_secs_f64(),
    }
}

// ---------------------------------------------------------------------------
// Blue/green swap (`supervisor.swap`).
//
// Each test starts an incumbent whose stub writes its events to one file, then
// changes the module's spec (as a rescan would) so the swap's candidate writes
// to another. Which file an `attach` or `draining` event lands in is therefore
// which process received the route or the drain.
// ---------------------------------------------------------------------------

fn swap_spec<K, V, I>(server: &TestServer, module_id: &str, env: I) -> ModuleSpec
where
    K: Into<String>,
    V: Into<String>,
    I: IntoIterator<Item = (K, V)>,
{
    let mut spec = stub_spec_with_env(server, module_id, env);
    spec.overlap = ModuleOverlap::Safe;
    spec
}

fn events_of_kind(path: &Path, kind: &str) -> usize {
    stub_events(path)
        .iter()
        .filter(|event| event["kind"] == kind)
        .count()
}

async fn wait_for_candidate(
    registry: &Registry,
    module_id: &str,
    wait: Duration,
) -> subc_daemon::ModuleRegistration {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(registration) = registry.get_candidate(module_id).unwrap() {
            return registration;
        }
        assert!(
            Instant::now() < deadline,
            "no swap candidate for {module_id} registered within {wait:?}"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

fn send_swap(module_id: &str, corr: u64, ready_timeout_ms: u64) -> Frame {
    control_request_frame(
        corr,
        ClientControlRequest::SupervisorSwap {
            module_id: module_id.to_string(),
            ready_timeout_ms: Some(ready_timeout_ms),
        },
    )
}

/// The full swap on the stub: the candidate registers unroutable, flips
/// ready, cutover sends new route.opens to it, and the incumbent is drained
/// with reason `restart` and exits. Also pins the spawn role end to end: the
/// incumbent's spec tries to set `SUBC_SPAWN_ROLE` and the process sees it
/// unset; the candidate sees `swap_candidate`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swap_cuts_new_routes_over_to_a_ready_candidate_and_drains_the_incumbent() {
    let module_id = "fake-aft-swap-e2e";
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let incumbent_events = server.stub_events_path("swap-e2e-incumbent");
    let candidate_events = server.stub_events_path("swap-e2e-candidate");
    let ready_gate = server.temp_dir.join("swap-e2e-ready");
    let module = supervisor
        .spawn(swap_spec(
            &server,
            module_id,
            [
                (
                    "FAKE_AFT_EVENTS_PATH",
                    incumbent_events.to_string_lossy().into_owned(),
                ),
                ("FAKE_AFT_ECHO_ENV", "SUBC_SPAWN_ROLE".to_string()),
                ("SUBC_SPAWN_ROLE", "swap_candidate".to_string()),
            ],
        ))
        .unwrap();
    let incumbent = wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    let incumbent_status = module.status().unwrap();

    let project = TestProject::new();
    let mut before_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let before = attach_on_stream(
        &mut before_client,
        &project,
        1,
        "ses-swap-before",
        module_id,
    )
    .await;
    assert_eq!(events_of_kind(&incumbent_events, "attach"), 1);

    module
        .update_spec_for_test(swap_spec(
            &server,
            module_id,
            [
                (
                    "FAKE_AFT_EVENTS_PATH",
                    candidate_events.to_string_lossy().into_owned(),
                ),
                ("FAKE_AFT_READY_FALSE", "1".to_string()),
                (
                    "FAKE_AFT_READY_UPDATE_PATH",
                    ready_gate.to_string_lossy().into_owned(),
                ),
                ("FAKE_AFT_ECHO_ENV", "SUBC_SPAWN_ROLE".to_string()),
            ],
        ))
        .await
        .unwrap();
    let mut control = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(&mut control, &send_swap(module_id, 2, 10_000))
        .await
        .unwrap();
    control.flush().await.unwrap();

    // Registered, not ready, and not routable: a route.open still lands on the
    // incumbent.
    let candidate = wait_for_candidate(&server.registry, module_id, SETUP_TIMEOUT).await;
    assert!(!candidate.ready);
    assert_ne!(candidate.connection_id, incumbent.connection_id);
    let mut during_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    attach_on_stream(
        &mut during_client,
        &project,
        3,
        "ses-swap-during",
        module_id,
    )
    .await;
    assert_eq!(events_of_kind(&incumbent_events, "attach"), 2);
    assert_eq!(events_of_kind(&candidate_events, "attach"), 0);

    // Ready: the swap cuts over and answers.
    fs::write(&ready_gate, b"ready").unwrap();
    assert!(read_supervisor_ack_on_stream(&mut control, 2, module_id).await);
    let promoted = server.registry.get_module(module_id).unwrap().unwrap();
    assert_eq!(promoted.connection_id, candidate.connection_id);

    let mut after_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    attach_on_stream(&mut after_client, &project, 4, "ses-swap-after", module_id).await;
    assert_eq!(events_of_kind(&candidate_events, "attach"), 1);
    assert_eq!(events_of_kind(&incumbent_events, "attach"), 2);

    // The incumbent is drained as a restart, by its endpoint, and exits.
    let closing = read_frame_timeout(&mut before_client).await;
    assert_route_lifecycle_push(
        &closing,
        "route.closing",
        module_id,
        "restart",
        None,
        None,
        None,
    );
    let closed = read_frame_timeout(&mut before_client).await;
    assert_eq!(
        serde_json::from_slice::<Value>(&closed.body).unwrap()["op"],
        "route.closed"
    );
    let goodbye = read_frame_timeout(&mut before_client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, before.route_channel);
    wait_for_stub_event(&incumbent_events, SETUP_TIMEOUT, |event| {
        event["kind"] == "draining" && event["reason"] == "restart"
    })
    .await;
    let deadline = Instant::now() + SETUP_TIMEOUT;
    while server
        .registry
        .get_module_by_connection(incumbent.connection_id)
        .unwrap()
        .is_some()
    {
        assert!(
            Instant::now() < deadline,
            "the incumbent never deregistered"
        );
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        events_of_kind(&candidate_events, "draining"),
        0,
        "the promoted candidate must not be drained"
    );

    let status = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running && status.pid != incumbent_status.pid
    })
    .await;
    assert_eq!(status.restart_count, incumbent_status.restart_count);
    assert!(status.spawn_generation > incumbent_status.spawn_generation);

    let tail = module.stderr_tail(None, None);
    let roles = tail
        .entries
        .iter()
        .filter_map(|entry| match entry {
            TailEntry::Line { text, .. } if text.starts_with("echo-env SUBC_SPAWN_ROLE=") => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        [
            "echo-env SUBC_SPAWN_ROLE=<UNSET>",
            "echo-env SUBC_SPAWN_ROLE=swap_candidate"
        ],
        "a plain spawn must not carry the spawn role even when its spec sets it"
    );
    module.stop().await.unwrap();
}

/// Design mutation arm (i). A candidate that never declares itself ready
/// fails the swap, and the incumbent is left serving and UNDRAINED: its
/// consumers see no `route.closing`, and it was never told to drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swap_whose_candidate_never_becomes_ready_leaves_the_incumbent_undrained() {
    let module_id = "fake-aft-swap-never-ready";
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let incumbent_events = server.stub_events_path("swap-never-ready-incumbent");
    let candidate_events = server.stub_events_path("swap-never-ready-candidate");
    let module = supervisor
        .spawn(swap_spec(
            &server,
            module_id,
            [(
                "FAKE_AFT_EVENTS_PATH",
                incumbent_events.to_string_lossy().into_owned(),
            )],
        ))
        .unwrap();
    let incumbent = wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    let project = TestProject::new();
    let mut route_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    attach_on_stream(&mut route_client, &project, 1, "ses-never-ready", module_id).await;

    module
        .update_spec_for_test(swap_spec(
            &server,
            module_id,
            [
                (
                    "FAKE_AFT_EVENTS_PATH",
                    candidate_events.to_string_lossy().into_owned(),
                ),
                ("FAKE_AFT_READY_FALSE", "1".to_string()),
            ],
        ))
        .await
        .unwrap();
    let mut control = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(&mut control, &send_swap(module_id, 2, 500))
        .await
        .unwrap();
    control.flush().await.unwrap();

    // Watch the incumbent's consumer across the whole readiness budget and
    // past it, BEFORE reading the swap's answer: a drain of the incumbent
    // would begin right after the answer, so this is where it would show.
    if let Ok(frame) = timeout(Duration::from_millis(1500), read_frame(&mut route_client)).await {
        let frame = frame.unwrap().expect("route connection stays open");
        panic!(
            "the incumbent's consumer received {:?} during a failed swap; the incumbent was drained",
            serde_json::from_slice::<Value>(&frame.body)
                .ok()
                .and_then(|body| body.get("op").cloned())
        );
    }
    let error = read_control_error_on_stream(&mut control, 2, "swap_failed").await;
    assert_eq!(error.detail.unwrap()["arm"], "never_ready");
    assert_eq!(
        events_of_kind(&incumbent_events, "draining"),
        0,
        "a failed swap must never drain the incumbent"
    );
    assert!(server.registry.get_candidate(module_id).unwrap().is_none());
    assert_eq!(
        server
            .registry
            .get_module(module_id)
            .unwrap()
            .unwrap()
            .connection_id,
        incumbent.connection_id
    );
    // Still serving: a fresh route.open lands on the incumbent.
    let mut next_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    attach_on_stream(
        &mut next_client,
        &project,
        3,
        "ses-never-ready-next",
        module_id,
    )
    .await;
    assert_eq!(events_of_kind(&incumbent_events, "attach"), 2);
    module.stop().await.unwrap();
}

/// A candidate that dies while warming is reaped on its own path: it spends
/// no unit of the module's crash budget, adds nothing to its lifetime restart
/// ledger, and leaves the incumbent `running` with the same process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swap_candidate_dying_mid_warm_spends_no_restart_budget() {
    let module_id = "fake-aft-swap-dies-warming";
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module = supervisor
        .spawn(swap_spec(
            &server,
            module_id,
            std::iter::empty::<(&str, &str)>(),
        ))
        .unwrap();
    let incumbent = wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    let before = module.status().unwrap();

    module
        .update_spec_for_test(swap_spec(
            &server,
            module_id,
            [
                ("FAKE_AFT_READY_FALSE", "1"),
                ("FAKE_AFT_CRASH_AFTER_MS", "200"),
            ],
        ))
        .await
        .unwrap();
    let error = module
        .swap(Some(Duration::from_secs(10)))
        .await
        .expect_err("a candidate that crashes while warming fails the swap");
    match error {
        SuperviseError::SwapFailed {
            arm,
            candidate_exit,
            ..
        } => {
            assert_eq!(arm, SwapFailureArm::CandidateExited);
            assert_eq!(candidate_exit.unwrap().kind, ExitKind::Crash);
        }
        other => panic!("expected SwapFailed, got {other:?}"),
    }

    let after = module.status().unwrap();
    assert_eq!(after.state, ModuleState::Running);
    assert_eq!(after.pid, before.pid);
    assert_eq!(after.restart_count, before.restart_count);
    assert_eq!(after.lifetime_restarts, before.lifetime_restarts);
    assert_eq!(after.spawn_generation, before.spawn_generation);
    assert_eq!(after.last_exit, before.last_exit);
    assert!(server.registry.get_candidate(module_id).unwrap().is_none());
    assert_eq!(
        server
            .registry
            .get_module(module_id)
            .unwrap()
            .unwrap()
            .connection_id,
        incumbent.connection_id
    );
    // And the supervise loop is not left believing the module crashed: it
    // stays running with the same process well past the respawn backoff.
    sleep(Duration::from_millis(200)).await;
    let settled = module.status().unwrap();
    assert_eq!(settled.state, ModuleState::Running);
    assert_eq!(settled.pid, before.pid);
    assert_eq!(settled.restart_count, before.restart_count);
    module.stop().await.unwrap();
}

/// `overlap` defaults to exclusive, and a swap of an exclusive module is
/// refused before anything is spawned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swap_of_an_exclusive_module_is_refused_and_spawns_nothing() {
    let module_id = "fake-aft-swap-exclusive";
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module = spawn_stub(&server, &supervisor, module_id).await;
    let before = module.status().unwrap();

    let mut control = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(&mut control, &send_swap(module_id, 1, 10_000))
        .await
        .unwrap();
    control.flush().await.unwrap();
    let error = read_control_error_on_stream(&mut control, 1, "swap_refused").await;
    assert_eq!(error.detail.unwrap()["reason"], "overlap_exclusive");
    assert!(
        error.message.contains("exclusive"),
        "the refusal names the declaration: {}",
        error.message
    );

    let after = module.status().unwrap();
    assert_eq!(after.spawn_generation, before.spawn_generation);
    assert_eq!(after.pid, before.pid);
    assert!(server.registry.get_candidate(module_id).unwrap().is_none());
    module.stop().await.unwrap();
}

/// An operator disable issued while a swap's candidate is warming must not
/// wait out the readiness budget. It aborts the swap: the candidate is killed
/// on its own path (no crash budget spent), the swap answers `interrupted`,
/// and the disable is then carried out on the incumbent. Sent over the wire,
/// because the control handler's operation lock is part of what could make it
/// wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabling_a_module_mid_swap_aborts_the_swap_without_waiting_for_it() {
    let module_id = "fake-aft-swap-disabled-mid-warm";
    let ready_budget = Duration::from_secs(30);
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module = supervisor
        .spawn(swap_spec(
            &server,
            module_id,
            std::iter::empty::<(&str, &str)>(),
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    let before = module.status().unwrap();
    module
        .update_spec_for_test(swap_spec(
            &server,
            module_id,
            [("FAKE_AFT_READY_FALSE", "1")],
        ))
        .await
        .unwrap();

    let mut swap_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    write_frame(
        &mut swap_client,
        &send_swap(module_id, 1, ready_budget.as_millis() as u64),
    )
    .await
    .unwrap();
    swap_client.flush().await.unwrap();
    let candidate = wait_for_candidate(&server.registry, module_id, SETUP_TIMEOUT).await;

    let disable_started = Instant::now();
    let mut disable_client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    assert!(
        supervisor_ack_on_stream(
            &mut disable_client,
            2,
            ClientControlRequest::SupervisorSetEnabled {
                module_id: module_id.to_string(),
                enabled: false,
            },
            module_id,
        )
        .await,
        "the disable must apply"
    );
    let waited = disable_started.elapsed();
    assert!(
        waited < Duration::from_secs(10),
        "the disable waited {waited:?} behind a swap whose readiness budget is {ready_budget:?}"
    );

    let error = read_control_error_on_stream(&mut swap_client, 1, "swap_failed").await;
    assert_eq!(error.detail.unwrap()["arm"], "interrupted");
    assert!(server.registry.get_candidate(module_id).unwrap().is_none());
    assert!(
        server
            .registry
            .get_module_by_connection(candidate.connection_id)
            .unwrap()
            .is_none(),
        "the candidate is still registered"
    );
    let after = module.status().unwrap();
    assert_eq!(after.state, ModuleState::Disabled);
    assert!(!after.enabled);
    assert_eq!(after.restart_count, before.restart_count);
    assert_eq!(after.lifetime_restarts, before.lifetime_restarts);
    assert!(server.registry.get_module(module_id).unwrap().is_none());
}
