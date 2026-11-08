use super::*;
#[cfg(feature = "dev-desktop-approval")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn owner_peer(handler: &ControlHandler, connection: ConnectionId) {
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
fn config_desktop_owner_uid_with_development_feature_and_no_verifier_is_refused() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let connection = ConnectionId::new(700);
    owner_peer(&handler, connection);
    assert!(handler.config_owner_connection(connection));
    assert_eq!(
        handler
            .config_desktop_identity(connection)
            .unwrap_err()
            .code,
        "dev_desktop_verifier_unspecified"
    );
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
struct Verifier(Arc<AtomicBool>);
#[cfg(feature = "dev-desktop-approval")]
impl crate::config_gate::desktop::DesktopVerifier for Verifier {
    fn verify(&self, _: &subc_os::local_peer::CapturedPeer) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_desktop_typed_verifier_is_not_an_owner_or_presence_bypass_and_is_rechecked() {
    let live = Arc::new(AtomicBool::new(true));
    let handler = ControlHandler::new(Arc::new(Registry::default())).with_dev_desktop_verifier(
        crate::config_gate::desktop::DevDesktopVerifier::Supplied(Arc::new(Verifier(live.clone()))),
    );
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
    live.store(false, Ordering::Release);
    assert!(!handler.config_approval_facts(&ctx).await.desktop_app);
    live.store(true, Ordering::Release);
    handler.cleanup_connection(connection).unwrap();
    assert!(!handler.config_desktop_identity(connection).unwrap());
    assert!(!handler
        .captured_peers
        .lock()
        .unwrap()
        .contains_key(&connection));
}

#[cfg(feature = "dev-desktop-approval")]
struct Scratch(std::path::PathBuf);
#[cfg(feature = "dev-desktop-approval")]
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(feature = "dev-desktop-approval")]
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
async fn config_desktop_status_names_unspecified_verifier_and_missing_surfaces() {
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
        .any(|name| name == "dev_desktop_verifier_unspecified"));
    assert!(status
        .missing_approval_surfaces
        .iter()
        .any(|name| name.starts_with("desktop")));
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
        ControlHandler::new(Arc::new(Registry::default()))
            .with_config_runtime(owner.clone())
            .with_dev_desktop_verifier(crate::config_gate::desktop::DevDesktopVerifier::Supplied(
                Arc::new(Verifier(Arc::new(AtomicBool::new(true)))),
            )),
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
    assert!(matches!(
        owner.begin_approval(&view.digest, &facts).unwrap(),
        ApprovalChallenge::Desktop { .. }
    ));
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_desktop_failed_flush_revocation_and_disconnect_never_create_a_view_receipt() {
    for end in ["failed-write", "revoked", "disconnected"] {
        let (_scratch, owner, view) = review_fixture();
        let live = Arc::new(AtomicBool::new(true));
        let handler = Arc::new(
            ControlHandler::new(Arc::new(Registry::default()))
                .with_config_runtime(owner.clone())
                .with_dev_desktop_verifier(
                    crate::config_gate::desktop::DevDesktopVerifier::Supplied(Arc::new(Verifier(
                        live.clone(),
                    ))),
                ),
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
                live.store(false, Ordering::Release);
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
