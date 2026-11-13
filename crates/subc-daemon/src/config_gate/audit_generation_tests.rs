use super::*;

#[cfg(unix)]
#[tokio::test]
async fn audit_registration_fence_release_preserves_exhausted_budget_and_slow_retry() {
    terminal_fence_release(true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn audit_registration_fence_release_preserves_deliberate_clean_exit() {
    terminal_fence_release(false).await;
}

#[cfg(unix)]
async fn terminal_fence_release(crash: bool) {
    use crate::supervise::ModuleState;
    let (rig, owner, supervisor, _) = supervised_fixture_with_program(true, false,
        b"#!/bin/sh\nprintf 'started\\n' >> \"$2\"\nwhile [ ! -e \"$1\" ]; do /bin/sleep 0.01; done\nexit 0\n");
    let module = supervisor
        .spawn_async(supervised_spec(&rig, "a"))
        .await
        .unwrap();
    if crash {
        module.seed_crash_budget_for_test(3);
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(module.status().unwrap().pid.unwrap() as i32).unwrap(),
            rustix::process::Signal::KILL,
        )
        .unwrap();
    } else {
        std::fs::write(rig.path.join("a.seen"), b"exit cleanly").unwrap();
    }
    let terminal = if crash {
        ModuleState::Failed
    } else {
        ModuleState::Stopped
    };
    wait_for_supervised_stage(&module, "terminal exit behind delivery fence", || {
        module.state().unwrap() == terminal && owner.delivery_exit_fence("a").unwrap().is_some()
    })
    .await;
    let before = module.status().unwrap();
    let retry = module.retry_deadline_for_test();
    assert_eq!(retry.is_some(), crash);
    let exited = owner
        .delivery_exits()
        .unwrap()
        .into_iter()
        .find(|exit| exit.module == "a")
        .unwrap();
    owner.delivery_release_exit(&exited).unwrap();
    assert!(owner
        .lock()
        .unwrap()
        .admission_releases
        .iter()
        .any(|module| module == "a"));
    // Await the worker's actor command, not a timer: a module that exited on
    // its own must ignore the release.
    let released = module.release_config_hold().await.unwrap();
    let after = module.status().unwrap();
    let after_retry = module.retry_deadline_for_test();
    module.stop().await.unwrap();
    assert!(
        !released,
        "a delivery fence did not park the terminal actor"
    );
    assert_eq!(after.state, terminal);
    assert!(!after.process_alive);
    assert_eq!(after.spawn_generation, before.spawn_generation);
    assert_eq!(after.restart_count, before.restart_count);
    assert_eq!(
        after_retry, retry,
        "release must not cancel or reschedule slow retry"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn audit_registration_hold_keeps_disabled_actor_disabled() {
    use crate::supervise::ModuleState;
    let (rig, owner, supervisor, _) = supervised_fixture(false);
    let module = supervisor
        .supervise_configured_requested_async(
            supervised_spec(&rig, "a"),
            false,
            Default::default(),
            None,
            Default::default(),
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    let path = rig.path.join("subc.jsonc");
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc["modules"]["a"]["registration_channel"] = json!(false);
    std::fs::write(path, doc.to_string()).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::Live]);
    module.config_exit_hold().await.unwrap();
    let status = module.status().unwrap();
    module.stop().await.unwrap();
    assert_eq!(status.state, ModuleState::Disabled);
    assert!(!status.enabled);
    assert!(status.admission_refused.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn audit_registration_fence_only_approved_enable_releases_its_parked_actor() {
    use crate::supervise::ModuleState;
    let (rig, owner, supervisor, _) = supervised_fixture(false);
    std::fs::write(rig.path.join("a.seen"), b"keep running").unwrap();
    let module = supervisor
        .supervise_configured_requested_async(
            supervised_spec(&rig, "a"),
            false,
            Default::default(),
            None,
            Default::default(),
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    let spawn = owner
        .spawned(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(120),
            Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
    owner.delivery_child_exited(&spawn).unwrap();
    let approved = owner.authority_exit_configuration(1).unwrap().unwrap().3;
    module
        .reconcile_approved_configuration(approved)
        .await
        .unwrap();
    let staged = module.status().unwrap();
    let exited = owner
        .delivery_exits()
        .unwrap()
        .into_iter()
        .find(|exit| exit.module == "a")
        .unwrap();
    owner.delivery_release_exit(&exited).unwrap();
    let released = module.release_config_hold().await.unwrap();
    let after = module.status().unwrap();
    module.stop().await.unwrap();
    assert!(staged.enabled && !staged.process_alive);
    assert_eq!(
        staged.state,
        ModuleState::Stopped,
        "an accepted enable must be parked, not stranded Disabled"
    );
    assert!(
        released && after.process_alive,
        "clearing its only fence must resume the staged enable"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn audit_registration_deferred_generation_survives_two_active_child_exits() {
    use std::sync::Arc;
    let (rig, owner, supervisor, handler) = supervised_fixture(false);
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    std::fs::write(rig.path.join("a.seen"), b"keep running").unwrap();
    let module = supervisor
        .spawn_async(supervised_spec(&rig, "a"))
        .await
        .unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::DaemonRestart]);
    let worker = handler.spawn_config_delivery_worker().unwrap();
    for _ in 0..2 {
        let before = module.status().unwrap();
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(before.pid.unwrap() as i32).unwrap(),
            rustix::process::Signal::KILL,
        )
        .unwrap();
        wait_for_supervised_stage(
            &module,
            "active child replaced under daemon deferral",
            || {
                let status = module.status().unwrap();
                status.process_alive && status.spawn_generation > before.spawn_generation
            },
        )
        .await;
    }
    let (still_pending, never_redeliver) = {
        let mut state = owner.lock().unwrap();
        let body = state.store.body().unwrap();
        (
            body.versions
                .iter()
                .find(|(tuple, _)| tuple.module == "a" && tuple.project.is_none())
                .unwrap()
                .1
                .pending,
            body.generations[&2].generation.never_redeliver.clone(),
        )
    };
    worker.abort();
    let _ = worker.await;
    module.stop().await.unwrap();
    drop((module, supervisor, handler, owner));
    assert_eq!(
        still_pending,
        Some(2),
        "neither active child attempted the daemon-restart generation"
    );
    assert!(!never_redeliver.iter().any(|module| module == "a"));
    let restarted = Arc::new(rig.owner("daemon-after-deferred-exits"));
    rig.declare(&restarted, INCUMBENT.get());
    let spawn = restarted
        .spawned(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(120),
            Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
    restarted.registered(INCUMBENT, &spawn).unwrap();
    let view = pending(&restarted, INCUMBENT);
    assert_eq!(view.generation, 2);
    restarted.ready(INCUMBENT, "a", 2).unwrap();
    assert_eq!(
        applied(&restarted, INCUMBENT, &view, true).unwrap(),
        AppliedDisposition::Active
    );
    assert_active(&restarted, INCUMBENT, 2);
}

#[tokio::test]
async fn audit_registration_list_captures_once_for_all_rows() {
    capture_once_for_all_rows("supervisor.list").await;
}

#[tokio::test]
async fn audit_registration_health_captures_once_for_all_rows() {
    capture_once_for_all_rows("supervisor.health").await;
}

async fn capture_once_for_all_rows(operation: &str) {
    use std::sync::{atomic::Ordering, Arc};
    let list_request = operation == "supervisor.list";
    {
        let rig = Arc::new(Rig::new());
        let path = rig.path.join("subc.jsonc");
        let config = json!({"version":1,"modules":{
            "a":{"program":rig.path.join("program"),"enabled":false,"registration_channel":operation != "supervisor.list"},
            "b":{"program":rig.path.join("program"),"enabled":false,"registration_channel":true}}});
        std::fs::write(&path, config.to_string()).unwrap();
        let owner = Arc::new(rig.owner("multi-row-publication"));
        rig.stamp(&owner);
        let registry = Arc::new(crate::Registry::default());
        let handle = crate::supervise::SupervisorHandle::new();
        let supervisor =
            crate::supervise::Supervisor::new_for_test(registry.clone(), Default::default())
                .with_handle(handle.clone())
                .with_config_runtime(owner.clone());
        let configuration = crate::daemon_config::load(&path).unwrap().unwrap();
        let mut modules = Vec::new();
        for (index, entry) in configuration.modules.into_iter().enumerate() {
            modules.push(
                supervisor
                    .supervise_configured_requested_async(
                        entry.module_spec(),
                        operation == "supervisor.list" && index == 0,
                        entry.health,
                        entry.drain_timeout_ms,
                        entry.restart,
                        entry.register_timeout,
                        None,
                    )
                    .await
                    .unwrap(),
            );
            registry
                .register_with_control_ops(
                    subc_protocol::manifest::ModuleManifest::builder(entry.module_id, "1")
                        .registration_channel(false)
                        .build(),
                    subc_protocol::PROTOCOL_VERSION,
                    ConnectionId::new(100 + index as u64),
                    vec![],
                )
                .unwrap();
        }
        let handler = Arc::new(
            crate::ControlHandler::new(registry.clone())
                .with_supervisor(handle)
                .with_config_runtime(owner.clone()),
        );
        let during_owner = owner.clone();
        let during_rig = rig.clone();
        *owner.polled_capture_hook.lock().unwrap() = Some(Box::new(move || {
            let mut next = config;
            for module in ["a", "b"] {
                next["modules"][module]["registration_channel"] = json!(false);
                next["modules"][module]["enabled"] = json!(true);
            }
            if list_request {
                next["modules"]["a"]["registration_channel"] = json!(true);
            }
            std::fs::write(during_rig.path.join("subc.jsonc"), next.to_string()).unwrap();
            approve(&during_rig, &during_owner, 2);
        }));
        let before = owner.polled_capture_count.load(Ordering::Relaxed);
        let response = registration_declaration_poll(handler, operation).await;
        assert_eq!(
            owner.polled_capture_count.load(Ordering::Relaxed) - before,
            1,
            "{operation}: {response}"
        );
        assert_eq!(
            response["modules"].as_array().unwrap().len(),
            2,
            "{response}"
        );
        if operation == "supervisor.health" {
            for entry in response["modules"].as_array().unwrap() {
                assert!(
                    entry["detail"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("HELLO registration_channel"),
                    "{response}"
                );
            }
        } else {
            for entry in response["modules"].as_array().unwrap() {
                assert_eq!(entry["configured_enabled"], false, "{response}");
            }
            let refused = response["modules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["module_id"] == "a")
                .unwrap();
            assert_eq!(
                refused["admission_refused"],
                subc_control::REGISTRATION_CHANNEL_UNSUPPORTED,
                "the held row must use the captured undeclared generation: {response}"
            );
        }
        for index in 0..2 {
            registry
                .deregister_connection(ConnectionId::new(100 + index))
                .unwrap();
        }
        for module in modules {
            module.stop().await.unwrap();
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn audit_registration_nack_keeps_in_force_declaration_across_respawn() {
    let (rig, owner, supervisor, handler) = supervised_fixture(false);
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    std::fs::write(rig.path.join("a.seen"), b"keep running").unwrap();
    let path = rig.path.join("subc.jsonc");
    let (config, _) =
        crate::daemon_config::parse_with_activity(&std::fs::read_to_string(&path).unwrap(), &path)
            .unwrap();
    let launch = config
        .modules
        .iter()
        .find(|entry| entry.module_id == "a")
        .unwrap()
        .module_spec();
    let module = supervisor.spawn_async(launch).await.unwrap();
    let initial = module.status().unwrap().spawn_generation;
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    document["modules"]["a"]["registration_channel"] = json!(false);
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    write(&rig, 2);
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::Live]);
    let rejected = pending(&owner, INCUMBENT);
    applied(&owner, INCUMBENT, &rejected, false).unwrap();
    assert_eq!(
        owner.registration_declaration("a").as_deref(),
        Some("declared")
    );
    module.restart(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while module.status().unwrap().spawn_generation == initial {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(module.status().unwrap().process_alive);
    let list = registration_declaration_poll(handler.clone(), "supervisor.list").await;
    let a = list["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["module_id"] == "a")
        .unwrap();
    assert!(a.get("admission_refused").is_none(), "{a}");
    let provenance = registration_declaration_poll(handler, "supervisor.provenance").await;
    let a = provenance["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["module_id"] == "a")
        .unwrap();
    assert_eq!(a["registration_declaration"], "declared", "{a}");
    module.stop().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn audit_registration_next_spawn_declaration_is_not_the_running_spawn_declaration() {
    declaration_after_crash(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn audit_registration_daemon_restart_declaration_does_not_restrict_current_respawn() {
    declaration_after_crash(true).await;
}

#[cfg(unix)]
async fn declaration_after_crash(deferred: bool) {
    use crate::supervise::ModuleState;
    let (rig, owner, supervisor, handler) = supervised_fixture(false);
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    std::fs::write(rig.path.join("a.seen"), b"keep running").unwrap();
    let module = supervisor
        .spawn_async(supervised_spec(&rig, "a"))
        .await
        .unwrap();
    let before = module.status().unwrap();
    let path = rig.path.join("subc.jsonc");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    document["modules"]["a"]["registration_channel"] = json!(false);
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    approve_kinds(
        &rig,
        &owner,
        &[if deferred {
            subc_config::ApplyKind::DaemonRestart
        } else {
            subc_config::ApplyKind::NextSupervision
        }],
    );
    let running = registration_declaration_poll(handler.clone(), "supervisor.provenance").await;
    let a = running["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["module_id"] == "a")
        .unwrap();
    assert_eq!(a["registration_declaration"], "declared", "{running}");
    let list = registration_declaration_poll(handler.clone(), "supervisor.list").await;
    let a = list["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["module_id"] == "a")
        .unwrap();
    assert!(a.get("admission_refused").is_none(), "{list}");
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(before.pid.unwrap() as i32).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    wait_for_supervised_stage(&module, "post-crash admission selected", || {
        let status = module.status().unwrap();
        if deferred {
            status.spawn_generation > before.spawn_generation && status.process_alive
        } else {
            status.state == ModuleState::Stopped && !status.process_alive
        }
    })
    .await;
    let after = module.status().unwrap();
    if deferred {
        assert_eq!(after.registration_declaration.as_deref(), Some("declared"));
        assert_eq!(after.restart_count, 1);
    } else {
        assert_eq!(after.spawn_generation, before.spawn_generation);
        assert_eq!(after.restart_count, before.restart_count);
        assert_eq!(
            after.admission_refused.as_deref(),
            Some(subc_control::REGISTRATION_CHANNEL_UNSUPPORTED)
        );
        assert!(module.next_retry_at_ms().unwrap().is_none());
        let stopped = registration_declaration_poll(handler, "supervisor.provenance").await;
        let a = stopped["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["module_id"] == "a")
            .unwrap();
        assert_eq!(a["registration_declaration"], "undeclared", "{stopped}");
    }
    module.stop().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn audit_registration_release_consumed_behind_exit_fence_is_retried_after_clear() {
    use std::sync::Arc;
    let (rig, owner, supervisor, handler) = supervised_fixture(true);
    std::fs::write(rig.path.join("a.seen"), b"keep running").unwrap();
    let module = supervisor
        .spawn_async(supervised_spec(&rig, "a"))
        .await
        .unwrap();
    let before = module.status().unwrap();
    let fenced_spawn = owner
        .spawned(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(120),
            Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
    owner.delivery_child_exited(&fenced_spawn).unwrap();
    let path = rig.path.join("subc.jsonc");
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc["modules"]["a"]["registration_channel"] = json!(false);
    std::fs::write(&path, doc.to_string()).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::Live]);
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(before.pid.unwrap() as i32).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    wait_for_supervised_stage(&module, "exit fence installed", || {
        !module.status().unwrap().process_alive && owner.delivery_exit_fence("a").unwrap().is_some()
    })
    .await;
    let gate = owner.module_gate("a").unwrap();
    let held = gate.lock_owned().await;
    doc["modules"]["a"]["registration_channel"] = json!(true);
    std::fs::write(&path, doc.to_string()).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::Live]);
    let exited = owner
        .delivery_exits()
        .unwrap()
        .into_iter()
        .find(|exit| exit.module == "a")
        .unwrap();
    let (tx, mut wakes) = tokio::sync::mpsc::unbounded_channel();
    let handler = Arc::new((*handler).clone().with_delivery_wakes_for_test(tx));
    let worker = handler.spawn_config_delivery_worker().unwrap();
    // The next reported wake follows a full pass that consumed the release;
    // exit reconciliation cannot clear the fence while its module gate is held.
    wakes.recv().await.unwrap();
    assert!(owner.lock().unwrap().admission_releases.is_empty());
    assert!(!module.status().unwrap().process_alive);
    owner.delivery_release_exit(&exited).unwrap();
    drop(held);
    wait_for_supervised_stage(&module, "release after exit fence", || {
        module.status().unwrap().process_alive
    })
    .await;
    let after = module.status().unwrap();
    worker.abort();
    let _ = worker.await;
    module.stop().await.unwrap();
    assert!(after.spawn_generation > before.spawn_generation);
}

#[tokio::test]
async fn audit_registration_status_uses_one_publication_across_a_commit() {
    let rig = std::sync::Arc::new(Rig::new());
    std::fs::write(
        rig.path.join("subc.jsonc"),
        br#"{"version":1,"modules":{}}"#,
    )
    .unwrap();
    let owner = std::sync::Arc::new(rig.owner("atomic-status"));
    rig.stamp(&owner);
    owner.complete_authority_exit(1).unwrap();
    let handler = std::sync::Arc::new(
        crate::ControlHandler::new(std::sync::Arc::new(crate::Registry::default()))
            .with_config_runtime(owner.clone()),
    );
    let revision = owner.polled_status().unwrap().revision;
    let during_owner = owner.clone();
    let during_rig = rig.clone();
    *owner.polled_capture_hook.lock().unwrap() = Some(Box::new(move || {
        let path = during_rig.path.join("subc.jsonc");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        document["route_bind_relay_timeout_ms"] = json!(30000);
        std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
        approve(&during_rig, &during_owner, 2);
    }));
    let old = registration_declaration_poll(handler.clone(), "config.status").await;
    assert_eq!(old["status"]["revision"], revision);
    assert!(
        !old["status"]["approved_but_inactive"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["entry"] == "route_bind_relay_timeout_ms"),
        "{old}"
    );
    let new = registration_declaration_poll(handler, "config.status").await;
    assert!(new["status"]["revision"].as_u64().unwrap() > revision);
    assert_eq!(new["status"]["pending_proposals"], 0);
    assert!(
        new["status"]["approved_but_inactive"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["entry"] == "route_bind_relay_timeout_ms" && row["generation"] == 2),
        "{new}"
    );
}
