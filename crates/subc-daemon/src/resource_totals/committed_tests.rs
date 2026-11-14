use super::*;

fn launch() -> crate::supervise::ResourceLaunch {
    crate::supervise::ResourceLaunch {
        pid: Some(1),
        start: Some(1),
        spawn_generation: 1,
        enabled: true,
        alive: true,
        target: crate::subtree_resources::Target::Unavailable("no_container"),
    }
}

fn table(revision: u64, cpu: u64) -> Arc<ProcessTable> {
    Arc::new(ProcessTable::from_test_members(
        revision,
        revision * 60_000,
        vec![subc_os::ProcessMember {
            pid: 1,
            parent_pid: 0,
            session_id: None,
            start_time: 1,
            start_time_ms: 1,
            name: "worker".into(),
            resources: Some(subc_os::ResourceUsage {
                memory_bytes: 20,
                memory_kind: subc_os::MemoryKind::ResidentSet,
                swap_bytes: None,
                cpu_user: Duration::from_millis(cpu),
                cpu_system: Duration::ZERO,
            }),
            own_cpu_ms: Some(cpu),
            reaped_cpu_ms: Some(0),
            #[cfg(windows)]
            process: Arc::new(subc_os::Process::open(std::process::id()).unwrap().unwrap()),
        }],
        Vec::new(),
    ))
}

#[test]
fn committed_view_preserves_measured_tree_and_hour_after_failed_walk() {
    let tracker = Tracker::default();
    let launches = BTreeMap::from([("worker".into(), Some(launch()))]);
    for (revision, cpu) in [(1, 10), (2, 110)] {
        let read = tracker.read_observed(
            launches.clone(),
            table(revision, cpu),
            true,
            revision * 60_000,
            Instant::now() + Duration::from_secs(1),
        );
        assert!(read.modules["worker"].0.is_some());
    }
    let failed = tracker.read_observed(
        launches.clone(),
        Arc::new(ProcessTable::default()),
        false,
        180_000,
        Instant::now() + Duration::from_secs(1),
    );
    assert!(
        failed.modules["worker"].0.is_none(),
        "the sampler must not sample a stale tree as fresh"
    );
    let committed = tracker.committed_view(&launches, 180_000);
    let (tree, hour) = committed
        .modules
        .get("worker")
        .expect("the last completed reading must survive a failed walk");
    assert_eq!(tree.as_ref().unwrap().cpu_ms, 110);
    assert_eq!(tree.as_ref().unwrap().measured_at_ms, Some(120_000));
    assert_eq!(committed.reasons["worker"], "unreadable");
    assert_eq!(
        (
            hour.as_ref().unwrap().cpu_ms,
            hour.as_ref().unwrap().covered_ms
        ),
        (100, 60_000)
    );
}

fn seeded() -> (
    Arc<Tracker>,
    BTreeMap<String, Option<crate::supervise::ResourceLaunch>>,
) {
    let tracker = Arc::new(Tracker::default());
    let launches = BTreeMap::from([("worker".into(), Some(launch()))]);
    tracker.read_observed(
        launches.clone(),
        table(1, 10),
        true,
        60_000,
        Instant::now() + Duration::from_secs(1),
    );
    (tracker, launches)
}

#[tokio::test]
async fn list_with_expired_diagnostics_budget_returns_committed_resources() {
    let (tracker, launches) = seeded();
    crate::control::list_diagnostics::scope(async {
        tokio::time::sleep(Duration::from_millis(110)).await;
        assert!(
            crate::control::list_diagnostics::deadline().unwrap() <= tokio::time::Instant::now()
        );
        let cached = tracker.list_for_launches(Vec::new(), &launches);
        assert_eq!(
            cached.modules["worker"].0.as_ref().unwrap().measured_at_ms,
            Some(60_000)
        );
        assert!(cached.daemon.is_some());
        assert!(
            !tracker.view_refreshing.load(Ordering::Acquire),
            "a fresh committed view needs no walk"
        );
        assert_eq!(tracker.view_refreshes.load(Ordering::Relaxed), 0);
    })
    .await;
}

#[tokio::test]
async fn list_during_refresh_returns_previous_view_without_waiting_or_another_refresh() {
    let (tracker, launches) = seeded();
    tracker.committed.lock().unwrap().checked_at = Some(Instant::now() - Duration::from_secs(3));
    let guard = tracker.walk.lock().await;
    let first = tracker.list_for_launches(Vec::new(), &launches);
    assert!(tracker.view_refreshing.load(Ordering::Acquire));
    assert_eq!(tracker.view_refreshes.load(Ordering::Relaxed), 1);
    tokio::task::yield_now().await;
    let second = tracker.list_for_launches(Vec::new(), &launches);
    assert_eq!(first.modules["worker"].0, second.modules["worker"].0);
    assert_eq!(second.modules["worker"].0.as_ref().unwrap().cpu_ms, 10);
    assert!(tracker.view_refreshing.load(Ordering::Acquire));
    assert_eq!(
        tracker.view_refreshes.load(Ordering::Relaxed),
        1,
        "a second list must not enqueue another refresh behind the first"
    );
    assert!(
        tracker.walk.try_lock().is_err(),
        "the blocked refresh cannot bypass walk serialization"
    );
    drop(guard);
}

#[test]
fn a_new_launch_never_inherits_the_previous_launch_tree_or_agents() {
    let (tracker, mut launches) = seeded();
    launches
        .get_mut("worker")
        .unwrap()
        .as_mut()
        .unwrap()
        .spawn_generation += 1;
    let view = tracker.committed_view(&launches, 60_000);
    assert!(view.modules["worker"].0.is_none());
    assert_eq!(view.reasons["worker"], "not_yet_measured");
    assert!(view.agent_commands.unwrap().is_empty());
}

#[test]
fn stale_launch_walk_keeps_newer_numbers_and_reports_launch_changed() {
    let (tracker, old_launches) = seeded();
    let mut new_launches = old_launches.clone();
    new_launches
        .get_mut("worker")
        .unwrap()
        .as_mut()
        .unwrap()
        .spawn_generation += 1;
    tracker.read_observed(
        new_launches.clone(),
        table(3, 110),
        false,
        180_000,
        Instant::now() + Duration::from_secs(1),
    );
    tracker.read_observed(
        old_launches,
        table(2, 50),
        false,
        120_000,
        Instant::now() + Duration::from_secs(1),
    );
    let view = tracker.committed_view(&new_launches, 180_000);
    assert_eq!(
        view.modules["worker"].0.as_ref().unwrap().measured_at_ms,
        Some(180_000)
    );
    assert_eq!(view.reasons["worker"], "launch_changed");
}

#[test]
fn failed_first_module_walk_and_state_contention_never_fabricate_zero_readings() {
    let tracker = Tracker::default();
    let launches = BTreeMap::from([("worker".into(), Some(launch()))]);
    tracker.read_observed(
        launches.clone(),
        Arc::new(ProcessTable::default()),
        false,
        60_000,
        Instant::now() + Duration::from_secs(1),
    );
    let _state = tracker.state.lock().unwrap();
    let view = tracker.committed_view(&launches, 60_000);
    assert_eq!(view.modules["worker"], (None, None));
    assert_eq!(view.reasons["worker"], "not_yet_measured");
    assert!(
        matches!(
            view.daemon.unwrap().resources,
            Some(subc_control::ChildResourceUsage::Measured(_))
        ),
        "a separate successful daemon measurement remains valid"
    );
}

#[tokio::test]
async fn incomplete_pass_changes_neither_committed_view_reason_nor_time() {
    let (tracker, launches) = seeded();
    tracker.read_observed(
        launches.clone(),
        Arc::new(ProcessTable::default()),
        false,
        120_000,
        Instant::now() + Duration::from_secs(1),
    );
    let before = tracker.list_for_launches(Vec::new(), &launches);
    let checked_at = tracker.committed.lock().unwrap().checked_at;
    let incomplete = tracker.read_observed(
        launches.clone(),
        table(3, 999),
        false,
        180_000,
        Instant::now() - Duration::from_secs(1),
    );
    assert!(incomplete.modules.is_empty());
    let contended = tracker.read_observed(
        BTreeMap::from([("worker".into(), None)]),
        table(4, 1000),
        false,
        240_000,
        Instant::now() + Duration::from_secs(1),
    );
    assert!(contended.modules.is_empty());
    let after = tracker.list_for_launches(Vec::new(), &launches);
    assert_eq!(before.modules, after.modules);
    assert_eq!(before.daemon, after.daemon);
    assert_eq!(before.agent_commands, after.agent_commands);
    assert_eq!(before.reasons, after.reasons);
    assert_eq!(after.reasons["worker"], "unreadable");
    assert_eq!(
        after.modules["worker"].0.as_ref().unwrap().measured_at_ms,
        Some(60_000)
    );
    assert_eq!(tracker.committed.lock().unwrap().checked_at, checked_at);
}

#[tokio::test]
async fn minute_sample_uses_its_own_walk_instead_of_a_recent_cached_table() {
    let _readers = crate::subtree_resources::fault_read_guard().await;
    let tracker = Arc::new(Tracker::default());
    let cached = table(1, 10);
    *tracker.cache.lock().unwrap() = Some((Instant::now(), Arc::clone(&cached)));
    let reads = tracker.inject_sampler_tables_for_test([(*table(2, 20)).clone()]);
    let readings = tracker.read(Vec::new(), true, "minute_fixture").await;
    let cache = tracker.cache.lock().unwrap();
    assert!(!Arc::ptr_eq(&cached, &cache.as_ref().unwrap().1));
    assert_eq!(cache.as_ref().unwrap().1.revision(), 2);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert!(readings.daemon.as_ref().unwrap().measured_at_ms.is_some());
}

#[test]
fn an_unaccepted_background_result_cannot_change_committed_state_or_history() {
    let (tracker, launches) = seeded();
    let before = tracker.committed_view(&launches, 180_000);
    let discarded = tracker.read_observed_before(
        launches.clone(),
        table(3, 999),
        true,
        180_000,
        Instant::now() + Duration::from_secs(1),
        BTreeMap::new(),
    );
    drop(discarded);
    let after = tracker.committed_view(&launches, 180_000);
    assert_eq!(
        before.modules, after.modules,
        "a timed-out caller must not publish a late worker's result or its hour sample"
    );
    assert_eq!(before.daemon, after.daemon);
}

#[test]
fn failed_measurement_preserves_daemon_and_agent_times_and_new_launch_drops_agents() {
    let (tracker, mut launches) = seeded();
    let mut previous = Readings::default();
    previous.modules.insert("worker".into(), (None, None));
    previous.reasons.insert("worker".into(), "unreadable");
    previous.agent_commands = Some(vec![
        AgentCommandResources::new("worker", 50, 30, 1).with_measured_at_ms(Some(40_000))
    ]);
    previous.daemon = Some(
        DaemonResources::new(7, 1, "test")
            .with_resources(Some(subc_control::ChildResourceUsage::Measured(
                subc_control::ChildResourceReading::new(
                    20,
                    subc_control::ChildMemoryKind::ResidentSet,
                    10,
                    0,
                ),
            )))
            .with_measured_at_ms(Some(30_000)),
    );
    let generations = BTreeMap::from([("worker".into(), launch().generation())]);
    tracker
        .committed
        .lock()
        .unwrap()
        .publish(&previous, generations.clone());
    let failed = Readings {
        modules: BTreeMap::from([("worker".into(), (None, None))]),
        reasons: BTreeMap::from([("worker".into(), "unreadable")]),
        ..Readings::default()
    };
    tracker
        .committed
        .lock()
        .unwrap()
        .publish(&failed, generations);
    let view = tracker.committed_view(&launches, 60_000);
    assert_eq!(view.daemon.unwrap().measured_at_ms, Some(30_000));
    let agents = view.agent_commands.unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(
        (agents[0].cpu_ms, agents[0].measured_at_ms),
        (50, Some(40_000))
    );
    launches
        .get_mut("worker")
        .unwrap()
        .as_mut()
        .unwrap()
        .spawn_generation += 1;
    assert!(tracker
        .committed_view(&launches, 60_000)
        .agent_commands
        .unwrap()
        .is_empty());
}
