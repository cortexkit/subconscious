use super::*;
use subc_control::{
    ClientControlRequest, ClientControlResponse, ModuleProtocol, SpawnSnapshot, SupervisorEntry,
};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::{read_frame, write_frame};
use tokio::io::AsyncWriteExt;

fn configured(module_id: &str, program: PathBuf, enabled: bool) -> ConfiguredModule {
    ConfiguredModule {
        module_id: module_id.into(),
        program,
        args: Vec::new(),
        env: Vec::new(),
        log: None,
        enabled,
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        health: HealthConfig::default(),
        drain_timeout_ms: Some(123),
        route_bind_relay_timeout_ms: None,
        restart: RestartPolicy::new(7, Duration::from_millis(321)),
    }
}

fn fake_aft_stub_path() -> PathBuf {
    let stub = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(if cfg!(windows) {
            "fake-aft-stub.exe"
        } else {
            "fake-aft-stub"
        });
    assert!(
        stub.is_file(),
        "build fake-aft-stub before daemon lib tests"
    );
    stub
}

async fn client(path: &Path) -> TcpStream {
    let info = connection_file::read(path).unwrap();
    let mut stream = connect_from_info(&info).await.unwrap();
    authenticate_client(&mut stream, &info, Duration::from_secs(5))
        .await
        .unwrap();
    stream
}

async fn rpc(stream: &mut TcpStream, request: ClientControlRequest) -> ClientControlResponse {
    let described = format!("{request:?}");
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
    stream.flush().await.unwrap();
    let reply = match timeout(Duration::from_secs(5), read_frame(stream)).await {
        Ok(reply) => reply.unwrap().unwrap(),
        Err(_) => panic!(
            "no daemon reply within 5s to {described}\n{}",
            DaemonLog::dump_current()
        ),
    };
    assert_eq!(reply.header.channel, 0);
    assert_eq!(reply.header.corr, 1);
    assert_eq!(
        reply.header.ty,
        FrameType::Response,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    serde_json::from_slice(&reply.body).unwrap()
}

/// The in-process daemon's log lines, kept so a test that times out waiting
/// for the daemon can print what the daemon was doing. These tests run on a
/// current-thread runtime, so every daemon task logs on the test's own thread
/// and a thread-local subscriber sees it; work the daemon hands to other
/// threads (blocking lookups, child reaping) is not captured.
#[derive(Clone, Default)]
struct DaemonLog(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

thread_local! {
    static CURRENT_DAEMON_LOG: std::cell::RefCell<Option<DaemonLog>> =
        const { std::cell::RefCell::new(None) };
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for DaemonLog {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Line(String);
        impl tracing::field::Visit for Line {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                let _ = write!(self.0, " {}={value:?}", field.name());
            }
        }
        let metadata = event.metadata();
        let mut line = Line(format!("{} {}:", metadata.level(), metadata.target()));
        event.record(&mut line);
        self.0.lock().unwrap().push(line.0);
    }
}

impl DaemonLog {
    /// Capture this thread's log events until the guard drops.
    fn install() -> (DaemonLog, impl Drop) {
        use tracing_subscriber::layer::SubscriberExt;
        let log = DaemonLog::default();
        CURRENT_DAEMON_LOG.with(|current| *current.borrow_mut() = Some(log.clone()));
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(log.clone()));
        (log, guard)
    }

    /// The last lines captured on this thread, for a timeout message.
    fn dump_current() -> String {
        const LAST_LINES: usize = 200;
        CURRENT_DAEMON_LOG.with(|current| match current.borrow().as_ref() {
            None => "(no daemon log captured for this test)".to_string(),
            Some(log) => {
                let lines = log.0.lock().unwrap();
                let start = lines.len().saturating_sub(LAST_LINES);
                format!(
                    "daemon log, last {} of {} lines:\n{}",
                    lines.len() - start,
                    lines.len(),
                    lines[start..].join("\n")
                )
            }
        })
    }
}

async fn roster(stream: &mut TcpStream) -> Vec<SupervisorEntry> {
    match rpc(stream, ClientControlRequest::SupervisorList {}).await {
        ClientControlResponse::SupervisorList { modules, .. } => modules,
        other => panic!("unexpected supervisor.list response: {other:?}"),
    }
}

async fn spawns(stream: &mut TcpStream) -> SpawnSnapshot {
    match rpc(stream, ClientControlRequest::SupervisorSpawnSnapshot {}).await {
        ClientControlResponse::SupervisorSpawnSnapshot { snapshot } => snapshot,
        other => panic!("unexpected spawn snapshot response: {other:?}"),
    }
}

/// Hold startup at the first discoverable moment, while the real listeners
/// answer the same supervisor.list operation used by `ck module status`.
#[tokio::test(flavor = "current_thread")]
async fn configured_modules_are_listed_when_connection_file_is_published() {
    let (dir, path) = temp_connection_file_path("configured-at-publication");
    let missing = dir.path().join("missing-executable");
    let modules = vec![
        configured("enabled-b", missing.clone(), true),
        configured("disabled", missing.clone(), false),
        configured("enabled-a", missing, true),
    ];
    let (entered, release) = startup_publication_hook::install(&path);
    let config = BootstrapConfig::new(&path, 0).with_configured_modules(modules);
    #[cfg(target_os = "macos")]
    let config = config.with_privacy_trampoline(crate::supervise::test_privacy_trampoline());
    let daemon = tokio::spawn(run_with_config(config));
    timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let mut stream = client(&path).await;
    let at_publication = roster(&mut stream).await;
    release.send(()).unwrap();
    let after_activation = timeout(Duration::from_secs(10), async {
        loop {
            let entries = roster(&mut stream).await;
            if entries.len() == 3 && entries.iter().all(|entry| entry.state != "starting") {
                break entries;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());

    let states = at_publication
        .iter()
        .map(|entry| {
            (
                entry.module_id.as_str(),
                entry.state.as_str(),
                entry.enabled,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![
            ("disabled", "disabled", false),
            ("enabled-a", "starting", true),
            ("enabled-b", "starting", true),
        ],
        "publication must expose every configured module before any activation"
    );
    for entry in &at_publication {
        assert!(!entry.live);
        assert_eq!(entry.spawn_generation, Some(0));
        assert_eq!(entry.drain_timeout_ms, Some(123));
        assert_eq!(entry.max_restarts, Some(7));
        assert_eq!(entry.restart_backoff_ms, Some(321));
    }
    let entries = after_activation.expect("spawn failures must settle on the registered handles");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.state.as_str())
            .collect::<Vec<_>>(),
        ["disabled", "failed", "failed"]
    );
}

/// The spawn event cursor records even a child that has already exited. Zero
/// at the publication barrier proves no earlier process was hidden by timing.
#[tokio::test(flavor = "current_thread")]
async fn configured_processes_start_only_after_connection_file_publication() {
    let (dir, path) = temp_connection_file_path("spawn-after-publication");
    let mut module = configured("publication-child", fake_aft_stub_path(), true);
    module.protocol = ModuleProtocol::None;
    module.env = ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
        .into_iter()
        .map(|key| (key.to_string(), dir.path().display().to_string()))
        .chain([("FAKE_AFT_NEVER_CONNECT".into(), "1".into())])
        .collect();
    let (entered, release) = startup_publication_hook::install(&path);
    let config = BootstrapConfig::new(&path, 0).with_configured_modules([module]);
    #[cfg(target_os = "macos")]
    let config = config.with_privacy_trampoline(crate::supervise::test_privacy_trampoline());
    let daemon = tokio::spawn(run_with_config(config));
    timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let mut stream = client(&path).await;
    let before_activation = spawns(&mut stream).await;
    release.send(()).unwrap();
    let after_activation = timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = spawns(&mut stream).await;
            if !snapshot.live.is_empty() {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Stop the real child before assertions, including on mutation runs.
    rpc(
        &mut stream,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: "publication-child".into(),
            enabled: false,
        },
    )
    .await;
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());

    assert_eq!(
        before_activation.cursor.seq, 0,
        "a process was spawned before the publication barrier"
    );
    assert!(before_activation.live.is_empty());
    assert_eq!(after_activation.cursor.seq, 1);
    assert_eq!(after_activation.live.len(), 1);
    assert_eq!(after_activation.live[0].module_id, "publication-child");
    assert_eq!(after_activation.live[0].spawn_generation, 1);
}

/// A refused spec must not prevent a valid sibling from being discoverable
/// at publication or from starting afterward.
#[tokio::test(flavor = "current_thread")]
async fn invalid_configured_module_is_skipped_while_sibling_is_listed_at_publication() {
    let (dir, path) = temp_connection_file_path("registration-failure");
    let invalid = configured("", dir.path().join("missing"), true);
    let mut sibling = configured("valid-sibling", fake_aft_stub_path(), true);
    sibling.protocol = ModuleProtocol::None;
    sibling.env = ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
        .into_iter()
        .map(|key| (key.to_string(), dir.path().display().to_string()))
        .chain([("FAKE_AFT_NEVER_CONNECT".into(), "1".into())])
        .collect();
    let (entered, release) = startup_publication_hook::install(&path);
    let config = BootstrapConfig::new(&path, 0).with_configured_modules([invalid, sibling]);
    #[cfg(target_os = "macos")]
    let config = config.with_privacy_trampoline(crate::supervise::test_privacy_trampoline());
    let daemon = tokio::spawn(run_with_config(config));
    timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let mut stream = client(&path).await;
    let at_publication = roster(&mut stream).await;
    release.send(()).unwrap();
    let after_activation = timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = spawns(&mut stream).await;
            if !snapshot.live.is_empty() {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    rpc(
        &mut stream,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: "valid-sibling".into(),
            enabled: false,
        },
    )
    .await;
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());

    assert_eq!(
        at_publication.len(),
        1,
        "the refused entry must stay off the roster"
    );
    assert_eq!(at_publication[0].module_id, "valid-sibling");
    assert_eq!(at_publication[0].state, "starting");
    assert!(at_publication[0].enabled);
    assert_eq!(at_publication[0].spawn_generation, Some(0));
    assert_eq!(after_activation.live.len(), 1);
    assert_eq!(after_activation.live[0].module_id, "valid-sibling");
    assert_eq!(after_activation.live[0].spawn_generation, 1);
}

/// A configured non-wire child never registers over HELLO, so its first spawn
/// must persist its kernel identity without relying on module registration.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn configured_protocol_none_first_spawn_persists_its_identity() {
    let (_daemon_log, _log_guard) = DaemonLog::install();
    let (dir, path) = temp_connection_file_path("configured-none-record");
    let record = crate::live_children::record_path(dir.path());
    let pid_path = dir.path().join("child.pid");
    let ready_path = dir.path().join("child.ready");
    let mut module = configured("recorded-none", fake_aft_stub_path(), true);
    module.protocol = ModuleProtocol::None;
    module.env = ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
        .into_iter()
        .map(|key| (key.to_string(), dir.path().display().to_string()))
        .chain([
            ("FAKE_AFT_NEVER_CONNECT".into(), "1".into()),
            ("FAKE_AFT_PID_PATH".into(), pid_path.display().to_string()),
            (
                "FAKE_AFT_NEVER_CONNECT_READY_PATH".into(),
                ready_path.display().to_string(),
            ),
        ])
        .collect();
    let (entered, release) = startup_publication_hook::install(&path);
    let config = BootstrapConfig::new(&path, 0)
        .with_live_children_record(&record)
        .with_configured_modules([module]);
    #[cfg(target_os = "macos")]
    let config = config.with_privacy_trampoline(crate::supervise::test_privacy_trampoline());
    let daemon = tokio::spawn(run_with_config(config));
    timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    let before_activation = crate::live_children::read_record(&record).unwrap();
    let mut stream = client(&path).await;
    release.send(()).unwrap();
    timeout(Duration::from_secs(10), async {
        while !ready_path.exists() {
            assert!(
                !daemon.is_finished(),
                "daemon exited before the child became ready"
            );
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let pid = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    let observed = subc_os::Process::open(pid)
        .unwrap()
        .unwrap()
        .observe()
        .unwrap();
    let persisted = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(entry) = crate::live_children::read_record(&record)
                .unwrap()
                .into_iter()
                .find(|entry| {
                    entry.module_id == "recorded-none"
                        && entry.pid == pid
                        && entry.protocol == ModuleProtocol::None
                        && crate::live_children::identity_verdict(entry, pid, &observed)
                            == crate::live_children::IdentityVerdict::Matches
                })
            {
                break entry;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let captured_record = fs::read_to_string(&record);
    // Stop the actual child even when the persistence mutation makes the
    // assertion fail; absence from the crash record must not leak a test process.
    rpc(
        &mut stream,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: "recorded-none".into(),
            enabled: false,
        },
    )
    .await;
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());

    assert!(
        before_activation.is_empty(),
        "registration alone must not record a process"
    );
    assert!(persisted.is_ok(), "the first configured protocol:none spawn must persist its matching identity: observed {observed:?}, record {captured_record:?}");
}
