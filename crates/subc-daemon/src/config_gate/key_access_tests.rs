use super::*;
use crate::config_store::TestStoreKey;
use subc_os::config_keychain::KeychainError;

fn owner(rig: &Rig, key: &TestStoreKey) -> RuntimeOwner {
    RuntimeOwner::new(
        "key-access".into(),
        ConfigStore::open_sealed_for_tests(
            StoreRoot::from_service_definition(rig.path.join("store")).unwrap(),
            key.clone(),
        ),
        ConfigCapturer {
            daemon_path: rig.path.join("subc.jsonc"),
            config_home: rig.path.clone(),
            projects: std::sync::Arc::new(super::super::super::capture::OfflineProjectRegistry),
        },
    )
}

fn unstamp(rig: &Rig, owner: &RuntimeOwner) {
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Unstamp).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    let generation = owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            2,
        )
        .unwrap();
    owner.complete_authority_exit_for_test(generation).unwrap();
    assert!(rig.path.join("store/approved.json").is_file());
}

fn spec(rig: &Rig) -> crate::supervise::ModuleSpec {
    crate::daemon_config::load(rig.path.join("subc.jsonc"))
        .unwrap()
        .unwrap()
        .modules[0]
        .module_spec()
}

#[test]
fn config_key_access_unstamped_runs_and_reports_degraded() {
    for retained in [false, true] {
        for (error, class) in [
            (KeychainError::Denied, "Denied"),
            (KeychainError::Unavailable, "Unavailable"),
            (KeychainError::OsStatus(-25308), "OsStatus"),
            (KeychainError::InvalidData, "InvalidData"),
        ] {
            let rig = Rig::new();
            std::fs::write(
                rig.path.join("cortexkit/a.jsonc"),
                br#"{"credential":{"vault_ref":"test-reference"},"limit":7}"#,
            )
            .unwrap();
            let key = TestStoreKey::default();
            if retained {
                let before = owner(&rig, &key);
                rig.stamp(&before);
                unstamp(&rig, &before);
            }
            key.fail_load(Some(error));
            let degraded = owner(&rig, &key);
            let admission = degraded
                .prepare_spawn(&spec(&rig))
                .expect("unstamped launch");
            assert!(!admission.gated());
            degraded.check_spawn("a", &admission).unwrap();
            assert!(!degraded.is_recovering());
            rig.declare(&degraded, 10);
            let view = degraded
                .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
                .unwrap()
                .active
                .unwrap();
            assert_eq!(view.status, ConfigStatus::Unstamped);
            assert_eq!(view.generation, 0);
            assert_eq!(view.effective["limit"], 7);
            let status = degraded.status().unwrap();
            assert_eq!(status.state, "unstamped");
            assert!(
                status.warnings.iter().any(|warning| {
                    warning.contains("degraded")
                        && warning.contains("unverified record")
                        && warning.contains(class)
                }),
                "{:?}",
                status.warnings
            );
            assert_eq!(status.revision, 0, "unverified revisions are not imported");
            assert_eq!(status.base_revision, 0);
            assert!(degraded
                .lock()
                .unwrap()
                .store
                .body()
                .unwrap()
                .generations
                .is_empty());
            assert_eq!(degraded.polled_status().unwrap().warnings, status.warnings);
        }
    }
}

#[test]
fn config_key_access_stamped_still_holds() {
    for error in [
        Some(KeychainError::Denied),
        Some(KeychainError::Unavailable),
        Some(KeychainError::OsStatus(-25308)),
        None,
    ] {
        let rig = Rig::new();
        let key = TestStoreKey::default();
        rig.stamp(&owner(&rig, &key));
        if error.is_none() {
            key.remove();
        }
        key.fail_load(error);
        let held = owner(&rig, &key);
        assert_eq!(
            held.prepare_spawn(&spec(&rig)).err().unwrap().code,
            "config_gated"
        );
        assert!(held.is_recovering());
        assert_eq!(held.status().unwrap().state, "store_unverified");
    }
}

#[test]
fn config_key_access_unsupported_backend_does_not_gain_fallback() {
    let rig = Rig::new();
    let key = TestStoreKey::default();
    let before = owner(&rig, &key);
    rig.stamp(&before);
    unstamp(&rig, &before);
    drop(before);
    let unsupported = rig.owner("unsupported-backend");
    assert!(unsupported.prepare_spawn(&spec(&rig)).is_err());
    assert_eq!(unsupported.status().unwrap().state, "store_unverified");
}

#[test]
fn config_key_access_stamp_refuses_unreadable_key() {
    let rig = Rig::new();
    let key = TestStoreKey::default();
    key.fail_load(Some(KeychainError::Denied));
    let degraded = owner(&rig, &key);
    let plan = degraded
        .finish_plan(degraded.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    degraded
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = degraded
        .begin_approval(&plan.digest, &Rig::facts())
        .unwrap();
    let error = degraded
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            1,
        )
        .expect_err("stamping requires readable key access");
    assert_eq!(error.code, "store_unverified");
    assert!(!rig.path.join("store/approved.json").exists());
}

#[test]
fn config_key_access_missing_expected_key_runs_but_cannot_stamp() {
    let rig = Rig::new();
    let key = TestStoreKey::default();
    let before = owner(&rig, &key);
    rig.stamp(&before);
    unstamp(&rig, &before);
    drop(before);
    key.remove();
    let degraded = owner(&rig, &key);
    let admission = degraded.prepare_spawn(&spec(&rig)).unwrap();
    degraded.check_spawn("a", &admission).unwrap();
    assert!(degraded
        .status()
        .unwrap()
        .warnings
        .iter()
        .any(|warning| { warning.contains("degraded") && warning.contains("Missing") }));
    let bytes = std::fs::read(rig.path.join("store/approved.json")).unwrap();
    let plan = degraded
        .finish_plan(degraded.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    degraded
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = degraded
        .begin_approval(&plan.digest, &Rig::facts())
        .unwrap();
    assert_eq!(
        degraded
            .finish_approval(
                challenge.attempt(),
                &Rig::facts(),
                ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
                3,
            )
            .unwrap_err()
            .code,
        "store_unverified"
    );
    assert_eq!(
        std::fs::read(rig.path.join("store/approved.json")).unwrap(),
        bytes
    );
}

#[test]
fn config_key_access_unreadable_key_and_malformed_record_still_holds() {
    for bytes in [b"truncated".as_slice(), br#"{"format_version":999}"#] {
        let rig = Rig::new();
        std::fs::write(rig.path.join("store/approved.json"), bytes).unwrap();
        let key = TestStoreKey::default();
        key.fail_load(Some(KeychainError::Denied));
        let held = owner(&rig, &key);
        assert_eq!(
            held.prepare_spawn(&spec(&rig)).err().unwrap().code,
            "config_gated"
        );
        assert_eq!(held.status().unwrap().state, "store_unverified");
    }
}

#[test]
fn config_key_access_restored_key_rechecks_and_holds_stamped_record() {
    let rig = Rig::new();
    let key = TestStoreKey::default();
    let before = owner(&rig, &key);
    rig.stamp(&before);
    let stamped = std::fs::read(rig.path.join("store/approved.json")).unwrap();
    drop(before);
    let mut unverified: serde_json::Value = serde_json::from_slice(&stamped).unwrap();
    unverified["body"]["stamped"] = json!(false);
    std::fs::write(
        rig.path.join("store/approved.json"),
        serde_json::to_vec(&unverified).unwrap(),
    )
    .unwrap();
    key.fail_load(Some(KeychainError::Denied));
    let degraded = owner(&rig, &key);
    let admission = degraded.prepare_spawn(&spec(&rig)).unwrap();
    // A readable anchor that really covers a stamped record must revoke the
    // degraded launch admission, even if the daemon booted without key access.
    std::fs::write(rig.path.join("store/approved.json"), stamped).unwrap();
    key.fail_load(None);
    assert!(degraded.check_recovery().unwrap());
    assert!(degraded.check_spawn("a", &admission).is_err());
}

#[test]
fn config_key_access_recovery_updates_published_unstamped_status() {
    let rig = Rig::new();
    let key = TestStoreKey::default();
    key.fail_load(Some(KeychainError::Denied));
    let degraded = owner(&rig, &key);
    assert!(degraded
        .polled_status()
        .unwrap()
        .warnings
        .iter()
        .any(|warning| warning.contains("Denied")));
    key.fail_load(Some(KeychainError::Unavailable));
    assert!(!degraded.check_recovery().unwrap());
    assert!(degraded
        .polled_status()
        .unwrap()
        .warnings
        .iter()
        .any(|warning| warning.contains("Unavailable")));
    key.fail_load(None);
    assert!(!degraded.check_recovery().unwrap());
    assert!(!degraded
        .polled_status()
        .unwrap()
        .warnings
        .iter()
        .any(|warning| warning.contains("degraded")));
}

#[tokio::test]
async fn config_key_access_degraded_lifecycle_preserves_sealed_record_and_recovers() {
    use crate::supervise::{RestartPolicy, Supervisor, SupervisorHandle};
    use std::sync::Arc;
    let rig = Rig::new();
    let config_path = rig.path.join("subc.jsonc");
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["modules"]["a"]["enabled"] = json!(false);
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        br#"{"credential":{"vault_ref":"test-reference"},"limit":7}"#,
    )
    .unwrap();
    let key = TestStoreKey::default();
    let sealed = owner(&rig, &key);
    rig.declare(&sealed, 10);
    rig.stamp(&sealed);
    unstamp(&rig, &sealed);
    drop(sealed);
    let record_path = rig.path.join("store/approved.json");
    let before = std::fs::read(&record_path).unwrap();
    key.fail_load(Some(KeychainError::Denied));
    let degraded = Arc::new(owner(&rig, &key));
    assert_eq!(
        std::fs::read(&record_path).unwrap(),
        before,
        "boot wrote the record"
    );
    degraded
        .lock()
        .unwrap()
        .store
        .record_daemon_ready()
        .unwrap();
    assert_eq!(
        std::fs::read(&record_path).unwrap(),
        before,
        "ready wrote the record"
    );

    let admission = degraded.prepare_spawn(&spec(&rig)).unwrap();
    degraded.check_spawn("a", &admission).unwrap();
    rig.declare(&degraded, 20);
    let view = degraded
        .module_get(ConnectionId::new(20), "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(
        view.active.as_ref().unwrap().status,
        ConfigStatus::Unstamped
    );
    degraded
        .module_view_delivered(
            ConnectionId::new(20),
            "a",
            &view,
            tokio::time::Instant::now(),
            true,
        )
        .unwrap();
    assert!(degraded
        .ready(ConnectionId::new(20), "a", 0)
        .unwrap()
        .is_none());
    assert_eq!(
        std::fs::read(&record_path).unwrap(),
        before,
        "module delivery wrote the record"
    );

    let registry = Arc::new(crate::Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(degraded.clone());
    let handler = crate::ControlHandler::new(registry)
        .with_supervisor(handle)
        .with_supervisor_rescan(supervisor, &config_path, None)
        .with_config_runtime(degraded.clone());
    let (sender, _receiver) = tokio::sync::mpsc::channel(16);
    let context = crate::RouteCtx {
        connection_id: ConnectionId::new(50),
        egress: crate::FrameSink::new(sender),
    };
    for preview in [true, false] {
        let frame = subc_protocol::Frame::build(
            subc_protocol::FrameType::Request,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&json!({"op":"supervisor.rescan", "preview":preview})).unwrap(),
        )
        .unwrap();
        let replies = handler.handle_control_frame(&context, frame).await.unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(
            replies[0].header.ty,
            subc_protocol::FrameType::Response,
            "{}",
            String::from_utf8_lossy(&replies[0].body)
        );
        assert_eq!(
            std::fs::read(&record_path).unwrap(),
            before,
            "rescan wrote the record"
        );
    }

    key.fail_load(None);
    assert!(!degraded.check_recovery().unwrap());
    let status = degraded.status().unwrap();
    assert_eq!(status.state, "unstamped");
    assert_eq!(status.integrity, "sealed");
    assert_eq!(status.revision, 2);
    assert!(!status
        .warnings
        .iter()
        .any(|warning| warning.contains("degraded")));
    let admission = degraded.prepare_spawn(&spec(&rig)).unwrap();
    degraded.check_spawn("a", &admission).unwrap();
    assert_eq!(
        std::fs::read(&record_path).unwrap(),
        before,
        "key restoration changed the record"
    );
}

#[test]
fn config_key_access_warns_once_per_boot_without_record_contents() {
    #[derive(Clone)]
    struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = Log(bytes.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let rig = Rig::new();
        let key = TestStoreKey::default();
        key.fail_load(Some(KeychainError::Denied));
        let degraded = owner(&rig, &key);
        for _ in 0..3 {
            degraded.prepare_spawn(&spec(&rig)).unwrap();
            degraded.status().unwrap();
        }
    });
    let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert_eq!(log.matches("WARN").count(), 1, "{log}");
    assert!(log.contains("Denied"), "{log}");
    assert!(!log.contains("StoreKey"), "{log}");
}

#[cfg(unix)]
#[tokio::test]
async fn config_key_access_unstamped_supervisor_launches_module() {
    use crate::supervise::{RestartPolicy, Supervisor};
    use std::{sync::Arc, time::Duration};
    let rig = Rig::new();
    let program = crate::config_gate::program::native_fixture_for_test();
    std::fs::write(rig.path.join("subc.jsonc"), serde_json::to_vec(&json!({
        "version": 1, "modules": {"a": {"program": program, "args": ["600"], "protocol": "none"}}
    })).unwrap()).unwrap();
    let key = TestStoreKey::default();
    key.fail_load(Some(KeychainError::Denied));
    let degraded = Arc::new(owner(&rig, &key));
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(degraded);
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let running = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if module.status().unwrap().process_alive {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    module.set_enabled(false).await.unwrap();
    module.stop().await.unwrap();
    running.expect("unstamped module runs despite unreadable sealing key");
}
