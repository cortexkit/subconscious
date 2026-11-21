use super::{
    apply_cgroup_placement, remove_module_cgroup, LaunchSpecDigest, ModuleProtocol, ModuleSpec,
    SuperviseError, SupervisedChild,
};
use crate::stderr_tail::{StderrRing, StderrTailConfig};
use cortexkit_test_support::ScratchDir;
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::process::Command;

#[tokio::test]
async fn unique_spawn_cgroups_clean_up_across_restarts_and_shutdown() {
    use super::*;
    let dir = ScratchDir::new("unique-spawn-cgroups");
    let root = PathBuf::from(format!(
        "/sys/fs/cgroup/subc-unique-test-{}-{}",
        std::process::id(),
        unix_ms_now()
    ));
    if let Err(error) = fs::create_dir(&root) {
        assert!(
            std::env::var_os("SUBC_REQUIRE_CGROUP_TEST").is_none(),
            "required cgroup test cannot execute: {error}"
        );
        eprintln!("SKIP unique_spawn_cgroups_clean_up_across_restarts_and_shutdown: {error}");
        return;
    }
    let placement = subc_cgroup::prepare_at(&root).unwrap().unwrap();
    let group_count = || {
        fs::read_dir(root.join("subc-modules"))
            .unwrap()
            .map(|entry| entry.unwrap().file_type().unwrap())
            .filter(|kind| kind.is_dir())
            .count()
    };
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
            .with_cgroup_placement(Some(placement.clone()));
    let runtime = supervisor.runtime_config();
    let mut spec = ModuleSpec {
        script: None,
        module_id: "unique-spawn".into(),
        program: PathBuf::from("/bin/sleep"),
        args: vec!["60".into()],
        env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.into(), dir.to_string_lossy().into_owned()))
            .collect(),
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    };
    let spawn = |spec: &ModuleSpec| {
        spawn_child(
            spec,
            None,
            None,
            &runtime.stderr_ring,
            None,
            &runtime.child_roster,
            runtime.spawn_delivery(),
            Some(&placement),
            runtime.register_timeout,
        )
        .unwrap()
    };
    let mut live = spawn(&spec);
    for _ in 0..3 {
        // A new process can enter the old slot while retirement is pending.
        let next = spawn(&spec);
        assert_ne!(live.module_id, next.module_id);
        live.start_kill().unwrap();
        live.wait().await.unwrap();
        live.finish_exit(false).await.unwrap();
        live = next;
        assert!(
            live.child.try_wait().unwrap().is_none(),
            "retiring the old slot must not kill the replacement"
        );
        assert_eq!(
            group_count(),
            1,
            "only the live spawn's cgroup should remain"
        );
    }
    supervisor.begin_daemon_shutdown();
    let reap = tokio::spawn(async move {
        live.wait().await.unwrap();
        live.finish_exit(false).await.unwrap();
    });
    supervisor
        .end_children_for_daemon_shutdown(false, std::future::pending())
        .await;
    reap.await.unwrap();
    assert_eq!(group_count(), 0);
    // A normal exit uses the same tree-cleanup path as a killed spawn.
    spec.program = PathBuf::from("/bin/true");
    spec.args.clear();
    let fresh_roster = ChildRoster::default();
    let mut short = spawn_child(
        &spec,
        None,
        None,
        &runtime.stderr_ring,
        None,
        &fresh_roster,
        None,
        Some(&placement),
        runtime.register_timeout,
    )
    .unwrap();
    short.wait().await.unwrap();
    short.finish_exit(false).await.unwrap();
    assert_eq!(group_count(), 0);
    spec.module_id = "_".repeat(255);
    let mut long_id = spawn_child(
        &spec,
        None,
        None,
        &runtime.stderr_ring,
        None,
        &fresh_roster,
        None,
        Some(&placement),
        runtime.register_timeout,
    )
    .unwrap();
    long_id.wait().await.unwrap();
    long_id.finish_exit(false).await.unwrap();
    assert_eq!(
        group_count(),
        0,
        "valid long module IDs must not exceed cgroup NAME_MAX"
    );
    fs::remove_dir(root.join("subc-modules")).unwrap();
    fs::remove_dir(root).unwrap();
    eprintln!("EXECUTED unique_spawn_cgroups_clean_up_across_restarts_and_shutdown");
}

#[test]
fn failed_parent_cgroup_open_is_a_cgroup_supervision_error() {
    let path = Path::new("/definitely-missing-subc-cgroup");
    let mut command = Command::new("true");
    let error = apply_cgroup_placement(
        &mut command,
        &ModuleSpec {
            script: None,
            module_id: "broken-cgroup".to_string(),
            program: PathBuf::from("true"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
            carry_containment: false,
        },
        path,
    )
    .expect_err("a parent cgroup open failure must reject the supervised spawn");
    let reason = error.to_string();

    assert!(
        matches!(error, SuperviseError::Cgroup { .. }),
        "parent cgroup open must be reported as a cgroup supervision error: {reason}"
    );
    assert!(
        reason.contains("/definitely-missing-subc-cgroup/cgroup.procs"),
        "parent cgroup open failure must name cgroup.procs: {reason}"
    );
}

#[tokio::test]
async fn reaping_a_child_does_not_treat_missing_cgroup_events_as_empty() {
    let root = ScratchDir::new("supervisor-reap-cgroup");
    fs::write(root.join("cgroup.procs"), b"").expect("write scratch cgroup marker");
    let placement = subc_cgroup::prepare_at(&root)
        .expect("prepare scratch cgroup root")
        .expect("scratch root has a cgroup.procs marker");
    let module_id = "reaped-module";
    let module = placement
        .module_path(module_id)
        .expect("create scratch module cgroup");
    let child = Command::new("true")
        .env("XDG_DATA_HOME", root.path())
        .env("XDG_RUNTIME_DIR", root.path())
        .env("XDG_CONFIG_HOME", root.path())
        .spawn()
        .expect("spawn short-lived child");
    let pid = child.id().expect("spawned child has pid");
    let mut child = SupervisedChild {
        registration_channel: None,
        registration_spawn: None,
        live_program_binding: None,
        registered_config_connection: None,
        admission_base: None,
        registration_declaration: None,
        _program_binding: None,
        subtree_target: crate::subtree_resources::Target::Cgroup(module.clone()),
        child: crate::supervise::containment::ProcessChild::plain(child, pid),
        exec: crate::activity::ExecOwner(Arc::new(crate::activity::ExecOutcome::new())),
        protocol: ModuleProtocol::Subc,
        module_id: module_id.to_string(),
        cgroup_placement: Some(placement),
        unresolved_cgroup: None,
        stdout_pump: None,
        stderr_pump: None,
        capture_stop: None,
        late_output_lost: false,
        stderr_ring: Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default()))),
        spawned_at: tokio::time::Instant::now(),
        config_delivery: None,
        registration_deadline: None,
        registration_watch: None,
        spawned_at_ms: 0,
        spawned_from: PathBuf::from("true"),
        launch_spec_digest: LaunchSpecDigest([0; 32]),
        spawned_file_identity: None,
        process_start_time: None,
        process_identity: None,
        pid,
        roster_guard: None,
        #[cfg(target_os = "macos")]
        privacy_exec: None,
        spawn_failure: None,
        admission_kill_sent: false,
    };

    child.wait().await.expect("reap short-lived child");

    child.cleanup_ordinary_cgroup().await;
    assert!(module.is_dir());
    assert!(child
        .exit_cleanup_detail(None)
        .unwrap()
        .contains(&module.display().to_string()));
    child.finish_exit(false).await.unwrap();
}

#[test]
fn non_empty_cgroup_removal_is_reported_without_blocking_teardown() {
    let root = ScratchDir::new("supervisor-non-empty-cgroup");
    fs::write(root.join("cgroup.procs"), b"").expect("write scratch cgroup marker");
    let placement = subc_cgroup::prepare_at(&root)
        .expect("prepare scratch cgroup root")
        .expect("scratch root has a cgroup.procs marker");
    let module = placement
        .module_path("surviving-module")
        .expect("create scratch module cgroup");
    fs::write(module.join("surviving-process"), b"still present")
        .expect("make scratch cgroup non-empty");
    let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::WARN);

    remove_module_cgroup(&placement, "surviving-module");

    let logs = crate::router::test_log::captured_logs(&logs);
    assert!(
        module.exists(),
        "failed removal must leave the cgroup intact"
    );
    assert!(
        logs.contains("could not remove module cgroup after process exit; continuing teardown")
            && logs.contains("surviving-module"),
        "best-effort removal must report the failure without returning it: {logs}"
    );
}

#[test]
fn cgroup_pre_exec_spawn_failure_names_the_cgroup_path() {
    let cgroup_path = PathBuf::from("/sys/fs/cgroup/subc-modules/broken-module");
    let reason = SuperviseError::Spawn {
        program: PathBuf::from("/bin/true"),
        source: io::Error::from_raw_os_error(13),
        cgroup_path: Some(cgroup_path.clone()),
    }
    .to_string();

    assert!(
        reason.contains(&cgroup_path.display().to_string()),
        "a pre_exec spawn failure must name the cgroup path: {reason}"
    );
}
