use super::*;

#[cfg(all(unix, feature = "dev-placement"))]
#[test]
fn origins_placement_records_exact_launch_and_rederives_after_reload() {
    use crate::{program_origin::LaunchWitness, spawn_authority::SpawnAuthorityRecord};
    use subc_protocol::peer::CallerOrigin;
    let rig = program_rig();
    let owner = rig.owner("origin-placement");
    rig.stamp(&owner);
    let digest = replace_program(&rig);
    let manifest = json!({"module_id":"a", "hosts_third_party_code":false,
        "provider_declarations":{"provider":{"operation":"read"}},
        "provenance":{"build_git_sha":"a".repeat(40)}});
    let id = owner
        .place_program_with_manifest(
            "a",
            rig.path.join("program").to_str().unwrap(),
            &digest,
            Some(&manifest),
        )
        .unwrap()
        .unwrap();
    {
        let mut state = owner.lock().unwrap();
        let evidence = &state.store.body().unwrap().generations[&id]
            .generation
            .changeset
            .launch_manifests["a"]["origin_evidence"];
        assert_eq!(evidence["module_id"], "a");
        assert_eq!(evidence["published_commit"], "a".repeat(40));
        assert_eq!(evidence["hosts_third_party_code"], false);
        assert_eq!(evidence["declarations"], manifest["provider_declarations"]);
        assert_eq!(
            evidence["launch"]["working_directory"],
            std::env::current_dir().unwrap().to_str().unwrap()
        );
    }
    drop(owner);
    let owner = rig.owner("origin-placement-reloaded");
    let spec = module_spec(&rig);
    let admission = owner.prepare_spawn(&spec).unwrap();
    let witness = LaunchWitness::capture(&spec).unwrap();
    let record = SpawnAuthorityRecord::new("incarnation", 1, "a");
    admission.initialize_origin(&record, &witness);
    assert_eq!(record.capture().unwrap().origin, CallerOrigin::Development);
    let requested = serde_json::from_value(manifest["provider_declarations"].clone()).unwrap();
    let differing = record.initialize_declarations(requested);
    let plan = owner.plan_development_widening(&record, differing).unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(crate::config_gate::PresenceDecision::InteractiveApproval),
            1,
        )
        .unwrap();
    assert_eq!(
        owner.check_spawn("a", &admission).unwrap_err().code,
        "config_delivery_deferred"
    );
    drop(owner);
    let owner = rig.owner("origin-placement-after-widening");
    let admission = owner.prepare_spawn(&spec).unwrap();
    let next = SpawnAuthorityRecord::new("incarnation", 2, "a");
    admission.initialize_origin(&next, &witness);
    assert_eq!(next.capture().unwrap().origin, CallerOrigin::Development);
    assert!(next
        .initialize_declarations(
            serde_json::from_value(manifest["provider_declarations"].clone()).unwrap()
        )
        .is_empty());
    assert!(next
        .capture()
        .unwrap()
        .approved
        .entries
        .contains_key("provider"));
    let mut changed = witness;
    changed.working_directory = rig.path.join("different-directory");
    let respawn = SpawnAuthorityRecord::new("incarnation", 3, "a");
    admission.initialize_origin(&respawn, &changed);
    assert_eq!(respawn.capture().unwrap().origin, CallerOrigin::Other);
}

#[test]
fn config_status_tracks_live_declarations_without_approved_config() {
    let rig = Rig::new();
    let owner = rig.owner("gated-status");
    rig.stamp(&owner);
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"approved"}}"#,
    )
    .unwrap();
    rig.declare(&owner, 10);
    assert_eq!(
        owner
            .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
            .unwrap_err()
            .code,
        "config_gated"
    );
    assert_eq!(owner.status().unwrap().gated_modules, ["a"]);
    owner.disconnected(ConnectionId::new(10));
    assert!(owner.status().unwrap().gated_modules.is_empty());
    rig.declare(&owner, 11);
    assert_eq!(owner.status().unwrap().gated_modules, ["a"]);
    let generation = rig.stamp(&owner);
    owner
        .lock()
        .unwrap()
        .store
        .record_delivery(
            &crate::config_store::Tuple {
                module: "a".into(),
                project: None,
            },
            generation,
            true,
        )
        .unwrap();
    assert!(owner
        .module_get(ConnectionId::new(11), "a", ConfigGetRequest::new())
        .unwrap()
        .active
        .is_some());
    assert!(owner.status().unwrap().gated_modules.is_empty());
    owner.disconnected(ConnectionId::new(11));
    rig.declare(&owner, 12);
    assert!(owner.status().unwrap().gated_modules.is_empty());
}

#[test]
fn config_status_lists_ungated_modules() {
    let rig = Rig::new();
    let owner = rig.owner("ungated-status");
    assert_eq!(owner.status().unwrap().ungated_modules, ["a"]);
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"approved"}}"#,
    )
    .unwrap();
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    assert!(owner.status().unwrap().ungated_modules.is_empty());
}

#[cfg(unix)]
fn program_rig() -> Rig {
    let rig = Rig::new();
    // The native fixture sleeps for a numeric argument; a copied /bin/sleep
    // fails where coreutils is one multi-call binary.
    std::fs::copy(
        crate::config_gate::program::native_fixture_for_test(),
        rig.path.join("program"),
    )
    .unwrap();
    std::fs::write(rig.path.join("subc.jsonc"), serde_json::to_vec(&json!({
        "version":1,"modules":{"a":{"program":rig.path.join("program"),"args":["600"],"protocol":"none","env":{
            "HOME":rig.path,"XDG_DATA_HOME":rig.path,"XDG_CONFIG_HOME":rig.path,"XDG_RUNTIME_DIR":rig.path,"SUBC_CONNECTION_FILE":rig.path.join("connection.json")
        }}}
    })).unwrap()).unwrap();
    rig
}

#[cfg(all(unix, feature = "dev-placement"))]
#[tokio::test]
async fn config_placement_restarts_module_on_recorded_program_without_crash_budget() {
    use crate::supervise::{RestartPolicy, Supervisor};
    use std::{sync::Arc, time::Duration};
    let rig = program_rig();
    let owner = Arc::new(rig.owner("placement-restart"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(module_spec(&rig)).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pid) = module.status().unwrap().pid {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let digest = replace_program(&rig);
    owner
        .place_program("a", rig.path.join("program").to_str().unwrap(), &digest)
        .unwrap();
    module.restart(None).await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = module.status().unwrap();
            if status.pid.is_some_and(|pid| pid != first) {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    module.stop().await.unwrap();
    assert_eq!(status.restart_count, 0);
    assert_eq!(status.spawn_generation, 2);
}

#[cfg(unix)]
fn module_spec(rig: &Rig) -> crate::supervise::ModuleSpec {
    crate::daemon_config::load(rig.path.join("subc.jsonc"))
        .unwrap()
        .unwrap()
        .modules[0]
        .module_spec()
}

#[cfg(unix)]
fn replace_program(rig: &Rig) -> Digest {
    let path = rig.path.join("program");
    std::fs::copy(&path, rig.path.join("replacement")).unwrap();
    std::fs::rename(rig.path.join("replacement"), &path).unwrap();
    subc_config::raw_digest(&std::fs::read(path).unwrap())
}

#[cfg(all(unix, feature = "dev-placement"))]
#[tokio::test]
async fn first_party_mark_placement_without_a_qualifying_manifest_lowers_and_rules_out_spawns() {
    use crate::{
        program_origin::LaunchWitness,
        spawn_authority::{MarkStatus, SpawnAuthorityRecord},
    };
    use std::{sync::Arc, time::Duration};
    use subc_protocol::peer::CallerOrigin;
    let rig = program_rig();
    let owner = Arc::new(rig.owner("placement-mark"));
    rig.stamp(&owner);
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let _handler = crate::ControlHandler::with_forwarding(
        Arc::new(crate::Registry::default()),
        forwarding.clone(),
    )
    .with_config_runtime(owner.clone());
    let path = rig.path.join("program");
    let digest = replace_program(&rig);
    let manifest = json!({"module_id":"a", "hosts_third_party_code":false,
        "provenance":{"build_git_sha":"a".repeat(40)}});
    owner
        .place_program_with_manifest("a", path.to_str().unwrap(), &digest, Some(&manifest))
        .unwrap()
        .unwrap();

    // Two spawns admitted under that placement: one marked and serving a
    // route, one still waiting to report ready.
    let spec = module_spec(&rig);
    let witness = LaunchWitness::capture(&spec).unwrap();
    let launch = |spawn: u64| {
        let admission = owner.prepare_spawn(&spec).unwrap();
        let record = SpawnAuthorityRecord::new("incarnation", spawn, "a");
        admission.initialize_origin(&record, &witness);
        record.set_development_daemon_for_test(true);
        assert_eq!(record.capture().unwrap().origin, CallerOrigin::Development);
        let delivery = owner
            .delivery_spawned_for_admission_in_slot(
                "a",
                &admission,
                tokio::time::Instant::now(),
                Duration::from_secs(5),
                Duration::from_secs(5),
                true,
                Some(record.clone()),
            )
            .unwrap()
            .unwrap();
        (record, delivery)
    };
    let (marked, marked_spawn) = launch(1);
    owner.mark_ready(&marked_spawn).unwrap();
    assert!(marked.first_party(), "{:?}", marked.mark_status());
    let module_connection = ConnectionId::new(40);
    let (module_tx, _module_rx) = tokio::sync::mpsc::channel(32);
    forwarding
        .register_module_connection(
            module_connection,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(module_tx),
        )
        .unwrap();
    forwarding
        .bind_target_authority(module_connection, marked.clone())
        .unwrap();
    let (client_tx, _client_rx) = tokio::sync::mpsc::channel(32);
    let route = forwarding
        .begin_route_bind_relay_for_test(
            ConnectionId::new(41),
            crate::FrameSink::new(client_tx),
            1,
            "a",
        )
        .unwrap();
    forwarding
        .complete_pending_relay(
            module_connection,
            route.corr,
            crate::forwarding::RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    assert_eq!(forwarding.active_binding_count().unwrap(), 1);
    let (waiting, waiting_spawn) = launch(2);
    assert!(!waiting.first_party());

    // The same bytes are placed again with no manifest: a spawn admitted now
    // would have no development origin, so neither running spawn may keep
    // or earn the mark.
    let digest = replace_program(&rig);
    owner
        .place_program("a", path.to_str().unwrap(), &digest)
        .unwrap()
        .unwrap();
    assert!(!marked.first_party());
    assert_eq!(marked.mark_status(), MarkStatus::Lowered);
    assert_eq!(
        marked.epoch_for_test(),
        crate::spawn_authority::INITIAL_AUTHORITY_EPOCH + 1
    );
    assert_eq!(
        forwarding.active_binding_count().unwrap(),
        0,
        "the lowered spawn's route is closed"
    );
    owner.mark_ready(&waiting_spawn).unwrap();
    assert!(!waiting.first_party(), "{:?}", waiting.mark_status());
    assert_eq!(waiting.mark_status(), MarkStatus::Lowered);
}

#[cfg(all(unix, feature = "dev-placement"))]
#[test]
fn config_placement_preserves_active_pending_and_nack_restores_active() {
    let rig = program_rig();
    let bytes = br#"{"credential":{"vault_ref":"approved"},"limit":2}"#;
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), bytes).unwrap();
    let owner = rig.owner("placed-vectors");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let tuple = crate::config_store::Tuple {
        module: "a".into(),
        project: None,
    };
    owner
        .lock()
        .unwrap()
        .store
        .record_delivery(&tuple, 1, true)
        .unwrap();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"approved"},"limit":3}"#,
    )
    .unwrap();
    rig.stamp(&owner);
    let digest = replace_program(&rig);
    assert_eq!(
        owner
            .place_program("a", rig.path.join("program").to_str().unwrap(), &digest)
            .unwrap(),
        Some(3)
    );
    assert!(owner.prepare_spawn(&module_spec(&rig)).is_ok());
    let view = owner
        .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(view.active.unwrap().generation, 1);
    assert_eq!(view.pending.unwrap().generation, 2);
    {
        let mut state = owner.lock().unwrap();
        let body = state.store.body().unwrap();
        assert_eq!(body.versions[0].1.active, Some(1));
        assert_eq!(body.versions[0].1.pending, Some(2));
        assert!(body.generations[&3].generation.development);
        assert_eq!(
            body.generations[&3].generation.approval_channel,
            subc_config::ApprovalChannel::DevelopmentPlacement
        );
        state.store.record_delivery(&tuple, 2, false).unwrap();
    }
    let view = owner
        .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(view.active.unwrap().generation, 1);
    assert!(view.pending.is_none());
    drop(owner);
    let restarted = rig.owner("placed-restarted");
    assert!(restarted.prepare_spawn(&module_spec(&rig)).is_ok());
    assert!(restarted
        .status()
        .unwrap()
        .development_switches
        .contains(&"dev-placement".into()));
}

#[cfg(all(unix, feature = "dev-placement"))]
#[test]
fn config_placement_verifies_module_path_digest_and_is_superseded_by_approval() {
    let rig = program_rig();
    let owner = rig.owner("placement-identity");
    rig.stamp(&owner);
    let digest = replace_program(&rig);
    for (module, path, digest) in [
        ("missing", rig.path.join("program"), digest.clone()),
        ("a", rig.path.join("subc.jsonc"), digest.clone()),
        (
            "a",
            rig.path.join("program"),
            subc_config::raw_digest(b"wrong"),
        ),
    ] {
        assert_eq!(
            owner
                .place_program(module, path.to_str().unwrap(), &digest)
                .unwrap_err()
                .code,
            "program_refused"
        );
    }
    assert_eq!(owner.history().unwrap().0.len(), 1);
    owner
        .place_program("a", rig.path.join("program").to_str().unwrap(), &digest)
        .unwrap();
    assert!(owner.prepare_spawn(&module_spec(&rig)).is_ok());
    // The next approval is the program authority; an older placement must not
    // override it after a subsequent replacement.
    rig.stamp(&owner);
    replace_program(&rig);
    assert_eq!(
        owner.prepare_spawn(&module_spec(&rig)).err().unwrap().code,
        "program_refused"
    );
}

#[cfg(all(unix, feature = "dev-placement"))]
#[test]
fn config_placement_keeps_each_modules_program_through_history_pruning() {
    let rig = program_rig();
    let b = rig.path.join("program-b");
    std::fs::copy(rig.path.join("program"), &b).unwrap();
    let path = rig.path.join("subc.jsonc");
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["modules"]["b"] = json!({"program":b,"args":["600"],"protocol":"none"});
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    let owner = rig.owner("placement-retention");
    rig.stamp(&owner);
    let digest = replace_program(&rig);
    owner
        .place_program("a", rig.path.join("program").to_str().unwrap(), &digest)
        .unwrap();
    let digest = subc_config::raw_digest(&std::fs::read(&b).unwrap());
    for _ in 0..25 {
        owner
            .place_program("b", b.to_str().unwrap(), &digest)
            .unwrap();
    }
    assert!(owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .generations
        .contains_key(&2));
    assert!(owner.prepare_spawn(&module_spec(&rig)).is_ok());
}

#[cfg(all(unix, not(feature = "dev-placement")))]
#[test]
fn config_placement_release_refuses_op_and_persisted_development_identity() {
    use crate::config_gate::{GateSnapshot, InputKey};
    let rig = program_rig();
    let owner = rig.owner("release-placement");
    rig.stamp(&owner);
    let digest = replace_program(&rig);
    assert_eq!(
        owner
            .place_program("a", rig.path.join("program").to_str().unwrap(), &digest)
            .unwrap_err()
            .code,
        "program_refused"
    );
    // Reconstruct the durable record written by a development daemon, then open
    // it with this release-feature owner.
    {
        let mut state = owner.lock().unwrap();
        let body = state.store.body().unwrap();
        let approved = body.approved.clone();
        let mut entry = body.generations[&1].clone();
        let mut snapshot = GateSnapshot::from_store(&approved).unwrap();
        let key = InputKey::Program {
            module: "a".into(),
            path: rig.path.join("program").to_string_lossy().into_owned(),
        };
        let input = crate::config_gate::capture::capture_program(&rig.path.join("program"))
            .unwrap()
            .1;
        snapshot.inputs.insert(key, input);
        entry.configuration = snapshot.to_store().unwrap();
        entry.kind = crate::config_store::CommitKind::Development;
        entry.generation.development = true;
        entry.generation.approval_channel = subc_config::ApprovalChannel::DevelopmentPlacement;
        entry
            .generation
            .changeset
            .launch_manifests
            .insert("a".into(), json!({"program":"placed"}));
        entry.tuples.clear();
        state
            .store
            .commit_generation(entry, approved, snapshot.to_store().unwrap(), 0)
            .unwrap();
    }
    drop(owner);
    let restarted = rig.owner("release-restart");
    assert_eq!(
        restarted
            .prepare_spawn(&module_spec(&rig))
            .err()
            .unwrap()
            .code,
        "program_refused"
    );
    assert_eq!(
        restarted
            .prepare_spawn(&module_spec(&rig))
            .err()
            .unwrap()
            .code,
        "config_gated"
    );
    assert_eq!(
        restarted
            .take_activities()
            .iter()
            .filter(|a| a.event == "config.refused")
            .count(),
        1
    );
    assert_eq!(restarted.status().unwrap().pending_proposals, 1);
    rig.stamp(&restarted);
    assert!(restarted.prepare_spawn(&module_spec(&rig)).is_ok());
}

#[test]
fn config_placement_channel_is_refused_for_non_development_commits() {
    let rig = Rig::new();
    let owner = rig.owner("channel-kind");
    rig.stamp(&owner);
    let mut state = owner.lock().unwrap();
    let body = state.store.body().unwrap();
    let approved = body.approved.clone();
    let mut entry = body.generations[&1].clone();
    entry.generation.approval_channel = subc_config::ApprovalChannel::DevelopmentPlacement;
    for kind in [
        crate::config_store::CommitKind::Stamp,
        crate::config_store::CommitKind::Approved,
        crate::config_store::CommitKind::Unstamp,
        crate::config_store::CommitKind::Rollback,
    ] {
        entry.kind = kind;
        assert!(state
            .store
            .commit_generation(entry.clone(), approved.clone(), approved.clone(), 0)
            .is_err());
    }
    assert_eq!(state.store.body().unwrap().last_generation, 1);
}

#[test]
fn config_status_refuses_interpreters_instead_of_reporting_partial_coverage() {
    let rig = Rig::new();
    #[cfg(unix)]
    let interpreter = "/bin/sh";
    #[cfg(windows)]
    let interpreter = "python.exe";
    // Capture classifies the configured executable by name, without executing it.
    let path = rig.path.join(interpreter.rsplit('/').next().unwrap());
    std::fs::copy(rig.path.join("program"), &path).unwrap();
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&json!({
            "version":1,"modules":{"a":{"program":path}}
        }))
        .unwrap(),
    )
    .unwrap();
    let owner = rig.owner("interpreter-status");
    assert_eq!(owner.status().unwrap_err().code, "program_refused");
    assert_eq!(
        owner.prepare(PlanKind::Stamp).err().unwrap().code,
        "program_refused"
    );
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&json!({
            "version":1,"modules":{"a":{"program":rig.path.join("program")}}
        }))
        .unwrap(),
    )
    .unwrap();
    let status = owner.status().unwrap();
    assert!(status.interpreter_only_covered.is_empty());
    assert_eq!(
        serde_json::to_value(status).unwrap()["interpreter_only_covered"],
        json!([])
    );
}

#[test]
fn config_status_lists_startup_overrides_without_values() {
    const NAME: &str = "config_gate::runtime::tests::status_placement::config_status_lists_startup_overrides_without_values";
    if std::env::var("SUBC_PORT").ok().as_deref() != Some("19473") {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env("SUBC_PORT", "19473")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let rig = Rig::new();
    let owner = rig.owner("environment-status");
    // This branch runs alone in a child test process, so changing its environment
    // cannot race another test. Status must retain the startup fact, not re-read it.
    std::env::remove_var("SUBC_PORT");
    let status = owner.status().unwrap();
    assert!(
        status.ungated_inputs.contains(&"SUBC_PORT".into()),
        "{:?}",
        status.ungated_inputs
    );
    assert!(!serde_json::to_string(&status).unwrap().contains("19473"));
}
