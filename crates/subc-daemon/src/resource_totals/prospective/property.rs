use super::*;
use crate::resource_totals::cpu_ledger::tests::property::{Minimum, World};
#[path = "finish_regressions.rs"]
mod finish_regressions;

fn table(
    world: &World,
    born: &BTreeMap<u32, u64>,
    revision: u64,
    unreadable: Option<u32>,
) -> ProcessTable {
    let current = world
        .nodes
        .iter()
        .filter(|(_, node)| node.alive)
        .map(|(pid, node)| subc_os::ProcessMember {
            pid: *pid,
            parent_pid: node.parent.unwrap_or(0),
            session_id: Some(0),
            start_time: 1,
            start_time_ms: born[pid],
            name: format!("program-{pid}"),
            resources: (Some(*pid) != unreadable).then_some(subc_os::ResourceUsage {
                cpu_user: Duration::from_millis(node.own + node.reaped),
                cpu_system: Duration::ZERO,
                memory_bytes: 10,
                swap_bytes: None,
                memory_kind: subc_os::MemoryKind::ResidentSet,
            }),
            own_cpu_ms: Some(node.own),
            reaped_cpu_ms: Some(node.reaped),
            #[cfg(windows)]
            process: Arc::new(subc_os::Process::open(std::process::id()).unwrap().unwrap()),
        })
        .collect();
    ProcessTable::from_test_members(revision, revision * 60_000, current, Vec::new())
}

fn scope(table: &ProcessTable) -> Option<(TreeReading, BTreeSet<(u32, u64)>)> {
    let usage = table.tree(1, 1, false)?;
    let identities = usage
        .members
        .iter()
        .map(|member| (member.pid, member.start_time))
        .collect();
    Some((wire(usage, TreeSource::ProcessTree), identities))
}

fn declare(module: &mut Module, table: &ProcessTable, pid: u32) -> bool {
    declare_location(module, table, pid, true)
}

fn declare_location(module: &mut Module, table: &ProcessTable, pid: u32, inside: bool) -> bool {
    let root = AgentCommandRoot {
        pid,
        start_time_ms: table.process(pid).unwrap().start_time_ms,
    };
    let candidate = VerifiedRoot {
        root,
        native_start: 1,
        inside,
        members: table.tree_identities(pid, 1, true).unwrap(),
        #[cfg(windows)]
        process: Some(Arc::new(
            subc_os::Process::open(std::process::id()).unwrap().unwrap(),
        )),
    };
    module.accept(1, root, Some(candidate), table, table.observed_at_ms())
}

#[cfg(unix)]
#[test]
fn actual_ambiguous_init_reap_and_unseen_child_histories_publish_the_same_provable_floor() {
    for grandchild_reaped_by_parent in [false, true] {
        let mut world = World::new();
        let child = world.spawn(1);
        let grandchild = world.spawn(child);
        let born = BTreeMap::from([(1, 0), (child, 1), (grandchild, 2)]);
        world.burn(1, 10);
        world.burn(child, 20);
        world.burn(grandchild, 90);
        let mut module = Module::default();
        let first = table(&world, &born, 1, None);
        assert_eq!(
            module
                .prospective(1, scope(&first), &first, 60_000, 0)
                .unwrap()
                .0
                .unwrap()
                .cpu_ms,
            120
        );
        world.exit(grandchild, grandchild_reaped_by_parent);
        world.exit(child, true);
        world.unseen_child(1, if grandchild_reaped_by_parent { 10 } else { 100 });
        world.burn(1, 5);
        let after = table(&world, &born, 2, None);
        let (tree, agents) = module
            .prospective(1, scope(&after), &after, 120_000, 0)
            .unwrap();
        let tree = tree.unwrap();
        assert_eq!(tree.cpu_ms + agents.cpu_ms, 135);
        assert!(tree.cpu_ms <= world.true_scope);
        assert_eq!(
            world.true_scope,
            if grandchild_reaped_by_parent {
                135
            } else {
                225
            }
        );
        assert_eq!(tree.bound, Some(subc_control::ResourceBound::Lower));
    }
}

#[cfg(unix)]
#[test]
fn actual_unmeasured_finish_stays_in_the_module_and_live_baseline_outlives_tombstone_expiry() {
    let mut world = World::new();
    let root = world.spawn(1);
    let born = BTreeMap::from([(1, 0), (root, 1)]);
    world.burn(1, 10);
    world.burn(root, 90);
    let mut module = Module::default();
    let unknown = table(&world, &born, 1, Some(root));
    assert!(!declare(&mut module, &unknown, root));
    assert!(!module.roots.values().next().unwrap().measured);
    let identity = *module.roots.keys().next().unwrap();
    assert!(module
        .prospective(1, scope(&unknown), &unknown, 60_000, 0)
        .is_none());
    module.finish_at(identity, Some(u64::MAX), 60_000);
    let recovered = table(&world, &born, 2, None);
    let (tree, agents) = module
        .prospective(1, scope(&recovered), &recovered, 120_000, 0)
        .unwrap();
    assert_eq!((tree.unwrap().cpu_ms, agents.cpu_ms), (100, 0));
    assert_eq!(agents.unmeasured_finished, Some(1));
    module.prune(60_000 + 10 * 60_000 + 1);
    assert!(module.finished.is_empty());
    module.prune_members(&recovered);
    assert!(!declare(&mut module, &recovered, root));
    assert!(
        module.roots.is_empty(),
        "a still-live finished identity cannot earn a second baseline"
    );
    assert_eq!(module.unmeasured_finished, 1);
}

fn cgroup_scope(
    world: &World,
    table: &ProcessTable,
    cpu_ms: u64,
) -> (TreeReading, BTreeSet<(u32, u64)>) {
    let current: Vec<_> = world
        .nodes
        .iter()
        .filter(|(_, node)| node.inside && node.alive)
        .map(|(pid, _)| table.process(*pid).unwrap().clone())
        .collect();
    let identities = current
        .iter()
        .map(|member| (member.pid, member.start_time))
        .collect();
    let mut tree = wire(ProcessTable::sum(current).unwrap(), TreeSource::Cgroup);
    tree.cpu_ms = cpu_ms;
    (tree, identities)
}

#[cfg(unix)]
#[test]
fn actual_linux_inside_outside_and_mixed_intervals_follow_the_measured_domain() {
    for (inside_before, inside_after) in
        [(true, true), (false, false), (true, false), (false, true)]
    {
        for hidden_inside_burn in [false, true] {
            let mut world = World::new();
            let root = world.spawn(1);
            let born = BTreeMap::from([(1, 0), (root, 1)]);
            world.nodes.get_mut(&root).unwrap().inside = inside_before;
            let mut module = Module {
                generation: 1,
                counter: Some(1),
                ..Module::default()
            };
            let first = table(&world, &born, 1, None);
            assert!(!declare_location(&mut module, &first, root, inside_before));
            module
                .prospective(1, Some(cgroup_scope(&world, &first, 0)), &first, 60_000, 0)
                .unwrap();
            world.nodes.get_mut(&root).unwrap().inside = hidden_inside_burn;
            world.burn(root, 50);
            if !hidden_inside_burn {
                world.burn(1, 50);
            }
            world.nodes.get_mut(&root).unwrap().inside = inside_after;
            let after = table(&world, &born, 2, None);
            let (tree, agents) = module
                .prospective(
                    1,
                    Some(cgroup_scope(&world, &after, 50)),
                    &after,
                    120_000,
                    0,
                )
                .unwrap();
            let tree = tree.unwrap();
            assert_eq!(tree.cpu_ms + agents.cpu_ms, 50);
            assert!(50 <= world.true_scope + if hidden_inside_burn { 0 } else { 50 });
            if inside_before && inside_after {
                assert_eq!((tree.cpu_ms, agents.cpu_ms), (0, 50));
                assert_eq!(tree.bound, None);
            } else {
                assert_eq!((tree.cpu_ms, agents.cpu_ms), (50, 0));
                assert_eq!(tree.bound, Some(subc_control::ResourceBound::Lower));
            }
            assert_eq!((tree.memory_bytes, agents.memory_bytes), (10, 10));
        }
    }
}

#[cfg(unix)]
#[test]
fn actual_module_model_keeps_five_new_module_ms_after_the_ninety_ms_command_leaves() {
    let mut world = World::new();
    let root = world.spawn(1);
    let born = BTreeMap::from([(1, 0), (root, 1)]);
    let mut module = Module::default();
    let first = table(&world, &born, 1, None);
    assert!(!declare(&mut module, &first, root));
    module
        .prospective(1, scope(&first), &first, 60_000, 0)
        .unwrap();
    world.burn(1, 10);
    world.burn(root, 90);
    let current = table(&world, &born, 2, None);
    let (tree, agents) = module
        .prospective(1, scope(&current), &current, 120_000, 0)
        .unwrap();
    assert_eq!((tree.unwrap().cpu_ms, agents.cpu_ms), (10, 90));
    world.detach(root);
    world.burn(1, 5);
    let current = table(&world, &born, 3, None);
    let (tree, agents) = module
        .prospective(1, scope(&current), &current, 180_000, 0)
        .unwrap();
    assert_eq!((tree.unwrap().cpu_ms, agents.cpu_ms), (15, 90));
}

#[cfg(unix)]
#[test]
fn actual_late_declaration_keeps_old_cpu_in_the_module_and_out_of_new_hourly_progress() {
    let mut world = World::new();
    let root = world.spawn(1);
    let born = BTreeMap::from([(1, 0), (root, 1)]);
    world.burn(1, 10);
    world.burn(root, 90);
    let mut module = Module::default();
    let old = table(&world, &born, 1, None);
    let (tree, agents) = module.prospective(1, scope(&old), &old, 60_000, 0).unwrap();
    module
        .history
        .record_observation(module.table_revision, 60_000, 0, tree.unwrap().cpu_ms);
    module
        .agents_history
        .record_observation(module.table_revision, 60_000, 0, agents.cpu_ms);
    let accepted = table(&world, &born, 2, None);
    assert!(!declare(&mut module, &accepted, root));
    let (tree, agents) = module
        .prospective(1, scope(&accepted), &accepted, 120_000, 0)
        .unwrap();
    assert_eq!((tree.as_ref().unwrap().cpu_ms, agents.cpu_ms), (100, 0));
    assert_eq!(
        agents.groups.iter().map(|group| group.cpu_ms).sum::<u64>(),
        0
    );
    module
        .history
        .record_observation(module.table_revision, 120_000, 0, tree.unwrap().cpu_ms);
    module
        .agents_history
        .record_observation(module.table_revision, 120_000, 0, agents.cpu_ms);
    assert_eq!(module.last_hour(120_000).unwrap().cpu_ms, 0);
    assert_eq!(
        module.agents_history.last_hour_at(120_000).unwrap().cpu_ms,
        0
    );
}

fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn descendants(world: &World, roots: impl IntoIterator<Item = u32>) -> BTreeSet<u32> {
    let mut selected: BTreeSet<_> = roots
        .into_iter()
        .filter(|pid| world.nodes[pid].alive)
        .collect();
    loop {
        let before = selected.len();
        for (pid, node) in &world.nodes {
            if node.alive && node.parent.is_some_and(|parent| selected.contains(&parent)) {
                selected.insert(*pid);
            }
        }
        if selected.len() == before {
            return selected;
        }
    }
}

#[cfg(unix)]
#[test]
fn randomized_actual_module_observations_prove_the_floor_and_prospective_row_invariants() {
    let mut transferred_total = 0;
    for seed in 1..=48 {
        let mut rng = seed;
        let mut world = World::new();
        let mut born = BTreeMap::from([(1, 0)]);
        let mut module = Module {
            pid: Some(1),
            ..Module::default()
        };
        let mut transfers_since_published = 0;
        let mut oracle = Minimum::default();
        let mut minimum = 0;
        let mut true_outside = 0;
        let mut ownership = BTreeMap::<u32, u32>::new();
        let mut observed_members = BTreeMap::<u32, BTreeSet<u32>>::new();
        let mut previous = (0, 0, 0);
        let mut total_hour = History::default();
        let mut last_table = None;
        for step in 1..=96_u64 {
            let alive: Vec<_> = world
                .nodes
                .iter()
                .filter_map(|(pid, node)| node.alive.then_some(*pid))
                .collect();
            let pid = alive[random(&mut rng) as usize % alive.len()];
            let amount = random(&mut rng) % 19 + 1;
            let action = random(&mut rng) % 12;
            let active: BTreeSet<_> = module.roots.keys().map(|root| root.pid).collect();
            match action {
                0 | 1 => {
                    if !world.nodes[&pid].inside
                        && ownership
                            .get(&pid)
                            .is_some_and(|root| active.contains(root))
                    {
                        true_outside += amount;
                    }
                    world.burn(pid, amount);
                }
                2 if alive.len() < 12 => {
                    let child = world.spawn(pid);
                    born.insert(child, step * 60_000 - 1);
                    if let Some(owner) = ownership
                        .get(&pid)
                        .copied()
                        .filter(|owner| active.contains(owner))
                    {
                        ownership.insert(child, owner);
                    }
                }
                3 if pid != 1 => {
                    // Leave real unobserved command tail for finish claims.
                    // Burning and reaping on separate sampled steps would
                    // credit every unit before a finish could transfer it.
                    if ownership
                        .get(&pid)
                        .is_some_and(|root| active.contains(root))
                    {
                        if !world.nodes[&pid].inside {
                            true_outside += amount;
                        }
                        world.burn(pid, amount);
                    }
                    world.exit(pid, true);
                }
                4 if pid != 1 => world.exit(pid, false),
                5 if pid != 1 => world.detach(pid),
                6 => {
                    if !world.nodes[&pid].inside
                        && ownership
                            .get(&pid)
                            .is_some_and(|root| active.contains(root))
                    {
                        true_outside += amount;
                    }
                    world.unseen_child(pid, amount);
                }
                8 => {
                    if let Some(root) = module
                        .roots
                        .keys()
                        .copied()
                        .find(|root| !world.nodes[&root.pid].alive)
                    {
                        let before = module.cpu_model.rows();
                        module.finish_at(root, Some(u64::MAX), step * 60_000);
                        let after = module.cpu_model.rows();
                        let transfer = before.0 - after.0;
                        assert_eq!(after.1 - before.1, transfer);
                        assert_eq!(
                            after.2, before.2,
                            "a finish changes ownership, not measured CPU"
                        );
                        transfers_since_published += transfer;
                        transferred_total += transfer;
                    }
                }
                _ => {}
            }
            let current = table(&world, &born, step, (action == 9).then_some(pid));
            if action == 7
                && pid != 1
                && world.nodes[&pid].alive
                && world.nodes[&pid].parent == Some(1)
                && !declare(&mut module, &current, pid)
                && module.roots.keys().any(|root| root.pid == pid)
            {
                let descendants = current.tree_identities(pid, 1, true).unwrap();
                for (member, _) in descendants {
                    ownership.insert(member, pid);
                }
            }
            if action == 10 {
                if let Some(old) = &last_table {
                    let snapshot = module.cpu_model.rows();
                    let _ = module.prospective(1, scope(old), old, step * 60_000, 0);
                    assert_eq!(
                        module.cpu_model.rows(),
                        snapshot,
                        "a stale observation cannot commit CPU twice"
                    );
                }
            }
            let before = module.cpu_model.rows();
            let Some((Some(tree), agents)) =
                module.prospective(1, scope(&current), &current, step * 60_000, 0)
            else {
                assert_eq!(
                    module.cpu_model.rows(),
                    before,
                    "an unreadable tree cannot advance a partial sum"
                );
                continue;
            };
            let mut selected: BTreeMap<_, _> = world
                .members()
                .into_iter()
                .map(|member| (member.identity.0, member))
                .collect();
            for root in module.roots.keys() {
                let current_members = if world.nodes[&root.pid].alive {
                    descendants(&world, [root.pid])
                } else {
                    descendants(
                        &world,
                        observed_members.get(&root.pid).cloned().unwrap_or_default(),
                    )
                };
                for (pid, node) in &world.nodes {
                    if current_members.contains(pid) {
                        selected.insert(
                            *pid,
                            Member {
                                identity: (*pid, 1),
                                parent: node.parent.map(|parent| (parent, 1)),
                                own_ms: node.own,
                                reaped_ms: node.reaped,
                                new_since_baseline: true,
                            },
                        );
                    }
                }
                observed_members.insert(root.pid, current_members);
            }
            minimum =
                minimum.max(oracle.observe_selected(&world, selected.into_values().collect()));
            let total = tree.cpu_ms + agents.cpu_ms;
            let (all_module_groups, all_agent_groups) = module.group_credit.totals();
            assert!(all_module_groups <= tree.cpu_ms && all_agent_groups <= agents.cpu_ms,
                "funded groups exceed a row: seed={seed} step={step} groups=({all_module_groups},{all_agent_groups}) rows=({},{})", tree.cpu_ms, agents.cpu_ms);
            assert!(
                tree.own.as_ref().map_or(0, |own| own.cpu_ms)
                    + tree.groups.iter().map(|group| group.cpu_ms).sum::<u64>()
                    <= tree.cpu_ms
            );
            assert!(
                tree.own.as_ref().map_or(0, |own| own.memory_bytes)
                    + tree
                        .groups
                        .iter()
                        .map(|group| group.memory_bytes)
                        .sum::<u64>()
                    <= tree.memory_bytes
            );
            assert!(tree.own.as_ref().is_none_or(|own| own.bound == tree.bound));
            assert!(agents.groups.iter().map(|group| group.cpu_ms).sum::<u64>() <= agents.cpu_ms);
            assert!(tree.groups.iter().all(|group| group.bound == tree.bound));
            assert!(agents
                .groups
                .iter()
                .all(|group| group.bound == agents.bound));
            assert!(total <= world.true_scope + true_outside,
                "published exceeds true history: seed={seed} step={step} action={action} rows=({}, {}) true={} world={world:?}", tree.cpu_ms, agents.cpu_ms, world.true_scope + true_outside);
            assert!(total >= minimum, "lost provable CPU: seed={seed} step={step} published={total} minimum={minimum} world={world:?}");
            assert!(tree.cpu_ms+transfers_since_published >= previous.0 && agents.cpu_ms >= previous.1 && total >= previous.2,
                "every module decrease must have an explicit equal agents transfer: seed={seed} step={step}");
            transfers_since_published = 0;
            assert_eq!((tree.cpu_ms, agents.cpu_ms, total), module.cpu_model.rows());
            module
                .history
                .record_observation(module.table_revision, step * 60_000, 0, tree.cpu_ms);
            module.agents_history.record_observation(
                module.table_revision,
                step * 60_000,
                0,
                agents.cpu_ms,
            );
            total_hour.record(step * 60_000, 0, total);
            if let Some(total) = total_hour.last_hour_at(step * 60_000) {
                let module_hour = module.last_hour(step * 60_000).unwrap();
                let agents_hour = module.agents_history.last_hour_at(step * 60_000).unwrap();
                assert_eq!(module_hour.covered_ms, agents_hour.covered_ms);
                assert_eq!(
                    module_hour.cpu_ms + agents_hour.cpu_ms,
                    total.cpu_ms,
                    "hourly attribution cannot manufacture historical CPU: seed={seed} step={step}"
                );
            }
            previous = (tree.cpu_ms, agents.cpu_ms, total);
            last_table = Some(current);
        }
    }
    assert!(
        transferred_total != 0,
        "random histories must exercise actual finish transfers"
    );
}

#[cfg(unix)]
#[test]
fn root_parts_conserve_cpu_and_memory_with_capped_child_groups() {
    let mut world = World::new();
    let mut born = BTreeMap::from([(1, 0)]);
    world.burn(1, 100);
    for amount in 1..=10 {
        let child = world.spawn(1);
        born.insert(child, 1);
        world.burn(child, amount * 10);
    }
    let current = table(&world, &born, 1, None);
    let mut module = Module {
        pid: Some(1),
        ..Module::default()
    };
    let (mut reading, identities) = cgroup_scope(&world, &current, 123);
    // A native container can report less memory than summed process RSS.
    // The detail parts must still fit the measured row, not the raw sum.
    reading.memory_bytes = 25;
    let (tree, _) = module
        .prospective(1, Some((reading, identities)), &current, 60_000, 0)
        .unwrap();
    let tree = tree.unwrap();
    let own = tree
        .own
        .as_ref()
        .expect("root has its own attributed share");
    assert_eq!((own.cpu_ms, own.memory_bytes), (100, 10));
    assert_eq!(tree.groups.len(), 8);
    assert!(tree.groups.iter().all(|group| group.name != "program-1"));
    assert!(own.cpu_ms + tree.groups.iter().map(|group| group.cpu_ms).sum::<u64>() <= tree.cpu_ms);
    assert!(
        own.memory_bytes
            + tree
                .groups
                .iter()
                .map(|group| group.memory_bytes)
                .sum::<u64>()
            <= tree.memory_bytes
    );
    assert!(tree
        .groups
        .windows(2)
        .all(|pair| pair[0].memory_bytes >= pair[1].memory_bytes));
}

#[cfg(unix)]
#[test]
fn root_parts_keep_same_named_children_in_their_group() {
    let mut world = World::new();
    let child = world.spawn(1);
    let born = BTreeMap::from([(1, 0), (child, 1)]);
    world.burn(1, 30);
    world.burn(child, 70);
    let base = table(&world, &born, 1, None);
    let current = ProcessTable::from_test_members(
        1,
        60_000,
        base.members()
            .cloned()
            .map(|mut member| {
                member.name = "module-program".into();
                member
            })
            .collect(),
        Vec::new(),
    );
    let mut module = Module {
        pid: Some(1),
        ..Module::default()
    };
    let (tree, _) = module
        .prospective(1, scope(&current), &current, 60_000, 0)
        .unwrap();
    let tree = tree.unwrap();
    assert_eq!(tree.groups.len(), 1);
    let child_group = &tree.groups[0];
    assert_eq!(
        (
            &*child_group.name,
            child_group.count,
            child_group.cpu_ms,
            child_group.memory_bytes
        ),
        ("module-program", 1, 70, 10)
    );
    let own = tree.own.unwrap();
    assert_eq!((own.cpu_ms, own.memory_bytes), (30, 10));
}

#[cfg(unix)]
#[test]
fn root_parts_without_children_report_only_funded_own_cpu() {
    let mut world = World::new();
    world.burn(1, 120);
    world.unseen_child(1, 80);
    let born = BTreeMap::from([(1, 0)]);
    let current = table(&world, &born, 1, None);
    for contained in [false, true] {
        let mut module = Module {
            pid: Some(1),
            ..Module::default()
        };
        let reading = if contained {
            Some(cgroup_scope(&world, &current, 30))
        } else {
            scope(&current)
        };
        let (tree, _) = module.prospective(1, reading, &current, 60_000, 0).unwrap();
        let tree = tree.unwrap();
        assert!(tree.groups.is_empty(), "a root alone is not a child group");
        let own = tree.own.unwrap();
        assert_eq!(own.cpu_ms, if contained { 30 } else { 120 });
        assert_eq!(own.memory_bytes, 10);
        assert_eq!(tree.cpu_ms, if contained { 30 } else { 200 });
    }
}

#[cfg(unix)]
#[test]
fn root_parts_follow_the_module_rows_lower_bound() {
    let mut world = World::new();
    let child = world.spawn(1);
    world.burn(1, 20);
    world.burn(child, 80);
    let born = BTreeMap::from([(1, 0), (child, 1)]);
    let current = table(&world, &born, 1, None);
    for contained in [false, true] {
        let mut module = Module {
            pid: Some(1),
            ..Module::default()
        };
        let reading = if contained {
            Some(cgroup_scope(&world, &current, 100))
        } else {
            scope(&current)
        };
        let (tree, _) = module.prospective(1, reading, &current, 60_000, 0).unwrap();
        let tree = tree.unwrap();
        let expected = (!contained).then_some(subc_control::ResourceBound::Lower);
        assert_eq!(tree.bound, expected);
        assert_eq!(tree.own.unwrap().bound, expected);
        assert!(tree.groups.iter().all(|group| group.bound == expected));
    }
}

#[cfg(unix)]
#[test]
fn initial_module_groups_are_funded_largest_own_counter_first_and_new_module_identities_keep_their_cpu(
) {
    let mut world = World::new();
    let child = world.spawn(1);
    let mut born = BTreeMap::from([(1, 0), (child, 1)]);
    world.burn(1, 70);
    world.burn(child, 90);
    let mut module = Module {
        pid: Some(1),
        ..Module::default()
    };
    let first = table(&world, &born, 1, None);
    let (tree, agents) = module
        .prospective(
            1,
            Some(cgroup_scope(&world, &first, 100)),
            &first,
            60_000,
            0,
        )
        .unwrap();
    let tree = tree.unwrap();
    assert_eq!(
        tree.groups
            .iter()
            .find(|group| group.name == "program-2")
            .unwrap()
            .cpu_ms,
        90
    );
    assert_eq!(tree.own.as_ref().unwrap().cpu_ms, 10);
    assert_eq!(module.group_credit.totals(), (100, 0));
    assert_eq!(agents.cpu_ms, 0);
    let late = world.spawn(1);
    born.insert(late, 1);
    world.burn(late, 80);
    let second = table(&world, &born, 2, None);
    let (tree, _) = module
        .prospective(
            1,
            Some(cgroup_scope(&world, &second, 180)),
            &second,
            120_000,
            100,
        )
        .unwrap();
    assert_eq!(
        tree.unwrap()
            .groups
            .iter()
            .find(|group| group.name == "program-3")
            .unwrap()
            .cpu_ms,
        80
    );
    world.burn(late, 5);
    let third = table(&world, &born, 3, None);
    let (tree, _) = module
        .prospective(
            1,
            Some(cgroup_scope(&world, &third, 185)),
            &third,
            180_000,
            180,
        )
        .unwrap();
    assert_eq!(
        tree.unwrap()
            .groups
            .iter()
            .find(|group| group.name == "program-3")
            .unwrap()
            .cpu_ms,
        85
    );
}

#[cfg(unix)]
#[test]
fn carried_baselines_continue_but_replaced_domains_rebaseline_only_moving_survivors() {
    for transition in [0, 7] {
        let mut world = World::new();
        let root = world.spawn(1);
        let born = BTreeMap::from([(1, 0), (root, 1)]);
        let mut module = Module {
            generation: 1,
            counter: Some(1),
            ..Module::default()
        };
        let first = table(&world, &born, 1, None);
        declare(&mut module, &first, root);
        module
            .prospective(1, Some(cgroup_scope(&world, &first, 0)), &first, 60_000, 0)
            .unwrap();
        world.burn(root, 80);
        let second = table(&world, &born, 2, None);
        module
            .prospective(
                1,
                Some(cgroup_scope(&world, &second, 80)),
                &second,
                120_000,
                0,
            )
            .unwrap();
        module.bind_counter(2, 1, None, Some(1));
        world.burn(root, 5);
        let carried = table(&world, &born, 3, None);
        let (tree, agents) = module
            .prospective(
                2,
                Some(cgroup_scope(&world, &carried, 85)),
                &carried,
                180_000,
                80,
            )
            .unwrap();
        assert_eq!((tree.unwrap().cpu_ms, agents.cpu_ms), (0, 85));
        assert_eq!(agents.groups[0].cpu_ms, 85);
        assert_eq!(module.roots.values().next().unwrap().accepted_generation, 2);
        module.bind_counter(3, 2, None, Some(1));
        world.burn(root, transition);
        let replaced = table(&world, &born, 4, None);
        let (tree, agents) = module
            .prospective(
                3,
                Some(cgroup_scope(&world, &replaced, 10)),
                &replaced,
                240_000,
                0,
            )
            .unwrap();
        let tree = tree.unwrap();
        assert_eq!((tree.cpu_ms, agents.cpu_ms), (10, 85));
        assert_eq!(
            agents.groups[0].cpu_ms, 85,
            "credited lifetime CPU survives rebaseline"
        );
        assert_eq!(tree.bound.is_some(), transition != 0);
        assert_eq!(module.group_credit.totals(), (0, 85));
    }
}

#[cfg(unix)]
#[test]
fn seeded_actual_cgroup_domain_carries_restarts_and_funded_groups_without_inventing_cpu() {
    for seed in 1..=32 {
        let mut rng = seed;
        let mut world = World::new();
        let root = world.spawn(1);
        let born = BTreeMap::from([(1, 0), (root, 1)]);
        let mut module = Module {
            generation: 1,
            counter: Some(1),
            ..Module::default()
        };
        let mut generation = 1_u128;
        let mut container = 1_u128;
        let mut base = 0;
        let mut outside_true = 0;
        let mut previous = (0, 0, 0);
        let mut total_hour = History::default();
        let mut last = None;
        for step in 1..=128_u64 {
            let action = random(&mut rng) % 10;
            let amount = random(&mut rng) % 21 + 1;
            let accepted = !module.roots.is_empty();
            match action {
                0 | 1 => world.burn(1, amount),
                2 | 3 => {
                    if !world.nodes[&root].inside && accepted {
                        outside_true += amount;
                    }
                    world.burn(root, amount);
                }
                4 => {
                    let node = world.nodes.get_mut(&root).unwrap();
                    node.inside = !node.inside;
                }
                5 if generation < 20 => {
                    generation += 1;
                    module.bind_counter(generation, container, None, Some(1));
                }
                6 if generation < 20 => {
                    generation += 1;
                    container += 1;
                    base = world.true_scope;
                    module.bind_counter(generation, container, None, Some(1));
                }
                7 if accepted => {
                    let identity = *module.roots.keys().next().unwrap();
                    module.finish_at(identity, Some(u64::MAX), step * 60_000);
                }
                _ => {}
            }
            let current = table(&world, &born, step, None);
            if action == 8 && !accepted && module.finished_live.is_empty() {
                declare_location(&mut module, &current, root, world.nodes[&root].inside);
            }
            if action == 9 {
                if let Some(old) = &last {
                    let rows = module.cpu_model.rows();
                    let _ = module.prospective(
                        generation,
                        Some(cgroup_scope(&world, old, world.true_scope - base)),
                        old,
                        step * 60_000,
                        0,
                    );
                    assert_eq!(
                        module.cpu_model.rows(),
                        rows,
                        "stale raw/native mixtures cannot commit"
                    );
                }
            }
            let (tree, agents) = module
                .prospective(
                    generation,
                    Some(cgroup_scope(&world, &current, world.true_scope - base)),
                    &current,
                    step * 60_000,
                    world.true_scope - base,
                )
                .unwrap();
            let tree = tree.unwrap();
            let rows = module.cpu_model.rows();
            assert_eq!(tree.cpu_ms + agents.cpu_ms, rows.2);
            assert!(
                rows.0 >= previous.0 && rows.1 >= previous.1 && rows.2 >= previous.2,
                "seed={seed} step={step}"
            );
            assert!(
                rows.2 >= world.true_scope && rows.2 <= world.true_scope + outside_true,
                "seed={seed} step={step} rows={rows:?} true=({}, {})",
                world.true_scope,
                outside_true
            );
            let groups = module.group_credit.totals();
            assert!(
                groups.0 <= rows.0 && groups.1 <= rows.1,
                "seed={seed} step={step}"
            );
            assert!(tree.groups.iter().map(|group| group.cpu_ms).sum::<u64>() <= tree.cpu_ms);
            assert!(agents.groups.iter().map(|group| group.cpu_ms).sum::<u64>() <= agents.cpu_ms);
            module
                .history
                .record_observation(module.table_revision, step * 60_000, 0, rows.0);
            module.agents_history.record_observation(
                module.table_revision,
                step * 60_000,
                0,
                rows.1,
            );
            total_hour.record(step * 60_000, 0, rows.2);
            if let Some(total) = total_hour.last_hour_at(step * 60_000) {
                assert_eq!(
                    module.last_hour(step * 60_000).unwrap().cpu_ms
                        + module
                            .agents_history
                            .last_hour_at(step * 60_000)
                            .unwrap()
                            .cpu_ms,
                    total.cpu_ms
                );
            }
            previous = rows;
            last = Some(current);
        }
    }
}

#[test]
fn late_outside_declaration_does_not_turn_pre_acceptance_cpu_into_new_hourly_work() {
    let mut world = World::new();
    let root = world.spawn(1);
    let born = BTreeMap::from([(1, 0), (root, 1)]);
    world.nodes.get_mut(&root).unwrap().inside = false;
    world.burn(1, 10);
    world.burn(root, 90);
    let mut module = Module {
        generation: 1,
        counter: Some(1),
        ..Module::default()
    };
    let first = table(&world, &born, 1, None);
    module
        .prospective(
            1,
            Some(cgroup_scope(&world, &first, 10)),
            &first,
            60_000,
            10,
        )
        .unwrap();
    module
        .history
        .record_observation(module.table_revision, 60_000, 0, 10);
    module
        .agents_history
        .record_observation(module.table_revision, 60_000, 0, 0);
    let accepted = table(&world, &born, 2, None);
    declare_location(&mut module, &accepted, root, false);
    let (tree, agents) = module
        .prospective(
            1,
            Some(cgroup_scope(&world, &accepted, 10)),
            &accepted,
            120_000,
            10,
        )
        .unwrap();
    let tree = tree.unwrap();
    assert_eq!((tree.cpu_ms, agents.cpu_ms), (10, 0));
    module
        .history
        .record_observation(module.table_revision, 120_000, 0, tree.cpu_ms);
    module
        .agents_history
        .record_observation(module.table_revision, 120_000, 0, agents.cpu_ms);
    assert_eq!(
        module.last_hour(120_000).unwrap().cpu_ms
            + module.agents_history.last_hour_at(120_000).unwrap().cpu_ms,
        0
    );
    assert_eq!(agents.groups[0].cpu_ms, 0);
}

#[cfg(unix)]
#[test]
fn finished_reports_move_only_post_acceptance_reaped_cpu_once_without_changing_the_stack() {
    let mut world = World::new();
    let first = world.spawn(1);
    let second = world.spawn(1);
    let born = BTreeMap::from([(1, 0), (first, 1), (second, 2)]);
    world.burn(1, 10);
    world.burn(first, 90);
    let mut module = Module {
        pid: Some(1),
        generation: 1,
        counter: Some(1),
        ..Module::default()
    };
    let initial = table(&world, &born, 1, None);
    declare(&mut module, &initial, first);
    declare(&mut module, &initial, second);
    module
        .prospective(
            1,
            Some(cgroup_scope(&world, &initial, 100)),
            &initial,
            60_000,
            0,
        )
        .unwrap();
    world.burn(first, 10);
    world.burn(second, 20);
    world.exit(first, true);
    world.exit(second, true);
    let after = table(&world, &born, 2, None);
    let (tree, agents) = module
        .prospective(
            1,
            Some(cgroup_scope(&world, &after, 130)),
            &after,
            120_000,
            0,
        )
        .unwrap();
    assert_eq!((tree.unwrap().cpu_ms, agents.cpu_ms), (130, 0));
    let first = AgentCommandRoot {
        pid: first,
        start_time_ms: born[&first],
    };
    let second = AgentCommandRoot {
        pid: second,
        start_time_ms: born[&second],
    };
    module.finish_at(first, Some(100), 120_000);
    assert_eq!(
        module.cpu_model.rows(),
        (120, 10, 130),
        "the90ms acceptance baseline cannot move"
    );
    module.finish_at(second, Some(u64::MAX), 120_000);
    assert_eq!(
        module.cpu_model.rows(),
        (100, 30, 130),
        "a huge claim cannot spend module history or earlier claims twice"
    );
    module.finish_at(first, Some(u64::MAX), 120_000);
    assert_eq!(module.cpu_model.rows(), (100, 30, 130));
}

#[cfg(unix)]
#[test]
fn a_finish_without_a_report_or_readable_reap_marks_agents_lower() {
    for reported in [None, Some(10)] {
        let mut world = World::new();
        let root = world.spawn(1);
        let born = BTreeMap::from([(1, 0), (root, 1)]);
        let mut module = Module {
            pid: Some(1),
            ..Module::default()
        };
        let initial = table(&world, &born, 1, None);
        declare(&mut module, &initial, root);
        module
            .prospective(1, scope(&initial), &initial, 60_000, 0)
            .unwrap();
        if reported.is_some() {
            module
                .finish_cpu
                .observe_reaped(None, Duration::from_millis(120_000), 2);
        }
        module.finish_at(
            AgentCommandRoot {
                pid: root,
                start_time_ms: 1,
            },
            reported,
            120_000,
        );
        assert!(module.agents_lower, "absence is not an exact zero tail");
    }
}

#[cfg(unix)]
#[test]
fn a_dead_roots_last_seen_is_not_refreshed_by_empty_successful_walks() {
    let mut world = World::new();
    let root = world.spawn(1);
    let born = BTreeMap::from([(1, 0), (root, 1)]);
    let mut module = Module::default();
    let initial = table(&world, &born, 1, None);
    declare(&mut module, &initial, root);
    module
        .prospective(1, scope(&initial), &initial, 60_000, 0)
        .unwrap();
    world.exit(root, true);
    for revision in 2..=12 {
        let current = table(&world, &born, revision, None);
        module
            .prospective(1, scope(&current), &current, revision * 60_000, 0)
            .unwrap();
    }
    assert_eq!(module.roots.values().next().unwrap().last_seen_ms, 60_000);
    module.prune(12 * 60_000);
    assert!(
        module.roots.is_empty(),
        "an exited root must release the live admission budget"
    );
}

#[cfg(unix)]
#[test]
fn outside_only_agents_keep_hourly_history_and_an_unreadable_peer_cannot_hide_them() {
    let mut world = World::new();
    let root = world.spawn(1);
    let born = BTreeMap::from([(1, 0), (root, 1)]);
    let tracker = Tracker::default();
    let initial = table(&world, &born, 1, None);
    let mut module = Module {
        outside_only: true,
        ..Module::default()
    };
    declare_location(&mut module, &initial, root, false);
    tracker
        .state
        .lock()
        .unwrap()
        .modules
        .insert("outside".into(), module);
    let launch = crate::supervise::ResourceLaunch {
        pid: None,
        start: None,
        spawn_generation: 0,
        enabled: true,
        alive: false,
        target: crate::subtree_resources::Target::Unavailable("no_container"),
    };
    let bad = crate::supervise::ResourceLaunch {
        target: crate::subtree_resources::Target::Unavailable("read_failed"),
        ..launch.clone()
    };
    let launches = BTreeMap::from([
        ("outside".into(), Some(launch)),
        ("unreadable".into(), Some(bad)),
    ]);
    tracker.read_observed(
        launches.clone(),
        Arc::new(initial),
        true,
        60_000,
        Instant::now() + Duration::from_secs(5),
    );
    world.burn(root, 25);
    let current = table(&world, &born, 2, None);
    let reading = tracker.read_observed(
        launches,
        Arc::new(current),
        true,
        120_000,
        Instant::now() + Duration::from_secs(5),
    );
    assert!(reading.modules["outside"].0.is_none());
    let agents = reading
        .agent_commands
        .expect("one unknown module must not erase successful peers");
    let outside = agents
        .iter()
        .find(|row| row.module_id == "outside")
        .unwrap();
    assert_eq!(outside.cpu_ms, 25);
    assert_eq!(
        outside.cpu_last_hour,
        Some(
            subc_control::CpuLastHour::new(25, 60_000)
                .with_bound(Some(subc_control::ResourceBound::Lower))
        )
    );
    assert!(!agents.iter().any(|row| row.module_id == "unreadable"));
}

#[cfg(unix)]
#[test]
fn a_late_finish_can_claim_the_tail_a_sampler_already_published_but_not_after_expiry() {
    for delay in [1_000, 300_001] {
        let mut world = World::new();
        let pid = world.spawn(1);
        let born = BTreeMap::from([(1, 0), (pid, 1)]);
        let mut module = Module {
            clock: Clock::manual(Duration::ZERO),
            pid: Some(1),
            generation: 1,
            counter: Some(1),
            ..Module::default()
        };
        let initial = table(&world, &born, 1, None);
        declare(&mut module, &initial, pid);
        module
            .prospective(
                1,
                Some(cgroup_scope(&world, &initial, 0)),
                &initial,
                60_000,
                0,
            )
            .unwrap();
        module
            .history
            .record_observation(module.table_revision, 60_000, 0, 0);
        module
            .agents_history
            .record_observation(module.table_revision, 60_000, 0, 0);
        world.burn(pid, 100);
        world.exit(pid, true);
        let reaped = table(&world, &born, 2, None);
        module
            .prospective(
                1,
                Some(cgroup_scope(&world, &reaped, 100)),
                &reaped,
                120_000,
                0,
            )
            .unwrap();
        module
            .history
            .record_observation(module.table_revision, 120_000, 0, 100);
        module
            .agents_history
            .record_observation(module.table_revision, 120_000, 0, 0);
        let before = module.history.last_hour_at(120_000).unwrap().cpu_ms;
        module.observe_reaped(&reaped, 120_000 + delay);
        module.clock.set(Duration::from_millis(delay));
        module.finish_at(
            AgentCommandRoot {
                pid,
                start_time_ms: 1,
            },
            Some(100),
            120_000 + delay,
        );
        let expected = if delay < 300_000 { 100 } else { 0 };
        assert_eq!(module.cpu_model.rows(), (100 - expected, expected, 100));
        assert_eq!(module.agents_lower, expected == 0);
        assert_eq!(
            module.history.last_hour_at(120_000).unwrap().cpu_ms
                + module.agents_history.last_hour_at(120_000).unwrap().cpu_ms,
            before
        );
    }
}

#[cfg(unix)]
#[test]
fn old_command_funding_never_steals_the_module_own_work_from_a_newer_minute() {
    for elapsed in [60_000, 3_660_000] {
        let mut world = World::new();
        let pid = world.spawn(1);
        world.nodes.get_mut(&pid).unwrap().inside = false;
        let born = BTreeMap::from([(1, 0), (pid, 1)]);
        let mut module = Module {
            clock: Clock::manual(Duration::ZERO),
            pid: Some(1),
            counter: Some(1),
            generation: 1,
            ..Module::default()
        };
        let initial = table(&world, &born, 1, None);
        declare_location(&mut module, &initial, pid, false);
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
        world.burn(pid, 100);
        let old = table(&world, &born, 2, None);
        module
            .prospective(1, Some(cgroup_scope(&world, &old, 100)), &old, 120_000, 0)
            .unwrap();
        assert_eq!(module.cpu_model.rows(), (100, 0, 100));
        world.burn(1, 10);
        world.exit(pid, true);
        let now = 120_000 + elapsed;
        module.clock.set(Duration::from_millis(elapsed));
        let current = table(&world, &born, 3, None);
        // These two points measure only the latest minute. The funding's age
        // remains its original observation time, not this history baseline.
        module
            .history
            .record_observation(module.table_revision, now - 60_000, 0, 100);
        module
            .agents_history
            .record_observation(module.table_revision, now - 60_000, 0, 0);
        module
            .prospective(
                1,
                Some(cgroup_scope(&world, &current, 110)),
                &current,
                now,
                100,
            )
            .unwrap();
        module
            .history
            .record_observation(module.table_revision, now, 0, 110);
        module
            .agents_history
            .record_observation(module.table_revision, now, 0, 0);
        module.finish_at(
            AgentCommandRoot {
                pid,
                start_time_ms: 1,
            },
            Some(100),
            now,
        );
        let paid = if elapsed < finish_cpu::REAP_POOL_RETENTION_MS {
            100
        } else {
            0
        };
        assert_eq!(module.cpu_model.rows(), (110 - paid, paid, 110));
        assert_eq!(
            module.history.last_hour_at(now).unwrap().cpu_ms,
            10,
            "new module-own work keeps its original minute"
        );
        assert_eq!(
            module.agents_history.last_hour_at(now).unwrap().cpu_ms,
            0,
            "classification cannot manufacture current-hour agent CPU"
        );
        assert!(module.group_credit.module_credit() <= module.cpu_model.rows().0);
        if paid == 0 {
            assert!(
                module.agents_lower,
                "expired timing metadata is not replaced by newer own funding"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn replaying_started_and_finished_identities_is_accounting_idempotent() {
    let mut world = World::new();
    let pid = world.spawn(1);
    world.burn(pid, 90);
    let born = BTreeMap::from([(1, 0), (pid, 1)]);
    let first = table(&world, &born, 1, None);
    let mut module = Module {
        pid: Some(1),
        ..Module::default()
    };
    declare(&mut module, &first, pid);
    let root = *module.roots.keys().next().unwrap();
    let accepted = module.roots[&root].accepted_at;
    world.burn(pid, 10);
    let measured = table(&world, &born, 2, None);
    module
        .prospective(1, scope(&measured), &measured, 120_000, 0)
        .unwrap();
    let rows = module.cpu_model.rows();
    let baseline = module.roots[&root].acceptance_cpu;
    assert!(!declare(&mut module, &measured, pid));
    assert_eq!(module.cpu_model.rows(), rows);
    assert_eq!(module.roots[&root].acceptance_cpu, baseline);
    assert_eq!(
        module.roots[&root].accepted_at, accepted,
        "retry cannot restart prospective attribution"
    );
    world.exit(pid, true);
    let final_table = table(&world, &born, 3, None);
    module
        .prospective(1, scope(&final_table), &final_table, 180_000, 0)
        .unwrap();
    module.finish_at(root, Some(100), 180_000);
    let settled = module.cpu_model.rows();
    let count = module.finished.len();
    module.finish_at(root, Some(u64::MAX), 180_000);
    assert_eq!(module.cpu_model.rows(), settled);
    assert_eq!(module.finished.len(), count);
    assert!(!module.roots.contains_key(&root));
}

#[cfg(unix)]
#[test]
fn module_own_minute_is_not_available_even_after_another_module_member_retires() {
    let mut world = World::new();
    let command = world.spawn(1);
    let other = world.spawn(1);
    world.nodes.get_mut(&command).unwrap().inside = false;
    let born = BTreeMap::from([(1, 0), (command, 1), (other, 2)]);
    let mut module = Module {
        pid: Some(1),
        counter: Some(1),
        generation: 1,
        ..Module::default()
    };
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
    world.burn(other, 100);
    let old = table(&world, &born, 2, None);
    module
        .prospective(1, Some(cgroup_scope(&world, &old, 100)), &old, 120_000, 0)
        .unwrap();
    world.burn(1, 10);
    world.burn(command, 10);
    world.exit(other, true);
    world.exit(command, true);
    let current = table(&world, &born, 3, None);
    module
        .history
        .record_observation(module.table_revision, 120_000, 0, 100);
    module
        .agents_history
        .record_observation(module.table_revision, 120_000, 0, 0);
    module
        .prospective(
            1,
            Some(cgroup_scope(&world, &current, 110)),
            &current,
            180_000,
            100,
        )
        .unwrap();
    module
        .history
        .record_observation(module.table_revision, 180_000, 0, 110);
    module
        .agents_history
        .record_observation(module.table_revision, 180_000, 0, 0);
    module.finish_at(
        AgentCommandRoot {
            pid: command,
            start_time_ms: 1,
        },
        Some(10),
        180_000,
    );
    assert_eq!(
        module.cpu_model.rows(),
        (110, 0, 110),
        "retired history without eligible timing cannot authorize a newer own-CPU transfer"
    );
    assert!(module.agents_lower);
    assert_eq!(module.history.last_hour_at(180_000).unwrap().cpu_ms, 10);
    assert_eq!(
        module.agents_history.last_hour_at(180_000).unwrap().cpu_ms,
        0
    );
}
