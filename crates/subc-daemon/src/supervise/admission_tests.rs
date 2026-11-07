use super::*;
use cortexkit_test_support::ScratchDir;
use std::sync::atomic::{AtomicBool, Ordering};

fn spec(directory: &std::path::Path, name: &str) -> ModuleSpec {
    ModuleSpec {
        module_id: name.into(),
        program: if cfg!(target_os = "macos") {
            "/bin/bash"
        } else {
            "/bin/sh"
        }
        .into(),
        args: vec![
            "-c".into(),
            "echo exec >\"$1\"; exec /bin/sleep 60".into(),
            "admission-test".into(),
            directory.join(format!("{name}.exec")).display().to_string(),
        ],
        env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.into(), directory.display().to_string()))
            .collect(),
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: true,
    }
}

fn new_supervisor(directory: &std::path::Path) -> Supervisor {
    Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
        .with_live_children_record(directory.join("live-children.json"))
        .with_drain_timeout(Duration::from_millis(20))
}

async fn until(description: &str, check: impl Fn() -> bool) {
    timeout(Duration::from_secs(5), async {
        while !check() {
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{description}"));
}

struct Stall {
    release: Arc<(Mutex<bool>, Condvar)>,
    entered: mpsc::Receiver<u32>,
}
impl Stall {
    fn install(supervisor: &Supervisor, module: &str, fail: bool) -> Self {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let hold = Arc::clone(&release);
        let module = module.to_owned();
        let once = AtomicBool::new(false);
        let (sender, entered) = mpsc::channel();
        supervisor
            .child_roster
            .set_record_write_hook_for_test(move |entries| {
                let Some(entry) = entries.values().find(|entry| entry.module_id == module) else {
                    return;
                };
                if once.swap(true, Ordering::SeqCst) {
                    return;
                }
                sender.send(entry.pid).unwrap();
                let mut ready = hold.0.lock().unwrap();
                while !*ready {
                    ready = hold.1.wait(ready).unwrap();
                }
                if fail {
                    crate::live_children::fail_next_file_sync();
                }
            });
        Self { release, entered }
    }
    fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
}
impl Drop for Stall {
    fn drop(&mut self) {
        self.release();
    }
}

async fn dropped_attempt_at(stage: &'static str) {
    let dir = ScratchDir::new("admission-attempt-drop");
    let supervisor = new_supervisor(&dir);
    let module_spec = spec(&dir, stage);
    let name = module_spec.module_id.clone();
    let rejected = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&rejected);
    let owned = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&owned);
    supervisor
        .child_roster
        .set_admission_hook_for_test(move |_, at, pid| {
            if at != stage {
                return true;
            }
            if let Some(pid) = pid {
                assert_eq!(subc_os::parent_pid(pid), Some(std::process::id()));
                *captured.lock().unwrap() = Some(pid);
            }
            seen.store(true, Ordering::SeqCst);
            false
        });
    assert!(
        supervisor.spawn(module_spec.clone()).is_err(),
        "stage {stage} must reject"
    );
    assert!(
        rejected.load(Ordering::SeqCst),
        "the requested stage must run"
    );
    until(
        "a dropped attempt must finish owned-child and durable cleanup",
        || !supervisor.child_roster.admission_pending(&name),
    )
    .await;
    if let Some(pid) = *owned.lock().unwrap() {
        assert_eq!(
            subc_os::child_exit::observe(pid)
                .unwrap_err()
                .raw_os_error(),
            Some(rustix::io::Errno::CHILD.raw_os_error()),
            "the attempt must be reaped, not merely signalled"
        );
    }
    assert!(!dir.join(format!("{stage}.exec")).exists());
    assert!(
        crate::live_children::read_record(&dir.join("live-children.json"))
            .unwrap()
            .is_empty()
    );
    supervisor
        .child_roster
        .set_admission_hook_for_test(|_, _, _| true);
    let module = supervisor
        .spawn(module_spec)
        .expect("failed attempt must not leak the permit or its per-module block");
    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_attempt_before_pid_report_never_execs_and_releases_its_permit() {
    dropped_attempt_at("before_pid_report").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_attempt_after_pid_report_never_execs_and_reaps_its_owned_child() {
    dropped_attempt_at("after_pid").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_attempt_after_placement_never_execs_and_reaps_its_owned_child() {
    dropped_attempt_at("after_placement").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_attempt_after_durable_admission_never_execs_and_clears_its_record() {
    dropped_attempt_at("after_durable").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durably_adopted_attempt_is_disarmed_and_keeps_its_live_unreaped_pin() {
    let dir = ScratchDir::new("admission-disarm");
    let supervisor = new_supervisor(&dir);
    let module = supervisor.spawn(spec(&dir, "adopted")).unwrap();
    until(
        "privacy exec confirmation must publish the adopted PID",
        || module.status().unwrap().pid.is_some(),
    )
    .await;
    let pid = module.status().unwrap().pid.unwrap();
    until("the adopted image must run", || {
        dir.join("adopted.exec").exists()
    })
    .await;
    assert_eq!(
        subc_os::child_exit::observe(pid).unwrap(),
        None,
        "dropping the disarmed attempt must neither kill nor reap the live pin"
    );
    assert_eq!(subc_os::parent_pid(pid), Some(std::process::id()));
    let entries = crate::live_children::read_record(&dir.join("live-children.json")).unwrap();
    assert!(entries
        .iter()
        .any(|entry| entry.pid == pid && entry.container.is_some()));
    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_timeout_never_sends_go_and_lifts_only_after_durable_late_cleanup() {
    let dir = ScratchDir::new("admission-timeout");
    let supervisor = new_supervisor(&dir);
    supervisor
        .child_roster
        .set_admission_timeout_for_test(Duration::from_millis(60));
    let stall = Stall::install(&supervisor, "timed", false);
    let result = supervisor.spawn(spec(&dir, "timed"));
    assert!(
        matches!(result, Err(SuperviseError::Spawn { source, .. }) if source.to_string().contains("carried admission timed out"))
    );
    let pid = stall.entered.recv_timeout(Duration::from_secs(1)).unwrap();
    until(
        "the timed-out pre-exec child must be reaped while its writer is still stalled",
        || subc_os::child_exit::observe(pid).is_err(),
    )
    .await;
    assert!(
        !dir.join("timed.exec").exists(),
        "no image or HELLO is possible before go"
    );
    assert!(supervisor.child_roster.admission_pending("timed"));
    assert!(supervisor.child_roster.previous_instance("timed").is_some());
    assert!(
        supervisor.spawn(spec(&dir, "timed")).is_err(),
        "pending cleanup must block a second copy"
    );
    // A different carried launch reaches record admission (and its own bounded
    // timeout), proving that the first timeout released the global spawn permit.
    let other = supervisor.spawn(spec(&dir, "other"));
    assert!(
        matches!(other, Err(SuperviseError::Spawn { source, .. }) if source.to_string().contains("carried admission timed out"))
    );
    assert!(!dir.join("other.exec").exists());
    stall.release();
    until(
        "late success must remove provisional identity durably before lifting its block",
        || {
            !supervisor.child_roster.admission_pending("timed")
                && !supervisor.child_roster.admission_pending("other")
        },
    )
    .await;
    assert!(
        !dir.join("timed.exec").exists(),
        "a late writer must never send go"
    );
    assert!(supervisor.child_roster.recovery_block().is_none());
    supervisor
        .child_roster
        .set_admission_timeout_for_test(ADMISSION_TIMEOUT);
    let module = supervisor.spawn(spec(&dir, "timed")).unwrap();
    module.stop().await.unwrap();
    supervisor
        .child_roster
        .set_admission_timeout_for_test(Duration::from_millis(60));
    supervisor
        .child_roster
        .set_admission_hook_for_test(|module, stage, _| {
            if module == "deadline-boundary" && stage == "after_durable" {
                // Model descheduling after recording succeeded but before the go
                // write. A late success must not turn into permission to execute.
                std::thread::sleep(Duration::from_millis(120));
            }
            true
        });
    let boundary = supervisor.spawn(spec(&dir, "deadline-boundary"));
    let rejected = matches!(&boundary, Err(SuperviseError::Spawn { source, .. })
        if source.to_string().contains("carried admission timed out"));
    if let Ok(module) = boundary {
        module.stop().await.unwrap();
    }
    assert!(
        rejected,
        "go must not be sent after the deadline even if durable admission already succeeded"
    );
    until(
        "late permission refusal must finish its durable cleanup",
        || {
            !supervisor
                .child_roster
                .admission_pending("deadline-boundary")
        },
    )
    .await;
    assert!(!dir.join("deadline-boundary.exec").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_late_admission_write_quarantines_every_carried_start_across_restart() {
    let dir = ScratchDir::new("admission-late-write-failure");
    let record = dir.join("live-children.json");
    let owner = crate::run_dir_lock::RunDirLock::acquire(&record).unwrap();
    let supervisor = new_supervisor(&dir);
    supervisor
        .child_roster
        .set_admission_timeout_for_test(Duration::from_millis(60));
    let stall = Stall::install(&supervisor, "late-failed", true);
    assert!(supervisor.spawn(spec(&dir, "late-failed")).is_err());
    let pid = stall.entered.recv_timeout(Duration::from_secs(1)).unwrap();
    until(
        "cancelled child must be reaped before the failed late writer settles",
        || subc_os::child_exit::observe(pid).is_err(),
    )
    .await;
    assert!(
        supervisor.child_roster.recovery_block().is_none(),
        "a timeout alone is only a per-module block"
    );
    stall.release();
    until(
        "an actual late write failure must quarantine the recovery store",
        || crate::live_children::quarantine_block(&record).is_some(),
    )
    .await;
    assert!(!dir.join("late-failed.exec").exists());
    let reason = supervisor.child_roster.recovery_block().unwrap();
    assert!(
        reason.contains("failed late admission write")
            && reason.contains("injected live-children file sync failure")
    );
    for name in ["late-failed", "another-carried"] {
        assert!(
            supervisor.spawn(spec(&dir, name)).is_err(),
            "all carried starts must block"
        );
    }
    let configured = supervisor
        .supervise_configured_requested(
            spec(&dir, "status-carried"),
            true,
            HealthConfig::default(),
            None,
            RestartPolicy::new(0, Duration::ZERO),
            Duration::from_secs(1),
            None,
        )
        .unwrap();
    let detail = configured
        .status()
        .unwrap()
        .waiting_for_previous_detail
        .unwrap();
    assert!(
        detail.contains("failed late admission write")
            && detail.contains("injected live-children file sync failure"),
        "module status must show the underlying late-write cause: {detail}"
    );
    configured.stop().await.unwrap();
    let mut ordinary_spec = spec(&dir, "ordinary");
    ordinary_spec.carry_containment = false;
    let ordinary = supervisor
        .spawn(ordinary_spec)
        .expect("ordinary starts do not inherit carried quarantine policy");
    ordinary.stop().await.unwrap();
    drop(configured);
    drop(supervisor);
    drop(owner);
    // Parallel gated forks may briefly inherit the old CLOEXEC lock descriptor.
    // Wait for those owned launches to exec/cancel rather than mistaking that
    // bounded inheritance for a failed quarantine restart policy.
    let owner = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(owner) = crate::run_dir_lock::RunDirLock::acquire(&record) {
                break owner;
            }
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the old lock must be released before the next daemon incarnation");
    crate::live_children::sweep_orphans(
        &owner,
        &crate::live_children::AdoptedPids::none(),
        crate::live_children::SweepBounds::default(),
    )
    .await;
    let restarted = new_supervisor(&dir);
    let reason = restarted.child_roster.recovery_block().unwrap();
    assert!(
        reason.contains("failed late admission write")
            && reason.contains("injected_live-children_file_sync_failure"),
        "restart must retain the marker and its cause: {reason}"
    );
    for name in ["late-failed", "another-carried"] {
        assert!(restarted.spawn(spec(&dir, name)).is_err());
    }
    let mut ordinary_spec = spec(&dir, "ordinary-restarted");
    ordinary_spec.carry_containment = false;
    let ordinary = restarted.spawn(ordinary_spec).unwrap();
    ordinary.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "launched only as an isolated parent-death fixture"]
async fn simultaneous_gate_daemon_fixture() {
    let Some(directory) = std::env::var_os("SUBC_TEST_ADMISSION_CRASH_FIXTURE") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let supervisor = Arc::new(new_supervisor(&directory));
    let phases = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let phase = Arc::clone(&phases);
    let root = directory.clone();
    supervisor
        .child_roster
        .set_admission_hook_for_test(move |module, stage, pid| {
            if module == "first" && stage == "after_gate_install" {
                std::fs::write(root.join("first-gate-created"), "ready").unwrap();
                let mut state = phase.0.lock().unwrap();
                while !state.0 {
                    state = phase.1.wait(state).unwrap();
                }
            }
            if module == "second" && matches!(stage, "permit_waiting" | "after_gate_install") {
                let mut state = phase.0.lock().unwrap();
                state.0 = true;
                phase.1.notify_all();
                if stage == "permit_waiting" {
                    std::fs::write(root.join("second-stage"), "queued").unwrap();
                } else {
                    // With serialization removed, both writers exist before either
                    // fork. Delay the second fork until the first has inherited it.
                    while !state.1 {
                        state = phase.1.wait(state).unwrap();
                    }
                }
            }
            if stage == "after_pid" {
                let pid = pid.unwrap();
                let facts = subc_os::process_group::inspect(pid).unwrap().unwrap();
                std::fs::write(
                    root.join(format!("{module}.pid")),
                    format!("{pid} {}", facts.start_time),
                )
                .unwrap();
                if module == "first" {
                    phase.0.lock().unwrap().1 = true;
                    phase.1.notify_all();
                } else {
                    std::fs::write(root.join("second-stage"), "forked").unwrap();
                }
            }
            true
        });
    supervisor
        .child_roster
        .set_record_write_hook_for_test(|_| loop {
            std::thread::park();
        });
    let spawn = |name| {
        let supervisor = Arc::clone(&supervisor);
        let module_spec = spec(&directory, name);
        let runtime = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let _runtime = runtime.enter();
            let _module = supervisor.spawn(module_spec);
        })
    };
    let _first = spawn("first");
    until(
        "first request must own gate descriptors before second request begins",
        || directory.join("first-gate-created").exists(),
    )
    .await;
    let _second = spawn("second");
    std::future::pending::<()>().await;
}

struct OwnedDescendant {
    process: subc_os::Process,
    birth: u64,
}
impl OwnedDescendant {
    fn capture(pid: u32, parent: u32, birth: u64) -> Self {
        assert_eq!(
            subc_os::parent_pid(pid),
            Some(parent),
            "fixture must prove the child belongs to its own daemon"
        );
        let process = subc_os::Process::open(pid).unwrap().unwrap();
        assert_eq!(
            subc_os::process_group::inspect(pid)
                .unwrap()
                .unwrap()
                .start_time,
            birth
        );
        assert_eq!(subc_os::parent_pid(pid), Some(parent));
        Self { process, birth }
    }
    fn alive(&self) -> bool {
        subc_os::process_group::inspect(self.process.pid())
            .ok()
            .flatten()
            .is_some_and(|facts| facts.start_time == self.birth && !facts.exited)
    }
}
impl Drop for OwnedDescendant {
    fn drop(&mut self) {
        if self.alive() {
            let _ = self.process.signal(subc_os::Signal::Kill);
        }
    }
}

#[tokio::test]
async fn two_carried_requests_in_flight_exit_without_exec_when_daemon_dies_before_go() {
    let dir = ScratchDir::new("simultaneous-admission-eof");
    let mut daemon = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "supervise::admission::tests::simultaneous_gate_daemon_fixture",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("SUBC_TEST_ADMISSION_CRASH_FIXTURE", &*dir)
        .env("XDG_DATA_HOME", &*dir)
        .env("XDG_RUNTIME_DIR", &*dir)
        .env("XDG_CONFIG_HOME", &*dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    struct Daemon<'a>(&'a mut std::process::Child);
    impl Drop for Daemon<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let daemon = Daemon(&mut daemon);
    until(
        "both carried requests must be in flight with the first child gated",
        || {
            read_report(&dir.join("first.pid")).is_some()
                && std::fs::read_to_string(dir.join("second-stage"))
                    .ok()
                    .is_some_and(|stage| {
                        stage == "queued"
                            || (stage == "forked" && read_report(&dir.join("second.pid")).is_some())
                    })
        },
    )
    .await;
    let mut children = Vec::new();
    for name in ["first", "second"] {
        if let Some((pid, birth)) = read_report(&dir.join(format!("{name}.pid"))) {
            children.push(OwnedDescendant::capture(pid, daemon.0.id(), birth));
        }
    }
    assert!(!children.is_empty());
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    until(
        "every gated child must see daemon death, not another child's inherited writer",
        || children.iter().all(|child| !child.alive()),
    )
    .await;
    assert!(!dir.join("first.exec").exists() && !dir.join("second.exec").exists());
}

async fn rejected_after_holder_fork(shutdown: bool) {
    let dir = ScratchDir::new("admission-terminal-after-go");
    let supervisor = new_supervisor(&dir);
    let mut module_spec = spec(&dir, "rejected");
    let holder_file = dir.join("holder.pid");
    module_spec.args = vec![
        "-c".into(),
        "/bin/sleep 60 </dev/null >/dev/null 2>&1 & echo $! >\"$1\"; exec /bin/sleep 60".into(),
        "adoption-holder".into(),
        holder_file.display().to_string(),
    ];
    let holder = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&holder);
    let rejection_reached_holder = Arc::new(AtomicBool::new(false));
    let reached = Arc::clone(&rejection_reached_holder);
    if !shutdown {
        supervisor.child_roster.fail_adoption_for_test();
    }
    let shutdown_flag = supervisor.child_roster.shutdown_flag();
    supervisor
        .child_roster
        .set_admission_hook_for_test(move |_, stage, leader| {
            if stage == "after_rejected_child" {
                // Check whether this rejection stops the holder before the
                // attempt guard runs its fallback cleanup. The fallback must
                // not hide a leader-only rejection that leaves the already-
                // forked holder alive.
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while captured
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(OwnedDescendant::alive)
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                reached.store(
                    captured
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|holder| !holder.alive()),
                    Ordering::SeqCst,
                );
                return true;
            }
            if stage != "after_go" {
                return true;
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let pid = loop {
                if let Some(pid) = std::fs::read_to_string(&holder_file)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok())
                {
                    break pid;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the admitted image must publish its own holder before rejection"
                );
                std::thread::sleep(Duration::from_millis(2));
            };
            let birth = subc_os::process_group::inspect(pid)
                .unwrap()
                .unwrap()
                .start_time;
            *captured.lock().unwrap() = Some(OwnedDescendant::capture(pid, leader.unwrap(), birth));
            if shutdown {
                shutdown_flag.set_for_test();
            }
            true
        });
    assert!(supervisor.spawn(module_spec).is_err());
    assert!(
        rejection_reached_holder.load(Ordering::SeqCst),
        "the rejected-child branch must reach the holder before the attempt fallback runs"
    );
    let holder = holder
        .lock()
        .unwrap()
        .take()
        .expect("rejection must happen after the real holder fork");
    until(
        "whole-container rejection must terminate the holder, not just its leader",
        || !holder.alive(),
    )
    .await;
    until(
        "terminal rejection must finish cleanup before a fresh container is admitted",
        || !supervisor.child_roster.admission_pending("rejected"),
    )
    .await;
    assert!(
        crate::live_children::read_record(&dir.join("live-children.json"))
            .unwrap()
            .is_empty()
    );
    if !shutdown {
        supervisor
            .child_roster
            .set_admission_hook_for_test(|_, _, _| true);
        let replacement = supervisor.spawn(spec(&dir, "rejected")).unwrap();
        replacement.stop().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_final_adoption_terminates_the_already_forked_holder_before_fresh_retry() {
    rejected_after_holder_fork(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_roster_rejection_terminates_the_whole_container_after_go() {
    rejected_after_holder_fork(true).await;
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn privacy_trampoline_behind_the_admission_gate_preserves_the_carried_pgid() {
    let dir = ScratchDir::new("admission-privacy-pgid");
    let supervisor = new_supervisor(&dir);
    let mut module_spec = spec(&dir, "privacy");
    let holder_file = dir.join("privacy-holder.pid");
    module_spec.args = vec!["-c".into(),
        "if [ ! -s \"$1\" ]; then /bin/sleep 60 </dev/null >/dev/null 2>&1 & echo $! >\"$1\"; fi; exec /bin/sleep 60".into(),
        "privacy-holder".into(), holder_file.display().to_string()];
    let module = supervisor.spawn(module_spec).unwrap();
    until("the first privacy image must finish exec", || {
        module.status().unwrap().pid.is_some()
    })
    .await;
    let pin = module.status().unwrap().pid.unwrap();
    until(
        "a live owned holder must keep the group joinable after its first leader exits",
        || {
            std::fs::read_to_string(&holder_file)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                .is_some_and(|pid| {
                    subc_os::process_group::inspect(pid)
                        .ok()
                        .flatten()
                        .is_some()
                })
        },
    )
    .await;
    let holder: u32 = std::fs::read_to_string(holder_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let birth = subc_os::process_group::inspect(holder)
        .unwrap()
        .unwrap()
        .start_time;
    let _holder = OwnedDescendant::capture(holder, pin, birth);
    assert_eq!(subc_os::process_group::pgid(pin).unwrap(), Some(pin));
    for _ in 0..2 {
        let previous = module.status().unwrap().pid.unwrap();
        module.restart(None).await.unwrap();
        until("the replacement privacy image must finish exec", || {
            module
                .status()
                .unwrap()
                .pid
                .is_some_and(|pid| pid != previous)
        })
        .await;
        let leader = module.status().unwrap().pid.unwrap();
        assert_ne!(leader, pin);
        assert_eq!(
            subc_os::process_group::pgid(leader).unwrap(),
            Some(pin),
            "privacy exec must preserve the group inherited behind the gate"
        );
        assert!(
            subc_os::child_exit::observe(pin).unwrap().is_some(),
            "the original pin remains unreaped"
        );
    }
    module.stop().await.unwrap();
}

fn read_report(path: &std::path::Path) -> Option<(u32, u64)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut fields = text.split_whitespace();
    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_placement_times_out_with_known_pid_without_go_and_releases_permit_before_reap() {
    let dir = ScratchDir::new("admission-stalled-placement");
    let supervisor = Arc::new(new_supervisor(&dir));
    supervisor
        .child_roster
        .set_admission_timeout_for_test(Duration::from_millis(250));
    let (pause, _placement_release) = subc_os::admission_gate::PlacementPause::pipe().unwrap();
    supervisor
        .child_roster
        .set_placement_pause_for_test("placement", pause);
    let reap = Arc::new((Mutex::new(false), Condvar::new()));
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0 .0.lock().unwrap() = true;
            self.0 .1.notify_all();
        }
    }
    let release_reap = Release(Arc::clone(&reap));
    let (reported, pid_receiver) = mpsc::channel();
    supervisor
        .child_roster
        .set_admission_hook_for_test(move |module, stage, pid| {
            if module != "placement" {
                return true;
            }
            if stage == "after_pid" {
                reported.send(pid.unwrap()).unwrap();
            }
            if stage == "before_reap" {
                let mut ready = reap.0.lock().unwrap();
                while !*ready {
                    ready = reap.1.wait(ready).unwrap();
                }
            }
            true
        });
    let spawning = Arc::clone(&supervisor);
    let module_spec = spec(&dir, "placement");
    let launched = tokio::task::spawn_blocking(move || spawning.spawn(module_spec));
    let pid = pid_receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(subc_os::parent_pid(pid), Some(std::process::id()));
    until(
        "both provisional identities must become durable even while placement is unconfirmed",
        || {
            crate::live_children::read_record(&dir.join("live-children.json"))
                .ok()
                .is_some_and(|entries| {
                    entries
                        .iter()
                        .filter(|entry| entry.module_id == "placement")
                        .count()
                        == 2
                })
        },
    )
    .await;
    assert!(
        !dir.join("placement.exec").exists(),
        "durable identity alone must not send go"
    );
    let result = launched.await.unwrap();
    assert!(
        matches!(result, Err(SuperviseError::Spawn { source, .. }) if source.to_string().contains("carried admission timed out"))
    );
    assert!(
        supervisor.child_roster.admission_pending("placement"),
        "block must remain until the child is reaped"
    );
    until(
        "deadline must synchronously kill the known child before deferred reap",
        || subc_os::child_exit::observe(pid).ok().flatten().is_some(),
    )
    .await;
    let spawning = Arc::clone(&supervisor);
    let next_spec = spec(&dir, "placement-next");
    let next = tokio::task::spawn_blocking(move || spawning.spawn(next_spec));
    let other = tokio::time::timeout(Duration::from_secs(5), next)
        .await
        .expect("a stalled placement must release the global permit before deferred reap")
        .unwrap()
        .expect("the next carried module must start while the first awaits reap");
    other.stop().await.unwrap();
    assert!(supervisor.child_roster.admission_pending("placement"));
    drop(release_reap);
    until(
        "reap and durable cleanup must lift the per-module block",
        || !supervisor.child_roster.admission_pending("placement"),
    )
    .await;
    assert_eq!(
        subc_os::child_exit::observe(pid)
            .unwrap_err()
            .raw_os_error(),
        Some(rustix::io::Errno::CHILD.raw_os_error())
    );
    assert!(!dir.join("placement.exec").exists());
    supervisor
        .child_roster
        .set_admission_timeout_for_test(ADMISSION_TIMEOUT);
    let restarted = supervisor.spawn(spec(&dir, "placement")).unwrap();
    restarted.stop().await.unwrap();
}
