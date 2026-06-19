use std::{ops::Deref, path::PathBuf, sync::Arc, time::Duration};

use subc_core::{
    ModuleSpec, ModuleState, ModuleStatus, Registry, RestartPolicy, SupervisedModule, Supervisor,
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
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;
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
