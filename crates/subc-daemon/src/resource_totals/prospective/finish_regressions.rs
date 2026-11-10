use super::*;

#[cfg(target_os = "linux")]
#[test]
fn a_tolerated_start_time_alias_cannot_retire_a_successfully_resumed_root() {
    let mut world = World::new();
    let pid = world.spawn(1);
    let born = BTreeMap::from([(1, 0), (pid, 1)]);
    let mut module = module();
    let root = AgentCommandRoot {
        pid,
        start_time_ms: 1,
    };
    module.omitted.insert(
        root,
        OmittedRoot {
            native_start: 1,
            generation: 1,
            counter: Some(1),
            sequence: 0,
            accepted_at: Instant::now(),
        },
    );
    let alias = AgentCommandRoot {
        pid,
        start_time_ms: 1 + subc_os::process_start_time_precision_ms() - 1,
    };
    assert_ne!(alias, root);
    let snapshot = table(&world, &born, 1, None);
    let verified = VerifiedRoot {
        root,
        native_start: 1,
        inside: true,
        members: BTreeSet::from([(pid, 1)]),
    };
    let tracker = Tracker::default();
    let result = tracker.apply_prepared(
        &mut module,
        1,
        Prepared {
            scope_target: crate::subtree_resources::Target::Unavailable("no_container"),
            module_pid: 1,
            before_cpu: None,
            started: Vec::new(),
            pending: Vec::new(),
            finished: Vec::new(),
            known: Some(vec![(alias, Some(verified))]),
            table: snapshot,
            at_ms: 60_000,
            finish_until: u64::MAX,
            scope: None,
        },
    );
    assert!(result.full_resync);
    assert!(module.roots.contains_key(&root));
    assert!(!module.omitted.contains_key(&root));
}

#[test]
fn an_exact_container_cannot_hide_a_process_tree_interval_still_in_the_hour() {
    let world = World::new();
    let born = BTreeMap::from([(1, 0)]);
    let mut module = module();
    module.declared_generation = Some(1);
    let tree = table(&world, &born, 1, None);
    module
        .prospective(1, scope(&tree), &tree, 60_000, 0)
        .unwrap();
    module.record_sample(0, Some(0), Some(0));
    module.record_sample(60_000, Some(0), Some(0));
    module.clock.set(Duration::from_secs(10));
    let container = table(&world, &born, 2, None);
    let (_, agents) = module
        .prospective(
            1,
            Some(cgroup_scope(&world, &container, 0)),
            &container,
            120_000,
            0,
        )
        .unwrap();
    assert_eq!(agents.bound, Some(subc_control::ResourceBound::Lower));
    assert_eq!(
        module.agents_last_hour(60_000).unwrap().bound,
        Some(subc_control::ResourceBound::Lower)
    );
    module.clock.set(Duration::from_millis(3_599_999));
    assert_eq!(
        module.agents_last_hour(60_000).unwrap().bound,
        Some(subc_control::ResourceBound::Lower)
    );
    module.clock.set(Duration::from_secs(3_600));
    assert_eq!(
        module.agents_last_hour(60_000).unwrap().bound,
        Some(subc_control::ResourceBound::Lower)
    );
    module.record_sample(120_000, Some(0), Some(0));
    module.record_sample(3_720_000, Some(0), Some(0));
    assert_eq!(module.agents_last_hour(3_720_000).unwrap().bound, None);
    assert!(module.cpu_model.lower());
}

#[test]
fn backward_wall_time_cannot_extend_attribution_retention() {
    let mut world = World::new();
    let command = world.spawn(1);
    let born = BTreeMap::from([(1, 0), (command, 1)]);
    let mut module = module();
    let initial = table(&world, &born, 1, None);
    assert!(!declare(&mut module, &initial, command));
    observe(&mut module, &world, &born, 1, 0, 60_000);
    world.burn(command, 10);
    world.exit(command, true);
    observe(&mut module, &world, &born, 2, 10, 120_000);
    module.clock.set(Duration::from_millis(420_000));
    let snapshot = table(&world, &born, 3, None);
    module
        .prospective(
            1,
            Some(cgroup_scope(&world, &snapshot, 10)),
            &snapshot,
            30_000,
            10,
        )
        .unwrap();
    module.finish_at(
        AgentCommandRoot {
            pid: command,
            start_time_ms: 1,
        },
        Some(10),
        30_000,
    );
    assert_eq!(module.cpu_model.rows(), (10, 0, 10));
    assert!(module.agents_lower);
}

fn module() -> Module {
    Module {
        pid: Some(1),
        counter: Some(1),
        generation: 1,
        clock: Clock::manual(Duration::ZERO),
        ..Module::default()
    }
}

fn observe(
    module: &mut Module,
    world: &World,
    born: &BTreeMap<u32, u64>,
    sequence: u64,
    raw: u64,
    at: u64,
) {
    module.clock.set(Duration::from_millis(at));
    let snapshot = table(world, born, sequence, None);
    let (tree, agents) = module
        .prospective(
            1,
            Some(cgroup_scope(world, &snapshot, raw)),
            &snapshot,
            at,
            0,
        )
        .unwrap();
    module
        .history
        .record_observation(sequence, at, 0, tree.unwrap().cpu_ms);
    module
        .agents_history
        .record_observation(sequence, at, 0, agents.cpu_ms);
}

#[test]
fn a_first_seen_module_child_reserves_its_fifty_ms_before_a_finish_can_claim_it() {
    let mut world = World::new();
    world.burn(1, 90);
    let command = world.spawn(1);
    let mut born = BTreeMap::from([(1, 0), (command, 1)]);
    let mut module = module();
    assert!(!declare(
        &mut module,
        &table(&world, &born, 1, None),
        command
    ));
    observe(&mut module, &world, &born, 1, 90, 60_000);
    let own_child = world.spawn(1);
    born.insert(own_child, 61_000);
    world.burn(own_child, 50);
    world.burn(command, 10);
    observe(&mut module, &world, &born, 2, 150, 120_000);
    assert_eq!(module.cpu_model.rows(), (140, 10, 150));
    assert_eq!(
        module.group_credit.module_credit(),
        140,
        "the first-seen child's50ms must be reserved before any later finish"
    );
    world.exit(command, true);
    world.exit(own_child, true);
    observe(&mut module, &world, &born, 3, 150, 180_000);
    module.finish_at(
        AgentCommandRoot {
            pid: command,
            start_time_ms: 1,
        },
        Some(60),
        180_000,
    );
    assert_eq!(
        module.cpu_model.rows(),
        (140, 10, 150),
        "the child's50ms remain module-owned, not finish funding"
    );
    assert!(module.agents_lower);
}

#[test]
fn a_finish_cannot_reclassify_funding_from_before_its_last_successful_measurement() {
    let mut world = World::new();
    let command = world.spawn(1);
    let born = BTreeMap::from([(1, 0), (command, 1)]);
    let mut module = module();
    assert!(!declare(
        &mut module,
        &table(&world, &born, 1, None),
        command
    ));
    observe(&mut module, &world, &born, 1, 0, 60_000);
    world.unseen_child(1, 100);
    observe(&mut module, &world, &born, 2, 100, 120_000);
    world.burn(command, 10);
    world.exit(command, true);
    observe(&mut module, &world, &born, 3, 110, 180_000);
    // Isolate the last interval: the older unclassified100ms is its baseline.
    module.history = History::default();
    module.agents_history = History::default();
    module.history.record_observation(2, 120_000, 0, 100);
    module.agents_history.record_observation(2, 120_000, 0, 0);
    module.history.record_observation(3, 180_000, 0, 110);
    module.agents_history.record_observation(3, 180_000, 0, 0);
    module.finish_at(
        AgentCommandRoot {
            pid: command,
            start_time_ms: 1,
        },
        Some(10),
        180_000,
    );
    assert_eq!(module.cpu_model.rows(), (100, 10, 110));
    assert_eq!(module.history.last_hour_at(180_000).unwrap().cpu_ms, 0);
    assert_eq!(
        module.agents_history.last_hour_at(180_000).unwrap().cpu_ms,
        10
    );
}

#[test]
fn expired_measured_uncredited_cpu_stays_module_side_even_with_newer_funding() {
    let mut world = World::new();
    let command = world.spawn(1);
    world.nodes.get_mut(&command).unwrap().inside = false;
    let born = BTreeMap::from([(1, 0), (command, 1)]);
    let mut module = module();
    let initial = table(&world, &born, 1, None);
    declare_location(&mut module, &initial, command, false);
    module
        .prospective(
            1,
            Some(cgroup_scope(&world, &initial, 0)),
            &initial,
            60_000,
            0,
        )
        .unwrap();
    world.unseen_child(1, 100);
    world.burn(command, 100);
    let measured = table(&world, &born, 2, None);
    module.clock.set(Duration::from_millis(120_000));
    module
        .prospective(
            1,
            Some(cgroup_scope(&world, &measured, 100)),
            &measured,
            120_000,
            0,
        )
        .unwrap();
    assert_eq!(module.cpu_model.rows(), (100, 0, 100));
    world.unseen_child(1, 100);
    world.exit(command, true);
    let expired = table(&world, &born, 3, None);
    module.clock.set(Duration::from_millis(420_000));
    module
        .prospective(
            1,
            Some(cgroup_scope(&world, &expired, 200)),
            &expired,
            420_000,
            100,
        )
        .unwrap();
    module.finish_at(
        AgentCommandRoot {
            pid: command,
            start_time_ms: 1,
        },
        Some(100),
        420_000,
    );
    assert_eq!(module.cpu_model.rows(), (200, 0, 200));
    assert_eq!(
        module.agent_bound(None),
        Some(subc_control::ResourceBound::Lower)
    );
}

#[test]
fn a_late_finish_cannot_spend_a_new_child_reaped_after_its_first_confirmed_exit() {
    let mut world = World::new();
    world.burn(1, 90);
    let command = world.spawn(1);
    let mut born = BTreeMap::from([(1, 0), (command, 1)]);
    let mut module = module();
    assert!(!declare(
        &mut module,
        &table(&world, &born, 1, None),
        command
    ));
    observe(&mut module, &world, &born, 1, 90, 60_000);
    world.burn(command, 10);
    world.exit(command, true);
    observe(&mut module, &world, &born, 2, 100, 120_000);
    let newer = world.spawn(1);
    born.insert(newer, 400_000);
    world.burn(newer, 50);
    world.exit(newer, true);
    observe(&mut module, &world, &born, 3, 150, 480_000);
    module.finish_at(
        AgentCommandRoot {
            pid: command,
            start_time_ms: 1,
        },
        Some(10),
        480_000,
    );
    assert_eq!(module.cpu_model.rows(), (150, 0, 150));
    assert!(
        module.agents_lower,
        "expired original funding is a shortfall, not permission to use newer work"
    );
}

#[test]
fn a_sampler_after_the_finish_receipt_cannot_fund_that_finish_on_a_retry() {
    let mut world = World::new();
    world.burn(1, 90);
    let command = world.spawn(1);
    let mut born = BTreeMap::from([(1, 0), (command, 1)]);
    let root = AgentCommandRoot {
        pid: command,
        start_time_ms: 1,
    };
    let mut module = module();
    assert!(!declare(
        &mut module,
        &table(&world, &born, 1, None),
        command
    ));
    observe(&mut module, &world, &born, 1, 90, 60_000);
    world.burn(command, 10);
    world.exit(command, true);
    let first = table(&world, &born, 2, None);
    let uncertain = ProcessTable::from_test_members(
        2,
        120_000,
        first.members().cloned().collect(),
        vec![(command, Some(1), Some(0))],
    );
    module.observe_reaped(&uncertain, 120_000);
    assert_eq!(module.roots[&root].first_gone_sequence, None);
    let newer = world.spawn(1);
    born.insert(newer, 121_000);
    world.burn(newer, 50);
    world.exit(newer, true);
    observe(&mut module, &world, &born, 3, 150, 180_000);
    assert_eq!(module.roots[&root].first_gone_sequence, Some(3));
    module.finish_at_observation(root, Some(10), 180_000, 2);
    assert_eq!(
        module.cpu_model.rows(),
        (150, 0, 150),
        "a retry cannot extend the receipt's first snapshot boundary"
    );
    assert!(module.agents_lower);
}
