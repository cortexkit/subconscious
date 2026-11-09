use super::*;
use std::path::Path;

fn fifo_at(path: &Path) {
    std::fs::remove_file(path).unwrap();
    assert!(std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .unwrap()
        .success());
}

#[tokio::test]
async fn config_spawn_async_fifo_executable_refuses_without_stranding_the_gate() {
    let (_rig, owner, supervisor, _handler, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let roster = supervisor.child_roster.clone();
    let program = spec.program.clone();
    let gate = owner.module_gate(&spec.module_id).unwrap();
    fifo_at(&program);
    let mut launch = tokio::spawn(async move { supervisor.spawn_async(spec).await });
    let prompt = tokio::time::timeout(Duration::from_millis(250), &mut launch).await;
    let refused_promptly =
        matches!(&prompt, Ok(Ok(Err(error))) if error.to_string().contains("regular"));
    if prompt.is_err() {
        launch.abort();
        let _ = launch.await;
        // Rescue only the broken implementation, so the red test cannot leave
        // an uninterruptible blocking worker behind when the runtime shuts down.
        let _rescue = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&program)
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), gate.lock())
            .await
            .unwrap();
    }
    assert_eq!(roster.live_count_for_test(), 0);
    assert_eq!(owner.unregistered_spawn_count_for_test(), 0);
    assert!(gate.try_lock().is_ok());
    assert!(
        refused_promptly,
        "a FIFO executable must refuse as non-regular without blocking its module gate"
    );
}

#[tokio::test]
async fn config_spawn_async_cancel_at_fifo_executable_leaves_no_child_or_gate() {
    let (_rig, owner, supervisor, _handler, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let roster = supervisor.child_roster.clone();
    let program = spec.program.clone();
    let gate = owner.module_gate(&spec.module_id).unwrap();
    fifo_at(&program);
    let launch = tokio::spawn(async move { supervisor.spawn_async(spec).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !launch.is_finished() && gate.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    launch.abort();
    let _ = launch.await;
    let free = tokio::time::timeout(Duration::from_millis(250), gate.lock()).await;
    let not_stranded = free.is_ok();
    drop(free);
    if !not_stranded {
        let _rescue = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&program)
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), gate.lock())
            .await
            .unwrap();
    }
    assert_eq!(roster.live_count_for_test(), 0);
    assert_eq!(owner.unregistered_spawn_count_for_test(), 0);
    assert!(
        not_stranded,
        "cancelling FIFO preflight must not strand a non-cancellable worker holding the gate"
    );
}

#[test]
fn config_spawn_path_skips_a_non_executable_regular_file() {
    use std::os::unix::fs::PermissionsExt;
    let root = cortexkit_test_support::ScratchDir::new("spawn-path-permission");
    let early = root.join("early");
    let later = root.join("later");
    std::fs::create_dir(&early).unwrap();
    std::fs::create_dir(&later).unwrap();
    std::fs::write(early.join("command"), b"not executable").unwrap();
    std::fs::set_permissions(
        early.join("command"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::copy("/bin/sh", later.join("command")).unwrap();
    let spec = ModuleSpec {
        module_id: "path-probe".into(),
        program: "command".into(),
        args: vec![],
        env: vec![(
            "PATH".into(),
            std::env::join_paths([&early, &later])
                .unwrap()
                .into_string()
                .unwrap(),
        )],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    };
    assert_eq!(
        SpawnProgramBinding::capture(&spec).unwrap().path,
        later.join("command")
    );
}

#[test]
fn config_spawn_resolved_target_swapped_to_a_symlink_is_refused() {
    let (_rig, _owner, _supervisor, _handler, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let result = SpawnProgramBinding::capture_after_resolution(&spec, |resolved| {
        std::fs::remove_file(resolved).unwrap();
        std::os::unix::fs::symlink("/bin/sh", resolved).unwrap();
    });
    assert!(
        result.is_err(),
        "no-follow descriptor capture must refuse a post-resolution symlink substitution"
    );
}

#[tokio::test]
async fn config_spawn_symlinked_executable_keeps_the_original_launch_name() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = cortexkit_test_support::ScratchDir::new("spawn-symlink-name");
    let target = root.join("target");
    let alias = root.join("alias");
    let observed = root.join("observed");
    std::fs::write(
        &target,
        b"#!/bin/sh\nprintf '%s' \"$0\" > \"$1\"\nexec /bin/sleep 600\n",
    )
    .unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&target, &alias).unwrap();
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let module = supervisor
        .spawn_async(ModuleSpec {
            module_id: "symlink-probe".into(),
            program: alias.clone(),
            args: vec![observed.to_string_lossy().into_owned()],
            env: vec![],
            reserved: false,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
            carry_containment: false,
        })
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(name) = std::fs::read_to_string(&observed) {
                if !name.is_empty() {
                    break name;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    module.stop().await.unwrap();
    assert_eq!(result.unwrap(), alias.to_string_lossy());
}

#[tokio::test]
async fn config_spawn_admission_refusal_removes_its_prepared_cgroup() {
    let (_rig, _owner, mut supervisor, _handler, mut spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let root = cortexkit_test_support::ScratchDir::new("spawn-admission-cgroup");
    std::fs::write(root.join("cgroup.procs"), b"").unwrap();
    let placement = subc_cgroup::prepare_at(&root).unwrap().unwrap();
    let probe = placement.module_path("probe").unwrap();
    let modules = probe.parent().unwrap().to_path_buf();
    placement.remove_module("probe").unwrap();
    supervisor.cgroup_placement = Some(placement);
    spec.carry_containment = true;
    let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = seen.clone();
    supervisor
        .child_roster
        .set_admission_hook_for_test(move |_, stage, _| {
            if stage == "before_permit" {
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                false
            } else {
                true
            }
        });
    let roster = supervisor.child_roster.clone();
    assert!(supervisor.spawn_async(spec).await.is_err());
    assert!(
        seen.load(std::sync::atomic::Ordering::SeqCst),
        "the actual per-roster admission hook must refuse before fork"
    );
    assert_eq!(roster.live_count_for_test(), 0);
    assert!(
        std::fs::read_dir(modules).unwrap().next().is_none(),
        "an admission refusal must not leak its newly created cgroup directory"
    );
}

fn pause_preflight(
    owner: &Arc<crate::config_gate::runtime::RuntimeOwner>,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (reached, observed) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    owner.set_spawn_preflight_hook_for_test(move || {
        let _ = reached.send(());
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    (observed, release)
}

#[tokio::test]
async fn config_spawn_async_cancel_during_preflight_creates_no_child_or_slot() {
    let (_rig, owner, supervisor, _handler, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let roster = supervisor.child_roster.clone();
    let gate = owner.module_gate(&spec.module_id).unwrap();
    let (reached, release) = pause_preflight(&owner);
    let launch = tokio::spawn(async move { supervisor.spawn_async(spec).await });
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner.unregistered_spawn_count_for_test(), 1);
    assert_eq!(roster.live_count_for_test(), 0);
    launch.abort();
    assert!(launch.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    // The blocking preparation owns the gate until its abandoned result and
    // reservation guard are dropped. Acquiring it observes that cleanup point.
    let _prepared = tokio::time::timeout(Duration::from_secs(5), gate.lock())
        .await
        .unwrap();
    assert_eq!(owner.unregistered_spawn_count_for_test(), 0);
    assert_eq!(roster.live_count_for_test(), 0);
}

#[tokio::test]
async fn config_spawn_async_program_replaced_during_preflight_is_plan_stale() {
    let (_rig, owner, supervisor, _handler, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let roster = supervisor.child_roster.clone();
    let program = spec.program.clone();
    let (reached, release) = pause_preflight(&owner);
    let launch = tokio::spawn(async move { supervisor.spawn_async(spec).await });
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .unwrap()
        .unwrap();
    let replacement = program.with_extension("replacement");
    std::fs::copy(&program, &replacement).unwrap();
    std::fs::rename(replacement, &program).unwrap();
    release.send(()).unwrap();
    let error = match launch.await.unwrap() {
        Err(error) => error,
        Ok(module) => {
            module.stop().await.unwrap();
            panic!("plan_stale was required, but the replaced program was launched");
        }
    };
    assert!(error.to_string().contains("plan_stale"), "{error}");
    assert_eq!(owner.unregistered_spawn_count_for_test(), 0);
    assert_eq!(roster.live_count_for_test(), 0);
}
