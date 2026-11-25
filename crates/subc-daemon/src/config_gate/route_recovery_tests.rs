use super::*;
use crate::{ControlHandler, FrameSink, Registry, RouteCtx};
use std::sync::Arc;
use subc_control::ClientControlResponse;
use subc_protocol::{
    manifest::{Concurrency, ExecutionMode, IdentityScope, ModuleManifest, ProviderRole, Tool},
    session::ModuleControlResponse,
    Flags, Frame, FrameType, ModuleHelloBody, Priority,
};
use tokio::sync::{mpsc, oneshot};

const DEADLINE: Duration = Duration::from_secs(2);

struct RouteRig {
    handler: Arc<ControlHandler>,
    target: RouteCtx,
    binds: mpsc::Receiver<crate::router::OutboundFrame>,
}

impl RouteRig {
    async fn new(owner: Arc<RuntimeOwner>) -> Self {
        let handler =
            Arc::new(ControlHandler::new(Arc::new(Registry::default())).with_config_runtime(owner));
        let (target, mut binds) = connection(50);
        let hello = frame(
            FrameType::Hello,
            1,
            serde_json::to_value(ModuleHelloBody {
                manifest: ModuleManifest::builder("ordinary", "1.0.0")
                    .provides(vec![ProviderRole::ToolProvider {
                        tools: vec![Tool {
                            name: "read".into(),
                            description: None,
                            execution_mode: ExecutionMode::Pure,
                            schema: json!({"type":"object"}),
                        }],
                        identity_scope: vec![IdentityScope::Session],
                        concurrency: Concurrency::ModuleManaged,
                        emits_push: false,
                        sub_supervises: false,
                    }])
                    .build(),
                protocol_ver: subc_protocol::PROTOCOL_VERSION,
                control_ops: None,
                launch_nonce: None,
            })
            .unwrap(),
        );
        let replies = handler.handle_control_frame(&target, hello).await.unwrap();
        assert!(replies.is_empty(), "HELLO refused: {replies:?}");
        assert_eq!(
            binds.try_recv().unwrap().frame.header.ty,
            FrameType::HelloAck
        );
        Self {
            handler,
            target,
            binds,
        }
    }

    async fn open(&mut self, connection_id: u64) {
        let (client, mut replies) = connection(connection_id);
        let open = self.handler.handle_control_frame(&client, open_frame());
        tokio::pin!(open);
        let bind = tokio::select! {
            result = &mut open => panic!("route.open returned before binding: {result:?}"),
            bind = self.binds.recv() => bind.unwrap().frame,
        };
        let body: serde_json::Value = serde_json::from_slice(&bind.body).unwrap();
        assert_eq!(body["op"], "route.bind");
        let ack = frame(
            FrameType::Response,
            bind.header.corr,
            serde_json::to_value(ModuleControlResponse::RouteBindAck {}).unwrap(),
        );
        assert!(self
            .handler
            .handle_control_frame(&self.target, ack)
            .await
            .unwrap()
            .is_empty());
        assert!(open.await.unwrap().is_empty());
        let reply = replies.recv().await.unwrap().frame;
        assert_eq!(reply.header.ty, FrameType::Response);
        assert!(matches!(
            serde_json::from_slice(&reply.body).unwrap(),
            ClientControlResponse::RouteOpen { .. }
        ));
    }

    async fn refuse(&mut self, connection_id: u64) {
        let (client, _replies) = connection(connection_id);
        let replies = self
            .handler
            .handle_control_frame(&client, open_frame())
            .await
            .unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].header.ty, FrameType::Error);
        let body: serde_json::Value = serde_json::from_slice(&replies[0].body).unwrap();
        assert_eq!(body["code"], "config_gated");
        assert_eq!(body["message"], "configuration store is in recovery");
        assert!(
            self.binds.try_recv().is_err(),
            "a refused open must not reach its target"
        );
    }
}

fn connection(id: u64) -> (RouteCtx, mpsc::Receiver<crate::router::OutboundFrame>) {
    let (tx, rx) = mpsc::channel(8);
    (
        RouteCtx {
            connection_id: ConnectionId::new(id),
            egress: FrameSink::new(tx),
        },
        rx,
    )
}

fn frame(ty: FrameType, corr: u64, body: serde_json::Value) -> Frame {
    Frame::build(
        ty,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

fn open_frame() -> Frame {
    frame(
        FrameType::Request,
        2,
        json!({
            "op": "route.open",
            "target": {"kind": "tool_provider", "module_id": "ordinary"},
            "identity": {"harness": "test", "session": "route-recovery"},
        }),
    )
}

async fn pause_approval(
    owner: &Arc<RuntimeOwner>,
    kind: PlanKind,
) -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<u64>) {
    let plan = owner
        .finish_plan(owner.prepare(kind).unwrap(), daemon_review)
        .unwrap();
    let facts = Rig::facts();
    owner
        .desktop_view_delivered(ConnectionId::new(facts.connection), &plan)
        .unwrap();
    let attempt = owner
        .begin_approval(&plan.digest, &facts)
        .unwrap()
        .attempt();
    let (reached, paused) = oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    owner.inner.lock().unwrap().prepublication_hook = Some(Box::new(move || {
        reached.send(()).unwrap();
        let _ = resume.recv();
    }));
    let owner = owner.clone();
    let approval = tokio::task::spawn_blocking(move || {
        owner
            .finish_approval(
                attempt,
                &facts,
                ApprovalReply::Presence(crate::config_gate::PresenceDecision::InteractiveApproval),
                1,
            )
            .unwrap()
    });
    tokio::time::timeout(DEADLINE, paused)
        .await
        .expect("approval did not reach precommit")
        .unwrap();
    (release, approval)
}

#[tokio::test]
async fn route_open_completes_while_config_approval_holds_owner() {
    let rig = Rig::new();
    let owner = Arc::new(rig.owner("route-while-approval"));
    let mut route = RouteRig::new(owner.clone()).await;
    let (release, approval) = pause_approval(&owner, PlanKind::Stamp).await;

    let opened = tokio::time::timeout(DEADLINE, route.open(51)).await;
    release.send(()).unwrap();
    approval.await.unwrap();
    opened.expect("route.open did not complete while configuration approval held the owner");
}

#[tokio::test]
async fn route_open_refuses_until_the_published_recovery_exit_is_complete() {
    let rig = Rig::new();
    let owner = Arc::new(rig.owner("route-recovery-exit"));
    let mut route = RouteRig::new(owner.clone()).await;
    let stamped = rig.stamp(&owner);
    owner.complete_authority_exit_for_test(stamped).unwrap();
    std::fs::write(
        rig.path.join("store").join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    owner.poll_store_integrity().unwrap();
    let recovering = owner.polled_snapshot().unwrap();
    assert!(recovering.is_recovering());
    route.refuse(51).await;

    let (release, approval) = pause_approval(&owner, PlanKind::Unstamp).await;
    let refused = tokio::time::timeout(DEADLINE, route.refuse(52)).await;
    release.send(()).unwrap();
    let generation = approval.await.unwrap();
    refused.expect("route.open did not refuse from the published state during recovery exit");

    // The unstamp is durable, but routes stay blocked until the supervisor has
    // reconciled the running modules with the newly authoritative configuration.
    route.refuse(53).await;
    owner.complete_authority_exit_for_test(generation).unwrap();
    assert!(!owner.polled_snapshot().unwrap().is_recovering());
    assert!(
        recovering.is_recovering(),
        "a retained publication cannot change"
    );
    tokio::time::timeout(DEADLINE, route.open(54))
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn settings_poll_publishes_store_tampering_without_a_request() {
    let rig = Rig::new();
    let owner = Arc::new(rig.owner("settings-poll-store-integrity"));
    let stamped = rig.stamp(&owner);
    owner.complete_authority_exit_for_test(stamped).unwrap();
    owner.poll_store_integrity().unwrap();
    assert!(!owner.project_settings_enabled());
    let (idle, mut idled) = mpsc::unbounded_channel();
    let handler = Arc::new(
        ControlHandler::new(Arc::new(Registry::default()))
            .with_config_runtime(owner.clone())
            .with_delivery_idle_for_test(idle),
    );
    let delivery = handler.spawn_config_delivery_worker().unwrap();
    // Park delivery so no integrity check other than the settings poll can observe the edit.
    let parked = tokio::time::timeout(DEADLINE, idled.recv())
        .await
        .unwrap()
        .unwrap();
    let mut changes = owner.subscribe_settings();
    assert!(!owner.refuses_control_in_recovery());
    std::fs::write(
        rig.path.join("store").join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    let elapsed = tokio::time::timeout(
        unstamped::USER_FILE_POLL_INTERVAL + Duration::from_millis(250),
        changes.changed(),
    )
    .await;
    delivery.abort();
    drop(parked);
    let _ = delivery.await;
    elapsed
        .expect("the next settings poll must detect tampering")
        .unwrap();
    assert!(owner.refuses_control_in_recovery());
}
