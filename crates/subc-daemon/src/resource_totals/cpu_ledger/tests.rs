use super::*;

fn member(pid: u32, parent: Option<u32>, own_ms: u64, reaped_ms: u64) -> Member {
    Member {
        new_since_baseline: true,
        identity: (pid, 1),
        parent: parent.map(|pid| (pid, 1)),
        own_ms,
        reaped_ms,
    }
}

fn observe(ledger: &mut Ledger, revision: u64, members: &[Member], alive: &[u32]) {
    ledger.observe(
        revision,
        members.iter().copied(),
        &alive.iter().map(|pid| (*pid, 1)).collect(),
    );
}

#[test]
fn leaving_ninety_ms_never_masks_the_next_five_ms() {
    let mut scope = Ledger::new(false);
    let mut command = Ledger::new(true);
    let mut allocation = Allocation::default();
    let mut minute_rows = Vec::new();
    for (revision, members, alive) in [
        (
            1,
            vec![member(1, None, 10, 0), member(2, Some(1), 0, 0)],
            vec![1, 2],
        ),
        (
            2,
            vec![member(1, None, 10, 0), member(2, Some(1), 90, 0)],
            vec![1, 2],
        ),
        (3, vec![member(1, None, 10, 0)], vec![1, 2]),
        (4, vec![member(1, None, 15, 0)], vec![1, 2]),
    ] {
        let alive: BTreeSet<_> = alive.into_iter().map(|pid| (pid, 1)).collect();
        scope.observe(revision, members.iter().copied(), &alive);
        let own = if revision == 1 { 0 } else { 90 };
        let increments = command.observe(revision, [member(2, Some(1), own, 0)], &alive);
        allocation.observe(scope.total(), increments.values().sum(), 0);
        minute_rows.push(allocation.rows());
    }
    assert_eq!(minute_rows[1], (10, 90, 100));
    assert_eq!(minute_rows[2], (10, 90, 100));
    assert_eq!(minute_rows[3], (15, 90, 105));
}

#[test]
fn late_declaration_starts_at_a_baseline_and_creates_no_hourly_cpu() {
    let mut scope = Ledger::new(false);
    let mut command = Ledger::new(true);
    let mut allocation = Allocation::default();
    let mut module_hour = crate::resource_history::History::default();
    let mut agents_hour = crate::resource_history::History::default();
    let mut total_hour = crate::resource_history::History::default();
    for revision in 1..=3 {
        let alive = BTreeSet::from([(1, 1), (2, 1)]);
        scope.observe(
            revision,
            [member(1, None, 10, 0), member(2, Some(1), 90, 0)],
            &alive,
        );
        let inside = if revision == 3 {
            command
                .observe(revision, [member(2, Some(1), 90, 0)], &alive)
                .values()
                .sum()
        } else {
            0
        };
        allocation.observe(scope.total(), inside, 0);
        let (module, agents, total) = allocation.rows();
        module_hour.record(revision * 60_000, 0, module);
        agents_hour.record(revision * 60_000, 0, agents);
        total_hour.record(revision * 60_000, 0, total);
    }
    assert_eq!(allocation.rows(), (100, 0, 100));
    let module = module_hour.last_hour_at(180_000).unwrap();
    let agents = agents_hour.last_hour_at(180_000).unwrap();
    let total = total_hour.last_hour_at(180_000).unwrap();
    assert_eq!(module.cpu_ms + agents.cpu_ms, total.cpu_ms);
    assert_eq!(total.cpu_ms, 0);
}

#[test]
fn init_reap_and_unseen_child_histories_share_a_provable_floor() {
    for true_cpu in [135, 225] {
        let mut ledger = Ledger::new(false);
        observe(
            &mut ledger,
            1,
            &[
                member(1, None, 10, 0),
                member(2, Some(1), 20, 0),
                member(3, Some(2), 90, 0),
            ],
            &[1, 2, 3],
        );
        observe(&mut ledger, 2, &[member(1, None, 15, 120)], &[1]);
        assert_eq!(ledger.total(), 135);
        assert!(ledger.total() <= true_cpu);
        assert_eq!(ledger.members.len(), 1);
        observe(&mut ledger, 3, &[member(1, None, 20, 120)], &[1]);
        assert_eq!(ledger.total(), 140);
        assert!(ledger.total() <= true_cpu + 5);
    }
}

#[test]
fn confirmed_live_orphans_cannot_be_absorbed_as_reaped_cpu() {
    let mut ledger = Ledger::new(false);
    observe(
        &mut ledger,
        1,
        &[
            member(1, None, 10, 0),
            member(2, Some(1), 20, 0),
            member(3, Some(2), 90, 0),
        ],
        &[1, 2, 3],
    );
    observe(&mut ledger, 2, &[member(1, None, 15, 120)], &[1, 3]);
    assert_eq!(ledger.total(), 225);
    assert_eq!(ledger.outside.len(), 1);
    observe(&mut ledger, 3, &[member(1, None, 20, 120)], &[1]);
    assert_eq!(ledger.total(), 230);
    assert!(ledger.outside.is_empty());
}

#[test]
fn inside_debit_skew_retains_the_pending_increment() {
    let mut allocation = Allocation::default();
    allocation.observe(0, 0, 0);
    allocation.observe(100, 150, 0);
    assert_eq!(allocation.rows(), (0, 100, 100));
    assert_eq!(allocation.pending, 50);
    allocation.observe(150, 0, 0);
    assert_eq!(allocation.rows(), (0, 150, 150));
    assert_eq!(allocation.pending, 0);
}

#[path = "property.rs"]
pub(crate) mod property;

#[test]
fn overlapping_walks_cannot_lower_an_identity_baseline_and_count_its_recovery_twice() {
    let mut ledger = Ledger::new(false);
    let alive = BTreeSet::from([(1, 1)]);
    ledger.observe(1, [member(1, None, 10, 20)], &alive);
    ledger.observe(2, [member(1, None, 9, 18)], &alive);
    assert_eq!(ledger.total(), 30);
    ledger.observe(3, [member(1, None, 11, 21)], &alive);
    assert_eq!(
        ledger.total(),
        32,
        "only one new own and one new reaped millisecond were observed"
    );
}
