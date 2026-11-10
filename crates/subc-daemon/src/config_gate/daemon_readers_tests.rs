use super::*;
use crate::supervise::{RestartPolicy, Supervisor, SupervisorHandle};
use std::sync::Arc;

fn commit(owner: &RuntimeOwner, kind: PlanKind) -> u64 {
    let view = owner
        .finish_plan(owner.prepare(kind).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &view)
        .unwrap();
    let challenge = owner.begin_approval(&view.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            2,
        )
        .unwrap()
}

async fn request(handler: &crate::ControlHandler, body: serde_json::Value) -> serde_json::Value {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
    let ctx = crate::RouteCtx {
        connection_id: ConnectionId::new(50),
        egress: crate::FrameSink::new(sender),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Request,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    let mut replies = handler.handle_control_frame(&ctx, frame).await.unwrap();
    if replies.is_empty() {
        let mut outbound = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        if let Some(flushed) = outbound.flushed.take() {
            let _ = flushed.send(());
        }
        replies.push(outbound.frame);
    }
    serde_json::from_slice(&replies[0].body).unwrap()
}

async fn handler(
    rig: &Rig,
    owner: Arc<RuntimeOwner>,
) -> (
    crate::ControlHandler,
    crate::supervise::SupervisedModule,
    SupervisorHandle,
) {
    let registry = Arc::new(crate::Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor =
        Supervisor::new(registry.clone(), RestartPolicy::default()).with_handle(handle.clone());
    let config = crate::daemon_config::load(rig.path.join("subc.jsonc"))
        .unwrap()
        .unwrap();
    let module = &config.modules[0];
    let module = supervisor
        .supervise_configured_requested_async(
            module.module_spec(),
            false,
            module.health.clone(),
            module.drain_timeout_ms,
            module.restart,
            module.register_timeout,
            None,
        )
        .await
        .unwrap();
    let handler = crate::ControlHandler::new(registry)
        .with_supervisor(handle.clone())
        .with_supervisor_rescan(supervisor, rig.path.join("subc.jsonc"), config.port)
        .with_config_runtime(owner);
    #[cfg(feature = "test-support")]
    let handler = handler.with_config_os_prompt_policy_for_tests();
    let connection = ConnectionId::new(50);
    handler.record_local_connection(connection);
    handler.record_captured_peer(
        connection,
        subc_os::local_peer::ObservedPeer::Unobserved.into(),
    );
    #[cfg(unix)]
    let user = subc_os::local_peer::PeerUser::Uid(subc_os::effective_uid());
    #[cfg(windows)]
    let user =
        subc_os::local_peer::PeerUser::Sid(subc_os::named_pipe::current_user_sid_string().unwrap());
    handler.record_observed_peer(
        connection,
        subc_os::local_peer::ObservedPeer::Observed(subc_os::local_peer::PeerObservation {
            user,
            pid: std::process::id(),
            exe: None,
            signing_id: None,
        }),
    );
    (handler, module, handle)
}

#[tokio::test]
async fn config_readers_preview_records_files_candidate_records_nothing() {
    let rig = Rig::new();
    let owner = Arc::new(rig.owner("reader-preview"));
    rig.stamp(&owner);
    let (handler, module, _) = handler(&rig, owner.clone()).await;
    let before =
        std::fs::read(rig.path.join("store").join(crate::config_store::STORE_FILE)).unwrap();
    let candidate = json!({"version":1,"modules":{},"port":4321}).to_string();
    let reply = request(
        &handler,
        json!({"op":"supervisor.rescan_checked","preview":true,"candidate":candidate}),
    )
    .await;
    assert!(reply.get("code").is_none(), "{reply}");
    assert_eq!(
        std::fs::read(rig.path.join("store").join(crate::config_store::STORE_FILE)).unwrap(),
        before
    );
    std::fs::write(rig.path.join("subc.jsonc"), candidate).unwrap();
    let reply = request(&handler, json!({"op":"supervisor.rescan","preview":true})).await;
    assert!(reply.get("code").is_none(), "{reply}");
    let after =
        std::fs::read(rig.path.join("store").join(crate::config_store::STORE_FILE)).unwrap();
    assert_ne!(
        after, before,
        "file Preview must record a proposal before any status call"
    );
    assert_eq!(owner.status().unwrap().pending_proposals, 1);
    request(&handler, json!({"op":"supervisor.rescan","preview":true})).await;
    assert_eq!(
        std::fs::read(rig.path.join("store").join(crate::config_store::STORE_FILE)).unwrap(),
        after
    );
    module.stop().await.unwrap();
}

#[tokio::test]
async fn config_readers_execute_and_checked_refuse_before_legacy_prompt() {
    struct NoPrompt;
    impl crate::operator_confirm::OperatorProvider for NoPrompt {
        fn prompt(
            &self,
            _: &str,
            _: Box<dyn FnOnce(Arc<dyn crate::operator_confirm::OperatorWithdraw>) + Send>,
        ) -> crate::operator_confirm::ProviderResult {
            panic!("stamped rescan must not reach the legacy prompt");
        }
    }
    let rig = Rig::new();
    let owner = Arc::new(rig.owner("reader-refusals"));
    rig.stamp(&owner);
    let (handler, module, _) = handler(&rig, owner.clone()).await;
    let handler = handler
        .with_privileged_operator_prompt(crate::daemon_config::PrivilegedOperatorPrompt::Enforce);
    handler.forwarding().operator_confirms().configure(
        Arc::new(NoPrompt),
        crate::operator_confirm::OperatorLimits::default(),
    );
    let configuration = module.configuration().unwrap();
    let status = module.status().unwrap();
    std::fs::write(
        rig.path.join("subc.jsonc"),
        json!({"version":1,"modules":{}}).to_string(),
    )
    .unwrap();
    for op in ["supervisor.rescan", "supervisor.rescan_checked"] {
        let reply = request(&handler, json!({"op":op,"preview":false})).await;
        assert_eq!(reply["code"], "config_gated", "{op}: {reply}");
        assert_eq!(module.configuration().unwrap(), configuration);
        assert_eq!(
            module.status().unwrap().spawn_generation,
            status.spawn_generation
        );
        assert_eq!(module.status().unwrap().enabled, status.enabled);
    }
    assert_eq!(owner.status().unwrap().pending_proposals, 1);
    module.stop().await.unwrap();
}

#[test]
fn config_readers_unstamped_proposal_is_informational_without_a_store_file() {
    let rig = Rig::new();
    let owner = rig.owner("reader-unstamped");
    std::fs::write(
        rig.path.join("subc.jsonc"),
        json!({"version":1,"modules":{},"port":4321}).to_string(),
    )
    .unwrap();
    assert_eq!(owner.status().unwrap().pending_proposals, 1);
    assert!(!rig
        .path
        .join("store")
        .join(crate::config_store::STORE_FILE)
        .exists());
    let prepared = owner.prepare(PlanKind::Change).unwrap();
    assert!(owner.finish_plan(prepared, daemon_review).is_err());
    rig.stamp(&owner);
    assert_eq!(owner.status().unwrap().pending_proposals, 0);
}

#[cfg(all(feature = "dev-desktop-approval", feature = "test-support"))]
#[tokio::test]
async fn config_readers_apply_reports_approved_entries_separately_from_in_force_configuration() {
    struct Presence;
    impl crate::operator_confirm::OperatorProvider for Presence {
        fn config_gate_ready(&self) -> bool {
            true
        }
        fn prompt(
            &self,
            _: &str,
            _: Box<dyn FnOnce(Arc<dyn crate::operator_confirm::OperatorWithdraw>) + Send>,
        ) -> crate::operator_confirm::ProviderResult {
            panic!("config.apply must use gate approval, not the legacy provider");
        }
        fn prompt_gate(
            &self,
            _: &str,
            _: Box<dyn FnOnce(Arc<dyn crate::operator_confirm::OperatorWithdraw>) + Send>,
        ) -> crate::operator_confirm::ProviderResult {
            crate::operator_confirm::ProviderResult::Approved
        }
    }
    let rig = Rig::new();
    std::fs::write(
        rig.path.join("subc.jsonc"),
        json!({"version":1,"port":1234,
        "modules":{"a":{"program":rig.path.join("program"),"enabled":false}}})
        .to_string(),
    )
    .unwrap();
    let owner = Arc::new(rig.owner("reader-reporting"));
    rig.stamp(&owner);
    let (handler, module, _) = handler(&rig, owner.clone()).await;
    handler.forwarding().operator_confirms().configure(
        Arc::new(Presence),
        crate::operator_confirm::OperatorLimits::default(),
    );
    let initial = module.configuration().unwrap();
    std::fs::write(
        rig.path.join("subc.jsonc"),
        json!({"version":1,"port":4321,
        "modules":{"a":{"program":rig.path.join("program"),"args":["new-argument"],"enabled":true,
            "restart":{"max_restarts":9},"drain_timeout_ms":17,"register_timeout_s":25}}})
        .to_string(),
    )
    .unwrap();
    let view = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(50), &view)
        .unwrap();
    let reply = request(&handler, json!({"op":"config.apply","digest":view.digest})).await;
    assert_eq!(reply["generation"], 2, "{reply}");
    assert_eq!(reply["restart_required"], true, "{reply}");
    let reply = request(&handler, json!({"op":"config.status"})).await;
    let inactive = reply["status"]["approved_but_inactive"].as_array().unwrap();
    for entry in [
        "port",
        "args",
        "enabled",
        "restart",
        "drain_timeout_ms",
        "register_timeout_s",
    ] {
        let row = inactive
            .iter()
            .find(|row| row["entry"] == entry)
            .unwrap_or_else(|| panic!("missing {entry}: {reply}"));
        assert_eq!(row["waiting_for"], "daemon_restart");
        if entry != "port" {
            assert_eq!(row["module_id"], "a");
        }
        assert_ne!(row["in_force"], row["approved"]);
    }
    assert_eq!(module.configuration().unwrap(), initial);
    assert!(!module.status().unwrap().enabled);
    let preview = request(&handler, json!({"op":"supervisor.rescan","preview":true})).await;
    assert_eq!(
        preview["unchanged"], 1,
        "Preview compares files with approval, not inactive supervisor settings: {preview}"
    );
    // A later file edit changes neither approved configured_enabled nor supervision.
    std::fs::write(
        rig.path.join("subc.jsonc"),
        json!({"version":1,"modules":{}}).to_string(),
    )
    .unwrap();
    let reply = request(&handler, json!({"op":"supervisor.list"})).await;
    assert_eq!(reply["modules"][0]["configured_enabled"], true, "{reply}");
    assert_eq!(reply["modules"][0]["enabled"], false, "{reply}");
    module.stop().await.unwrap();
}

#[tokio::test]
async fn config_readers_release_reserved_uses_approved_removal_not_proposed_readd() {
    let rig = Rig::new();
    let original = json!({"version":1,"modules":{"a":{"program":rig.path.join("program"),"reserved":true,"enabled":false}}}).to_string();
    std::fs::write(rig.path.join("subc.jsonc"), &original).unwrap();
    let owner = Arc::new(rig.owner("reader-release"));
    rig.stamp(&owner);
    let (handler, module, supervisor) = handler(&rig, owner.clone()).await;
    std::fs::write(
        rig.path.join("subc.jsonc"),
        json!({"version":1,"modules":{}}).to_string(),
    )
    .unwrap();
    commit(&owner, PlanKind::Change);
    // Reservation release does not retire a supervised module; retire it first.
    module.retire_requested(None).await.unwrap();
    supervisor.retire("a");
    std::fs::write(rig.path.join("subc.jsonc"), original).unwrap();
    let reply = request(
        &handler,
        json!({"op":"supervisor.release_reserved","module_id":"a"}),
    )
    .await;
    assert_eq!(reply["applied"], true, "{reply}");
    assert_eq!(owner.status().unwrap().pending_proposals, 1);
}
