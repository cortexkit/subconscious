use super::*;
use crate::supervise::{ModuleSpec, ModuleState, RestartPolicy, Supervisor, SupervisorHandle};
use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

fn rig() -> Rig {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!(
            "admission-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
    std::fs::create_dir_all(path.join("store")).unwrap();
    std::fs::create_dir_all(path.join("cortexkit")).unwrap();
    std::fs::copy("/bin/sleep", path.join("program")).unwrap();
    #[cfg(target_os = "macos")]
    assert!(
        std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(path.join("program"))
            .status()
            .unwrap()
            .success(),
        "a copied Apple platform binary needs a local test signature before exec"
    );
    std::fs::write(path.join("subc.jsonc"), serde_json::to_vec(&json!({"version":1,"modules":{"a":{"program":path.join("program"),"args":["600"],"protocol":"none"}}})).unwrap()).unwrap();
    let declaration = ConfigDeclareRequest::new(
        json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"properties":{"limit":{"type":"integer","description":"Limit","default":1}},"x-ck-layers":["user"],"x-ck-apply":"live"}),
        path.join("cortexkit/a.jsonc").to_string_lossy(),
        vec![],
    );
    Rig { path, declaration }
}

fn spec(rig: &Rig) -> ModuleSpec {
    crate::daemon_config::parse_with_activity(
        &std::fs::read_to_string(rig.path.join("subc.jsonc")).unwrap(),
        rig.path.join("subc.jsonc"),
    )
    .unwrap()
    .0
    .modules[0]
        .module_spec()
}

fn approve(_rig: &Rig, owner: &RuntimeOwner, kind: PlanKind) -> u64 {
    let plan = owner
        .finish_plan(owner.prepare(kind).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            2,
        )
        .unwrap()
}

#[test]
fn config_admission_changed_binary_is_held_and_refused_once_per_identity() {
    let rig = rig();
    let owner = rig.owner("binary");
    rig.stamp(&owner);
    let spec = spec(&rig);
    assert!(owner.prepare_spawn(&spec).is_ok());
    std::fs::copy(&spec.program, rig.path.join("replacement")).unwrap();
    std::fs::rename(rig.path.join("replacement"), &spec.program).unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec).err().unwrap().code,
        "program_refused"
    );
    for _ in 0..3 {
        assert_eq!(
            owner.prepare_spawn(&spec).err().unwrap().code,
            "config_gated"
        );
    }
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.refused")
            .count(),
        1
    );
    drop(owner);
    let owner = rig.owner("binary-restarted");
    assert_eq!(
        owner.prepare_spawn(&spec).err().unwrap().code,
        "config_gated"
    );
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.refused")
            .count(),
        0
    );
    assert_eq!(owner.status().unwrap().pending_proposals, 1);
    assert_eq!(owner.status().unwrap().gated_modules, ["a"]);
    approve(&rig, &owner, PlanKind::Change);
    assert!(owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .program_refusals
        .is_empty());
    assert!(owner.prepare_spawn(&spec).is_ok());
    assert!(owner.status().unwrap().gated_modules.is_empty());
}

#[test]
fn config_admission_declared_files_are_covered_but_argument_files_are_not() {
    let rig = rig();
    let declared = rig.path.join("code.cjs");
    let argument = rig.path.join("settings.jsonc");
    std::fs::write(&declared, "approved code").unwrap();
    std::fs::write(&argument, "old settings").unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["program_files"] = json!([declared]);
    doc["modules"]["a"]["args"] = json!([argument]);
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
    let owner = rig.owner("files");
    rig.stamp(&owner);
    std::fs::write(argument, "edited settings").unwrap();
    assert!(owner.prepare_spawn(&spec(&rig)).is_ok());
    assert_eq!(owner.status().unwrap().pending_proposals, 0);
    std::fs::write(declared, "unapproved code").unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
}

#[test]
fn config_admission_shebang_forms_resolve_interpreter_once_and_reject_env_options() {
    let rig = rig();
    let interpreter = rig.path.join("node");
    std::fs::copy("/bin/sleep", &interpreter).unwrap();
    let script = rig.path.join("main.cjs");
    std::fs::write(&script, "#!/usr/bin/env node\ncode\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut spec = spec(&rig);
    spec.program = script.clone();
    spec.env
        .push(("PATH".into(), rig.path.to_string_lossy().into_owned()));
    spec.args = vec!["argument".into()];
    let launch = crate::config_gate::program::capture(&spec, &[]).unwrap();
    assert_eq!(launch.spec.program, interpreter);
    assert_eq!(
        launch.spec.args,
        [script.to_string_lossy().as_ref(), "argument"]
    );
    assert_eq!(launch.inputs.len(), 2);
    assert!(launch.inputs.values().any(|input| matches!(&input.value, subc_config::RecordedValue::Value { value } if value["coverage"] == "interpreter only covered")));
    for rejected in [
        "#!/usr/bin/env -S node --flag\n",
        "#!relative\n",
        "#!/usr/bin/env NAME=value\n",
    ] {
        std::fs::write(&script, rejected).unwrap();
        assert_eq!(
            crate::config_gate::program::capture(&spec, &[])
                .err()
                .unwrap()
                .code,
            "program_refused"
        );
    }
    std::fs::write(&script, "#!/bin/sleep 600\n").unwrap();
    let launch = crate::config_gate::program::capture(&spec, &[]).unwrap();
    assert_eq!(launch.spec.args[0], "600");
    assert_eq!(launch.inputs.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn config_admission_program_hold_never_arms_retry_or_spends_restart_budget() {
    let rig = rig();
    let owner = Arc::new(rig.owner("held"));
    rig.stamp(&owner);
    let spec = spec(&rig);
    std::fs::write(&spec.program, "unapproved replacement").unwrap();
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_handle(handle)
    .with_config_runtime(owner);
    let module = supervisor
        .supervise_configured_requested_async(
            spec,
            true,
            Default::default(),
            None,
            RestartPolicy::default(),
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    let before = module.status().unwrap();
    assert_eq!(before.state, ModuleState::Stopped);
    assert!(module
        .restart(None)
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert!(module
        .reload()
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert!(module
        .set_enabled(true)
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert!(module
        .swap(None)
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    tokio::time::advance(Duration::from_secs(7200)).await;
    let after = module.status().unwrap();
    assert_eq!(after.spawn_generation, before.spawn_generation);
    assert_eq!(after.restart_count, 0);
    assert!(module.next_retry_at_ms().unwrap().is_none());
}

#[tokio::test]
async fn config_admission_crash_during_approval_defers_without_parking_the_actor() {
    let rig = rig();
    let owner = Arc::new(rig.owner("publication"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_handle(SupervisorHandle::new())
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while module.status().unwrap().pid.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let before = module.status().unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["args"] = json!(["601"]);
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    let attempt = challenge.attempt();
    let gates = owner.approval_commit_gates(attempt).await.unwrap();
    assert!(!gates.is_empty());
    assert!(before.process_start_time.is_some());
    assert_eq!(
        crate::provenance::process_start_time(before.pid.unwrap()),
        before.process_start_time
    );
    subc_os::Process::open(before.pid.unwrap())
        .unwrap()
        .unwrap_or_else(|| {
            panic!(
                "child vanished: {:?}; stderr {:?}",
                module.status(),
                module.stderr_tail(None, None)
            )
        })
        .signal(subc_os::Signal::Kill)
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while module.status().unwrap().process_alive && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let responsive = tokio::time::timeout(Duration::from_secs(1), module.set_enabled(true)).await;
    let during = module.status().unwrap();
    let published = owner
        .on_blocking(move |owner| {
            owner.finish_approval(
                attempt,
                &Rig::facts(),
                ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
                2,
            )
        })
        .await;
    drop(gates);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while module.status().unwrap().spawn_generation == before.spawn_generation
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let after = module.status().unwrap();
    module.stop().await.unwrap();
    assert!(published.is_ok(), "{published:?}");
    assert!(
        matches!(responsive, Ok(Ok(false))),
        "the actor must still answer commands while publication owns its gate: {responsive:?}"
    );
    assert_eq!(during.spawn_generation, before.spawn_generation);
    assert_eq!(after.spawn_generation, before.spawn_generation + 1);
    assert_eq!(
        after.restart_count, during.restart_count,
        "deferred admission cannot spend another restart-budget entry"
    );
}

#[tokio::test]
async fn config_admission_rollback_executes_the_current_restored_path_and_rechecks_it() {
    let rig = rig();
    let owner = Arc::new(rig.owner("rollback-spawn"));
    rig.stamp(&owner);
    let restored = spec(&rig);
    let other = rig.path.join("new-program");
    std::fs::copy("/bin/sleep", &other).unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["program"] = json!(other);
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
    approve(&rig, &owner, PlanKind::Change);
    std::fs::copy("/bin/sleep", rig.path.join("restored-replacement")).unwrap();
    std::fs::rename(rig.path.join("restored-replacement"), &restored.program).unwrap();
    let generation = approve(&rig, &owner, PlanKind::Rollback(1));
    let approved = owner.approved_rollback_commands(generation).unwrap();
    assert_eq!(
        approved.programs["a"].path,
        restored.program.to_string_lossy()
    );
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let status = module.status().unwrap();
    module.stop().await.unwrap();
    assert_eq!(status.spawned_from.as_ref(), Some(&restored.program));
    std::fs::write(&restored.program, "changed after rollback approval").unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_admission_unix_replacement_between_check_and_exec_is_killed_and_held() {
    let rig = rig();
    let owner = Arc::new(rig.owner("exec-race"));
    rig.stamp(&owner);
    let program = rig.path.join("program");
    let replacement = rig.path.join("racing-program");
    std::fs::copy(&program, &replacement).unwrap();
    owner.set_spawn_exec_hook_for_test(move || std::fs::rename(replacement, program).unwrap());
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let status = module.status().unwrap();
    assert_eq!(status.state, ModuleState::Stopped);
    assert!(!status.process_alive);
    assert_eq!(status.spawn_generation, 0);
    assert_eq!(status.restart_count, 0);
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.refused")
            .count(),
        1
    );
}

#[tokio::test]
async fn config_admission_tamper_recovery_preserves_running_children_but_refuses_new_use() {
    let rig = rig();
    let owner = Arc::new(rig.owner("tamper"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let pid = module.status().unwrap().pid;
    let store = rig.path.join("store").join(crate::config_store::STORE_FILE);
    let mut edited = std::fs::read(&store).unwrap();
    edited.push(b' ');
    std::fs::write(store, edited).unwrap();
    assert_eq!(
        owner.checked_admission_hold(Some("a")).unwrap_err().code,
        "store_tampered"
    );
    assert!(module.status().unwrap().process_alive);
    assert_eq!(module.status().unwrap().pid, pid);
    assert!(module
        .restart(None)
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert!(module
        .reload()
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert!(module
        .swap(None)
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert_eq!(module.status().unwrap().pid, pid);
    module.stop().await.unwrap();
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let held = supervisor.spawn_async(spec(&rig)).await.unwrap();
    assert_eq!(held.status().unwrap().state, ModuleState::Stopped);
    assert_eq!(held.status().unwrap().restart_count, 0);
    assert_eq!(held.status().unwrap().spawn_generation, 0);
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.tamper")
            .count(),
        1
    );
}

#[test]
fn config_admission_unstamp_clears_durable_refusals_and_restores_file_authority() {
    let rig = rig();
    let owner = rig.owner("unstamp");
    rig.stamp(&owner);
    std::fs::write(rig.path.join("program"), "different code").unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
    approve(&rig, &owner, PlanKind::Unstamp);
    assert!(owner.prepare_spawn(&spec(&rig)).unwrap().launch.is_none());
    assert!(owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .program_refusals
        .is_empty());
    assert_eq!(owner.status().unwrap().state, "unstamped");
}

#[test]
fn config_admission_program_refusal_records_are_capped_oldest_first() {
    let rig = rig();
    let owner = rig.owner("refusal-cap");
    rig.stamp(&owner);
    let approved = owner.lock().unwrap().store.body().unwrap().approved.clone();
    let base = crate::config_gate::program::approved_base(
        &crate::config_gate::GateSnapshot::from_store(&approved).unwrap(),
        "a",
    );
    let limit = crate::config_store::PROGRAM_REFUSAL_LIMIT;
    let mut state = owner.lock().unwrap();
    for identity in 0..=limit {
        state
            .store
            .record_program_refusal(
                "a",
                base.clone(),
                subc_config::raw_digest(identity.to_string().as_bytes()),
            )
            .unwrap();
    }
    let records = &state.store.body().unwrap().program_refusals;
    assert_eq!(records.len(), limit);
    assert_eq!(
        records.first().unwrap().refused_identity,
        subc_config::raw_digest(b"1")
    );
    assert_eq!(
        records.last().unwrap().refused_identity,
        subc_config::raw_digest(limit.to_string().as_bytes())
    );
}

#[tokio::test]
async fn config_admission_approving_a_refused_program_starts_its_enabled_held_actor() {
    let rig = rig();
    let owner = Arc::new(rig.owner("release"));
    rig.stamp(&owner);
    std::fs::copy(rig.path.join("program"), rig.path.join("replacement")).unwrap();
    std::fs::rename(rig.path.join("replacement"), rig.path.join("program")).unwrap();
    let handle = SupervisorHandle::new();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    assert_eq!(module.status().unwrap().state, ModuleState::Stopped);
    let handler = Arc::new(
        crate::ControlHandler::new(registry)
            .with_supervisor(handle)
            .with_config_runtime(owner.clone()),
    );
    let worker = handler.spawn_config_delivery_worker().unwrap();
    approve(&rig, &owner, PlanKind::Change);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while module.status().unwrap().pid.is_none() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let status = module.status().unwrap();
    worker.abort();
    module.stop().await.unwrap();
    assert!(status.process_alive);
    assert_eq!(status.spawn_generation, 1);
    assert_eq!(status.lifetime_restarts, 0);
    assert_eq!(status.restart_count, 0);
}

#[test]
fn config_admission_final_check_keeps_the_pending_exit_fence_then_holds_without_an_active_vector() {
    let rig = rig();
    let owner = rig.owner("pending-exit");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let admission = owner.prepare_spawn(&spec(&rig)).unwrap();
    let spawn = owner
        .delivery_spawned(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(10),
            Duration::from_secs(10),
        )
        .unwrap()
        .unwrap();
    owner.delivery_child_exited(&spawn).unwrap();
    assert_eq!(
        owner.check_spawn("a", &admission).unwrap_err().code,
        "config_delivery_deferred"
    );
    let exit = owner.delivery_exits().unwrap().pop().unwrap();
    owner.delivery_record_exit(&exit).unwrap();
    owner.delivery_release_exit(&exit).unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "config_gated"
    );
}

#[test]
fn config_admission_daemon_restart_precedence_keeps_the_active_command_until_a_new_daemon() {
    let rig = rig();
    let owner = rig.owner("deferred-command");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    owner
        .lock()
        .unwrap()
        .store
        .record_delivery(
            &crate::config_store::Tuple {
                module: "a".into(),
                project: None,
            },
            1,
            true,
        )
        .unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["args"] = json!(["601"]);
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), |old, new| {
            let mut changes = daemon_review(old, new)?;
            changes.fields.push(subc_config::FieldChange::new(
                "a",
                "/daemon_dependency",
                subc_config::RecordedValue::new(json!(1)),
                subc_config::RecordedValue::new(json!(2)),
                subc_config::ApplyKind::DaemonRestart,
            ));
            Ok(changes)
        })
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            2,
        )
        .unwrap();
    assert_eq!(
        owner
            .prepare_spawn(&spec(&rig))
            .unwrap()
            .launch
            .unwrap()
            .spec
            .args,
        ["600"]
    );
    drop(owner);
    let restarted = rig.owner("new-daemon");
    assert_eq!(
        restarted
            .prepare_spawn(&spec(&rig))
            .unwrap()
            .launch
            .unwrap()
            .spec
            .args,
        ["601"]
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_admission_exited_before_binding_keeps_its_real_exit_without_admitting_hello() {
    let rig = rig();
    std::fs::write(rig.path.join("program"), "#!/bin/sh\nexit 23\n").unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["protocol"] = json!("subc");
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
    let owner = Arc::new(rig.owner("already-exited"));
    rig.stamp(&owner);
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    )
    .with_handle(handle.clone())
    .with_config_runtime(owner);
    supervisor.set_admission_hook_for_test(|_, stage, pid| {
        if stage == "before_image_binding" {
            let path = format!("/proc/{}/stat", pid.unwrap());
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !std::fs::read_to_string(&path)
                .unwrap()
                .rsplit_once(") ")
                .unwrap()
                .1
                .starts_with("Z ")
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the fixture must exit before its image is sampled"
                );
                std::thread::yield_now();
            }
        }
        true
    });
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let nonce = handle.spawn_launch_nonce_for("a").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while module.status().unwrap().last_exit.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let status = module.status().unwrap();
    assert_eq!(status.last_exit.unwrap().code, Some(23));
    assert_eq!(status.state, ModuleState::Failed);
    assert_eq!(status.spawn_generation, 1);
    assert!(!status.process_alive);
    assert!(!handle.program_binding_verified("a", &nonce));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_admission_wrapper_exec_to_a_different_program_is_program_refused() {
    let rig = rig();
    std::fs::write(rig.path.join("program"), "#!/bin/sh\nexec /bin/sleep 600\n").unwrap();
    let owner = Arc::new(rig.owner("wrapper-exec"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let replacement_image = subc_os::file_identity(std::path::Path::new("/bin/sleep")).unwrap();
    supervisor.set_admission_hook_for_test(move |_, stage, pid| {
        if stage == "before_image_binding" {
            let process = subc_os::Process::open(pid.unwrap()).unwrap().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while process.observe().and_then(|image| image.executable) != Some(replacement_image) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the wrapper must exec the other program before binding is sampled"
                );
                std::thread::yield_now();
            }
        }
        true
    });
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let status = module.status().unwrap();
    assert_eq!(status.state, ModuleState::Stopped);
    assert!(!status.process_alive);
    assert_eq!(status.spawn_generation, 0);
    assert_eq!(status.restart_count, 0);
    let refused: Vec<_> = owner
        .take_activities()
        .into_iter()
        .filter(|activity| activity.event == "config.refused")
        .collect();
    assert_eq!(refused.len(), 1);
    assert!(
        refused[0].detail.starts_with("a: program_refused;"),
        "{:?}",
        refused[0]
    );
}

#[tokio::test]
async fn config_admission_tamper_activity_is_emitted_only_after_the_module_gate_releases() {
    async fn logged(ledger: &crate::activity::ActivityLedger) -> usize {
        ledger.record("barrier".into(), serde_json::from_value(json!({"op":"activity.record","kind":"barrier.flushed","summary":"Queue flushed"})).unwrap(), subc_control::ActivityActor { principal:subc_protocol::Principal::Direct, scope_owner:None,agent_id:None,run_id:None,flow_id:None,project_root:None,workdir:None,project_id:None },1).await.unwrap();
        ledger
            .query(
                serde_json::from_value(
                    json!({"op":"activity.query","limit":1000,"kind":"config.tamper"}),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .entries
            .len()
    }
    let rig = rig();
    let owner = Arc::new(rig.owner("tamper-activity"));
    rig.stamp(&owner);
    let ledger = crate::activity::ActivityLedger::open(
        rig.path.join("activity.jsonl"),
        "tamper-activity".into(),
    );
    owner.set_activity(Some(ledger.clone())).unwrap();
    let gate = owner.module_gate("a").unwrap().lock_owned().await;
    let path = rig.path.join("store").join(crate::config_store::STORE_FILE);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.push(b' ');
    std::fs::write(path, bytes).unwrap();
    let result = owner
        .on_blocking(|owner| owner.checked_admission_hold(Some("a")))
        .await;
    assert_eq!(result.unwrap_err().code, "store_tampered");
    assert_eq!(logged(&ledger).await, 0);
    drop(gate);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while logged(&ledger).await == 0 && tokio::time::Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    assert_eq!(logged(&ledger).await, 1);
}
