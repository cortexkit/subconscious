use super::*;

#[derive(Clone)]
pub(crate) struct VerifiedRoot {
    pub root: AgentCommandRoot,
    pub native_start: u64,
    pub inside: bool,
    pub members: BTreeSet<(u32, u64)>,
    #[cfg(windows)]
    pub process: Option<Arc<subc_os::Process>>,
}

pub(super) enum Validation {
    Valid(VerifiedRoot),
    Invalid,
    Exited,
    Incomplete,
}

pub(crate) struct Prepared {
    pub scope_target: crate::subtree_resources::Target,
    pub module_pid: u32,
    pub before_cpu: Option<(u128, u64)>,
    pub started: Vec<(AgentCommandRoot, Option<VerifiedRoot>)>,
    pub exited_before_admission: Vec<AgentCommandRoot>,
    pub pending: Vec<AgentCommandRoot>,
    pub finished: Vec<AgentCommandFinished>,
    pub known: Option<Vec<(AgentCommandRoot, Option<VerifiedRoot>)>>,
    pub table: ProcessTable,
    pub at_ms: u64,
    pub finish_until: u64,
    #[cfg(unix)]
    pub scope: Option<(TreeReading, BTreeSet<(u32, u64)>)>,
}

pub(super) struct Snapshot {
    pub(super) before_cpu: Option<(u128, u64)>,
    pub(super) table: ProcessTable,
    #[cfg(unix)]
    pub(super) scope: Option<(TreeReading, BTreeSet<(u32, u64)>)>,
}

impl Snapshot {
    #[cfg(test)]
    pub(super) fn read(target: &crate::subtree_resources::Target) -> Option<Self> {
        Self::read_with(target, || ProcessTable::read().ok())
    }

    pub(super) fn read_with(
        target: &crate::subtree_resources::Target,
        read_table: impl FnOnce() -> Option<ProcessTable>,
    ) -> Option<Self> {
        let before_cpu = target.counter_cpu();
        let table = read_table()?;
        #[cfg(unix)]
        let scope = target
            .has_container()
            .then(|| target.tree(&table))
            .flatten();
        Some(Self {
            before_cpu,
            table,
            #[cfg(unix)]
            scope,
        })
    }
}

#[cfg(test)]
pub(super) fn prepare(
    module_pid: u32,
    target: crate::subtree_resources::Target,
    batch: Declaration,
    trusted: &BTreeMap<AgentCommandRoot, u64>,
    finish_until: &mut Option<u64>,
) -> Option<Prepared> {
    let snapshot = Snapshot::read(&target)?;
    let finish_until = *finish_until.get_or_insert(snapshot.table.revision());
    prepare_snapshot(module_pid, target, batch, trusted, snapshot, finish_until)
}

pub(super) fn prepare_snapshot(
    module_pid: u32,
    target: crate::subtree_resources::Target,
    batch: Declaration,
    trusted: &BTreeMap<AgentCommandRoot, u64>,
    snapshot: Snapshot,
    finish_until: u64,
) -> Option<Prepared> {
    prepare_snapshot_with_epoch(
        module_pid,
        target,
        batch,
        trusted,
        snapshot,
        finish_until,
        subc_os::process_start_time_ms,
    )
}

fn prepare_snapshot_with_epoch(
    module_pid: u32,
    target: crate::subtree_resources::Target,
    batch: Declaration,
    trusted: &BTreeMap<AgentCommandRoot, u64>,
    snapshot: Snapshot,
    finish_until: u64,
    read_epoch: impl Fn(u32) -> Option<u64>,
) -> Option<Prepared> {
    let before_cpu = snapshot.before_cpu;
    let table = snapshot.table;
    #[cfg(unix)]
    let scope = snapshot.scope;
    let validate = |root, resync, trusted: &BTreeMap<AgentCommandRoot, u64>| {
        validate_root_with_epoch(
            module_pid,
            &target,
            root,
            &table,
            trusted,
            resync,
            &read_epoch,
        )
    };
    let mut pending = Vec::new();
    let mut exited_before_admission = Vec::new();
    let started: Vec<_> = batch
        .started
        .into_iter()
        .filter_map(|root| match validate(root, false, trusted) {
            Validation::Valid(verified) => Some((root, Some(verified))),
            Validation::Invalid => Some((root, None)),
            Validation::Exited => {
                exited_before_admission.push(root);
                None
            }
            Validation::Incomplete => {
                pending.push(root);
                None
            }
        })
        .collect();
    let known = batch.known.map(|roots| {
        let mut with_started = trusted.clone();
        with_started.extend(started.iter().filter_map(|(_, verified)| {
            let verified = verified.as_ref()?;
            Some((verified.root, verified.native_start))
        }));
        roots
            .into_iter()
            .map(|root| {
                let accepted_start = started.iter().find_map(|(requested, verified)| {
                    let verified = verified.as_ref()?;
                    (*requested == root || same_epoch(verified.root, root))
                        .then(|| verified.clone())
                });
                let verified =
                    accepted_start.or_else(|| match validate(root, true, &with_started) {
                        Validation::Valid(verified) => Some(verified),
                        Validation::Invalid | Validation::Exited | Validation::Incomplete => None,
                    });
                (root, verified)
            })
            .collect()
    });
    Some(Prepared {
        scope_target: target,
        module_pid,
        before_cpu,
        started,
        exited_before_admission,
        pending,
        finished: batch.finished,
        known,
        table,
        at_ms: now_ms(),
        finish_until,
        #[cfg(unix)]
        scope,
    })
}

/// Unknown ownership is not evidence of an invalid declaration. Keep starts
/// pending until a later snapshot can prove both identity and tree membership.
pub(super) fn validate_root(
    module_pid: u32,
    target: &crate::subtree_resources::Target,
    root: AgentCommandRoot,
    table: &ProcessTable,
    trusted: &BTreeMap<AgentCommandRoot, u64>,
) -> Validation {
    validate_root_with_epoch(
        module_pid,
        target,
        root,
        table,
        trusted,
        false,
        subc_os::process_start_time_ms,
    )
}

fn validate_root_with_epoch(
    module_pid: u32,
    target: &crate::subtree_resources::Target,
    root: AgentCommandRoot,
    table: &ProcessTable,
    trusted: &BTreeMap<AgentCommandRoot, u64>,
    resync: bool,
    read_epoch: impl FnOnce(u32) -> Option<u64>,
) -> Validation {
    if root.pid == module_pid || is_ancestor(table, root.pid, module_pid) {
        return Validation::Invalid;
    }
    let process = match subc_os::Process::open(root.pid) {
        Ok(Some(process)) => process,
        // A missing process cannot establish ownership. Its CPU stays with
        // the module; a later finish report cannot supply that proof.
        Ok(None) => return Validation::Exited,
        Err(_) => return Validation::Incomplete,
    };
    let Some(observed) = process.observe() else {
        return Validation::Incomplete;
    };
    let matching_native = trusted
        .iter()
        .find(|(known, start)| known.pid == root.pid && **start == observed.start_time);
    if matching_native.is_none() && (resync || trusted.keys().any(|known| same_epoch(*known, root)))
    {
        return Validation::Invalid;
    }
    let Some(start_time_ms) = read_epoch(root.pid) else {
        return Validation::Incomplete;
    };
    let fresh = AgentCommandRoot {
        pid: root.pid,
        start_time_ms,
    };
    if !same_epoch(fresh, root)
        && !matching_native.is_some_and(|(known, _)| same_epoch(*known, root))
    {
        return Validation::Invalid;
    }
    let direct_child = match subc_os::parent_pid(root.pid) {
        Some(parent) => parent == module_pid,
        None if resync => false,
        None => return Validation::Incomplete,
    };
    if !direct_child && !resync {
        return Validation::Invalid;
    }
    // Membership is read after identity validation, then the same native
    // identity is rechecked. Windows queries the retained handle itself.
    let Some(contained) = target.contains_identity(root.pid, observed.start_time, &process) else {
        return Validation::Incomplete;
    };
    match subc_os::start_time(root.pid) {
        Some(start) if start != observed.start_time => return Validation::Invalid,
        None => return Validation::Incomplete,
        Some(_) => {}
    }
    let members = match table.tree_identities(root.pid, observed.start_time, true) {
        Some(members) => members,
        None if resync => BTreeSet::from([(root.pid, observed.start_time)]),
        None => return Validation::Incomplete,
    };
    Validation::Valid(VerifiedRoot {
        root: matching_native.map_or(fresh, |(known, _)| *known),
        native_start: observed.start_time,
        inside: if target.has_container() {
            contained
        } else {
            direct_child && !cfg!(windows)
        },
        members,
        #[cfg(windows)]
        process: Some(Arc::new(process)),
    })
}

fn is_ancestor(table: &ProcessTable, candidate: u32, module_pid: u32) -> bool {
    let mut current = Some(module_pid);
    let mut seen = BTreeSet::new();
    while let Some(pid) = current.filter(|pid| seen.insert(*pid)) {
        if pid == candidate {
            return true;
        }
        current = table
            .process(pid)
            .and_then(|member| (member.parent_pid != 0).then_some(member.parent_pid));
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn root_gone_at_first_validation_is_not_rejected_or_credited() {
        let mut child = Child(
            std::process::Command::new("sh")
                .args(["-c", "exec sleep 30"])
                .spawn()
                .unwrap(),
        );
        let root = AgentCommandRoot {
            pid: child.0.id(),
            start_time_ms: subc_os::process_start_time_ms(child.0.id()).unwrap(),
        };
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(matches!(subc_os::Process::open(root.pid), Ok(None)));

        let tracker = Tracker::default();
        let target = crate::subtree_resources::Target::Unavailable("no_container");
        let declare = |batch| {
            let prepared = tracker
                .prepare(
                    "module",
                    1,
                    std::process::id(),
                    target.clone(),
                    batch,
                    &mut Receipt::default(),
                )
                .unwrap();
            tracker
                .commit_fenced("module", 1, prepared, &CommitFence::default())
                .unwrap()
        };
        let committed = declare(Declaration {
            started: vec![root],
            known: Some(vec![root]),
            ..Declaration::default()
        });
        assert!(
            committed.rejected.is_empty(),
            "exited root was rejected: {:?}",
            committed.rejected
        );
        assert_eq!(committed.exited_before_admission, vec![root]);

        declare(Declaration {
            finished: vec![AgentCommandFinished {
                pid: root.pid,
                start_time_ms: root.start_time_ms,
                cpu_ms: Some(1_000),
            }],
            ..Declaration::default()
        });
        let state = tracker.state.lock().unwrap();
        let module = &state.modules["module"];
        assert!(module.roots.is_empty());
        assert!(module.pending.is_empty());
        assert!(module.finished.is_empty());
        assert_eq!(module.cpu_model.rows().1, 0);
    }

    #[test]
    fn known_reconfirms_accepted_roots_but_never_admits_a_new_child() {
        let child = Child(
            std::process::Command::new("sh")
                .args(["-c", "exec sleep 30"])
                .spawn()
                .unwrap(),
        );
        let root = AgentCommandRoot {
            pid: child.0.id(),
            start_time_ms: subc_os::process_start_time_ms(child.0.id()).unwrap(),
        };
        let batch = || Declaration {
            started: Vec::new(),
            finished: Vec::new(),
            known: Some(vec![root]),
        };
        let target = crate::subtree_resources::Target::Unavailable("no_container");
        let unknown = prepare(
            std::process::id(),
            target.clone(),
            batch(),
            &BTreeMap::new(),
            &mut None,
        )
        .unwrap();
        assert!(
            unknown.known.unwrap()[0].1.is_none(),
            "known is not an alternative started call"
        );
        let trusted = prepare(
            std::process::id(),
            target.clone(),
            batch(),
            &BTreeMap::from([(root, subc_os::start_time(root.pid).unwrap())]),
            &mut None,
        )
        .unwrap();
        assert!(trusted.known.unwrap()[0].1.is_some());
        let reused = prepare(
            std::process::id(),
            target,
            batch(),
            &BTreeMap::from([(root, 0)]),
            &mut None,
        )
        .unwrap();
        assert!(
            reused.known.unwrap()[0].1.is_none(),
            "matching rounded wire time cannot admit a new native identity"
        );
    }

    #[cfg(target_os = "linux")]
    fn clock_step_case(use_fresh_epoch: bool, reused: bool) {
        let pid = std::process::id();
        let parent = subc_os::parent_pid(pid).unwrap();
        let table = ProcessTable::read_for(&BTreeSet::from([parent, pid])).unwrap();
        let process = table.process(pid).unwrap();
        let root = AgentCommandRoot {
            pid,
            start_time_ms: 1_000_000 + process.start_time * 10,
        };
        let fresh = AgentCommandRoot {
            start_time_ms: root.start_time_ms + 2_000,
            ..root
        };
        let snapshot = |revision, epoch| {
            let mut member = process.clone();
            member.start_time_ms = epoch;
            Snapshot {
                before_cpu: None,
                table: ProcessTable::from_test_members(
                    revision,
                    epoch + 10_000,
                    vec![table.process(parent).unwrap().clone(), member],
                    vec![],
                ),
                scope: None,
            }
        };
        let target = crate::subtree_resources::Target::Unavailable("no_container");
        let prepare = |batch, trusted: &_, seq, epoch| {
            prepare_snapshot_with_epoch(
                parent,
                target.clone(),
                batch,
                trusted,
                snapshot(seq, epoch),
                seq,
                |_| Some(epoch),
            )
            .unwrap()
        };
        let tracker = Tracker::default();
        let mut module = Module::default();
        let first = prepare(
            Declaration {
                started: vec![root],
                ..Default::default()
            },
            &BTreeMap::new(),
            1,
            root.start_time_ms,
        );
        assert!(tracker
            .apply_prepared(&mut module, 1, first)
            .rejected
            .is_empty());
        let native = &mut module.roots.get_mut(&root).unwrap().native_start;
        if reused {
            *native = native.saturating_sub(1);
        }
        let trusted = BTreeMap::from([(root, *native)]);
        let requested = if use_fresh_epoch { fresh } else { root };
        let after = prepare(
            Declaration {
                known: Some(vec![requested]),
                ..Default::default()
            },
            &trusted,
            2,
            fresh.start_time_ms,
        );
        let applied = tracker.apply_prepared(&mut module, 1, after);
        if reused {
            assert!(!applied.full_resync);
            assert!(!module.roots.values().any(|root| root.live));
            return;
        }
        assert!(applied.full_resync);
        assert!(applied.rejected.is_empty());
        assert_eq!(module.roots.len(), 1);
        assert!(module.roots[&root].live);

        let repeated_start = prepare(
            Declaration {
                started: vec![requested],
                ..Default::default()
            },
            &trusted,
            3,
            fresh.start_time_ms,
        );
        assert!(repeated_start.started[0].1.is_some());
        assert!(tracker
            .apply_prepared(&mut module, 1, repeated_start)
            .rejected
            .is_empty());
        assert_eq!(module.roots.len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn known_keeps_attribution_after_a_clock_step_with_original_epoch() {
        clock_step_case(false, false);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn known_keeps_attribution_after_a_clock_step_with_new_projection() {
        clock_step_case(true, false);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn known_refuses_a_reused_pid_after_a_clock_step() {
        clock_step_case(true, true);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finish_matches_native_birth_and_either_epoch_after_a_clock_step() {
        let root = AgentCommandRoot {
            pid: 42,
            start_time_ms: 1_000_050,
        };
        let stepped = AgentCommandRoot {
            start_time_ms: 1_002_050,
            ..root
        };
        let sample = |native| {
            ProcessTable::from_test_members(
                1,
                1_002_100,
                vec![subc_os::ProcessMember {
                    pid: root.pid,
                    parent_pid: 1,
                    session_id: None,
                    start_time: native,
                    start_time_ms: stepped.start_time_ms,
                    name: "worker".into(),
                    resources: None,
                    own_cpu_ms: None,
                    reaped_cpu_ms: None,
                }],
                vec![],
            )
        };
        let mut module = Module::default();
        module.roots.insert(
            root,
            Command {
                native_start: 5,
                live: true,
                ..Command::default()
            },
        );
        let live = sample(5);
        assert_eq!(module.finish_key(stepped, &live), Some(root));
        assert_eq!(module.finish_key(root, &live), Some(root));
        let reused = sample(6);
        assert_eq!(module.finish_key(root, &reused), None);
        assert_eq!(module.finish_key(stepped, &reused), None);
        let unreadable =
            ProcessTable::from_test_members(2, 1_002_110, vec![], vec![(root.pid, None, None)]);
        assert_eq!(module.finish_key(root, &unreadable), None);
        let gone = ProcessTable::from_test_members(3, 1_002_120, vec![], vec![]);
        assert_eq!(module.finish_key(root, &gone), Some(root));
        assert_eq!(module.finish_key(stepped, &gone), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn known_cannot_launder_the_module_even_when_the_container_contains_it() {
        let pid = std::process::id();
        let root = AgentCommandRoot {
            pid,
            start_time_ms: subc_os::process_start_time_ms(pid).unwrap(),
        };
        let prepared = prepare(
            pid,
            crate::subtree_resources::Target::Cgroup("/sys/fs/cgroup".into()),
            Declaration {
                started: Vec::new(),
                finished: Vec::new(),
                known: Some(vec![root]),
            },
            &BTreeMap::from([(root, subc_os::start_time(root.pid).unwrap())]),
            &mut None,
        )
        .unwrap();
        assert!(prepared.known.unwrap()[0].1.is_none());
    }
}
