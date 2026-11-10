use super::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command as ProcessCommand, Stdio};

#[test]
fn resource_command_fixture() {
    let Some(mode) = std::env::var_os("SUBC_RESOURCE_COMMAND_FIXTURE") else {
        return;
    };
    if mode == "parent" {
        let mut child = command();
        let child_cpu = subc_os::resource_usage(child.id()).map(cpu_ms).unwrap();
        println!("CHILD_READY {} {child_cpu}", child.id());
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        if line.trim() == "burn" {
            writeln!(child.stdin.as_mut().unwrap(), "burn").unwrap();
            let mut output = BufReader::new(child.stdout.take().unwrap());
            loop {
                let mut line = String::new();
                assert_ne!(output.read_line(&mut line).unwrap(), 0);
                if line.contains("RESOURCE_BURNED") {
                    break;
                }
            }
            child.stdout = Some(output.into_inner());
            println!("CHILD_BURNED");
            std::io::stdout().flush().unwrap();
            line.clear();
            std::io::stdin().read_line(&mut line).unwrap();
        }
        writeln!(child.stdin.take().unwrap(), "exit").unwrap();
        assert!(child.wait().unwrap().success());
        println!("CHILD_REAPED");
        std::io::stdout().flush().unwrap();
        line.clear();
        std::io::stdin().read_line(&mut line).unwrap();
        if std::env::var_os("SUBC_RESOURCE_PARENT_FINAL_BURN").is_some() {
            let deadline = Instant::now() + Duration::from_millis(350);
            while Instant::now() < deadline {
                std::hint::black_box(
                    (0_u64..10_000).fold(1_u64, |a, b| a.wrapping_mul(31).wrapping_add(b)),
                );
            }
        }
        return;
    }
    let deadline = Instant::now() + Duration::from_millis(350);
    while Instant::now() < deadline {
        std::hint::black_box(
            (0_u64..10_000).fold(1_u64, |a, b| a.wrapping_mul(31).wrapping_add(b)),
        );
    }
    println!("RESOURCE_READY");
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    loop {
        line.clear();
        if std::io::stdin().read_line(&mut line).unwrap() == 0 || line.trim() != "burn" {
            break;
        }
        let baseline = subc_os::resource_usage(std::process::id())
            .map(cpu_ms)
            .unwrap();
        let timeout = Instant::now() + Duration::from_secs(10);
        while subc_os::resource_usage(std::process::id())
            .map(cpu_ms)
            .unwrap()
            < baseline + 150
        {
            assert!(
                Instant::now() < timeout,
                "fixture could not obtain 150ms CPU"
            );
            std::hint::black_box(
                (0_u64..10_000).fold(1_u64, |a, b| a.wrapping_mul(31).wrapping_add(b)),
            );
        }
        println!("RESOURCE_BURNED");
        std::io::stdout().flush().unwrap();
    }
}

fn command() -> std::process::Child {
    let mut child = ProcessCommand::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "resource_totals::tests::resource_command_fixture",
            "--nocapture",
        ])
        .env("SUBC_RESOURCE_COMMAND_FIXTURE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        if line.contains("RESOURCE_READY") {
            break;
        }
    }
    child.stdout = Some(output.into_inner());
    child
}

#[cfg(unix)]
fn module_tree(table: &ProcessTable, pid: u32) -> (TreeReading, BTreeSet<(u32, u64)>) {
    let tree = table
        .tree(pid, subc_os::start_time(pid).unwrap(), false)
        .unwrap();
    let identities = tree
        .members
        .iter()
        .map(|member| (member.pid, member.start_time))
        .collect();
    (wire(tree, TreeSource::ProcessTree), identities)
}

#[test]
#[cfg(unix)]
fn agents_move_cpu_and_settle_once_with_report_and_fallback() {
    for reported in [false, true] {
        // A separate module parent excludes unrelated tests' rapidly exiting
        // children from this complete-tree assertion.
        let mut parent = ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "resource_totals::tests::resource_command_fixture",
                "--nocapture",
            ])
            .env("SUBC_RESOURCE_COMMAND_FIXTURE", "parent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(parent.stdout.take().unwrap());
        let marker = |output: &mut BufReader<std::process::ChildStdout>, prefix: &str| -> String {
            loop {
                let mut line = String::new();
                assert_ne!(output.read_line(&mut line).unwrap(), 0);
                if let Some(value) = line.trim().strip_prefix(prefix) {
                    return value.into();
                }
            }
        };
        let child = marker(&mut output, "CHILD_READY ");
        let child_pid = child.split_whitespace().next().unwrap().parse().unwrap();
        let root = AgentCommandRoot {
            pid: child_pid,
            start_time_ms: subc_os::process_start_time_ms(child_pid).unwrap(),
        };
        let generation = identity(parent.id(), subc_os::start_time(parent.id()).unwrap());
        let mut module = Module::default();
        module.start("test-module", parent.id(), generation, root, &[], false);
        assert_eq!(module.roots.len(), 1);
        writeln!(parent.stdin.as_mut().unwrap(), "burn").unwrap();
        marker(&mut output, "CHILD_BURNED");
        // A repeated declaration at the accepted clock precision is not a
        // second command, and must not subtract the same process twice.
        module.start(
            "test-module",
            parent.id(),
            generation,
            AgentCommandRoot {
                start_time_ms: root.start_time_ms + subc_os::process_start_time_precision_ms() - 1,
                ..root
            },
            &[],
            false,
        );
        assert_eq!(module.roots.len(), 1);
        let table = ProcessTable::read().unwrap();
        let raw = module_tree(&table, parent.id());
        let (attributed, agents) = module
            .prospective(generation, Some(raw), &table, now_ms(), 0)
            .unwrap();
        assert!(
            agents.cpu_ms >= 100,
            "fixture must really burn CPU: {agents:?}"
        );
        assert_eq!(
            attributed.unwrap().cpu_ms + agents.cpu_ms,
            module.cpu_floor.total()
        );
        assert!(agents.process_count >= 1);
        let credited = agents.cpu_ms;
        let reported_cpu = credited + 10;
        writeln!(parent.stdin.as_mut().unwrap(), "reap").unwrap();
        marker(&mut output, "CHILD_REAPED");
        module.finish(root, reported.then_some(reported_cpu));
        module.finish(root, Some(credited + 10_000));
        let table = ProcessTable::read().unwrap();
        let raw = module_tree(&table, parent.id());
        let retained_container = raw.clone();
        let (attributed, agents) = module
            .prospective(generation, Some(raw), &table, now_ms(), 0)
            .unwrap();
        let raw_cpu = module.cpu_floor.total();
        #[cfg(unix)]
        assert_eq!(
            module
                .prospective(0, Some(retained_container), &table, now_ms(), 0)
                .unwrap()
                .0
                .unwrap()
                .cpu_ms,
            raw_cpu.saturating_sub(credited),
            "a missing root must not reset container attribution"
        );
        #[cfg(windows)]
        let _ = retained_container;
        assert_eq!(
            agents.cpu_ms, credited,
            "a duplicate finish must be ignored"
        );
        assert_eq!(agents.process_count, 0);
        assert_eq!(agents.memory_bytes, 0);
        #[cfg(unix)]
        assert_eq!(
            attributed.unwrap().cpu_ms,
            raw_cpu.saturating_sub(credited),
            "reaped CPU must be subtracted exactly once"
        );
        #[cfg(windows)]
        let _ = (attributed, raw_cpu);
        writeln!(parent.stdin.take().unwrap(), "exit").unwrap();
        assert!(parent.wait().unwrap().success());
    }
}

#[test]
fn foreign_roots_and_wrong_start_times_are_ignored() {
    let mut child = command();
    let pid = child.id();
    let root = AgentCommandRoot {
        pid,
        start_time_ms: subc_os::process_start_time_ms(pid).unwrap(),
    };
    let mut module = Module::default();
    module.start("test-module", u32::MAX, 1, root, &[], false);
    module.start("test-module", u32::MAX, 1, root, &[], false);
    module.start(
        "test-module",
        std::process::id(),
        1,
        AgentCommandRoot {
            start_time_ms: 0,
            ..root
        },
        &[],
        false,
    );
    writeln!(child.stdin.take().unwrap(), "exit").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(module.roots.is_empty());
    assert_eq!(module.rejected.len(), 2, "each invalid root is logged once");
}

#[test]
fn windows_accumulator_preserves_exited_member_cpu_and_pid_reuse() {
    let mut cpu = cpu_ledger::Ledger::new(false);
    let observe = |cpu: &mut cpu_ledger::Ledger, revision, items: &[((u32, u64), u64)]| {
        let alive = items.iter().map(|(id, _)| *id).collect();
        cpu.observe(
            revision,
            items.iter().map(|(id, value)| cpu_ledger::Member {
                identity: *id,
                parent: None,
                own_ms: *value,
                reaped_ms: 0,
                new_since_baseline: true,
            }),
            &alive,
        );
        cpu.total()
    };
    assert_eq!(observe(&mut cpu, 1, &[((1, 10), 5), ((2, 20), 100)]), 105);
    assert_eq!(observe(&mut cpu, 2, &[((1, 10), 8)]), 108);
    assert_eq!(observe(&mut cpu, 3, &[((1, 10), 9), ((2, 30), 2)]), 111);
    assert_eq!(observe(&mut cpu, 4, &[]), 111);
    assert_eq!(observe(&mut cpu, 5, &[]), 111);
    assert_eq!(cpu.tracked(), 0);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_declared_breakaway_tree_keeps_cpu_after_child_and_root_exit() {
    windows_declared_tree(false).await;
}

#[cfg(windows)]
#[tokio::test]
async fn windows_in_job_finish_replaces_the_root_component_and_keeps_exited_descendants() {
    windows_declared_tree(true).await;
}

#[cfg(windows)]
#[tokio::test]
async fn windows_partial_known_stops_live_attribution_and_keeps_post_finish_cpu_module_side() {
    windows_stop_snapshot(false).await;
}

#[cfg(windows)]
#[tokio::test]
async fn windows_first_stop_snapshot_survives_a_failed_commit() {
    windows_stop_snapshot(true).await;
}

#[cfg(windows)]
async fn windows_stop_snapshot(retry: bool) {
    fn line(output: &mut impl BufRead, prefix: &str) {
        loop {
            let mut text = String::new();
            assert_ne!(output.read_line(&mut text).unwrap(), 0);
            if text.contains(prefix) {
                return;
            }
        }
    }
    for partial in [true, false] {
        let mut program = ProcessCommand::new(std::env::current_exe().unwrap());
        program
            .args([
                "--exact",
                "resource_totals::tests::resource_command_fixture",
                "--nocapture",
            ])
            .env("SUBC_RESOURCE_COMMAND_FIXTURE", "parent")
            .env("SUBC_RESOURCE_PARENT_FINAL_BURN", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let spawned = subc_jobobject::spawn_contained(&mut program).unwrap();
        let mut root = spawned.child;
        let job = Arc::new(Mutex::new(spawned.job));
        let target = crate::subtree_resources::Target::Job(Arc::downgrade(&job));
        let launch = crate::supervise::ResourceLaunch {
            pid: Some(std::process::id()),
            start: subc_os::start_time(std::process::id()),
            spawn_generation: 1,
            enabled: true,
            alive: true,
            target: target.clone(),
        };
        let mut output = BufReader::new(root.stdout.take().unwrap());
        line(&mut output, "CHILD_READY");
        let identity = AgentCommandRoot {
            pid: root.id(),
            start_time_ms: subc_os::process_start_time_ms(root.id()).unwrap(),
        };
        let tracker = Arc::new(Tracker::default());
        tracker.declare(
            "early-finish",
            std::process::id(),
            launch.generation(),
            target.clone(),
            Declaration {
                started: vec![identity],
                ..Declaration::default()
            },
        );
        writeln!(root.stdin.as_mut().unwrap(), "burn").unwrap();
        line(&mut output, "CHILD_BURNED");
        let read = || {
            tracker.read_observed(
                BTreeMap::from([("early-finish".into(), Some(launch.clone()))]),
                Arc::new(ProcessTable::read().unwrap()),
                false,
                now_ms(),
                Instant::now() + Duration::from_secs(5),
            )
        };
        read();
        writeln!(root.stdin.as_mut().unwrap(), "reap").unwrap();
        line(&mut output, "CHILD_REAPED");
        let batch = Declaration {
            known: partial.then_some(Vec::new()),
            finished: if partial {
                Vec::new()
            } else {
                vec![AgentCommandFinished {
                    pid: identity.pid,
                    start_time_ms: identity.start_time_ms,
                    cpu_ms: None,
                }]
            },
            ..Declaration::default()
        };
        let mut receipt = Receipt::default();
        let first = tracker
            .prepare(
                "early-finish",
                launch.generation(),
                std::process::id(),
                target.clone(),
                batch.clone(),
                &mut receipt,
            )
            .unwrap();
        let first_module_cpu = first.module.cpu_model.rows().0;
        let expected_snapshot_cpu: u64 = {
            let command = &first.module.roots[&identity];
            command
                .native_latest_samples
                .iter()
                .map(|(member, cpu)| {
                    cpu.checked_sub(
                        command
                            .native_baseline_samples
                            .get(member)
                            .copied()
                            .unwrap_or(0),
                    )
                    .unwrap()
                })
                .sum()
        };
        if retry {
            tracker.inject_revision_conflicts(1);
            assert!(tracker
                .commit_fenced(
                    "early-finish",
                    launch.generation(),
                    first,
                    &CommitFence::default()
                )
                .is_none());
            // The root burns more CPU before exiting. No sampler runs between
            // attempts: only the retried declaration could misattribute it.
            writeln!(root.stdin.take().unwrap(), "exit").unwrap();
            assert!(root.wait().unwrap().success());
            let next = tracker
                .prepare(
                    "early-finish",
                    launch.generation(),
                    std::process::id(),
                    target,
                    batch,
                    &mut receipt,
                )
                .unwrap();
            assert!(tracker
                .commit_fenced(
                    "early-finish",
                    launch.generation(),
                    next,
                    &CommitFence::default()
                )
                .is_some());
        } else {
            assert!(tracker
                .commit_fenced(
                    "early-finish",
                    launch.generation(),
                    first,
                    &CommitFence::default()
                )
                .is_some());
        }
        let stopped = read();
        if !retry {
            writeln!(root.stdin.take().unwrap(), "exit").unwrap();
            assert!(root.wait().unwrap().success());
        }
        let after = read();
        let stopped_agents = &stopped.agent_commands.as_ref().unwrap()[0];
        let agents = &after.agent_commands.as_ref().unwrap()[0];
        assert!(
            stopped_agents.cpu_ms >= 100,
            "the independently CPU-burning child must earn nonzero pre-finish credit"
        );
        assert_eq!(
            stopped_agents.process_count, 0,
            "a partial replacement must remove the live root immediately"
        );
        assert!(
            stopped_agents.groups.is_empty(),
            "stopped commands have no live program groups"
        );
        // Pending pre-stop credit may settle after the stop. The independent
        // raw snapshot, not the already-paid row, is the exact allowed total.
        assert!(stopped_agents.cpu_ms <= expected_snapshot_cpu);
        println!("WINDOWS_FROZEN_SNAPSHOT partial={partial} credited_at_stop={} raw_snapshot={expected_snapshot_cpu} credited_after={}", stopped_agents.cpu_ms, agents.cpu_ms);
        assert_eq!(
            agents.cpu_ms, expected_snapshot_cpu,
            "CPU observed after the stop snapshot belongs to the module"
        );
        assert!(
            after.modules["early-finish"].0.as_ref().unwrap().cpu_ms
                > if retry {
                    first_module_cpu
                } else {
                    stopped.modules["early-finish"].0.as_ref().unwrap().cpu_ms
                }
        );
        // Only the non-retry case omits a root that is still alive. The retry
        // observes its confirmed exit, so no live identity was lost there.
        if partial && !retry {
            assert!(
                tracker.loss_diagnostics()["early-finish"]["lost_identities"]
                    .as_u64()
                    .unwrap()
                    >= 1
            );
        } else if !partial {
            assert_eq!(
                agents.bound, None,
                "post-finish module work does not make agent accounting incomplete"
            );
        }
        let state = tracker.state.lock().unwrap();
        let rows = state.modules["early-finish"].cpu_model.rows();
        assert_eq!(rows.0 + rows.1, rows.2);
        assert_eq!(rows.1, agents.cpu_ms);
        assert_eq!(
            after.modules["early-finish"].0.as_ref().unwrap().cpu_ms + agents.cpu_ms,
            rows.2
        );
    }
}

#[cfg(windows)]
async fn windows_declared_tree(contained: bool) {
    fn until(output: &mut impl BufRead, prefix: &str) -> String {
        loop {
            let mut line = String::new();
            assert_ne!(output.read_line(&mut line).unwrap(), 0);
            if let Some(at) = line.find(prefix) {
                return line[at + prefix.len()..].trim().to_owned();
            }
        }
    }
    async fn observe(
        tracker: &Arc<Tracker>,
        launch: &crate::supervise::ResourceLaunch,
        table: Option<ProcessTable>,
    ) -> Readings {
        let table = Arc::new(table.unwrap_or_else(|| ProcessTable::read().unwrap()));
        let tracker = Arc::clone(tracker);
        let launch = launch.clone();
        crate::subtree_resources::bounded(
            Instant::now() + Duration::from_secs(1),
            move |deadline| {
                tracker.read_observed(
                    BTreeMap::from([("test-module".into(), Some(launch))]),
                    table,
                    false,
                    now_ms(),
                    deadline,
                )
            },
            |_| Readings::default(),
        )
        .await
    }
    async fn tick(
        tracker: &Arc<Tracker>,
        launch: &crate::supervise::ResourceLaunch,
    ) -> AgentCommandResources {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // A fresh snapshot can still contain unreadable descendants while
            // processes exit. Retry the walk, not the accounting assertions.
            let readings = observe(tracker, launch, None).await;
            if let Some(mut agents) = readings.agent_commands {
                return agents
                    .pop()
                    .expect("a complete walk reports agent commands");
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a complete resource walk: {:?}",
                readings.reasons
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let earlier_table = ProcessTable::read().unwrap();
    let mut command = ProcessCommand::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "resource_totals::tests::resource_command_fixture",
            "--nocapture",
        ])
        .env("SUBC_RESOURCE_COMMAND_FIXTURE", "parent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let (mut root, owner) = if contained {
        command.env("SUBC_RESOURCE_PARENT_FINAL_BURN", "1");
        let child = subc_jobobject::spawn_contained(&mut command).unwrap();
        (child.child, Some(Arc::new(Mutex::new(child.job))))
    } else {
        (command.spawn().unwrap(), None)
    };
    let target = owner.as_ref().map_or(
        crate::subtree_resources::Target::Unavailable("no_job"),
        |owner| crate::subtree_resources::Target::Job(Arc::downgrade(owner)),
    );
    let launch = crate::supervise::ResourceLaunch {
        pid: Some(std::process::id()),
        start: subc_os::start_time(std::process::id()),
        spawn_generation: 1,
        enabled: true,
        alive: true,
        target: target.clone(),
    };
    let mut output = BufReader::new(root.stdout.take().unwrap());
    let child = until(&mut output, "CHILD_READY ");
    let child_pid: u32 = child.split_whitespace().next().unwrap().parse().unwrap();
    let child_cpu: u64 = child.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert!(child_cpu >= 100);
    let identity = AgentCommandRoot {
        pid: root.id(),
        start_time_ms: subc_os::process_start_time_ms(root.id()).unwrap(),
    };
    let tracker = Arc::new(Tracker::default());
    let generation = launch.generation();
    let committed = tracker.declare(
        "test-module",
        std::process::id(),
        generation,
        target.clone(),
        Declaration {
            started: vec![identity],
            ..Declaration::default()
        },
    );
    assert!(
        committed.rejected.is_empty() && !committed.roots_full,
        "start refused: rejected={:?}, roots_full={}",
        committed.rejected,
        committed.roots_full
    );
    // Acknowledged starts can be pending while the process walk is incomplete.
    // Tick until validation accepts the root; never re-declare or move CPU early.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if tracker.state.lock().unwrap().modules["test-module"]
            .roots
            .contains_key(&identity)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "acknowledged root was not accepted: {identity:?}"
        );
        let _ = observe(&tracker, &launch, None).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let baseline_root = tracker.state.lock().unwrap().modules["test-module"].roots[&identity]
        .cpu_floor
        .own((identity.pid, subc_os::start_time(identity.pid).unwrap()))
        .unwrap();
    let before_stale = tracker.state.lock().unwrap().modules["test-module"]
        .cpu_model
        .rows();
    let _ = observe(&tracker, &launch, Some(earlier_table)).await;
    assert_eq!(
        tracker.state.lock().unwrap().modules["test-module"]
            .cpu_model
            .rows(),
        before_stale,
        "a snapshot from before acceptance cannot move CPU"
    );
    assert!(
        tracker.has_live_roots(),
        "a snapshot from before declaration must not disable the command tick"
    );
    writeln!(root.stdin.as_mut().unwrap(), "burn").unwrap();
    until(&mut output, "CHILD_BURNED");
    let before = tick(&tracker, &launch).await;
    assert!(
        before.cpu_ms >= 100,
        "only post-declaration CPU belongs to agents"
    );
    let reported_root_cpu = subc_os::resource_usage(root.id()).map(cpu_ms).unwrap();
    writeln!(root.stdin.as_mut().unwrap(), "reap").unwrap();
    until(&mut output, "CHILD_REAPED");
    let after = tick(&tracker, &launch).await;
    assert!(
        after.cpu_ms >= before.cpu_ms,
        "child CPU dropped after reap: {before:?} -> {after:?}"
    );
    let sampled_root = tracker.state.lock().unwrap().modules["test-module"].roots[&identity]
        .cpu_floor
        .own((identity.pid, subc_os::start_time(identity.pid).unwrap()))
        .unwrap();
    let (after_rows, measured_after, credited_after, pending_after) = {
        let state = tracker.state.lock().unwrap();
        let module = &state.modules["test-module"];
        let command = &module.roots[&identity];
        (
            module.cpu_model.rows(),
            command.measured_cpu,
            command.credited_cpu,
            module.cpu_model.pending_for(identity),
        )
    };
    // Process counters can grow before the job total covers the same work.
    // Retained native counters include descendants not yet credited to agents.
    writeln!(root.stdin.take().unwrap(), "exit").unwrap();
    assert!(root.wait().unwrap().success());
    let confirmed_root = tracker
        .command_process("test-module", identity)
        .unwrap()
        .cumulative_cpu_ms()
        .unwrap();
    if !contained {
        let after_exit = tick(&tracker, &launch).await;
        assert!(after_exit.cpu_ms >= after.cpu_ms);
        assert!(
            !tracker.has_live_roots(),
            "exited roots must stop the five-second tick"
        );
    }
    tracker.declare(
        "test-module",
        std::process::id(),
        generation,
        target,
        Declaration {
            finished: vec![AgentCommandFinished {
                pid: identity.pid,
                start_time_ms: identity.start_time_ms,
                cpu_ms: Some(reported_root_cpu),
            }],
            ..Declaration::default()
        },
    );
    let finished = tick(&tracker, &launch).await;
    // Compare against the exact pinned sample consumed by the daemon, not an
    // earlier independent query, while still checking native monotonicity.
    let observed_root = tracker.state.lock().unwrap().modules["test-module"]
        .finished_root_cpu
        .unwrap();
    let (native_baselines, native_finals) = tracker.state.lock().unwrap().modules["test-module"]
        .finished_native_samples
        .clone()
        .unwrap();
    let names = tracker.state.lock().unwrap().modules["test-module"]
        .finished_native_names
        .clone();
    for (identity, cpu) in &native_baselines {
        println!("WINDOWS_NATIVE_INPUT identity={identity:?} image={:?} baseline_cpu={cpu} final_cpu={:?}", names.get(identity), native_finals.get(identity));
    }
    assert!(native_baselines.keys().any(|(pid, _)| *pid == identity.pid));
    assert!(native_baselines.keys().any(|(pid, _)| *pid == child_pid));
    assert!(native_baselines
        .keys()
        .all(|identity| native_finals.contains_key(identity)));
    let job_samples = tracker.state.lock().unwrap().modules["test-module"].native_job_samples;
    assert_eq!(job_samples.is_some(), contained);
    let expected_cpu = windows_finish_cpu_reference(&native_baselines, &native_finals, job_samples);
    // Include every native identity, including conhost, but respect the job's
    // independently rounded growth. No attributed or pending credit funds this reference.
    assert!(observed_root >= confirmed_root);
    println!("WINDOWS_FINISH contained={contained} job_samples={job_samples:?} baseline_root={baseline_root} sampled_root={sampled_root} early_final_root={confirmed_root} observed_final_root={observed_root} before_agents={} after_agents={} after_rows={after_rows:?} native_command_after={measured_after} credited_after={credited_after} pending_after={pending_after} final_rows={:?} final_agents={}",
        before.cpu_ms, after.cpu_ms, tracker.state.lock().unwrap().modules["test-module"].cpu_model.rows(), finished.cpu_ms);
    assert!(finished.cpu_ms >= after.cpu_ms);
    assert!(finished.cpu_ms >= 100);
    assert_eq!(
        finished.cpu_ms, expected_cpu,
        "finish must keep descendants and replace only the sampled root CPU"
    );
    if contained {
        assert_eq!(
            tracker.state.lock().unwrap().modules["test-module"]
                .cpu_model
                .rows()
                .1,
            finished.cpu_ms
        );
        assert!(
            confirmed_root > sampled_root,
            "the root must burn after its last sample"
        );
    }
    assert_eq!(finished.process_count, 0);
    assert_eq!(finished.memory_bytes, 0);
}

fn windows_finish_cpu_reference(
    baselines: &BTreeMap<(u32, u64), u64>,
    finals: &BTreeMap<(u32, u64), u64>,
    job_samples: Option<(u64, u64)>,
) -> u64 {
    let process_cpu: u64 = finals
        .iter()
        .map(|(identity, final_cpu)| {
            final_cpu
                .checked_sub(baselines.get(identity).copied().unwrap_or(0))
                .unwrap()
        })
        .sum();
    // Subtracting individually floored endpoints can exceed the growth of
    // the once-rounded job total. Inside attribution must fit both readings.
    job_samples.map_or(process_cpu, |(before, after)| {
        process_cpu.min(after.checked_sub(before).unwrap())
    })
}

#[test]
fn windows_finish_reference_respects_rounded_job_counter_growth() {
    // Fixed user/kernel counters in Windows' 100 ns units reproduce the
    // different rounding boundaries of per-process and combined job totals.
    let before_ticks = [(0_u64, 0_u64), (156_250, 0), (1_562_500, 1_875_000)];
    let after_ticks = [
        (1_562_500_u64, 1_718_750_u64),
        (156_250, 0),
        (2_187_500, 2_812_500),
    ];
    let process_ms = |samples: &[(u64, u64)]| {
        samples
            .iter()
            .enumerate()
            .map(|(index, (user, kernel))| ((index as u32 + 1, 1), (user + kernel) / 10_000))
            .collect::<BTreeMap<_, _>>()
    };
    let job_ms = |samples: &[(u64, u64)]| {
        samples
            .iter()
            .map(|(user, kernel)| user + kernel)
            .sum::<u64>()
            / 10_000
    };
    let baselines = process_ms(&before_ticks);
    let finals = process_ms(&after_ticks);
    let job_samples = (job_ms(&before_ticks), job_ms(&after_ticks));
    assert_eq!(job_samples, (359, 843));
    assert_eq!(windows_finish_cpu_reference(&baselines, &finals, None), 485);
    assert_eq!(
        windows_finish_cpu_reference(&baselines, &finals, Some(job_samples)),
        484,
        "individually rounded endpoints cannot authorize more CPU than job growth"
    );

    let mut model = counter_model::Model::default();
    model.observe(counter_model::Kind::WindowsJob, 1, job_samples.0, &[], 0);
    model.observe(
        counter_model::Kind::WindowsJob,
        1,
        job_samples.1,
        &[counter_model::Demand {
            owner: Some(AgentCommandRoot {
                pid: 1,
                start_time_ms: 1,
            }),
            identity: Some((1, 1)),
            cpu: 485,
            before: job_samples.0,
            inside_at_both_ends: true,
        }],
        job_samples.0,
    );
    assert_eq!(model.rows(), (359, 484, 843));
}

#[tokio::test]
async fn process_walk_is_shared_within_a_second() {
    let tracker = Arc::new(Tracker::default());
    let first = tracker.table().await.unwrap();
    let shared = tracker.table().await.unwrap();
    assert!(Arc::ptr_eq(&first, &shared));
    tracker.cache.lock().unwrap().as_mut().unwrap().0 = Instant::now() - Duration::from_secs(2);
    let fresh = tracker.table().await.unwrap();
    assert!(
        !Arc::ptr_eq(&first, &fresh),
        "expired snapshots must trigger a new process walk"
    );
}

#[test]
fn command_tombstones_expire_by_age_without_a_process_table_read() {
    let root = AgentCommandRoot {
        pid: 42,
        start_time_ms: 100,
    };
    let mut module = Module::default();
    module.finished.insert(root, 1_000);
    module.rejected.insert(root, 1_000);
    module.prune(600_999);
    assert_eq!(module.finished.len(), 1);
    assert_eq!(module.rejected.len(), 1);
    module.prune(601_000);
    assert!(module.finished.is_empty());
    assert!(module.rejected.is_empty());
}

#[test]
fn settled_credit_survives_restarts_without_debiting_a_new_counter_twice() {
    let mut module = Module::default();
    for generation in 1..=256_u128 {
        module.bind_counter(generation, generation, None, None);
        module
            .cpu_model
            .observe(counter_model::Kind::WindowsJob, generation, 0, &[], 0);
        module.cpu_model.observe(
            counter_model::Kind::WindowsJob,
            generation,
            100,
            &[counter_model::Demand {
                owner: None,
                identity: None,
                cpu: 25,
                before: 0,
                inside_at_both_ends: true,
            }],
            100,
        );
        let root = AgentCommandRoot {
            pid: 1,
            start_time_ms: generation as u64,
        };
        module.roots.insert(
            root,
            Command {
                measured: true,
                ..Command::default()
            },
        );
        module.finish(root, Some(u64::MAX));
        module.finish(root, Some(u64::MAX));
        assert_eq!(
            module.cpu_model.rows(),
            (
                75 * generation as u64,
                25 * generation as u64,
                100 * generation as u64
            )
        );
        module
            .cpu_model
            .observe(counter_model::Kind::WindowsJob, generation, 100, &[], 100);
        assert_eq!(
            module.cpu_model.rows().0,
            75 * generation as u64,
            "settled CPU cannot charge a later native counter again"
        );
        assert!(module.roots.is_empty());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn empty_container_keeps_reaped_cpu_without_live_processes() {
    let dir = cortexkit_test_support::ScratchDir::new("empty-cgroup-resources");
    std::fs::write(dir.join("cpu.stat"), "usage_usec 1234000\n").unwrap();
    std::fs::write(dir.join("memory.current"), "512\n").unwrap();
    std::fs::write(dir.join("cgroup.procs"), "").unwrap();
    let table = ProcessTable::read().unwrap();
    let (tree, identities) = crate::subtree_resources::Target::Cgroup(dir.to_path_buf())
        .tree(&table)
        .expect("kernel totals do not require a live member");
    assert_eq!(tree.source, TreeSource::Cgroup);
    assert_eq!(
        (tree.cpu_ms, tree.memory_bytes, tree.process_count),
        (1234, 512, 0)
    );
    assert!(tree.groups.is_empty());
    assert!(identities.is_empty());
}

#[cfg(unix)]
#[test]
fn finish_only_declaration_measures_cpu_without_a_list_or_minute_sample() {
    for reported in [None, Some(u64::MAX)] {
        let mut parent = ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "resource_totals::tests::resource_command_fixture",
                "--nocapture",
            ])
            .env("SUBC_RESOURCE_COMMAND_FIXTURE", "parent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(parent.stdout.take().unwrap());
        let read_marker =
            |output: &mut BufReader<std::process::ChildStdout>, marker: &str| -> String {
                loop {
                    let mut line = String::new();
                    assert_ne!(output.read_line(&mut line).unwrap(), 0);
                    if let Some(value) = line.trim().strip_prefix(marker) {
                        return value.into();
                    }
                }
            };
        let child = read_marker(&mut output, "CHILD_READY ");
        let pid = child.split_whitespace().next().unwrap().parse().unwrap();
        let root = AgentCommandRoot {
            pid,
            start_time_ms: subc_os::process_start_time_ms(pid).unwrap(),
        };
        let tracker = Tracker::default();
        let parent_pid = parent.id();
        let declare = |batch| {
            let prepared = tracker
                .prepare(
                    "module",
                    1,
                    parent_pid,
                    crate::subtree_resources::Target::Unavailable("no_container"),
                    batch,
                    &mut Receipt::default(),
                )
                .unwrap();
            tracker
                .commit_fenced("module", 1, prepared, &CommitFence::default())
                .unwrap();
        };
        declare(Declaration {
            started: vec![root],
            ..Declaration::default()
        });
        assert_eq!(
            tracker.state.lock().unwrap().modules["module"]
                .cpu_model
                .rows()
                .1,
            0
        );
        writeln!(parent.stdin.as_mut().unwrap(), "burn").unwrap();
        read_marker(&mut output, "CHILD_BURNED");
        declare(Declaration {
            finished: vec![AgentCommandFinished {
                pid,
                start_time_ms: root.start_time_ms,
                cpu_ms: reported,
            }],
            ..Declaration::default()
        });
        let credited = tracker.state.lock().unwrap().modules["module"]
            .cpu_model
            .rows()
            .1;
        assert!(
            credited >= 100,
            "finish must measure the CPU burned after baseline, not settle stale zero"
        );
        declare(Declaration {
            finished: vec![AgentCommandFinished {
                pid,
                start_time_ms: root.start_time_ms,
                cpu_ms: Some(u64::MAX),
            }],
            ..Declaration::default()
        });
        assert_eq!(
            tracker.state.lock().unwrap().modules["module"]
                .cpu_model
                .rows()
                .1,
            credited
        );
        writeln!(parent.stdin.as_mut().unwrap(), "reap").unwrap();
        read_marker(&mut output, "CHILD_REAPED");
        writeln!(parent.stdin.take().unwrap(), "exit").unwrap();
        assert!(parent.wait().unwrap().success());
    }
}
