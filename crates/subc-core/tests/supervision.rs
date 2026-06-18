use std::{
    fs,
    path::PathBuf,
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use subc_core::{
    serve_listener, ControlHandler, ModuleSpec, ModuleState, ModuleStatus, Registry, RestartPolicy,
    Router, SupervisedModule, Supervisor, SUBC_SOCKET_ENV,
};
use tokio::{
    net::UnixListener,
    task::JoinHandle,
    time::{sleep, Instant},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestServer {
    registry: Arc<Registry>,
    socket_path: PathBuf,
    temp_dir: PathBuf,
    task: JoinHandle<Result<(), subc_core::ServerError>>,
}

impl TestServer {
    fn start() -> Self {
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let socket_path = temp_dir.join("s.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let registry = Arc::new(Registry::default());
        let control = Arc::new(ControlHandler::new(Arc::clone(&registry)));
        let router = Arc::new(Router::with_control_handler(control));
        let task = tokio::spawn(serve_listener(listener, router));

        Self {
            registry,
            socket_path,
            temp_dir,
            task,
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_registers_stub_and_reports_running() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-spawn";
    let module = supervisor.spawn(stub_spec(&server, module_id, [])).unwrap();

    let registration =
        wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;
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
    let server = TestServer::start();
    let supervisor = supervisor(&server, 5, Duration::from_millis(20));
    let module_id = "fake-aft-restart";
    let module = supervisor
        .spawn(stub_spec(
            &server,
            module_id,
            [("FAKE_AFT_CRASH_AFTER_MS", "250")],
        ))
        .unwrap();

    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

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
    let server = TestServer::start();
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
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-drain";
    let module = supervisor.spawn(stub_spec(&server, module_id, [])).unwrap();

    let registration =
        wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;
    assert!(server
        .registry
        .is_channel_active(registration.channels[0])
        .unwrap());

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
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-live";
    let module = supervisor.spawn(stub_spec(&server, module_id, [])).unwrap();

    let registration =
        wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;
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
}

fn stub_spec<'a>(
    server: &TestServer,
    module_id: &str,
    extra_env: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> ModuleSpec {
    let mut env = vec![
        (
            SUBC_SOCKET_ENV.to_string(),
            server.socket_path.to_string_lossy().into_owned(),
        ),
        ("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()),
    ];
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

fn unique_temp_dir() -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sc-{}-{nonce}", process::id()))
}
