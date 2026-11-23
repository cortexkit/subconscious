use super::*;

#[test]
fn uncontended_snapshot_access_keeps_the_runtime_worker() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let threads = Arc::new(AtomicUsize::new(0));
    let started = threads.clone();
    let daemon = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .on_thread_start(move || {
            started.fetch_add(1, Ordering::Relaxed);
        })
        .build()
        .unwrap();
    daemon.block_on(async {
        // Run on the worker, not on the thread calling block_on: a blocking
        // section on the worker would start a second thread to take over.
        tokio::spawn(async {
            let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
            drop(lock_snapshot(&snapshot).unwrap());
            update_snapshot(&snapshot, None, |state| state.enabled = false).unwrap();
            assert!(!lock_snapshot(&snapshot).unwrap().enabled);
        })
        .await
        .unwrap();
    });
    drop(daemon);
    assert_eq!(threads.load(Ordering::Relaxed), 1);
}

#[test]
fn registration_retirement_self_prefixed_owner_is_not_a_prefix_submodule() {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("prefix:owner", "owner-nonce".into());
    handle.set_reserved_prefixes("prefix:owner", &["prefix:".into()]);
    let owner = handle
        .reserved_hello_verification("prefix:owner", Some("owner-nonce"))
        .unwrap();
    assert!(owner.nonce_verified);
    assert!(
        owner.prefix_owner.is_none(),
        "an unreserved programmatic owner is not its own prefix child"
    );
    let child = handle
        .reserved_hello_verification("prefix:child", Some("owner-nonce"))
        .unwrap();
    assert!(child.nonce_verified);
    assert_eq!(child.prefix_owner.as_deref(), Some("prefix:owner"));
}

#[test]
fn readding_a_module_clears_its_rescan_removal_tombstone() {
    let handle = SupervisorHandle::new();
    let module_id = "readded-tombstone";
    handle.record_rescan_removal(module_id);
    assert!(handle.removal_tombstone_age_ms(module_id).is_some());

    handle.apply_identity_configuration(&ModuleSpec {
        script: None,
        module_id: module_id.to_string(),
        program: PathBuf::from("/test/module"),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    });

    assert!(
        handle.removal_tombstone_age_ms(module_id).is_none(),
        "a re-added module must not retain a stale removal tombstone"
    );
}

/// What one module's owner looked like from the control plane at the
/// instant after its first process was spawned.
#[derive(Debug, PartialEq, Eq)]
struct OwnerInSpawnWindow {
    module_id: String,
    configured: bool,
    on_roster: bool,
    admission_refusal: Option<&'static str>,
}

/// A supervised module's process can connect, register, sync its scopes
/// and describe them as soon as it is spawned. A plain `spawn` does not put
/// the module on the roster until afterward; configured supervision does
/// so before spawning. In both cases the owner must
/// already read as configured, so a scoped `route.open` against it is
/// refused as retryable `scope_not_synced` and not as terminal
/// `scope_not_live` ("will never sync").
///
/// The hook runs just after each initial spawn attempt, before returning to
/// the caller, so no race with a real child's registration is needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_module_is_configured_before_its_first_process_can_register() {
    use crate::scopes::ScopeTable;
    use subc_protocol::{error_codes, scope::ScopeSelector, Principal};

    let dir = cortexkit_test_support::ScratchDir::new("configured-before-spawn");
    let stub = |module_id: &str, program: PathBuf| ModuleSpec {
        script: None,
        module_id: module_id.to_string(),
        program,
        args: Vec::new(),
        env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.to_string(), dir.path().display().to_string()))
            .chain([("FAKE_AFT_NEVER_CONNECT".to_string(), "1".to_string())])
            .collect(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let live = super::terminal_history_tests::fake_aft_stub_path();
    let missing = dir.path().join("definitely-missing-module");

    let handle = SupervisorHandle::new();
    let mut supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
            .with_handle(handle.clone());
    let observed = Arc::new(Mutex::new(Vec::<OwnerInSpawnWindow>::new()));
    let hook_handle = handle.clone();
    let hook_observed = Arc::clone(&observed);
    supervisor.test_after_first_spawn = AfterFirstSpawnHook(Some(Arc::new(move |module_id| {
        // Exactly what the control plane computes for a scoped route.open
        // naming this module as the owner of a scope it has not synced.
        let configured = hook_handle.is_configured(module_id);
        let selector = ScopeSelector::new(
            Principal::Reserved {
                module_id: module_id.to_string(),
            },
            "s".to_string(),
            Some(1),
        );
        let carrier = Principal::Reserved {
            module_id: "carrier".to_string(),
        };
        let admission_refusal = match ScopeTable::new(Vec::<String>::new())
            .admit(&carrier, module_id, &selector, configured)
        {
            Ok(_) => None,
            Err(refusal) => Some(refusal.code),
        };
        hook_observed.lock().unwrap().push(OwnerInSpawnWindow {
            module_id: module_id.to_string(),
            configured,
            on_roster: hook_handle.get(module_id).is_some(),
            admission_refusal,
        });
    })));

    let plain = supervisor.spawn(stub("plain", live.clone())).unwrap();
    let configured = supervisor
        .supervise_configured(stub("configured", live.clone()), true)
        .unwrap();
    let with_health = supervisor
        .supervise_configured_with_health(
            stub("with-health", live.clone()),
            true,
            HealthConfig::default(),
            None,
            RestartPolicy::default(),
        )
        .unwrap();
    // The failed-spawn path still puts the module on the roster (as
    // failed), so it is configured throughout.
    let failed = supervisor
        .supervise_configured_with_health(
            stub("failed-spawn", missing.clone()),
            true,
            HealthConfig::default(),
            None,
            RestartPolicy::default(),
        )
        .unwrap();
    // A failed plain `spawn` puts nothing on the roster, so its mark is
    // taken back once the spawn has failed.
    assert!(supervisor.spawn(stub("spawn-error", missing)).is_err());

    let expected = [
        "plain",
        "configured",
        "with-health",
        "failed-spawn",
        "spawn-error",
    ]
    .into_iter()
    .map(|module_id| OwnerInSpawnWindow {
        module_id: module_id.to_string(),
        configured: true,
        on_roster: !matches!(module_id, "plain" | "spawn-error"),
        admission_refusal: Some(error_codes::SCOPE_NOT_SYNCED),
    })
    .collect::<Vec<_>>();
    assert_eq!(*observed.lock().unwrap(), expected);

    for module_id in ["plain", "configured", "with-health", "failed-spawn"] {
        assert!(
            handle.get(module_id).is_some(),
            "{module_id} is on the roster"
        );
        assert!(
            handle.is_configured(module_id),
            "{module_id} stays configured"
        );
    }
    assert!(handle.get("spawn-error").is_none());
    assert!(
        !handle.is_configured("spawn-error"),
        "a plain spawn that failed must not leave its module marked configured"
    );

    // Leaving the roster clears the mark with it.
    handle.retire("failed-spawn");
    assert!(!handle.is_configured("failed-spawn"));

    for module in [plain, configured, with_health] {
        module.stop().await.unwrap();
    }
    drop(failed);
}

/// A configured module reads as starting until the supervisor has admitted
/// its first process (exec confirmed, identity durably recorded), even
/// though that process is already running by then and can register over
/// HELLO, which is what lists it in catalog.list. So a client that needs
/// the supervisor state must wait for that state; seeing the module in
/// the catalog does not mean supervisor.list already says running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_configured_module_reads_starting_while_its_first_process_already_runs() {
    let dir = cortexkit_test_support::ScratchDir::new("starting-while-spawned");
    let spec = ModuleSpec {
        script: None,
        module_id: "spawned-not-admitted".to_string(),
        program: super::terminal_history_tests::fake_aft_stub_path(),
        args: Vec::new(),
        env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.to_string(), dir.path().display().to_string()))
            .chain([("FAKE_AFT_NEVER_CONNECT".to_string(), "1".to_string())])
            .collect(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let handle = SupervisorHandle::new();
    let mut supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
            .with_handle(handle.clone());
    let observed = Arc::new(Mutex::new(None));
    let hook_handle = handle.clone();
    let hook_observed = Arc::clone(&observed);
    supervisor.test_after_first_spawn = AfterFirstSpawnHook(Some(Arc::new(move |module_id| {
        let state = hook_handle
            .get(module_id)
            .expect("a configured module is on the roster before it is spawned")
            .state()
            .unwrap();
        *hook_observed.lock().unwrap() = Some(state);
    })));

    let module = supervisor.supervise_configured(spec, true).unwrap();
    assert_eq!(
        *observed.lock().unwrap(),
        Some(ModuleState::Starting),
        "between the spawn and its admission the supervisor still reports starting"
    );
    assert_eq!(module.state().unwrap(), ModuleState::Running);
    module.stop().await.unwrap();
}

fn stale_process_snapshot(state: ModuleState, enabled: bool) -> SharedSnapshot {
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::new(state, enabled)));
    update_snapshot(&snapshot, Some("stale-process-facts"), |snapshot| {
        snapshot.process_alive = true;
        snapshot.pid = Some(41);
        snapshot.spawned_at_ms = Some(42);
        snapshot.spawned_from = Some(PathBuf::from("/spawned/module"));
        snapshot.launch_spec_digest = Some(LaunchSpecDigest([0xA5; 32]));
        snapshot.spawned_file_identity = Some(SpawnedFileIdentity {
            device: 43,
            inode: 44,
            #[cfg(windows)]
            windows_image: None,
        });
    })
    .unwrap();
    snapshot
}

fn assert_snapshot_process_facts_cleared(snapshot: &SharedSnapshot) {
    let snapshot = lock_snapshot(snapshot).unwrap();
    assert!(!snapshot.process_alive);
    assert_eq!(snapshot.pid, None);
    assert_eq!(snapshot.spawned_at_ms, None);
    assert_eq!(snapshot.spawned_from, None);
    assert_eq!(snapshot.launch_spec_digest, None);
    assert_eq!(snapshot.spawned_file_identity, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_enable_spawn_clears_preexisting_current_process_facts() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let mut runtime = supervisor.runtime_config();
    runtime.test_seed_stale_facts_before_enable_spawn = true;
    let snapshot = stale_process_snapshot(ModuleState::Disabled, false);
    let mut child = None;
    let spec = ModuleSpec {
        script: None,
        module_id: "failed-enable-clears-facts".to_string(),
        program: PathBuf::from("/definitely/missing/failed-enable-module"),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };

    let result = set_child_enabled(
        &spec,
        &runtime,
        &supervisor.registry,
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        true,
        &runtime.failure_context(false),
    )
    .await;

    assert!(matches!(result, Err(SuperviseError::Spawn { .. })));
    assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
    assert_snapshot_process_facts_cleared(&snapshot);
}

#[tokio::test]
async fn start_revives_stranded_restarting_but_not_pending_backoff() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let runtime = supervisor.runtime_config();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::new(
        ModuleState::Restarting,
        true,
    )));
    let spec = ModuleSpec {
        script: None,
        module_id: "start-stranded-restarting".to_string(),
        program: super::terminal_history_tests::fake_aft_stub_path(),
        args: Vec::new(),
        env: vec![("FAKE_AFT_NEVER_CONNECT".to_string(), "1".to_string())],
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    };
    let mut child = None;
    lock_snapshot(&snapshot).unwrap().respawn_pending = true;
    assert!(!super::set_child_enabled(
        &spec,
        &runtime,
        &Registry::default(),
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        true,
        &runtime.failure_context(false),
    )
    .await
    .unwrap());
    assert!(child.is_none());
    lock_snapshot(&snapshot).unwrap().respawn_pending = false;
    assert!(super::set_child_enabled(
        &spec,
        &runtime,
        &Registry::default(),
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        true,
        &runtime.failure_context(false),
    )
    .await
    .unwrap());
    assert_eq!(
        lock_snapshot(&snapshot).unwrap().state,
        ModuleState::Running
    );
    let mut child = child.unwrap();
    child.start_kill().unwrap();
    child.wait().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_reload_spawn_clears_current_process_facts() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let mut runtime = supervisor.runtime_config();
    runtime.restart_policy = RestartPolicy::new(0, Duration::ZERO);
    let snapshot = stale_process_snapshot(ModuleState::Running, true);
    let mut child = None;
    let spec = ModuleSpec {
        script: None,
        module_id: "failed-reload-clears-facts".to_string(),
        program: PathBuf::from("/unused/failed-reload-module"),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };

    let result = handle_reload_spawn_failure(
        &spec,
        &runtime,
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        "forced reload spawn failure".to_string(),
    )
    .await;

    assert!(matches!(result, Err(SuperviseError::ReloadFailed { .. })));
    assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
    assert_snapshot_process_facts_cleared(&snapshot);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_module_with_an_active_monitor_clears_current_process_facts() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let snapshot = stale_process_snapshot(ModuleState::Running, true);
    let module = supervisor.supervised_module(
        ModuleSpec {
            script: None,
            module_id: "drop-clears-facts".to_string(),
            program: PathBuf::from("/unused/drop-module"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
            carry_containment: false,
        },
        supervisor.runtime_config(),
        Arc::clone(&snapshot),
        None,
    );
    assert!(!module
        .inner
        .monitor
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_finished());

    drop(module);

    assert_eq!(
        lock_snapshot(&snapshot).unwrap().state,
        ModuleState::Stopped
    );
    assert_snapshot_process_facts_cleared(&snapshot);
}

#[cfg(unix)]
#[tokio::test]
async fn rescan_preserves_running_protocol_until_respawn() {
    let dir = cortexkit_test_support::ScratchDir::new("rescan-protocol");
    let initial = ModuleSpec {
        script: None,
        module_id: "rescan-protocol".into(),
        program: PathBuf::from("/bin/sleep"),
        args: vec!["60".into()],
        env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.into(), dir.path().to_string_lossy().into_owned()))
            .collect(),
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    };
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let module = supervisor.spawn(initial.clone()).unwrap();
    assert!(module.status().unwrap().live);
    let mut next = initial;
    next.protocol = ModuleProtocol::Subc;
    module
        .update_configuration(next.clone(), HealthConfig::default(), None)
        .await
        .unwrap();
    assert!(
        module.status().unwrap().live,
        "rescan must not require HELLO from the old non-wire process"
    );
    let runtime = supervisor.runtime_config();
    let action = on_child_exit(
        &next,
        RestartPolicy::default(),
        &supervisor.registry,
        &module.inner.snapshot,
        &runtime.terminal_ring,
        &runtime.spawn_events,
        &runtime.child_roster,
        ExitReport {
            kind: ExitKind::Clean,
            code: Some(0),
            signal: None,
            at_ms: unix_ms_now(),
        },
        true,
    )
    .await;
    assert!(
        matches!(action, NextAction::Restart { .. }),
        "the old non-wire process's clean exit must restart"
    );
    module.drain().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn orphan_identity_matches_path_names_and_shebang_interpreters() {
    use std::os::unix::fs::PermissionsExt;
    let dir = cortexkit_test_support::ScratchDir::new("spawn-image-identity");
    let script = dir.join("module.sh");
    std::fs::write(&script, "#!/bin/sh\nwhile :; do sleep 0.1; done\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let record_path = dir.join("live-children.json");
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
            .with_live_children_record(&record_path);
    for (program, args) in [
        (PathBuf::from("sleep"), vec!["60".into()]),
        (script, vec![]),
    ] {
        let spec = ModuleSpec {
            script: None,
            module_id: "image-identity".into(),
            program: program.clone(),
            args,
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| (key.into(), dir.to_string_lossy().into_owned()))
                .collect(),
            reserved: false,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
            carry_containment: false,
        };
        let module = supervisor.spawn(spec).unwrap();
        #[cfg(target_os = "macos")]
        {
            // SETEXEC confirmation is asynchronous; the orphan record must
            // identify the final image, never the intermediate trampoline.
            let deadline = Instant::now() + Duration::from_secs(5);
            while crate::live_children::read_record(&record_path)
                .unwrap()
                .iter()
                .all(|entry| entry.executable.is_none())
            {
                assert!(Instant::now() < deadline, "module image was not confirmed");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        let entry = crate::live_children::read_record(&record_path)
            .unwrap()
            .pop()
            .unwrap();
        let observed = subc_os::Process::open(entry.pid)
            .unwrap()
            .unwrap()
            .observe()
            .unwrap();
        let verdict = crate::live_children::identity_verdict(&entry, entry.pid, &observed);
        module.drain().await.unwrap();
        assert_eq!(
            verdict,
            crate::live_children::IdentityVerdict::Matches,
            "program {program:?}: recorded {entry:?}, observed {observed:?}"
        );
    }
}

#[cfg(unix)]
fn http_fixture(
    dir: &std::path::Path,
    url: &str,
    threshold: u32,
) -> crate::daemon_config::ConfiguredModule {
    let path = dir.join("subc.jsonc");
    std::fs::write(
        &path,
        serde_json::json!({"version": 1, "modules": {"http-process": {
            "program": "/bin/sleep", "args": ["60"], "protocol": "none",
            "env": {"XDG_DATA_HOME":dir,"XDG_RUNTIME_DIR":dir,"XDG_CONFIG_HOME":dir},
            "health": {"http":url,"cadence_ms":20,"deadline_ms":200,"failure_threshold":threshold},
            "restart": {"backoff_ms":1,"max_backoff_ms":1}, "drain_timeout_ms":10
        }}})
        .to_string(),
    )
    .unwrap();
    crate::daemon_config::load(&path)
        .unwrap()
        .unwrap()
        .modules
        .pop()
        .unwrap()
}

#[cfg(unix)]
async fn wait_http_health(module: &SupervisedModule, status: SupervisorHealthStatus) {
    timeout(Duration::from_secs(5), async {
        loop {
            if module.status().unwrap().health.status == status {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "expected {status:?}, got {:?}",
            module.status().unwrap().health
        )
    });
}

#[cfg(unix)]
#[tokio::test]
async fn http_health_status_flips_ok_failing_ok() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = cortexkit_test_support::ScratchDir::new("http-status-flips");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let status = Arc::new(std::sync::atomic::AtomicU16::new(200));
    let serving_status = status.clone();
    let url = format!("http://{}/healthz", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let count = stream.read(&mut request).await.unwrap();
            assert!(count > 0, "a probe must send an HTTP request");
            let code = serving_status.load(std::sync::atomic::Ordering::SeqCst);
            let body = if code == 200 {
                "ready"
            } else {
                "{\"status\":\"unavailable\",\"error\":\"scratch failure\"}"
            };
            let response = format!(
                "HTTP/1.1 {code} scratch\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    let configured = http_fixture(&dir, &url, 1000);
    let module = Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
        .supervise_configured_with_health(
            configured.module_spec(),
            true,
            configured.health,
            configured.drain_timeout_ms,
            configured.restart,
        )
        .unwrap();
    wait_http_health(&module, SupervisorHealthStatus::Ok).await;
    status.store(503, std::sync::atomic::Ordering::SeqCst);
    wait_http_health(&module, SupervisorHealthStatus::Failing).await;
    assert!(module
        .status()
        .unwrap()
        .health
        .detail
        .unwrap()
        .contains("scratch failure"));
    status.store(200, std::sync::atomic::Ordering::SeqCst);
    wait_http_health(&module, SupervisorHealthStatus::Ok).await;
    assert_eq!(module.status().unwrap().health.consecutive_failures, 0);
    let before = module.status().unwrap();
    let (spec, mut health) = module.configuration().unwrap();
    health.http = None;
    module
        .update_configuration(spec.clone(), health.clone(), Some(10))
        .await
        .unwrap();
    wait_http_health(&module, SupervisorHealthStatus::Unknown).await;
    health.http = Some(url);
    module
        .update_configuration(spec, health, Some(10))
        .await
        .unwrap();
    wait_http_health(&module, SupervisorHealthStatus::Ok).await;
    assert_eq!(
        module.status().unwrap().pid,
        before.pid,
        "changing a probe must apply live, not restart its process"
    );
    let (spec, mut health) = module.configuration().unwrap();
    health.failure_threshold = 2;
    module
        .update_configuration(spec, health, Some(10))
        .await
        .unwrap();
    status.store(503, std::sync::atomic::Ordering::SeqCst);
    timeout(Duration::from_secs(5), async {
        while module.status().unwrap().spawn_generation == before.spawn_generation {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("sustained HTTP 503 responses must trigger the health restart policy");
    module.drain().await.unwrap();
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn http_health_replacement_recovers_after_one_restart() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = cortexkit_test_support::ScratchDir::new("http-replacement-recovers");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/healthz", listener.local_addr().unwrap());
    let mut configured = http_fixture(&dir, &url, 2);
    configured.restart.max_restarts = 2;
    let module = Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
        .supervise_configured_with_health(
            configured.module_spec(),
            true,
            configured.health,
            configured.drain_timeout_ms,
            configured.restart,
        )
        .unwrap();
    let first_generation = module.status().unwrap().spawn_generation;
    let observed = module.clone();
    let (healthy_tx, mut healthy_rx) = mpsc::channel(8);
    let server = tokio::spawn(async move {
        let mut generation = first_generation;
        let mut probes = 0;
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let current = observed.status().unwrap().spawn_generation;
            if current != generation {
                generation = current;
                probes = 0;
            }
            probes += 1;
            // The original process never recovers. Each replacement has one
            // transient miss, then answers successfully for its remaining life.
            let healthy = generation != first_generation && probes > 1;
            let code = if healthy { 200 } else { 503 };
            let response = format!(
                "HTTP/1.1 {code} scratch\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(response.as_bytes()).await;
            if healthy {
                healthy_tx.send(()).await.unwrap();
            }
        }
    });
    timeout(Duration::from_secs(5), async {
        let mut healthy_probes = 0;
        loop {
            if module.state().unwrap() == ModuleState::Failed {
                break;
            }
            while healthy_rx.try_recv().is_ok() {
                healthy_probes += 1;
            }
            if healthy_probes >= 3
                && module.status().unwrap().health.status == SupervisorHealthStatus::Ok
            {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the replacement must settle healthy without exhausting its restart budget");
    let status = module.status().unwrap();
    module.drain().await.unwrap();
    server.abort();
    assert_eq!(status.state, ModuleState::Running);
    assert_eq!(status.health.status, SupervisorHealthStatus::Ok);
    assert_eq!(status.health.consecutive_failures, 0);
    assert_eq!(status.lifetime_restarts, 1);
    assert_eq!(status.restart_count, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn http_health_sustained_failure_exhausts_restart_budget() {
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = cortexkit_test_support::ScratchDir::new("http-health-budget");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/healthz", listener.local_addr().unwrap());
    let mut configured = http_fixture(&dir, &url, 2);
    configured.restart.max_restarts = 2;
    let module = Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
        .supervise_configured_with_health(
            configured.module_spec(),
            true,
            configured.health,
            configured.drain_timeout_ms,
            configured.restart,
        )
        .unwrap();
    let observed = module.clone();
    let probes = Arc::new(Mutex::new(BTreeMap::<u64, u32>::new()));
    let served = probes.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            *served
                .lock()
                .unwrap()
                .entry(observed.status().unwrap().spawn_generation)
                .or_default() += 1;
            let _ = stream
                .write_all(
                    b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        }
    });
    timeout(Duration::from_secs(5), async {
        while module.state().unwrap() != ModuleState::Failed {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("sustained failures must still exhaust the health restart budget");
    let status = module.status().unwrap();
    server.abort();
    assert_eq!(status.restart_count, 2);
    assert_eq!(status.lifetime_restarts, 2);
    assert!(!status.process_alive);
    assert_eq!(
        probes.lock().unwrap().values().copied().collect::<Vec<_>>(),
        vec![2, 2, 2]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn spawn_clears_the_previous_process_health() {
    let dir = cortexkit_test_support::ScratchDir::new("spawn-fresh-health");
    let mut configured = http_fixture(&dir, "http://127.0.0.1:1/healthz", 2);
    configured.health.cadence = Duration::from_secs(3600);
    let module = Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
        .supervise_configured_with_health(
            configured.module_spec(),
            false,
            configured.health,
            configured.drain_timeout_ms,
            configured.restart,
        )
        .unwrap();
    update_snapshot(&module.inner.snapshot, Some(module.module_id()), |state| {
        state.health.status = SupervisorHealthStatus::Unresponsive;
        state.health.consecutive_failures = 3;
        state.health.detail = Some("old process refused connections".into());
        state.health.metrics = Some(serde_json::json!({"old": true}));
        state.health.last_probe_ms = Some(123);
        state.health.last_action = Some("restart".into());
        state.health.last_action_ms = Some(124);
    })
    .unwrap();
    module.set_enabled(true).await.unwrap();
    let health = module.status().unwrap().health;
    module.drain().await.unwrap();
    assert_eq!(health.consecutive_failures, 0);
    assert_eq!(health.status, SupervisorHealthStatus::Unknown);
    assert_eq!(health.detail, None);
    assert_eq!(health.metrics, None);
    assert_eq!(health.last_probe_ms, None);
    assert_eq!(health.last_action.as_deref(), Some("restart"));
    assert_eq!(health.last_action_ms, Some(124));
}

#[cfg(unix)]
#[tokio::test]
async fn http_health_no_listener_restarts_after_consecutive_failures() {
    let dir = cortexkit_test_support::ScratchDir::new("http-refused");
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/healthz", unused.local_addr().unwrap());
    drop(unused);
    let configured = http_fixture(&dir, &url, 2);
    let module = Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
        .supervise_configured_with_health(
            configured.module_spec(),
            true,
            configured.health,
            configured.drain_timeout_ms,
            configured.restart,
        )
        .unwrap();
    let before = module.status().unwrap().spawn_generation;
    timeout(Duration::from_secs(5), async {
        loop {
            let status = module.status().unwrap();
            if status.spawn_generation > before {
                assert!(status.lifetime_restarts > 0);
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("sustained HTTP refusal must trigger the health restart policy");
    module.drain().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn http_health_timeout_honours_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/healthz", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let _held = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let error = timeout(
        Duration::from_secs(1),
        probe_http_health(&url, Duration::from_millis(10)),
    )
    .await
    .expect("the probe must enforce its own deadline")
    .unwrap_err();
    server.abort();
    assert!(error.to_string().contains("timed out"));
}

#[cfg(unix)]
#[tokio::test]
async fn http_health_localhost_reaches_an_ipv6_only_listener() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let url = format!(
        "http://localhost:{}/healthz",
        listener.local_addr().unwrap().port()
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 2048];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
    });
    // The deadline only bounds a hang. A probe that never tried the IPv6
    // address would be refused on 127.0.0.1 and fail at once, so a longer
    // deadline does not weaken the assertion; one second timed out under a
    // loaded parallel test run.
    assert_eq!(
        probe_http_health(&url, Duration::from_secs(10))
            .await
            .unwrap()
            .status,
        HealthStatus::Ok
    );
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn http_health_timeout_keeps_partial_status_and_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/healthz", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 2048];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        stream
            .write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 100\r\n\r\npartial diagnostic")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let error = probe_http_health(&url, Duration::from_secs(1))
        .await
        .unwrap_err()
        .to_string();
    server.abort();
    assert!(
        error.contains("timed out")
            && error.contains("503 Unavailable")
            && error.contains("partial diagnostic"),
        "{error}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn http_health_diagnostic_body_is_bounded_to_200_bytes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/healthz", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 2048];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        let body = format!("{}not-in-diagnostic", "x".repeat(400));
        let response = format!(
            "HTTP/1.1 503 Unavailable\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let error = probe_http_health(&url, Duration::from_secs(1))
        .await
        .unwrap_err()
        .to_string();
    server.await.unwrap();
    assert!(error.ends_with(&"x".repeat(200)), "{error}");
    assert_eq!(error.split(": ").last().unwrap().len(), 200);
    assert!(!error.contains("not-in-diagnostic"));
}

#[cfg(unix)]
#[tokio::test]
async fn http_health_real_nats_server_monitoring() {
    if std::process::Command::new("nats-server")
        .arg("--version")
        .env("XDG_DATA_HOME", std::env::temp_dir())
        .env("XDG_RUNTIME_DIR", std::env::temp_dir())
        .env("XDG_CONFIG_HOME", std::env::temp_dir())
        .output()
        .is_err()
    {
        eprintln!("skipped: http_health_real_nats_server_monitoring (nats-server is not on PATH)");
        return;
    }
    let dir = cortexkit_test_support::ScratchDir::new("nats-http-monitoring");
    let monitor = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = monitor.local_addr().unwrap().port();
    let client = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client_port = client.local_addr().unwrap().port();
    let config = dir.join("server.conf");
    std::fs::write(&config, format!("listen: 127.0.0.1:{client_port}\nhttp: 127.0.0.1:{port}\njetstream {{ store_dir: \"{}\" }}\n", dir.join("jetstream").display())).unwrap();
    let mut configured = http_fixture(&dir, &format!("http://127.0.0.1:{port}/healthz"), 1000);
    configured.program = PathBuf::from("nats-server");
    configured.args = vec!["-c".into(), config.to_string_lossy().into_owned()];
    drop(monitor);
    drop(client);
    let module = Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
        .supervise_configured_with_health(
            configured.module_spec(),
            true,
            configured.health,
            configured.drain_timeout_ms,
            configured.restart,
        )
        .unwrap();
    wait_http_health(&module, SupervisorHealthStatus::Ok).await;
    module.drain().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configuration_update_does_not_replace_captured_running_process_facts() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let snapshot = stale_process_snapshot(ModuleState::Running, true);
    let initial = ModuleSpec {
        script: None,
        module_id: "rescan-preserves-spawn-facts".to_string(),
        program: PathBuf::from("/spawned/module"),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let module =
        supervisor.supervised_module(initial.clone(), supervisor.runtime_config(), snapshot, None);
    let before = module.status().unwrap();
    let mut replacement = initial;
    replacement.program = PathBuf::from("/rescanned/replacement-module");

    module
        .update_configuration(replacement, HealthConfig::default(), None)
        .await
        .unwrap();

    let after = module.status().unwrap();
    assert_eq!(after.pid, before.pid);
    assert_eq!(after.spawned_at_ms, before.spawned_at_ms);
    assert_eq!(after.spawned_from, before.spawned_from);
    drop(module);
}
