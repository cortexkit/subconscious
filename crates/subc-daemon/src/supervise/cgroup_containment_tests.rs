use super::*;
use cortexkit_test_support::ScratchDir;

#[tokio::test]
async fn linux_subtree_memory_covers_module_children() {
    let dir = ScratchDir::new("subtree-memory");
    let root = PathBuf::from(format!(
        "/sys/fs/cgroup/subc-memory-test-{}-{}",
        std::process::id(),
        unix_ms_now()
    ));
    if let Err(error) = std::fs::create_dir(&root) {
        assert!(
            std::env::var_os("SUBC_REQUIRE_CGROUP_TEST").is_none(),
            "required cgroup test cannot execute: {error}"
        );
        eprintln!("SKIP linux_subtree_memory_covers_module_children: {error}");
        return;
    }
    let placement = subc_cgroup::prepare_at(&root)
        .unwrap()
        .expect("delegated cgroup");
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(3, Duration::ZERO),
    )
    .with_cgroup_placement(Some(placement));
    let ready = dir.join("ready");
    let module = supervisor
        .spawn(ModuleSpec {
            script: None,
            module_id: "memory-tree".into(),
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".into(),
                "sleep 600 & sleep 600 & sleep 600 & echo ready > \"$1\"; wait".into(),
                "fixture".into(),
                ready.display().to_string(),
            ],
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| (key.into(), dir.display().to_string()))
                .collect(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
            carry_containment: false,
        })
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "children never started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let readings = crate::subtree_resources::read(vec![module.subtree_target()]).await;
    let reading = &readings[0];
    assert!(
        reading.process_count.is_some_and(|count| count >= 4),
        "{reading:?}"
    );
    assert!(
        reading.memory_bytes.is_some_and(|bytes| bytes > 0),
        "{reading:?}"
    );
    module.stop().await.unwrap();
    drop(module);
    std::fs::remove_dir(root.join("subc-modules")).unwrap();
    std::fs::remove_dir(&root).unwrap();
    eprintln!("EXECUTED linux_subtree_memory_covers_module_children: {reading:?}");
}

fn running(pid: u32) -> bool {
    // An orphan can remain a zombie until the container init reaps it.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(") ")
                .map(|(_, rest)| rest.starts_with('Z'))
        })
        .is_some_and(|zombie| !zombie)
}

#[tokio::test]
async fn linux_teardown_reaps_the_grandchild() {
    teardown_tree("linux_teardown_reaps_the_grandchild", false).await;
}

#[tokio::test]
async fn linux_shutdown_straggler_reaps_the_grandchild() {
    teardown_tree("linux_shutdown_straggler_reaps_the_grandchild", true).await;
}

async fn teardown_tree(test_name: &str, shutdown: bool) {
    let dir = ScratchDir::new(test_name);
    let root = PathBuf::from(format!(
        "/sys/fs/cgroup/subc-tree-test-{test_name}-{}-{}",
        std::process::id(),
        unix_ms_now()
    ));
    if let Err(error) = std::fs::create_dir(&root) {
        assert!(
            std::env::var_os("SUBC_REQUIRE_CGROUP_TEST").is_none(),
            "required cgroup test cannot execute: {error}"
        );
        eprintln!(
            "SKIP {test_name}: no writable delegated cgroup at {}: {error}",
            root.display()
        );
        return;
    }
    let placement = subc_cgroup::prepare_at(&root)
        .expect("prepare isolated kernel cgroup")
        .expect("isolated cgroup is delegated");
    let module_id = "tree-teardown";
    let module = placement
        .module_path(module_id)
        .expect("create isolated module cgroup");
    if !module.join("cgroup.kill").exists() {
        std::fs::remove_dir(&module).unwrap();
        std::fs::remove_dir(root.join("subc-modules")).unwrap();
        std::fs::remove_dir(&root).unwrap();
        assert!(
            std::env::var_os("SUBC_REQUIRE_CGROUP_TEST").is_none(),
            "required cgroup.kill interface unavailable"
        );
        eprintln!("SKIP {test_name}: cgroup.kill unavailable (kernel < 5.14)");
        return;
    }
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(3, Duration::ZERO),
    )
    .with_cgroup_placement(Some(placement));
    let mut runtime = supervisor.runtime_config();
    runtime.child_roster = runtime
        .child_roster
        .for_module(Arc::new(Mutex::new(Duration::from_millis(100))));
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let pid_file = dir.join("grandchild.pid");
    let spec = ModuleSpec {
        script: None,
        module_id: module_id.to_string(),
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".into(),
            "trap '' TERM; sleep 600 & echo $! > \"$1\"; wait".into(),
            "fixture".into(),
            pid_file.display().to_string(),
        ],
        env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.to_string(), dir.display().to_string()))
            .collect(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    };
    let child = spawn_and_mark_running(&spec, &runtime, &snapshot).expect("spawn supervised tree");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let grandchild: u32 = loop {
        if let Ok(contents) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = contents.trim().parse() {
                break pid;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "grandchild pid was not recorded"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(
        running(grandchild),
        "grandchild must be alive before teardown"
    );
    if shutdown {
        let mut child = child;
        crate::child_roster::end_children_for_daemon_shutdown(
            &runtime.child_roster,
            false,
            std::future::pending(),
        )
        .await;
        child.wait().await.expect("reap shutdown straggler");
        child
            .finish_exit(false)
            .await
            .expect("finalize shutdown straggler's cgroup");
    } else {
        drain_child_to_state(
            module_id,
            ModuleProtocol::None,
            StopNotice::NotSent,
            &Registry::default(),
            None,
            &snapshot,
            &runtime.terminal_ring,
            &SpawnEventFeed::default(),
            child,
            Duration::from_millis(100),
            ModuleState::Stopped,
            Some(false),
            &FailureContext::operator(),
        )
        .await
        .expect("real supervisor teardown");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while running(grandchild) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let survived = running(grandchild);
    // Kill a surviving grandchild so a failed test does not leave it behind.
    if survived {
        let pid = rustix::process::Pid::from_raw(grandchild as i32).unwrap();
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if module.exists() {
        std::fs::remove_dir(&module).expect("remove empty module cgroup");
    }
    std::fs::remove_dir(root.join("subc-modules")).unwrap();
    std::fs::remove_dir(&root).unwrap();
    assert!(
        !survived,
        "grandchild {grandchild} outlived module teardown"
    );
    eprintln!("EXECUTED {test_name}: grandchild {grandchild} killed in isolated cgroup");
}
