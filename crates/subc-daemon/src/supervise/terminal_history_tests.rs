include!("../supervise_route_open_recovery_tests.rs");
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{sync::mpsc, time::sleep};

use super::{
    apply_deliberate_severance_marker, daemon_will_restart, drain_child_to_state,
    drained_after_quiescence_wait, handle_reload_spawn_failure, health_restart_child,
    lock_snapshot, on_child_exit, record_deliberate_severance, record_wait_error_terminal,
    reset_restart_count, spawn_and_mark_running, update_snapshot, wait_error_exit_report, ExitKind,
    ExitReport, ModuleProtocol, ModuleSpec, ModuleState, NextAction, ProcessIdentity,
    RestartPolicy, SpawnEventKind, StopNotice, SuperviseError, SupervisedConfiguration,
    SupervisedModule, SupervisedModuleInner, Supervisor, SupervisorHandle, SupervisorHealthStatus,
    SupervisorSnapshot,
};
// The supervisor's clock, distinct from the `std::time::Instant` these tests
// use for their own wall-clock deadlines: crash-restart instants must be on
// the same clock the production code stamps them with, which is tokio's (and
// is what `start_paused` tests can move).
use super::Instant as ClockInstant;
use crate::{
    registry::Registry,
    terminal_ring::{TerminalRing, TerminalRingConfig},
};
use std::sync::Mutex;
use subc_control::TerminalDisposition;

/// See the twin in `control.rs` for why this derives the path from
/// `current_exe()` and why the existence check is here: `--lib` alone does
/// not build `[[bin]]` targets, and a bare spawn then fails with a raw
/// `NotFound` that reads as a broken test rather than an unbuilt dependency.
pub(super) fn fake_aft_stub_path() -> PathBuf {
    let path = cortexkit_test_support::cargo_profile_bin("fake-aft-stub");
    assert!(
        path.exists(),
        "fake-aft-stub not built at {}: run `cargo test -p subc-core` (which builds \
             [[bin]] targets) rather than `cargo test -p subc-core --lib` (which does not)",
        path.display()
    );
    path
}

#[test]
fn reserved_never_spawned_refuses_every_hello() {
    // The canary hole: a reserved id whose module has never spawned had NO
    // gate entry and admitted anyone -- the reservation protected the nonce
    // holder, not the NAME. Now the entry is present with no legitimate
    // holder and refuses all comers.
    let supervisor = SupervisorHandle::default();
    supervisor.apply_identity_configuration(&ModuleSpec {
        script: None,
        module_id: "never-spawned".to_string(),
        program: PathBuf::from("/usr/bin/false"),
        args: Vec::new(),
        env: Vec::new(),
        reserved: true,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    });
    assert!(
        supervisor
            .reserved_hello_verification("never-spawned", Some("any-forged-nonce"))
            .is_err(),
        "forged nonce must refuse on a reserved never-spawned id"
    );
    assert!(
        supervisor
            .reserved_hello_verification("never-spawned", None)
            .is_err(),
        "absent nonce must refuse on a reserved never-spawned id"
    );
    // And a real spawn nonce minted later admits exactly that nonce.
    supervisor.set_spawn_nonce("never-spawned", "minted".to_string());
    supervisor.apply_identity_configuration(&ModuleSpec {
        script: None,
        module_id: "never-spawned".to_string(),
        program: PathBuf::from("/usr/bin/false"),
        args: Vec::new(),
        env: Vec::new(),
        reserved: true,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    });
    assert!(supervisor
        .reserved_hello_verification("never-spawned", Some("minted"))
        .is_ok());
    assert!(supervisor
        .reserved_hello_verification("never-spawned", Some("forged"))
        .is_err());
}

/// Put `count` crash restarts on a snapshot's ring as if they had all just
/// happened, which is what "spent budget" looks like to every reader.
fn seed_crash_restarts(state: &mut SupervisorSnapshot, count: u32) {
    let now = ClockInstant::now();
    for _ in 0..count {
        state.crash_restarts.push_back(now);
    }
}

/// Age the oldest recorded restart out of `window`, standing in for the hours
/// that would otherwise have to pass. Injecting the instant is the point: a
/// test that slept a real window would take ten minutes and still prove less.
fn age_oldest_crash_restart_out_of_window(state: &mut SupervisorSnapshot, window: Duration) {
    let aged = state
        .crash_restarts
        .front()
        .expect("a crash restart must be recorded before it can be aged")
        .checked_sub(window + Duration::from_secs(1))
        .expect("the test clock is far enough from its origin to age an instant");
    state.crash_restarts[0] = aged;
}

fn snapshot_with_restarts(enabled: bool, count: u32) -> SupervisorSnapshot {
    let mut state = SupervisorSnapshot::new(ModuleState::Running, enabled);
    seed_crash_restarts(&mut state, count);
    state
}

#[test]
fn daemon_owned_recovery_predicate_uses_the_pre_increment_budget() {
    let policy = RestartPolicy::new(3, Duration::ZERO);
    let now = ClockInstant::now();
    assert!(daemon_will_restart(
        &mut snapshot_with_restarts(true, 2),
        &policy,
        now
    ));
    assert!(!daemon_will_restart(
        &mut snapshot_with_restarts(true, 3),
        &policy,
        now
    ));
    assert!(!daemon_will_restart(
        &mut snapshot_with_restarts(false, 0),
        &policy,
        now
    ));
}

#[test]
fn crash_restart_backoff_escalates_with_in_window_count() {
    let policy =
        RestartPolicy::new(4, Duration::from_millis(100)).with_max_backoff(Duration::from_secs(30));
    let now = ClockInstant::now();
    let mut state = SupervisorSnapshot::new(ModuleState::Running, true);
    let schedules = (0..4)
        .map(|_| {
            let schedule = state
                .next_crash_restart(&policy, now)
                .expect("the test policy allows four crash restarts");
            state.record_crash_restart(&policy, now);
            schedule
        })
        .collect::<Vec<_>>();

    assert_eq!(
        schedules
            .iter()
            .map(|schedule| schedule.restart_in_window)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert_eq!(
        schedules
            .iter()
            .map(|schedule| schedule.delay)
            .collect::<Vec<_>>(),
        vec![
            Duration::from_millis(100),
            Duration::from_secs(1),
            Duration::from_secs(10),
            Duration::from_secs(30),
        ]
    );
}

#[test]
fn crash_restart_backoff_resets_after_ring_clear() {
    let policy = RestartPolicy::new(3, Duration::from_millis(100));
    let now = ClockInstant::now();
    let mut state = SupervisorSnapshot::new(ModuleState::Running, true);
    assert_eq!(
        state.next_crash_restart(&policy, now).unwrap().delay,
        Duration::from_millis(100)
    );
    state.record_crash_restart(&policy, now);
    assert_eq!(
        state.next_crash_restart(&policy, now).unwrap().delay,
        Duration::from_secs(1)
    );

    state.clear_crash_restarts();
    let schedule = state
        .next_crash_restart(&policy, now)
        .expect("a cleared ring must allow another restart");
    assert_eq!(schedule.restart_in_window, 0);
    assert_eq!(schedule.delay, Duration::from_millis(100));
}

#[test]
fn crash_restart_backoff_ignores_aged_restarts() {
    let policy = RestartPolicy::new(3, Duration::from_millis(100));
    let now = ClockInstant::now();
    let mut state = SupervisorSnapshot::new(ModuleState::Running, true);
    state
        .next_crash_restart(&policy, now)
        .expect("the first restart is allowed");
    state.record_crash_restart(&policy, now);
    state
        .next_crash_restart(&policy, now)
        .expect("the second restart is allowed");
    state.record_crash_restart(&policy, now);
    state.crash_restarts[0] = now
        .checked_sub(policy.window + Duration::from_secs(1))
        .expect("the fake clock can age a restart past the window");

    let schedule = state
        .next_crash_restart(&policy, now)
        .expect("an aged restart must release its slot");
    assert_eq!(schedule.restart_in_window, 1);
    assert_eq!(schedule.delay, Duration::from_secs(1));
    state.record_crash_restart(&policy, now);
    assert_eq!(state.crash_restarts.len(), 2);
}

/// The budget is a rate: the same three spent restarts refuse a respawn
/// while they are recent and allow one once they have aged past the window.
/// Nothing about the module changed in between, which is the whole point.
#[test]
fn a_budget_spent_before_the_window_no_longer_refuses() {
    let policy = RestartPolicy::new(3, Duration::ZERO);
    let mut state = snapshot_with_restarts(true, 3);
    let now = ClockInstant::now();
    assert!(!daemon_will_restart(&mut state, &policy, now));

    assert!(daemon_will_restart(
        &mut state,
        &policy,
        now + policy.window + Duration::from_secs(1)
    ));
    assert!(
        state.crash_restarts.is_empty(),
        "reading the budget must drop the instants that left the window"
    );
}

fn module_with_recovery_snapshot(
    state: ModuleState,
    enabled: bool,
    restart_count: u32,
) -> SupervisedModule {
    let registry = Arc::new(Registry::default());
    let supervisor =
        Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::new(3, Duration::ZERO));
    let runtime = supervisor.runtime_config();
    let spec = ModuleSpec {
        script: None,
        module_id: "recovery-snapshot".to_string(),
        program: fake_aft_stub_path(),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let mut snapshot = SupervisorSnapshot::new(state, enabled);
    seed_crash_restarts(&mut snapshot, restart_count);
    // These tests read synthetic snapshots. A real child and monitor would
    // race those reads by replacing the requested state during startup.
    let (commands, _rx) = mpsc::channel(4);
    let configured_enabled = snapshot.enabled;
    SupervisedModule {
        inner: Arc::new(SupervisedModuleInner {
            module_id: spec.module_id.clone(),
            registry,
            snapshot: Arc::new(Mutex::new(snapshot)),
            configuration: Arc::new(Mutex::new(SupervisedConfiguration {
                spec,
                enabled: configured_enabled,
                health: runtime.health,
                register_timeout: runtime.register_timeout,
                restart_policy: runtime.restart_policy,
            })),
            stderr_ring: runtime.stderr_ring,
            terminal_ring: runtime.terminal_ring,
            commands,
            monitor: Mutex::new(None),
            effective_drain_timeout: runtime.effective_drain_timeout,
            provenance_probe: supervisor.provenance_probe.clone(),
            retained: runtime.child_roster.retained_view(),
        }),
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn no_cgroup_placement_does_not_block_fake_aft_stub_spawn() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
            .with_cgroup_placement(None);
    let result = supervisor.spawn(ModuleSpec {
        script: None,
        module_id: "no-cgroup-placement".to_string(),
        program: fake_aft_stub_path(),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    });

    assert!(
        result.is_ok(),
        "no delegation must not turn an otherwise valid spawn into a failure: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undecided_snapshot_uses_shared_restart_predicate() {
    assert!(module_with_recovery_snapshot(ModuleState::Running, true, 2)
        .will_recover_after_connection_loss()
        .unwrap());
    assert!(
        !module_with_recovery_snapshot(ModuleState::Running, true, 3)
            .will_recover_after_connection_loss()
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restarting_snapshot_at_exhausted_budget_is_non_terminal() {
    assert!(
        module_with_recovery_snapshot(ModuleState::Restarting, true, 3)
            .will_recover_after_connection_loss()
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_phase_snapshots_are_terminal_before_budget_exhaustion() {
    assert!(!module_with_recovery_snapshot(ModuleState::Failed, true, 0)
        .will_recover_after_connection_loss()
        .unwrap());
    assert!(
        !module_with_recovery_snapshot(ModuleState::Disabled, true, 0)
            .will_recover_after_connection_loss()
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warming_snapshot_is_limited_to_startup_phases() {
    for state in [
        ModuleState::Starting,
        ModuleState::Running,
        ModuleState::Restarting,
    ] {
        assert!(
            module_with_recovery_snapshot(state, true, 0)
                .is_warming()
                .unwrap(),
            "{state:?} should be warming"
        );
    }
    for state in [
        ModuleState::Unresponsive,
        ModuleState::Draining,
        ModuleState::Stopped,
        ModuleState::Failed,
        ModuleState::Disabled,
    ] {
        assert!(
            !module_with_recovery_snapshot(state, true, 0)
                .is_warming()
                .unwrap(),
            "{state:?} should not be warming"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_history_survives_respawn_and_keeps_both_crashes_in_order() {
    let registry = Arc::new(Registry::default());
    let supervisor =
        Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::new(1, Duration::ZERO));
    let module = supervisor
        .spawn(ModuleSpec {
            script: None,
            module_id: "terminal-history".to_string(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
            carry_containment: false,
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let history = module.terminal_history();
        if history.entries.len() == 2 {
            assert_eq!(module.status().unwrap().state, ModuleState::Failed);
            assert_eq!(history.dropped, 0);
            assert_eq!(
                history
                    .entries
                    .iter()
                    .map(|entry| entry.exit_code)
                    .collect::<Vec<_>>(),
                vec![Some(23), Some(23)]
            );
            assert!(history.entries[0].at_ms <= history.entries[1].at_ms);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "module did not retain two terminal exits: {history:?}"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

/// A disable issued while a crash respawn is still backing off must preempt
/// that respawn: the operator's stop wins, the disable must not queue behind
/// the backoff, and the module must never come back up afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disable_during_crash_backoff_cancels_pending_respawn() {
    let backoff = Duration::from_secs(2);
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(10, backoff),
    );
    let module = supervisor
        .spawn(ModuleSpec {
            script: None,
            module_id: "disable-during-backoff".to_string(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
            carry_containment: false,
        })
        .unwrap();

    // Wait for the first crash to put the module into its backoff window.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if module.status().unwrap().state == ModuleState::Restarting {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "module never entered the crash backoff"
        );
        sleep(Duration::from_millis(10)).await;
    }

    let started = Instant::now();
    module.set_enabled(false).await.unwrap();
    let waited = started.elapsed();

    assert!(
            waited < backoff / 2,
            "disable waited {waited:?} behind the {backoff:?} crash backoff; the operator command must preempt the pending respawn"
        );
    assert_eq!(module.status().unwrap().state, ModuleState::Disabled);

    // Outlast the backoff: the respawn it was counting down to must never run.
    sleep(backoff + Duration::from_millis(500)).await;
    let status = module.status().unwrap();
    assert_eq!(status.state, ModuleState::Disabled);
    assert_eq!(
        status.spawn_generation, 1,
        "module respawned after the operator disabled it"
    );
    assert_eq!((status.restart_count, status.lifetime_restarts), (0, 0));
}

/// A `protocol: "none"` child that stays parked and exits 0 on SIGTERM:
/// the shape of nats-server, the program this rule exists for.
#[cfg(unix)]
fn protocol_none_sigterm_exits_clean_spec(
    module_id: &str,
    dir: &std::path::Path,
) -> (ModuleSpec, PathBuf, PathBuf) {
    let ready = dir.join("ready");
    let marker = dir.join("sigterm");
    let spec = ModuleSpec {
        script: None,
        module_id: module_id.to_string(),
        program: fake_aft_stub_path(),
        args: Vec::new(),
        env: vec![
            ("FAKE_AFT_NEVER_CONNECT".to_string(), "1".to_string()),
            (
                "FAKE_AFT_SIGTERM_MARKER_PATH".to_string(),
                marker.display().to_string(),
            ),
            (
                "FAKE_AFT_NEVER_CONNECT_READY_PATH".to_string(),
                ready.display().to_string(),
            ),
        ],
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    };
    (spec, ready, marker)
}

/// Wait for a file the child writes, so a signal is never sent before the
/// child's SIGTERM handler is installed (the default disposition would
/// kill it by signal and the exit would not be clean).
#[cfg(unix)]
async fn wait_for_file(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        sleep(Duration::from_millis(10)).await;
    }
}

/// A protocol-none module that exits 0 because something OUTSIDE the
/// supervisor sent it SIGTERM is respawned, and the terminal record carries
/// the crash-path disposition rather than `stopped`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_none_unrequested_clean_exit_restarts_as_a_crash() {
    let dir = cortexkit_test_support::ScratchDir::new("none-unrequested-clean-exit");
    let (spec, ready, marker) =
        protocol_none_sigterm_exits_clean_spec("none-unrequested-clean-exit", dir.path());
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(3, Duration::ZERO),
    );
    let module = supervisor.spawn(spec).unwrap();
    wait_for_file(&ready).await;
    // The ready file proves the child installed its SIGTERM handler, not
    // that the supervisor has processed the privacy trampoline's exec
    // acknowledgement. On macOS status withholds the pid until then.
    let deadline = Instant::now() + Duration::from_secs(10);
    let first_pid = loop {
        let status = module.status().unwrap();
        if status.state == ModuleState::Running {
            if let Some(pid) = status.pid {
                break pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "a running module must report its pid after exec confirmation: {status:?}"
        );
        sleep(Duration::from_millis(10)).await;
    };

    rustix::process::kill_process(
        rustix::process::Pid::from_raw(i32::try_from(first_pid).unwrap()).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let respawned = loop {
        let status = module.status().unwrap();
        if status.state == ModuleState::Running && status.pid.is_some_and(|pid| pid != first_pid) {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "protocol-none module was not respawned after an unrequested clean exit: {status:?}"
        );
        sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(respawned.spawn_generation, 2);
    assert!(
            marker.exists(),
            "the child must have exited through its SIGTERM handler (exit 0), or this proves nothing about clean exits"
        );

    let history = module.terminal_history();
    assert_eq!(history.entries.len(), 1, "{history:?}");
    let entry = &history.entries[0];
    assert_eq!(entry.exit_code, Some(0));
    assert_eq!(entry.exit_kind, subc_control::TerminalExitKind::Clean);
    assert_eq!(entry.disposition, TerminalDisposition::Restarting);

    module.stop().await.unwrap();
}

/// Repeated unrequested clean exits of a protocol-none module spend the
/// restart budget exactly as crashes do, and the module ends `failed` with
/// the budget named.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_none_repeated_clean_exits_exhaust_the_restart_budget() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(1, Duration::ZERO),
    );
    let module = supervisor
        .spawn(ModuleSpec {
            script: None,
            module_id: "none-clean-exit-budget".to_string(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "0".to_string())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
            carry_containment: false,
        })
        .unwrap();

    // Failed follows the terminal write; this deadline only bounds a hang,
    // not an assumed duration for the two launches or their exit recording.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = module.status().unwrap();
        if status.state == ModuleState::Failed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "module never exhausted its budget: {status:?} {:?}",
            module.terminal_history()
        );
        sleep(Duration::from_millis(10)).await;
    }
    let history = module.terminal_history();
    assert_eq!(
        history
            .entries
            .iter()
            .map(|entry| (entry.exit_code, entry.disposition.clone()))
            .collect::<Vec<_>>(),
        vec![
            (Some(0), TerminalDisposition::Restarting),
            (Some(0), TerminalDisposition::Failed),
        ]
    );
    let detail = history.entries[1]
        .disposition_detail
        .as_deref()
        .expect("a budget failure names the budget");
    assert!(detail.contains("max_restarts=1"), "{detail}");
    assert_eq!(module.status().unwrap().spawn_generation, 2);
}

#[test]
fn restart_budget_failure_is_published_after_its_terminal_record() {
    use std::io::{Read, Write};
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let spec = ModuleSpec {
        script: None,
        protocol: ModuleProtocol::None,
        ..windowed_crash_spec("budget-publication-order")
    };
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::new(
        ModuleState::Running,
        true,
    )));
    lock_snapshot(&snapshot).unwrap().process_alive = true;
    let reaped_snapshot = snapshot.clone();
    let mut barrier = terminal_record_barrier(&runtime.spawn_events);
    let history = runtime.terminal_ring.clone();
    let reap = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(on_child_exit(
                &spec,
                runtime.restart_policy,
                &supervisor.registry,
                &reaped_snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                &runtime.child_roster,
                ExitReport {
                    kind: ExitKind::Clean,
                    code: Some(0),
                    signal: None,
                    at_ms: 1,
                },
                true,
            ))
    });
    let mut reached = [0];
    barrier.read_exact(&mut reached).unwrap();
    assert_eq!(&reached, b"R");
    let (before_state, before_alive) = {
        let state = lock_snapshot(&snapshot).unwrap();
        (state.state, state.process_alive)
    };
    let before_history = history.lock().unwrap().snapshot();
    barrier.write_all(b"X").unwrap();
    assert!(matches!(reap.join().unwrap(), NextAction::Stop { .. }));
    assert!(before_history.entries.is_empty());
    assert_ne!(
        before_state,
        ModuleState::Failed,
        "Failed was visible before its terminal record could be written"
    );
    assert!(before_alive);
    assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
    let history = history.lock().unwrap().snapshot();
    assert_eq!(history.entries.len(), 1);
    assert_eq!(history.entries[0].exit_code, Some(0));
    assert_eq!(history.entries[0].disposition, TerminalDisposition::Failed);
}

#[test]
fn crash_restart_at_budget_limit_remains_recoverable_while_recording() {
    use std::io::{Read, Write};

    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(3, Duration::ZERO),
    );
    let mut runtime = supervisor.runtime_config();
    let module = module_with_recovery_snapshot(ModuleState::Running, true, 2);
    let snapshot = module.inner.snapshot.clone();
    lock_snapshot(&snapshot).unwrap().process_alive = true;
    runtime.terminal_ring = module.inner.terminal_ring.clone();
    let spec = windowed_crash_spec(module.module_id());
    let mut barrier = terminal_record_barrier(&runtime.spawn_events);
    let reaped_snapshot = snapshot.clone();
    let reap = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(on_child_exit(
                &spec,
                runtime.restart_policy,
                &supervisor.registry,
                &reaped_snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                &runtime.child_roster,
                ExitReport {
                    kind: ExitKind::Crash,
                    code: Some(2),
                    signal: None,
                    at_ms: 1,
                },
                true,
            ))
    });

    // Hold the crash handler between its budget decision and publication.
    // Connection cleanup must still see the recovery that this crash chose,
    // even when that recovery consumes the last available restart slot.
    let mut reached = [0];
    barrier.read_exact(&mut reached).unwrap();
    assert_eq!(&reached, b"R");
    let before_status = module.status().unwrap();
    let before_history = module.terminal_history();
    let before_recovers = module.will_recover_after_connection_loss().unwrap();
    barrier.write_all(b"X").unwrap();
    assert!(matches!(reap.join().unwrap(), NextAction::Restart { .. }));

    assert_eq!(before_status.state, ModuleState::Running);
    assert!(before_status.process_alive);
    assert_eq!(before_status.restart_count, 2);
    assert!(before_history.entries.is_empty());
    assert!(
        before_recovers,
        "connection cleanup would report a terminal close for a scheduled crash restart"
    );
    let restarting = module.status().unwrap();
    assert_eq!(restarting.state, ModuleState::Restarting);
    assert_eq!(restarting.restart_count, 2);
    assert!(module.will_recover_after_connection_loss().unwrap());
    let history = module.terminal_history();
    assert_eq!(history.entries.len(), 1);
    assert_eq!(
        history.entries[0].disposition,
        TerminalDisposition::Restarting
    );

    // Once the replacement is running, its next crash must be judged
    // against the exhausted budget rather than the previous recovery.
    update_snapshot(&snapshot, Some(module.module_id()), |state| {
        state.record_crash_restart(&RestartPolicy::new(3, Duration::ZERO), ClockInstant::now());
        state.state = ModuleState::Running;
        state.respawn_pending = false;
    })
    .unwrap();
    assert!(!module.will_recover_after_connection_loss().unwrap());
    update_snapshot(&snapshot, Some(module.module_id()), |state| {
        state.enabled = false;
        state.respawn_pending = true;
    })
    .unwrap();
    assert!(!module.will_recover_after_connection_loss().unwrap());
}

fn terminal_record_barrier(events: &super::SpawnEventFeed) -> std::net::TcpStream {
    use std::net::{TcpListener, TcpStream};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (barrier, _) = listener.accept().unwrap();
    for stream in [&writer, &barrier] {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
    }
    *events.1.lock().unwrap() = Some(writer);
    barrier
}

pub(super) fn before_terminal_record(events: &super::SpawnEventFeed) {
    use std::io::{Read, Write};
    let barrier = events.1.lock().unwrap().take();
    if let Some(mut barrier) = barrier {
        barrier.write_all(b"R").unwrap();
        let mut release = [0];
        barrier.read_exact(&mut release).unwrap();
        assert_eq!(&release, b"X");
    }
}

#[test]
fn health_restart_budget_failure_is_published_after_its_terminal_record() {
    use std::io::{Read, Write};

    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let mut runtime = supervisor.runtime_config();
    let module = module_with_recovery_snapshot(ModuleState::Running, true, 0);
    let snapshot = module.inner.snapshot.clone();
    runtime.terminal_ring = module.inner.terminal_ring.clone();
    let spec = ModuleSpec {
        script: None,
        module_id: module.module_id().into(),
        program: fake_aft_stub_path(),
        protocol: ModuleProtocol::None,
        env: vec![("FAKE_AFT_EXIT_CODE".into(), "17".into())],
        ..windowed_crash_spec(module.module_id())
    };
    let mut barrier = terminal_record_barrier(&runtime.spawn_events);
    let reap = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut child = Some(spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap());
                health_restart_child(
                    &spec,
                    &runtime,
                    &supervisor.registry,
                    &supervisor.process_liveness,
                    &snapshot,
                    &mut child,
                    SupervisorHealthStatus::Failing,
                    Some("health probe did not answer"),
                    1,
                )
                .await
                .unwrap();
                assert!(child.is_none());
            });
    });
    // The loopback barrier acknowledges entry into terminal recording by
    // health_restart_child's drain, then waits for the test's release byte.
    // Status and history are observed while that writer is held, not after
    // an elapsed-time guess about when the health action has completed.
    let mut reached = [0];
    barrier.read_exact(&mut reached).unwrap();
    assert_eq!(&reached, b"R");
    let before_status = module.status().unwrap();
    let before_history = module.terminal_history();
    barrier.write_all(b"X").unwrap();
    reap.join().unwrap();
    assert!(before_history.entries.is_empty());
    assert_ne!(
        before_status.state,
        ModuleState::Failed,
        "Failed was visible before its terminal record could be written"
    );
    assert!(
        before_status.process_alive,
        "process death was visible before its terminal record could be written"
    );
    let failed = module.status().unwrap();
    assert_eq!(failed.state, ModuleState::Failed);
    assert!(!failed.process_alive);
    let history = module.terminal_history();
    assert_eq!(history.entries.len(), 1);
    assert_eq!(history.entries[0].disposition, TerminalDisposition::Failed);
    assert_eq!(
        history.entries[0].disposition_detail.as_deref(),
        Some("crash budget exhausted: max_restarts=0 within window_secs=600")
    );
}

#[test]
fn observed_exits_publish_process_death_after_terminal_history() {
    use std::io::{Read, Write};
    for (kind, enabled, shutdown, wait_error, expected_state) in [
        (ExitKind::Clean, true, false, false, ModuleState::Stopped),
        (ExitKind::Crash, true, false, false, ModuleState::Restarting),
        (ExitKind::Crash, false, false, false, ModuleState::Disabled),
        (
            ExitKind::DeliberateSeverance,
            true,
            false,
            false,
            ModuleState::Restarting,
        ),
        #[cfg(unix)]
        (ExitKind::Crash, true, true, false, ModuleState::Stopped),
        (ExitKind::Crash, true, false, true, ModuleState::Failed),
    ] {
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(1, Duration::ZERO),
        );
        let runtime = supervisor.runtime_config();
        #[cfg(unix)]
        if shutdown {
            runtime.child_roster.close();
        }
        #[cfg(not(unix))]
        let _ = shutdown;
        let spec = windowed_crash_spec("exit-publication-order");
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::new(
            ModuleState::Running,
            enabled,
        )));
        lock_snapshot(&snapshot).unwrap().process_alive = true;
        let reaped_snapshot = snapshot.clone();
        let history = runtime.terminal_ring.clone();
        let mut barrier = terminal_record_barrier(&runtime.spawn_events);
        let reap = std::thread::spawn(move || {
            if wait_error {
                super::fail_after_wait_error(
                    &spec.module_id,
                    &reaped_snapshot,
                    &runtime.terminal_ring,
                    &runtime.spawn_events,
                    "wait failed".into(),
                    &runtime.failure_context(true),
                );
            } else {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(on_child_exit(
                        &spec,
                        runtime.restart_policy,
                        &supervisor.registry,
                        &reaped_snapshot,
                        &runtime.terminal_ring,
                        &runtime.spawn_events,
                        &runtime.child_roster,
                        ExitReport {
                            kind,
                            code: Some(0),
                            signal: None,
                            at_ms: 1,
                        },
                        true,
                    ));
            }
        });
        let mut reached = [0];
        barrier.read_exact(&mut reached).unwrap();
        assert_eq!(&reached, b"R");
        let before_alive = lock_snapshot(&snapshot).unwrap().process_alive;
        let before_history = history.lock().unwrap().snapshot();
        barrier.write_all(b"X").unwrap();
        reap.join().unwrap();
        assert!(
            before_alive,
            "{kind:?}: death was published before recording"
        );
        assert!(before_history.entries.is_empty());
        assert_eq!(lock_snapshot(&snapshot).unwrap().state, expected_state);
        assert!(!lock_snapshot(&snapshot).unwrap().process_alive);
        assert_eq!(history.lock().unwrap().snapshot().entries.len(), 1);
    }
}

#[tokio::test]
async fn health_restart_budget_failure_remains_failed_when_journal_append_fails() {
    let dir = cortexkit_test_support::ScratchDir::new("health-budget-journal-failure");
    let path = dir.join("terminals.jsonl");
    std::fs::create_dir(&path).unwrap();
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    )
    .with_terminal_journal(path, "health-budget-journal-failure".into());
    let runtime = supervisor.runtime_config();
    let spec = ModuleSpec {
        script: None,
        program: fake_aft_stub_path(),
        protocol: ModuleProtocol::None,
        env: vec![("FAKE_AFT_EXIT_CODE".into(), "17".into())],
        ..windowed_crash_spec("health-budget-journal-failure")
    };
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let mut child = Some(spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap());
    health_restart_child(
        &spec,
        &runtime,
        &supervisor.registry,
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        SupervisorHealthStatus::Failing,
        None,
        1,
    )
    .await
    .unwrap();
    assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
    assert!(!lock_snapshot(&snapshot).unwrap().process_alive);
    let history = runtime
        .terminal_ring
        .lock()
        .unwrap()
        .durable_history(&spec.module_id);
    assert!(history.journal_write_failures > 0);
    assert_eq!(history.entries.len(), 1);
    assert_eq!(history.entries[0].disposition, TerminalDisposition::Failed);
}

#[test]
fn health_restart_budget_failure_remains_failed_when_exit_recording_panics() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let spec = ModuleSpec {
        script: None,
        program: fake_aft_stub_path(),
        protocol: ModuleProtocol::None,
        env: vec![("FAKE_AFT_EXIT_CODE".into(), "17".into())],
        ..windowed_crash_spec("health-budget-recording-panic")
    };
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut child = Some(spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap());
                runtime.spawn_events.0.lock().unwrap().seq = u64::MAX;
                health_restart_child(
                    &spec,
                    &runtime,
                    &supervisor.registry,
                    &supervisor.process_liveness,
                    &snapshot,
                    &mut child,
                    SupervisorHealthStatus::Failing,
                    None,
                    1,
                )
                .await
                .unwrap();
            });
    }));
    let panic = result.expect_err("recording must still unwind");
    assert_eq!(
        panic.downcast_ref::<String>().map(String::as_str),
        Some("spawn event sequence exhausted")
    );
    let state = lock_snapshot(&snapshot).unwrap();
    assert_eq!(state.state, ModuleState::Failed);
    assert!(!state.process_alive);
    assert!(state.last_exit.is_some());
}

#[tokio::test]
async fn restart_budget_failure_remains_failed_when_journal_append_fails() {
    let dir = cortexkit_test_support::ScratchDir::new("budget-journal-failure");
    let path = dir.join("terminals.jsonl");
    std::fs::create_dir(&path).unwrap();
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    )
    .with_terminal_journal(path, "budget-journal-failure".into());
    let runtime = supervisor.runtime_config();
    let spec = windowed_crash_spec("budget-journal-failure");
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    assert!(matches!(
        on_child_exit(
            &spec,
            runtime.restart_policy,
            &supervisor.registry,
            &snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
            &runtime.child_roster,
            crash_exit_report(1),
            true,
        )
        .await,
        NextAction::Stop { .. }
    ));
    assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
    let history = runtime
        .terminal_ring
        .lock()
        .unwrap()
        .durable_history(&spec.module_id);
    assert!(history.journal_write_failures > 0);
    assert_eq!(history.entries.len(), 1);
    assert_eq!(history.entries[0].disposition, TerminalDisposition::Failed);
}

#[test]
fn restart_budget_failure_remains_failed_when_exit_recording_panics() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let spec = windowed_crash_spec("budget-recording-panic");
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    runtime
        .spawn_events
        .emit_spawned(&spec.module_id, None, 1, 1);
    // Exhausting the event sequence makes emit_exited panic before the
    // terminal write, exercising publication on the recording unwind.
    runtime.spawn_events.0.lock().unwrap().seq = u64::MAX;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(on_child_exit(
                &spec,
                runtime.restart_policy,
                &supervisor.registry,
                &snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                &runtime.child_roster,
                crash_exit_report(1),
                true,
            ))
    }));
    let panic = result.err().expect("recording must still unwind");
    assert_eq!(
        panic.downcast_ref::<String>().map(String::as_str),
        Some("spawn event sequence exhausted")
    );
    assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
}

/// A stop the supervisor itself requests still stops a protocol-none
/// module, even though the child answers the SIGTERM with exit 0.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_none_requested_stop_and_disable_do_not_respawn() {
    for disable in [false, true] {
        let label = if disable {
            "none-requested-disable"
        } else {
            "none-requested-stop"
        };
        let dir = cortexkit_test_support::ScratchDir::new(label);
        let (spec, ready, marker) = protocol_none_sigterm_exits_clean_spec(label, dir.path());
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(3, Duration::ZERO),
        );
        let module = supervisor.spawn(spec).unwrap();
        wait_for_file(&ready).await;

        if disable {
            module.set_enabled(false).await.unwrap();
        } else {
            module.stop().await.unwrap();
        }
        assert!(
            marker.exists(),
            "{label}: the child must have left through its SIGTERM handler with exit 0"
        );

        // Long enough for a zero-backoff respawn to have happened if the
        // exit had been treated as a crash.
        sleep(Duration::from_millis(500)).await;
        let status = module.status().unwrap();
        let expected = if disable {
            ModuleState::Disabled
        } else {
            ModuleState::Stopped
        };
        assert_eq!(status.state, expected, "{label}");
        assert_eq!(
            status.spawn_generation, 1,
            "{label}: respawned after a requested stop"
        );
        let history = module.terminal_history();
        assert_eq!(history.entries.len(), 1, "{label}: {history:?}");
        assert_eq!(history.entries[0].exit_code, Some(0), "{label}");
        assert_ne!(
            history.entries[0].disposition,
            TerminalDisposition::Restarting,
            "{label}"
        );
    }
}

/// A subc-wire module that exits 0 on its own is still a stop: the
/// protocol-none rule must not reach it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subc_wire_clean_exit_is_still_a_stop() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(3, Duration::ZERO),
    );
    let module = supervisor
        .spawn(ModuleSpec {
            script: None,
            module_id: "wire-clean-exit".to_string(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "0".to_string())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
            carry_containment: false,
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while module.terminal_history().entries.is_empty() {
        assert!(Instant::now() < deadline, "module never exited");
        sleep(Duration::from_millis(10)).await;
    }
    // Long enough for a zero-backoff respawn to have happened.
    sleep(Duration::from_millis(500)).await;
    let status = module.status().unwrap();
    assert_eq!(status.state, ModuleState::Stopped);
    assert_eq!(status.spawn_generation, 1);
    let history = module.terminal_history();
    assert_eq!(history.entries.len(), 1, "{history:?}");
    assert_eq!(history.entries[0].exit_code, Some(0));
    assert_eq!(history.entries[0].disposition, TerminalDisposition::Stopped);
}

#[cfg(unix)]
#[tokio::test]
async fn reload_exit_keeps_roster_until_terminal_is_recorded_during_shutdown() {
    let dir = cortexkit_test_support::ScratchDir::new("reload-roster-terminal-order");
    let record = dir.join("live-children.json");
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let mut runtime = supervisor.runtime_config();
    runtime.child_roster.record_to(record.clone());
    let gate = Arc::new(super::ReloadExitRecordGate::default());
    runtime.test_reload_exit_record_gate = Some(Arc::clone(&gate));
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let spec = ModuleSpec {
        script: None,
        module_id: "reload-exit-roster".into(),
        program: fake_aft_stub_path(),
        args: Vec::new(),
        env: vec![("FAKE_AFT_EXIT_CODE".into(), "23".into())],
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let mut child = None;
    let reload = super::finish_reload_child(
        &spec,
        &runtime,
        &supervisor.registry,
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
    );
    tokio::pin!(reload);
    tokio::select! {
        result = &mut reload => panic!("reload missed the before-record gate: {result:?}"),
        _ = gate.reached.notified() => {}
    }
    assert!(runtime
        .terminal_ring
        .lock()
        .unwrap()
        .snapshot()
        .entries
        .is_empty());
    assert_eq!(
        crate::live_children::read_record(&record).unwrap().len(),
        1,
        "shutdown must still wait for the reaped child until its terminal record exists"
    );
    runtime.child_roster.close();
    gate.resume.notify_one();
    assert!(reload.await.is_err());
    assert!(crate::live_children::read_record(&record)
        .unwrap()
        .is_empty());
    let history = runtime.terminal_ring.lock().unwrap().snapshot();
    assert_eq!(history.entries.len(), 1);
    assert_eq!(
        history.entries[0].disposition,
        TerminalDisposition::DaemonShutdown
    );
}

/// Each budgeted recovery can be blocked or cancelled before the new child
/// starts, so none of their disposition decisions should count a spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_budgeted_recovery_is_counted_before_spawn() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(1, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let spec = ModuleSpec {
        script: None,
        module_id: "lifetime-increment-path".to_string(),
        program: PathBuf::from("/unused/lifetime-increment-path"),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };

    let crash_snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    assert!(matches!(
        on_child_exit(
            &spec,
            runtime.restart_policy,
            &supervisor.registry,
            &crash_snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
            &runtime.child_roster,
            ExitReport {
                kind: ExitKind::Crash,
                code: Some(1),
                signal: None,
                at_ms: 1,
            },
            true,
        )
        .await,
        NextAction::Restart { schedule: _ }
    ));
    let (crash_restarts, crash_lifetime) = {
        let state = lock_snapshot(&crash_snapshot).unwrap();
        (state.crash_restarts.len(), state.lifetime_restarts)
    };
    assert_eq!((crash_restarts, crash_lifetime), (0, 0));

    let health_snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let mut health_child = None;
    assert!(matches!(
        health_restart_child(
            &spec,
            &runtime,
            &supervisor.registry,
            &supervisor.process_liveness,
            &health_snapshot,
            &mut health_child,
            SupervisorHealthStatus::Failing,
            None,
            2,
        )
        .await,
        Ok(())
    ));
    assert!(health_child.is_none());
    assert!(lock_snapshot(&health_snapshot).unwrap().respawn_pending);
    assert!(runtime.scheduled_respawn.lock().unwrap().take().is_some());
    let (health_restarts, health_lifetime) = {
        let state = lock_snapshot(&health_snapshot).unwrap();
        (state.crash_restarts.len(), state.lifetime_restarts)
    };
    assert_eq!((health_restarts, health_lifetime), (0, 0));

    let reload_snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let mut reload_child = None;
    assert!(matches!(
        handle_reload_spawn_failure(
            &spec,
            &runtime,
            &supervisor.process_liveness,
            &reload_snapshot,
            &mut reload_child,
            "forced reload spawn failure".to_string(),
        )
        .await,
        Err(SuperviseError::ReloadFailed { .. })
    ));
    let (reload_restarts, reload_lifetime) = {
        let state = lock_snapshot(&reload_snapshot).unwrap();
        (state.crash_restarts.len(), state.lifetime_restarts)
    };
    assert_eq!((reload_restarts, reload_lifetime), (0, 0));
}

#[tokio::test]
async fn deliberately_severed_live_child_records_lifetime_without_spending_restart_budget() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(3, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let spec = ModuleSpec {
        script: None,
        module_id: "deliberately-severed".to_string(),
        program: PathBuf::from("/unused/deliberately-severed"),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let process = ProcessIdentity {
        pid: 41,
        start_time: 101,
    };
    record_deliberate_severance(&snapshot, process).unwrap();
    let exit_report = apply_deliberate_severance_marker(
        &snapshot,
        Some(process),
        ExitReport {
            kind: ExitKind::Crash,
            code: Some(1),
            signal: None,
            at_ms: 1,
        },
    );
    assert_eq!(exit_report.kind, ExitKind::DeliberateSeverance);

    assert!(matches!(
        on_child_exit(
            &spec,
            runtime.restart_policy,
            &supervisor.registry,
            &snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
            &runtime.child_roster,
            exit_report,
            true,
        )
        .await,
        NextAction::Restart { schedule: _ }
    ));
    let state = lock_snapshot(&snapshot).unwrap();
    assert_eq!(state.lifetime_restarts, 1);
    assert_eq!(state.crash_restarts.len(), 0);
}

#[tokio::test]
async fn genuine_crash_spends_restart_budget_and_records_lifetime() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(3, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let spec = ModuleSpec {
        script: None,
        module_id: "genuine-crash".to_string(),
        program: PathBuf::from("/unused/genuine-crash"),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));

    assert!(matches!(
        on_child_exit(
            &spec,
            runtime.restart_policy,
            &supervisor.registry,
            &snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
            &runtime.child_roster,
            ExitReport {
                kind: ExitKind::Crash,
                code: Some(1),
                signal: None,
                at_ms: 1,
            },
            true,
        )
        .await,
        NextAction::Restart { schedule: Some(_) }
    ));
    let mut state = lock_snapshot(&snapshot).unwrap();
    assert!(state.respawn_pending);
    assert_eq!(state.lifetime_restarts, 0);
    state.record_crash_restart(&runtime.restart_policy, ClockInstant::now());
    assert_eq!(state.lifetime_restarts, 1);
    assert_eq!(state.crash_restarts.len(), 1);
}

fn crash_exit_report(at_ms: u64) -> ExitReport {
    ExitReport {
        kind: ExitKind::Crash,
        code: Some(1),
        signal: None,
        at_ms,
    }
}

fn windowed_crash_spec(module_id: &str) -> ModuleSpec {
    ModuleSpec {
        script: None,
        module_id: module_id.to_string(),
        program: PathBuf::from("/unused").join(module_id),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    }
}

/// A real crash loop still stops. Three crashes with nothing aging out spend
/// a budget of two and the third respawn is refused, and both surfaces an
/// operator has -- the log line and the retained terminal record -- name the
/// window rather than only the cap, because `max_restarts=2` alone is what
/// this budget used to mean.
#[tokio::test]
async fn three_crashes_inside_the_window_stop_the_module_and_name_the_window() {
    let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::ERROR);
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(2, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let spec = windowed_crash_spec("crash-loop-in-window");
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));

    for attempt in 1..=2 {
        assert!(
            matches!(
                on_child_exit(
                    &spec,
                    runtime.restart_policy,
                    &supervisor.registry,
                    &snapshot,
                    &runtime.terminal_ring,
                    &runtime.spawn_events,
                    &runtime.child_roster,
                    crash_exit_report(attempt),
                    true,
                )
                .await,
                NextAction::Restart { schedule: _ }
            ),
            "crash {attempt} is inside the budget and must respawn"
        );
        lock_snapshot(&snapshot)
            .unwrap()
            .record_crash_restart(&runtime.restart_policy, ClockInstant::now());
    }

    assert!(matches!(
        on_child_exit(
            &spec,
            runtime.restart_policy,
            &supervisor.registry,
            &snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
            &runtime.child_roster,
            crash_exit_report(3),
            true,
        )
        .await,
        NextAction::Stop { .. }
    ));

    {
        let state = lock_snapshot(&snapshot).unwrap();
        assert_eq!(state.state, ModuleState::Failed);
        assert_eq!(state.crash_restarts.len(), 2);
        assert_eq!(state.lifetime_restarts, 2);
    }

    let history = runtime
        .terminal_ring
        .lock()
        .expect("terminal ring is not poisoned")
        .snapshot();
    let last = history
        .entries
        .last()
        .expect("the refused crash is retained");
    assert_eq!(last.disposition, TerminalDisposition::Failed);
    assert_eq!(
        last.disposition_detail.as_deref(),
        Some("crash budget exhausted: max_restarts=2 within window_secs=600")
    );

    let captured = crate::router::test_log::captured_logs(&logs);
    assert!(
        captured.contains("crash budget exhausted: max_restarts=2 within window_secs=600"),
        "the stop must be logged with its window: {captured}"
    );
}

/// The rate, stated as a test: three crashes where the first has aged past
/// the window are two crashes as far as the budget is concerned, so the
/// third respawn is allowed and the ring holds only the two recent ones.
///
/// This is the case a lifetime counter got wrong -- and the case the daemon
/// now hits routinely, since a module exits non-zero every time its
/// connection to the daemon drops.
#[tokio::test]
async fn a_crash_older_than_the_window_frees_its_slot_for_a_later_crash() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(2, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let spec = windowed_crash_spec("crash-across-windows");
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));

    for attempt in 1..=2 {
        assert!(matches!(
            on_child_exit(
                &spec,
                runtime.restart_policy,
                &supervisor.registry,
                &snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                &runtime.child_roster,
                crash_exit_report(attempt),
                true,
            )
            .await,
            NextAction::Restart { schedule: _ }
        ));
        lock_snapshot(&snapshot)
            .unwrap()
            .record_crash_restart(&runtime.restart_policy, ClockInstant::now());
    }

    // The oldest crash moves out of the window; nothing else about the
    // module changes.
    update_snapshot(&snapshot, Some(&spec.module_id), |state| {
        age_oldest_crash_restart_out_of_window(state, runtime.restart_policy.window);
    })
    .unwrap();

    assert!(
        matches!(
            on_child_exit(
                &spec,
                runtime.restart_policy,
                &supervisor.registry,
                &snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                &runtime.child_roster,
                crash_exit_report(3),
                true,
            )
            .await,
            NextAction::Restart { schedule: _ }
        ),
        "a crash older than the window must not hold a budget slot"
    );

    let mut state = lock_snapshot(&snapshot).unwrap();
    state.record_crash_restart(&runtime.restart_policy, ClockInstant::now());
    assert_eq!(state.state, ModuleState::Restarting);
    assert_eq!(
        state.crash_restarts.len(),
        2,
        "the aged instant is dropped and the new one takes its place"
    );
    assert_eq!(
        state.lifetime_restarts, 3,
        "the ledger counts every restart, including the ones the window forgot"
    );
}

/// An operator restart hands the budget back whole, and the ledger keeps
/// counting. Those are different questions -- "how close is this module to
/// being stopped" and "how many times has it been replaced" -- and the
/// operator action answers only the first.
#[tokio::test]
async fn an_operator_restart_clears_the_ring_and_leaves_the_ledger_alone() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(2, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let spec = windowed_crash_spec("operator-cleared-budget");
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));

    for attempt in 1..=2 {
        assert!(matches!(
            on_child_exit(
                &spec,
                runtime.restart_policy,
                &supervisor.registry,
                &snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                &runtime.child_roster,
                crash_exit_report(attempt),
                true,
            )
            .await,
            NextAction::Restart { schedule: _ }
        ));
        lock_snapshot(&snapshot)
            .unwrap()
            .record_crash_restart(&runtime.restart_policy, ClockInstant::now());
    }

    reset_restart_count(&snapshot, &spec.module_id).unwrap();
    {
        let state = lock_snapshot(&snapshot).unwrap();
        assert!(
            state.crash_restarts.is_empty(),
            "an operator restart returns the full budget"
        );
        assert_eq!(
            state.lifetime_restarts, 2,
            "clearing the budget must not unmake the crashes"
        );
    }

    assert!(
        matches!(
            on_child_exit(
                &spec,
                runtime.restart_policy,
                &supervisor.registry,
                &snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                &runtime.child_roster,
                crash_exit_report(3),
                true,
            )
            .await,
            NextAction::Restart { schedule: _ }
        ),
        "the cleared budget must be spendable again"
    );
    let mut state = lock_snapshot(&snapshot).unwrap();
    state.record_crash_restart(&runtime.restart_policy, ClockInstant::now());
    assert_eq!(state.crash_restarts.len(), 1);
    assert_eq!(state.lifetime_restarts, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn severance_marker_for_a_dead_child_does_not_label_its_successor() {
    let severed = ProcessIdentity {
        pid: 41,
        start_time: 101,
    };
    let successor = ProcessIdentity {
        pid: 41,
        start_time: 202,
    };
    let module = module_with_recovery_snapshot(ModuleState::Running, true, 0);
    update_snapshot(&module.inner.snapshot, Some("recovery-snapshot"), |state| {
        state.pid = Some(successor.pid);
        state.process_start_time = Some(successor.start_time);
    })
    .unwrap();
    assert!(!module.record_deliberate_severance(severed).unwrap());

    let exit_report = apply_deliberate_severance_marker(
        &module.inner.snapshot,
        Some(successor),
        ExitReport {
            kind: ExitKind::Crash,
            code: Some(1),
            signal: None,
            at_ms: 1,
        },
    );

    assert_eq!(exit_report.kind, ExitKind::Crash);
}

#[tokio::test]
async fn drain_reap_marks_deliberate_severance_and_records_lifetime_without_budget() {
    let registry = Registry::default();
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(3, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let spec = ModuleSpec {
        script: None,
        module_id: "drain-deliberate-severance".to_string(),
        program: fake_aft_stub_path(),
        args: Vec::new(),
        env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let mut child = spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap();
    let process = ProcessIdentity {
        pid: 41,
        start_time: 101,
    };
    child.process_identity = Some(process);
    update_snapshot(&snapshot, Some(&spec.module_id), |state| {
        state.pid = Some(process.pid);
        state.process_start_time = Some(process.start_time);
    })
    .unwrap();
    record_deliberate_severance(&snapshot, process).unwrap();

    drain_child_to_state(
        &spec.module_id,
        spec.protocol,
        // The child exits on its own; no signal may change the exit this
        // test classifies.
        StopNotice::SentOverConnection,
        &registry,
        None,
        &snapshot,
        &runtime.terminal_ring,
        &runtime.spawn_events,
        child,
        Duration::from_secs(1),
        ModuleState::Stopped,
        Some(false),
        &super::FailureContext::operator(),
    )
    .await
    .unwrap();

    let state = lock_snapshot(&snapshot).unwrap();
    assert_eq!(
        state.last_exit.as_ref().map(|exit| exit.kind),
        Some(ExitKind::DeliberateSeverance)
    );
    assert_eq!(state.lifetime_restarts, 1);
    assert_eq!(state.crash_restarts.len(), 0);
    drop(state);
    let history = runtime.terminal_ring.lock().unwrap().snapshot();
    assert_eq!(
        history.entries[0].exit_kind,
        subc_control::TerminalExitKind::DeliberateSeverance
    );
}

#[tokio::test]
async fn ordinary_drain_reap_does_not_record_a_lifetime_restart() {
    let registry = Registry::default();
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(3, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let spec = ModuleSpec {
        script: None,
        module_id: "ordinary-drain".to_string(),
        program: fake_aft_stub_path(),
        args: Vec::new(),
        env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let child = spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap();

    drain_child_to_state(
        &spec.module_id,
        spec.protocol,
        // The child exits on its own; no signal may change the exit this
        // test classifies.
        StopNotice::SentOverConnection,
        &registry,
        None,
        &snapshot,
        &runtime.terminal_ring,
        &runtime.spawn_events,
        child,
        Duration::from_secs(1),
        ModuleState::Stopped,
        Some(false),
        &super::FailureContext::operator(),
    )
    .await
    .unwrap();

    let state = lock_snapshot(&snapshot).unwrap();
    assert_eq!(
        state.last_exit.as_ref().map(|exit| exit.kind),
        Some(ExitKind::Crash)
    );
    assert_eq!(state.lifetime_restarts, 0);
    assert_eq!(state.crash_restarts.len(), 0);
}

#[test]
fn fatal_connection_teardown_cannot_arm_a_marker_for_a_surviving_process() {
    // The server's generic fatal-routing branch only knows that the
    // connection failed; it does not know that the daemon deliberately
    // initiated a process-killing severance. Keep this seam explicit so a
    // future connection error path cannot silently reintroduce the stale
    // exemption that mislabels a later genuine crash.
    assert!(!include_str!("../server.rs")
        .contains("router.record_deliberate_connection_severance(ctx.connection_id)"));
}

/// The `route.closed` `drained` value must be the quiescence wait's own
/// measurement (`Ok`), never invented -- except on `Err`, where there is no
/// measurement at all and `false` is the one honest constant. This is the exact
/// logic `begin_forwarding_drain_with` now applies before sending `route.closed`
/// on every return path, including the one that used to return early via `?`
/// with `route.closing` already sent and no `route.closed` ever following.
#[test]
fn drained_after_quiescence_wait_passes_ok_through_and_forces_false_on_err() {
    assert!(drained_after_quiescence_wait(&Ok(true)));
    assert!(!drained_after_quiescence_wait(&Ok(false)));
    assert!(!drained_after_quiescence_wait(&Err(
        SuperviseError::StatePoisoned { module_id: None }
    )));
}

/// `supervise_loop`'s `wait()`-error arm now calls `record_terminal` like every
/// other exit path does, so a module whose child `wait()` itself errored (e.g.
/// already reaped out-of-band) still leaves a terminal record rather than none
/// at all. Triggering the real `wait()` I/O error from an integration test would
/// need a genuine already-reaped-child race, which is OS-specific and not
/// something this suite attempts elsewhere; this test instead verifies the
/// record produced for that arm end-to-end through the real `TerminalRing`, and
/// the call site itself is verified by inspection to sit in that exact arm.
#[test]
fn wait_error_exit_report_records_a_failed_terminal_with_no_code_or_signal() {
    let ring = Arc::new(Mutex::new(TerminalRing::new(
        TerminalRingConfig::default(),
        0,
    )));
    record_wait_error_terminal("wait-error", &ring, &super::SpawnEventFeed::default());

    let snapshot = ring.lock().unwrap().snapshot();
    assert_eq!(snapshot.entries.len(), 1);
    let entry = &snapshot.entries[0];
    assert_eq!(entry.exit_code, None);
    assert_eq!(entry.exit_signal, None);
    assert_eq!(entry.disposition, TerminalDisposition::Failed);
}

/// A launch is numbered before it starts. One that never runs keeps its
/// number, and the next launch of the module is numbered after it.
#[test]
fn spawn_generation_of_a_failed_launch_is_never_reused() {
    let feed = super::SpawnEventFeed::default();
    feed.configure_incarnation("launch-numbering".to_string());
    let (incarnation, failed) = feed.allocate_generation("relaunched");
    assert_eq!(incarnation, "launch-numbering");
    let (_, next) = feed.allocate_generation("relaunched");
    assert!(next > failed);
    assert_eq!(feed.emit_spawned("relaunched", Some(next), 7, 1), next);
    // A process with no pre-allocated number is numbered after both.
    assert!(feed.emit_spawned("relaunched", None, 8, 2) > next);
    assert_eq!(
        feed.allocate_generation("other").1,
        1,
        "numbering is per module"
    );
}

#[test]
fn wait_error_exit_path_preserves_spawn_event_density() {
    let feed = super::SpawnEventFeed::default();
    feed.configure_incarnation("wait-error-density".to_string());
    feed.emit_spawned("wait-error", None, 41, 1);
    let ring = Arc::new(Mutex::new(TerminalRing::new(
        TerminalRingConfig::default(),
        0,
    )));

    record_wait_error_terminal("wait-error", &ring, &feed);
    feed.emit_spawned("after-wait-error", None, 42, 2);

    let state = feed.0.lock().unwrap();
    let sequences = state
        .events
        .iter()
        .map(|event| event.cursor.seq)
        .collect::<Vec<_>>();
    assert_eq!(sequences, vec![1, 2, 3]);
    assert_eq!(state.events[1].kind, SpawnEventKind::Exited);
    assert_eq!(state.events[1].exit_code, None);
    assert_eq!(state.events[1].exit_signal, None);
}

/// Pins the report's `kind` too: the wait-error arm treats an unwaitable child
/// as a crash (matching `fail_snapshot`'s `Failed` disposition for this arm),
/// not a clean exit it never actually observed.
#[test]
fn wait_error_exit_report_is_classified_as_a_crash() {
    assert_eq!(wait_error_exit_report().kind, ExitKind::Crash);
}
