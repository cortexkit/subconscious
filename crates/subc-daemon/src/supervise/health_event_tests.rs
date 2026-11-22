use super::*;
use cortexkit_test_support::ScratchDir;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use subc_protocol::{manifest::Concurrency, session::ModuleControlResponse};

struct Actor {
    module: SupervisedModule,
    registry: Arc<Registry>,
    forwarding: Arc<ForwardingTable>,
    spec: ModuleSpec,
    health: HealthConfig,
    rx: mpsc::Receiver<crate::router::OutboundFrame>,
    roster: ChildRoster,
    home: ScratchDir,
}

struct TermIgnoringHelper(std::process::Child);

impl TermIgnoringHelper {
    fn start(group: u32, ready: &std::path::Path) -> Self {
        // Run in the leader's group but hold our own child handle for cleanup.
        Self(
            std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    "trap '' TERM; : > \"$1\"; exec /bin/sleep 120",
                    "helper",
                ])
                .arg(ready)
                .process_group(i32::try_from(group).unwrap())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
}

impl Drop for TermIgnoringHelper {
    fn drop(&mut self) {
        // Do not signal a numeric group after the leader has been reaped.
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

async fn wait_for<T>(reason: &str, mut observe: impl FnMut() -> Option<T>) -> T {
    // OS exec and socket readiness are real I/O. Stay runnable while waiting
    // for an observed condition, rather than letting paused time auto-advance
    // deadlines or assuming a fixed number of yields finishes the work.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let tick = Instant::now();
    loop {
        if let Some(value) = observe() {
            assert_eq!(Instant::now(), tick, "{reason} must not wait for a timer");
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {reason}"
        );
        tokio::task::yield_now().await;
    }
}

impl Actor {
    async fn start(protocol: ModuleProtocol, cadence: Duration) -> Self {
        Self::start_with_policy(
            protocol,
            cadence,
            DEFAULT_REGISTER_TIMEOUT,
            RestartPolicy::new(3, Duration::ZERO),
        )
        .await
    }

    async fn start_with_policy(
        protocol: ModuleProtocol,
        cadence: Duration,
        register_timeout: Duration,
        policy: RestartPolicy,
    ) -> Self {
        let home = ScratchDir::new("health-events");
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let health = HealthConfig {
            cadence,
            deadline: Duration::from_secs(3600),
            ..HealthConfig::default()
        };
        let supervisor = Supervisor::new_for_test(registry.clone(), policy)
            .with_forwarding(forwarding.clone())
            .with_health_config(health.clone());
        let spec = ModuleSpec {
            script: None,
            module_id: "event-health-child".into(),
            // `/bin/sleep` itself, not `/bin/sh -c "exec sleep"`: on macOS
            // the privacy trampoline refuses a module whose running image
            // is not the configured program, and `start` below waits for
            // the process to be confirmed (`reported_pid`). The shell form
            // passed only while the image check ran before the shell had
            // replaced itself with `sleep` (see
            // `record_write_stall_tests::sleeping_spec`).
            program: PathBuf::from("/bin/sleep"),
            // The process stays alive but has no real bus peer. Registrations
            // below exercise the same registry writes as accepted HELLOs.
            args: vec!["600".into()],
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| (key.into(), home.path().display().to_string()))
                .collect(),
            reserved: false,
            reserved_prefixes: vec![],
            protocol,
            overlap: Default::default(),
            carry_containment: false,
        };
        let module = supervisor
            .supervise_configured_requested(
                spec.clone(),
                true,
                health.clone(),
                Some(0),
                policy,
                register_timeout,
                None,
            )
            .unwrap();
        let (_, rx) = mpsc::channel(8);
        let actor = Self {
            module,
            registry,
            forwarding,
            spec,
            health,
            rx,
            roster: supervisor.child_roster.clone(),
            home,
        };
        actor
            .wait_for_select(|checkpoint| checkpoint.generation > 0)
            .await;
        actor
    }

    async fn expire_registration_wait(&self, remaining: Duration) {
        let pid = self.module.status().unwrap().pid.unwrap();
        let group = self.roster.plain_group_for_test(pid).unwrap();
        tokio::time::advance(remaining).await;
        let grace = wait_for("registration timeout group TERM", || {
            group.remaining_term_grace_for_test()
        })
        .await;
        // A sample partway through the grace can still see a live member and
        // arm another poll timer. Advance to the deadline before waiting for I/O.
        if !grace.is_zero() {
            tokio::time::advance(grace).await;
        }
    }

    fn turns(&self) -> u64 {
        lock_snapshot(&self.module.inner.snapshot)
            .unwrap()
            .actor_turns
    }

    async fn wait_for_select(
        &self,
        expected: impl Fn(&ActorSelectCheckpoint) -> bool,
    ) -> ActorSelectCheckpoint {
        wait_for("actor select checkpoint", || {
            let state = lock_snapshot(&self.module.inner.snapshot).unwrap();
            state.actor_select.filter(|checkpoint| {
                checkpoint.generation == state.spawn_generation
                    && state.state == ModuleState::Running
                    && state.process_alive
                    && state.reported_pid().is_some()
                    && expected(checkpoint)
            })
        })
        .await
    }

    fn hello(&mut self, connection: u64, health: bool) {
        let manifest =
            subc_protocol::manifest::ModuleManifest::builder(&self.spec.module_id, "1.0.0")
                .protocol_ver(subc_protocol::PROTOCOL_VERSION)
                .build();
        let connection = ConnectionId::new(connection);
        let (tx, rx) = mpsc::channel(8);
        self.rx = rx;
        self.forwarding
            .register_module_connection(
                connection,
                self.spec.module_id.clone(),
                subc_protocol::PROTOCOL_VERSION,
                Concurrency::ModuleManaged,
                FrameSink::new(tx),
            )
            .unwrap();
        self.registry
            .register_with_control_ops(
                manifest,
                subc_protocol::PROTOCOL_VERSION,
                connection,
                if health {
                    vec![MODULE_CONTROL_OP_HEALTH_CHECK.into()]
                } else {
                    vec![]
                },
            )
            .unwrap();
    }

    async fn probe(&mut self) -> crate::router::OutboundFrame {
        let frame = wait_for("outbound health probe", || match self.rx.try_recv() {
            Ok(frame) => Some(frame),
            Err(mpsc::error::TryRecvError::Empty) => None,
            Err(mpsc::error::TryRecvError::Disconnected) => panic!("health peer disconnected"),
        })
        .await;
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["op"], MODULE_CONTROL_OP_HEALTH_CHECK);
        frame
    }

    fn no_probe(&mut self) {
        assert!(matches!(
            self.rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    fn answer(&self, connection: u64, frame: crate::router::OutboundFrame) {
        self.forwarding
            .complete_module_control_rpc(
                ConnectionId::new(connection),
                frame.header.corr,
                Some(MODULE_CONTROL_OP_HEALTH_CHECK),
                ModuleControlRpcOutcome::Response(ModuleControlResponse::HealthCheck {
                    status: HealthStatus::Ok,
                    detail: None,
                    metrics: None,
                }),
            )
            .unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn registration_timeout_kills_at_120_seconds_and_respawns() {
    let actor = Actor::start(ModuleProtocol::Subc, Duration::from_secs(30)).await;
    let initial = actor.module.status().unwrap();
    tokio::time::advance(Duration::from_secs(119)).await;
    tokio::task::yield_now().await;
    assert_eq!(actor.module.status().unwrap().pid, initial.pid);
    assert!(actor.module.terminal_history().entries.is_empty());
    actor.expire_registration_wait(Duration::from_secs(1)).await;
    actor
        .wait_for_select(|at| at.generation > initial.spawn_generation)
        .await;
    let status = actor.module.status().unwrap();
    assert_ne!(status.pid, initial.pid);
    assert!(!cortexkit_test_support::process_alive(
        initial.pid.unwrap() as i32
    ));
    assert_eq!(status.restart_count, 1);
    assert_eq!(status.last_exit.unwrap().kind, ExitKind::Crash);
    let entries = actor.module.terminal_history().entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].disposition_detail.as_deref(),
        Some("registration_timeout")
    );
    actor.module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn registration_timeout_is_cancelled_by_hello_at_119_seconds() {
    let mut actor = Actor::start(ModuleProtocol::Subc, Duration::from_secs(30)).await;
    let pid = actor.module.status().unwrap().pid;
    tokio::time::advance(Duration::from_secs(119)).await;
    actor.hello(1, false);
    let registered = actor
        .wait_for_select(|at| at.registered_connection.is_some())
        .await;
    assert_eq!(
        registered.wake_after, None,
        "HELLO must cancel the deadline"
    );
    tokio::time::advance(Duration::from_secs(300)).await;
    tokio::task::yield_now().await;
    assert_eq!(actor.module.status().unwrap().pid, pid);
    assert!(actor.module.terminal_history().entries.is_empty());
    // A disconnect after the first HELLO must not re-arm a startup timer.
    actor
        .registry
        .deregister_connection(ConnectionId::new(1))
        .unwrap();
    actor
        .wait_for_select(|at| at.registered_connection.is_none())
        .await;
    tokio::time::advance(Duration::from_secs(300)).await;
    tokio::task::yield_now().await;
    assert_eq!(actor.module.status().unwrap().pid, pid);
    assert!(actor.module.terminal_history().entries.is_empty());
    actor.module.stop().await.unwrap();

    let mut transient = Actor::start(ModuleProtocol::Subc, Duration::from_secs(30)).await;
    let pid = transient.module.status().unwrap().pid;
    tokio::time::advance(Duration::from_secs(119)).await;
    let turns = transient.turns();
    // Both writes happen before the supervisor can refresh the registry.
    // An accepted HELLO must remain evidence after its connection is gone.
    transient.hello(1, false);
    transient
        .registry
        .deregister_connection(ConnectionId::new(1))
        .unwrap();
    let parked = transient.wait_for_select(|at| at.turn > turns).await;
    assert_eq!(
        parked.wake_after, None,
        "transient HELLO must cancel the deadline"
    );
    tokio::time::advance(Duration::from_secs(300)).await;
    tokio::task::yield_now().await;
    assert_eq!(transient.module.status().unwrap().pid, pid);
    assert!(transient.module.terminal_history().entries.is_empty());
    transient.module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn registration_timeouts_exhaust_budget_and_schedule_slow_retry() {
    let actor = Actor::start_with_policy(
        ModuleProtocol::Subc,
        Duration::from_secs(30),
        Duration::from_secs(10),
        RestartPolicy::new(1, Duration::ZERO),
    )
    .await;
    let generation = actor.module.status().unwrap().spawn_generation;
    actor
        .expire_registration_wait(Duration::from_secs(10))
        .await;
    actor.wait_for_select(|at| at.generation > generation).await;
    // Keep the second group live across TERM and intermediate membership
    // polls. A cooperative leader can disappear before even the first poll.
    let ready = actor.home.join("helper.ready");
    let mut helper = TermIgnoringHelper::start(actor.module.status().unwrap().pid.unwrap(), &ready);
    wait_for("TERM-ignoring group member to be ready", || {
        assert!(helper.0.try_wait().unwrap().is_none());
        ready.exists().then_some(())
    })
    .await;
    actor
        .expire_registration_wait(Duration::from_secs(10))
        .await;
    let retry = wait_for("failed retry schedule", || {
        let state = lock_snapshot(&actor.module.inner.snapshot).unwrap();
        (state.state == ModuleState::Failed)
            .then_some(state.failed_retry.pending)
            .flatten()
    })
    .await;
    let helper_exit = wait_for("TERM-ignoring group member to exit", || {
        helper.0.try_wait().unwrap()
    })
    .await;
    assert_eq!(helper_exit.signal(), Some(9));
    assert_eq!(retry.delay, Duration::from_secs(600));
    assert_eq!(retry.attempt, 1);
    let entries = actor.module.terminal_history().entries;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].disposition, TerminalDisposition::Failed);
    assert!(entries[1]
        .disposition_detail
        .as_deref()
        .unwrap()
        .contains("registration_timeout"));
    tokio::time::advance(Duration::from_secs(599)).await;
    tokio::task::yield_now().await;
    assert_eq!(actor.module.state().unwrap(), ModuleState::Failed);
    tokio::time::advance(Duration::from_secs(1)).await;
    actor
        .wait_for_select(|at| at.generation > generation + 1)
        .await;
    // The slow retry is another ordinary spawn, with its own HELLO budget.
    actor
        .expire_registration_wait(Duration::from_secs(10))
        .await;
    wait_for("slow retry registration timeout", || {
        (actor.module.terminal_history().entries.len() == 3).then_some(())
    })
    .await;
    actor.module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn registration_timeout_honours_ten_second_override() {
    let actor = Actor::start_with_policy(
        ModuleProtocol::Subc,
        Duration::from_secs(30),
        Duration::from_secs(10),
        RestartPolicy::new(3, Duration::ZERO),
    )
    .await;
    let generation = actor.module.status().unwrap().spawn_generation;
    tokio::time::advance(Duration::from_secs(9)).await;
    tokio::task::yield_now().await;
    assert!(actor.module.terminal_history().entries.is_empty());
    actor.expire_registration_wait(Duration::from_secs(1)).await;
    actor.wait_for_select(|at| at.generation > generation).await;
    assert_eq!(actor.module.terminal_history().entries.len(), 1);
    actor.module.stop().await.unwrap();
}

#[tokio::test]
async fn registration_timeout_kills_term_ignoring_helper_before_reaping_leader() {
    let home = ScratchDir::new("registration-timeout-helper");
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let spec = ModuleSpec {
        script: None,
        module_id: "registration-timeout-group".into(),
        program: "/bin/sleep".into(),
        args: vec!["120".into()],
        env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.into(), home.path().display().to_string()))
            .collect(),
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let module = supervisor
        .supervise_configured_requested(
            spec,
            true,
            HealthConfig::default(),
            None,
            RestartPolicy::new(0, Duration::ZERO),
            Duration::from_secs(10),
            None,
        )
        .unwrap();
    let leader = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pid) = module.status().unwrap().pid {
                break pid;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("supervised group leader must be reported before the deadline");
    assert!(leader > 1);
    let ready = home.join("helper.ready");
    let mut helper = TermIgnoringHelper::start(leader, &ready);
    timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            assert!(helper.0.try_wait().unwrap().is_none());
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("helper must install TERM ignore before the registration deadline");
    let helper_pid = rustix::process::Pid::from_raw(i32::try_from(helper.0.id()).unwrap()).unwrap();
    rustix::process::kill_process(helper_pid, rustix::process::Signal::TERM).unwrap();
    sleep(Duration::from_millis(20)).await;
    assert!(
        helper.0.try_wait().unwrap().is_none(),
        "helper must ignore TERM"
    );

    timeout(Duration::from_secs(20), async {
        while module.state().unwrap() != ModuleState::Failed {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("registration timeout must stop and reap the group leader");
    let helper_exit = timeout(Duration::from_secs(3), async {
        loop {
            if let Some(status) = helper.0.try_wait().unwrap() {
                break status;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("TERM-ignoring helper survived the registration timeout's group KILL");
    assert_eq!(helper_exit.signal(), Some(9));
    assert!(!cortexkit_test_support::process_alive(
        i32::try_from(leader).unwrap()
    ));
    assert_eq!(
        module.status().unwrap().last_exit.unwrap().kind,
        ExitKind::Crash
    );
    assert!(module.terminal_history().entries[0]
        .disposition_detail
        .as_deref()
        .unwrap()
        .contains("registration_timeout"));
    module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn registration_timeout_does_not_count_an_earlier_exit() {
    let actor = Actor::start_with_policy(
        ModuleProtocol::Subc,
        Duration::from_secs(30),
        DEFAULT_REGISTER_TIMEOUT,
        RestartPolicy::new(0, Duration::ZERO),
    )
    .await;
    let pid = actor.module.status().unwrap().pid.unwrap();
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    wait_for("early exit", || {
        (actor.module.state().unwrap() == ModuleState::Failed).then_some(())
    })
    .await;
    tokio::time::advance(Duration::from_secs(121)).await;
    tokio::task::yield_now().await;
    let entries = actor.module.terminal_history().entries;
    assert_eq!(entries.len(), 1);
    assert!(!entries[0]
        .disposition_detail
        .as_deref()
        .unwrap()
        .contains("registration_timeout"));
    assert_eq!(actor.module.status().unwrap().lifetime_restarts, 0);
    actor.module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn registration_timeout_is_cancelled_by_daemon_shutdown() {
    let actor = Actor::start(ModuleProtocol::Subc, Duration::from_secs(30)).await;
    let pid = actor.module.status().unwrap().pid;
    tokio::time::advance(Duration::from_secs(119)).await;
    actor.roster.close();
    let turns = actor.turns();
    tokio::time::advance(Duration::from_secs(300)).await;
    let parked = actor.wait_for_select(|at| at.turn > turns).await;
    assert_eq!(parked.wake_after, None);
    assert_eq!(actor.module.status().unwrap().pid, pid);
    assert!(actor.module.terminal_history().entries.is_empty());
    actor.module.stop().await.unwrap();
    let status = actor.module.status().unwrap();
    assert_eq!(status.restart_count, 0);
    assert_eq!(
        actor.module.terminal_history().entries[0].disposition,
        TerminalDisposition::DaemonShutdown
    );
}

#[tokio::test(start_paused = true)]
async fn no_health_children_park_for_sixty_seconds() {
    let none = Actor::start(ModuleProtocol::None, Duration::from_secs(30)).await;
    let unregistered = Actor::start(ModuleProtocol::Subc, Duration::from_secs(30)).await;
    let mut unadvertised = Actor::start(ModuleProtocol::Subc, Duration::from_secs(30)).await;
    unadvertised.hello(1, false);
    // Include any one-off registration wake in the budget; receiving a
    // no-health HELLO must not turn a parked child into a polling child.
    // Advance in 10 ms steps so the old poll really executes ~6,000 turns;
    // a single 60 s jump would only observe one expired timer.
    for _ in 0..6000 {
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
    }
    for actor in [&none, &unregistered, &unadvertised] {
        assert!(
            actor.turns() <= 5,
            "no-health actor woke {} times",
            actor.turns()
        );
        assert_eq!(actor.module.state().unwrap(), ModuleState::Running);
    }
}

#[tokio::test(start_paused = true)]
async fn late_hello_starts_a_due_probe_in_the_notification_tick() {
    // With a zero cadence the first probe is due at once, so this measures
    // only how fast the registration notification starts health probing,
    // not the normal 30-33 s wait before a first probe (checked below).
    let mut actor = Actor::start(ModuleProtocol::Subc, Duration::ZERO).await;
    let tick = Instant::now();
    actor.hello(1, true);
    actor.probe().await;
    assert_eq!(Instant::now(), tick, "HELLO must not wait for a poll");
}

#[tokio::test(start_paused = true)]
async fn hello_keeps_the_jittered_cadence_and_catalog_updates_do_not_reset_it() {
    let mut actor = Actor::start(ModuleProtocol::Subc, Duration::from_secs(30)).await;
    let hello_tick = Instant::now();
    actor.hello(1, true);
    let armed = actor
        .wait_for_select(|checkpoint| {
            checkpoint.registered_connection == Some(ConnectionId::new(1))
                && checkpoint.next_probe_at.is_some()
        })
        .await;
    let delay = jittered_health_delay(&actor.spec.module_id, 0, actor.health.cadence);
    assert!((Duration::from_secs(30)..Duration::from_secs(33)).contains(&delay));
    let first_deadline = hello_tick + delay;
    assert_eq!(armed.next_probe_at, Some(first_deadline));
    tokio::time::advance(Duration::from_secs(29)).await;
    let turns = actor.turns();
    actor
        .registry
        .replace_catalog_for_connection(ConnectionId::new(1), vec![], None, Some(true))
        .unwrap();
    let updated = actor
        .wait_for_select(|checkpoint| checkpoint.turn > turns)
        .await;
    assert_eq!(
        updated.next_probe_at,
        Some(first_deadline),
        "catalog update must not reset cadence"
    );
    actor.no_probe();
    tokio::time::advance(first_deadline - Instant::now()).await;
    let frame = actor.probe().await;
    actor.answer(1, frame);
    let next = jittered_health_delay(&actor.spec.module_id, 1, actor.health.cadence);
    let next_deadline = Instant::now() + next;
    let rearmed = actor
        .wait_for_select(|checkpoint| {
            checkpoint
                .next_probe_at
                .is_some_and(|deadline| deadline > first_deadline)
        })
        .await;
    assert_eq!(rearmed.next_probe_at, Some(next_deadline));
    tokio::time::advance(next - Duration::from_nanos(1)).await;
    actor.no_probe();
    tokio::time::advance(Duration::from_nanos(1)).await;
    actor.probe().await;
}

#[tokio::test(start_paused = true)]
async fn disconnect_disarms_and_reconnect_rearms_health() {
    let mut actor = Actor::start(ModuleProtocol::Subc, Duration::from_secs(30)).await;
    actor.hello(1, true);
    actor
        .wait_for_select(|checkpoint| {
            checkpoint.registered_connection == Some(ConnectionId::new(1))
                && checkpoint.next_probe_at.is_some()
        })
        .await;
    let turns = actor.turns();
    let tick = Instant::now();
    actor
        .registry
        .deregister_connection(ConnectionId::new(1))
        .unwrap();
    let disarmed = actor
        .wait_for_select(|checkpoint| {
            checkpoint.turn > turns && checkpoint.registered_connection.is_none()
        })
        .await;
    assert_eq!(disarmed.next_probe_at, None);
    assert_eq!(disarmed.wake_after, None);
    assert_eq!(
        Instant::now(),
        tick,
        "disconnect must disarm in the notification tick"
    );
    tokio::time::advance(Duration::from_secs(60)).await;
    actor.no_probe();
    let reconnect_tick = Instant::now();
    actor.hello(2, true);
    let rearmed = actor
        .wait_for_select(|checkpoint| {
            checkpoint.registered_connection == Some(ConnectionId::new(2))
                && checkpoint.next_probe_at.is_some()
        })
        .await;
    let delay = jittered_health_delay(&actor.spec.module_id, 0, actor.health.cadence);
    assert_eq!(rearmed.next_probe_at, Some(reconnect_tick + delay));
    tokio::time::advance(delay).await;
    actor.probe().await;
}

#[tokio::test(start_paused = true)]
async fn reregistration_without_health_disarms_the_old_schedule() {
    let mut actor = Actor::start(ModuleProtocol::Subc, Duration::from_secs(30)).await;
    actor.hello(1, true);
    actor
        .wait_for_select(|checkpoint| {
            checkpoint.registered_connection == Some(ConnectionId::new(1))
                && checkpoint.next_probe_at.is_some()
        })
        .await;
    actor
        .registry
        .deregister_connection(ConnectionId::new(1))
        .unwrap();
    actor.hello(2, false);
    let disarmed = actor
        .wait_for_select(|checkpoint| {
            checkpoint.registered_connection == Some(ConnectionId::new(2))
        })
        .await;
    assert_eq!(disarmed.next_probe_at, None);
    assert_eq!(disarmed.wake_after, None);
    tokio::time::advance(Duration::from_secs(60)).await;
    actor.no_probe();
}

#[tokio::test(start_paused = true)]
async fn restart_waits_for_its_own_hello_before_rearming_health() {
    let mut actor = Actor::start(ModuleProtocol::Subc, Duration::from_secs(30)).await;
    actor.hello(1, true);
    let armed = actor
        .wait_for_select(|checkpoint| {
            checkpoint.registered_connection == Some(ConnectionId::new(1))
                && checkpoint.next_probe_at.is_some()
        })
        .await;
    // Simulate the old process's connection closing before the restart
    // tears the process down. The replacement process has started but has
    // not sent its HELLO, so it is not registered.
    actor
        .registry
        .deregister_connection(ConnectionId::new(1))
        .unwrap();
    actor.module.restart(Some(0)).await.unwrap();
    let parked = actor
        .wait_for_select(|checkpoint| {
            checkpoint.generation > armed.generation
                && checkpoint.registered_connection.is_none()
                && checkpoint.next_probe_at.is_none()
        })
        .await;
    assert_eq!(parked.wake_after, Some(DEFAULT_REGISTER_TIMEOUT));
    actor.rx.close();
    // Drain the stop notice the restart queued for the old process's connection,
    // so it isn't mistaken for traffic to the replacement.
    while actor.rx.try_recv().is_ok() {}
    let turns = actor.turns();
    tokio::time::advance(Duration::from_secs(60)).await;
    let after = actor.turns();
    eprintln!(
        "restart park turns: before={turns} after={after} delta={}",
        after - turns
    );
    assert_eq!(after, turns, "replacement must park before HELLO");
    let hello_tick = Instant::now();
    actor.hello(2, true);
    let rearmed = actor
        .wait_for_select(|checkpoint| {
            checkpoint.registered_connection == Some(ConnectionId::new(2))
                && checkpoint.next_probe_at.is_some()
        })
        .await;
    let delay = jittered_health_delay(&actor.spec.module_id, 0, actor.health.cadence);
    assert_eq!(rearmed.next_probe_at, Some(hello_tick + delay));
    tokio::time::advance(delay).await;
    actor.probe().await;
}

#[tokio::test(start_paused = true)]
async fn http_probe_waits_a_full_cadence_for_each_spawn() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let mut actor = Actor::start(ModuleProtocol::None, Duration::from_secs(30)).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    actor.health.http = Some(format!("http://{}/healthz", listener.local_addr().unwrap()));
    let (started_tx, mut started_rx) = mpsc::channel(8);
    let peer = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            started_tx.send(Instant::now()).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        }
    });
    actor
        .module
        .update_configuration(actor.spec.clone(), actor.health.clone(), None)
        .await
        .unwrap();
    let first_tick = Instant::now();
    let first = actor
        .wait_for_select(|checkpoint| checkpoint.next_probe_at.is_some())
        .await;
    assert!(first.next_probe_at.unwrap() >= first_tick + actor.health.cadence);
    assert!(started_rx.try_recv().is_err());

    // Restart just before the old deadline: reusing it would give the new
    // process only one second to start, rather than a full cadence.
    tokio::time::advance(first.next_probe_at.unwrap() - Instant::now() - Duration::from_secs(1))
        .await;
    actor.module.restart(Some(0)).await.unwrap();
    let spawn_tick = Instant::now();
    let replacement = actor
        .wait_for_select(|checkpoint| checkpoint.generation > first.generation)
        .await;
    let deadline = replacement.next_probe_at.unwrap();
    assert!(
        deadline >= spawn_tick + actor.health.cadence,
        "a replacement must not inherit its predecessor's probe deadline"
    );
    tokio::time::advance(actor.health.cadence - Duration::from_nanos(1)).await;
    assert!(
        started_rx.try_recv().is_err(),
        "no HTTP probe during startup grace"
    );
    tokio::time::advance(deadline - Instant::now()).await;
    let landed = wait_for("first replacement HTTP probe", || {
        started_rx.try_recv().ok()
    })
    .await;
    assert!(landed >= spawn_tick + actor.health.cadence);
    wait_for("healthy replacement", || {
        (actor.module.status().unwrap().health.status == SupervisorHealthStatus::Ok).then_some(())
    })
    .await;
    peer.abort();
}

#[tokio::test(start_paused = true)]
async fn rescan_adds_and_removes_http_health_without_polling() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let mut actor = Actor::start(ModuleProtocol::None, Duration::from_secs(30)).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    actor.health.http = Some(format!("http://{}/health", listener.local_addr().unwrap()));
    // One millisecond separates successive probes without a polling delay.
    actor.health.cadence = Duration::from_millis(1);
    let (started_tx, mut started_rx) = mpsc::channel(8);
    let peer = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            started_tx.send(()).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        }
    });
    let tick = Instant::now();
    let turns = actor.turns();
    actor
        .module
        .update_configuration(actor.spec.clone(), actor.health.clone(), None)
        .await
        .unwrap();
    let armed = actor
        .wait_for_select(|checkpoint| checkpoint.turn > turns && checkpoint.next_probe_at.is_some())
        .await;
    assert_eq!(armed.next_probe_at, Some(tick + Duration::from_millis(1)));
    assert_eq!(Instant::now(), tick);
    tokio::time::advance(Duration::from_millis(1)).await;
    let mut started = false;
    wait_for("successful HTTP probe", || {
        started |= started_rx.try_recv().is_ok();
        (started && actor.module.status().unwrap().health.status == SupervisorHealthStatus::Ok)
            .then_some(())
    })
    .await;
    assert!(started, "added HTTP check must start at its first deadline");
    assert_eq!(
        actor.module.status().unwrap().health.status,
        SupervisorHealthStatus::Ok
    );
    assert_eq!(Instant::now(), tick + Duration::from_millis(1));
    actor.health.http = None;
    let turns = actor.turns();
    actor
        .module
        .update_configuration(actor.spec.clone(), actor.health.clone(), None)
        .await
        .unwrap();
    let parked = actor
        .wait_for_select(|checkpoint| checkpoint.turn > turns && checkpoint.next_probe_at.is_none())
        .await;
    assert_eq!(parked.wake_after, None);
    let turns = actor.turns();
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(
        actor.turns(),
        turns,
        "removed HTTP check must park the actor"
    );
    assert!(
        started_rx.try_recv().is_err(),
        "removed HTTP check must stay disarmed"
    );
    assert_eq!(
        actor.module.status().unwrap().health,
        ModuleHealthStatus::default()
    );
    peer.abort();
}
