use super::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_desktop_background_waiters_do_not_retain_the_config_store_owner() {
    let (_scratch, owner, _view) = review_fixture();
    let weak_owner = Arc::downgrade(&owner);
    let handler =
        Arc::new(ControlHandler::new(Arc::new(Registry::default())).with_config_runtime(owner));
    handler.clone().spawn_capability_deadline_loop();
    handler.clone().spawn_scope_expiry_loop();
    drop(handler);
    tokio::task::yield_now().await;
    assert!(
        weak_owner.upgrade().is_none(),
        "background waiters must not retain the lifetime store lock after the daemon is dropped"
    );
}

fn owner_peer(handler: &ControlHandler, connection: ConnectionId) {
    handler.record_local_connection(connection);
    #[cfg(unix)]
    let observed = subc_protocol::peer::ObservedPeer::observed_uid(subc_os::effective_uid());
    #[cfg(windows)]
    let observed = subc_protocol::peer::ObservedPeer::observed_sid(
        subc_os::named_pipe::current_user_sid_string().unwrap(),
    );
    handler
        .observed_peers
        .lock()
        .unwrap()
        .insert(connection, observed);
    handler.captured_peers.lock().unwrap().insert(
        connection,
        subc_os::local_peer::ObservedPeer::Unobserved.into(),
    );
}

#[test]
fn config_desktop_os_presence_requires_both_provider_and_policy() {
    assert!(!config_os_prompt_available(false, false));
    assert!(!config_os_prompt_available(false, true));
    assert!(!config_os_prompt_available(true, false));
    assert!(config_os_prompt_available(true, true));
}

#[cfg(feature = "dev-desktop-approval")]
#[test]
fn config_desktop_development_accepts_owner_local_review_without_app_verification() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let connection = ConnectionId::new(700);
    owner_peer(&handler, connection);
    assert!(handler.config_owner_connection(connection));
    assert!(handler.config_desktop_identity(connection).unwrap());
    assert!(handler.config_desktop_stamps.lock().unwrap().is_empty());
}

#[cfg(not(feature = "dev-desktop-approval"))]
#[test]
fn config_desktop_production_owner_uid_and_path_observations_are_not_attestation() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let connection = ConnectionId::new(700);
    owner_peer(&handler, connection);
    assert!(handler.config_owner_connection(connection));
    assert!(!handler.config_desktop_identity(connection).unwrap());
}

#[cfg(all(windows, not(feature = "dev-desktop-approval")))]
#[test]
fn config_desktop_windows_connection_keeps_its_launch_incarnation_without_prompt_exemption() {
    use subc_os::local_peer::{ObservedPeer as OsPeer, PeerObservation, PeerUser};
    let handler = ControlHandler::new(Arc::new(Registry::default())).with_test_operator_app_policy(
        crate::operator_apps::TestOperatorAppPolicy {
            macos_enabled: false,
            windows_enabled: false,
            linux_enabled: false,
            app: "desktop-fixture",
            requirement: "always",
            floor: semver::Version::new(1, 2, 10),
            windows_publisher: None,
            windows_path: None,
            linux_path: None,
            windows_breakaway: true,
            linux_force_direct: false,
        },
    );
    let own_pid = std::process::id();
    let process = || subc_os::Process::open(own_pid).unwrap().unwrap();
    let first = handler
        .launch_table
        .publish("desktop-fixture", own_pid, process(), None);
    let peer = subc_os::local_peer::CapturedPeer::from(OsPeer::Observed(PeerObservation {
        user: PeerUser::Sid(subc_os::named_pipe::current_user_sid_string().unwrap()),
        pid: own_pid,
        exe: None,
        signing_id: None,
    }));
    let connection = ConnectionId::new(710);
    handler.record_captured_peer(connection, peer.clone());
    assert!(handler.operator_apps.lock().unwrap().is_empty());
    assert!(handler.config_desktop_identity(connection).unwrap());
    // Reusing the numeric PID for a different launch must not rebind the old
    // connection, even when the current launch independently verifies.
    handler.launch_table.retire("desktop-fixture");
    let second = handler
        .launch_table
        .publish("desktop-fixture", own_pid, process(), None);
    assert_ne!(first, second);
    assert_eq!(
        handler
            .launch_table
            .match_live_pid("desktop-fixture", own_pid),
        Some(second)
    );
    assert!(!handler.config_desktop_identity(connection).unwrap());
    let reconnected = ConnectionId::new(711);
    handler.record_captured_peer(reconnected, peer);
    assert!(handler.config_desktop_identity(reconnected).unwrap());
    assert!(handler.operator_apps.lock().unwrap().is_empty());
    handler.cleanup_connection(connection).unwrap();
    assert!(!handler
        .config_desktop_stamps
        .lock()
        .unwrap()
        .contains_key(&connection));
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_desktop_development_requires_a_same_user_local_peer_and_rechecks_it() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let connection = ConnectionId::new(700);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let ctx = RouteCtx {
        connection_id: connection,
        egress: crate::FrameSink::new(tx),
    };
    assert!(!handler.config_desktop_identity(connection).unwrap());
    owner_peer(&handler, connection);
    let facts = handler.config_approval_facts(&ctx).await;
    assert!(facts.owner_uid && facts.desktop_app);
    assert!(!facts.attested_operator_app);
    assert_eq!(
        facts.os_prompt_available,
        crate::config_gate::os_prompt_policy_present()
    );
    handler
        .local_connections
        .lock()
        .unwrap()
        .remove(&connection);
    assert!(
        !handler.config_approval_facts(&ctx).await.desktop_app,
        "same-user TCP is not the local endpoint"
    );
    handler.record_local_connection(connection);
    handler.observed_peers.lock().unwrap().remove(&connection);
    assert!(
        !handler.config_approval_facts(&ctx).await.desktop_app,
        "local without kernel owner credentials is not same-user"
    );
    owner_peer(&handler, connection);
    handler.cleanup_connection(connection).unwrap();
    assert!(!handler.config_desktop_identity(connection).unwrap());
    assert!(!handler
        .captured_peers
        .lock()
        .unwrap()
        .contains_key(&connection));
}

struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn review_fixture() -> (
    Scratch,
    Arc<crate::config_gate::runtime::RuntimeOwner>,
    subc_control::config::ConfigPlanView,
) {
    use crate::{
        config_gate::{
            capture::{ConfigCapturer, UnavailableProjectRegistry},
            runtime::{daemon_review, RuntimeOwner},
            PlanKind,
        },
        config_store::{ConfigStore, StoreRoot},
    };
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!(
            "config-desktop-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    std::fs::create_dir_all(path.join("store")).unwrap();
    std::fs::write(path.join("subc.jsonc"), br#"{"version":1,"modules":{}}"#).unwrap();
    let owner = Arc::new(RuntimeOwner::new(
        "desktop-adapter".into(),
        ConfigStore::open_digest_only_for_tests(
            StoreRoot::from_service_definition(path.join("store")).unwrap(),
        ),
        ConfigCapturer {
            daemon_path: path.join("subc.jsonc"),
            config_home: path.clone(),
            projects: Arc::new(UnavailableProjectRegistry),
        },
    ));
    let view = owner
        .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    (Scratch(path), owner, view)
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_desktop_cleanup_does_not_convoy_behind_an_owner_waiting_receipt() {
    let (_scratch, owner, view) = review_fixture();
    let connection = ConnectionId::new(700);
    let handler = Arc::new(
        ControlHandler::new(Arc::new(Registry::default())).with_config_runtime(owner.clone()),
    );
    owner_peer(&handler, connection);
    let digest = view.digest.clone();
    let mut facts = handler
        .config_approval_facts(&RouteCtx {
            connection_id: connection,
            egress: crate::FrameSink::new(tokio::sync::mpsc::channel(1).0),
        })
        .await;
    facts.attested_operator_app = true;
    facts.os_prompt_available = true;
    let (held, observed_hold) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let holder = owner.clone();
    let holding = std::thread::spawn(move || {
        holder.with_owner_lock_for_test(move || {
            let _ = held.send(());
            wait.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        })
    });
    observed_hold.await.unwrap();
    let (reached, receipt_waiting) = tokio::sync::oneshot::channel();
    owner.set_desktop_receipt_hook(move || {
        let _ = reached.send(());
    });
    let receipt_handler = handler.clone();
    let receipt = tokio::spawn(async move {
        receipt_handler
            .config_desktop_review_delivered(connection, view)
            .await
    });
    receipt_waiting.await.unwrap();
    let (done, mut cleanup_done) = tokio::sync::oneshot::channel();
    let cleanup_handler = handler.clone();
    let cleanup = std::thread::spawn(move || {
        cleanup_handler.cleanup_connection(connection).unwrap();
        let _ = done.send(());
    });
    let independent =
        tokio::time::timeout(std::time::Duration::from_millis(250), &mut cleanup_done)
            .await
            .is_ok();
    release.send(()).unwrap();
    holding.join().unwrap();
    cleanup.join().unwrap();
    receipt.await.unwrap();
    assert_eq!(
        owner.begin_approval(&digest, &facts).unwrap_err().code,
        "approval_surface_unavailable",
        "a delayed writer acknowledgement must not resurrect a disconnected desktop receipt"
    );
    assert!(independent, "cleanup must finish before the held owner lock is released, not wait on captured_peers behind a receipt");
}

#[cfg(not(feature = "dev-desktop-approval"))]
#[tokio::test]
async fn config_desktop_without_development_switch_refuses_same_user_local_review() {
    let (_scratch, owner, view) = review_fixture();
    let handler =
        ControlHandler::new(Arc::new(Registry::default())).with_config_runtime(owner.clone());
    let connection = ConnectionId::new(700);
    owner_peer(&handler, connection);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let ctx = RouteCtx {
        connection_id: connection,
        egress: crate::FrameSink::new(tx),
    };
    let mut facts = handler.config_approval_facts(&ctx).await;
    facts.os_prompt_available = true;
    assert!(!facts.desktop_app && !facts.attested_operator_app);
    assert_eq!(
        owner.begin_approval(&view.digest, &facts).unwrap_err().code,
        "approval_surface_unavailable"
    );
    assert!(owner.history().unwrap().0.is_empty());
}

#[cfg(feature = "dev-desktop-approval")]
fn stamp_response(view: subc_control::config::ConfigPlanView) -> Frame {
    let request = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        91,
        br#"{"op":"config.stamp"}"#.to_vec(),
    )
    .unwrap();
    control_response_body_frame(
        &request,
        &ConfigControlResponse::Stamp { view },
        "ConfigControlResponse",
    )
    .unwrap()
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_desktop_status_names_development_switch_without_claiming_attestation() {
    let (_scratch, owner, _view) = review_fixture();
    let handler = ControlHandler::new(Arc::new(Registry::default())).with_config_runtime(owner);
    let connection = ConnectionId::new(700);
    owner_peer(&handler, connection);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let ctx = RouteCtx {
        connection_id: connection,
        egress: crate::FrameSink::new(tx),
    };
    let request = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        91,
        br#"{"op":"config.status"}"#.to_vec(),
    )
    .unwrap();
    let response = handler.handle_control_frame(&ctx, request).await.unwrap();
    let ConfigControlResponse::Status { status } =
        serde_json::from_slice(&response[0].body).unwrap()
    else {
        panic!("expected config.status")
    };
    assert!(status
        .development_switches
        .iter()
        .any(|name| name == "dev-desktop-approval"));
    assert!(status
        .missing_approval_surfaces
        .iter()
        .all(|name| name != "dev_desktop_verifier_unspecified"));
    assert!(status
        .missing_approval_surfaces
        .iter()
        .all(|name| !name.starts_with("desktop")));
    assert!(status
        .missing_approval_surfaces
        .iter()
        .any(|name| name == "phone"));
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_desktop_complete_response_binds_only_after_successful_writer_flush() {
    let (_scratch, owner, view) = review_fixture();
    let handler = Arc::new(
        ControlHandler::new(Arc::new(Registry::default())).with_config_runtime(owner.clone()),
    );
    let connection = ConnectionId::new(700);
    owner_peer(&handler, connection);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let ctx = RouteCtx {
        connection_id: connection,
        egress: crate::FrameSink::new(tx),
    };
    let response = stamp_response(view.clone());
    let delivered = handler
        .config_desktop_review_response(connection, &response)
        .unwrap();
    assert_eq!(delivered, view);
    let mut facts = handler.config_approval_facts(&ctx).await;
    // Presence is a separate provider operation, not granted by the verifier.
    facts.os_prompt_available = true;
    let task = {
        let handler = handler.clone();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            handler
                .deliver_config_desktop_review(&ctx, response, delivered)
                .await
        })
    };
    let mut outbound = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!task.is_finished());
    assert_eq!(
        owner.begin_approval(&view.digest, &facts).unwrap_err().code,
        "approval_surface_unavailable"
    );
    outbound
        .flushed
        .take()
        .expect("review requires a writer receipt")
        .send(())
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let challenge = owner.begin_approval(&view.digest, &facts).unwrap();
    assert!(matches!(challenge, ApprovalChallenge::Desktop { .. }));
    owner
        .finish_approval(
            challenge.attempt(),
            &facts,
            crate::config_gate::ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
            1,
        )
        .unwrap();
    assert_eq!(
        owner.history().unwrap().0[0].approval_channel,
        subc_config::ApprovalChannel::UnverifiedDevelopmentDesktop
    );
    assert_eq!(
        owner
            .status()
            .unwrap()
            .approval_channels
            .iter()
            .find(|record| record.generation == 1)
            .map(|record| record.channel),
        Some(subc_config::ApprovalChannel::UnverifiedDevelopmentDesktop)
    );
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_desktop_failed_flush_revocation_and_disconnect_never_create_a_view_receipt() {
    for end in ["failed-write", "revoked", "disconnected"] {
        let (_scratch, owner, view) = review_fixture();
        let handler = Arc::new(
            ControlHandler::new(Arc::new(Registry::default())).with_config_runtime(owner.clone()),
        );
        let connection = ConnectionId::new(700);
        owner_peer(&handler, connection);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let ctx = RouteCtx {
            connection_id: connection,
            egress: crate::FrameSink::new(tx),
        };
        let mut facts = handler.config_approval_facts(&ctx).await;
        facts.os_prompt_available = true;
        let response = stamp_response(view.clone());
        let delivered = handler
            .config_desktop_review_response(connection, &response)
            .unwrap();
        let task = {
            let handler = handler.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                handler
                    .deliver_config_desktop_review(&ctx, response, delivered)
                    .await
            })
        };
        let mut outbound = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match end {
            "failed-write" => {
                drop(outbound.flushed.take());
            }
            "revoked" => {
                handler.observed_peers.lock().unwrap().remove(&connection);
                outbound.flushed.take().unwrap().send(()).unwrap();
            }
            "disconnected" => {
                handler.cleanup_connection(connection).unwrap();
                outbound.flushed.take().unwrap().send(()).unwrap();
            }
            _ => unreachable!(),
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.is_err(), end == "failed-write");
        assert_eq!(
            owner.begin_approval(&view.digest, &facts).unwrap_err().code,
            "approval_surface_unavailable",
            "{end}"
        );
    }
}

#[cfg(all(feature = "dev-desktop-approval", target_os = "macos"))]
#[tokio::test]
async fn config_desktop_apply_prompts_before_waiting_for_the_cutover_gate_and_publishes_after_release(
) {
    use crate::operator_confirm::{OperatorProvider, OperatorWithdraw, ProviderResult};
    struct Presence(std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>);
    impl OperatorProvider for Presence {
        fn config_gate_ready(&self) -> bool {
            true
        }
        fn prompt(
            &self,
            _: &str,
            _: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
        ) -> ProviderResult {
            ProviderResult::Unavailable
        }
        fn prompt_gate(
            &self,
            _: &str,
            _: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
        ) -> ProviderResult {
            self.0.lock().unwrap().take().unwrap().send(()).unwrap();
            ProviderResult::Approved
        }
    }
    let (scratch, owner, _) = review_fixture();
    std::fs::create_dir_all(scratch.0.join("cortexkit")).unwrap();
    std::fs::write(scratch.0.join("program"), b"captured program identity").unwrap();
    std::fs::write(scratch.0.join("cortexkit/a.jsonc"), b"{}").unwrap();
    std::fs::write(
        scratch.0.join("subc.jsonc"),
        serde_json::to_vec(&serde_json::json!({
            "version":1,"modules":{"a":{"program":scratch.0.join("program")}}
        }))
        .unwrap(),
    )
    .unwrap();
    owner.declare(ConnectionId::new(10), "a", subc_protocol::config::ConfigDeclareRequest::new(
        serde_json::json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"unevaluatedProperties":false,"x-ck-layers":["user"],"x-ck-apply":"live"}),
        scratch.0.join("cortexkit/a.jsonc").to_string_lossy(), vec![]), true).unwrap();
    let view = owner
        .finish_plan(
            owner.prepare(PlanKind::Stamp).unwrap(),
            crate::config_gate::runtime::daemon_review,
        )
        .unwrap();
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new(registry.clone(), crate::supervise::RestartPolicy::default())
        .with_handle(handle.clone());
    let handler = Arc::new(
        ControlHandler::new(registry)
            .with_supervisor(handle)
            .with_supervisor_rescan(supervisor, scratch.0.join("subc.jsonc"), None)
            .with_config_runtime(owner.clone()),
    );
    let (prompted, prompt) = tokio::sync::oneshot::channel();
    handler.forwarding.operator_confirms().configure(
        Arc::new(Presence(std::sync::Mutex::new(Some(prompted)))),
        crate::operator_confirm::OperatorLimits::default(),
    );
    let connection = ConnectionId::new(700);
    owner_peer(&handler, connection);
    owner.desktop_view_delivered(connection, &view).unwrap();
    let (sender, _receiver) = tokio::sync::mpsc::channel(8);
    let ctx = RouteCtx {
        connection_id: connection,
        egress: crate::FrameSink::new(sender),
    };
    let facts = handler.config_approval_facts(&ctx).await;
    assert!(
        facts.os_prompt_available,
        "this adapter test requires the macOS presence policy branch"
    );
    let challenge = owner.begin_approval(&view.digest, &facts).unwrap();
    let gates = owner
        .approval_commit_gates(challenge.attempt())
        .await
        .unwrap();
    assert_eq!(
        gates.len(),
        1,
        "the reviewed module must hold a real cutover lock"
    );
    let request = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        81,
        serde_json::to_vec(&ConfigControlRequest::Apply {
            digest: view.digest,
        })
        .unwrap(),
    )
    .unwrap();
    let mut apply = tokio::spawn(async move { handler.handle_control_frame(&ctx, request).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), prompt)
        .await
        .unwrap()
        .unwrap();
    assert!(tokio::time::timeout(std::time::Duration::from_millis(100), &mut apply).await.is_err(),
        "presence finishes without the module gate, but config.apply must not publish through an in-progress cutover");
    drop(gates);
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), apply)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.len(), 1);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&response[0].body).unwrap(),
        serde_json::json!({"op":"config.apply","generation":1,"restart_required":false})
    );
    assert_eq!(
        owner.history().unwrap().0[0].approval_channel,
        subc_config::ApprovalChannel::UnverifiedDevelopmentDesktop
    );
    assert_eq!(
        owner.status().unwrap().approval_channels[0].channel,
        subc_config::ApprovalChannel::UnverifiedDevelopmentDesktop
    );
}
