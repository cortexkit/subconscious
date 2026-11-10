use super::*;

struct Fixture {
    tracker: Tracker,
    module: Module,
    root: AgentCommandRoot,
    native_start: u64,
    module_pid: u32,
}

impl Fixture {
    fn new() -> Self {
        let pid = std::process::id();
        let clock = Clock::manual(Duration::ZERO);
        Self {
            tracker: Tracker {
                clock: clock.clone(),
                ..Tracker::default()
            },
            module: Module {
                clock,
                ..Module::default()
            },
            root: AgentCommandRoot {
                pid,
                start_time_ms: subc_os::process_start_time_ms(pid).unwrap(),
            },
            native_start: subc_os::start_time(pid).unwrap(),
            module_pid: subc_os::parent_pid(pid).unwrap(),
        }
    }

    fn target(&self) -> crate::subtree_resources::Target {
        crate::subtree_resources::Target::Unavailable("no_container")
    }

    fn table(&self, revision: u64, cpu: Option<u64>, incomplete: bool) -> ProcessTable {
        let member = |pid, parent_pid, start_time, start_time_ms, cpu| subc_os::ProcessMember {
            pid,
            parent_pid,
            start_time,
            start_time_ms,
            session_id: None,
            name: "worker".into(),
            resources: Some(subc_os::ResourceUsage {
                cpu_user: Duration::from_millis(cpu),
                cpu_system: Duration::ZERO,
                memory_bytes: 10,
                memory_kind: subc_os::MemoryKind::ResidentSet,
                swap_bytes: None,
            }),
            own_cpu_ms: Some(cpu),
            reaped_cpu_ms: Some(0),
            #[cfg(windows)]
            process: Arc::new(subc_os::Process::open(std::process::id()).unwrap().unwrap()),
        };
        let mut members = vec![member(self.module_pid, 0, 1, 1, 20)];
        if let Some(cpu) = cpu {
            members.push(member(
                self.root.pid,
                self.module_pid,
                self.native_start,
                self.root.start_time_ms,
                cpu,
            ));
        }
        ProcessTable::from_test_members(
            revision,
            self.root.start_time_ms + revision * 1_000,
            members,
            if incomplete {
                vec![(u32::MAX, Some(self.root.pid), None)]
            } else {
                Vec::new()
            },
        )
    }

    fn declare(&mut self, root: AgentCommandRoot, table: ProcessTable) -> Application {
        self.apply(
            Declaration {
                started: vec![root],
                ..Declaration::default()
            },
            table,
        )
    }

    fn apply(&mut self, batch: Declaration, table: ProcessTable) -> Application {
        let revision = table.revision();
        let prepared = validation::prepare_snapshot(
            self.module_pid,
            self.target(),
            batch,
            &BTreeMap::new(),
            validation::Snapshot {
                before_cpu: None,
                table,
                #[cfg(unix)]
                scope: None,
            },
            revision,
        )
        .unwrap();
        self.tracker.apply_prepared(&mut self.module, 1, prepared)
    }

    fn tick(
        &mut self,
        table: &ProcessTable,
    ) -> Option<(Option<TreeReading>, AgentCommandResources)> {
        self.module.validate_pending(
            "test-module",
            1,
            Some(&self.target()),
            table,
            table.observed_at_ms(),
        );
        // Windows normally obtains these paired counters in observe_windows_commands.
        // Feed the exact injected counters, rather than reading this test's real CPU.
        #[cfg(windows)]
        for command in self.module.roots.values_mut() {
            command.pinned_cpu = table
                .members()
                .filter(|member| command.members.contains(&(member.pid, member.start_time)))
                .map(|member| ((member.pid, member.start_time), member.own_cpu_ms))
                .collect();
        }
        let scope = table.tree(self.module_pid, 1, false).map(|usage| {
            let members = usage
                .members
                .iter()
                .map(|member| (member.pid, member.start_time))
                .collect();
            (wire(usage, TreeSource::ProcessTree), members)
        });
        self.module
            .prospective(1, scope, table, table.observed_at_ms(), 0)
    }
}

#[test]
fn incomplete_start_is_accepted_later_and_only_credits_post_acceptance_cpu() {
    let mut fixture = Fixture::new();
    let incomplete = fixture.table(1, Some(10), true);
    let applied = fixture.declare(fixture.root, incomplete);
    assert!(applied.rejected.is_empty());
    assert!(!applied.roots_full);
    assert!(fixture.module.pending.contains_key(&fixture.root));
    assert!(fixture.module.roots.is_empty());
    let accepted = fixture.table(2, Some(100), false);
    fixture.tick(&accepted).unwrap();
    assert!(fixture.module.pending.is_empty());
    assert!(fixture.module.roots.contains_key(&fixture.root));
    assert_eq!(
        fixture.module.cpu_model.rows(),
        (120, 0, 120),
        "CPU before acceptance remains in the module row"
    );
    let later = fixture.table(3, Some(125), false);
    fixture.tick(&later).unwrap();
    assert_eq!(fixture.module.cpu_model.rows(), (120, 25, 145));
}

#[test]
fn invalid_start_is_rejected_instead_of_pending() {
    let mut fixture = Fixture::new();
    let invalid = AgentCommandRoot {
        start_time_ms: fixture.root.start_time_ms + subc_os::process_start_time_precision_ms(),
        ..fixture.root
    };
    let table = fixture.table(1, Some(10), false);
    let applied = fixture.declare(invalid, table);
    assert_eq!(applied.rejected, vec![invalid]);
    assert!(!applied.roots_full);
    assert!(fixture.module.pending.is_empty());
    assert!(fixture.module.roots.is_empty());
}

#[test]
fn sampler_tick_accepts_a_pending_start_without_another_declaration() {
    let mut fixture = Fixture::new();
    let incomplete = fixture.table(1, Some(10), true);
    fixture.declare(fixture.root, incomplete);
    fixture
        .tracker
        .state
        .lock()
        .unwrap()
        .modules
        .insert("test-module".into(), fixture.module.clone());
    let launch = crate::supervise::ResourceLaunch {
        pid: Some(fixture.module_pid),
        start: Some(1),
        spawn_generation: 0,
        enabled: true,
        alive: true,
        target: crate::subtree_resources::Target::Unavailable("no_container"),
    };
    let table = Arc::new(fixture.table(2, Some(100), false));
    fixture.tracker.read_observed(
        BTreeMap::from([("test-module".into(), Some(launch))]),
        Arc::clone(&table),
        false,
        table.observed_at_ms(),
        read_deadline(),
    );
    let state = fixture.tracker.state.lock().unwrap();
    let module = &state.modules["test-module"];
    assert!(module.pending.is_empty());
    assert_eq!(module.roots[&fixture.root].acceptance_cpu, Some(100));
    assert_eq!(module.cpu_model.rows(), (120, 0, 120));
}

#[test]
fn pending_start_expires_at_sixty_seconds_without_following_wall_time() {
    let mut fixture = Fixture::new();
    let incomplete = fixture.table(1, Some(10), true);
    fixture.declare(fixture.root, incomplete);
    fixture.module.clock.set(Duration::from_millis(59_999));
    fixture.tick(&fixture.table(2, Some(10), true));
    assert!(fixture.module.pending.contains_key(&fixture.root));
    let repeated = fixture.table(3, Some(10), true);
    fixture.declare(fixture.root, repeated);
    fixture.module.clock.set(Duration::from_secs(60));
    fixture.tick(&fixture.table(4, Some(10), true));
    assert!(fixture.module.pending.is_empty());
    assert!(fixture.module.roots.is_empty());
    assert_eq!(fixture.module.cpu_model.rows().1, 0);
}

#[test]
fn pending_start_uses_root_capacity_until_validated() {
    let mut fixture = Fixture::new();
    let incomplete = fixture.table(1, Some(10), true);
    fixture.declare(fixture.root, incomplete);
    for pid in 1..ROOT_LIMIT as u32 {
        fixture.module.finished_live.insert(
            AgentCommandRoot {
                pid,
                start_time_ms: 0,
            },
            (0, Instant::now()),
        );
    }
    let extra = AgentCommandRoot {
        pid: u32::MAX,
        start_time_ms: 1,
    };
    let table = fixture.table(2, Some(10), false);
    let candidate = VerifiedRoot {
        root: extra,
        native_start: 1,
        inside: true,
        members: BTreeSet::new(),
        #[cfg(windows)]
        process: None,
    };
    let refused = fixture
        .module
        .accept_checked(1, extra, Some(candidate), &table, 0);
    assert!(refused.full && refused.log_rejected);
    let refused = fixture.module.pend(1, extra, &table, 0);
    assert!(refused.full);
    assert_eq!(fixture.module.pending.len(), 1);
    fixture
        .module
        .validate_pending("test-module", 1, Some(&fixture.target()), &table, 0);
    assert!(fixture.module.pending.is_empty());
    assert!(
        fixture.module.roots.contains_key(&fixture.root),
        "validation uses its reserved slot even at capacity"
    );
}

#[test]
fn pending_start_that_exits_is_dropped_without_credit() {
    let mut fixture = Fixture::new();
    let incomplete = fixture.table(1, Some(10), true);
    fixture.declare(fixture.root, incomplete);
    fixture.tick(&fixture.table(2, None, false));
    assert!(fixture.module.pending.is_empty());
    assert!(fixture.module.roots.is_empty());
    assert!(
        fixture.module.rejected.is_empty(),
        "an exited pending root is not an invalid declaration"
    );
    assert_eq!(fixture.module.cpu_model.rows(), (20, 0, 20));
}

#[test]
fn a_walk_started_before_declaration_cannot_validate_pending_roots() {
    let mut fixture = Fixture::new();
    let earlier = fixture.table(2, Some(10), false);
    let incomplete = fixture.table(1, Some(10), true);
    fixture.declare(fixture.root, incomplete);
    fixture.tick(&earlier);
    assert!(fixture.module.pending.contains_key(&fixture.root));
    assert!(fixture.module.roots.is_empty());
}

#[test]
fn a_known_pending_start_is_retained_but_finish_or_omission_withdraws_it() {
    for finish in [false, true] {
        let mut fixture = Fixture::new();
        let incomplete = fixture.table(1, Some(10), true);
        fixture.declare(fixture.root, incomplete);
        let known = fixture.table(2, Some(10), true);
        let applied = fixture.apply(
            Declaration {
                known: Some(vec![fixture.root]),
                ..Declaration::default()
            },
            known,
        );
        assert!(applied.rejected.is_empty());
        assert!(fixture.module.pending.contains_key(&fixture.root));
        let withdrawn = if finish {
            Declaration {
                finished: vec![AgentCommandFinished {
                    pid: fixture.root.pid,
                    start_time_ms: fixture.root.start_time_ms,
                    cpu_ms: Some(1_000),
                }],
                ..Declaration::default()
            }
        } else {
            Declaration {
                known: Some(Vec::new()),
                ..Declaration::default()
            }
        };
        let incomplete = fixture.table(3, Some(10), true);
        let applied = fixture.apply(withdrawn, incomplete);
        assert!(applied.rejected.is_empty());
        assert!(fixture.module.pending.is_empty());
        fixture.tick(&fixture.table(4, Some(100), false)).unwrap();
        assert!(fixture.module.roots.is_empty());
        assert_eq!(fixture.module.cpu_model.rows().1, 0);
    }
}

#[test]
fn pending_validation_uses_the_current_launch_membership_not_the_declaration_target() {
    let mut fixture = Fixture::new();
    let incomplete = fixture.table(1, Some(10), true);
    fixture.declare(fixture.root, incomplete);
    let complete = fixture.table(2, Some(100), false);
    fixture.module.validate_pending(
        "test-module",
        1,
        Some(&crate::subtree_resources::Target::Unavailable(
            "read_failed",
        )),
        &complete,
        0,
    );
    assert!(fixture.module.pending.contains_key(&fixture.root));
    assert!(fixture.module.roots.is_empty());
    fixture.tick(&fixture.table(3, Some(125), false)).unwrap();
    assert!(fixture.module.pending.is_empty());
    assert_eq!(
        fixture.module.roots[&fixture.root].acceptance_cpu,
        Some(125)
    );
    assert_eq!(fixture.module.cpu_model.rows(), (145, 0, 145));
}

#[test]
fn metadata_only_pending_requires_a_fresh_walk_above_the_sequence_floor() {
    let mut fixture = Fixture::new();
    fixture.module.table_revision = 10;
    let earlier = fixture.table(11, Some(10), false);
    let arrival = fixture.tracker.declaration_arrival();
    fixture
        .module
        .pend_at(1, fixture.module_pid, fixture.root, arrival, 0);
    assert_eq!(fixture.module.pending[&fixture.root].sequence, 10);
    fixture.tick(&earlier);
    assert!(
        fixture.module.roots.is_empty(),
        "a pre-arrival walk is not proof"
    );
    fixture.tick(&fixture.table(10, Some(50), false));
    assert!(
        fixture.module.roots.is_empty(),
        "the sequence floor is exclusive"
    );
    fixture.tick(&fixture.table(12, Some(100), false)).unwrap();
    assert!(fixture.module.pending.is_empty());
    assert_eq!(fixture.module.cpu_model.rows(), (120, 0, 120));
    fixture.tick(&fixture.table(13, Some(125), false)).unwrap();
    assert_eq!(fixture.module.cpu_model.rows(), (120, 25, 145));
}

#[test]
fn metadata_only_pending_retries_preserve_arrival_and_sixty_second_expiry() {
    let mut fixture = Fixture::new();
    let arrival = fixture.tracker.declaration_arrival();
    fixture.module.clock.set(Duration::from_secs(30));
    fixture
        .module
        .pend_at(1, fixture.module_pid, fixture.root, arrival, 0);
    fixture.module.clock.set(Duration::from_millis(59_999));
    fixture.module.pend_at(
        1,
        fixture.module_pid,
        fixture.root,
        fixture.tracker.declaration_arrival(),
        0,
    );
    assert_eq!(fixture.module.pending.len(), 1);
    assert_eq!(
        fixture.module.pending[&fixture.root].queued_at,
        Duration::ZERO
    );
    assert_eq!(
        fixture.module.pending[&fixture.root].declared_at,
        arrival.declared_at
    );
    fixture.module.clock.set(Duration::from_secs(60));
    fixture.tick(&fixture.table(1, Some(10), false));
    assert!(fixture.module.pending.is_empty());
    assert!(fixture.module.roots.is_empty());
}

#[test]
fn metadata_only_retention_dedupes_and_cannot_commit_after_cancel_or_at_capacity() {
    let fixture = Fixture::new();
    let tracker = &fixture.tracker;
    let arrival = tracker.declaration_arrival();
    let cancelled = CommitFence::default();
    assert!(cancelled.cancel());
    assert!(tracker
        .retain_started(
            "test-module",
            1,
            fixture.module_pid,
            &[fixture.root],
            arrival,
            &cancelled
        )
        .is_none());
    assert!(tracker.state.lock().unwrap().modules.is_empty());
    for _ in 0..2 {
        assert!(
            !tracker
                .retain_started(
                    "test-module",
                    1,
                    fixture.module_pid,
                    &[fixture.root, fixture.root],
                    arrival,
                    &CommitFence::default()
                )
                .unwrap()
                .roots_full
        );
    }
    assert_eq!(tracker.root_counts_for_test("test-module"), (0, 1, 0));
    tracker.fill_root_capacity_for_test("test-module");
    let extra = AgentCommandRoot {
        pid: u32::MAX,
        start_time_ms: 1,
    };
    assert!(
        tracker
            .retain_started(
                "test-module",
                1,
                fixture.module_pid,
                &[extra],
                arrival,
                &CommitFence::default()
            )
            .unwrap()
            .roots_full
    );
    assert_eq!(tracker.root_counts_for_test("test-module").1, 1);
}

#[tokio::test]
async fn a_retention_wake_replaces_a_predeclaration_cached_walk_and_validates_the_start() {
    let fixture = Fixture::new();
    let tracker = &fixture.tracker;
    let cached = Arc::new(fixture.table(10, Some(10), false));
    *tracker.cache.lock().unwrap() = Some((Instant::now(), Arc::clone(&cached)));
    let arrival = tracker.declaration_arrival();
    tracker
        .retain_started(
            "test-module",
            1,
            fixture.module_pid,
            &[fixture.root],
            arrival,
            &CommitFence::default(),
        )
        .unwrap();
    let reads = tracker.inject_sampler_tables_for_test([fixture.table(11, Some(100), false)]);
    let fresh = tracker.table().await.unwrap();
    assert!(
        !Arc::ptr_eq(&cached, &fresh),
        "the retention wake must not reuse a pre-declaration walk"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    let mut state = tracker.state.lock().unwrap();
    let module = state.modules.get_mut("test-module").unwrap();
    module.validate_pending("test-module", 1, Some(&fixture.target()), &fresh, 0);
    assert!(module.pending.is_empty());
    assert!(module.roots.contains_key(&fixture.root));
    assert_eq!(module.roots[&fixture.root].acceptance_cpu, Some(100));
}

#[tokio::test(start_paused = true)]
async fn idle_sampler_has_no_retry_reads_before_the_next_minute() {
    let tracker = Arc::new(Tracker::default());
    let mut minute = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(60),
        Duration::from_secs(60),
    );
    let next = tokio::spawn(async move { tracker.next_sample(&mut minute).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(59)).await;
    assert!(
        !next.is_finished(),
        "an idle daemon must not request retry walks"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(
        next.await.unwrap(),
        "the first read is the regular minute sample"
    );
}

#[tokio::test(start_paused = true)]
async fn retained_starts_request_another_read_after_five_seconds() {
    let fixture = Fixture::new();
    let tracker = Arc::new(fixture.tracker);
    tracker
        .retain_started(
            "test-module",
            1,
            fixture.module_pid,
            &[fixture.root],
            tracker.declaration_arrival(),
            &CommitFence::default(),
        )
        .unwrap();
    tracker.wake.notified().await;
    let mut minute = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(60),
        Duration::from_secs(60),
    );
    let next = tokio::spawn(async move { tracker.next_sample(&mut minute).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(!next.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(
        !next.await.unwrap(),
        "pending roots request a non-minute read"
    );
}

#[test]
fn metadata_only_pending_that_never_becomes_readable_still_expires_at_sixty_seconds() {
    let mut fixture = Fixture::new();
    fixture.module.pend_at(
        1,
        fixture.module_pid,
        fixture.root,
        fixture.tracker.declaration_arrival(),
        0,
    );
    for seconds in (5..60).step_by(5) {
        fixture.module.clock.set(Duration::from_secs(seconds));
        fixture.tick(&fixture.table(seconds, Some(100), true));
        assert_eq!(fixture.module.pending.len(), 1);
    }
    fixture.module.clock.set(Duration::from_secs(60));
    fixture.tick(&fixture.table(60, Some(100), true));
    assert!(fixture.module.pending.is_empty());
    assert!(fixture.module.roots.is_empty());
}
