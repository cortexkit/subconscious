use std::{ops::Deref, path::PathBuf, sync::Arc, time::Duration};

use subc_core::{
    stderr_tail::{CaptureState, StderrTailSnapshot, TailEntry},
    ModuleSpec, ModuleState, ModuleStatus, Registry, RestartPolicy, SuperviseError,
    SupervisedModule, Supervisor,
};
use tokio::time::{sleep, Instant};

mod common;
use common::TestDaemon;

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
    let missing_program = std::env::temp_dir().join(format!(
        "subc-missing-enable-program-{}",
        std::process::id()
    ));
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

    // Per-test copy of the stub so deleting it cannot affect parallel tests
    // (pid+nonce naming per the house temp convention; no tempfile dep here).
    let stub_copy = std::env::temp_dir().join(format!(
        "fake-aft-stub-copy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::copy(env!("CARGO_BIN_EXE_fake-aft-stub"), &stub_copy).unwrap();
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

    // And Failed is the revivable state: restoring the program and re-enabling
    // heals it, which is the property Restarting-stranding denied the operator.
    std::fs::copy(env!("CARGO_BIN_EXE_fake-aft-stub"), &stub_copy).unwrap();
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
    }
}

async fn wait_for_registration(
    registry: &Registry,
    module_id: &str,
    wait: Duration,
) -> subc_core::ModuleRegistration {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wedged_old_stderr_pump_is_stopped_before_the_next_restart_boundary() {
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
                    "1000".to_string(),
                ),
                (
                    "FAKE_AFT_ORPHAN_WRITER_LINE".to_string(),
                    "old-trailing".to_string(),
                ),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
        })
        .unwrap();

    let tail = wait_for_tail(&module, Duration::from_secs(5), |tail| {
        matches!(tail.capture, CaptureState::Incomplete { .. })
            && tail
                .entries
                .iter()
                .any(|entry| matches!(entry, TailEntry::ProcessStart))
    })
    .await;
    assert!(
        tail.entries
            .iter()
            .any(|entry| matches!(entry, TailEntry::Line { text, .. } if text == "old-start"),),
        "the initial process output was not retained: {:?}",
        tail.entries
    );

    sleep(Duration::from_millis(1200)).await;
    let tail = module.stderr_tail(None, None);
    assert!(
        !tail
            .entries
            .iter()
            .any(|entry| matches!(entry, TailEntry::Line { text, .. } if text == "old-trailing"),),
        "old output crossed the restart boundary: {:?}",
        tail.entries
    );
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
