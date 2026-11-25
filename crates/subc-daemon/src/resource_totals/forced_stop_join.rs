use super::*;
use std::os::windows::process::CommandExt as _;

#[test]
fn windows_forced_stop_membership_join() {
    let mut cmd = ProcessCommand::new("cmd.exe");
    cmd.args(["/D", "/Q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    let job_child = spawn_detached_job_fixture(&mut cmd);
    let job = Arc::new(Mutex::new(job_child.job));
    let root = job_child.child;
    let target = crate::subtree_resources::Target::Job(Arc::downgrade(&job));
    let module_pid = std::process::id();
    let launch = crate::supervise::ResourceLaunch {
        pid: Some(module_pid),
        start: subc_os::start_time(module_pid),
        spawn_generation: 1,
        enabled: true,
        alive: true,
        target: target.clone(),
    };
    let tracker = Tracker::default();
    let identity = AgentCommandRoot {
        pid: root.id(),
        start_time_ms: subc_os::process_start_time_ms(root.id()).unwrap(),
    };
    let started = tracker.declare(
        "forced-stop",
        module_pid,
        launch.generation(),
        target.clone(),
        Declaration {
            started: vec![identity],
            ..Declaration::default()
        },
    );
    assert!(started.rejected.is_empty());
    assert!(tracker.state.lock().unwrap().modules["forced-stop"]
        .roots
        .contains_key(&identity));
    let stop = tracker
        .prepare(
            "forced-stop",
            launch.generation(),
            module_pid,
            target.clone(),
            Declaration {
                known: Some(Vec::new()),
                ..Declaration::default()
            },
            &mut Receipt::default(),
        )
        .unwrap();
    assert!(tracker
        .commit_fenced(
            "forced-stop",
            launch.generation(),
            stop,
            &CommitFence::default()
        )
        .is_some());
    assert!(
        tracker.state.lock().unwrap().modules["forced-stop"].roots[&identity].attribution_stopped
    );

    // Make the native job's PID list strictly newer than the process snapshot.
    // Keeping the new process suspended isolates membership from CPU accounting.
    let stale = ProcessTable::read().unwrap();
    let mut entrant = ProcessCommand::new(std::env::current_exe().unwrap())
        .creation_flags(DETACHED_PROCESS | subc_jobobject::CREATE_SUSPENDED_FLAG)
        .spawn()
        .unwrap();
    let new_pid = entrant.id();
    let assigned = job.lock().unwrap().assign(&entrant);
    if let Err(error) = assigned {
        let _ = entrant.kill();
        let _ = entrant.wait();
        panic!("failed to assign the suspended fixture: {error}");
    }
    assert!(stale.process(new_pid).is_none());
    assert!(target.process_ids().unwrap().contains(&new_pid));
    assert!(target.tree(&stale).is_none());
    let read = |table| {
        tracker.read_observed(
            BTreeMap::from([("forced-stop".into(), Some(launch.clone()))]),
            Arc::new(table),
            false,
            now_ms(),
            Instant::now() + Duration::from_secs(5),
        )
    };
    let stopped = read(stale);
    assert_eq!(stopped.reasons.get("forced-stop"), Some(&"unreadable"));
    assert!(stopped.agent_commands.is_none());
    entrant.kill().unwrap();
    entrant.wait().unwrap();
    drop(entrant);
    let stopped = read(windows_fixture_table(&target, &[root.id()]));
    let stopped_agents = &stopped.agent_commands.as_ref().unwrap()[0];
    assert_eq!(stopped_agents.process_count, 0);
    assert!(stopped_agents.groups.is_empty());
}
