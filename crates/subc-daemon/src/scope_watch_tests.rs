use super::*;
use subc_control::{ScopeRef, ScopeWatchEvent, ScopeWatchPush, ScopeWatchRequest, ScopeWatchResponse, ScopeWatchSelector, ScopeWatchStatus};

fn selector(scope_ref: &str, epoch: u64) -> ScopeWatchSelector {
    ScopeWatchSelector { scope_ref: ScopeRef { owner: Principal::Reserved { module_id: OWNER.into() }, scope_ref: scope_ref.into() }, epoch }
}

async fn open(handler: &ControlHandler, ctx: &RouteCtx, corr: u64, scopes: Vec<ScopeWatchSelector>) -> Vec<Frame> {
    let frame = Frame::build(FrameType::Request, Flags(control_flags().0 | subc_protocol::FLAG_SUBSCRIPTION), 0, 0, corr, serde_json::to_vec(&ScopeWatchRequest::new(scopes)).unwrap()).unwrap();
    handler.handle_control_frame(ctx, frame).await.unwrap()
}

async fn received(rx: &mut mpsc::Receiver<crate::router::OutboundFrame>, ty: FrameType, corr: u64) -> Frame {
    let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.expect("scope watch frame").unwrap();
    assert_eq!(outbound.header.ty, ty);
    assert_eq!(outbound.header.channel, 0);
    assert_eq!(outbound.header.epoch, 0);
    assert_eq!(outbound.header.corr, corr);
    assert!(!outbound.header.flags.is_last());
    outbound.frame
}

async fn snapshot(rx: &mut mpsc::Receiver<crate::router::OutboundFrame>, corr: u64) -> ScopeWatchResponse {
    serde_json::from_slice(&received(rx, FrameType::Response, corr).await.body).unwrap()
}

async fn push(rx: &mut mpsc::Receiver<crate::router::OutboundFrame>, corr: u64) -> ScopeWatchPush {
    serde_json::from_slice(&received(rx, FrameType::StreamData, corr).await.body).unwrap()
}

async fn no_push(rx: &mut mpsc::Receiver<crate::router::OutboundFrame>) {
    assert!(tokio::time::timeout(Duration::from_millis(50), rx.recv()).await.is_err(), "unexpected scope event");
}

#[tokio::test]
async fn scope_watch_live_scope_gets_ended_exactly_once() {
    let mut rig = rig().await;
    rig.sync(vec![session(1)]).await;
    assert!(open(&rig.handler, &rig.owner, 77, vec![selector("s", 1), selector("s", 1)]).await.is_empty());
    let initial = snapshot(&mut rig._owner_rx, 77).await;
    assert_eq!(initial.scopes.len(), 1, "selectors form a set");
    assert_eq!(initial.scopes[0].status, ScopeWatchStatus::Live);
    rig.sync(vec![]).await;
    let event = push(&mut rig._owner_rx, 77).await;
    assert_eq!(event, ScopeWatchPush::new(ScopeWatchEvent::Ended, selector("s", 1).scope_ref, 1, None));
    rig.sync(vec![session(2)]).await;
    rig.sync(vec![]).await;
    no_push(&mut rig._owner_rx).await;
    rig.handler.cleanup_connection(rig.owner.connection_id).unwrap();
}

#[tokio::test]
async fn scope_watch_epoch_bump_is_superseded_with_by_epoch() {
    let mut rig = rig().await;
    rig.sync(vec![session(1)]).await;
    open(&rig.handler, &rig.owner, 77, vec![selector("s", 1)]).await;
    snapshot(&mut rig._owner_rx, 77).await;
    rig.sync(vec![session(5)]).await;
    assert_eq!(push(&mut rig._owner_rx, 77).await, ScopeWatchPush::new(ScopeWatchEvent::Superseded, selector("s", 1).scope_ref, 1, Some(5)));
    rig.handler.cleanup_connection(rig.owner.connection_id).unwrap();
}

#[tokio::test]
async fn scope_watch_open_after_end_and_epoch_bump_reports_current_state() {
    let mut rig = rig().await;
    rig.sync(vec![session(1), head("other", 1)]).await;
    rig.sync(vec![head("other", 4)]).await;
    open(&rig.handler, &rig.owner, 77, vec![selector("s", 1), selector("other", 1), selector("never", 1)]).await;
    let initial = snapshot(&mut rig._owner_rx, 77).await;
    assert_eq!(initial.scopes.iter().map(|entry| entry.status.clone()).collect::<Vec<_>>(), vec![ScopeWatchStatus::Ended, ScopeWatchStatus::Superseded, ScopeWatchStatus::UnknownScope]);
    assert_eq!(initial.scopes[1].by_epoch, Some(4));
    no_push(&mut rig._owner_rx).await;
    rig.handler.cleanup_connection(rig.owner.connection_id).unwrap();
}

#[tokio::test]
async fn scope_watch_denied_reader_sees_unknown_before_and_after_end() {
    use subc_protocol::scope::{FederatedAttributes, FederatedPeerClass, ScopeAttributes};
    let mut rig = rig().await;
    rig.handler = rig.handler.clone().with_federated_scope_owner(OWNER.into());
    let scope = ScopeRecord::new("s", 1, ScopeKind::Federated).with_attributes(
        ScopeAttributes::new().with_federated(Some(FederatedAttributes::new(FederatedPeerClass::Computer, "a".repeat(32), "pair".into(), "Computer".into())))
    );
    rig.sync(vec![scope]).await;
    let denied = call(&rig.handler, &rig.owner, &ModuleControlRequestFromModule::ScopeDescribe { owner: selector("s", 1).scope_ref.owner, scope_ref: "s".into() }).await;
    assert_eq!(parse_error(&denied)["code"], error_codes::SCOPE_FEDERATED_UNSUPPORTED);
    open(&rig.handler, &rig.owner, 77, vec![selector("s", 1)]).await;
    assert_eq!(snapshot(&mut rig._owner_rx, 77).await.scopes[0].status, ScopeWatchStatus::UnknownScope);
    rig.sync(vec![]).await;
    no_push(&mut rig._owner_rx).await;
    open(&rig.handler, &rig.owner, 78, vec![selector("s", 1)]).await;
    assert_eq!(snapshot(&mut rig._owner_rx, 78).await.scopes[0].status, ScopeWatchStatus::UnknownScope);
    rig.handler.cleanup_connection(rig.owner.connection_id).unwrap();
}

#[tokio::test]
async fn scope_watch_selector_and_connection_limits_are_refused() {
    let mut rig = rig().await;
    let scopes = (0..257).map(|epoch| selector("s", epoch)).collect();
    let refused = open(&rig.handler, &rig.owner, 77, scopes).await;
    assert_eq!(parse_error(&refused[0])["code"], "scope_watch_limit_exceeded");
    for corr in 1..=16 {
        let selectors = (0..256).map(|epoch| selector("s", epoch)).collect();
        assert!(open(&rig.handler, &rig.owner, corr, selectors).await.is_empty());
        assert_eq!(snapshot(&mut rig._owner_rx, corr).await.scopes.len(), 256);
    }
    let refused = open(&rig.handler, &rig.owner, 17, vec![]).await;
    assert_eq!(parse_error(&refused[0])["code"], "scope_watch_limit_exceeded");
    let duplicate = open(&rig.handler, &rig.owner, 1, vec![]).await;
    assert_eq!(parse_error(&duplicate[0])["code"], "duplicate_subscription");
    rig.handler.cleanup_connection(rig.owner.connection_id).unwrap();
}

#[tokio::test]
async fn scope_watch_cancel_stops_pushes_and_releases_slot() {
    let mut rig = rig().await;
    rig.sync(vec![session(1)]).await;
    open(&rig.handler, &rig.owner, 77, vec![selector("s", 1)]).await;
    snapshot(&mut rig._owner_rx, 77).await;
    let cancel = Frame::build(FrameType::Cancel, control_flags(), 0, 0, 77, vec![]).unwrap();
    assert!(rig.handler.handle_control_frame(&rig.owner, cancel).await.unwrap().is_empty());
    assert!(rig.handler.scope_streams.lock().unwrap().is_empty());
    rig.sync(vec![]).await;
    no_push(&mut rig._owner_rx).await;
    open(&rig.handler, &rig.owner, 77, vec![selector("s", 1)]).await;
    assert_eq!(snapshot(&mut rig._owner_rx, 77).await.scopes[0].status, ScopeWatchStatus::Ended);
    rig.handler.cleanup_connection(rig.owner.connection_id).unwrap();
    assert!(rig.handler.scope_streams.lock().unwrap().is_empty());
}

#[tokio::test]
async fn scope_watch_child_survives_head_epoch_move_without_push_or_route_drain() {
    let mut rig = rig().await;
    let child = |parent_epoch| ScopeRecord::new("child", 9, ScopeKind::Worker).with_parent(Some(ScopeParent::new(selector("s", 1).scope_ref.owner, "s", parent_epoch)));
    rig.sync(vec![session(1), child(1)]).await;
    let route = rig.bound(Some(OWNER), PLEXUS, Some(rig_selector("child", Some(9)))).await;
    open(&rig.handler, &rig.owner, 77, vec![selector("child", 9), selector("s", 1)]).await;
    let initial = snapshot(&mut rig._owner_rx, 77).await;
    assert!(initial.scopes.iter().all(|state| state.status == ScopeWatchStatus::Live));
    rig.sync(vec![session(2), child(2)]).await;
    assert_eq!(push(&mut rig._owner_rx, 77).await, ScopeWatchPush::new(ScopeWatchEvent::Superseded, selector("s", 1).scope_ref, 1, Some(2)));
    assert!(rig.live(&route), "re-parenting keeps the worker's route live");
    assert!(matches!(describe(&rig.handler, &rig.owner, OWNER, "child").await,
        ModuleControlResponseToModule::ScopeDescribe { status: ScopeStatus::Live, scope_epoch: Some(9), scope: Some(ScopeStamp { parent_state: Some(ParentState::Linked), .. }), .. }));
    no_push(&mut rig._owner_rx).await;
    rig.handler.cleanup_connection(rig.owner.connection_id).unwrap();
}

#[tokio::test]
async fn scope_watch_slow_consumer_retains_first_ended_and_superseded_events() {
    let mut rig = rig().await;
    rig.sync(vec![session(1), head("other", 1)]).await;
    let (tx, mut rx) = mpsc::channel(1);
    let ctx = RouteCtx { connection_id: rig.owner.connection_id, egress: FrameSink::new(tx) };
    ctx.egress.try_send(Frame::build(FrameType::Pong, control_flags(), 0, 0, 0, vec![]).unwrap()).unwrap();
    open(&rig.handler, &ctx, 77, vec![selector("s", 1), selector("other", 1)]).await;
    rig.sync(vec![head("other", 1)]).await;
    rig.sync(vec![head("other", 3)]).await;
    // Later epochs must not overwrite the first queued terminal events.
    rig.sync(vec![session(8)]).await;
    assert_eq!(rx.recv().await.unwrap().header.ty, FrameType::Pong);
    assert!(snapshot(&mut rx, 77).await.scopes.iter().all(|state| state.status == ScopeWatchStatus::Live));
    assert_eq!(push(&mut rx, 77).await, ScopeWatchPush::new(ScopeWatchEvent::Ended, selector("s", 1).scope_ref, 1, None));
    assert_eq!(push(&mut rx, 77).await, ScopeWatchPush::new(ScopeWatchEvent::Superseded, selector("other", 1).scope_ref, 1, Some(3)));
    no_push(&mut rx).await;
    rig.handler.cleanup_connection(rig.owner.connection_id).unwrap();
}

#[tokio::test]
async fn scope_watch_requires_registration_and_is_advertised() {
    let rig = rig().await;
    let (direct, _rx) = wide_ctx(999);
    let refused = open(&rig.handler, &direct, 77, vec![]).await;
    assert_eq!(parse_error(&refused[0])["code"], "not_registered");
    assert!(crate::control::MODULE_TO_SUBC_CONTROL_OPS.contains(&subc_control::ops::SCOPE_WATCH));
}
