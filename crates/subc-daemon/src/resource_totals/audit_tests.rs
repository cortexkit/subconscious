use super::*;

#[test]
fn unsampled_attributed_cpu_survives_a_clean_restart_until_a_measured_hour_is_zero() {
    let mut module = Module {
        generation: 2,
        ..Module::default()
    };
    progress(&mut module, 0, 0, 10);
    assert!(
        module.should_emit_agents(None),
        "no hour exists yet to show that attributed CPU has left it"
    );
    assert!(
        !module.should_emit_agents(Some(0)),
        "a measured zero hour permits a clean undeclared launch to hide its old row"
    );
}

#[test]
fn omitted_identity_proofs_have_the_same_hard_bound_as_loss_identities() {
    let tracker = Tracker::default();
    let mut module = Module {
        declaration_loss: tracker.declaration_loss("omitted-cap"),
        ..Default::default()
    };
    for pid in 2..=1_026 {
        module.roots.insert(
            root(pid),
            Command {
                native_start: 1,
                live: true,
                last_seen_ms: now_ms(),
                ..Command::default()
            },
        );
    }
    let table = ProcessTable::from_test_members(
        1,
        now_ms(),
        Vec::new(),
        (2..=1_026).map(|pid| (pid, Some(1), None)).collect(),
    );
    let prepared = Prepared {
        scope_target: crate::subtree_resources::Target::Unavailable("no_container"),
        module_pid: 1,
        before_cpu: None,
        started: Vec::new(),
        finished: Vec::new(),
        known: Some(Vec::new()),
        table,
        at_ms: now_ms(),
        finish_until: u64::MAX,
        #[cfg(unix)]
        scope: None,
    };
    let applied = tracker.apply_prepared(&mut module, 1, prepared);
    assert_eq!(module.omitted.len(), 1_024);
    assert!(applied.omitted_overflow);
    tracker
        .commit_fenced(
            "omitted-cap",
            1,
            PreparedCommit {
                expected_revision: 0,
                module,
                rejected: Vec::new(),
                recovered: Vec::new(),
                complete_recovery: None,
                lost: Vec::new(),
                roots_full: false,
                omitted_overflow: applied.omitted_overflow,
            },
            &CommitFence::default(),
        )
        .unwrap();
    assert_eq!(tracker.loss_diagnostics()["omitted-cap"]["overflow"], true);
}

#[test]
fn a_backward_wall_step_cannot_clear_the_bound_on_returned_uncertain_samples() {
    let clock = Clock::manual(Duration::ZERO);
    let mut module = Module {
        clock: clock.clone(),
        declared_generation: Some(0),
        last_source: Some(TreeSource::Cgroup),
        ..Module::default()
    };
    module.record_sample(0, Some(0), Some(0));
    module.agents_lower = true;
    clock.set(Duration::from_secs(60));
    module.recent_agent_loss.mark(clock.now());
    module.record_sample(60_000, Some(10), Some(10));
    clock.set(Duration::from_secs(7_200));
    module.record_sample(30_000, Some(20), Some(20));
    let hour = module.agents_last_hour(30_000).unwrap();
    assert_eq!((hour.cpu_ms, hour.covered_ms), (10, 60_000));
    assert_eq!(hour.bound, Some(subc_control::ResourceBound::Lower));
}

#[test]
fn a_source_switch_retains_uncertainty_until_its_sample_leaves_the_returned_hour() {
    let clock = Clock::manual(Duration::ZERO);
    let mut module = Module {
        clock: clock.clone(),
        declared_generation: Some(0),
        last_source: Some(TreeSource::ProcessTree),
        ..Module::default()
    };
    module.record_sample(0, Some(0), Some(0));
    module.record_sample(60_000, Some(10), Some(10));
    module.last_source = Some(TreeSource::Cgroup);
    clock.set(Duration::from_secs(7_200));
    module.record_sample(120_000, Some(20), Some(20));
    assert_eq!(
        module.agents_last_hour(120_000).unwrap().bound,
        Some(subc_control::ResourceBound::Lower)
    );
    for minute in 3..=63 {
        module.record_sample(minute * 60_000, Some(20), Some(20));
    }
    assert_eq!(module.agents_last_hour(63 * 60_000).unwrap().bound, None);
}

#[test]
fn inherited_active_commands_and_current_hour_cpu_keep_the_agents_row_visible() {
    let mut module = Module {
        generation: 1,
        counter: Some(1),
        ..Module::default()
    };
    module.roots.insert(
        root(2),
        Command {
            accepted_generation: 1,
            live: true,
            ..Command::default()
        },
    );
    module.bind_counter(2, 1, None, None);
    assert!(module.should_emit_agents(Some(0)));
    module.roots.clear();
    module.agents_history.record(now_ms() - 60_000, 0, 0);
    module.agents_history.record(now_ms(), 0, 10);
    assert!(module.should_emit_agents(Some(10)));
}

#[test]
fn a_walk_begun_before_acceptance_cannot_prune_an_omitted_identity_proof() {
    let old = ProcessTable::from_test_members(6, 60_000, Vec::new(), Vec::new());
    let mut module = Module::default();
    module.omitted.insert(
        root(2),
        OmittedRoot {
            native_start: 1,
            generation: 1,
            counter: Some(1),
            sequence: 5,
            accepted_at: Instant::now(),
        },
    );
    module.observe_reaped(&old, 60_000);
    assert!(module.omitted.contains_key(&root(2)));
}

#[test]
fn a_newer_revision_walk_begun_before_acceptance_cannot_claim_the_root_is_gone() {
    let earlier = ProcessTable::from_test_members(6, 60_000, Vec::new(), Vec::new());
    let root = root(10);
    let mut module = Module::default();
    module.roots.insert(
        root,
        Command {
            native_start: 1,
            accepted_at: Some(Instant::now()),
            ..Command::default()
        },
    );
    module.observe_reaped(&earlier, 60_000);
    assert_eq!(module.roots[&root].first_gone_sequence, None);
    let fresh = ProcessTable::from_test_members(7, 120_000, Vec::new(), Vec::new());
    module.observe_reaped(&fresh, 120_000);
    assert_eq!(module.roots[&root].first_gone_sequence, Some(7));
}

#[test]
fn loss_only_zero_agents_rows_are_not_suppressed() {
    let module = Module::default();
    assert!(!module.should_emit_agents(Some(0)));
    module.declaration_loss.mark(&[root(2)]);
    assert_eq!(
        module.agent_bound(None),
        Some(subc_control::ResourceBound::Lower)
    );
    assert!(module.should_emit_agents(Some(0)));
}

#[test]
fn an_agent_loss_expires_only_on_the_hourly_bound() {
    let clock = Clock::manual(Duration::from_secs(10));
    let mut module = Module {
        clock: clock.clone(),
        ..Module::default()
    };
    module.declared_generation = Some(0);
    module.record_sample(0, None, Some(0));
    module.roots.insert(root(2), Command::default());
    module.finish_at(root(2), None, 60_000);
    module.record_sample(60_000, None, Some(0));
    assert_eq!(
        module.agent_bound(None),
        Some(subc_control::ResourceBound::Lower)
    );
    assert_eq!(
        module.agents_last_hour(60_000).unwrap().bound,
        Some(subc_control::ResourceBound::Lower)
    );
    clock.set(Duration::from_millis(3_609_999));
    assert_eq!(
        module.agents_last_hour(60_000).unwrap().bound,
        Some(subc_control::ResourceBound::Lower)
    );
    clock.set(Duration::from_secs(3_610));
    assert_eq!(
        module.agents_last_hour(60_000).unwrap().bound,
        Some(subc_control::ResourceBound::Lower)
    );
    module.record_sample(120_000, None, Some(0));
    module.record_sample(3_720_000, None, Some(0));
    assert_eq!(module.agents_last_hour(3_720_000).unwrap().bound, None);
    assert_eq!(
        module.agent_bound(None),
        Some(subc_control::ResourceBound::Lower)
    );
}

fn root(pid: u32) -> AgentCommandRoot {
    AgentCommandRoot {
        pid,
        start_time_ms: 1,
    }
}

fn inherited_command_with_exact_hour() -> Module {
    let mut module = Module {
        generation: 1,
        declared_generation: Some(1),
        counter: Some(1),
        last_source: Some(TreeSource::Cgroup),
        ..Module::default()
    };
    module.roots.insert(
        root(2),
        Command {
            accepted_generation: 1,
            live: true,
            ..Command::default()
        },
    );
    module.record_sample(0, Some(0), Some(0));
    progress(&mut module, 100, 100, 0);
    module.record_sample(60_000, Some(0), Some(100));
    assert_eq!(module.agents_last_hour(60_000).unwrap().bound, None);
    module
}

#[test]
fn last_inherited_finish_keeps_its_missing_cpu_in_the_hour_bound() {
    let mut module = inherited_command_with_exact_hour();
    module.bind_counter(2, 1, None, None);
    // The normal observation advances the launch, not its declaration marker.
    module.generation = 2;
    assert_eq!(module.roots[&root(2)].accepted_generation, 2);
    module.finish_at(root(2), None, 120_000);
    assert!(module.roots.is_empty());
    assert_eq!(module.declared_generation, Some(1));
    module.record_sample(120_000, Some(0), Some(100));
    assert!(module.should_emit_agents(Some(100)));
    assert_eq!(
        module.agent_bound(None),
        Some(subc_control::ResourceBound::Lower)
    );
    let hour = module.agents_last_hour(120_000).unwrap();
    assert_eq!(hour.cpu_ms, 100);
    assert_eq!(hour.bound, Some(subc_control::ResourceBound::Lower));
}

#[test]
fn generation_prune_keeps_the_last_unreported_root_in_the_hour_bound() {
    let tracker = Tracker::default();
    let mut module = inherited_command_with_exact_hour();
    module.roots.get_mut(&root(2)).unwrap().live = false;
    tracker
        .state
        .lock()
        .unwrap()
        .modules
        .insert("pruned".into(), module);
    let launch = crate::supervise::ResourceLaunch {
        pid: Some(1),
        start: Some(1),
        spawn_generation: 2,
        enabled: true,
        alive: true,
        target: crate::subtree_resources::Target::Unavailable("no_container"),
    };
    tracker.prune(
        &BTreeMap::from([("pruned".into(), Some(launch.clone()))]),
        120_000,
    );
    let mut state = tracker.state.lock().unwrap();
    let module = state.modules.get_mut("pruned").unwrap();
    assert!(module.roots.is_empty());
    module.bind_counter(launch.generation(), 1, None, Some(1));
    module.generation = launch.generation();
    assert_ne!(module.declared_generation, Some(module.generation));
    module.record_sample(120_000, Some(0), Some(100));
    assert!(module.should_emit_agents(Some(100)));
    assert_eq!(
        module.agent_bound(None),
        Some(subc_control::ResourceBound::Lower)
    );
    let hour = module.agents_last_hour(120_000).unwrap();
    assert_eq!(hour.cpu_ms, 100);
    assert_eq!(hour.bound, Some(subc_control::ResourceBound::Lower));
}

fn verified(pid: u32, members: &[u32]) -> VerifiedRoot {
    VerifiedRoot {
        root: root(pid),
        native_start: 1,
        inside: true,
        members: members.iter().map(|pid| (*pid, 1)).collect(),
        #[cfg(windows)]
        process: None,
    }
}

fn progress(module: &mut Module, raw: u64, inside: u64, outside: u64) {
    let demands = [
        counter_model::Demand {
            owner: None,
            identity: None,
            cpu: inside,
            before: 0,
            inside_at_both_ends: true,
        },
        counter_model::Demand {
            owner: None,
            identity: None,
            cpu: outside,
            before: 0,
            inside_at_both_ends: false,
        },
    ];
    module
        .cpu_model
        .observe(counter_model::Kind::WindowsJob, 1, raw, &demands, 0);
}

#[test]
fn overlapping_started_and_known_roots_are_refused_in_both_orders() {
    for known in [false, true] {
        for parent_first in [false, true] {
            let tracker = Tracker::default();
            let mut candidates = vec![
                (root(10), Some(verified(10, &[10, 20]))),
                (root(20), Some(verified(20, &[20]))),
            ];
            if !parent_first {
                candidates.reverse();
            }
            let batch = Prepared {
                scope_target: crate::subtree_resources::Target::Unavailable("no_container"),
                #[cfg(unix)]
                scope: None,
                module_pid: 0,
                before_cpu: None,
                started: if known {
                    Vec::new()
                } else {
                    std::mem::take(&mut candidates)
                },
                finished: Vec::new(),
                known: known.then_some(candidates),
                table: ProcessTable::default(),
                at_ms: 1_000,
                finish_until: u64::MAX,
            };
            assert_eq!(tracker.commit("module", 1, batch).unwrap().len(), 1);
            assert_eq!(
                tracker.state.lock().unwrap().modules["module"].roots.len(),
                1
            );
        }
    }
}

#[test]
fn oversized_finish_cannot_credit_cpu_the_daemon_did_not_measure() {
    for reported in [None, Some(u64::MAX)] {
        let mut module = Module::default();
        module.roots.insert(
            root(10),
            Command {
                measured: true,
                ..Command::default()
            },
        );
        progress(&mut module, 200, 60, 40);
        let measured = module.cpu_model.rows();
        assert_eq!(measured, (140, 100, 240));
        module.finish_at(root(10), reported, 1_000);
        module.finish_at(root(10), Some(u64::MAX), 1_001);
        assert_eq!(
            module.cpu_model.rows(),
            measured,
            "finish reports cannot replace measured CPU or settle it twice"
        );
    }
}

#[test]
fn ancestor_after_settle_cannot_transfer_the_same_inside_cpu_twice() {
    let mut module = Module::default();
    module.roots.insert(
        root(10),
        Command {
            measured: true,
            ..Command::default()
        },
    );
    progress(&mut module, 200, 100, 0);
    module.finish_at(root(10), Some(u64::MAX), 1_000);
    assert_eq!(module.cpu_model.rows(), (100, 100, 200));
    progress(&mut module, 220, 0, 0);
    assert_eq!(
        module.cpu_model.rows(),
        (120, 100, 220),
        "later module work cannot pay already consumed command CPU again"
    );
    progress(&mut module, 200, 0, 0);
    assert_eq!(
        module.cpu_model.rows(),
        (120, 100, 220),
        "a stale native sample cannot lower or reattribute cumulative CPU"
    );
}

#[test]
fn outside_consumption_is_not_limited_by_a_module_debit_budget() {
    let mut module = Module::default();
    progress(&mut module, 10, 0, 500);
    assert_eq!(module.cpu_model.rows(), (10, 500, 510));
}

#[test]
fn declaring_a_root_keeps_historical_hourly_cpu_in_the_module_and_preserves_the_stack_total() {
    let mut module = Module::default();
    for (now, raw) in [(0, 0), (60_000, 100), (120_000, 100)] {
        progress(&mut module, raw, 0, 0);
        let (module_cpu, agents_cpu, _) = module.cpu_model.rows();
        module
            .history
            .record_observation(module.table_revision, now, 0, module_cpu);
        module
            .agents_history
            .record_observation(module.table_revision, now, 0, agents_cpu);
    }
    let module_hour = module.last_hour(120_000).unwrap();
    let agents_hour = module.agents_history.last_hour_at(120_000).unwrap();
    assert_eq!(module_hour.covered_ms, agents_hour.covered_ms);
    assert_eq!((module_hour.cpu_ms, agents_hour.cpu_ms), (100, 0));
    assert_eq!(module_hour.cpu_ms + agents_hour.cpu_ms, 100);
}

#[test]
fn missing_module_tree_keeps_confirmed_inside_facts() {
    let mut module = Module {
        generation: 1,
        ..Module::default()
    };
    progress(&mut module, 200, 100, 0);
    module.roots.insert(
        root(10),
        Command {
            measured: true,
            accepted_generation: 1,
            ..Command::default()
        },
    );
    assert!(module
        .prospective(2, None, &ProcessTable::default(), 1_000, 0)
        .is_none());
    assert_eq!(module.generation, 1);
    assert_eq!(module.roots[&root(10)].accepted_generation, 1);
    assert_eq!(module.cpu_model.rows(), (100, 100, 200));
}

#[tokio::test]
async fn daemon_resource_probe_never_holds_tracker_state() {
    let (entered, ready) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let tracker = Arc::new(Tracker {
        own_probe: Some(Arc::new((entered, Mutex::new(wait)))),
        ..Tracker::default()
    });
    let reader = Arc::clone(&tracker);
    let task = tokio::task::spawn_blocking(move || {
        reader.read_observed(
            BTreeMap::new(),
            Arc::new(ProcessTable::default()),
            false,
            1_000,
            Instant::now() + Duration::from_secs(10),
        )
    });
    tokio::task::spawn_blocking(move || ready.recv_timeout(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();
    let unlocked = tracker.state.try_lock().is_ok();
    drop(release);
    task.await.unwrap();
    assert!(
        unlocked,
        "a stalled daemon-resource syscall must not hold attribution state"
    );
}

#[tokio::test]
async fn an_in_progress_process_walk_is_unknown_not_a_second_walk() {
    let tracker = Tracker::default();
    tracker.refreshing.store(true, Ordering::Release);
    assert!(tracker.table().await.is_none());
    assert!(tracker.cache.try_lock().unwrap().is_none());
}

#[test]
fn failed_resource_pass_omits_agent_commands_instead_of_claiming_empty() {
    assert!(Readings::default().agent_commands.is_none());
    let tracker = Tracker::default();
    let reading = tracker.read_observed(
        BTreeMap::from([("contended-module".into(), None)]),
        Arc::new(ProcessTable::default()),
        false,
        0,
        Instant::now() + Duration::from_secs(1),
    );
    assert!(
        reading.agent_commands.is_none(),
        "a contended module snapshot is unknown even before any command has been declared"
    );
}

#[test]
fn confirmed_windows_exits_and_unreadable_members_do_not_recredit_cpu() {
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
    assert_eq!(observe(&mut cpu, 1, &[((10, 1), 10), ((20, 1), 100)]), 110);
    // An incomplete native read is not committed as a partial observation.
    assert_eq!(cpu.total(), 110);
    assert_eq!(observe(&mut cpu, 3, &[((10, 1), 20), ((20, 1), 100)]), 120);
    assert_eq!(observe(&mut cpu, 4, &[((10, 1), 30), ((20, 1), 100)]), 130);
    assert_eq!(observe(&mut cpu, 5, &[]), 130);
    assert_eq!(
        cpu.tracked(),
        0,
        "confirmed exits reduce identity state to retained scalar CPU"
    );
}

#[test]
fn carried_counter_survives_a_launch_but_recreation_starts_a_new_counter() {
    let mut module = Module {
        generation: 1,
        ..Module::default()
    };
    module.bind_counter(1, 100, None, None);
    module
        .cpu_model
        .observe(counter_model::Kind::WindowsJob, 100, 100, &[], 0);
    module
        .history
        .record_observation(module.table_revision, 0, 0, 100);
    module.bind_counter(2, 100, None, None);
    module
        .cpu_model
        .observe(counter_model::Kind::WindowsJob, 100, 150, &[], 0);
    module
        .history
        .record_observation(module.table_revision, 60_000, 0, module.cpu_model.rows().0);
    assert_eq!(module.cpu_model.rows(), (150, 0, 150));
    assert_eq!(module.last_hour(60_000).unwrap().cpu_ms, 50);
    module.bind_counter(3, 101, None, None);
    module
        .cpu_model
        .observe(counter_model::Kind::WindowsJob, 101, 20, &[], 0);
    module
        .history
        .record_observation(module.table_revision, 120_000, 0, module.cpu_model.rows().0);
    assert_eq!(module.cpu_model.rows(), (170, 0, 170));
    assert_eq!(module.last_hour(120_000).unwrap().cpu_ms, 70);
}

#[test]
fn cleanup_evicts_launch_state_but_preserves_bounded_history_and_settled_cpu() {
    let tracker = Tracker::default();
    let launch = crate::supervise::ResourceLaunch {
        pid: Some(10),
        start: Some(1),
        spawn_generation: 2,
        enabled: true,
        alive: true,
        target: crate::subtree_resources::Target::Unavailable("no_container"),
    };
    {
        let mut state = tracker.state.lock().unwrap();
        let module = state.modules.entry("module".into()).or_default();
        module.generation = 1;
        module.roots.insert(
            root(10),
            Command {
                measured: true,
                last_seen_ms: 0,
                ..Command::default()
            },
        );
        progress(module, 100, 100, 0);
        module.finished.insert(root(20), 0);
        module.rejected.insert(root(30), 0);
        module
            .history
            .record_observation(module.table_revision, 0, 1, 10);
        module
            .history
            .record_observation(module.table_revision, 60_000, 1, 20);
    }
    let mut launches = BTreeMap::from([("module".into(), Some(launch))]);
    tracker.prune(&launches, 600_001);
    {
        let state = tracker.state.lock().unwrap();
        let module = &state.modules["module"];
        assert!(module.roots.is_empty());
        assert!(module.finished.is_empty());
        assert!(module.rejected.is_empty());
        assert_eq!(module.cpu_model.rows(), (0, 100, 100));
        assert_eq!(module.history.last_hour_at(600_001).unwrap().cpu_ms, 10);
    }
    launches
        .get_mut("module")
        .unwrap()
        .as_mut()
        .unwrap()
        .enabled = false;
    tracker.prune(&launches, 600_002);
    assert!(tracker.state.lock().unwrap().modules.is_empty());
    tracker
        .state
        .lock()
        .unwrap()
        .modules
        .insert("removed".into(), Module::default());
    tracker.prune(&BTreeMap::new(), 600_003);
    assert!(tracker.state.lock().unwrap().modules.is_empty());
    let mut live = Module::default();
    live.roots.insert(
        root(40),
        Command {
            live: true,
            last_seen_ms: 0,
            ..Command::default()
        },
    );
    live.prune(600_004);
    assert!(
        live.roots.contains_key(&root(40)),
        "a failed refresh must not turn a previously live root into a dead root"
    );
}

#[test]
fn cancelled_declaration_never_creates_or_changes_resource_state() {
    let tracker = Tracker::default();
    let prepared = tracker
        .prepare(
            "cancelled",
            1,
            std::process::id(),
            crate::subtree_resources::Target::Unavailable("no_container"),
            Declaration::default(),
            &mut Receipt::default(),
        )
        .unwrap();
    let fence = CommitFence::default();
    assert!(fence.cancel());
    assert!(tracker
        .commit_fenced("cancelled", 1, prepared, &fence)
        .is_none());
    assert!(
        tracker.state.lock().unwrap().modules.is_empty(),
        "validation finishing after a timeout must leave no accounting mutation"
    );
}

#[cfg(unix)]
#[test]
fn an_older_cached_walk_cannot_end_a_newly_declared_roots_finish_window() {
    let root = root(10);
    let mut module = Module::default();
    module.roots.insert(
        root,
        Command {
            native_start: 1,
            last_cpu_sequence: 5,
            funding_sequence: 5,
            ..Command::default()
        },
    );
    module
        .finish_cpu
        .observe_reaped(Some((1, 10)), Duration::from_millis(60_000), 5);
    module.observe_reaped(
        &ProcessTable::from_test_members(4, 120_000, Vec::new(), Vec::new()),
        120_000,
    );
    assert_eq!(module.roots[&root].first_gone_sequence, None);
    assert!(module.finish_cpu.readable());
    module.observe_reaped(
        &ProcessTable::from_test_members(6, 180_000, Vec::new(), Vec::new()),
        180_000,
    );
    assert_eq!(module.roots[&root].first_gone_sequence, Some(6));
}

#[cfg(unix)]
#[test]
fn cached_rows_keep_their_committed_observation_sequence_for_history_transfers() {
    let tracker = Tracker::default();
    let table = Arc::new(ProcessTable::from_test_members(
        4,
        60_000,
        vec![subc_os::ProcessMember {
            pid: 1,
            parent_pid: 0,
            session_id: Some(0),
            start_time: 1,
            start_time_ms: 1,
            name: "module-own".into(),
            resources: Some(subc_os::ResourceUsage {
                cpu_user: Duration::from_millis(100),
                cpu_system: Duration::ZERO,
                memory_bytes: 1,
                swap_bytes: None,
                memory_kind: subc_os::MemoryKind::ResidentSet,
            }),
            own_cpu_ms: Some(100),
            reaped_cpu_ms: Some(0),
        }],
        Vec::new(),
    ));
    let tree = wire(table.tree(1, 1, false).unwrap(), TreeSource::ProcessTree);
    let agents = AgentCommandResources::new("cached-module", 0, 0, 0);
    let launch = crate::supervise::ResourceLaunch {
        pid: Some(1),
        start: Some(1),
        spawn_generation: 0,
        enabled: true,
        alive: true,
        target: crate::subtree_resources::Target::Unavailable("no_container"),
    };
    let mut module = Module {
        table_revision: 5,
        generation: launch.generation(),
        counter: Some(launch.generation()),
        pid: Some(1),
        last_readings: Some((Some(tree), agents)),
        ..Module::default()
    };
    progress(&mut module, 100, 0, 0);
    module
        .group_credit
        .observe(&ProcessTable::default(), &[], 100, BTreeMap::new())
        .unwrap();
    module.history.record_observation(4, 0, 0, 0);
    module.agents_history.record_observation(4, 0, 0, 0);
    tracker
        .state
        .lock()
        .unwrap()
        .modules
        .insert("cached-module".into(), module);
    let observation = tracker
        .read_observed_before(
            BTreeMap::from([("cached-module".into(), Some(launch))]),
            table,
            true,
            60_000,
            Instant::now() + Duration::from_secs(5),
            BTreeMap::new(),
        )
        .unwrap();
    let readings = tracker.commit_observation(observation);
    assert_eq!(
        readings.modules["cached-module"].0.as_ref().unwrap().cpu_ms,
        100
    );
    let mut state = tracker.state.lock().unwrap();
    let module = state.modules.get_mut("cached-module").unwrap();
    module.transfer_finish(root(10), 5, 100);
    assert_eq!(module.cpu_model.rows(), (0, 100, 100));
    assert_eq!(module.history.last_hour_at(60_000).unwrap().cpu_ms, 0);
    assert_eq!(
        module.agents_history.last_hour_at(60_000).unwrap().cpu_ms,
        100
    );
}

#[test]
fn revalidation_does_not_advance_the_finish_receipts_first_snapshot() {
    let tracker = Tracker::default();
    let mut receipt = Receipt::default();
    let mut prepare = || {
        tracker
            .prepare(
                "receipt",
                1,
                std::process::id(),
                crate::subtree_resources::Target::Unavailable("no_container"),
                Declaration::default(),
                &mut receipt,
            )
            .unwrap()
    };
    let first = prepare().module.finish_cpu.sequence;
    let second = prepare().module.finish_cpu.sequence;
    assert!(
        second > first,
        "revalidation actually took a newer snapshot"
    );
    assert_eq!(receipt.finish_until, Some(first));
}

#[cfg(unix)]
#[test]
fn an_unfunded_historical_transfer_is_atomic_and_marks_the_agents_row_lower() {
    let mut module = Module::default();
    progress(&mut module, 100, 0, 0);
    module
        .group_credit
        .observe(&ProcessTable::default(), &[], 100, BTreeMap::new())
        .unwrap();
    module.history.record_observation(1, 60_000, 0, 0);
    module.agents_history.record_observation(1, 60_000, 0, 0);
    let before = module.cpu_model.rows();
    module.transfer_finish(root(10), 1, 10);
    assert_eq!(module.cpu_model.rows(), before);
    assert!(module.agents_lower);
    assert_eq!(module.group_credit.totals(), (0, 0));
    assert!(
        module.group_credit.transfer(100),
        "history failure cannot consume the module's group budget either"
    );
}

#[test]
fn rejected_and_finished_sets_evict_the_oldest_at_a_hard_per_module_cap() {
    let mut module = Module::default();
    for pid in 1..=1_100 {
        let key = root(pid);
        module.accept(1, key, None, &ProcessTable::default(), u64::from(pid));
        module.roots.insert(
            key,
            Command {
                native_start: 1,
                measured: true,
                ..Command::default()
            },
        );
        module.finish_at(key, None, u64::from(pid));
        assert!(module.rejected.len() <= 1_024 && module.finished.len() <= 1_024);
    }
    assert!(!module.rejected.contains_key(&root(1)) && !module.finished.contains_key(&root(1)));
    assert!(
        module.rejected.contains_key(&root(1_100)) && module.finished.contains_key(&root(1_100))
    );
}

#[test]
fn a_full_live_set_skips_only_the_extra_root_without_growing() {
    let mut module = Module::default();
    for pid in 1..=1_024 {
        module.roots.insert(root(pid), Command::default());
    }
    assert!(module.accept(
        1,
        root(2_000),
        Some(verified(2_000, &[2_000])),
        &ProcessTable::default(),
        1
    ));
    assert_eq!(module.roots.len(), 1_024);
    assert!(!module.roots.contains_key(&root(2_000)));
    assert!(
        !module.accept(
            1,
            root(2_000),
            Some(verified(2_000, &[2_000])),
            &ProcessTable::default(),
            2
        ),
        "one retained rejection is logged only once"
    );
}

#[test]
fn stale_observation_generation_cannot_apply_after_a_declaration_commit() {
    let (started, waiting) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    let tracker = Arc::new(Tracker {
        own_probe: Some(Arc::new((started, Mutex::new(held)))),
        ..Tracker::default()
    });
    let reader = {
        let tracker = Arc::clone(&tracker);
        std::thread::spawn(move || {
            tracker.read_observed(
                BTreeMap::new(),
                Arc::new(ProcessTable::default()),
                false,
                1_000,
                Instant::now() + Duration::from_secs(5),
            )
        })
    };
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    {
        let mut state = tracker.state.lock().unwrap();
        state.revision += 1;
        state
            .modules
            .entry("committed".into())
            .or_default()
            .cpu_model
            .observe(
                counter_model::Kind::WindowsJob,
                1,
                10,
                &[counter_model::Demand {
                    owner: None,
                    identity: None,
                    cpu: 10,
                    before: 0,
                    inside_at_both_ends: true,
                }],
                10,
            );
    }
    release.send(()).unwrap();
    let result = reader.join().unwrap();
    assert!(
        result.daemon.is_none() && result.agent_commands.is_none(),
        "drop the stale observation, not just one root inside it"
    );
    assert_eq!(
        tracker.state.lock().unwrap().modules["committed"]
            .cpu_model
            .rows(),
        (0, 10, 10)
    );
}

#[test]
fn every_new_pass_prunes_exited_members_but_keeps_unreadable_identities_uncertain() {
    let mut module = Module::default();
    module.roots.insert(
        root(10),
        Command {
            members: BTreeSet::from([(1, 1), (2, 1)]),
            ..Command::default()
        },
    );
    let table = ProcessTable::from_test_members(1, 1, Vec::new(), vec![(2, Some(10), None)]);
    module.prune_members(&table);
    assert_eq!(module.roots[&root(10)].members, BTreeSet::from([(2, 1)]));
    let complete = ProcessTable::from_test_members(2, 2, Vec::new(), Vec::new());
    module.prune_members(&complete);
    assert!(module.roots[&root(10)].members.is_empty());
}
