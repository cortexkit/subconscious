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

async fn reported_pid(module: &crate::supervise::SupervisedModule) -> u32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pid) = module.status().unwrap().pid {
                return pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the owned child must cross its exec acknowledgement before observation")
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
fn config_admission_refusal_does_not_return_after_a_to_b_to_a_approval() {
    let rig = rig();
    let owner = rig.owner("refusal-a-b-a");
    rig.stamp(&owner);
    let path = rig.path.join("program");
    let original = std::fs::read(&path).unwrap();
    let original_spec = spec(&rig);
    std::fs::write(&path, b"unapproved content").unwrap();
    assert_eq!(
        owner.prepare_spawn(&original_spec).err().unwrap().code,
        "program_refused"
    );
    let mut changed = original.clone();
    changed.extend_from_slice(b"different program approval bytes");
    std::fs::write(&path, &changed).unwrap();
    approve(&rig, &owner, PlanKind::Change);
    assert!(
        owner.lock().unwrap().refused_generations.is_empty(),
        "approving B must retire A's volatile refusal"
    );
    std::fs::write(&path, &original).unwrap();
    approve(&rig, &owner, PlanKind::Change);
    assert!(
        owner.prepare_spawn(&original_spec).is_ok(),
        "approving the original A must not resurrect its old refusal"
    );
    approve(&rig, &owner, PlanKind::Unstamp);
    assert!(owner.lock().unwrap().refused_generations.is_empty());
}

#[test]
fn config_admission_transient_capture_failure_defers_without_a_durable_refusal() {
    let rig = rig();
    let owner = rig.owner("capture-unavailable");
    rig.stamp(&owner);
    let program = rig.path.join("program");
    let saved = rig.path.join("temporarily-unavailable");
    std::fs::rename(&program, &saved).unwrap();
    let result = owner.prepare_spawn(&spec(&rig));
    std::fs::rename(&saved, &program).unwrap();
    assert_eq!(result.err().unwrap().code, "config_delivery_deferred");
    assert!(owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .program_refusals
        .is_empty());
    assert!(owner.prepare_spawn(&spec(&rig)).is_ok());
}

#[test]
fn config_admission_capture_classification_approved_symlink_is_unchanged() {
    let rig = rig();
    let link = rig.path.join("program-link");
    std::os::unix::fs::symlink(rig.path.join("program"), &link).unwrap();
    let (_, input) = crate::config_gate::capture::capture_program(&link).unwrap();
    let approved = std::collections::BTreeMap::from([(
        crate::config_gate::InputKey::Program {
            module: "a".into(),
            path: link.to_string_lossy().into_owned(),
        },
        input,
    )]);
    assert!(
        !crate::config_gate::program::definite_program_change(&approved),
        "a previously approved symlink is not a new mismatch when capture temporarily fails"
    );
}

#[test]
fn config_admission_capture_classification_over_limit_is_held() {
    let rig = rig();
    let owner = rig.owner("over-limit-program");
    rig.stamp(&owner);
    std::fs::OpenOptions::new()
        .write(true)
        .open(rig.path.join("program"))
        .unwrap()
        .set_len(512 * 1024 * 1024 + 1)
        .unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
    assert_eq!(
        owner.check_module_hold("a").unwrap_err().code,
        "config_gated"
    );
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store
            .body()
            .unwrap()
            .program_refusals
            .len(),
        1
    );
}

#[test]
#[cfg(target_os = "macos")]
fn config_admission_capture_classification_malformed_macho_is_held() {
    let rig = rig();
    let owner = rig.owner("malformed-program");
    rig.stamp(&owner);
    std::fs::write(rig.path.join("program"), [0xcf, 0xfa, 0xed, 0xfe]).unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
    assert_eq!(
        owner.check_module_hold("a").unwrap_err().code,
        "config_gated"
    );
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store
            .body()
            .unwrap()
            .program_refusals
            .len(),
        1
    );
}

#[test]
fn config_admission_refusal_from_an_old_base_cannot_hold_the_new_approval() {
    let rig = rig();
    let owner = rig.owner("stale-refusal");
    rig.stamp(&owner);
    let old_base = crate::config_gate::program::approved_base(
        &crate::config_gate::GateSnapshot::from_store(
            &owner.lock().unwrap().store.body().unwrap().approved,
        )
        .unwrap(),
        "a",
    );
    let path = rig.path.join("program");
    let mut changed = std::fs::read(&path).unwrap();
    changed.extend_from_slice(b"different program approval bytes");
    std::fs::write(path, changed).unwrap();
    approve(&rig, &owner, PlanKind::Change);
    owner
        .record_program_refusal(
            "a",
            &old_base,
            subc_config::raw_digest(b"old failing image"),
        )
        .unwrap();
    assert!(
        owner.prepare_spawn(&spec(&rig)).is_ok(),
        "a refusal checked under {old_base:?} must not hold a different approved base"
    );
    assert!(owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .program_refusals
        .is_empty());
}

#[test]
fn config_admission_program_only_approval_of_a_held_module_needs_no_live_reviewer() {
    use std::io::Write;
    let rig = rig();
    let owner = rig.owner("held-program-review");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    std::fs::OpenOptions::new()
        .append(true)
        .open(rig.path.join("program"))
        .unwrap()
        .write_all(b"new approved executable content")
        .unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
    let prepared = owner.prepare(PlanKind::Change).unwrap();
    assert!(
        !prepared.review_modules.contains("a"),
        "a stopped program-only hold cannot answer its own review RPC"
    );
}

#[test]
fn config_admission_missing_approved_command_never_falls_back_to_the_live_spec() {
    let rig = rig();
    let owner = rig.owner("no-command");
    let generation = rig.stamp(&owner);
    owner
        .lock()
        .unwrap()
        .store
        .clear_delivery_snapshot_for_test(generation);
    let mut live = spec(&rig);
    live.args = vec!["unapproved live arguments".into()];
    assert_eq!(
        owner.prepare_spawn(&live).err().unwrap().code,
        "config_gated"
    );
}

#[tokio::test]
async fn config_admission_spawn_rehashes_scripts_and_covered_files_after_preparation() {
    use std::io::Write;
    for script_changed in [false, true] {
        let rig = rig();
        std::fs::write(
            rig.path.join("program"),
            "#!/bin/sh\nwhile :; do /bin/sleep 600; done\n",
        )
        .unwrap();
        let covered = rig.path.join("covered");
        std::fs::write(&covered, b"approved").unwrap();
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
        doc["modules"]["a"]["program_files"] = json!([covered]);
        std::fs::write(
            rig.path.join("subc.jsonc"),
            serde_json::to_vec(&doc).unwrap(),
        )
        .unwrap();
        let owner = Arc::new(rig.owner("spawn-double-check"));
        rig.stamp(&owner);
        let path = if script_changed {
            rig.path.join("program")
        } else {
            covered
        };
        owner.set_spawn_preflight_hook_for_test(move || {
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(b"\n# changed after preparation\n")
                .unwrap()
        });
        let supervisor = Supervisor::new_for_test(
            Arc::new(crate::Registry::default()),
            RestartPolicy::default(),
        )
        .with_config_runtime(owner);
        let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
        let status = module.status().unwrap();
        module.stop().await.unwrap();
        assert_eq!(status.state, ModuleState::Stopped);
        assert_eq!(status.spawn_generation, 0);
        assert_eq!(status.restart_count, 0);
    }
}

#[test]
fn config_admission_stamped_rescan_refuses_before_applying_a_live_file_spec() {
    let rig = rig();
    let owner = rig.owner("rescan-authority");
    rig.stamp(&owner);
    wire_fixture_document(&rig, json!(["unapproved arguments"]));
    assert_eq!(
        owner.checked_admission_hold(None).unwrap_err().code,
        "config_gated"
    );
    assert_eq!(owner.status().unwrap().pending_proposals, 1);
}

#[tokio::test]
async fn config_admission_update_configuration_cannot_install_an_unapproved_spec() {
    let rig = rig();
    let owner = Arc::new(rig.owner("update-authority"));
    rig.stamp(&owner);
    let original = spec(&rig);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner);
    let module = supervisor.spawn_async(original.clone()).await.unwrap();
    let mut changed = original.clone();
    changed.args = vec!["unapproved arguments".into()];
    changed.reserved_prefixes = vec!["unapproved".into()];
    let result = module
        .update_configuration(changed, crate::HealthConfig::default(), None)
        .await;
    let after = module.configuration().unwrap().0;
    module.stop().await.unwrap();
    assert!(
        result.is_err(),
        "the internal reconfiguration seam must not bypass stamped authority"
    );
    assert_eq!(after, original);
}

#[test]
fn config_admission_recovery_and_a_missing_binding_cannot_complete_hello() {
    for missing in [false, true] {
        let rig = rig();
        let owner = Arc::new(rig.owner("hello-recovery"));
        rig.stamp(&owner);
        let store = rig.path.join("store").join(crate::config_store::STORE_FILE);
        if missing {
            std::fs::remove_file(&store).unwrap();
        } else {
            let mut bytes = std::fs::read(&store).unwrap();
            bytes.push(b' ');
            std::fs::write(&store, bytes).unwrap();
        }
        assert!(owner.checked_admission_hold(Some("a")).is_err());
        let registry = Arc::new(crate::Registry::default());
        let handle = SupervisorHandle::new();
        handle.set_spawn_nonce("a", "known-but-unbound".into());
        handle.set_reserved_nonce("a", "known-but-unbound".into());
        let handler = crate::ControlHandler::new(registry.clone())
            .with_supervisor(handle)
            .with_config_runtime(owner);
        let hello = subc_protocol::ModuleHelloBody {
            manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
            protocol_ver: subc_protocol::PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: Some("known-but-unbound".into()),
        };
        let frame = subc_protocol::Frame::build(
            subc_protocol::FrameType::Hello,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&hello).unwrap(),
        )
        .unwrap();
        let reply = handler
            .handle_control(ConnectionId::new(999), frame)
            .unwrap();
        assert_ne!(
            reply[0].header.ty,
            subc_protocol::FrameType::HelloAck,
            "recovery must not use unbound nonce authority"
        );
        assert!(registry.get_module("a").unwrap().is_none());
    }
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
async fn config_admission_registered_actor_serves_commands_while_its_gate_is_held() {
    let rig = rig();
    let owner = Arc::new(rig.owner("registered-responsive"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let mut launch = spec(&rig);
    launch.protocol = subc_control::ModuleProtocol::Subc;
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_forwarding(forwarding.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(launch).await.unwrap();
    let gate = owner.module_gate("a").unwrap();
    let guard = gate.lock().await;
    let connection = ConnectionId::new(333);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    forwarding
        .register_module_connection(
            connection,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(tx),
        )
        .unwrap();
    registry
        .register_with_control_ops(
            subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
            subc_protocol::PROTOCOL_VERSION,
            connection,
            vec!["config.review".into()],
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let responsive = tokio::time::timeout(Duration::from_secs(1), module.set_enabled(true)).await;
    drop(guard);
    registry.deregister_connection(connection).unwrap();
    module.stop().await.unwrap();
    assert!(
        matches!(responsive, Ok(Ok(false))),
        "registered actor parked before its command select: {responsive:?}"
    );
}

#[tokio::test]
async fn config_admission_exit_observation_does_not_park_commands_behind_the_owner_mutex() {
    let rig = rig();
    let owner = Arc::new(rig.owner("exit-responsive"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let pid = reported_pid(&module).await;
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let publication = owner.clone();
    let task = tokio::task::spawn_blocking(move || {
        let _guard = publication.lock().unwrap();
        entered.send(()).unwrap();
        wait.recv().unwrap();
    });
    ready.await.unwrap();
    subc_os::Process::open(pid)
        .unwrap()
        .unwrap()
        .signal(subc_os::Signal::Kill)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let deferred_restart = tokio::time::timeout(Duration::from_secs(1), module.restart(None)).await;
    let responsive = tokio::time::timeout(Duration::from_secs(1), module.set_enabled(true)).await;
    release.send(()).unwrap();
    task.await.unwrap();
    module.stop().await.unwrap();
    assert!(
        matches!(
            deferred_restart,
            Ok(Err(
                crate::supervise::SuperviseError::ConfigDeliveryDeferred { .. }
            ))
        ),
        "a new spawn decision cannot precede the old child's recorded exit: {deferred_restart:?}"
    );
    assert!(
        matches!(responsive, Ok(Ok(false))),
        "exited actor parked inside the owner-lock wait: {responsive:?}"
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
    let pid = Some(reported_pid(&module).await);
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
fn config_admission_program_refusal_limit_never_releases_a_held_module() {
    let rig = rig();
    let path = rig.path.join("subc.jsonc");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    document["modules"]["b"] = document["modules"]["a"].clone();
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let owner = rig.owner("refusal-cap");
    rig.stamp(&owner);
    let approved = owner.lock().unwrap().store.body().unwrap().approved.clone();
    let snapshot = crate::config_gate::GateSnapshot::from_store(&approved).unwrap();
    let base_a = crate::config_gate::program::approved_base(&snapshot, "a");
    let base_b = crate::config_gate::program::approved_base(&snapshot, "b");
    let limit = crate::config_store::PROGRAM_REFUSAL_LIMIT;
    owner
        .record_program_refusal(
            "a",
            &base_a,
            subc_config::raw_digest(b"first refused program"),
        )
        .unwrap();
    for identity in 0..limit {
        owner
            .record_program_refusal(
                "b",
                &base_b,
                subc_config::raw_digest(identity.to_string().as_bytes()),
            )
            .unwrap();
    }
    assert_eq!(
        owner
            .check_module_hold("a")
            .expect_err("capacity eviction released a held module")
            .code,
        "config_gated"
    );
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store
            .body()
            .unwrap()
            .program_refusals
            .len(),
        limit + 1
    );
    drop(owner);
    let restarted = rig.owner("refusal-cap-restarted");
    assert_eq!(
        restarted
            .check_module_hold("a")
            .expect_err("restart lost the first durable hold")
            .code,
        "config_gated"
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn refused_content_hello(
    rig: &Rig,
    owner: Arc<RuntimeOwner>,
    handle: SupervisorHandle,
    registry: Arc<crate::Registry>,
    module: crate::supervise::SupervisedModule,
) {
    let nonce = handle.spawn_launch_nonce_for("a").unwrap();
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_config_runtime(owner.clone());
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: Some(nonce),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    let connection = ConnectionId::new(444);
    let reply = handler.handle_control(connection, frame).unwrap();
    let _ = registry.deregister_connection(connection);
    module.stop().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reply[0].body).unwrap();
    assert_eq!(
        body["code"], "program_refused",
        "unapproved bytes completed HELLO at {:?}: {body}",
        rig.path
    );
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.refused")
            .count(),
        1,
        "content refusal, not merely an unready latch, must reject HELLO"
    );
}

fn wire_fixture_document(rig: &Rig, args: serde_json::Value) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["protocol"] = json!("subc");
    doc["modules"]["a"]["args"] = args;
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn config_admission_hello_accepts_only_the_approved_current_native_image() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("native-content"));
    rig.stamp(&owner);
    let handle = SupervisorHandle::new();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let nonce = handle.spawn_launch_nonce_for("a").unwrap();
    assert!(handle.wait_program_binding("a", &nonce).await);
    assert!(
        !module.configuration().unwrap().0.reserved,
        "the launch-local nonce gate must not rewrite approved role policy"
    );
    assert!(
        handle.reserved_hello_authorized("a", None),
        "the configured unreserved legacy policy must not be rewritten by content admission"
    );
    assert!(handle.reserved_hello_authorized("a", Some(&nonce)));
    assert!(
        handle.reserved_hello_authorized("a.child", None),
        "no capability or child-id prefix was reserved by the launch-local flag"
    );
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_config_runtime(owner);
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: Some(nonce),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    let impostor = handler
        .handle_control(ConnectionId::new(446), module_hello(None))
        .unwrap();
    assert_ne!(
        impostor[0].header.ty,
        subc_protocol::FrameType::HelloAck,
        "gated identity checks remain mandatory without a synthetic legacy reservation"
    );
    assert!(registry.get_module("a").unwrap().is_none());
    let connection = ConnectionId::new(445);
    let reply = handler.handle_control(connection, frame).unwrap();
    registry.deregister_connection(connection).unwrap();
    module.stop().await.unwrap();
    assert_eq!(reply[0].header.ty, subc_protocol::FrameType::HelloAck);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn config_admission_unsigned_macos_program_is_held_before_any_spawn() {
    let rig = rig();
    assert!(std::process::Command::new("/usr/bin/codesign")
        .arg("--remove-signature")
        .arg(rig.path.join("program"))
        .status()
        .unwrap()
        .success());
    let owner = Arc::new(rig.owner("unsigned-image"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let status = module.status().unwrap();
    module.stop().await.unwrap();
    assert_eq!(status.state, ModuleState::Stopped);
    assert_eq!(status.spawn_generation, 0);
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.refused")
            .count(),
        1
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_admission_hello_hashes_the_running_image_after_a_same_inode_rewrite() {
    use std::io::Write;
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("image-content"));
    rig.stamp(&owner);
    let program = rig.path.join("program");
    owner.set_spawn_exec_hook_for_test(move || {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&program)
            .unwrap()
            .write_all(b"unapproved image bytes")
            .unwrap()
    });
    let handle = SupervisorHandle::new();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    assert!(module.status().unwrap().process_alive);
    refused_content_hello(&rig, owner, handle, registry, module).await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn config_admission_hello_rechecks_script_and_declared_file_content() {
    use std::io::Write;
    for script_changed in [false, true] {
        let rig = rig();
        std::fs::write(
            rig.path.join("program"),
            "#!/bin/sh\nwhile :; do /bin/sleep 600; done\n",
        )
        .unwrap();
        wire_fixture_document(&rig, json!([]));
        let covered = rig.path.join("covered");
        std::fs::write(&covered, b"approved").unwrap();
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
        doc["modules"]["a"]["program_files"] = json!([covered]);
        std::fs::write(
            rig.path.join("subc.jsonc"),
            serde_json::to_vec(&doc).unwrap(),
        )
        .unwrap();
        let owner = Arc::new(rig.owner("auxiliary-content"));
        rig.stamp(&owner);
        let handle = SupervisorHandle::new();
        let registry = Arc::new(crate::Registry::default());
        let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
            .with_handle(handle.clone())
            .with_config_runtime(owner.clone());
        let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
        assert!(
            handle
                .wait_program_binding("a", &handle.spawn_launch_nonce_for("a").unwrap())
                .await
        );
        let changed = if script_changed {
            rig.path.join("program")
        } else {
            covered
        };
        std::fs::OpenOptions::new()
            .append(true)
            .open(changed)
            .unwrap()
            .write_all(b"\n# unapproved bytes\n")
            .unwrap();
        refused_content_hello(&rig, owner, handle, registry, module).await;
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn config_admission_hello_rejects_a_wrapper_that_execs_after_the_spawn_sample() {
    let rig = rig();
    let marker = rig.path.join("release-wrapper");
    std::fs::write(
        rig.path.join("program"),
        "#!/bin/sh\nwhile [ ! -f \"$1\" ]; do /bin/sleep 0.01; done\nexec /bin/sleep 600\n",
    )
    .unwrap();
    wire_fixture_document(&rig, json!([marker]));
    let owner = Arc::new(rig.owner("late-exec"));
    rig.stamp(&owner);
    let handle = SupervisorHandle::new();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    assert!(
        handle
            .wait_program_binding("a", &handle.spawn_launch_nonce_for("a").unwrap())
            .await
    );
    std::fs::write(marker, "").unwrap();
    let process = subc_os::Process::open(module.status().unwrap().pid.unwrap())
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while process.observe().and_then(|image| image.executable)
            != subc_os::file_identity(std::path::Path::new("/bin/sleep"))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    refused_content_hello(&rig, owner, handle, registry, module).await;
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

fn module_hello(nonce: Option<String>) -> subc_protocol::Frame {
    let body = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: nonce,
    };
    subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn config_admission_stopped_gated_identity_refuses_impostors_without_a_live_spawn() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("stopped-hello"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let handle = SupervisorHandle::new();
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle.clone())
        .with_config_runtime(owner.clone());
    for nonce in [None, Some("unminted".into())] {
        let reply = handler
            .handle_control(ConnectionId::new(701), module_hello(nonce))
            .unwrap();
        assert_ne!(reply[0].header.ty, subc_protocol::FrameType::HelloAck);
    }
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    reported_pid(&module).await;
    let old = handle.spawn_launch_nonce_for("a").unwrap();
    module.stop().await.unwrap();
    for nonce in [None, Some(old.clone())] {
        let reply = handler
            .handle_control(ConnectionId::new(702), module_hello(nonce))
            .unwrap();
        assert_ne!(
            reply[0].header.ty,
            subc_protocol::FrameType::HelloAck,
            "a stopped child is not a live content witness"
        );
    }
    let mut changed = std::fs::read(rig.path.join("program")).unwrap();
    changed.extend_from_slice(b"changed while stopped");
    std::fs::write(rig.path.join("program"), changed).unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
    let held = handler
        .handle_control(ConnectionId::new(704), module_hello(Some(old)))
        .unwrap();
    assert_ne!(
        held[0].header.ty,
        subc_protocol::FrameType::HelloAck,
        "a held identity is protected without a spawn in flight"
    );
    assert!(registry.get_module("a").unwrap().is_none());
}

#[tokio::test]
async fn config_admission_unstamp_restores_unreserved_hello_without_a_synthetic_sticky_nonce() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("unstamp-reservation"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    reported_pid(&module).await;
    module.stop().await.unwrap();
    approve(&rig, &owner, PlanKind::Unstamp);
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_config_runtime(owner);
    let reply = handler
        .handle_control(ConnectionId::new(703), module_hello(None))
        .unwrap();
    let _ = registry.deregister_connection(ConnectionId::new(703));
    assert_eq!(
        reply[0].header.ty,
        subc_protocol::FrameType::HelloAck,
        "unstamp must restore the configured unreserved policy"
    );
}

#[tokio::test]
async fn config_admission_promoted_unreserved_child_can_reconnect_with_its_current_content_witness()
{
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    document["modules"]["a"]["overlap"] = json!("safe");
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    let owner = Arc::new(rig.owner("promoted-reservation"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_forwarding(forwarding.clone())
        .with_config_runtime(owner.clone())
        .with_drain_timeout(Duration::from_millis(20));
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    reported_pid(&module).await;
    let original = handle.spawn_launch_nonce_for("a").unwrap();
    let mut manifest = subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build();
    manifest.ready = Some(true);
    let incumbent = ConnectionId::new(710);
    let candidate = ConnectionId::new(711);
    registry
        .register_with_control_ops(
            manifest.clone(),
            subc_protocol::PROTOCOL_VERSION,
            incumbent,
            vec![],
        )
        .unwrap();
    let (tx, _incumbent_frames) = tokio::sync::mpsc::channel(32);
    forwarding
        .register_module_connection(
            incumbent,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(tx),
        )
        .unwrap();
    let swapping = {
        let module = module.clone();
        tokio::spawn(async move { module.swap(Some(Duration::from_secs(5))).await })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while !handle.swap_open("a") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    registry
        .register_candidate_with_control_ops(
            manifest,
            subc_protocol::PROTOCOL_VERSION,
            candidate,
            vec![],
        )
        .unwrap();
    let (tx, _candidate_frames) = tokio::sync::mpsc::channel(32);
    forwarding
        .register_candidate_module_connection(
            candidate,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(tx),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while registry
            .get_module("a")
            .unwrap()
            .is_none_or(|active| active.connection_id != candidate)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    registry.deregister_connection(incumbent).unwrap();
    let outcome = swapping.await.unwrap();
    assert!(outcome.is_ok(), "{outcome:?}");
    reported_pid(&module).await;
    let current = handle.spawn_launch_nonce_for("a").unwrap();
    assert_ne!(original, current);
    assert!(handle.wait_program_binding("a", &current).await);
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.swap_open("a") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let handler = crate::ControlHandler::with_forwarding(registry.clone(), forwarding)
        .with_supervisor(handle)
        .with_config_runtime(owner);
    handler.cleanup_connection(candidate).unwrap();
    let reconnect = ConnectionId::new(712);
    let reply = handler
        .handle_control(reconnect, module_hello(Some(current)))
        .unwrap();
    let _ = registry.deregister_connection(reconnect);
    module.stop().await.unwrap();
    assert_eq!(
        reply[0].header.ty,
        subc_protocol::FrameType::HelloAck,
        "the configured unreserved policy must not retain the incumbent's synthetic legacy nonce"
    );
}
