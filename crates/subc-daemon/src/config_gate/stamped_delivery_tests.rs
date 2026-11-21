use super::*;
use crate::config_store::ProjectDeliveryState;
use subc_protocol::config::{
    ConfigAppliedRequest, ConfigApplyResult, ConfigChanged, ConfigGetReply,
};
use subc_protocol::{Flags, Frame, FrameType, Priority, PROTOCOL_VERSION};

const INCUMBENT: ConnectionId = ConnectionId::new(10);

fn request(project: Option<&str>) -> ConfigGetRequest {
    let mut request = ConfigGetRequest::new();
    request.project_id = project.map(str::to_owned);
    request
}

fn served(owner: &RuntimeOwner, connection: ConnectionId, project: Option<&str>) -> ConfigGetReply {
    let reply = owner.module_get(connection, "a", request(project)).unwrap();
    owner
        .module_view_delivered(connection, "a", &reply, tokio::time::Instant::now(), true)
        .unwrap();
    reply
}

pub(super) fn report(view: &ConfigView, ack: bool) -> ConfigAppliedRequest {
    let mut report = ConfigAppliedRequest::new(
        view.generation,
        view.attempt.unwrap(),
        if ack {
            ConfigApplyResult::Ack
        } else {
            ConfigApplyResult::Nack
        },
        vec![],
    );
    report.project_id.clone_from(&view.project_id);
    report
}

async fn approve(rig: &CaptureRig) -> u64 {
    rig.apply(&finish(&rig.owner, rig.prepare().await.unwrap()))
        .await
        .unwrap()
}

async fn two_projects(rig: &CaptureRig) -> u64 {
    write_layer(&rig.workspace, 4);
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("B", Some("W"), &[]),
    ]);
    approve(rig).await
}

#[tokio::test]
async fn stamped_delivery_fallback_uses_confirmed_membership_and_never_acknowledges() {
    let rig = CaptureRig::new();
    let get = |project| rig.owner.module_get(INCUMBENT, "a", request(Some(project)));
    assert_eq!(get("P").unwrap_err().code, "project_registry_unavailable");
    assert_eq!(rig.registry.enumerations.load(Ordering::Relaxed), 0);
    rig.set_projects(vec![member("P", None, &[])]);
    rig.owner.refresh_project_membership().await.unwrap();
    *rig.registry.reply.lock().unwrap() = None;
    let fallback = get("P").unwrap().active.unwrap();
    assert_eq!(fallback.project_id.as_deref(), Some("P"));
    assert_eq!(fallback.status, ConfigStatus::Active);
    assert_eq!(fallback.generation, rig.stamp);
    assert_eq!(fallback.attempt, None);
    assert_eq!(get("other").unwrap_err().code, "unknown_project");
    for result in [ConfigApplyResult::Ack, ConfigApplyResult::Nack] {
        let mut report = ConfigAppliedRequest::new(rig.stamp, 1, result, vec![]);
        report.project_id = Some("P".into());
        assert_eq!(
            rig.owner.module_applied(INCUMBENT, "a", report).unwrap(),
            AppliedDisposition::Ignored
        );
    }
    let state = rig.owner.lock().unwrap();
    let body = state.store.loaded_admission_state().1;
    assert!(body.project_versions(&tuple(Some("P"))).is_none());
    assert_eq!(body.user_versions("a").unwrap().active, Some(rig.stamp));
    assert_eq!(rig.registry.enumerations.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn stamped_delivery_isolation_survives_reconnect_and_restart() {
    let rig = CaptureRig::new();
    let generation = two_projects(&rig).await;
    // A promoted connection can still be catching up to its active user view
    // while projects on that already active basis are deliverable.
    let user_window = rig
        .owner
        .delivery_promoted(INCUMBENT, "a", tokio::time::Instant::now())
        .unwrap()
        .unwrap();
    let a = served(&rig.owner, INCUMBENT, Some("A")).pending.unwrap();
    let b = served(&rig.owner, INCUMBENT, Some("B")).pending.unwrap();
    assert_eq!((a.generation, b.generation), (generation, generation));
    assert_ne!(a.attempt, b.attempt);
    let mut wrong = report(&b, true);
    wrong.project_id = Some("A".into());
    assert_eq!(
        rig.owner.module_applied(INCUMBENT, "a", wrong).unwrap(),
        AppliedDisposition::Ignored
    );
    let user_state = |owner: &RuntimeOwner| {
        let state = owner.lock().unwrap();
        let body = state.store.loaded_admission_state().1;
        let user = &body.generations[&rig.stamp];
        let projects = &body.generations[&generation];
        (
            user.generation.module_states.clone(),
            user.generation.never_redeliver.clone(),
            user.outcome.clone(),
            projects.generation.module_states.clone(),
            projects.generation.never_redeliver.clone(),
            projects.outcome.clone(),
        )
    };
    let baseline = user_state(&rig.owner);
    assert!(matches!(
        rig.owner
            .module_applied(INCUMBENT, "a", report(&a, false))
            .unwrap(),
        AppliedDisposition::Failed { .. }
    ));
    assert!(rig.owner.delivery_take_recoveries().unwrap().is_empty());
    assert_eq!(
        rig.owner
            .delivery_window_for(INCUMBENT, Some("B"))
            .unwrap()
            .unwrap()
            .attempt,
        b.attempt.unwrap()
    );
    let user = rig.owner.delivery_window(INCUMBENT).unwrap().unwrap();
    assert_eq!((user.generation, user.attempt), user_window);
    rig.owner.disconnected(INCUMBENT);
    rig.rig.declare(&rig.owner, 11);
    assert!(rig.get("A", 11).pending.is_none());
    let next = ConnectionId::new(11);
    let reissued = served(&rig.owner, next, Some("B")).pending.unwrap();
    assert_ne!(reissued.attempt, b.attempt);
    assert_eq!(
        rig.owner
            .module_applied(next, "a", report(&b, true))
            .unwrap(),
        AppliedDisposition::Ignored
    );
    assert_eq!(
        rig.owner
            .module_applied(next, "a", report(&reissued, true))
            .unwrap(),
        AppliedDisposition::Active
    );
    let mut restarted = rig.rig.owner("restarted");
    restarted.project_capture_enabled = true;
    rig.rig.declare(&restarted, 12);
    assert_eq!(user_state(&restarted), baseline);
    let state = restarted.lock().unwrap();
    let body = state.store.loaded_admission_state().1;
    let outcomes = &body.generations[&generation].project_outcomes["a"];
    assert_eq!(outcomes["A"].state, ProjectDeliveryState::Failed);
    assert!(outcomes["A"].never_redeliver);
    assert_eq!(outcomes["B"].state, ProjectDeliveryState::Active);
    assert_eq!(
        body.project_versions(&tuple(Some("B"))).unwrap().active,
        Some(generation)
    );
}

#[tokio::test]
async fn stamped_delivery_waits_for_user_basis_and_fails_only_dependents_on_user_nack() {
    let rig = CaptureRig::new();
    std::fs::write(&rig.rig.declaration.user_file, br#"{"limit":2}"#).unwrap();
    let generation = two_projects(&rig).await;
    let held = rig.get("A", 10);
    assert!(held.pending.is_none());
    assert_eq!(held.active.unwrap().generation, rig.stamp);
    assert!(rig
        .owner
        .project_delivery_pass(INCUMBENT, "a")
        .unwrap()
        .hints
        .is_empty());

    // Approve a different, project-only generation while its user basis is pending.
    write_layer(&rig.workspace, 5);
    let project_only = approve(&rig).await;
    assert!(rig.get("B", 10).pending.is_none());
    let user = served(&rig.owner, INCUMBENT, None).pending.unwrap();
    assert_eq!(user.generation, generation);
    let wake = rig.owner.delivery_notifications();
    let waiting = wake.notified();
    tokio::pin!(waiting);
    waiting.as_mut().enable();
    assert_eq!(
        rig.owner
            .module_applied(INCUMBENT, "a", report(&user, true))
            .unwrap(),
        AppliedDisposition::Active
    );
    tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap();
    let hints = rig
        .owner
        .project_delivery_pass(INCUMBENT, "a")
        .unwrap()
        .hints;
    assert_eq!(
        hints
            .iter()
            .map(|hint| (hint.project_id.as_deref().unwrap(), hint.generation))
            .collect::<Vec<_>>(),
        [("A", project_only), ("B", project_only)]
    );
    let old = served(&rig.owner, INCUMBENT, Some("A")).pending.unwrap();
    assert_eq!(
        rig.owner
            .module_applied(INCUMBENT, "a", report(&old, true))
            .unwrap(),
        AppliedDisposition::Active
    );
    let withdrawn = served(&rig.owner, INCUMBENT, Some("B")).pending.unwrap();

    std::fs::write(&rig.rig.declaration.user_file, br#"{"limit":3}"#).unwrap();
    let rejected_basis = approve(&rig).await;
    write_layer(&rig.workspace, 6);
    let rejected_project = approve(&rig).await;
    let user = served(&rig.owner, INCUMBENT, None).pending.unwrap();
    assert_eq!(user.generation, rejected_basis);
    assert!(matches!(
        rig.owner
            .module_applied(INCUMBENT, "a", report(&user, false))
            .unwrap(),
        AppliedDisposition::Failed { .. }
    ));
    assert_eq!(
        rig.owner
            .module_applied(INCUMBENT, "a", report(&withdrawn, true))
            .unwrap(),
        AppliedDisposition::Ignored
    );
    assert_eq!(rig.get("A", 10).active.unwrap().generation, project_only);
    let state = rig.owner.lock().unwrap();
    let body = state.store.loaded_admission_state().1;
    for project in ["A", "B"] {
        assert!(body
            .project_versions(&tuple(Some(project)))
            .unwrap()
            .pending
            .is_none());
        assert_eq!(
            body.generations[&rejected_project].project_outcomes["a"][project].state,
            ProjectDeliveryState::Failed
        );
    }
}

pub(super) struct Peer {
    pub(super) handler: Arc<crate::ControlHandler>,
    ctx: crate::router::RouteCtx,
    rx: tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
}

impl Peer {
    pub(super) fn new(owner: Arc<RuntimeOwner>, connection_id: ConnectionId) -> Self {
        let registry = Arc::new(crate::Registry::default());
        let forwarding = Arc::new(crate::ForwardingTable::default());
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let egress = crate::FrameSink::new(tx);
        registry
            .register_with_control_ops(
                subc_protocol::manifest::ModuleManifest::builder("a", "0.1.0").build(),
                PROTOCOL_VERSION,
                connection_id,
                vec!["config.review".into()],
            )
            .unwrap();
        forwarding
            .register_module_connection(
                connection_id,
                "a".into(),
                PROTOCOL_VERSION,
                subc_protocol::manifest::Concurrency::ModuleManaged,
                egress.clone(),
            )
            .unwrap();
        Self {
            handler: Arc::new(
                crate::ControlHandler::with_forwarding(registry, forwarding)
                    .with_config_runtime(owner),
            ),
            ctx: crate::router::RouteCtx {
                connection_id,
                egress,
            },
            rx,
        }
    }

    async fn next_frame(
        rx: &mut tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
    ) -> Frame {
        let mut outbound = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        outbound.flushed.take().unwrap().send(()).unwrap();
        outbound.frame
    }

    async fn next_hint(&mut self) -> ConfigChanged {
        serde_json::from_slice(&self.next_hint_frame().await.body).unwrap()
    }

    pub(super) async fn next_hint_frame(&mut self) -> Frame {
        let frame = Self::next_frame(&mut self.rx).await;
        assert_eq!(frame.header.ty, FrameType::Push);
        frame
    }

    pub(super) async fn rpc(&mut self, request: &impl serde::Serialize, flushed: bool) -> Frame {
        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(request).unwrap(),
        )
        .unwrap();
        let call = self.handler.handle_control_frame(&self.ctx, frame);
        if flushed {
            let (reply, result) = tokio::join!(Self::next_frame(&mut self.rx), call);
            assert!(result.unwrap().is_empty());
            assert_eq!(
                reply.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
            reply
        } else {
            let mut frames = call.await.unwrap();
            assert_eq!(frames.len(), 1);
            let reply = frames.pop().unwrap();
            assert_eq!(reply.header.ty, FrameType::Response);
            reply
        }
    }
}

#[tokio::test]
async fn stamped_delivery_cold_discovery_serves_saved_views_and_skips_confirmed_drops() {
    let rig = CaptureRig::new();
    write_layer(&rig.workspace, 4);
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("B", Some("W"), &[]),
        member("dropped", Some("W"), &[]),
        member("rejected", Some("W"), &[]),
    ]);
    let generation = approve(&rig).await;
    for project in ["A", "dropped", "rejected"] {
        let view = served(&rig.owner, INCUMBENT, Some(project))
            .pending
            .unwrap();
        rig.owner
            .module_applied(INCUMBENT, "a", report(&view, project != "rejected"))
            .unwrap();
    }
    let polling = Peer::new(rig.owner.clone(), INCUMBENT);
    let before_poll = rig.registry.enumerations.load(Ordering::Relaxed);
    let worker = polling.handler.spawn_config_delivery_worker().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while rig.registry.enumerations.load(Ordering::Relaxed) == before_poll {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(rig.get("dropped", 10).active.is_some());
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("B", Some("W"), &[]),
        member("rejected", Some("W"), &[]),
    ]);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if rig
                .owner
                .module_get(INCUMBENT, "a", request(Some("dropped")))
                .is_err_and(|e| e.code == "unknown_project")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    let _ = worker.await;
    *rig.registry.reply.lock().unwrap() = None;
    std::fs::remove_dir_all(&rig.workspace).unwrap();
    let mut restarted = rig.rig.owner("cold-discovery");
    restarted.project_capture_enabled = true;
    restarted.capturer.projects = rig.registry.clone();
    let restarted = Arc::new(restarted);
    let connection = ConnectionId::new(11);
    let mut peer = Peer::new(restarted.clone(), connection);
    let worker = peer.handler.spawn_config_delivery_worker().unwrap();
    peer.rpc(&rig.rig.declaration, false).await;
    for id in ["A", "B"] {
        let hint = peer.next_hint().await;
        assert_eq!(hint.project_id.as_deref(), Some(id));
        assert_eq!(hint.generation, generation);
    }
    let a: ConfigGetReply =
        serde_json::from_slice(&peer.rpc(&request(Some("A")), true).await.body).unwrap();
    assert!(a.pending.is_none());
    assert_eq!(a.active.unwrap().effective["limit"], 4);
    assert!(
        restarted
            .delivery_window_for(connection, Some("B"))
            .unwrap()
            .is_none(),
        "a push must not assign an attempt"
    );
    let b: ConfigGetReply =
        serde_json::from_slice(&peer.rpc(&request(Some("B")), true).await.body).unwrap();
    let b = b.pending.unwrap();
    assert_eq!(b.effective["limit"], 4);
    peer.rpc(&report(&b, true), false).await;
    assert_eq!(
        restarted
            .module_get(connection, "a", request(Some("dropped")))
            .unwrap_err()
            .code,
        "unknown_project"
    );
    assert!(restarted
        .project_delivery_pass(connection, "a")
        .unwrap()
        .hints
        .is_empty());
    let state = restarted.lock().unwrap();
    let body = state.store.loaded_admission_state().1;
    assert_eq!(
        body.project_versions(&tuple(Some("dropped")))
            .unwrap()
            .active,
        Some(generation)
    );
    assert_eq!(
        body.generations[&generation]
            .delivery
            .as_ref()
            .unwrap()
            .project_cutoffs["a"]["A"]
            .workspace
            .effective["limit"],
        4
    );
    assert_eq!(
        body.project_versions(&tuple(Some("B"))).unwrap().active,
        Some(generation)
    );
    drop(state);
    worker.abort();
    let _ = worker.await;
}

#[tokio::test]
async fn stamped_delivery_user_ack_wakes_held_tuple_and_requested_fallback_only() {
    let rig = CaptureRig::new();
    write_layer(&rig.workspace, 4);
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("P", None, &[]),
        member("Q", None, &[]),
    ]);
    rig.owner.refresh_project_membership().await.unwrap();
    let mut peer = Peer::new(rig.owner.clone(), INCUMBENT);
    let fallback: ConfigGetReply =
        serde_json::from_slice(&peer.rpc(&request(Some("P")), true).await.body).unwrap();
    assert_eq!(fallback.active.unwrap().effective["limit"], 1);
    std::fs::write(&rig.rig.declaration.user_file, br#"{"limit":2}"#).unwrap();
    let generation = approve(&rig).await;
    let worker = peer.handler.spawn_config_delivery_worker().unwrap();
    let hint = peer.next_hint().await;
    assert_eq!((hint.project_id, hint.generation), (None, generation));
    let user: ConfigGetReply =
        serde_json::from_slice(&peer.rpc(&request(None), true).await.body).unwrap();
    assert!(rig
        .owner
        .project_delivery_pass(INCUMBENT, "a")
        .unwrap()
        .hints
        .is_empty());
    let user = user.pending.unwrap();
    let mut fallback_report = report(&user, true);
    fallback_report.project_id = Some("P".into());
    peer.rpc(&fallback_report, false).await;
    let held = rig.get("A", 10);
    assert!(held.pending.is_none());
    assert_eq!(held.active.unwrap().generation, rig.stamp);
    peer.rpc(&report(&user, true), false).await;
    for project in ["A", "P"] {
        let hint = peer.next_hint().await;
        assert_eq!(
            (hint.project_id.as_deref(), hint.generation),
            (Some(project), generation)
        );
    }
    let fallback: ConfigGetReply =
        serde_json::from_slice(&peer.rpc(&request(Some("P")), true).await.body).unwrap();
    let view = fallback.active.unwrap();
    assert_eq!(view.effective["limit"], 2);
    assert!(view.attempt.is_none());
    let a: ConfigGetReply =
        serde_json::from_slice(&peer.rpc(&request(Some("A")), true).await.body).unwrap();
    assert!(a.pending.unwrap().attempt.is_some());
    assert!(rig
        .owner
        .project_delivery_pass(INCUMBENT, "a")
        .unwrap()
        .hints
        .is_empty());
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn stamped_delivery_worker_starts_no_registry_poll_with_guard_off() {
    let rig = CaptureRig::with_capture_enabled(false);
    rig.set_projects(vec![member("P", None, &[])]);
    let peer = Peer::new(rig.owner.clone(), INCUMBENT);
    let worker = peer.handler.spawn_config_delivery_worker().unwrap();
    for _ in 0..3 {
        rig.owner.notify_delivery_changed();
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(rig.registry.enumerations.load(Ordering::Relaxed), 0);
    assert!(rig
        .owner
        .lock()
        .unwrap()
        .store
        .loaded_admission_state()
        .1
        .registry_membership
        .is_none());
    worker.abort();
    let _ = worker.await;
}
