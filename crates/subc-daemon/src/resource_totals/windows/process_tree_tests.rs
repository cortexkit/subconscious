use super::*;

const MODULE: (u32, u64) = (1, 1);
const ROOT: (u32, u64) = (2, 2);
const CHILD: (u32, u64) = (3, 3);
const COMMAND: AgentCommandRoot = AgentCommandRoot {
    pid: ROOT.0,
    start_time_ms: ROOT.1,
};

fn snapshot(revision: u64, root: Option<u64>, child: Option<u64>) -> ProcessTable {
    // The native observation is injected separately; this handle is never sampled.
    let process = Arc::new(subc_os::Process::open(std::process::id()).unwrap().unwrap());
    let members = [
        (MODULE, 0, Some(1_000)),
        (ROOT, MODULE.0, root),
        (CHILD, ROOT.0, child),
    ]
    .into_iter()
    .filter_map(|((pid, start), parent_pid, cpu)| {
        let cpu = cpu?;
        Some(subc_os::ProcessMember {
            pid,
            parent_pid,
            start_time: start,
            start_time_ms: start,
            session_id: None,
            name: format!("process-{pid}"),
            resources: Some(subc_os::ResourceUsage {
                cpu_user: Duration::from_millis(cpu),
                cpu_system: Duration::ZERO,
                memory_bytes: 10,
                memory_kind: subc_os::MemoryKind::ResidentSet,
                swap_bytes: None,
            }),
            own_cpu_ms: Some(cpu),
            reaped_cpu_ms: Some(0),
            process: Arc::clone(&process),
        })
    })
    .collect();
    ProcessTable::from_test_members(revision, 1_000 + revision, members, Vec::new())
}

fn read(module: &mut Module, table: &ProcessTable) -> u64 {
    let usage = table.tree(MODULE.0, MODULE.1, false).unwrap();
    let identities = usage
        .members
        .iter()
        .map(|m| (m.pid, m.start_time))
        .collect();
    module
        .prospective(
            1,
            Some((wire(usage, TreeSource::ProcessTree), identities)),
            table,
            table.observed_at_ms(),
            0,
        )
        .unwrap()
        .1
        .cpu_ms
}

fn sample(
    module: &mut Module,
    table: &ProcessTable,
    cpu: &[((u32, u64), Option<u64>)],
    exited: &[(u32, u64)],
) {
    let handles = cpu
        .iter()
        .filter(|(identity, _)| !exited.contains(identity))
        .map(|(identity, _)| {
            (
                *identity,
                Arc::clone(&table.process(identity.0).unwrap().process),
            )
        })
        .collect();
    module.roots.get_mut(&COMMAND).unwrap().apply_windows(
        COMMAND,
        Observation {
            cpu: cpu.to_vec(),
            exited: exited.iter().copied().collect(),
            handles,
            exited_at: exited.iter().map(|&id| (id, Instant::now())).collect(),
        },
        true,
    );
}

fn verify_child_exit(snapshot_child_cpu: Option<u64>) {
    let baseline = snapshot(1, Some(15), Some(250));
    let mut command = Command {
        native_start: ROOT.1,
        members: BTreeSet::from([ROOT, CHILD]),
        fixed_outside: BTreeSet::from([ROOT]),
        ..Command::default()
    };
    command.baseline(COMMAND, &baseline);
    let mut module = Module {
        roots: BTreeMap::from([(COMMAND, command)]),
        ..Module::default()
    };
    sample(
        &mut module,
        &baseline,
        &[(ROOT, Some(15)), (CHILD, Some(250))],
        &[],
    );
    assert_eq!(read(&mut module, &baseline), 0);

    let before = snapshot(2, Some(15), Some(406));
    sample(
        &mut module,
        &before,
        &[(ROOT, Some(15)), (CHILD, Some(406))],
        &[],
    );
    assert_eq!(read(&mut module, &before), 156);

    // The held handle returns 421 after the exit probe, whether the earlier
    // process-table walk saw the child at 414 or missed it entirely.
    let after = snapshot(3, Some(15), snapshot_child_cpu);
    sample(
        &mut module,
        &after,
        &[(ROOT, Some(15)), (CHILD, Some(421))],
        &[CHILD],
    );
    read(&mut module, &after);

    let root_exited = snapshot(4, None, None);
    sample(&mut module, &root_exited, &[(ROOT, Some(31))], &[ROOT]);
    read(&mut module, &root_exited);
    module.finish_at(COMMAND, Some(15), root_exited.observed_at_ms());
    let finished = snapshot(5, None, None);
    assert_eq!(
        read(&mut module, &finished),
        187,
        "the child's full final tail belongs to the command"
    );
    assert_eq!(
        module.cpu_model.rows(),
        (1_265, 187, 1_452),
        "a live sample of the exiting child must not increase the combined CPU twice"
    );
}

#[test]
fn process_tree_credits_the_reaped_childs_final_cpu() {
    verify_child_exit(None);
}

#[test]
fn process_tree_counts_a_child_once_when_it_exits_after_the_snapshot() {
    verify_child_exit(Some(414));
}
