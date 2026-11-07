use super::*;
use cortexkit_test_support::ScratchDir;

struct OwnedProcess {
    process: subc_os::Process,
    start_time: u64,
}

impl OwnedProcess {
    fn capture(pid: u32, parent: u32) -> Self {
        assert_eq!(
            subc_os::parent_pid(pid),
            Some(parent),
            "fixture pid must belong to the process we spawned"
        );
        let process = subc_os::Process::open(pid)
            .unwrap()
            .expect("fixture process exists");
        let start_time = process
            .observe()
            .expect("fixture identity is readable")
            .start_time;
        Self {
            process,
            start_time,
        }
    }

    fn alive(&self) -> bool {
        self.process
            .observe()
            .is_some_and(|now| now.start_time == self.start_time)
    }

    fn kill(&self) {
        if self.alive() {
            self.process.signal(subc_os::Signal::Kill).unwrap();
        }
    }

    async fn gone(&self) {
        timeout(Duration::from_secs(5), async {
            while self.alive() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the carried holder must be terminated");
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if self.alive() {
            let _ = self.process.signal(subc_os::Signal::Kill);
        }
    }
}

struct Fixture {
    _dir: ScratchDir,
    supervisor: Supervisor,
    module: SupervisedModule,
    spec: ModuleSpec,
    holder: OwnedProcess,
    first_leader: OwnedProcess,
    record: PathBuf,
}

struct StartingModule(Option<SupervisedModule>);

impl Drop for StartingModule {
    fn drop(&mut self) {
        if let Some(module) = &self.0 {
            if let Some(monitor) = module.inner.monitor.lock().unwrap().take() {
                monitor.abort();
            }
        }
    }
}

impl Fixture {
    async fn new(label: &str, max_restarts: u32, backoff: Duration) -> Self {
        let dir = ScratchDir::new(label);
        let record = dir.join("live-children.json");
        let holder_path = dir.join("holder.pid");
        let spec = ModuleSpec {
            module_id: label.to_owned(),
            program: if cfg!(target_os = "macos") { "/bin/bash" } else { "/bin/sh" }.into(),
            args: vec!["-c".into(),
                "if [ ! -f \"$1\" ]; then /bin/sleep 600 </dev/null >/dev/null 2>&1 & echo $! >\"$1\"; fi; trap 'exit 0' TERM; while :; do /bin/sleep 1; done".into(),
                "holder-fixture".into(), holder_path.display().to_string()],
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"].into_iter()
                .map(|key| (key.to_owned(), dir.display().to_string())).collect(),
            reserved: false,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::None,
            overlap: ModuleOverlap::Exclusive,
            carry_containment: true,
        };
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(max_restarts, backoff),
        )
        .with_drain_timeout(Duration::from_millis(20))
        .with_forwarding(Arc::new(ForwardingTable::default()))
        .with_live_children_record(&record);
        let mut starting = StartingModule(Some(
            supervisor
                .supervise_configured_requested(
                    spec.clone(),
                    true,
                    HealthConfig::default(),
                    None,
                    RestartPolicy::new(max_restarts, backoff),
                    Duration::from_secs(1),
                    None,
                )
                .unwrap(),
        ));
        let module = starting.0.as_ref().unwrap();
        let leader = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(pid) = module.status().unwrap().pid {
                    break pid;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|error| panic!(
            "module must finish privacy exec confirmation before identity capture: {error}; status={:?}; terminals={:?}",
            module.status(), module.terminal_history()
        ));
        let first_leader = OwnedProcess::capture(leader, std::process::id());
        let holder = timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(&holder_path) {
                    if let Ok(pid) = contents.trim().parse::<u32>() {
                        if subc_os::Process::open(pid)
                            .ok()
                            .flatten()
                            .and_then(|process| process.observe())
                            .is_some()
                        {
                            break OwnedProcess::capture(pid, leader);
                        }
                    }
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("leader must publish its spawned holder");
        let module = starting.0.take().unwrap();
        Self {
            _dir: dir,
            supervisor,
            module,
            spec,
            holder,
            first_leader,
            record,
        }
    }

    async fn status(&self, predicate: impl Fn(&ModuleStatus) -> bool) -> ModuleStatus {
        timeout(Duration::from_secs(5), async {
            loop {
                let status = self.module.status().unwrap();
                if predicate(&status) {
                    break status;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("supervisor did not reach the expected lifecycle state")
    }

    async fn backoff(&self) {
        self.module.restart(None).await.unwrap();
        self.status(|status| {
            status.state == ModuleState::Restarting
                && !status.process_alive
                && lock_snapshot(&self.module.inner.snapshot)
                    .unwrap()
                    .respawn_pending
        })
        .await;
        assert!(
            self.holder.alive(),
            "a named pending replacement must retain its holder"
        );
        let records = crate::live_children::read_record(&self.record).unwrap();
        assert_eq!(
            records.len(),
            1,
            "the exited leader must release only its own roster entry"
        );
        assert!(
            records[0].container.is_some(),
            "the container must remain durable with no live leader"
        );
    }

    fn pin_waitable(&self) {
        assert!(
            subc_os::child_exit::observe(self.first_leader.process.pid())
                .unwrap()
                .is_some(),
            "the original leader must remain our unreaped child across replacements"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(status) = self.module.status() {
            if let (Some(pid), Some(start_time)) = (status.pid, status.process_start_time) {
                if subc_os::parent_pid(pid) == Some(std::process::id()) {
                    if let Ok(Some(process)) = subc_os::Process::open(pid) {
                        if process
                            .observe()
                            .is_some_and(|now| now.start_time == start_time)
                        {
                            let _ = process.signal(subc_os::Signal::Kill);
                        }
                    }
                }
            }
        }
        if let Some(monitor) = self.module.inner.monitor.lock().unwrap().take() {
            monitor.abort();
        }
    }
}

#[tokio::test]
async fn carried_operator_restart_keeps_holder_and_pin_across_two_generations() {
    let fixture = Fixture::new("carried-operator", 3, Duration::from_millis(5)).await;
    let mut generation = fixture.module.status().unwrap().spawn_generation;
    for _ in 0..2 {
        fixture.module.restart(None).await.unwrap();
        generation = fixture
            .status(|status| {
                status.state == ModuleState::Running && status.spawn_generation > generation
            })
            .await
            .spawn_generation;
        assert!(fixture.holder.alive());
        fixture.pin_waitable();
    }
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn carried_crash_restart_keeps_holder() {
    let fixture = Fixture::new("carried-crash", 3, Duration::from_millis(5)).await;
    let generation = fixture.module.status().unwrap().spawn_generation;
    fixture.first_leader.kill();
    fixture
        .status(|status| {
            status.state == ModuleState::Running && status.spawn_generation > generation
        })
        .await;
    assert!(fixture.holder.alive());
    fixture.pin_waitable();
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn carried_stop_terminates_holder() {
    let fixture = Fixture::new("carried-stop", 3, Duration::ZERO).await;
    fixture.module.stop().await.unwrap();
    fixture.holder.gone().await;
    assert!(crate::live_children::read_record(&fixture.record)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn carried_stop_during_backoff_terminates_holder() {
    let fixture = Fixture::new("carried-stop-backoff", 3, Duration::from_secs(60)).await;
    fixture.backoff().await;
    fixture.module.stop().await.unwrap();
    fixture.holder.gone().await;
}

#[tokio::test]
async fn carried_disable_during_backoff_terminates_holder() {
    let fixture = Fixture::new("carried-disable-backoff", 3, Duration::from_secs(60)).await;
    fixture.backoff().await;
    assert!(fixture.module.set_enabled(false).await.unwrap());
    fixture.holder.gone().await;
    assert!(crate::live_children::read_record(&fixture.record)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn carried_crash_budget_exhaustion_terminates_holder() {
    let fixture = Fixture::new("carried-budget", 0, Duration::ZERO).await;
    fixture.first_leader.kill();
    fixture
        .status(|status| status.state == ModuleState::Failed)
        .await;
    fixture.holder.gone().await;
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn carried_shutdown_during_backoff_terminates_holder() {
    let fixture = Fixture::new("carried-shutdown-backoff", 3, Duration::from_secs(60)).await;
    fixture.backoff().await;
    crate::child_roster::end_children_for_daemon_shutdown(
        &fixture.supervisor.child_roster,
        false,
        std::future::pending(),
    )
    .await;
    fixture.holder.gone().await;
    assert!(crate::live_children::read_record(&fixture.record)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn carried_monitor_abort_hard_kills_holder() {
    let fixture = Fixture::new("carried-abort", 3, Duration::ZERO).await;
    let monitor = fixture.module.inner.monitor.lock().unwrap().take().unwrap();
    monitor.abort();
    assert!(monitor.await.unwrap_err().is_cancelled());
    fixture.holder.gone().await;
    // The previous container may remain listed while its KILL is pending.
    // Rechecking must remove that record once the container is verified empty.
    timeout(Duration::from_secs(5), async {
        while fixture
            .supervisor
            .child_roster
            .previous_instance(&fixture.spec.module_id)
            .is_some()
        {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("verified empty containers must release their retained identity");
    assert!(crate::live_children::read_record(&fixture.record)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn carried_setting_is_sticky_until_terminal_end() {
    let fixture = Fixture::new("carried-sticky", 3, Duration::from_millis(5)).await;
    let mut changed = fixture.spec.clone();
    changed.carry_containment = false;
    fixture
        .module
        .update_configuration(changed, HealthConfig::default(), None)
        .await
        .unwrap();
    let generation = fixture.module.status().unwrap().spawn_generation;
    fixture.module.restart(None).await.unwrap();
    fixture
        .status(|status| {
            status.state == ModuleState::Running && status.spawn_generation > generation
        })
        .await;
    assert!(fixture.holder.alive());
    fixture.pin_waitable();
    fixture.module.set_enabled(false).await.unwrap();
    fixture.holder.gone().await;
}

#[tokio::test]
async fn quarantine_refuses_sticky_carried_restart_after_config_flag_is_disabled() {
    let fixture = Fixture::new("carried-sticky-quarantine", 3, Duration::from_millis(5)).await;
    let mut changed = fixture.spec.clone();
    changed.carry_containment = false;
    fixture
        .module
        .update_configuration(changed.clone(), HealthConfig::default(), None)
        .await
        .unwrap();
    let before = fixture.module.status().unwrap();
    fixture.supervisor.child_roster.quarantine_failed_admission(
        "another-carried-module",
        &io::Error::other("injected sticky-container recovery-store failure"),
    );
    assert!(crate::live_children::quarantine_block(&fixture.record).is_some());
    let blocked = fixture.module.status().unwrap();
    let error = fixture.module.restart(None).await.unwrap_err();
    assert!(error.to_string().contains("offline clearance"));
    assert!(
        blocked
            .waiting_for_previous_detail
            .as_ref()
            .is_some_and(|detail| detail.contains("failed late admission write")
                && detail.contains("offline clearance")),
        "status must report quarantine for a still-carried container, not just its current flag"
    );
    let after = fixture.module.status().unwrap();
    assert_eq!(after.spawn_generation, before.spawn_generation);
    assert_eq!(after.pid, before.pid);
    assert!(fixture.holder.alive());
    assert!(fixture.first_leader.alive());
    assert!(matches!(
        subc_os::child_exit::observe(before.pid.unwrap()),
        Ok(None)
    ));

    changed.module_id = "ordinary-during-sticky-quarantine".into();
    changed.program = "/bin/sleep".into();
    changed.args = vec!["60".into()];
    let ordinary = fixture
        .supervisor
        .spawn(changed)
        .expect("an ordinary module with no carried container must remain unaffected");
    ordinary.stop().await.unwrap();
    fixture.module.stop().await.unwrap();
    fixture.holder.gone().await;
}

#[tokio::test]
async fn carried_health_restart_keeps_holder() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fixture = Fixture::new("carried-health", 3, Duration::from_millis(5)).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut first = true;
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut method = [0; 4];
            stream.read_exact(&mut method).await.unwrap();
            assert_eq!(&method, b"GET ");
            let response: &[u8] = if first {
                b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            first = false;
            let _ = stream.write_all(response).await;
        }
    });
    let generation = fixture.module.status().unwrap().spawn_generation;
    fixture
        .module
        .update_configuration(
            fixture.spec.clone(),
            HealthConfig {
                http: Some(format!("http://{address}/health")),
                cadence: Duration::from_millis(50),
                deadline: Duration::from_millis(100),
                failure_threshold: 1,
                on_failing: HealthAction::Restart,
                ..HealthConfig::default()
            },
            None,
        )
        .await
        .unwrap();
    fixture
        .status(|status| {
            status.state == ModuleState::Running && status.spawn_generation > generation
        })
        .await;
    assert!(fixture.holder.alive());
    fixture.pin_waitable();
    fixture.module.stop().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn carried_registration_failure_terminates_holder() {
    let fixture = Fixture::new("carried-no-hello", 0, Duration::from_millis(5)).await;
    let mut wire_spec = fixture.spec.clone();
    wire_spec.protocol = ModuleProtocol::Subc;
    fixture
        .module
        .update_configuration(wire_spec, HealthConfig::default(), None)
        .await
        .unwrap();
    fixture.module.restart(None).await.unwrap();
    fixture
        .status(|status| status.state == ModuleState::Failed)
        .await;
    fixture.holder.gone().await;
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn carried_replacement_spawn_failure_terminates_holder() {
    let fixture = Fixture::new("carried-spawn-failure", 3, Duration::from_millis(5)).await;
    let mut missing = fixture.spec.clone();
    missing.program = fixture._dir.join("missing-executable");
    fixture
        .module
        .update_configuration(missing, HealthConfig::default(), None)
        .await
        .unwrap();
    fixture.module.restart(None).await.unwrap();
    fixture
        .status(|status| status.state == ModuleState::Failed)
        .await;
    fixture.holder.gone().await;
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn carried_reload_respawn_keeps_holder() {
    let fixture = Fixture::new("carried-reload", 3, Duration::from_millis(5)).await;
    let mut replacement = fixture.spec.clone();
    replacement.protocol = ModuleProtocol::Subc;
    fixture
        .module
        .update_configuration(replacement, HealthConfig::default(), None)
        .await
        .unwrap();
    let generation = fixture.module.status().unwrap().spawn_generation;
    let connection = crate::registry::ConnectionId::new(41);
    // The shell does not implement HELLO. Supply the replacement's Registry
    // transition directly: this unit test checks container lifetime, while the
    // transport integration tests exercise registration over the wire.
    let (reloaded, ()) = tokio::join!(fixture.module.reload(), async {
        fixture
            .status(|status| status.pid.is_some() && status.spawn_generation > generation)
            .await;
        fixture
            .supervisor
            .registry
            .register_with_control_ops(
                subc_protocol::manifest::ModuleManifest::builder(&fixture.spec.module_id, "0.1.0")
                    .build(),
                1,
                connection,
                Vec::new(),
            )
            .unwrap();
    });
    reloaded.unwrap();
    assert!(fixture.holder.alive());
    fixture.pin_waitable();
    fixture
        .supervisor
        .registry
        .deregister_connection(connection)
        .unwrap();
    fixture.module.stop().await.unwrap();
    fixture.holder.gone().await;
}

#[tokio::test]
async fn carried_retire_during_backoff_terminates_holder() {
    let fixture = Fixture::new("carried-retire-backoff", 3, Duration::from_secs(60)).await;
    fixture.backoff().await;
    fixture.module.retire_requested(None).await.unwrap();
    fixture.holder.gone().await;
    assert!(crate::live_children::read_record(&fixture.record)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn carried_closed_command_channel_terminates_holder() {
    let mut fixture = Fixture::new("carried-closed-commands", 3, Duration::ZERO).await;
    let inner =
        Arc::get_mut(&mut fixture.module.inner).expect("the fixture owns the only module handle");
    let (unrelated, _receiver) = mpsc::channel(1);
    // Replace the sole command sender without dropping the module/monitor.
    // Receiver closure must terminate the container without cancelling its monitor task.
    drop(std::mem::replace(&mut inner.commands, unrelated));
    fixture.holder.gone().await;
    timeout(Duration::from_secs(5), async {
        while !fixture
            .module
            .inner
            .monitor
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_finished()
        {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(crate::live_children::read_record(&fixture.record)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn carried_clean_subc_exit_terminates_holder() {
    let fixture = Fixture::new("carried-clean-subc-exit", 3, Duration::from_millis(5)).await;
    let trigger = fixture._dir.join("exit-cleanly");
    let mut spec = fixture.spec.clone();
    spec.protocol = ModuleProtocol::Subc;
    spec.args = vec![
        "-c".into(),
        "while [ ! -f \"$1\" ]; do /bin/sleep 0.01; done; exit 0".into(),
        "clean-exit".into(),
        trigger.display().to_string(),
    ];
    fixture
        .module
        .update_configuration(spec, HealthConfig::default(), None)
        .await
        .unwrap();
    let first = fixture.module.status().unwrap().spawn_generation;
    fixture.module.restart(None).await.unwrap();
    let running = fixture
        .status(|status| status.pid.is_some() && status.spawn_generation > first)
        .await;
    let generation = running.spawn_generation;
    assert!(fixture.holder.alive());
    let connection = crate::registry::ConnectionId::new(91);
    fixture
        .supervisor
        .registry
        .register_with_control_ops(
            subc_protocol::manifest::ModuleManifest::builder(&fixture.spec.module_id, "0.1.0")
                .build(),
            1,
            connection,
            Vec::new(),
        )
        .unwrap();
    std::fs::write(trigger, "exit").unwrap();
    fixture
        .status(|status| status.state == ModuleState::Stopped)
        .await;
    fixture
        .supervisor
        .registry
        .deregister_connection(connection)
        .unwrap();
    notify_registration_release();
    fixture.holder.gone().await;
    let stopped = fixture.module.status().unwrap();
    assert_eq!(stopped.spawn_generation, generation);
    assert_eq!(stopped.last_exit.as_ref().unwrap().kind, ExitKind::Clean);
    assert!(
        !lock_snapshot(&fixture.module.inner.snapshot)
            .unwrap()
            .respawn_pending
    );
}

#[tokio::test]
async fn carried_operator_restart_failure_after_ack_terminates_holder() {
    let fixture = Fixture::new("carried-ack-then-spawn-fail", 3, Duration::from_millis(5)).await;
    let mut spec = fixture.spec.clone();
    spec.program = fixture._dir.join("program-does-not-exist");
    fixture
        .module
        .update_configuration(spec, HealthConfig::default(), None)
        .await
        .unwrap();
    fixture
        .module
        .restart(None)
        .await
        .expect("restart acknowledges initiation, before the failed spawn");
    fixture
        .status(|status| status.state == ModuleState::Failed)
        .await;
    fixture.holder.gone().await;
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn carried_registration_release_failure_terminates_holder() {
    let fixture = Fixture::new("carried-release-failure", 3, Duration::from_millis(5)).await;
    let connection = crate::registry::ConnectionId::new(92);
    // No forwarding endpoint owns this synthetic registration, so requesting
    // its close cannot deregister it. Keep it held through the restart's grace.
    fixture
        .supervisor
        .registry
        .register_with_control_ops(
            subc_protocol::manifest::ModuleManifest::builder(&fixture.spec.module_id, "0.1.0")
                .build(),
            1,
            connection,
            Vec::new(),
        )
        .unwrap();
    fixture.module.restart(None).await.unwrap();
    fixture
        .status(|status| status.state == ModuleState::Failed)
        .await;
    fixture.holder.gone().await;
    fixture
        .supervisor
        .registry
        .deregister_connection(connection)
        .unwrap();
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn carried_reload_registration_timeout_terminates_holder() {
    let fixture = Fixture::new("carried-reload-timeout", 3, Duration::from_millis(5)).await;
    let error = fixture
        .module
        .reload()
        .await
        .expect_err("the replacement never registers");
    assert!(
        matches!(error, SuperviseError::ReloadFailed { ref reason, .. } if reason.contains("did not register")),
        "{error:?}"
    );
    fixture.holder.gone().await;
    fixture.module.stop().await.unwrap();
}

static JOIN_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static FAILED_FRESH_SPAWNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(super) fn record_spawn_attempt(spec: &ModuleSpec, command: &mut Command) {
    if spec.module_id == "carried-real-join-and-fresh-fail" {
        FAILED_FRESH_SPAWNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some((_, path)) = spec
            .env
            .iter()
            .find(|(name, _)| name == "SUBC_TEST_BAD_SPAWN_DIRECTORY")
        {
            // A child-side chdir failure reaches Command::spawn's real error
            // branch even after the program identity has been validated.
            command.current_dir(path);
        }
    }
}

#[tokio::test]
async fn carried_real_join_error_and_failed_fresh_spawn_do_not_retry_twice() {
    FAILED_FRESH_SPAWNS.store(0, std::sync::atomic::Ordering::SeqCst);
    let fixture = Fixture::new("carried-real-join-and-fresh-fail", 3, Duration::ZERO).await;
    let mut spec = fixture.spec.clone();
    spec.env.push((
        "SUBC_TEST_BAD_SPAWN_DIRECTORY".into(),
        fixture
            ._dir
            .join("missing-spawn-directory")
            .display()
            .to_string(),
    ));
    fixture
        .module
        .update_configuration(spec, HealthConfig::default(), None)
        .await
        .unwrap();
    fixture.first_leader.kill();
    fixture
        .status(|status| status.state == ModuleState::Failed)
        .await;
    fixture.holder.gone().await;
    assert_eq!(
        FAILED_FRESH_SPAWNS.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "initial spawn, one real failed group join, and exactly one failed fresh attempt"
    );
    fixture.module.stop().await.unwrap();
}

pub(super) fn join_failure(spec: &ModuleSpec, roster: &ChildRoster) -> Option<io::Error> {
    if spec.module_id != "carried-group-join-retry" {
        return None;
    }
    JOIN_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    containment::current(&roster.containment)
        .is_some_and(|container| container.group().is_some())
        .then(|| io::Error::other("injected carried-group join failure"))
}

#[tokio::test]
async fn carried_group_join_error_retries_exactly_one_fresh_container() {
    JOIN_ATTEMPTS.store(0, std::sync::atomic::Ordering::SeqCst);
    let fixture = Fixture::new("carried-group-join-retry", 3, Duration::from_millis(5)).await;
    assert_eq!(JOIN_ATTEMPTS.load(std::sync::atomic::Ordering::SeqCst), 1);
    let initial = fixture.module.status().unwrap();
    fixture.first_leader.kill();
    let replacement = fixture
        .status(|status| {
            status.state == ModuleState::Running
                && status.pid.is_some()
                && status.spawn_generation > initial.spawn_generation
        })
        .await;
    assert_eq!(
        JOIN_ATTEMPTS.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "one failed carry and exactly one fresh spawn, after the initial generation"
    );
    assert_ne!(replacement.pid, initial.pid);
    assert_eq!(
        replacement.restart_count, 1,
        "the fresh retry belongs to the same crash restart, not a second budget entry"
    );
    fixture.holder.gone().await;
    let entries = crate::live_children::read_record(&fixture.record).unwrap();
    let container = entries
        .iter()
        .find_map(|entry| entry.container.as_ref())
        .unwrap();
    assert_eq!(
        container.pgid, replacement.pid,
        "the fresh container must be anchored by the replacement, not the old P1"
    );
    assert!(
        subc_os::child_exit::observe(initial.pid.unwrap()).is_err(),
        "the old pin must be reaped only after its terminal cleanup"
    );
    fixture.module.stop().await.unwrap();
}
