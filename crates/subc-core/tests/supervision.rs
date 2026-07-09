use std::{ops::Deref, path::PathBuf, sync::Arc, time::Duration};

use subc_core::{
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
    wait_for_status(&module, Duration::from_secs(3), |status| {
        status.state == ModuleState::Disabled && !status.process_alive
    })
    .await;

    let restart_err = module
        .restart()
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
        [("FAKE_AFT_CRASH_AFTER_MS", "250")],
    )
    .await;

    let crashed = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.restart_count >= 1 && status.state == ModuleState::Running && status.live
    })
    .await;
    assert!(crashed.restart_count >= 1);

    module.restart().await.unwrap();

    let restarted = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.restart_count == 0 && status.state == ModuleState::Running && status.live
    })
    .await;
    assert_eq!(restarted.restart_count, 0);
    assert!(restarted.process_alive);
    assert!(restarted.registration_active);

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
            [("FAKE_AFT_CLEAN_EXIT_AFTER_MS", "0")],
        ))
        .unwrap();

    let stopped = wait_for_status(&module, Duration::from_secs(2), |status| {
        status.state == ModuleState::Stopped && !status.process_alive
    })
    .await;
    assert!(!stopped.live);

    // The load-bearing assertion: the supervision task must still answer
    // commands after the clean exit, and restart must fully revive the module.
    module
        .restart()
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
