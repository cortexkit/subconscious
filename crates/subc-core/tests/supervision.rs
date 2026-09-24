use std::{ops::Deref, path::PathBuf, sync::Arc, time::Duration};

use subc_control::{
    ClientControlRequest, ClientControlResponse, ModuleProtocol, SpawnCursor, SpawnEvent,
    SpawnEventKind, SpawnSnapshot, TerminalDisposition,
};
use subc_daemon::{
    stderr_tail::{CaptureState, StderrTailSnapshot, TailEntry},
    test_support::TestTempDir,
    ModuleSpec, ModuleState, ModuleStatus, Registry, RestartPolicy, SuperviseError,
    SupervisedModule, Supervisor, SupervisorHandle, SupervisorProcessLiveness,
};
use subc_protocol::{ErrorBody, Flags, FrameType, Priority};
use subc_transport::{read_frame, write_frame};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    time::{sleep, timeout, Instant},
};

mod common;
use common::{
    connect_authed_client, start_test_daemon_with_process_liveness_and_supervisor, TestDaemon,
};

struct TestServer {
    daemon: TestDaemon,
}

impl TestServer {
    async fn start() -> Self {
        Self {
            daemon: TestDaemon::start("supervision-server").await,
        }
    }
}

impl Deref for TestServer {
    type Target = TestDaemon;

    fn deref(&self) -> &Self::Target {
        &self.daemon
    }
}

fn assert_current_process_facts_cleared(status: &ModuleStatus) {
    assert_eq!(status.pid, None);
    assert_eq!(status.spawned_at_ms, None);
    assert_eq!(status.spawned_from, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_registers_stub_and_reports_running() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-spawn";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let registration = server
        .registry
        .get_module(module_id)
        .unwrap()
        .expect("spawn_stub waits for registration");
    assert_eq!(registration.manifest.module_id, module_id);

    let status = wait_for_status(&module, Duration::from_secs(1), |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;
    assert!(status.process_alive);
    assert!(status.registration_active);
    assert_eq!(status.restart_count, 0);
    assert_eq!(status.spawn_generation, 1);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_records_exact_process_facts() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-spawn-facts";
    let spec = stub_spec(&server, module_id, std::iter::empty::<(&str, &str)>());
    let expected_program = spec.program.clone();
    let before_spawn_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let module = supervisor.spawn(spec).unwrap();
    let after_spawn_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    wait_for_registration(&server.registry, module_id, Duration::from_secs(10)).await;

    let first = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;
    assert!(first.pid.is_some(), "running child PID must be retained");
    assert_ne!(first.spawned_at_ms, Some(0));
    assert!(
        first.spawned_at_ms.unwrap() >= before_spawn_ms
            && first.spawned_at_ms.unwrap() <= after_spawn_ms,
        "spawn time must be captured around Supervisor::spawn: {first:?}"
    );
    assert_eq!(first.spawned_from, Some(expected_program.clone()));

    let before_restart_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    module.restart(None).await.unwrap();
    let restarted = wait_for_status(&module, Duration::from_secs(5), |status| {
        status.state == ModuleState::Running && status.live && status.pid != first.pid
    })
    .await;
    let after_restart_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert_ne!(restarted.pid, first.pid);
    assert!(
        restarted.spawned_at_ms.unwrap() >= before_restart_ms
            && restarted.spawned_at_ms.unwrap() <= after_restart_ms,
        "restart must replace the spawn timestamp: {restarted:?}"
    );
    assert!(restarted.spawned_at_ms.unwrap() > first.spawned_at_ms.unwrap());
    assert_eq!(restarted.spawned_from, Some(expected_program));

    module.stop().await.unwrap();
    let stopped = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.state == ModuleState::Stopped && !status.process_alive
    })
    .await;
    assert_eq!(stopped.pid, None);
    assert_eq!(stopped.spawned_at_ms, None);
    assert_eq!(stopped.spawned_from, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_restarts_and_reregisters_stub() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 5, Duration::from_millis(20));
    let module_id = "fake-aft-restart";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_CRASH_AFTER_MS", "250")],
    )
    .await;

    let status = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.restart_count >= 1 && status.state == ModuleState::Running && status.live
    })
    .await;
    assert!(status.process_alive);
    assert!(status.registration_active);
    assert!(status.restart_count >= 1);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_clears_current_process_facts_before_replacement() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(250));
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "fake-aft-crash-clears-process-facts",
        [("FAKE_AFT_CRASH_AFTER_MS", "100")],
    )
    .await;

    let restarting = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.state == ModuleState::Restarting && !status.process_alive
    })
    .await;
    assert_current_process_facts_cleared(&restarting);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_enabled_current_value_returns_false_without_state_mutation() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-enabled-idempotent";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let before = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;
    let applied = module.set_enabled(true).await.unwrap();
    assert!(
        !applied,
        "setting enabled=true on an enabled module is a no-op"
    );

    let after = module.status().unwrap();
    assert_eq!(after.state, ModuleState::Running);
    assert!(after.enabled);
    assert!(after.live);
    assert_eq!(after.pid, before.pid);
    assert_eq!(after.restart_count, before.restart_count);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_spawn_during_enable_allows_a_later_retry() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    // A path that must NOT exist: the spawn is made to fail for real. The guard
    // owns the parent dir; the program path itself is a never-created child.
    let _dir = TestTempDir::new("missing-enable-program");
    let missing_program = _dir.path().join("missing-program");
    assert!(!missing_program.exists());
    let module = supervisor
        .supervise_configured(
            ModuleSpec {
                module_id: "missing-enable-program".to_string(),
                program: missing_program,
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            },
            false,
        )
        .unwrap();

    let first = module.set_enabled(true).await;
    let failed = module.status().unwrap();
    let second = module.set_enabled(true).await;

    assert!(matches!(first, Err(SuperviseError::Spawn { .. })));
    assert_eq!(
        failed.state,
        ModuleState::Failed,
        "failed enable must leave a retryable state; second enable returned {second:?}"
    );
    assert!(failed.enabled);
    assert!(!failed.process_alive);
    assert_current_process_facts_cleared(&failed);
    assert!(
        matches!(second, Err(SuperviseError::Spawn { .. })),
        "second enable must retry spawning instead of returning {second:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_without_forwarding_table_returns_reload_unavailable_without_state_mutation() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-reload-unavailable";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let before = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;
    let err = module
        .reload()
        .await
        .expect_err("reload without a forwarding table must be typed");
    assert!(
        matches!(err, SuperviseError::ReloadUnavailable { ref module_id, ref reason }
            if module_id == "fake-aft-reload-unavailable"
                && reason.contains("forwarding table")),
        "expected ReloadUnavailable, got {err:?}"
    );

    let after = module.status().unwrap();
    assert_eq!(after.state, ModuleState::Running);
    assert!(after.enabled);
    assert!(after.live);
    assert_eq!(after.pid, before.pid);
    assert_eq!(after.restart_count, before.restart_count);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_and_reload_are_rejected_for_a_disabled_module() {
    // restart/reload cycle a RUNNING module. A disabled module is intentionally
    // off, so these must be rejected (with a typed Disabled error) rather than
    // silently re-enabling and spawning it — that requires explicit set_enabled.
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 5, Duration::from_millis(20));
    let module_id = "fake-aft-disabled-guard";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    wait_for_status(&module, Duration::from_secs(3), |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;

    // Disable it, then confirm restart and reload both refuse.
    let changed = module.set_enabled(false).await.unwrap();
    assert!(changed, "module should transition from enabled to disabled");
    let disabled = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.state == ModuleState::Disabled && !status.process_alive
    })
    .await;
    assert_current_process_facts_cleared(&disabled);

    let restart_err = module
        .restart(None)
        .await
        .expect_err("restart on a disabled module must be rejected");
    assert!(
        matches!(restart_err, SuperviseError::Disabled { .. }),
        "expected Disabled, got {restart_err:?}"
    );
    let reload_err = module
        .reload()
        .await
        .expect_err("reload on a disabled module must be rejected");
    assert!(
        matches!(reload_err, SuperviseError::Disabled { .. }),
        "expected Disabled, got {reload_err:?}"
    );

    // It must still be disabled (the rejected commands did not start it).
    let status = module.status().unwrap();
    assert_eq!(status.state, ModuleState::Disabled);
    assert!(!status.process_alive);

    // Explicit enable still works and brings it back.
    let reenabled = module.set_enabled(true).await.unwrap();
    assert!(reenabled);
    wait_for_status(&module, Duration::from_secs(3), |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_restart_resets_restart_count() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 5, Duration::from_millis(20));
    let module_id = "fake-aft-operator-reset";
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        module_id,
        [("FAKE_AFT_CRASH_AFTER_MS", "750")],
    )
    .await;

    let crashed = wait_for_status(&module, Duration::from_secs(5), |status| {
        status.restart_count == 2
            && status.lifetime_restarts == 2
            && status.state == ModuleState::Running
            && status.live
    })
    .await;
    assert_eq!(crashed.restart_count, 2);
    assert_eq!(crashed.lifetime_restarts, 2);
    assert_eq!(crashed.spawn_generation, 3);

    module.restart(None).await.unwrap();

    let restarted = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.restart_count == 0
            && status.lifetime_restarts == 2
            && status.state == ModuleState::Running
            && status.live
    })
    .await;
    assert_eq!(restarted.restart_count, 0);
    assert_eq!(restarted.lifetime_restarts, 2);
    assert_eq!(restarted.spawn_generation, 4);
    assert!(restarted.process_alive);
    assert!(restarted.registration_active);

    module.stop().await.unwrap();
}

/// Issue #34, arm 2: a spawn failure on OPERATOR restart must land the module
/// in `Failed` -- the observable, revivable terminal -- not strand it in
/// `Restarting` with no child. The spawn is made to fail for real (the program
/// is a per-test copy of the stub, deleted before the restart), not by mocking:
/// the discriminator is the STATE the failure leaves behind, and before the fix
/// this test reads `Restarting` forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_restart_spawn_failure_lands_failed_not_restarting() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 3, Duration::from_millis(10));
    let module_id = "fake-aft-restart-spawn-fail";

    // Per-test copy of the stub so deleting it cannot affect parallel tests.
    // The guard owns the parent dir; the stub copy is a file inside it.
    let _dir = TestTempDir::new("fake-aft-stub-copy");
    let stub_copy = _dir.path().join("fake-aft-stub");
    common::copy_executable(
        std::path::Path::new(env!("CARGO_BIN_EXE_fake-aft-stub")),
        &stub_copy,
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub_copy, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut spec = stub_spec(&server, module_id, []);
    spec.program = stub_copy.clone();
    let module = supervisor.spawn(spec).unwrap();
    // 30s: the per-test copy is a FRESH INODE, so its first exec pays the macOS
    // assessment toll (0.5-22s observed); the usual 5s bound flakes here.
    wait_for_status(&module, Duration::from_secs(30), |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;

    // The respawn half of the restart must fail: the program is gone. RENAME
    // rather than delete -- the old child is still executing from this path,
    // and Windows refuses to delete a running executable (the lock is on the
    // object, so renaming the name away is permitted on every platform).
    let stub_moved = stub_copy.with_extension("moved");
    std::fs::rename(&stub_copy, &stub_moved).unwrap();
    // The restart command acks at initiation; the failure lands in state.
    module.restart(None).await.unwrap();

    let failed = wait_for_status(&module, Duration::from_secs(5), |status| {
        status.state != ModuleState::Restarting && !status.process_alive
    })
    .await;
    assert_eq!(
        failed.state,
        ModuleState::Failed,
        "spawn failure on operator restart must be visible as Failed, not stranded in a transient state"
    );
    assert_eq!(
        failed.spawn_generation, 1,
        "a failed spawn must not consume a successful-spawn generation"
    );

    // And Failed is the revivable state: restoring the program and re-enabling
    // heals it, which is the property Restarting-stranding denied the operator.
    common::copy_executable(
        std::path::Path::new(env!("CARGO_BIN_EXE_fake-aft-stub")),
        &stub_copy,
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub_copy, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let applied = module.set_enabled(true).await.unwrap();
    assert!(applied);
    // Fresh inode again after the re-copy: same assessment-toll bound.
    let revived = wait_for_status(&module, Duration::from_secs(30), |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;
    assert!(revived.process_alive);
    assert_eq!(revived.spawn_generation, 2);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_cap_marks_module_failed_without_infinite_loop() {
    let server = TestServer::start().await;
    let max_restarts = 2;
    let supervisor = supervisor(&server, max_restarts, Duration::from_millis(10));
    let module_id = "fake-aft-cap";
    let module = supervisor
        .spawn(stub_spec(
            &server,
            module_id,
            [("FAKE_AFT_CRASH_AFTER_MS", "0")],
        ))
        .unwrap();

    let status = wait_for_status(&module, Duration::from_secs(2), |status| {
        status.state == ModuleState::Failed
    })
    .await;

    assert_eq!(status.restart_count, max_restarts);
    assert!(!status.process_alive);
    assert!(!status.live);
}

/// The window must survive the real supervise loop, not only the exit handler
/// the unit tests drive: an operator reading a stopped module gets the spent
/// budget, the cap, AND the span they are counted over from the same status and
/// terminal history a running daemon serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_module_stopped_by_its_budget_reports_the_window_it_was_counted_over() {
    let server = TestServer::start().await;
    let max_restarts = 2;
    let supervisor = supervisor(&server, max_restarts, Duration::from_millis(10));
    let module = supervisor
        .spawn(stub_spec(
            &server,
            "fake-aft-window",
            [("FAKE_AFT_CRASH_AFTER_MS", "0")],
        ))
        .unwrap();

    let status = wait_for_status(&module, Duration::from_secs(2), |status| {
        status.state == ModuleState::Failed
    })
    .await;

    assert_eq!(status.restart_count, max_restarts);
    assert_eq!(status.max_restarts, max_restarts);
    assert_eq!(
        status.restart_window,
        Duration::from_secs(600),
        "the count is only readable against the span it was counted over"
    );

    let history = module.terminal_history();
    let last = history
        .entries
        .last()
        .expect("the refused crash is retained in the terminal ring");
    assert_eq!(last.disposition, TerminalDisposition::Failed);
    assert_eq!(
        last.disposition_detail.as_deref(),
        Some("crash budget exhausted: max_restarts=2 within window_secs=600"),
        "the terminal record must say which limit stopped the module"
    );
}

/// The budget a module is spent against must travel with the count that spends
/// it, on the same status a reader gets.
///
/// Without the pair, an operator reading `restart_count` cannot tell a module
/// one crash from being disabled apart from one with headroom, and the
/// neighbouring health counter cannot supply it because that one returns to zero
/// on any successful probe. The configured value is asserted rather than the
/// default, so a `status()` hard-coding `DEFAULT_MAX_RESTARTS` would fail here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_the_restart_budget_alongside_the_count() {
    let server = TestServer::start().await;
    let max_restarts = 2;
    let supervisor = supervisor(&server, max_restarts, Duration::from_millis(10));
    let module = supervisor
        .spawn(stub_spec(
            &server,
            "fake-aft-budget",
            [("FAKE_AFT_CRASH_AFTER_MS", "0")],
        ))
        .unwrap();

    let fresh = module.status().unwrap();
    assert_eq!(
        fresh.max_restarts, max_restarts,
        "the configured budget must be reported, not the default"
    );

    let exhausted = wait_for_status(&module, Duration::from_secs(2), |status| {
        status.state == ModuleState::Failed
    })
    .await;

    // The count moved and the budget did not: a reader can see the module is out
    // of headroom, which a bare count of 2 cannot express.
    assert_eq!(exhausted.restart_count, max_restarts);
    assert_eq!(exhausted.max_restarts, max_restarts);
}

/// `start` (enable on an already-enabled module) must heal a Failed module: the
/// budget is exhausted, no in-band retry remains, and the operator's start IS the
/// recovery act. Regression for the 2026-07-14 aft outage, where set_enabled(true)
/// returned applied=false on the failed module and the only revival was
/// subc-probe --supervisor-restart from a human terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_enabled_true_revives_a_failed_module_and_resets_budget() {
    let server = TestServer::start().await;
    let max_restarts = 2;
    let supervisor = supervisor(&server, max_restarts, Duration::from_millis(10));
    let module_id = "fake-aft-failed-revive";
    let module = supervisor
        .spawn(stub_spec(
            &server,
            module_id,
            [("FAKE_AFT_CRASH_AFTER_MS", "0")],
        ))
        .unwrap();

    let failed = wait_for_status(&module, Duration::from_secs(2), |status| {
        status.state == ModuleState::Failed
    })
    .await;
    assert_eq!(failed.restart_count, max_restarts);

    // The instant-crash env is still in the spec, so the revived child will crash
    // again — but the revival itself must be applied (not the old no-op) and must
    // have reset the budget, observable as the state leaving Failed and the
    // restart counter dropping below the exhausted value.
    let applied = module
        .set_enabled(true)
        .await
        .expect("set_enabled must reach the supervision task");
    assert!(
        applied,
        "start on a failed module must apply the revival, not no-op"
    );
    let revived = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.state != ModuleState::Failed || status.restart_count < max_restarts
    })
    .await;
    assert!(
        revived.state != ModuleState::Failed || revived.restart_count < max_restarts,
        "revival must reset the exhausted budget"
    );
}

/// A clean child exit of an ENABLED module must not kill the supervision task:
/// the command channel has to stay open so a later operator restart can revive
/// the module. Regression for a production wedge where a module that exited 0
/// became permanently unrestartable ("supervisor command channel is closed")
/// and only a full daemon restart recovered it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_exit_keeps_supervision_task_alive_for_operator_restart() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 2, Duration::from_millis(10));
    let module_id = "fake-aft-clean-exit";
    let module = supervisor
        .spawn(stub_spec(
            &server,
            module_id,
            [("FAKE_AFT_CLEAN_EXIT_AFTER_MS", "100")],
        ))
        .unwrap();

    let stopped = wait_for_status(&module, Duration::from_secs(2), |status| {
        status.state == ModuleState::Stopped && !status.process_alive
    })
    .await;
    assert!(!stopped.live);
    assert_current_process_facts_cleared(&stopped);

    // The load-bearing assertion: the supervision task must still answer
    // commands after the clean exit, and restart must fully revive the module.
    module
        .restart(None)
        .await
        .expect("restart after clean exit must reach a live supervision task");

    let running = wait_for_status(&module, Duration::from_secs(5), |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;
    assert!(running.process_alive);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_stops_child_and_releases_registration() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-drain";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    module.drain().await.unwrap();

    let status = wait_for_status(&module, Duration::from_secs(1), |status| {
        status.state == ModuleState::Stopped && !status.registration_active
    })
    .await;
    assert!(!status.process_alive);
    assert!(!status.live);
    assert!(server.registry.get_module(module_id).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_requires_process_alive_and_active_registration() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-live";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let registration = server
        .registry
        .get_module(module_id)
        .unwrap()
        .expect("spawn_stub waits for registration");
    let live = module.status().unwrap();
    assert_eq!(live.state, ModuleState::Running);
    assert!(live.process_alive);
    assert!(live.registration_active);
    assert!(live.live);

    server
        .registry
        .deregister_connection(registration.connection_id)
        .unwrap();

    let not_live = module.status().unwrap();
    assert_eq!(not_live.state, ModuleState::Running);
    assert!(not_live.process_alive);
    assert!(!not_live.registration_active);
    assert!(!not_live.live);

    module.stop().await.unwrap();
}

fn supervisor(server: &TestServer, max_restarts: u32, backoff: Duration) -> Supervisor {
    Supervisor::new(
        Arc::clone(&server.registry),
        RestartPolicy::new(max_restarts, backoff),
    )
    .with_drain_timeout(Duration::from_millis(25))
    .with_connection_file_path(server.connection_file_path.clone())
}

async fn spawn_stub(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
) -> SupervisedModule {
    spawn_stub_with_env(
        server,
        supervisor,
        module_id,
        std::iter::empty::<(&str, &str)>(),
    )
    .await
}

async fn spawn_stub_with_env<'a>(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
    extra_env: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> SupervisedModule {
    let module = supervisor
        .spawn(stub_spec(server, module_id, extra_env))
        .unwrap();
    // Generous setup hang-guard (deadlock detector, not a latency bound): a
    // spawn/connect/auth/register constellation under heavy parallel CI load
    // must not trip it. See forwarding.rs SETUP_TIMEOUT.
    wait_for_registration(&server.registry, module_id, Duration::from_secs(10)).await;
    module
}

fn stub_spec<'a>(
    _server: &TestServer,
    module_id: &str,
    extra_env: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> ModuleSpec {
    // ASSERT THE SILENT PRECONDITION EVERY CATALOG-REGISTRATION TEST RESTS ON.
    //
    // A test that asserts "the module registered as `module_id`" is vacuous if
    // `module_id` equals the stub's compiled fallback: a stub that never
    // received its env would register under exactly the string being asserted,
    // so pass and fail become the same observation. Every id in the suite
    // differs from the fallback today, which is why those assertions are real --
    // but that held only because of how they happen to be NAMED, and nothing
    // stated it. The day someone picks the fallback string, the tests keep
    // passing and stop proving anything.
    //
    // Asserted rather than commented for the reason CKE2E gave when they hit the
    // live form of this: a property the suite DEPENDS ON but does not state is a
    // silent precondition, and the day it stops holding is exactly the day
    // nothing complains.
    assert_ne!(
        module_id, "fake-aft",
        "test module id equals the stub's compiled fallback, so a catalog \
         assertion on it cannot distinguish a delivered id from an undelivered \
         one; pick a different id"
    );
    let mut env = vec![("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string())];
    env.extend(
        extra_env
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string())),
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

/// The case #7 was filed about: a module that dies with its cause on stderr.
///
/// The claustrum incident had `exit_code: 1` and the reason -- a missing config
/// section -- only in the text, which was gone from the journal by the time
/// anyone looked. This asserts the text is recoverable from the supervisor after
/// the process is dead, with no log file in the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_module_leaves_its_stderr_readable_from_the_supervisor() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 0, Duration::from_millis(10));
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: "stderr-tail-crasher".to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
            args: Vec::new(),
            env: vec![
                (
                    "FAKE_AFT_STDERR_LINE".to_string(),
                    "config error: missing top-level `storage`".to_string(),
                ),
                ("FAKE_AFT_EXIT_CODE".to_string(), "1".to_string()),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();

    let status = wait_for_status(&module, Duration::from_secs(5), |status| {
        status.state == ModuleState::Failed
    })
    .await;
    assert_eq!(
        status.last_exit.as_ref().and_then(|exit| exit.code),
        Some(1),
        "precondition: the module should have exited non-zero"
    );

    let tail = wait_for_tail(&module, Duration::from_secs(5), |tail| {
        matches!(tail.capture, CaptureState::Captured)
            && tail.entries.iter().any(
                |entry| matches!(entry, TailEntry::Line { text, .. } if text.contains("missing top-level `storage`")),
            )
    })
    .await;
    assert!(
        matches!(tail.capture, CaptureState::Captured),
        "a spawned module must report captured, not an empty tail that reads as silence"
    );

    let lines: Vec<&str> = tail
        .entries
        .iter()
        .filter_map(|entry| match entry {
            TailEntry::Line { text, .. } => Some(text.as_str()),
            TailEntry::ProcessStart => None,
        })
        .collect();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("missing top-level `storage`")),
        "the cause of the exit was not recoverable from the tail; got {lines:?}"
    );
}

/// A module that exits cleanly having printed nothing must not look like one
/// nobody was listening to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_module_reports_captured_and_empty_rather_than_uncaptured() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 0, Duration::from_millis(10));
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: "stderr-tail-silent".to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
            args: Vec::new(),
            env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "3".to_string())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();

    wait_for_status(&module, Duration::from_secs(5), |status| {
        status.state == ModuleState::Failed
    })
    .await;

    let tail = wait_for_tail(&module, Duration::from_secs(5), |tail| {
        matches!(tail.capture, CaptureState::Captured) && tail.entries.is_empty()
    })
    .await;
    assert!(
        matches!(tail.capture, CaptureState::Captured),
        "silence and absence must be distinguishable; got {:?}",
        tail.capture
    );
    assert!(
        tail.entries.is_empty(),
        "expected no lines from a module that printed nothing; got {:?}",
        tail.entries
    );
}

/// The tail has to outlive the process whose death it explains.
///
/// A ring recreated per spawn would be empty exactly when asked, and the restart
/// A supervised module INHERITS the daemon's environment.
///
/// The parent's own HOME is the fixture, so this needs no environment mutation:
/// whatever this process has, the child must have. With HOME and XDG_DATA_HOME
/// both unset, `default_data_home()` returns the RELATIVE `.local/share` and
/// every module derives a store path against its own CWD; `ck` also loses the
/// daemon, because connection-file discovery reads HOME and XDG_RUNTIME_DIR.
/// That was live from 0.17.41 to 0.18.3 via `env_clear()`, reported as #104.
///
/// The witness is the child reporting the variable directly (FAKE_AFT_ECHO_ENV)
/// rather than the child's downstream behaviour: a module that silently degrades
/// without HOME looks exactly like one that was configured.
///
/// The companion arm -- that ambient CK_LOG does NOT reach the child -- is a unit
/// test on the command plan (`supervise::tests`), because asserting it here would
/// require mutating this process's environment, which `forbid(unsafe_code)`
/// rightly refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supervised_module_inherits_the_parent_environment() {
    // The variable is chosen by MEANING, not spelling: whichever one this
    // platform's `default_data_home()` reads to find the user's home. HOME is a
    // POSIX concept that does not exist on Windows, where the resolver falls
    // back to APPDATA then USERPROFILE. Asserting "HOME" on every platform is a
    // true assertion about a variable the platform never had -- which is how
    // this test first went red on the Windows leg.
    #[cfg(windows)]
    let home_var = "USERPROFILE";
    #[cfg(not(windows))]
    let home_var = "HOME";

    let parent_home =
        std::env::var(home_var).unwrap_or_else(|_| panic!("this test needs {home_var} to inherit"));

    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: "env-inherit-probe".to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
            args: Vec::new(),
            env: vec![
                ("FAKE_AFT_ECHO_ENV".to_string(), home_var.to_string()),
                ("FAKE_AFT_EXIT_CODE".to_string(), "0".to_string()),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();

    let prefix = format!("echo-env {home_var}=");
    let tail = wait_for_tail(&module, Duration::from_secs(5), |tail| {
        tail.entries
            .iter()
            .any(|entry| matches!(entry, TailEntry::Line { text, .. } if text.starts_with(&prefix)))
    })
    .await;

    let observed = tail
        .entries
        .iter()
        .find_map(|entry| match entry {
            TailEntry::Line { text, .. } if text.starts_with(&prefix) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {prefix} line"));

    assert_eq!(
        observed,
        format!("{prefix}{parent_home}"),
        "a supervised module must inherit the user's home variable; without it \
         default_data_home() is relative and every module derives its store \
         against its own CWD (#104)"
    );
}

/// boundary has to be visible in-band -- which side of a restart a line falls on
/// is unanswerable from a count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stderr_from_before_a_restart_survives_with_a_marked_boundary() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 3, Duration::from_millis(10));
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: "stderr-tail-looper".to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
            args: Vec::new(),
            env: vec![
                ("FAKE_AFT_STDERR_LINE".to_string(), "boot {pid}".to_string()),
                ("FAKE_AFT_EXIT_CODE".to_string(), "1".to_string()),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();

    let tail = wait_for_tail(&module, Duration::from_secs(5), |tail| {
        let boots = tail
            .entries
            .iter()
            .filter(
                |entry| matches!(entry, TailEntry::Line { text, .. } if text.starts_with("boot ")),
            )
            .count();
        let process_starts = tail
            .entries
            .iter()
            .filter(|entry| matches!(entry, TailEntry::ProcessStart))
            .count()
            >= 2;
        boots >= 2 && process_starts
    })
    .await;

    let boots = tail
        .entries
        .iter()
        .filter(|entry| matches!(entry, TailEntry::Line { text, .. } if text.starts_with("boot ")))
        .count();
    assert!(
        boots >= 2,
        "output from before the restart was lost; got {:?}",
        tail.entries
    );
    assert!(
        tail.entries
            .iter()
            .filter(|entry| matches!(entry, TailEntry::ProcessStart))
            .count()
            >= 2,
        "restart boundaries were lost; got {:?}",
        tail.entries
    );
}

/// A process that exits while a descendant still holds its stderr pipe.
///
/// The restart must not wait on that descendant, the tail must say it is
/// incomplete while the pipe is open, and what the descendant writes later
/// belongs to the process that spawned it: it lands in front of the next
/// process's boundary, never after it. The orphan writes 3s after its parent
/// exits, far past the supervisor's wait, so a restart observed before the
/// orphan's line is a restart the held pipe did not delay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_stderr_pipe_marks_the_tail_incomplete_and_its_late_output_stays_with_its_process() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: "stderr-tail-wedged-pump".to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
            args: Vec::new(),
            env: vec![
                ("FAKE_AFT_STDERR_LINE".to_string(), "old-start".to_string()),
                ("FAKE_AFT_EXIT_CODE".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_ORPHAN_WRITER_DELAY_MS".to_string(),
                    "3000".to_string(),
                ),
                (
                    "FAKE_AFT_ORPHAN_WRITER_LINE".to_string(),
                    "old-trailing".to_string(),
                ),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();

    let restarted = wait_for_tail(&module, Duration::from_secs(5), |tail| {
        tail.entries
            .iter()
            .any(|entry| matches!(entry, TailEntry::ProcessStart))
    })
    .await;
    assert!(
        !restarted
            .entries
            .iter()
            .any(|entry| matches!(entry, TailEntry::Line { text, .. } if text == "old-trailing")),
        "the restart waited for the descendant holding the pipe: {:?}",
        restarted.entries
    );
    match &restarted.capture {
        CaptureState::Incomplete { reason } => assert!(
            reason.contains("had not reached EOF"),
            "the reason must name the open pipe: {reason}"
        ),
        other => panic!("expected Incomplete while a descendant holds the pipe, got {other:?}"),
    }

    // Both processes spawn an orphan, so each section ends with its own
    // orphan's line once both pipes close.
    let settled = wait_for_tail(&module, Duration::from_secs(15), |tail| {
        matches!(tail.capture, CaptureState::Captured)
            && tail
                .entries
                .iter()
                .filter(
                    |entry| matches!(entry, TailEntry::Line { text, .. } if text == "old-trailing"),
                )
                .count()
                == 2
    })
    .await;
    let sections: Vec<Vec<&str>> = settled
        .entries
        .split(|entry| matches!(entry, TailEntry::ProcessStart))
        .map(|section| {
            section
                .iter()
                .filter_map(|entry| match entry {
                    TailEntry::Line { text, .. } => Some(text.as_str()),
                    TailEntry::ProcessStart => None,
                })
                .collect()
        })
        .collect();
    assert_eq!(
        sections,
        vec![
            vec!["old-start", "old-trailing"],
            vec!["old-start", "old-trailing"]
        ],
        "a descendant's late output crossed into its successor's section"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_stdout_and_stderr_reach_the_capture_file_while_only_stderr_reaches_the_ring() {
    let server = TestServer::start().await;
    let capture = TestTempDir::new("child-output-capture");
    let logs_dir = capture.join("logs");
    let supervisor =
        supervisor(&server, 0, Duration::from_millis(10)).with_capture_logs_dir(&logs_dir);
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: "two-pipe-capture".to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_log-child-fixture")),
            args: Vec::new(),
            env: vec![
                (
                    "LOG_CHILD_STDOUT".to_string(),
                    "stdout-complete-line".to_string(),
                ),
                (
                    "LOG_CHILD_STDERR".to_string(),
                    "stderr-complete-line".to_string(),
                ),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();

    let tail = wait_for_tail(&module, Duration::from_secs(5), |tail| {
        matches!(tail.capture, CaptureState::Captured)
            && tail.entries.iter().any(
                |entry| matches!(entry, TailEntry::Line { text, .. } if text == "stderr-complete-line"),
            )
    })
    .await;
    let path = logs_dir.join("two-pipe-capture.stderr.log");
    let deadline = Instant::now() + Duration::from_secs(5);
    let contents = loop {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if contents.contains("stdout-complete-line")
                && contents.contains("stderr-complete-line")
            {
                break contents;
            }
        }
        assert!(
            Instant::now() < deadline,
            "capture file did not receive both pipes"
        );
        sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        contents.lines().collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["stderr-complete-line", "stdout-complete-line"]),
        "each complete source line must remain intact"
    );
    let ring_lines = tail
        .entries
        .iter()
        .filter_map(|entry| match entry {
            TailEntry::Line { text, .. } => Some(text.as_str()),
            TailEntry::ProcessStart => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ring_lines, ["stderr-complete-line"]);
}

/// The two lines above arrive one after the other, so they cannot tear however
/// the forwarder is written: that test proves DELIVERY. Tearing needs both pipes
/// writing at once, which is the shape a supervisor merging stdout and stderr
/// into one file actually meets, so the framing property needs its own arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_child_pipes_never_tear_a_line_in_the_capture_file() {
    const BURST: usize = 150;
    let server = TestServer::start().await;
    let capture = TestTempDir::new("child-output-burst");
    let logs_dir = capture.join("logs");
    let supervisor =
        supervisor(&server, 0, Duration::from_millis(10)).with_capture_logs_dir(&logs_dir);
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: "two-pipe-burst".to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_log-child-fixture")),
            args: Vec::new(),
            env: vec![("LOG_CHILD_BURST".to_string(), BURST.to_string())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();

    let capture_path = logs_dir.join("two-pipe-burst.stderr.log");
    let deadline = Instant::now() + Duration::from_secs(20);
    let contents = loop {
        if let Ok(contents) = std::fs::read_to_string(&capture_path) {
            if contents.lines().count() >= BURST * 2 {
                break contents;
            }
        }
        assert!(
            Instant::now() < deadline,
            "capture file never received {} lines from the two pipes",
            BURST * 2
        );
        sleep(Duration::from_millis(20)).await;
    };

    for line in contents.lines() {
        // A whole line is `<lane>-<4 digits>-` followed by 64 identical
        // padding characters; any split leaves a short line or two prefixes in
        // one. Checked structurally rather than by counting characters, since
        // the lane names themselves contain the padding letters.
        let intact = match line.split_once('-') {
            Some((lane @ ("out" | "err"), rest)) => {
                let padding = if lane == "out" { b'o' } else { b'e' };
                rest.split_once('-').is_some_and(|(index, pad)| {
                    index.len() == 4
                        && index.bytes().all(|b| b.is_ascii_digit())
                        && pad.len() == 64
                        && pad.bytes().all(|b| b == padding)
                })
            }
            _ => false,
        };
        assert!(
            intact,
            "a torn line proves the forwarder split a write between the two pipes: {line:?}"
        );
    }
    assert_eq!(
        contents.lines().count(),
        BURST * 2,
        "every line from both pipes must arrive exactly once"
    );

    module.stop().await.unwrap();
}

async fn wait_for_tail(
    module: &SupervisedModule,
    wait: Duration,
    matches: impl Fn(&StderrTailSnapshot) -> bool,
) -> StderrTailSnapshot {
    let deadline = Instant::now() + wait;
    loop {
        let tail = module.stderr_tail(None, None);
        if matches(&tail) {
            return tail;
        }
        if Instant::now() >= deadline {
            panic!(
                "module {} did not reach the expected stderr tail within {wait:?}; last: {tail:?}",
                module.module_id()
            );
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

struct SpawnEventHarness {
    server: TestServer,
    supervisor: Supervisor,
    handle: SupervisorHandle,
    incarnation: String,
}

impl SpawnEventHarness {
    async fn start(name: &str, capacity: Option<usize>) -> Self {
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let handle = SupervisorHandle::new();
        if let Some(capacity) = capacity {
            handle.set_spawn_event_capacity_for_test(capacity);
        }
        let daemon = start_test_daemon_with_process_liveness_and_supervisor(
            name,
            process_liveness.clone(),
            handle.clone(),
        )
        .await;
        let server = TestServer { daemon };
        let incarnation = format!("{name}-incarnation");
        let supervisor = Supervisor::new(
            Arc::clone(&server.registry),
            RestartPolicy::new(0, Duration::ZERO),
        )
        .with_process_liveness(process_liveness)
        .with_handle(handle.clone())
        .with_connection_file_path(server.connection_file_path.clone())
        .with_terminal_journal(
            server.temp_dir.join("spawn-events-terminals.jsonl"),
            incarnation.clone(),
        )
        .with_drain_timeout(Duration::from_millis(25));
        Self {
            server,
            supervisor,
            handle,
            incarnation,
        }
    }

    async fn client(&self) -> TcpStream {
        connect_authed_client(&self.server.connection_file_path)
            .await
            .unwrap()
    }

    async fn spawn(&self, module_id: &str) -> SupervisedModule {
        spawn_stub(&self.server, &self.supervisor, module_id).await
    }
}

fn spawn_control_request(corr: u64, request: ClientControlRequest) -> subc_daemon::Frame {
    subc_daemon::Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap()
}

async fn send_spawn_request(client: &mut TcpStream, corr: u64, request: ClientControlRequest) {
    write_frame(client, &spawn_control_request(corr, request))
        .await
        .unwrap();
    client.flush().await.unwrap();
}

async fn spawn_snapshot(client: &mut TcpStream, corr: u64) -> SpawnSnapshot {
    send_spawn_request(
        client,
        corr,
        ClientControlRequest::SupervisorSpawnSnapshot {},
    )
    .await;
    let frame = timeout(Duration::from_secs(5), read_frame(client))
        .await
        .expect("spawn snapshot response timed out")
        .unwrap()
        .expect("connection closed before spawn snapshot response");
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.corr, corr);
    match serde_json::from_slice(&frame.body).unwrap() {
        ClientControlResponse::SupervisorSpawnSnapshot { snapshot } => snapshot,
        other => panic!("unexpected spawn snapshot response: {other:?}"),
    }
}

async fn spawn_event(client: &mut TcpStream, corr: u64) -> SpawnEvent {
    let frame = timeout(Duration::from_secs(5), read_frame(client))
        .await
        .expect("spawn event timed out")
        .unwrap()
        .expect("connection closed before spawn event");
    assert_eq!(frame.header.ty, FrameType::StreamData);
    assert_eq!(frame.header.corr, corr);
    serde_json::from_slice(&frame.body).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_snapshot_then_subscribe_has_no_overlap_or_gap() {
    let harness = SpawnEventHarness::start("spawn-snapshot-subscribe", None).await;
    let before = harness.spawn("spawn-before-snapshot").await;
    let mut client = harness.client().await;
    send_spawn_request(&mut client, 70, ClientControlRequest::SupervisorList {}).await;
    let list = timeout(Duration::from_secs(5), read_frame(&mut client))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let list: ClientControlResponse = serde_json::from_slice(&list.body).unwrap();
    let ClientControlResponse::SupervisorList { modules, .. } = list else {
        panic!("unexpected supervisor.list response: {list:?}");
    };
    assert_eq!(modules[0].spawn_generation, Some(1));

    let snapshot = spawn_snapshot(&mut client, 71).await;
    assert_eq!(snapshot.ring_bound, 4096);
    assert!(snapshot
        .live
        .iter()
        .any(|live| live.module_id == "spawn-before-snapshot"));

    send_spawn_request(
        &mut client,
        72,
        ClientControlRequest::SupervisorSpawnSubscribe {
            since: Some(snapshot.cursor),
        },
    )
    .await;
    let after = harness.spawn("spawn-after-snapshot").await;
    let event = spawn_event(&mut client, 72).await;
    assert_eq!(event.kind, SpawnEventKind::Spawned);
    assert_eq!(event.module_id, "spawn-after-snapshot");
    assert_ne!(event.module_id, "spawn-before-snapshot");

    after.stop().await.unwrap();
    before.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_event_sequences_are_dense_across_spawn_and_exit_paths() {
    let harness = SpawnEventHarness::start("spawn-density", None).await;
    for index in 0..3 {
        let module = harness.spawn(&format!("spawn-density-{index}")).await;
        module.stop().await.unwrap();
    }
    let mut client = harness.client().await;
    send_spawn_request(
        &mut client,
        81,
        ClientControlRequest::SupervisorSpawnSubscribe {
            since: Some(SpawnCursor {
                daemon_incarnation: harness.incarnation.clone(),
                seq: 0,
            }),
        },
    )
    .await;
    let mut events = Vec::new();
    for _ in 0..6 {
        events.push(spawn_event(&mut client, 81).await);
    }
    assert_eq!(events.len(), 6);
    for pair in events.windows(2) {
        assert_eq!(pair[1].cursor.seq, pair[0].cursor.seq + 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_subscribe_replays_strictly_after_cursor_then_continues_live() {
    let harness = SpawnEventHarness::start("spawn-replay-live", None).await;
    let first = harness.spawn("spawn-replay-first").await;
    first.stop().await.unwrap();
    let mut client = harness.client().await;
    send_spawn_request(
        &mut client,
        91,
        ClientControlRequest::SupervisorSpawnSubscribe {
            since: Some(SpawnCursor {
                daemon_incarnation: harness.incarnation.clone(),
                seq: 1,
            }),
        },
    )
    .await;
    let replay = spawn_event(&mut client, 91).await;
    assert_eq!(replay.cursor.seq, 2);
    assert_eq!(replay.kind, SpawnEventKind::Exited);

    let live = harness.spawn("spawn-replay-live-next").await;
    let next = spawn_event(&mut client, 91).await;
    assert_eq!(next.cursor.seq, 3);
    assert_eq!(next.module_id, "spawn-replay-live-next");
    live.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_subscribe_refuses_stale_and_foreign_cursors_with_typed_details() {
    let harness = SpawnEventHarness::start("spawn-refusal", Some(2)).await;
    let first = harness.spawn("spawn-refusal-first").await;
    first.stop().await.unwrap();
    let second = harness.spawn("spawn-refusal-second").await;
    second.stop().await.unwrap();
    let mut client = harness.client().await;

    send_spawn_request(
        &mut client,
        101,
        ClientControlRequest::SupervisorSpawnSubscribe {
            since: Some(SpawnCursor {
                daemon_incarnation: harness.incarnation.clone(),
                seq: 0,
            }),
        },
    )
    .await;
    let stale = timeout(Duration::from_secs(5), read_frame(&mut client))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(stale.header.ty, FrameType::Error);
    let stale: ErrorBody = serde_json::from_slice(&stale.body).unwrap();
    assert_eq!(stale.code, "spawn_cursor_too_old");
    assert_eq!(stale.detail.unwrap()["oldest_retained_cursor"]["seq"], 3);

    send_spawn_request(
        &mut client,
        102,
        ClientControlRequest::SupervisorSpawnSubscribe {
            since: Some(SpawnCursor {
                daemon_incarnation: "foreign-incarnation".to_string(),
                seq: 4,
            }),
        },
    )
    .await;
    let foreign = timeout(Duration::from_secs(5), read_frame(&mut client))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(foreign.header.ty, FrameType::Error);
    let foreign: ErrorBody = serde_json::from_slice(&foreign.body).unwrap();
    assert_eq!(foreign.code, "spawn_cursor_incarnation_mismatch");
    assert_eq!(
        foreign.detail.unwrap()["current_daemon_incarnation"],
        harness.incarnation
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigkill_spawn_event_is_fact_only_while_terminal_keeps_disposition() {
    let harness = SpawnEventHarness::start("spawn-fact-only", None).await;
    let module = harness.spawn("spawn-fact-only-module").await;
    let status = module.status().unwrap();
    let pid = status.pid.expect("spawned module has pid");
    let mut client = harness.client().await;
    let snapshot = spawn_snapshot(&mut client, 111).await;
    send_spawn_request(
        &mut client,
        112,
        ClientControlRequest::SupervisorSpawnSubscribe {
            since: Some(snapshot.cursor),
        },
    )
    .await;
    let kill = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(kill.success());
    let frame = timeout(Duration::from_secs(5), read_frame(&mut client))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(raw["kind"], "exited");
    assert_eq!(raw["exit_signal"], 9);
    assert!(raw.get("reason").is_none());
    let event: SpawnEvent = serde_json::from_value(raw).unwrap();
    assert_eq!(event.pid, pid);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let history = module.durable_terminal_history();
        if let Some(entry) = history.entries.last() {
            assert_eq!(entry.exit_signal, Some(9));
            assert_eq!(entry.disposition, TerminalDisposition::Failed);
            break;
        }
        assert!(Instant::now() < deadline, "terminal record did not arrive");
        sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_spawn_subscription_ends_its_stream() {
    let harness = SpawnEventHarness::start("spawn-subscriber-cancel", None).await;
    let mut client = harness.client().await;
    send_spawn_request(
        &mut client,
        120,
        ClientControlRequest::SupervisorSpawnSubscribe { since: None },
    )
    .await;
    let cancel = subc_daemon::Frame::build(
        FrameType::Cancel,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        120,
        Vec::new(),
    )
    .unwrap();
    write_frame(&mut client, &cancel).await.unwrap();
    client.flush().await.unwrap();
    let end = timeout(Duration::from_secs(5), read_frame(&mut client))
        .await
        .expect("cancelled subscription did not end")
        .unwrap()
        .expect("connection closed before StreamEnd");
    assert_eq!(end.header.ty, FrameType::StreamEnd);
    assert_eq!(end.header.corr, 120);
    assert_eq!(harness.handle.spawn_subscriber_count_for_test(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_spawn_subscriber_is_removed_before_the_next_emit() {
    let harness = SpawnEventHarness::start("spawn-subscriber-cleanup", None).await;
    let mut client = harness.client().await;
    send_spawn_request(
        &mut client,
        121,
        ClientControlRequest::SupervisorSpawnSubscribe { since: None },
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(2);
    while harness.handle.spawn_subscriber_count_for_test() != 1 {
        assert!(Instant::now() < deadline, "subscriber was not registered");
        sleep(Duration::from_millis(10)).await;
    }
    drop(client);
    let deadline = Instant::now() + Duration::from_secs(2);
    while harness.handle.spawn_subscriber_count_for_test() != 0 {
        assert!(
            Instant::now() < deadline,
            "closed subscriber remained registered"
        );
        sleep(Duration::from_millis(10)).await;
    }
    let module = harness.spawn("spawn-after-subscriber-close").await;
    assert_eq!(module.status().unwrap().spawn_generation, 1);
    module.stop().await.unwrap();
}
