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
    Incomplete,
}

pub(crate) struct Prepared {
    pub scope_target: crate::subtree_resources::Target,
    pub module_pid: u32,
    pub before_cpu: Option<(u128, u64)>,
    pub started: Vec<(AgentCommandRoot, Option<VerifiedRoot>)>,
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
    let before_cpu = snapshot.before_cpu;
    let table = snapshot.table;
    #[cfg(unix)]
    let scope = snapshot.scope;
    let validate = |root: AgentCommandRoot, resync| {
        let native = trusted.iter().find_map(|(known, start)| {
            (known.pid == root.pid
                && known.start_time_ms.abs_diff(root.start_time_ms)
                    < subc_os::process_start_time_precision_ms())
            .then_some(*start)
        });
        if resync && native.is_none() {
            Validation::Invalid
        } else {
            validate_root(
                module_pid,
                &target,
                root,
                &table,
                resync.then_some(native).flatten(),
            )
        }
    };
    let mut pending = Vec::new();
    let started: Vec<_> = batch
        .started
        .into_iter()
        .filter_map(|root| match validate(root, false) {
            Validation::Valid(verified) => Some((root, Some(verified))),
            Validation::Invalid => Some((root, None)),
            Validation::Incomplete => {
                pending.push(root);
                None
            }
        })
        .collect();
    let known = match batch.known {
        Some(roots) => Some(
            roots
                .into_iter()
                .map(|root| {
                    let accepted_start = started.iter().find_map(|(started, verified)| {
                        (started.pid == root.pid
                            && started.start_time_ms.abs_diff(root.start_time_ms)
                                < subc_os::process_start_time_precision_ms())
                        .then(|| verified.clone())
                        .flatten()
                    });
                    Some((
                        root,
                        if accepted_start.is_some() {
                            accepted_start
                        } else {
                            match validate(root, true) {
                                Validation::Valid(verified) => Some(verified),
                                Validation::Invalid | Validation::Incomplete => None,
                            }
                        },
                    ))
                })
                .collect::<Option<_>>()?,
        ),
        None => None,
    };
    Some(Prepared {
        scope_target: target,
        module_pid,
        before_cpu,
        started,
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
    trusted_start: Option<u64>,
) -> Validation {
    if root.pid == module_pid || is_ancestor(table, root.pid, module_pid) {
        return Validation::Invalid;
    }
    let process = match subc_os::Process::open(root.pid) {
        Ok(Some(process)) => process,
        Ok(None) => return Validation::Invalid,
        Err(_) => return Validation::Incomplete,
    };
    let Some(observed) = process.observe() else {
        return Validation::Incomplete;
    };
    if trusted_start.is_some_and(|start| start != observed.start_time) {
        return Validation::Invalid;
    }
    let Some(start_time_ms) = subc_os::process_start_time_ms(root.pid) else {
        return Validation::Incomplete;
    };
    if start_time_ms.abs_diff(root.start_time_ms) >= subc_os::process_start_time_precision_ms() {
        return Validation::Invalid;
    }
    let direct_child = match subc_os::parent_pid(root.pid) {
        Some(parent) => parent == module_pid,
        None if trusted_start.is_some() => false,
        None => return Validation::Incomplete,
    };
    if !direct_child && trusted_start.is_none() {
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
        None if trusted_start.is_some() => BTreeSet::from([(root.pid, observed.start_time)]),
        None => return Validation::Incomplete,
    };
    Validation::Valid(VerifiedRoot {
        root: AgentCommandRoot {
            pid: root.pid,
            start_time_ms,
        },
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
